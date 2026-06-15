//! Synthesizing an `Arbitrary` for a contract's binder types.
//!
//! Built-in types compose the `std/Test.fai` combinators (`Test.int`,
//! `Test.list arb`, …). A user **record** or **ADT** has no generic combinator,
//! so the compiler synthesizes a top-level `Arbitrary` definition per type —
//! referenced as a `Global`, so composing them needs no closures, and a recursive
//! type is just a self-reference guarded by the size budget. Every synthesized
//! function is **capture-free**: a value it would otherwise close over becomes a
//! leading parameter supplied by partial application (the runtime forms the
//! closure), so no by-hand capture analysis is required.

use std::sync::Arc;

use fai_core::ir::{
    CExpr, CoreFn, ExprKind as K, FieldIndex, Lit, LoweredDef, Prim, scalar_field_mask,
};
use fai_db::{Db, SourceFile};
use fai_resolve::{AdtRef, DefId, LocalId, ModuleName, module_defs, module_file, type_decls};
use fai_span::SourceId;
use fai_syntax::Symbol;
use fai_types::{Con, RecordRow, RowEnd, Scheme, Ty, TyVarId, constructor_scheme, def_type};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::synth::NotRunnable;
use crate::{CONTRACT_AMBIGUOUS_GENERATOR, CONTRACT_NON_GROUNDABLE};

/// Field offsets in the std `Arbitrary` record (sorted labels: `gen`, `show`,
/// `shrink`) and the `(value, seed)` pair a generator returns.
const GEN: u32 = 0;
const SHOW: u32 = 1;
const SHRINK: u32 = 2;
const PAIR_VALUE: u32 = 0;
const PAIR_SEED: u32 = 1;

/// Synthesizes `Arbitrary` definitions for user types, deduplicated per run.
pub struct ArbBuilder<'a> {
    db: &'a dyn Db,
    /// The contract's file (its declarations supply custom-generator overrides).
    file: SourceFile,
    /// The file the synthesized defs belong to (the contract's source).
    source: SourceId,
    /// A prefix making synthesized names unique across a file's contracts.
    prefix: String,
    /// Counter for unique synthesized-def names.
    counter: usize,
    /// Already-synthesized arbitraries, by type (also breaks recursion).
    seen: FxHashMap<Ty, DefId>,
    /// User-defined `Arbitrary T` overrides, keyed by the generated type `T`.
    /// Consulted at the top of [`Self::arb_for`] and treated as opaque leaves by
    /// the reachability/rank analyses.
    custom: FxHashMap<Ty, DefId>,
    /// Types with more than one matching user `Arbitrary` (an ambiguous override).
    ambiguous: FxHashSet<Ty>,
    /// Least-fixpoint smallest-value depth of each reachable groundable user ADT
    /// (absent ⇒ no finite value). Drives the recursion base case.
    ranks: FxHashMap<Ty, u32>,
    /// The synthesized definitions and their runtime arities.
    pub defs: Vec<(LoweredDef, usize)>,
}

impl<'a> ArbBuilder<'a> {
    /// Creates a builder; `prefix` (e.g. `contract#3`) namespaces synthesized defs.
    pub fn new(db: &'a dyn Db, file: SourceFile, prefix: String) -> Self {
        ArbBuilder {
            db,
            file,
            source: file.source(db),
            prefix,
            counter: 0,
            seen: FxHashMap::default(),
            custom: FxHashMap::default(),
            ambiguous: FxHashSet::default(),
            ranks: FxHashMap::default(),
            defs: Vec::new(),
        }
    }

    /// Discovers user-defined `Arbitrary` overrides in the contract's file and
    /// ranks the types reachable from the binders (for the recursion base case).
    /// Call once, before [`Self::arb_for`].
    pub fn prepare(&mut self, binder_types: &[Ty]) {
        self.scan_custom();
        // Rank the types reachable once opaque aliases are expanded to their
        // representation, so a base case hidden inside an opaque record is found.
        let binders: Vec<Ty> =
            binder_types.iter().map(|t| expand_opaque(self.db, &self.custom, t)).collect();
        self.ranks = compute_ranks(self.db, &self.custom, &binders);
    }

    /// Scans the contract file's top-level definitions for values whose type is
    /// `Arbitrary T` (a closed `{ gen, show, shrink }` record), recording each as
    /// the override for `T`. A `T` matched by two definitions is ambiguous.
    fn scan_custom(&mut self) {
        for info in &module_defs(self.db, self.file).defs {
            let scheme = def_type(self.db, self.file, info.name);
            // Only a monomorphic `Arbitrary T` value (not a parametric combinator
            // `Arbitrary 'a -> Arbitrary (T 'a)`, whose type is an arrow).
            if !scheme.vars.is_empty() || !scheme.row_vars.is_empty() {
                continue;
            }
            let Some(elem) = arbitrary_element(&scheme.ty) else { continue };
            // Overrides apply to user records/ADTs only — not built-in generators.
            if !is_custom_eligible(&elem) {
                continue;
            }
            let def = DefId::new(self.source, info.name);
            if self.custom.insert(elem.clone(), def).is_some() {
                self.ambiguous.insert(elem);
            }
        }
    }

    /// A `DefId` in the contract's file with a unique synthesized name.
    fn fresh_def(&mut self, what: &str) -> DefId {
        let name = format!("{}${what}#{}", self.prefix, self.counter);
        self.counter += 1;
        DefId::new(self.source, Symbol::intern(&name))
    }

    /// Resolves a `Module.name` standard-library definition.
    fn std(&self, module: &str, name: &str) -> Result<DefId, NotRunnable> {
        let m = module_file(self.db, ModuleName(Symbol::intern(module)))
            .ok_or_else(|| NotRunnable::reason(format!("the std `{module}` module is missing")))?;
        Ok(DefId::new(m.source(self.db), Symbol::intern(name)))
    }

    fn test(&self, name: &str) -> Result<CExpr, NotRunnable> {
        Ok(global(self.std("Test", name)?))
    }

    /// The `Arbitrary` expression for `ty`: a combinator composition for built-in
    /// types, or a `Global` reference to a synthesized definition for a
    /// record/ADT (which it generates on demand).
    pub fn arb_for(&mut self, ty: &Ty) -> Result<CExpr, NotRunnable> {
        // A user-defined `Arbitrary T` takes precedence over synthesis (and over
        // the groundability analysis), wherever `T` is generated.
        if self.ambiguous.contains(ty) {
            return Err(NotRunnable::coded(
                CONTRACT_AMBIGUOUS_GENERATOR,
                format!(
                    "more than one `Arbitrary` is defined for `{}`",
                    fai_types::render(ty, &fai_types::VarNames::new())
                ),
            ));
        }
        if let Some(&def) = self.custom.get(ty) {
            return Ok(global(def));
        }
        // An opaque alias reaches generation nominally (its body is hidden from
        // this file); expand it to its representation, having first honored any
        // custom override and the ambiguity check above.
        let expanded = expand_opaque(self.db, &self.custom, ty);
        let ty = &expanded;
        if let Ty::Record(row) = ty {
            return if row.tail == RowEnd::Closed {
                Ok(global(self.ensure_record(ty, row)?))
            } else {
                self.unsupported(ty)
            };
        }
        if let Ty::Tuple(elems) = ty {
            let combinator = match elems.len() {
                2 => "tuple2",
                3 => "tuple3",
                4 => "tuple4",
                _ => return self.unsupported(ty),
            };
            let args = elems.iter().map(|e| self.arb_for(e)).collect::<Result<_, _>>()?;
            return Ok(app(self.test(combinator)?, args));
        }
        let (head, args) = peel_app(ty);
        match head {
            Ty::Con(Con::Int) => self.test("int"),
            Ty::Con(Con::Bool) => self.test("bool"),
            Ty::Con(Con::Float) => self.test("float"),
            Ty::Con(Con::Char) => self.test("char"),
            Ty::Con(Con::String) => self.test("string"),
            Ty::Unit => self.test("unit"),
            Ty::Con(Con::List) if args.len() == 1 => {
                Ok(app(self.test("list")?, vec![self.arb_for(args[0])?]))
            }
            Ty::Con(Con::Array) if args.len() == 1 => {
                Ok(app(self.test("array")?, vec![self.arb_for(args[0])?]))
            }
            Ty::Adt(adt) => match (adt.name.as_str(), args.len()) {
                ("Option", 1) => Ok(app(self.test("option")?, vec![self.arb_for(args[0])?])),
                ("Result", 2) => Ok(app(
                    self.test("result")?,
                    vec![self.arb_for(args[0])?, self.arb_for(args[1])?],
                )),
                _ => Ok(global(self.ensure_adt(ty, *adt, &args)?)),
            },
            _ => self.unsupported(ty),
        }
    }

    fn unsupported(&self, ty: &Ty) -> Result<CExpr, NotRunnable> {
        Err(NotRunnable::reason(format!(
            "cannot generate values of type `{}`",
            fai_types::render(ty, &fai_types::VarNames::new())
        )))
    }

    /// Rejects a type with no finite value (no base case ⇒ generation can't
    /// terminate) as [`CONTRACT_NON_GROUNDABLE`].
    fn require_groundable(&self, ty: &Ty) -> Result<(), NotRunnable> {
        if self.groundable(ty) {
            return Ok(());
        }
        Err(NotRunnable::coded(
            CONTRACT_NON_GROUNDABLE,
            format!(
                "type `{}` has no finite value (every constructor is recursive)",
                fai_types::render(ty, &fai_types::VarNames::new())
            ),
        ))
    }

    /// The generator for one constructor/record field, paired with whether it
    /// re-enters `target` (and so consumes a split slice of the budget). A
    /// recursive `List` field uses `recList`, which splits its slice across its
    /// elements; every other field uses its ordinary arbitrary.
    fn field_arb(&mut self, field_ty: &Ty, target: &Ty) -> Result<(CExpr, bool), NotRunnable> {
        let recursive = self.can_reach(field_ty, target);
        if recursive && let Some((elem, rec_combinator)) = sequence_element(field_ty) {
            let elem_arb = self.arb_for(elem)?;
            return Ok((app(self.test(rec_combinator)?, vec![elem_arb]), true));
        }
        Ok((self.arb_for(field_ty)?, recursive))
    }

    /// Whether `ty` has a finite value (can be generated at all).
    fn groundable(&self, ty: &Ty) -> bool {
        self.rank_of(ty).is_some()
    }

    /// The smallest-value depth of `ty` (its rank), or `None` if it has no finite
    /// value. ADT ranks come from the precomputed [`Self::ranks`] fixpoint.
    fn rank_of(&self, ty: &Ty) -> Option<u32> {
        rank_of(&self.ranks, &self.custom, ty)
    }

    /// The constructors eligible at the budget floor: those whose smallest value
    /// is as shallow as the type's (so floor generation strictly shrinks).
    fn base_ctors<'c>(&self, ty: &Ty, ctors: &'c [Ctor]) -> Vec<&'c Ctor> {
        let target = self.rank_of(ty);
        ctors.iter().filter(|c| self.ctor_rank(c) == target).collect()
    }

    /// The rank of a constructor: one deeper than its deepest field, or `None` if
    /// any field has no finite value.
    fn ctor_rank(&self, ctor: &Ctor) -> Option<u32> {
        let mut worst = 0;
        for f in &ctor.fields {
            worst = worst.max(self.rank_of(f)?);
        }
        Some(worst + 1)
    }

    /// Whether generating `field` can re-enter `target` (directly, mutually, or
    /// through a collection/wrapper). A custom-overridden type is an opaque leaf.
    fn can_reach(&self, field: &Ty, target: &Ty) -> bool {
        let mut visited = FxHashSet::default();
        self.reaches(field, target, &mut visited)
    }

    fn reaches(&self, ty: &Ty, target: &Ty, visited: &mut FxHashSet<Ty>) -> bool {
        if ty == target {
            return true;
        }
        if self.custom.contains_key(ty) {
            return false;
        }
        match ty {
            Ty::Tuple(es) => es.iter().any(|e| self.reaches(e, target, visited)),
            Ty::Record(row) => row.fields.iter().any(|(_, t)| self.reaches(t, target, visited)),
            _ => {
                let (head, args) = peel_app(ty);
                match head {
                    Ty::Con(Con::List | Con::Array) => {
                        args.iter().any(|a| self.reaches(a, target, visited))
                    }
                    Ty::Adt(adt) if is_builtin_adt(*adt) => {
                        args.iter().any(|a| self.reaches(a, target, visited))
                    }
                    Ty::Adt(adt) => {
                        // Pathological non-regular recursion: force a split (safe).
                        if visited.len() > MAX_REACH {
                            return true;
                        }
                        if !visited.insert(ty.clone()) {
                            return false; // already exploring this applied type
                        }
                        adt_ctor_fields(self.db, &self.custom, *adt, &args).is_some_and(|ctors| {
                            ctors.iter().flatten().any(|f| self.reaches(f, target, visited))
                        })
                    }
                    _ => false,
                }
            }
        }
    }

    /// Ensures a synthesized `Arbitrary` for a closed record type and returns it.
    fn ensure_record(&mut self, ty: &Ty, row: &RecordRow) -> Result<DefId, NotRunnable> {
        self.require_groundable(ty)?;
        if let Some(def) = self.seen.get(ty) {
            return Ok(*def);
        }
        let arb_def = self.fresh_def("arb");
        self.seen.insert(ty.clone(), arb_def);

        // Each field's arbitrary plus whether it re-enters this record (so it is
        // generated with a divided slice of the budget).
        let fields: Vec<(CExpr, bool)> =
            row.fields.iter().map(|(_, t)| self.field_arb(t, ty)).collect::<Result<_, _>>()?;
        let field_arbs: Vec<CExpr> = fields.iter().map(|(a, _)| a.clone()).collect();

        let scalars = scalar_field_mask(row.fields.iter().map(|(_, t)| t));
        let gen_fn = self.record_gen(&fields, scalars);
        let show_fn = self.record_show(row, &field_arbs);
        let shrink_fn = self.record_shrink(ty, row, &field_arbs)?;
        self.push_arbitrary(arb_def, gen_fn, show_fn, shrink_fn);
        Ok(arb_def)
    }

    /// `fun size seed -> let (v0, s1) = a0.gen <b0> seed in … ({ … }, sN)`, where a
    /// recursive field's budget `<bi>` is the size split across the recursive
    /// fields (so the record stays within the fuel budget).
    fn record_gen(&mut self, fields: &[(CExpr, bool)], scalars: u64) -> CoreFn {
        let mut next = 2; // 0 = size, 1 = seed
        let size = LocalId::from_index(0);
        let mut seed = LocalId::from_index(1);
        let k = fields.iter().filter(|(_, rec)| *rec).count();
        let mut binds: Vec<(LocalId, CExpr)> = Vec::new();
        let mut values: Vec<CExpr> = Vec::with_capacity(fields.len());
        for (arb, rec) in fields {
            let size_arg = if *rec { split_budget(size, k) } else { local(size) };
            let pair = fresh(&mut next);
            let value = fresh(&mut next);
            let next_seed = fresh(&mut next);
            binds.push((pair, app(field(arb.clone(), GEN), vec![size_arg, local(seed)])));
            binds.push((value, field(local(pair), PAIR_VALUE)));
            binds.push((next_seed, field(local(pair), PAIR_SEED)));
            values.push(local(value));
            seed = next_seed;
        }
        let record = make_data(0, values, scalars);
        let result = make_data(0, vec![record, local(seed)], 0);
        let body = lets(binds, result);
        CoreFn { params: vec![size, LocalId::from_index(1)], captures: Vec::new(), body }
    }

    /// `fun r -> "{ l0 = " ++ a0.show r.l0 ++ ", l1 = " ++ … ++ " }"`.
    fn record_show(&self, row: &RecordRow, field_arbs: &[CExpr]) -> CoreFn {
        let r = LocalId::from_index(0);
        let mut parts: Vec<CExpr> = Vec::new();
        for (i, ((label, _), arb)) in row.fields.iter().zip(field_arbs).enumerate() {
            let sep = if i == 0 { format!("{{ {label} = ") } else { format!(", {label} = ") };
            parts.push(str_lit(&sep));
            let i = u32::try_from(i).unwrap_or(0);
            parts.push(app(field(arb.clone(), SHOW), vec![field(local(r), i)]));
        }
        parts.push(str_lit(" }"));
        CoreFn { params: vec![r], captures: Vec::new(), body: concat_all(parts) }
    }

    /// `fun r -> List.append (List.map (set0 r) (a0.shrink r.l0)) (…)` — each field
    /// shrunk in turn, rebuilding the record via a partially-applied setter.
    fn record_shrink(
        &mut self,
        ty: &Ty,
        row: &RecordRow,
        field_arbs: &[CExpr],
    ) -> Result<CoreFn, NotRunnable> {
        let r = LocalId::from_index(0);
        let list_map = self.std("List", "map")?;
        let list_append = self.std("List", "append")?;
        let n = row.fields.len();
        // Build each field's shrink list, then append them (right-fold onto []).
        let mut acc = make_data(0, Vec::new(), 0); // []
        for i in (0..n).rev() {
            let setter = self.record_setter(ty, row, i);
            let idx = u32::try_from(i).unwrap_or(0);
            let field_shrinks =
                app(field(field_arbs[i].clone(), SHRINK), vec![field(local(r), idx)]);
            let rebuilt =
                app(global(list_map), vec![app(global(setter), vec![local(r)]), field_shrinks]);
            acc = app(global(list_append), vec![rebuilt, acc]);
        }
        Ok(CoreFn { params: vec![r], captures: Vec::new(), body: acc })
    }

    /// A setter helper `fun r v -> { r with l_i = v }` (capture-free; the record
    /// is a parameter, so `List.map` uses a partial application).
    fn record_setter(&mut self, _ty: &Ty, row: &RecordRow, i: usize) -> DefId {
        let def = self.fresh_def("set");
        let r = LocalId::from_index(0);
        let v = LocalId::from_index(1);
        let fields: Vec<CExpr> = (0..row.fields.len())
            .map(|j| if j == i { local(v) } else { field(local(r), u32::try_from(j).unwrap_or(0)) })
            .collect();
        let body = make_data(0, fields, scalar_field_mask(row.fields.iter().map(|(_, t)| t)));
        self.defs.push((
            LoweredDef {
                def,
                fns: vec![CoreFn { params: vec![r, v], captures: Vec::new(), body }],
                entry_borrowed: Vec::new(),
                reuse_entry: None,
                entry_spread_params: Vec::new(),
            },
            2,
        ));
        def
    }

    /// Ensures a synthesized `Arbitrary` for a (possibly recursive) ADT applied
    /// to `args`, and returns its definition.
    fn ensure_adt(&mut self, ty: &Ty, adt: AdtRef, args: &[&Ty]) -> Result<DefId, NotRunnable> {
        // A type with no finite value cannot be generated (no base case to
        // terminate generation); reject before recursing into its constructors.
        self.require_groundable(ty)?;
        if let Some(def) = self.seen.get(ty) {
            return Ok(*def);
        }
        let arb_def = self.fresh_def("arb");
        self.seen.insert(ty.clone(), arb_def); // before building, so recursion self-refers

        let unknown = || NotRunnable::reason(format!("type `{}` is unavailable", adt.name));
        let adt_file = self.db.source_file(adt.file).ok_or_else(unknown)?;
        let decls = type_decls(self.db, adt_file);
        let info = decls.type_named(adt.name).filter(|i| !i.is_alias).ok_or_else(unknown)?;
        let ctor_names = info.ctors.clone();
        let mut ctors = Vec::with_capacity(ctor_names.len());
        for cname in ctor_names {
            let scheme = constructor_scheme(self.db, adt_file, cname).ok_or_else(unknown)?;
            let tag = decls.ctor(cname).map_or(0, |c| c.tag);
            let fields = ctor_field_types(&scheme, args)
                .iter()
                .map(|f| expand_opaque(self.db, &self.custom, f))
                .collect();
            ctors.push(Ctor { tag, name: cname, fields });
        }

        let gen_fn = self.adt_gen(ty, &ctors)?;
        let show_fn = self.adt_show(&ctors)?;
        let shrink_fn = self.adt_shrink(ty, &ctors)?;
        self.push_arbitrary(arb_def, gen_fn, show_fn, shrink_fn);
        Ok(arb_def)
    }

    /// `fun size seed -> if size <= 0 then <choose a base ctor> else <choose any>`.
    ///
    /// At the budget floor only the **minimal-rank** (smallest-value) constructors
    /// are eligible, so generation deterministically bottoms out; above the floor
    /// any constructor is fair game, with recursive fields drawn at a split budget.
    fn adt_gen(&mut self, ty: &Ty, ctors: &[Ctor]) -> Result<CoreFn, NotRunnable> {
        let size = LocalId::from_index(0);
        let seed = LocalId::from_index(1);
        let mut next = 2;
        let base = self.base_ctors(ty, ctors);
        let all: Vec<&Ctor> = ctors.iter().collect();
        let small = self.choose_among(&base, ty, size, seed, &mut next)?;
        let big = self.choose_among(&all, ty, size, seed, &mut next)?;
        let cond = prim(Prim::IntLe, vec![local(size), int(0)]);
        Ok(CoreFn { params: vec![size, seed], captures: Vec::new(), body: if_(cond, small, big) })
    }

    /// Picks one of `ctors` with `Test.choose` and builds it (a single choice is
    /// built directly).
    fn choose_among(
        &mut self,
        ctors: &[&Ctor],
        ty: &Ty,
        size: LocalId,
        seed: LocalId,
        next: &mut usize,
    ) -> Result<CExpr, NotRunnable> {
        if let [only] = ctors {
            return self.build_ctor(only, ty, size, seed, next);
        }
        let cpair = fresh(next);
        let c = fresh(next);
        let s1 = fresh(next);
        let last = ctors.len() - 1;
        let mut chain = self.build_ctor(ctors[last], ty, size, s1, next)?;
        for k in (0..last).rev() {
            let alt = self.build_ctor(ctors[k], ty, size, s1, next)?;
            let cond = prim(Prim::Eq, vec![local(c), int(i64::try_from(k).unwrap_or(0))]);
            chain = if_(cond, alt, chain);
        }
        let choose = self.std("Test", "choose")?;
        let count = int(i64::try_from(ctors.len()).unwrap_or(0));
        Ok(lets(
            vec![
                (cpair, app(global(choose), vec![count, local(seed)])),
                (c, field(local(cpair), PAIR_VALUE)),
                (s1, field(local(cpair), PAIR_SEED)),
            ],
            chain,
        ))
    }

    /// Builds one constructor value, returning the `(value, seed)` pair. Each
    /// recursive field is drawn at the size split across the constructor's
    /// recursive fields (so the value stays within the fuel budget); a recursive
    /// `List` field splits its slice further across its elements (`recList`).
    fn build_ctor(
        &mut self,
        ctor: &Ctor,
        ty: &Ty,
        size: LocalId,
        seed: LocalId,
        next: &mut usize,
    ) -> Result<CExpr, NotRunnable> {
        if ctor.fields.is_empty() {
            return Ok(make_data(0, vec![make_data(ctor.tag, Vec::new(), 0), local(seed)], 0));
        }
        let fields: Vec<(CExpr, bool)> =
            ctor.fields.iter().map(|ft| self.field_arb(ft, ty)).collect::<Result<_, _>>()?;
        let k = fields.iter().filter(|(_, rec)| *rec).count();
        let mut binds = Vec::new();
        let mut values = Vec::with_capacity(fields.len());
        let mut cur = seed;
        for (arb, rec) in fields {
            let size_arg = if rec { split_budget(size, k) } else { local(size) };
            let pair = fresh(next);
            let value = fresh(next);
            let next_seed = fresh(next);
            binds.push((pair, app(field(arb, GEN), vec![size_arg, local(cur)])));
            binds.push((value, field(local(pair), PAIR_VALUE)));
            binds.push((next_seed, field(local(pair), PAIR_SEED)));
            values.push(local(value));
            cur = next_seed;
        }
        let made = make_data(ctor.tag, values, scalar_field_mask(ctor.fields.iter()));
        Ok(lets(binds, make_data(0, vec![made, local(cur)], 0)))
    }

    /// `fun v -> if tag v = t0 then "C0 …" else …`.
    fn adt_show(&mut self, ctors: &[Ctor]) -> Result<CoreFn, NotRunnable> {
        let v = LocalId::from_index(0);
        let mut chain = str_lit("?");
        for ctor in ctors.iter().rev() {
            let render = self.render_ctor(ctor, v)?;
            let cond = prim(Prim::Eq, vec![data_tag(local(v)), int(i64::from(ctor.tag))]);
            chain = if_(cond, render, chain);
        }
        Ok(CoreFn { params: vec![v], captures: Vec::new(), body: chain })
    }

    /// `"Ctor " ++ (show f0) ++ " " ++ …` (an ADT-typed field is parenthesized).
    fn render_ctor(&mut self, ctor: &Ctor, v: LocalId) -> Result<CExpr, NotRunnable> {
        let paren = self.std("Test", "parenIfSpaced")?;
        let mut parts = vec![str_lit(ctor.name.as_str())];
        for (i, ft) in ctor.fields.iter().enumerate() {
            parts.push(str_lit(" "));
            let shown = app(
                field(self.arb_for(ft)?, SHOW),
                vec![field(local(v), u32::try_from(i).unwrap_or(0))],
            );
            // A constructor argument that is itself an application needs parens;
            // decide at runtime (so a nullary value is not parenthesized).
            parts.push(if is_adt(ft) { app(global(paren), vec![shown]) } else { shown });
        }
        Ok(concat_all(parts))
    }

    /// `fun v -> if tag v = t0 then <shrink C0> else …` — each field shrunk and the
    /// constructor rebuilt, plus (for recursive fields) the subterm itself.
    fn adt_shrink(&mut self, ty: &Ty, ctors: &[Ctor]) -> Result<CoreFn, NotRunnable> {
        let v = LocalId::from_index(0);
        let list_map = self.std("List", "map")?;
        let list_append = self.std("List", "append")?;
        let mut chain = make_data(0, Vec::new(), 0); // []
        for ctor in ctors.iter().rev() {
            let mut acc = make_data(0, Vec::new(), 0);
            for i in (0..ctor.fields.len()).rev() {
                let setter = self.ctor_setter(ctor, i);
                let idx = u32::try_from(i).unwrap_or(0);
                let fshrink =
                    app(field(self.arb_for(&ctor.fields[i])?, SHRINK), vec![field(local(v), idx)]);
                let mapped =
                    app(global(list_map), vec![app(global(setter), vec![local(v)]), fshrink]);
                acc = app(global(list_append), vec![mapped, acc]);
            }
            // Recursive subterms are smaller candidates; try them first.
            for i in (0..ctor.fields.len()).rev() {
                if ctor.fields[i] == *ty {
                    let sub = field(local(v), u32::try_from(i).unwrap_or(0));
                    acc = make_data(1, vec![sub, acc], 0); // sub :: acc
                }
            }
            let cond = prim(Prim::Eq, vec![data_tag(local(v)), int(i64::from(ctor.tag))]);
            chain = if_(cond, acc, chain);
        }
        Ok(CoreFn { params: vec![v], captures: Vec::new(), body: chain })
    }

    /// A setter `fun v new -> C f0 … new … fN` rebuilding constructor `ctor` with
    /// field `i` replaced (capture-free; applied to `v` via partial application).
    fn ctor_setter(&mut self, ctor: &Ctor, i: usize) -> DefId {
        let def = self.fresh_def("set");
        let v = LocalId::from_index(0);
        let new = LocalId::from_index(1);
        let fields: Vec<CExpr> =
            (0..ctor.fields.len())
                .map(|j| {
                    if j == i { local(new) } else { field(local(v), u32::try_from(j).unwrap_or(0)) }
                })
                .collect();
        let body = make_data(ctor.tag, fields, scalar_field_mask(ctor.fields.iter()));
        self.defs.push((
            LoweredDef {
                def,
                fns: vec![CoreFn { params: vec![v, new], captures: Vec::new(), body }],
                entry_borrowed: Vec::new(),
                reuse_entry: None,
                entry_spread_params: Vec::new(),
            },
            2,
        ));
        def
    }

    /// Pushes the `arb$…` definition `{ gen, show, shrink }` (sorted fields) built
    /// from three capture-free functions.
    fn push_arbitrary(&mut self, def: DefId, gen_fn: CoreFn, show_fn: CoreFn, shrink_fn: CoreFn) {
        // fns[0] = entry (arity 0) building the record; fns[1..] = the three.
        let entry = CoreFn {
            params: Vec::new(),
            captures: Vec::new(),
            body: make_data(
                0,
                vec![
                    make_closure(1), // gen (Arbitrary field 0)
                    make_closure(2), // show (field 1)
                    make_closure(3), // shrink (field 2)
                ],
                0,
            ),
        };
        self.defs.push((
            LoweredDef {
                def,
                fns: vec![entry, gen_fn, show_fn, shrink_fn],
                entry_borrowed: Vec::new(),
                reuse_entry: None,
                entry_spread_params: Vec::new(),
            },
            0,
        ));
    }
}

/// A constructor with its tag and (monomorphized) field types.
struct Ctor {
    tag: u32,
    name: Symbol,
    fields: Vec<Ty>,
}

/// A cap on the type-graph walk depth, guarding against pathological non-regular
/// recursion (a type whose monomorphic field types never repeat).
const MAX_REACH: usize = 1000;

/// Whether `adt` is one of the built-in wrapper ADTs (`Option`/`Result`), which
/// are generated by the std combinators rather than synthesized per type.
fn is_builtin_adt(adt: AdtRef) -> bool {
    matches!(adt.name.as_str(), "Option" | "Result")
}

/// The element type of a sequence (`List T` or `Array T`) and the budget-splitting
/// generator combinator to use for a recursive field of that type.
fn sequence_element(ty: &Ty) -> Option<(&Ty, &'static str)> {
    let (head, args) = peel_app(ty);
    match (head, args.as_slice()) {
        (Ty::Con(Con::List), [elem]) => Some((elem, "recList")),
        (Ty::Con(Con::Array), [elem]) => Some((elem, "recArray")),
        _ => None,
    }
}

/// The `size` slice a recursive field receives when a constructor/record has `k`
/// recursive fields: `size - 1` split `k` ways, so the value stays within budget
/// (the `- 1` accounts for the node itself, keeping `k == 1` strictly smaller).
fn split_budget(size: LocalId, k: usize) -> CExpr {
    let dec = prim(Prim::IntSub, vec![local(size), int(1)]);
    if k <= 1 { dec } else { prim(Prim::IntDiv, vec![dec, int(i64::try_from(k).unwrap_or(1))]) }
}

/// The element type `T` of a value whose type is the `Arbitrary T` record
/// (`{ gen, show, shrink }`, sorted labels), recognized via `show : T -> String`.
fn arbitrary_element(ty: &Ty) -> Option<Ty> {
    let Ty::Record(row) = ty else { return None };
    if row.tail != RowEnd::Closed || row.fields.len() != 3 {
        return None;
    }
    let labels: Vec<&str> = row.fields.iter().map(|(l, _)| l.as_str()).collect();
    if labels != ["gen", "show", "shrink"] {
        return None;
    }
    match &row.fields[1].1 {
        Ty::Arrow(t, res, _) if matches!(&**res, Ty::Con(Con::String)) => Some((**t).clone()),
        _ => None,
    }
}

/// Whether a custom `Arbitrary T` is allowed to override synthesis for `T`: only
/// user records and ADTs (not built-in generators or the `Option`/`Result`
/// wrappers).
fn is_custom_eligible(ty: &Ty) -> bool {
    match ty {
        Ty::Record(_) => true,
        _ => matches!(peel_app(ty).0, Ty::Adt(adt) if !is_builtin_adt(*adt)),
    }
}

/// The field types of every constructor of `adt` applied to `args` (one inner
/// vec per constructor), or `None` if the type is unavailable or an alias.
fn adt_ctor_fields(
    db: &dyn Db,
    custom: &FxHashMap<Ty, DefId>,
    adt: AdtRef,
    args: &[&Ty],
) -> Option<Vec<Vec<Ty>>> {
    let adt_file = db.source_file(adt.file)?;
    let decls = type_decls(db, adt_file);
    let info = decls.type_named(adt.name).filter(|i| !i.is_alias)?;
    let mut out = Vec::with_capacity(info.ctors.len());
    for cname in &info.ctors {
        let scheme = constructor_scheme(db, adt_file, *cname)?;
        out.push(
            ctor_field_types(&scheme, args).iter().map(|f| expand_opaque(db, custom, f)).collect(),
        );
    }
    Some(out)
}

/// Deeply expands opaque aliases to their underlying types, leaving everything
/// else unchanged. Generating values for a property test legitimately needs an
/// opaque type's representation, so generation peeks past opacity here; a
/// user-supplied `Arbitrary` (a `custom` key) is honored as a leaf and never
/// expanded. Unions are untouched (they are not aliases).
fn expand_opaque(db: &dyn Db, custom: &FxHashMap<Ty, DefId>, ty: &Ty) -> Ty {
    if custom.contains_key(ty) {
        return ty.clone();
    }
    match ty {
        Ty::Tuple(es) => Ty::Tuple(es.iter().map(|e| expand_opaque(db, custom, e)).collect()),
        Ty::Record(row) => Ty::Record(RecordRow {
            fields: row.fields.iter().map(|(l, t)| (*l, expand_opaque(db, custom, t))).collect(),
            tail: row.tail,
        }),
        Ty::Arrow(a, b, e) => {
            Ty::arrow_eff(expand_opaque(db, custom, a), expand_opaque(db, custom, b), e.clone())
        }
        Ty::Adt(adt) => match fai_types::expand_alias_ty(db, *adt, &[]) {
            Some(body) => expand_opaque(db, custom, &body),
            None => ty.clone(),
        },
        Ty::App(..) => {
            let (head, args) = peel_app(ty);
            let args: Vec<Ty> = args.iter().map(|a| expand_opaque(db, custom, a)).collect();
            if let Ty::Adt(adt) = head
                && let Some(body) = fai_types::expand_alias_ty(db, *adt, &args)
            {
                return expand_opaque(db, custom, &body);
            }
            let mut t = head.clone();
            for a in args {
                t = Ty::App(Arc::new(t), Arc::new(a));
            }
            t
        }
        _ => ty.clone(),
    }
}

/// The smallest-value depth of `ty` (`None` ⇒ no finite value), reading ADT ranks
/// from `ranks` and treating custom-overridden types as opaque leaves.
///
/// Primitives, `List` (`[]`), and `Option` (`None`) ground at depth 0; `Result`
/// grounds through its `Ok` side; a tuple/record is as deep as its deepest
/// component; a user ADT's rank is the precomputed fixpoint value.
fn rank_of(ranks: &FxHashMap<Ty, u32>, custom: &FxHashMap<Ty, DefId>, ty: &Ty) -> Option<u32> {
    if custom.contains_key(ty) {
        return Some(0);
    }
    match ty {
        Ty::Unit | Ty::Con(_) => Some(0),
        Ty::Tuple(es) => max_rank(es.iter().map(|e| rank_of(ranks, custom, e))),
        Ty::Record(row) => max_rank(row.fields.iter().map(|(_, t)| rank_of(ranks, custom, t))),
        Ty::Var(_) | Ty::Arrow(..) | Ty::Interface(_) | Ty::Error => None,
        _ => {
            let (head, args) = peel_app(ty);
            match head {
                Ty::Con(Con::List | Con::Array) => Some(0),
                Ty::Adt(adt) if adt.name.as_str() == "Option" => Some(0),
                Ty::Adt(adt) if adt.name.as_str() == "Result" => {
                    args.first().and_then(|ok| rank_of(ranks, custom, ok))
                }
                Ty::Adt(_) => ranks.get(ty).copied(),
                _ => None,
            }
        }
    }
}

/// The max of ranks, or `None` if any input is `None` (an empty iterator ⇒ 0).
fn max_rank(ranks: impl Iterator<Item = Option<u32>>) -> Option<u32> {
    let mut worst = 0;
    for r in ranks {
        worst = worst.max(r?);
    }
    Some(worst)
}

/// The least-fixpoint rank of every user ADT reachable from `roots` (absent ⇒ no
/// finite value). Ranks start at ∞ and relax downward until stable, so a
/// mutually-recursive group settles on each member's true smallest-value depth.
fn compute_ranks(db: &dyn Db, custom: &FxHashMap<Ty, DefId>, roots: &[Ty]) -> FxHashMap<Ty, u32> {
    let mut adts: Vec<Ty> = Vec::new();
    let mut seen: FxHashSet<Ty> = FxHashSet::default();
    for r in roots {
        collect_adts(db, custom, r, &mut adts, &mut seen);
    }
    let mut ranks: FxHashMap<Ty, u32> = FxHashMap::default();
    loop {
        let mut changed = false;
        for adt_ty in &adts {
            if let Some(cost) = adt_min_cost(db, custom, &ranks, adt_ty) {
                let better = ranks.get(adt_ty).is_none_or(|&old| cost < old);
                if better {
                    ranks.insert(adt_ty.clone(), cost);
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    ranks
}

/// Collects the distinct user-ADT applied types reachable from `ty` (following
/// constructor fields and collection/wrapper arguments); custom-overridden types
/// are opaque leaves and are not entered.
fn collect_adts(
    db: &dyn Db,
    custom: &FxHashMap<Ty, DefId>,
    ty: &Ty,
    out: &mut Vec<Ty>,
    seen: &mut FxHashSet<Ty>,
) {
    if custom.contains_key(ty) || seen.len() > MAX_REACH {
        return;
    }
    match ty {
        Ty::Tuple(es) => es.iter().for_each(|e| collect_adts(db, custom, e, out, seen)),
        Ty::Record(row) => {
            row.fields.iter().for_each(|(_, t)| collect_adts(db, custom, t, out, seen));
        }
        _ => {
            let (head, args) = peel_app(ty);
            match head {
                Ty::Con(Con::List | Con::Array) => {
                    args.iter().for_each(|a| collect_adts(db, custom, a, out, seen));
                }
                Ty::Adt(adt) if is_builtin_adt(*adt) => {
                    args.iter().for_each(|a| collect_adts(db, custom, a, out, seen));
                }
                Ty::Adt(adt) => {
                    if !seen.insert(ty.clone()) {
                        return;
                    }
                    out.push(ty.clone());
                    if let Some(ctors) = adt_ctor_fields(db, custom, *adt, &args) {
                        for f in ctors.iter().flatten() {
                            collect_adts(db, custom, f, out, seen);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// The minimal constructor cost of an ADT applied type under the current `ranks`
/// (one deeper than the shallowest groundable constructor), or `None` if no
/// constructor is yet groundable.
fn adt_min_cost(
    db: &dyn Db,
    custom: &FxHashMap<Ty, DefId>,
    ranks: &FxHashMap<Ty, u32>,
    adt_ty: &Ty,
) -> Option<u32> {
    let (head, args) = peel_app(adt_ty);
    let Ty::Adt(adt) = head else { return None };
    let ctors = adt_ctor_fields(db, custom, *adt, &args)?;
    let mut best: Option<u32> = None;
    for fields in &ctors {
        if let Some(depth) = max_rank(fields.iter().map(|f| rank_of(ranks, custom, f))) {
            best = Some(best.map_or(depth + 1, |b| b.min(depth + 1)));
        }
    }
    best
}

/// The (monomorphic) field types of a constructor whose owning ADT is applied to
/// `concrete` arguments: peel the scheme's arrows for the field types, map the
/// result type's variables to `concrete`, and substitute.
fn ctor_field_types(scheme: &Scheme, concrete: &[&Ty]) -> Vec<Ty> {
    let (fields, result) = peel_arrows(&scheme.ty);
    let (_, result_args) = peel_app(result);
    let mut subst: FxHashMap<TyVarId, Ty> = FxHashMap::default();
    for (i, arg) in result_args.iter().enumerate() {
        if let Ty::Var(v) = arg
            && let Some(c) = concrete.get(i)
        {
            subst.insert(*v, (*c).clone());
        }
    }
    fields.iter().map(|f| subst_ty(f, &subst)).collect()
}

/// The head and argument types of a (curried) type application.
fn peel_app(ty: &Ty) -> (&Ty, Vec<&Ty>) {
    let mut args = Vec::new();
    let mut cur = ty;
    while let Ty::App(f, a) = cur {
        args.push(&**a);
        cur = f;
    }
    args.reverse();
    (cur, args)
}

/// The argument types and result of a (curried) function type.
fn peel_arrows(ty: &Ty) -> (Vec<&Ty>, &Ty) {
    let mut args = Vec::new();
    let mut cur = ty;
    while let Ty::Arrow(f, t, _) = cur {
        args.push(&**f);
        cur = t;
    }
    (args, cur)
}

/// Substitutes type variables in `ty` per `map`.
fn subst_ty(ty: &Ty, map: &FxHashMap<TyVarId, Ty>) -> Ty {
    use std::sync::Arc;
    match ty {
        Ty::Var(v) => map.get(v).cloned().unwrap_or_else(|| ty.clone()),
        Ty::App(f, a) => Ty::App(Arc::new(subst_ty(f, map)), Arc::new(subst_ty(a, map))),
        Ty::Arrow(f, t, e) => Ty::arrow_eff(subst_ty(f, map), subst_ty(t, map), e.clone()),
        Ty::Tuple(es) => Ty::Tuple(es.iter().map(|e| subst_ty(e, map)).collect()),
        Ty::Record(row) => Ty::Record(RecordRow {
            fields: row.fields.iter().map(|(l, t)| (*l, subst_ty(t, map))).collect(),
            tail: row.tail,
        }),
        Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::EffectArg(_) | Ty::Unit | Ty::Error => {
            ty.clone()
        }
    }
}

/// Whether `ty`'s head is a user/nominal ADT (so its rendering may contain spaces
/// and needs parentheses as a constructor argument).
fn is_adt(ty: &Ty) -> bool {
    matches!(peel_app(ty).0, Ty::Adt(_))
}

// --- Core IR construction helpers ------------------------------------------

fn global(def: DefId) -> CExpr {
    CExpr::new(K::Global(def), Ty::Error)
}

fn int(n: i64) -> CExpr {
    CExpr::new(K::Lit(Lit::Int(n)), Ty::Error)
}

fn prim(op: Prim, args: Vec<CExpr>) -> CExpr {
    CExpr::new(K::Prim { op, args }, Ty::Error)
}

fn if_(cond: CExpr, then: CExpr, els: CExpr) -> CExpr {
    CExpr::new(K::If { cond: Box::new(cond), then: Box::new(then), els: Box::new(els) }, Ty::Error)
}

fn data_tag(base: CExpr) -> CExpr {
    CExpr::new(K::DataTag { base: Box::new(base), niche: None }, Ty::Error)
}

fn local(l: LocalId) -> CExpr {
    CExpr::new(K::Local(l), Ty::Error)
}

fn app(func: CExpr, args: Vec<CExpr>) -> CExpr {
    CExpr::new(
        K::App {
            func: Box::new(func),
            args,
            reuse: Vec::new(),
            alloc: fai_core::ir::ClosureAlloc::Heap,
        },
        Ty::Error,
    )
}

fn make_data(tag: u32, args: Vec<CExpr>, scalars: u64) -> CExpr {
    // Generated values use the standard boxed representation; the contract harness
    // converts to a niche `Option` at the property's call boundary if needed.
    CExpr::new(K::MakeData { tag, args, reuse: None, scalars, niche: None }, Ty::Error)
}

/// Projects a field, always as a uniform read: a scalar `f64` slot of a generated
/// value is boxed by the runtime, giving the uniform value the generic `Gen`/`Show`
/// machinery (typed over a type variable) expects.
fn field(base: CExpr, index: u32) -> CExpr {
    CExpr::new(
        K::DataField {
            base: Box::new(base),
            index: FieldIndex::Const(index),
            scalar: false,
            niche: None,
        },
        Ty::Error,
    )
}

fn make_closure(func: u32) -> CExpr {
    CExpr::new(
        K::MakeClosure {
            func: fai_core::ir::FnId(func),
            captures: Vec::new(),
            alloc: fai_core::ir::ClosureAlloc::Static,
        },
        Ty::Error,
    )
}

fn str_lit(s: &str) -> CExpr {
    CExpr::new(K::Lit(Lit::Str(s.as_bytes().to_vec())), Ty::Error)
}

fn concat_all(mut parts: Vec<CExpr>) -> CExpr {
    let mut acc = parts.pop().unwrap_or_else(|| str_lit(""));
    while let Some(p) = parts.pop() {
        acc = CExpr::new(K::Prim { op: Prim::StrConcat, args: vec![p, acc] }, Ty::Error);
    }
    acc
}

fn lets(binds: Vec<(LocalId, CExpr)>, body: CExpr) -> CExpr {
    let mut acc = body;
    for (local, value) in binds.into_iter().rev() {
        acc = CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(acc) }, Ty::Error);
    }
    acc
}

fn fresh(next: &mut usize) -> LocalId {
    let id = LocalId::from_index(*next);
    *next += 1;
    id
}

#[cfg(test)]
mod tests {
    use fai_types::RowVarId;

    use super::*;

    fn sym(s: &str) -> Symbol {
        Symbol::intern(s)
    }

    fn arrow(from: Ty, to: Ty) -> Ty {
        Ty::arrow(from, to)
    }

    fn user_adt(name: &str) -> Ty {
        Ty::Adt(AdtRef::new(SourceId::new(0), sym(name)))
    }

    /// An `Arbitrary T` record (only the `show` field's shape is inspected).
    fn arbitrary_record(elem: Ty) -> Ty {
        Ty::Record(RecordRow {
            fields: vec![
                (sym("gen"), Ty::Error),
                (sym("show"), arrow(elem, Ty::Con(Con::String))),
                (sym("shrink"), Ty::Error),
            ],
            tail: RowEnd::Closed,
        })
    }

    #[test]
    fn arbitrary_element_extracts_the_generated_type() {
        assert_eq!(arbitrary_element(&arbitrary_record(user_adt("Rose"))), Some(user_adt("Rose")));
    }

    #[test]
    fn arbitrary_element_rejects_an_open_row() {
        let Ty::Record(mut row) = arbitrary_record(Ty::int()) else { unreachable!() };
        row.tail = RowEnd::Open(RowVarId(0));
        assert_eq!(arbitrary_element(&Ty::Record(row)), None);
    }

    #[test]
    fn arbitrary_element_rejects_a_non_arbitrary_record() {
        let ty = Ty::Record(RecordRow {
            fields: vec![(sym("x"), Ty::int()), (sym("y"), Ty::int())],
            tail: RowEnd::Closed,
        });
        assert_eq!(arbitrary_element(&ty), None);
    }

    #[test]
    fn arbitrary_element_rejects_when_show_does_not_return_string() {
        let ty = Ty::Record(RecordRow {
            fields: vec![
                (sym("gen"), Ty::Error),
                (sym("show"), arrow(Ty::int(), Ty::int())),
                (sym("shrink"), Ty::Error),
            ],
            tail: RowEnd::Closed,
        });
        assert_eq!(arbitrary_element(&ty), None);
    }

    #[test]
    fn sequence_element_unwraps_list_and_array() {
        let elem = user_adt("Rose");
        assert_eq!(sequence_element(&Ty::list(user_adt("Rose"))), Some((&elem, "recList")));
        assert_eq!(sequence_element(&Ty::array(user_adt("Rose"))), Some((&elem, "recArray")));
    }

    #[test]
    fn sequence_element_rejects_a_non_sequence() {
        assert_eq!(sequence_element(&Ty::int()), None);
        assert_eq!(sequence_element(&user_adt("Rose")), None);
    }

    #[test]
    fn custom_overrides_apply_to_user_records_and_adts() {
        assert!(is_custom_eligible(&user_adt("Rose")));
        assert!(is_custom_eligible(&Ty::Record(RecordRow {
            fields: Vec::new(),
            tail: RowEnd::Closed
        })));
    }

    #[test]
    fn custom_overrides_skip_builtins_and_wrappers() {
        assert!(!is_custom_eligible(&user_adt("Option")));
        assert!(!is_custom_eligible(&user_adt("Result")));
        assert!(!is_custom_eligible(&Ty::int()));
        assert!(!is_custom_eligible(&Ty::list(Ty::int())));
    }

    #[test]
    fn split_budget_for_one_recursive_field_just_decrements() {
        let e = split_budget(LocalId::from_index(0), 1);
        assert!(matches!(e.kind, K::Prim { op: Prim::IntSub, .. }));
    }

    #[test]
    fn split_budget_for_several_recursive_fields_divides() {
        let e = split_budget(LocalId::from_index(0), 3);
        assert!(matches!(e.kind, K::Prim { op: Prim::IntDiv, .. }));
    }

    #[test]
    fn max_rank_propagates_none() {
        assert_eq!(max_rank([Some(1), None, Some(2)].into_iter()), None);
    }

    #[test]
    fn max_rank_takes_the_deepest() {
        assert_eq!(max_rank([Some(1), Some(3), Some(2)].into_iter()), Some(3));
    }

    #[test]
    fn max_rank_of_no_fields_is_zero() {
        assert_eq!(max_rank(std::iter::empty()), Some(0));
    }
}

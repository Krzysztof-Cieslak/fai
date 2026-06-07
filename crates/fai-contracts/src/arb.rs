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

use fai_core::ir::{CExpr, CoreFn, ExprKind as K, FieldIndex, Lit, LoweredDef, Prim};
use fai_db::Db;
use fai_resolve::{AdtRef, DefId, LocalId, ModuleName, module_file, type_decls};
use fai_span::SourceId;
use fai_syntax::Symbol;
use fai_types::{Con, RecordRow, RowEnd, Scheme, Ty, TyVarId, constructor_scheme};
use rustc_hash::FxHashMap;

use crate::synth::NotRunnable;

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
    /// The file the synthesized defs belong to (the contract's source).
    source: SourceId,
    /// A prefix making synthesized names unique across a file's contracts.
    prefix: String,
    /// Counter for unique synthesized-def names.
    counter: usize,
    /// Already-synthesized arbitraries, by type (also breaks recursion).
    seen: FxHashMap<Ty, DefId>,
    /// The synthesized definitions and their runtime arities.
    pub defs: Vec<(LoweredDef, usize)>,
}

impl<'a> ArbBuilder<'a> {
    /// Creates a builder; `prefix` (e.g. `contract#3`) namespaces synthesized defs.
    pub fn new(db: &'a dyn Db, source: SourceId, prefix: String) -> Self {
        ArbBuilder { db, source, prefix, counter: 0, seen: FxHashMap::default(), defs: Vec::new() }
    }

    /// A `DefId` in the contract's file with a unique synthesized name.
    fn fresh_def(&mut self, what: &str) -> DefId {
        let name = format!("{}${what}#{}", self.prefix, self.counter);
        self.counter += 1;
        DefId::new(self.source, Symbol::intern(&name))
    }

    /// Resolves a `Module.name` standard-library definition.
    fn std(&self, module: &str, name: &str) -> Result<DefId, NotRunnable> {
        let m = module_file(self.db, ModuleName(Symbol::intern(module))).ok_or_else(|| {
            NotRunnable { reason: format!("the std `{module}` module is missing") }
        })?;
        Ok(DefId::new(m.source(self.db), Symbol::intern(name)))
    }

    fn test(&self, name: &str) -> Result<CExpr, NotRunnable> {
        Ok(global(self.std("Test", name)?))
    }

    /// The `Arbitrary` expression for `ty`: a combinator composition for built-in
    /// types, or a `Global` reference to a synthesized definition for a
    /// record/ADT (which it generates on demand).
    pub fn arb_for(&mut self, ty: &Ty) -> Result<CExpr, NotRunnable> {
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
            Ty::Con(Con::String) => self.test("string"),
            Ty::Unit => self.test("unit"),
            Ty::Con(Con::List) if args.len() == 1 => {
                Ok(app(self.test("list")?, vec![self.arb_for(args[0])?]))
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
        Err(NotRunnable {
            reason: format!(
                "cannot generate values of type `{}`",
                fai_types::render(ty, &fai_types::VarNames::new())
            ),
        })
    }

    /// Ensures a synthesized `Arbitrary` for a closed record type and returns it.
    fn ensure_record(&mut self, ty: &Ty, row: &RecordRow) -> Result<DefId, NotRunnable> {
        if let Some(def) = self.seen.get(ty) {
            return Ok(*def);
        }
        let arb_def = self.fresh_def("arb");
        self.seen.insert(ty.clone(), arb_def);

        // The per-field arbitraries (as expressions referenced from each function).
        let field_arbs: Vec<CExpr> =
            row.fields.iter().map(|(_, t)| self.arb_for(t)).collect::<Result<_, _>>()?;
        let n = row.fields.len();

        let gen_fn = self.record_gen(&field_arbs, n);
        let show_fn = self.record_show(row, &field_arbs);
        let shrink_fn = self.record_shrink(ty, row, &field_arbs)?;
        self.push_arbitrary(arb_def, gen_fn, show_fn, shrink_fn);
        Ok(arb_def)
    }

    /// `fun size seed -> let (v0, s1) = a0.gen size seed in … ({ … }, sN)`.
    fn record_gen(&mut self, field_arbs: &[CExpr], n: usize) -> CoreFn {
        let mut next = 2; // 0 = size, 1 = seed
        let size = LocalId::from_index(0);
        let mut seed = LocalId::from_index(1);
        let mut binds: Vec<(LocalId, CExpr)> = Vec::new();
        let mut values: Vec<CExpr> = Vec::with_capacity(n);
        for arb in field_arbs.iter().take(n) {
            let pair = fresh(&mut next);
            let value = fresh(&mut next);
            let next_seed = fresh(&mut next);
            binds.push((pair, app(field(arb.clone(), GEN), vec![local(size), local(seed)])));
            binds.push((value, field(local(pair), PAIR_VALUE)));
            binds.push((next_seed, field(local(pair), PAIR_SEED)));
            values.push(local(value));
            seed = next_seed;
        }
        let record = make_data(0, values);
        let result = make_data(0, vec![record, local(seed)]);
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
        let mut acc = make_data(0, Vec::new()); // []
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
        let body = make_data(0, fields);
        self.defs.push((
            LoweredDef {
                def,
                fns: vec![CoreFn { params: vec![r, v], captures: Vec::new(), body }],
                entry_borrowed: Vec::new(),
            },
            2,
        ));
        def
    }

    /// Ensures a synthesized `Arbitrary` for a (possibly recursive) ADT applied
    /// to `args`, and returns its definition.
    fn ensure_adt(&mut self, ty: &Ty, adt: AdtRef, args: &[&Ty]) -> Result<DefId, NotRunnable> {
        if let Some(def) = self.seen.get(ty) {
            return Ok(*def);
        }
        let arb_def = self.fresh_def("arb");
        self.seen.insert(ty.clone(), arb_def); // before building, so recursion self-refers

        let unknown = || NotRunnable { reason: format!("type `{}` is unavailable", adt.name) };
        let adt_file = self.db.source_file(adt.file).ok_or_else(unknown)?;
        let decls = type_decls(self.db, adt_file);
        let info = decls.type_named(adt.name).filter(|i| !i.is_alias).ok_or_else(unknown)?;
        let ctor_names = info.ctors.clone();
        let mut ctors = Vec::with_capacity(ctor_names.len());
        for cname in ctor_names {
            let scheme = constructor_scheme(self.db, adt_file, cname).ok_or_else(unknown)?;
            let tag = decls.ctor(cname).map_or(0, |c| c.tag);
            ctors.push(Ctor { tag, name: cname, fields: ctor_field_types(&scheme, args) });
        }
        // A non-recursive constructor (no field of the ADT's own type) is needed
        // to terminate generation at size 0.
        if !ctors.iter().any(|c| c.fields.iter().all(|f| f != ty)) {
            return Err(NotRunnable {
                reason: format!("type `{}` has no non-recursive constructor", adt.name),
            });
        }

        let gen_fn = self.adt_gen(ty, &ctors)?;
        let show_fn = self.adt_show(&ctors)?;
        let shrink_fn = self.adt_shrink(ty, &ctors)?;
        self.push_arbitrary(arb_def, gen_fn, show_fn, shrink_fn);
        Ok(arb_def)
    }

    /// `fun size seed -> if size <= 0 then <choose a terminal> else <choose any>`.
    fn adt_gen(&mut self, ty: &Ty, ctors: &[Ctor]) -> Result<CoreFn, NotRunnable> {
        let size = LocalId::from_index(0);
        let seed = LocalId::from_index(1);
        let mut next = 2;
        let terminals: Vec<&Ctor> =
            ctors.iter().filter(|c| c.fields.iter().all(|f| f != ty)).collect();
        let all: Vec<&Ctor> = ctors.iter().collect();
        let small = self.choose_among(&terminals, ty, size, seed, &mut next)?;
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

    /// Builds one constructor value, returning the `(value, seed)` pair; recursive
    /// fields are generated at `size - 1`.
    fn build_ctor(
        &mut self,
        ctor: &Ctor,
        ty: &Ty,
        size: LocalId,
        seed: LocalId,
        next: &mut usize,
    ) -> Result<CExpr, NotRunnable> {
        if ctor.fields.is_empty() {
            return Ok(make_data(0, vec![make_data(ctor.tag, Vec::new()), local(seed)]));
        }
        let mut binds = Vec::new();
        let mut values = Vec::with_capacity(ctor.fields.len());
        let mut cur = seed;
        for ft in &ctor.fields {
            let arb = self.arb_for(ft)?;
            let pair = fresh(next);
            let value = fresh(next);
            let next_seed = fresh(next);
            let size_arg =
                if ft == ty { prim(Prim::IntSub, vec![local(size), int(1)]) } else { local(size) };
            binds.push((pair, app(field(arb, GEN), vec![size_arg, local(cur)])));
            binds.push((value, field(local(pair), PAIR_VALUE)));
            binds.push((next_seed, field(local(pair), PAIR_SEED)));
            values.push(local(value));
            cur = next_seed;
        }
        let made = make_data(ctor.tag, values);
        Ok(lets(binds, make_data(0, vec![made, local(cur)])))
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
        let mut chain = make_data(0, Vec::new()); // []
        for ctor in ctors.iter().rev() {
            let mut acc = make_data(0, Vec::new());
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
                    acc = make_data(1, vec![sub, acc]); // sub :: acc
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
        let body = make_data(ctor.tag, fields);
        self.defs.push((
            LoweredDef {
                def,
                fns: vec![CoreFn { params: vec![v, new], captures: Vec::new(), body }],
                entry_borrowed: Vec::new(),
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
            ),
        };
        self.defs.push((
            LoweredDef {
                def,
                fns: vec![entry, gen_fn, show_fn, shrink_fn],
                entry_borrowed: Vec::new(),
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
    while let Ty::Arrow(f, t) = cur {
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
        Ty::Arrow(f, t) => Ty::Arrow(Arc::new(subst_ty(f, map)), Arc::new(subst_ty(t, map))),
        Ty::Tuple(es) => Ty::Tuple(es.iter().map(|e| subst_ty(e, map)).collect()),
        Ty::Record(row) => Ty::Record(RecordRow {
            fields: row.fields.iter().map(|(l, t)| (*l, subst_ty(t, map))).collect(),
            tail: row.tail,
        }),
        Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::Unit | Ty::Error => ty.clone(),
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
    CExpr::new(K::DataTag(Box::new(base)), Ty::Error)
}

fn local(l: LocalId) -> CExpr {
    CExpr::new(K::Local(l), Ty::Error)
}

fn app(func: CExpr, args: Vec<CExpr>) -> CExpr {
    CExpr::new(K::App { func: Box::new(func), args }, Ty::Error)
}

fn make_data(tag: u32, args: Vec<CExpr>) -> CExpr {
    CExpr::new(K::MakeData { tag, args, reuse: None }, Ty::Error)
}

fn field(base: CExpr, index: u32) -> CExpr {
    CExpr::new(K::DataField { base: Box::new(base), index: FieldIndex::Const(index) }, Ty::Error)
}

fn make_closure(func: u32) -> CExpr {
    CExpr::new(K::MakeClosure { func: fai_core::ir::FnId(func), captures: Vec::new() }, Ty::Error)
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

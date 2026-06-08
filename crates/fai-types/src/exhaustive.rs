//! Exhaustiveness and redundancy checking for `match`, via Maranget's
//! usefulness algorithm.
//!
//! A `match` is exhaustive iff a fresh wildcard row is *not* useful against the
//! matrix of its arm patterns (no value escapes every arm). An arm is redundant
//! iff its pattern is *not* useful against the arms above it. The algorithm needs
//! each column's type to know a constructor set is complete, so column types are
//! threaded alongside the pattern matrix.

use std::rc::Rc;

use fai_db::{Db, SourceFile, emit};
use fai_diagnostics::Diagnostic;
use fai_resolve::{Res, ResolvedBodies, resolve, type_decls};
use fai_span::Span;
use fai_syntax::ast::{ExprId, ExprKind, ItemKind, MatchArm, Module, PatId, PatKind};

use crate::infer::{InferCtx, SolveTy};
use crate::ty::{Con, Ty, TyVarId};
use crate::{NON_EXHAUSTIVE_MATCH, UNREACHABLE_ARM, body_types};

/// The built-in `List` constructor tags.
const NIL_TAG: i64 = 0;
const CONS_TAG: i64 = 1;

/// A finite-domain constructor key (used for coverage/completeness).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConKey {
    /// A tagged constructor (ADT variant, list `Nil`/`Cons`, `Bool`).
    Tag(i64),
    /// A tuple of the given arity.
    Tuple,
    /// The unit value.
    Unit,
}

/// An internal pattern for the usefulness algorithm.
#[derive(Debug, Clone)]
enum IPat {
    Wild,
    Con { key: ConKey, args: Vec<IPat> },
    Lit(String),
    Or(Vec<IPat>),
}

/// One constructor of a type's complete signature.
#[derive(Debug, Clone)]
struct SigCtor {
    key: ConKey,
    /// A display name for witnesses (e.g. `Some`, `Cons`).
    name: String,
    /// Field types, in order (for the sub-columns after specialization).
    fields: Vec<Ty>,
}

/// A type's constructor signature.
enum Sig {
    /// A finite, enumerable set of constructors.
    Finite(Vec<SigCtor>),
    /// An effectively infinite domain (numeric/string/char literals, or an
    /// unknown type) — only a wildcard is exhaustive.
    Infinite,
}

/// Checks every `match` in `file`'s definition bodies for exhaustiveness and
/// redundancy.
pub fn check_matches(db: &dyn Db, file: SourceFile) {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let resolved = resolve(db, file);
    let mut scope: Vec<fai_syntax::Symbol> = Vec::new();
    check_matches_in(db, file, module, &resolved, &mut scope, &module.roots);
}

/// Checks the `match`es of one module scope's bindings (by their qualified
/// names, so `body_types` is keyed correctly), recursing into nested modules.
fn check_matches_in(
    db: &dyn Db,
    file: SourceFile,
    module: &Module,
    resolved: &ResolvedBodies,
    scope: &mut Vec<fai_syntax::Symbol>,
    items: &[fai_syntax::ast::ItemId],
) {
    for &id in items {
        match &module.items[id.index()].kind {
            ItemKind::Binding { name, body, .. } => {
                let qual = fai_resolve::qualify(scope, *name);
                let body = *body;
                let types = body_types(db, file, qual);
                let mut cx = MatchChecker { db, file, module, resolved, types: &types };
                cx.walk(body);
            }
            ItemKind::Module { name, body } => {
                scope.push(*name);
                check_matches_in(db, file, module, resolved, scope, body);
                scope.pop();
            }
            _ => {}
        }
    }
}

struct MatchChecker<'a> {
    db: &'a dyn Db,
    file: SourceFile,
    module: &'a Module,
    resolved: &'a ResolvedBodies,
    types: &'a crate::query::BodyTypes,
}

impl MatchChecker<'_> {
    /// Recursively visits expressions, checking each `match`.
    fn walk(&mut self, expr: ExprId) {
        match &self.module.expr(expr).kind {
            ExprKind::Match { scrutinee, arms } => {
                self.check_match(*scrutinee, arms);
                self.walk(*scrutinee);
                for arm in arms {
                    self.walk(arm.body);
                }
            }
            ExprKind::App { func, arg } => {
                self.walk(*func);
                self.walk(*arg);
            }
            ExprKind::Infix { lhs, rhs, .. } => {
                self.walk(*lhs);
                self.walk(*rhs);
            }
            ExprKind::Prefix { operand, .. } | ExprKind::Paren(operand) => self.walk(*operand),
            ExprKind::If { cond, then_branch, else_branch } => {
                self.walk(*cond);
                self.walk(*then_branch);
                self.walk(*else_branch);
            }
            ExprKind::Lambda { body, .. } => self.walk(*body),
            ExprKind::Field { base, .. } => self.walk(*base),
            ExprKind::Block { stmts, tail } => {
                for stmt in stmts {
                    self.walk(stmt.value);
                }
                self.walk(*tail);
            }
            ExprKind::Tuple(xs) | ExprKind::List(xs) => xs.iter().for_each(|&x| self.walk(x)),
            _ => {}
        }
    }

    fn check_match(&mut self, scrutinee: ExprId, arms: &[MatchArm]) {
        let scrutinee_ty = self.types.get(scrutinee).cloned().unwrap_or(Ty::Error);

        // Build the matrix, expanding or-patterns into separate rows.
        let mut matrix: Vec<Vec<IPat>> = Vec::new();
        for arm in arms {
            let rows = expand_or(self.lower_pat(arm.pat));
            // An arm is redundant when none of its rows is useful against the
            // matrix built from earlier arms.
            let useful_here = rows.iter().any(|row| {
                self.useful(&matrix, std::slice::from_ref(row), std::slice::from_ref(&scrutinee_ty))
            });
            if !useful_here {
                emit(
                    self.db,
                    Diagnostic::error(
                        UNREACHABLE_ARM,
                        "this match arm is unreachable",
                        Span::new(self.file.source(self.db), arm.span),
                    ),
                );
            }
            for row in rows {
                matrix.push(vec![row]);
            }
        }

        // Exhaustiveness: a wildcard row is useful iff some value escapes.
        if self.useful(&matrix, &[IPat::Wild], std::slice::from_ref(&scrutinee_ty)) {
            let span = self.module.expr(scrutinee).span;
            let help = self.missing_hint(&matrix, &scrutinee_ty);
            emit(
                self.db,
                Diagnostic::error(
                    NON_EXHAUSTIVE_MATCH,
                    "this match does not cover every case",
                    Span::new(self.file.source(self.db), span),
                )
                .with_help(help),
            );
        }
    }

    /// A help message naming the missing top-level constructors, when the
    /// scrutinee is a finite type with some constructor unhandled.
    fn missing_hint(&self, matrix: &[Vec<IPat>], scrutinee_ty: &Ty) -> String {
        if let Sig::Finite(ctors) = self.signature_of(scrutinee_ty) {
            let present = head_keys(matrix);
            let missing: Vec<&str> = ctors
                .iter()
                .filter(|c| !present.contains(&c.key))
                .map(|c| c.name.as_str())
                .collect();
            if !missing.is_empty() {
                return format!("add arms for: {}", missing.join(", "));
            }
        }
        "add the missing arms, or a `_` catch-all".to_owned()
    }

    /// Lowers a surface pattern to an [`IPat`] (type-independent: list literals
    /// desugar to `Cons`/`Nil`, constructor tags come from resolution).
    fn lower_pat(&self, pat: PatId) -> IPat {
        match &self.module.pat(pat).kind {
            PatKind::Var(_) | PatKind::Wildcard | PatKind::Error => IPat::Wild,
            PatKind::Unit => IPat::Con { key: ConKey::Unit, args: Vec::new() },
            PatKind::Paren(inner) => self.lower_pat(*inner),
            // An as-pattern covers exactly what its inner pattern covers.
            PatKind::As { pat: inner, .. } => self.lower_pat(*inner),
            PatKind::Bool(b) => IPat::Con { key: ConKey::Tag(i64::from(*b)), args: Vec::new() },
            PatKind::Int(s) | PatKind::Float(s) | PatKind::String(s) | PatKind::Char(s) => {
                IPat::Lit(s.as_str().to_owned())
            }
            PatKind::Tuple(elems) => IPat::Con {
                key: ConKey::Tuple,
                args: elems.iter().map(|&e| self.lower_pat(e)).collect(),
            },
            PatKind::Cons { head, tail } => IPat::Con {
                key: ConKey::Tag(CONS_TAG),
                args: vec![self.lower_pat(*head), self.lower_pat(*tail)],
            },
            PatKind::List(elems) => {
                let mut list = IPat::Con { key: ConKey::Tag(NIL_TAG), args: Vec::new() };
                for &e in elems.iter().rev() {
                    list = IPat::Con {
                        key: ConKey::Tag(CONS_TAG),
                        args: vec![self.lower_pat(e), list],
                    };
                }
                list
            }
            PatKind::Constructor { args, .. } => {
                // An unresolved constructor (a typo, or one whose declaration is
                // missing) has no known tag or arity. Lowering it to tag 0 would
                // collide with the real first constructor while carrying a
                // different field count, leaving an arity-inconsistent matrix row.
                // The unbound name is already reported; treat the pattern as a
                // distinct, unmatchable value (as refutable records are below) so
                // it neither claims coverage nor corrupts the matrix.
                let Some((tag, arity)) = self.ctor_tag_arity(pat) else {
                    return IPat::Lit(format!("@unresolved{}", pat.index()));
                };
                // Normalize to the declared arity; an arity error is reported by
                // type checking, and keeping the matrix consistent avoids panics.
                let mut sub: Vec<IPat> = args.iter().map(|&a| self.lower_pat(a)).collect();
                sub.resize(arity, IPat::Wild);
                IPat::Con { key: ConKey::Tag(i64::from(tag)), args: sub }
            }
            PatKind::Or(alts) => IPat::Or(alts.iter().map(|&a| self.lower_pat(a)).collect()),
            // Records are single-constructor: an irrefutable record pattern (all
            // sub-patterns irrefutable) acts as a wildcard; a refutable one is
            // treated as a distinct value (sound — it under-claims coverage).
            PatKind::Record { .. } => {
                if self.is_irrefutable(pat) {
                    IPat::Wild
                } else {
                    IPat::Lit(format!("@record{}", pat.index()))
                }
            }
        }
    }

    /// The declared `(tag, arity)` of a resolved constructor pattern, or `None`
    /// when the constructor is unresolved or has no declaration (so the matrix
    /// must not treat it as a real, tag-bearing constructor).
    fn ctor_tag_arity(&self, pat: PatId) -> Option<(u32, usize)> {
        let Some(Res::Ctor(ctor)) = self.resolved.pat_res(pat) else { return None };
        let decls = type_decls(self.db, self.ctor_file(ctor.file));
        let info = decls.ctor(ctor.name)?;
        Some((info.tag, info.arity))
    }

    /// Whether a pattern always matches (binds without testing).
    fn is_irrefutable(&self, pat: PatId) -> bool {
        match &self.module.pat(pat).kind {
            PatKind::Var(_) | PatKind::Wildcard | PatKind::Unit | PatKind::Error => true,
            PatKind::Paren(inner) => self.is_irrefutable(*inner),
            PatKind::Tuple(elems) => elems.iter().all(|&e| self.is_irrefutable(e)),
            PatKind::Record { fields, .. } => fields.iter().all(|f| self.is_irrefutable(f.pat)),
            PatKind::Or(alts) => alts.iter().any(|&a| self.is_irrefutable(a)),
            _ => false,
        }
    }

    fn ctor_file(&self, file: fai_span::SourceId) -> SourceFile {
        self.db.source_file(file).unwrap_or(self.file)
    }

    /// Whether `q` (a single-row pattern vector) is useful against `matrix`.
    fn useful(&self, matrix: &[Vec<IPat>], q: &[IPat], col_types: &[Ty]) -> bool {
        // Base case: no columns. Useful iff the matrix has no rows.
        let Some((head, rest)) = q.split_first() else {
            return matrix.is_empty();
        };
        let rest_types = &col_types[1..];
        match head {
            IPat::Or(alts) => alts.iter().any(|alt| {
                let mut q2 = vec![alt.clone()];
                q2.extend_from_slice(rest);
                self.useful(matrix, &q2, col_types)
            }),
            IPat::Con { key, args } => {
                let spec = self.specialize(matrix, key, args.len());
                let mut q2 = args.clone();
                q2.extend_from_slice(rest);
                let field_types = self.ctor_field_types(&col_types[0], key, args.len());
                let mut types2 = field_types;
                types2.extend_from_slice(rest_types);
                self.useful(&spec, &q2, &types2)
            }
            IPat::Lit(value) => {
                let spec = self.specialize_lit(matrix, value);
                self.useful(&spec, rest, rest_types)
            }
            IPat::Wild => self.useful_wild(matrix, rest, col_types),
        }
    }

    /// The wildcard case: if the first column's constructors form a complete
    /// signature, a value escapes only if it escapes under some constructor;
    /// otherwise it escapes via a missing constructor (the default matrix).
    fn useful_wild(&self, matrix: &[Vec<IPat>], rest: &[IPat], col_types: &[Ty]) -> bool {
        match self.signature_of(&col_types[0]) {
            Sig::Finite(ctors) => {
                let present = head_keys(matrix);
                let complete = ctors.iter().all(|c| present.contains(&c.key));
                if complete {
                    ctors.iter().any(|c| {
                        let spec = self.specialize(matrix, &c.key, c.fields.len());
                        let mut q2 = vec![IPat::Wild; c.fields.len()];
                        q2.extend_from_slice(rest);
                        let mut types2 = c.fields.clone();
                        types2.extend_from_slice(&col_types[1..]);
                        self.useful(&spec, &q2, &types2)
                    })
                } else {
                    self.useful(&default_matrix(matrix), rest, &col_types[1..])
                }
            }
            Sig::Infinite => self.useful(&default_matrix(matrix), rest, &col_types[1..]),
        }
    }

    /// Specialization `S(c, matrix)`: keep rows whose first pattern is `c` (with
    /// its arguments) or a wildcard (expanded to `arity` wildcards).
    fn specialize(&self, matrix: &[Vec<IPat>], key: &ConKey, arity: usize) -> Vec<Vec<IPat>> {
        let mut out = Vec::new();
        for row in matrix {
            // A row shorter than the column being matched can only come from an
            // already-reported ill-typed pattern (e.g. a wrong-arity constructor
            // or tuple). Skip it rather than panic.
            let Some((head, rest)) = row.split_first() else { continue };
            match head {
                IPat::Con { key: k, args } if k == key => {
                    let mut new = args.clone();
                    new.extend_from_slice(rest);
                    out.push(new);
                }
                IPat::Wild => {
                    let mut new = vec![IPat::Wild; arity];
                    new.extend_from_slice(rest);
                    out.push(new);
                }
                IPat::Or(alts) => {
                    for alt in expand_or(IPat::Or(alts.clone())) {
                        let mut expanded = vec![alt];
                        expanded.extend_from_slice(rest);
                        out.extend(self.specialize(&[expanded], key, arity));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Literal specialization: rows with the same literal, or a wildcard.
    fn specialize_lit(&self, matrix: &[Vec<IPat>], value: &str) -> Vec<Vec<IPat>> {
        let mut out = Vec::new();
        for row in matrix {
            // A row shorter than the column being matched can only come from an
            // already-reported ill-typed pattern (e.g. a wrong-arity constructor
            // or tuple). Skip it rather than panic.
            let Some((head, rest)) = row.split_first() else { continue };
            match head {
                IPat::Lit(v) if v == value => out.push(rest.to_vec()),
                IPat::Wild => out.push(rest.to_vec()),
                IPat::Or(alts) => {
                    for alt in expand_or(IPat::Or(alts.clone())) {
                        let mut expanded = vec![alt];
                        expanded.extend_from_slice(rest);
                        out.extend(self.specialize_lit(&[expanded], value));
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// The complete constructor signature of a type, if finite.
    fn signature_of(&self, ty: &Ty) -> Sig {
        let (head, args) = peel_ty(ty);
        match head {
            Ty::Con(Con::Bool) => Sig::Finite(vec![
                SigCtor { key: ConKey::Tag(0), name: "false".into(), fields: Vec::new() },
                SigCtor { key: ConKey::Tag(1), name: "true".into(), fields: Vec::new() },
            ]),
            Ty::Con(Con::List) => {
                let elem = args.first().cloned().unwrap_or(Ty::Error);
                Sig::Finite(vec![
                    SigCtor { key: ConKey::Tag(NIL_TAG), name: "[]".into(), fields: Vec::new() },
                    SigCtor {
                        key: ConKey::Tag(CONS_TAG),
                        name: "_ :: _".into(),
                        fields: vec![elem.clone(), Ty::list(elem)],
                    },
                ])
            }
            Ty::Unit => Sig::Finite(vec![SigCtor {
                key: ConKey::Unit,
                name: "()".into(),
                fields: Vec::new(),
            }]),
            Ty::Tuple(elems) => Sig::Finite(vec![SigCtor {
                key: ConKey::Tuple,
                name: "(…)".into(),
                fields: elems.clone(),
            }]),
            Ty::Adt(adt) => {
                let file = self.ctor_file(adt.file);
                let decls = type_decls(self.db, file);
                let Some(info) = decls.type_named(adt.name) else { return Sig::Infinite };
                let ctors = info
                    .ctors
                    .iter()
                    .filter_map(|cname| {
                        let cinfo = decls.ctor(*cname)?;
                        Some(SigCtor {
                            key: ConKey::Tag(i64::from(cinfo.tag)),
                            name: cname.as_str().to_owned(),
                            fields: self.ctor_field_types(
                                ty,
                                &ConKey::Tag(i64::from(cinfo.tag)),
                                cinfo.arity,
                            ),
                        })
                    })
                    .collect();
                Sig::Finite(ctors)
            }
            // Int/Float/String/Char/Runtime/var/error: not finitely enumerable.
            _ => Sig::Infinite,
        }
    }

    /// The field types of a constructor at a given (applied) scrutinee type. For
    /// the built-in finite types the fields follow directly; for ADTs the
    /// constructor scheme is instantiated and unified against the scrutinee type.
    fn ctor_field_types(&self, ty: &Ty, key: &ConKey, arity: usize) -> Vec<Ty> {
        let (head, args) = peel_ty(ty);
        match (head, key) {
            (Ty::Con(Con::List), ConKey::Tag(t)) if *t == CONS_TAG => {
                let elem = args.first().cloned().unwrap_or(Ty::Error);
                vec![elem.clone(), Ty::list(elem)]
            }
            (Ty::Tuple(elems), ConKey::Tuple) => elems.clone(),
            (Ty::Adt(adt), ConKey::Tag(tag)) => {
                let file = self.ctor_file(adt.file);
                let decls = type_decls(self.db, file);
                let cname = decls
                    .ctors
                    .values()
                    .find(|c| i64::from(c.tag) == *tag && c.adt == adt.name)
                    .map(|c| c.name);
                match cname.and_then(|n| crate::query::constructor_scheme(self.db, file, n)) {
                    Some(scheme) => instantiate_fields(&scheme, ty, arity),
                    None => vec![Ty::Error; arity],
                }
            }
            _ => vec![Ty::Error; arity],
        }
    }
}

/// Expands an or-pattern (at any depth in the first position) into a list of
/// or-free rows.
fn expand_or(pat: IPat) -> Vec<IPat> {
    match pat {
        IPat::Or(alts) => alts.into_iter().flat_map(expand_or).collect(),
        IPat::Con { key, args } => {
            // Cartesian product over the arguments' expansions.
            let mut rows: Vec<Vec<IPat>> = vec![Vec::new()];
            for arg in args {
                let expansions = expand_or(arg);
                let mut next = Vec::new();
                for row in &rows {
                    for e in &expansions {
                        let mut r = row.clone();
                        r.push(e.clone());
                        next.push(r);
                    }
                }
                rows = next;
            }
            rows.into_iter().map(|args| IPat::Con { key: key.clone(), args }).collect()
        }
        other => vec![other],
    }
}

/// The default matrix `D(matrix)`: rows with a wildcard first pattern, tails kept.
fn default_matrix(matrix: &[Vec<IPat>]) -> Vec<Vec<IPat>> {
    let mut out = Vec::new();
    for row in matrix {
        let (head, rest) = row.split_first().expect("non-empty row");
        match head {
            IPat::Wild => out.push(rest.to_vec()),
            IPat::Or(alts) => {
                for alt in expand_or(IPat::Or(alts.clone())) {
                    let mut expanded = vec![alt];
                    expanded.extend_from_slice(rest);
                    out.extend(default_matrix(&[expanded]));
                }
            }
            _ => {}
        }
    }
    out
}

/// The set of finite-constructor keys appearing in the matrix's first column.
fn head_keys(matrix: &[Vec<IPat>]) -> Vec<ConKey> {
    let mut keys = Vec::new();
    for row in matrix {
        if let Some(first) = row.first() {
            collect_keys(first, &mut keys);
        }
    }
    keys
}

fn collect_keys(pat: &IPat, out: &mut Vec<ConKey>) {
    match pat {
        IPat::Con { key, .. } => {
            if !out.contains(key) {
                out.push(key.clone());
            }
        }
        IPat::Or(alts) => alts.iter().for_each(|a| collect_keys(a, out)),
        _ => {}
    }
}

/// Peels a type application spine into its head and arguments.
fn peel_ty(ty: &Ty) -> (Ty, Vec<Ty>) {
    let mut args = Vec::new();
    let mut cur = ty.clone();
    while let Ty::App(f, a) = cur {
        args.push((*a).clone());
        cur = (*f).clone();
    }
    args.reverse();
    (cur, args)
}

/// Instantiates a constructor scheme and unifies its result with `ty` to recover
/// the field types at that scrutinee type.
fn instantiate_fields(scheme: &crate::ty::Scheme, ty: &Ty, arity: usize) -> Vec<Ty> {
    let mut cx = InferCtx::new();
    let ctor = cx.instantiate(scheme);
    let mut cur = ctor;
    let mut fields = Vec::new();
    for _ in 0..arity {
        match cx.resolve_shallow(&cur) {
            SolveTy::Arrow(from, to) => {
                fields.push(Rc::unwrap_or_clone(from));
                cur = Rc::unwrap_or_clone(to);
            }
            _ => break,
        }
    }
    let scrutinee = ty_to_solve(&mut cx, ty);
    let _ = cx.unify(&cur, &scrutinee);
    let reified: Vec<Ty> = fields.iter().map(|f| cx.reify(f)).collect();
    if reified.len() == arity { reified } else { vec![Ty::Error; arity] }
}

/// Converts a reified type to a solver type, mapping each variable to a fresh
/// solver variable (consistently).
fn ty_to_solve(cx: &mut InferCtx, ty: &Ty) -> SolveTy {
    let mut order: Vec<TyVarId> = Vec::new();
    collect_ty_vars(ty, &mut order);
    let scheme = crate::ty::Scheme::new(order, ty.clone());
    cx.instantiate(&scheme)
}

fn collect_ty_vars(ty: &Ty, out: &mut Vec<TyVarId>) {
    match ty {
        Ty::Var(v) => {
            if !out.contains(v) {
                out.push(*v);
            }
        }
        Ty::App(f, a) | Ty::Arrow(f, a) => {
            collect_ty_vars(f, out);
            collect_ty_vars(a, out);
        }
        Ty::Tuple(elems) => elems.iter().for_each(|e| collect_ty_vars(e, out)),
        Ty::Record(row) => row.fields.iter().for_each(|(_, t)| collect_ty_vars(t, out)),
        Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::Unit | Ty::Error => {}
    }
}

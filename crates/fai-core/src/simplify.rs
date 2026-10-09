//! Local reduction that confines composed and partially-applied closures.
//!
//! A point-free value built from `>>`/`|>` composition, `identity`/`const`, and
//! partial application — especially one bound at a top-level value (a *constant
//! applicative form*, e.g. `let transform = (fun x -> x + 1) >> shift 3`) — would
//! otherwise compile to reference-counted heap closures invoked through the
//! first-class `apply_n` path at every use site. Since such a value is not
//! memoized, a CAF referenced inside a loop body is even *rebuilt* per iteration.
//!
//! This pass contracts those redexes before reference counting, so escape analysis
//! and fusion see ordinary direct code instead of escaping closures. Five
//! behavior-preserving rewrites, applied to a fixpoint within each definition:
//!
//! * **CAF inlining** — a saturated-or-over application of a same-file,
//!   non-recursive, nullary, small value binding splices that binding's body in
//!   head position (relocating its lifted lambdas into the caller), so the value's
//!   construction meets its use and the remaining rules can fire.
//! * **Constant expansion** — a small, monomorphic, same-file literal value is
//!   also expanded in value position. Literal aggregates and capture-free
//!   dictionaries become visible to scalar replacement; scalar globals no longer
//!   need a uniform call and box on each read. A separate eligibility query cuts
//!   off dependencies on bodies that cannot be expanded.
//! * **Combinator reduction** — recognized by the resolved `Prelude` identities
//!   (never by reading a combinator's body, so a body edit can't change what
//!   reduces — the cross-module firewall): `(f >> g) x → g (f x)`, `x |> f → f x`,
//!   `identity x → x`, and `const a b → (let saved = a in let _ = b in saved)`
//!   (both arguments keep their strict, left-to-right evaluation). The reordered
//!   operands of `>>`/`|>` must be
//!   **pure and total**, mirroring fusion's reorder barrier, so an unsafe composition is
//!   left intact.
//! * **Application flattening** — `App(App(h, xs), ys) → App(h, xs ++ ys)`,
//!   re-normalizing spines the other rules split and collapsing a curried partial
//!   application into a saturated direct call.
//! * **Beta reduction** — a saturated-or-over application of a literal lambda
//!   (`MakeClosure`) inlines the lambda's body, binding arguments to fresh locals
//!   (so a multiply-used parameter is not duplicated and evaluation order is
//!   preserved) and mapping its captures to the supplied locals.
//!
//! It is **layered on [`core_inlined`]** and feeds [`helper_inlined`]: the small
//! same-file helpers a reduced composition leaves (e.g. a now-saturated `shift`
//! call) are folded by helper inlining, and a pipeline whose element function is
//! thereby reduced to arithmetic is then deforested into a register loop by
//! [`fuse_def`](crate::fuse). Only constant expansion runs inside the standard
//! library, so its combinators stay exercised by their own contracts.

use std::sync::Arc;

use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId, ModuleName, module_file, recursive_defs};
use fai_span::SourceId;
use fai_syntax::Symbol;
use fai_types::Ty;
use rustc_hash::FxHashMap;

use crate::core_inlined;
use crate::fuse::prune_dead_fns;
use crate::inline::{fresh_local, next_free_local, remap_expr, remap_local};
use crate::ir::{CExpr, ClosureAlloc, CoreFn, ExprKind as K, FnId, LoweredDef};

#[path = "choices.rs"]
mod choices;

/// The largest a CAF's body may be (total Core nodes across its entry and lifted
/// lambdas) to be inlined at an applied use. Matches the helper-inliner budget so
/// the two passes admit work of the same scale; a tunable bound, not a contract.
const CAF_NODE_BUDGET: usize = 64;

/// A defensive cap on the number of redex contractions performed per definition.
/// The rewrites strictly reduce a (composition/CAF/beta/nesting) measure over a
/// finite acyclic set, so this is only a totality backstop — hitting it leaves
/// residual redexes (still correct), never a hang.
const SIMPLIFY_STEP_BUDGET: u32 = 100_000;

/// The resolved `Prelude` definition ids of the combinators this pass recognizes.
/// Resolved from the module header (never a body), so it is independent of any
/// combinator-body edit (the firewall). A user-shadowed `>>`/`identity`/… resolves
/// to its own id at the call site, so it simply does not match these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CombinatorDefs {
    /// `>>` (left-to-right composition): `(f >> g) x = g (f x)`.
    compose: Option<DefId>,
    /// `|>` (forward application): `x |> f = f x`.
    pipe: Option<DefId>,
    /// `identity`: `identity x = x`.
    identity: Option<DefId>,
    /// `const`: `const a b = a`.
    konst: Option<DefId>,
}

/// Resolves the recognized combinators' definition ids from `Prelude`.
///
/// Reads only the module header (`module_file`) and forms `DefId`s by name, so it
/// does not depend on any combinator's body — editing `>>`'s body never changes
/// what this pass reduces (salsa early cutoff).
#[salsa::tracked]
pub fn combinator_defs(db: &dyn Db) -> Arc<CombinatorDefs> {
    let resolve = |name: &str| -> Option<DefId> {
        let f = module_file(db, ModuleName(Symbol::intern("Prelude")))?;
        Some(DefId::new(f.source(db), Symbol::intern(name)))
    };
    Arc::new(CombinatorDefs {
        compose: resolve(">>"),
        pipe: resolve("|>"),
        identity: resolve("identity"),
        konst: resolve("const"),
    })
}

/// `name`'s lowered definition with composed/partially-applied/CAF closures
/// confined by local reduction (see the module docs).
///
/// The base [`helper_inlined`](crate::helper_inlined) reads (in place of
/// [`core_inlined`]), so every back-end consumer sees the reduced form. Returns the
/// input lowering unchanged when nothing reduced (an O(1) salsa early cutoff for
/// the common definition with no composition/CAF). In the standard library only
/// small literal values are expanded.
#[salsa::tracked]
pub fn simplified(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<LoweredDef> {
    let base = core_inlined(db, file, name);
    let mut cx = Simplifier {
        db,
        file,
        source: file.source(db),
        combinators: combinator_defs(db),
        fns: base.fns.clone(),
        next: next_free_local(&base),
        steps: 0,
        changed: false,
        reduce_combinators: !file.is_std(db),
        known_values: FxHashMap::default(),
        method_functions: Default::default(),
        choice_growth: 4096,
    };
    // Process functions by index, since CAF inlining appends relocated lambdas
    // that must themselves be simplified (and may be pruned if beta consumes them).
    let mut i = 0;
    while i < cx.fns.len() {
        let body = std::mem::replace(&mut cx.fns[i].body, CExpr::new(K::Error, Ty::Error));
        let body = cx.simplify_expr(body);
        cx.fns[i].body = body;
        i += 1;
    }
    if !cx.changed {
        return base;
    }
    let lowered = LoweredDef {
        def: base.def,
        fns: cx.fns,
        entry_borrowed: base.entry_borrowed.clone(),
        reuse_entry: base.reuse_entry.clone(),
        entry_spread_params: base.entry_spread_params.clone(),
        data_shapes: base.data_shapes.clone(),
    };
    Arc::new(prune_dead_fns(lowered))
}

/// Per-definition reduction state.
struct Simplifier<'a> {
    db: &'a dyn Db,
    file: SourceFile,
    /// The definition's source file id (for the same-file CAF check).
    source: SourceId,
    combinators: Arc<CombinatorDefs>,
    /// The functions being reduced; CAF inlining appends relocated lambdas.
    fns: Vec<CoreFn>,
    /// The next free local slot (for the locals reduction synthesizes).
    next: usize,
    /// Contractions performed so far (against [`SIMPLIFY_STEP_BUDGET`]).
    steps: u32,
    /// Whether any reduction fired (drives the early-cutoff return).
    changed: bool,
    /// Literal-value expansion is also useful inside std; broader combinator
    /// reduction stays disabled there so its contracts exercise the definitions.
    reduce_combinators: bool,
    /// Stable lexical values available to interface projection/call reduction.
    known_values: FxHashMap<LocalId, CExpr>,
    /// Lifted functions exposed by a known interface projection.
    method_functions: rustc_hash::FxHashSet<FnId>,
    /// Remaining per-definition budget for duplicated choice continuations.
    choice_growth: usize,
}

impl Simplifier<'_> {
    /// Fully reduces `e`: simplifies its children, then contracts the resulting
    /// node and re-simplifies the contractum (so a cascade of exposed redexes runs
    /// to completion), bounded by the step budget.
    fn simplify_expr(&mut self, e: CExpr) -> CExpr {
        if self.steps >= SIMPLIFY_STEP_BUDGET {
            return e;
        }
        let e = self.simplify_children(e);
        match self.try_contract(e) {
            (true, contractum) => {
                self.steps += 1;
                self.changed = true;
                self.simplify_expr(contractum)
            }
            (false, e) => e,
        }
    }

    /// Rebuilds `e` with every child reduced (children first), leaving the node
    /// itself for [`Self::try_contract`].
    fn simplify_children(&mut self, e: CExpr) -> CExpr {
        let ty = e.ty.clone();
        let kind = match e.kind {
            K::App { func, args, reuse, alloc } => K::App {
                func: Box::new(self.simplify_expr(*func)),
                args: args.into_iter().map(|a| self.simplify_expr(a)).collect(),
                reuse,
                alloc,
            },
            K::Prim { op, args } => {
                K::Prim { op, args: args.into_iter().map(|a| self.simplify_expr(a)).collect() }
            }
            K::Foreign { symbol, args, marshalled } => K::Foreign {
                symbol,
                args: args.into_iter().map(|a| self.simplify_expr(a)).collect(),
                marshalled,
            },
            K::MakeData { tag, args, reuse, scalars, niche } => K::MakeData {
                tag,
                args: args.into_iter().map(|a| self.simplify_expr(a)).collect(),
                reuse,
                scalars,
                niche,
            },
            K::If { cond, then, els } => K::If {
                cond: Box::new(self.simplify_expr(*cond)),
                then: Box::new(self.simplify_expr(*then)),
                els: Box::new(self.simplify_expr(*els)),
            },
            K::Let { local, value, body } => {
                let value = self.simplify_expr(*value);
                let previous = self.known_values.remove(&local);
                if let Some(known) = self.known_binding(&value) {
                    self.known_values.insert(local, known);
                }
                let body = self.simplify_expr(*body);
                self.known_values.remove(&local);
                if let Some(previous) = previous {
                    self.known_values.insert(local, previous);
                }
                K::Let { local, value: Box::new(value), body: Box::new(body) }
            }
            K::DataTag { base, niche } => {
                K::DataTag { base: Box::new(self.simplify_expr(*base)), niche }
            }
            K::DataField { base, index, scalar, niche } => {
                K::DataField { base: Box::new(self.simplify_expr(*base)), index, scalar, niche }
            }
            // Spread/LetMany are produced after this pre-count pass; reduced
            // children for completeness.
            K::Spread { components } => K::Spread {
                components: components.into_iter().map(|a| self.simplify_expr(a)).collect(),
            },
            K::LetMany { locals, value, body } => K::LetMany {
                locals,
                value: Box::new(self.simplify_expr(*value)),
                body: Box::new(self.simplify_expr(*body)),
            },
            // Leaves and lifted-lambda references (the lambda's body is a separate
            // `CoreFn`, simplified by the driver loop).
            kind @ (K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error) => {
                kind
            }
            // Reference-counting and tail-call nodes do not exist in this pre-count
            // Core; reconstructed with reduced children for completeness.
            K::Reset { value, token, body } => K::Reset {
                value: Box::new(self.simplify_expr(*value)),
                token,
                body: Box::new(self.simplify_expr(*body)),
            },
            K::FreeReuse { token, body } => {
                K::FreeReuse { token, body: Box::new(self.simplify_expr(*body)) }
            }
            K::Dup { local, body } => K::Dup { local, body: Box::new(self.simplify_expr(*body)) },
            K::Drop { local, body } => K::Drop { local, body: Box::new(self.simplify_expr(*body)) },
            K::Join { params, body } => {
                K::Join { params, body: Box::new(self.simplify_expr(*body)) }
            }
            K::Recur { args } => {
                K::Recur { args: args.into_iter().map(|a| self.simplify_expr(a)).collect() }
            }
            K::HoleStart { hole, body } => {
                K::HoleStart { hole, body: Box::new(self.simplify_expr(*body)) }
            }
            K::HoleFill { hole, cell, field } => {
                K::HoleFill { hole, cell: Box::new(self.simplify_expr(*cell)), field }
            }
            K::HoleClose { hole, base } => {
                K::HoleClose { hole, base: Box::new(self.simplify_expr(*base)) }
            }
        };
        CExpr::new(kind, ty)
    }

    /// Attempts one top-level contraction of `e` (its children already reduced).
    /// Returns `(true, contractum)` on a hit, else `(false, e)` unchanged.
    fn try_contract(&mut self, e: CExpr) -> (bool, CExpr) {
        if let K::Global(def) = e.kind
            && let Some(value) = self.caf_body(def, Some(&e.ty))
        {
            return (true, value);
        }
        if !self.reduce_combinators {
            return (false, e);
        }
        if let Some(reduced) = self.contract_choice(&e) {
            return (true, reduced);
        }
        let CExpr { kind, ty } = e;
        let K::App { func, args, reuse, alloc } = kind else {
            return (false, CExpr::new(kind, ty));
        };
        // Flatten `App(App(h, xs), ys)` into `App(h, xs ++ ys)`, *except* a
        // row-polymorphic Global's evidence partial-application: there `xs` are
        // leading offset-evidence arguments that code generation passes as a separate
        // partial application, not as ordinary arguments, so merging them would
        // misalign the call.
        if let K::App { func: inner_func, .. } = &func.kind {
            let evidence_app =
                matches!(&inner_func.kind, K::Global(g) if self.is_row_polymorphic(*g));
            if !evidence_app {
                let CExpr { kind: inner, .. } = *func;
                let K::App { func: h, args: mut merged, reuse: mut r, alloc: _ } = inner else {
                    unreachable!("checked App above")
                };
                merged.extend(args);
                r.extend(reuse);
                return (true, CExpr::new(K::App { func: h, args: merged, reuse: r, alloc }, ty));
            }
        }
        let CExpr { kind: fkind, ty: fty } = *func;
        match fkind {
            K::Global(def) => {
                if let Some(new) = self.combinator(def, &args, &ty) {
                    return (true, new);
                }
                if let Some(body) = self.caf_body(def, None) {
                    let app = K::App { func: Box::new(body), args, reuse, alloc };
                    return (true, CExpr::new(app, ty));
                }
                let func = Box::new(CExpr::new(K::Global(def), fty));
                (false, CExpr::new(K::App { func, args, reuse, alloc }, ty))
            }
            K::MakeClosure { func: fnid, captures, alloc: cl_alloc } => {
                match self.beta(fnid, &captures, args, &ty) {
                    Ok(new) => (true, new),
                    Err(args) => {
                        let mc = K::MakeClosure { func: fnid, captures, alloc: cl_alloc };
                        let func = Box::new(CExpr::new(mc, fty));
                        (false, CExpr::new(K::App { func, args, reuse, alloc }, ty))
                    }
                }
            }
            other => {
                let func = Box::new(CExpr::new(other, fty));
                (false, CExpr::new(K::App { func, args, reuse, alloc }, ty))
            }
        }
    }

    /// Reduces an application of a recognized combinator, or `None` if `def` is not
    /// one (or its operands are not safe to reorder). `result_ty` is the
    /// application's type.
    fn combinator(&mut self, def: DefId, args: &[CExpr], result_ty: &Ty) -> Option<CExpr> {
        let compose = self.combinators.compose;
        let pipe = self.combinators.pipe;
        let identity = self.combinators.identity;
        let konst = self.combinators.konst;

        // `identity x rest… → x rest…` (no operand is dropped or reordered).
        if Some(def) == identity && !args.is_empty() {
            let head = args[0].clone();
            return Some(apply(head, args[1..].to_vec(), result_ty.clone()));
        }
        // Save `a` before discarding `b`: strict evaluation includes the order of
        // both effects and traps. Surplus arguments are applied afterward.
        if Some(def) == konst && args.len() >= 2 {
            let a = args[0].clone();
            let b = args[1].clone();
            let saved = fresh_local(&mut self.next);
            let dead = fresh_local(&mut self.next);
            let ty = a.ty.clone();
            let value = CExpr::new(K::Local(saved), ty.clone());
            let discarded = CExpr::new(
                K::Let { local: dead, value: Box::new(b), body: Box::new(value) },
                ty.clone(),
            );
            let kept = CExpr::new(
                K::Let { local: saved, value: Box::new(a), body: Box::new(discarded) },
                ty,
            );
            return Some(apply(kept, args[2..].to_vec(), result_ty.clone()));
        }
        // `x |> f rest… → f x rest…` — `x` and `f` swap evaluation order, so reduce
        // only when both are pure.
        if Some(def) == pipe && args.len() >= 2 {
            let x = &args[0];
            let f = &args[1];
            if !self.reorderable(x) || !self.reorderable(f) {
                return None;
            }
            let mut applied = vec![x.clone()];
            applied.extend(args[2..].iter().cloned());
            return Some(apply(f.clone(), applied, result_ty.clone()));
        }
        // `(f >> g) x rest… → g (f x) rest…` — `f` and `g` swap build order, so
        // reduce only when both are pure.
        if Some(def) == compose && args.len() >= 3 {
            let f = &args[0];
            let g = &args[1];
            let x = &args[2];
            if !self.reorderable(f) || !self.reorderable(g) {
                return None;
            }
            // `f x` has `f`'s result type; bail if `f` is not a concrete arrow.
            let inner_ty = arrow_codomain(&f.ty)?;
            let inner = apply(f.clone(), vec![x.clone()], inner_ty);
            let mut applied = vec![inner];
            applied.extend(args[3..].iter().cloned());
            return Some(apply(g.clone(), applied, result_ty.clone()));
        }
        None
    }

    /// If `def` is an inlinable nullary CAF, relocates its lifted lambdas into this
    /// definition and returns its (remapped) body — the value to splice in head
    /// position. `None` if `def` is not an eligible CAF.
    fn caf_body(&mut self, def: DefId, value_ty: Option<&Ty>) -> Option<CExpr> {
        // Same-file (the firewall), non-recursive (so the inline graph is a DAG and
        // this query stays cycle-free), nullary, non-row-polymorphic, and small.
        if def.file != self.source {
            return None;
        }
        if value_ty.is_some() {
            if !constant_value_eligible(self.db, self.file, def.name) {
                return None;
            }
        } else {
            // The signature-derived ABI keeps ordinary function-body edits out
            // of callers that are merely checking for a nullary CAF.
            if !crate::abi_of(self.db, def).params.is_empty()
                || recursive_defs(self.db, self.file).contains(&def)
            {
                return None;
            }
        }
        let caf = simplified(self.db, self.file, def.name);
        if !caf.entry().params.is_empty() {
            return None;
        }
        let evidence = crate::representation::definition_scheme(self.db, def)
            .map_or(0, |s| fai_types::evidence_count(&s));
        if evidence > 0 {
            return None;
        }
        if total_nodes(&caf) > CAF_NODE_BUDGET {
            return None;
        }
        if let Some(ty) = value_ty
            && &caf.entry().body.ty != ty
        {
            return None;
        }
        // A CAF whose body contains a lowering error (an unsupported construct, e.g.
        // a first-class comparison operator) carries a diagnostic reported only while
        // it is reachable. Inlining would move the error into the caller and make the
        // CAF unreachable, dropping the diagnostic — so leave such a CAF as a call.
        if caf.fns.iter().any(|f| body_has_error(&f.body)) {
            return None;
        }
        Some(self.relocated_body(&caf, &[]))
    }

    /// Copies a same-file definition into this one, retaining its argument
    /// evaluation order and assigning fresh local and lifted-function identities.
    fn relocated_body(&mut self, caf: &LoweredDef, args: &[CExpr]) -> CExpr {
        // Relocate the callee's lifted lambdas (`fns[1..]`) into this definition's
        // `fns`, giving each a fresh id; the entry (`fns[0]`) is spliced as the
        // returned body, not appended. Locals are unique across a `LoweredDef`, so
        // one shared map freshens them all consistently.
        let mut fn_map: FxHashMap<FnId, FnId> = FxHashMap::default();
        let lifted_start = self.fns.len();
        for i in 1..caf.fns.len() {
            fn_map.insert(FnId(i as u32), FnId((lifted_start + i - 1) as u32));
        }
        let mut locals: FxHashMap<LocalId, LocalId> = FxHashMap::default();
        let bindings: Vec<_> = caf
            .entry()
            .params
            .iter()
            .zip(args)
            .map(|(&param, value)| (remap_local(param, &mut locals, &mut self.next), value.clone()))
            .collect();
        for f in caf.fns.iter().skip(1) {
            let params =
                f.params.iter().map(|p| remap_local(*p, &mut locals, &mut self.next)).collect();
            let captures =
                f.captures.iter().map(|c| remap_local(*c, &mut locals, &mut self.next)).collect();
            let body = remap_expr(&f.body, &mut locals, &fn_map, &mut self.next);
            self.fns.push(CoreFn { params, captures, body });
        }
        let mut body = remap_expr(&caf.entry().body, &mut locals, &fn_map, &mut self.next);
        for (local, value) in bindings.into_iter().rev() {
            let ty = body.ty.clone();
            body = CExpr::new(K::Let { local, value: Box::new(value), body: Box::new(body) }, ty);
        }
        body
    }

    /// Beta-reduces a saturated-or-over application of the lambda `fnid` (with the
    /// `MakeClosure`'s outer `captures`) to `args`. `Err(args)` returns the
    /// arguments unconsumed when the application is partial (too few arguments).
    fn beta(
        &mut self,
        fnid: FnId,
        captures: &[LocalId],
        args: Vec<CExpr>,
        result_ty: &Ty,
    ) -> Result<CExpr, Vec<CExpr>> {
        let f = &self.fns[fnid.index()];
        let arity = f.params.len();
        if args.len() < arity {
            return Err(args);
        }
        let params = f.params.clone();
        let fn_captures = f.captures.clone();
        let body_src = f.body.clone();

        // Bind every argument to a fresh local, in source order (so a multiply-used
        // parameter is not duplicated and evaluation order is preserved).
        let arg_tys: Vec<Ty> = args.iter().map(|a| a.ty.clone()).collect();
        let arg_locals: Vec<LocalId> =
            (0..args.len()).map(|_| fresh_local(&mut self.next)).collect();
        let mut locals: FxHashMap<LocalId, LocalId> = FxHashMap::default();
        for (p, &l) in params.iter().zip(&arg_locals) {
            locals.insert(*p, l);
        }
        // Map the lambda's capture slots to the locals the `MakeClosure` supplied.
        for (inner, &outer) in fn_captures.iter().zip(captures) {
            locals.insert(*inner, outer);
        }
        // The body's own `MakeClosure`s reference sibling lifted fns of *this*
        // definition, whose ids stay valid — an identity fn map.
        let body = remap_expr(&body_src, &mut locals, &FxHashMap::default(), &mut self.next);

        // Over-application: apply the saturated result to the surplus arguments.
        let mut result = if args.len() > arity {
            let surplus: Vec<CExpr> = arg_locals[arity..]
                .iter()
                .zip(&arg_tys[arity..])
                .map(|(&l, t)| CExpr::new(K::Local(l), t.clone()))
                .collect();
            let app = K::App {
                func: Box::new(body),
                args: surplus,
                reuse: Vec::new(),
                alloc: ClosureAlloc::Heap,
            };
            CExpr::new(app, result_ty.clone())
        } else {
            body
        };
        // Bind every argument, outermost first, so they evaluate in source order.
        for (local, value) in arg_locals.into_iter().zip(args).rev() {
            let ty = result.ty.clone();
            let let_ = K::Let { local, value: Box::new(value), body: Box::new(result) };
            result = CExpr::new(let_, ty);
        }
        Ok(result)
    }

    /// Reordered operands must be effect-free and guaranteed to terminate without
    /// a semantic trap. Sharing this analysis keeps all reorder passes consistent.
    fn reorderable(&self, e: &CExpr) -> bool {
        crate::purity::expr_pure_total(self.db, e)
    }

    /// Whether `g` is row-polymorphic (its entry takes leading offset-evidence
    /// parameters). Such a function's call is a nested application whose inner
    /// arguments are evidence, which flattening must not merge.
    fn is_row_polymorphic(&self, g: DefId) -> bool {
        crate::representation::definition_scheme(self.db, g)
            .is_some_and(|s| fai_types::evidence_count(&s) > 0)
    }
}

/// Eligibility is a separate firewall: changing an ineligible initializer does
/// not make all of its value-position users repeat the simplification pass.
#[salsa::tracked]
fn constant_value_eligible(db: &dyn Db, file: SourceFile, name: Symbol) -> bool {
    let base = core_inlined(db, file, name);
    if !base.entry().params.is_empty()
        || recursive_defs(db, file).contains(&base.def)
        || !ground_type(&base.entry().body.ty)
        || crate::representation::definition_scheme(db, base.def)
            .is_some_and(|s| fai_types::evidence_count(&s) > 0)
    {
        return false;
    }
    let reduced = simplified(db, file, name);
    total_nodes(&reduced) <= CAF_NODE_BUDGET
        && literal_value(&reduced.entry().body)
        && !reduced.fns.iter().any(|f| body_has_error(&f.body))
}

/// A bounded constructor/literal expression has no forcing effects or traps.
/// Lambdas may have latent effects; constructing a capture-free one is pure.
fn literal_value(e: &CExpr) -> bool {
    use crate::Prim;
    match &e.kind {
        K::Lit(_) | K::Local(_) => true,
        K::MakeClosure { captures, .. } => captures.is_empty(),
        K::MakeData { args, .. } => args.iter().all(literal_value),
        K::Let { value, body, .. } => literal_value(value) && literal_value(body),
        K::If { cond, then, els } => {
            literal_value(cond) && literal_value(then) && literal_value(els)
        }
        K::Prim { op, args } => {
            matches!(
                op,
                Prim::IntAdd
                    | Prim::IntSub
                    | Prim::IntMul
                    | Prim::IntAnd
                    | Prim::IntOr
                    | Prim::IntXor
                    | Prim::IntShl
                    | Prim::IntShr
                    | Prim::IntShrLogical
                    | Prim::IntComplement
                    | Prim::FloatAdd
                    | Prim::FloatSub
                    | Prim::FloatMul
                    | Prim::FloatNeg
                    | Prim::IntToFloat
                    | Prim::FloatFromBits
                    | Prim::FloatToBits
                    | Prim::Not
            ) && args.iter().all(literal_value)
        }
        _ => false,
    }
}

/// Keep generic and representation-changing uses on their original value ABI.
fn ground_type(ty: &Ty) -> bool {
    match ty {
        Ty::Var(_) | Ty::Error => false,
        Ty::App(a, b) => ground_type(a) && ground_type(b),
        Ty::Arrow(a, b, effect) => {
            ground_type(a) && ground_type(b) && effect.tail == fai_types::EffEnd::Closed
        }
        Ty::Tuple(fields) => fields.iter().all(ground_type),
        Ty::Record(row) => {
            row.tail == fai_types::RowEnd::Closed && row.fields.iter().all(|(_, t)| ground_type(t))
        }
        Ty::EffectArg(row) => row.tail == fai_types::EffEnd::Closed,
        Ty::Con(_) | Ty::Adt(_) | Ty::Interface(_) | Ty::Unit => true,
    }
}

/// Applies `func` to `extra` arguments, or returns `func` unchanged when there are
/// none. The result carries `ty` (the surrounding application's type).
fn apply(func: CExpr, extra: Vec<CExpr>, ty: Ty) -> CExpr {
    if extra.is_empty() {
        return func;
    }
    let app =
        K::App { func: Box::new(func), args: extra, reuse: Vec::new(), alloc: ClosureAlloc::Heap };
    CExpr::new(app, ty)
}

/// The codomain of an arrow type, or `None` for a non-arrow (a type variable or
/// otherwise not a concrete function type).
fn arrow_codomain(ty: &Ty) -> Option<Ty> {
    match ty {
        Ty::Arrow(_, to, _) => Some((**to).clone()),
        _ => None,
    }
}

/// The total number of Core nodes across a definition's entry and lifted lambdas
/// (the size CAF inlining is budgeted against).
fn total_nodes(def: &LoweredDef) -> usize {
    def.fns.iter().map(|f| node_count(&f.body)).sum()
}

/// Whether `e` contains a lowering error node (an unsupported construct, which
/// carries a reachability-gated diagnostic that must not be inlined away).
fn body_has_error(e: &CExpr) -> bool {
    if matches!(e.kind, K::Error) {
        return true;
    }
    let any = |xs: &[CExpr]| xs.iter().any(body_has_error);
    match &e.kind {
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } => false,
        K::Error => true,
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args } => any(args),
        K::App { func, args, .. } => body_has_error(func) || any(args),
        K::If { cond, then, els } => {
            body_has_error(cond) || body_has_error(then) || body_has_error(els)
        }
        K::Let { value, body, .. } => body_has_error(value) || body_has_error(body),
        K::Spread { components } => any(components),
        K::LetMany { value, body, .. } => body_has_error(value) || body_has_error(body),
        K::DataTag { base, .. } | K::DataField { base, .. } => body_has_error(base),
        K::Reset { value, body, .. } => body_has_error(value) || body_has_error(body),
        K::FreeReuse { body, .. } | K::Dup { body, .. } | K::Drop { body, .. } => {
            body_has_error(body)
        }
        K::Join { body, .. } | K::HoleStart { body, .. } => body_has_error(body),
        K::HoleFill { cell, .. } => body_has_error(cell),
        K::HoleClose { base, .. } => body_has_error(base),
    }
}

/// The number of Core nodes in `e` (every [`CExpr`] is one, recursing into its
/// expression children).
fn node_count(e: &CExpr) -> usize {
    let kids = |xs: &[CExpr]| -> usize { xs.iter().map(node_count).sum() };
    1 + match &e.kind {
        K::Lit(_) | K::Local(_) | K::Global(_) | K::Error | K::MakeClosure { .. } => 0,
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args } => kids(args),
        K::App { func, args, .. } => node_count(func) + kids(args),
        K::If { cond, then, els } => node_count(cond) + node_count(then) + node_count(els),
        K::Let { value, body, .. } => node_count(value) + node_count(body),
        K::Spread { components } => kids(components),
        K::LetMany { value, body, .. } => node_count(value) + node_count(body),
        K::DataTag { base, .. } | K::DataField { base, .. } => node_count(base),
        K::Reset { value, body, .. } => node_count(value) + node_count(body),
        K::FreeReuse { body, .. } | K::Dup { body, .. } | K::Drop { body, .. } => node_count(body),
        K::Join { body, .. } | K::HoleStart { body, .. } => node_count(body),
        K::HoleFill { cell, .. } => node_count(cell),
        K::HoleClose { base, .. } => node_count(base),
    }
}

//! Deforestation of `List`/`Array` combinator pipelines.
//!
//! A pipeline of directly-nested standard combinators — a producer, then
//! transformers, then a consumer (`Array.sum (Array.map f (Array.range 0 n))`) —
//! builds one intermediate sequence per stage only to walk it once. This pass
//! recognizes such a chain by the **resolved identity** of the combinators and
//! rewrites it to a single synthesized self-tail-recursive loop that materializes
//! no intermediate sequence; the loop is reference-counted and tail-call-flattened
//! by the ordinary back-end passes (so a unique producer is still recycled, and
//! the loop runs in constant stack), and a literal element function is inlined
//! into the loop body (so a pure arithmetic pipeline becomes a register loop).
//!
//! Run just after the prim/helper inliners (which never touch the recursive
//! combinators) and just before reference counting, so a combinator call is still
//! a `Call` to its resolved std symbol. Recognition is by `DefId` (never by
//! reading a combinator's body), so editing a combinator's body does not change
//! what fuses — the cross-module firewall.
//!
//! **Behavior-preserving.** Only *directly-nested* applications fuse, so every
//! intermediate is an unnamed temporary consumed exactly once; a `let`-bound or
//! shared sequence is the loop's (materialized) source, never fused away. A stage
//! fuses only when its element function is **pure** (an effectful stage ends the
//! fusable chain), so reordering element applications across stages — including
//! which element a trap falls on — is unobservable.
//!
//! The synthesized loop is a new top-level definition (a `fuse#…` name in the
//! consuming file, sharing no name with a source binding), built and emitted the
//! way the mutual-recursion combined loop is: the consuming definition's
//! [`fuse_def`] result carries both the rewritten body and the loops, the driver
//! reference-counts and code-generates the loops at assembly time, and they are
//! invisible to source-level reachability. Fusion does not fire inside the
//! standard library itself, so the combinators stay tested by their own contracts.

use std::sync::Arc;

use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId, ModuleName, module_file};
use fai_syntax::Symbol;
use fai_types::{Con, Ty};
use rustc_hash::FxHashMap;

use crate::helper_inlined;
use crate::ir::{
    CExpr, ClosureAlloc, CoreFn, ExprKind as K, FnAbi, FnId, Lit, LoweredDef, Prim, Repr,
};

/// The `List` constructor tags (mirrors lowering's `NIL_TAG`/`CONS_TAG`).
const NIL_TAG: u32 = 0;
const CONS_TAG: u32 = 1;

/// A synthesized loop for one fused pipeline: a pre-reference-counting definition
/// whose entry is the self-tail-recursive loop, plus the native calling-convention
/// shape its direct callers (and code generation) need. The driver
/// reference-counts it (`rc_owned`) and emits it like a mutual-recursion combined
/// loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusedLoop {
    /// The loop's lowered definition (entry only; no lifted lambdas).
    pub lowered: LoweredDef,
    /// The loop's native ABI (raw scalars for `Int`/`Float` state, uniform for
    /// closures/sequences), used by direct callers and code generation.
    pub abi: FnAbi,
    /// The loop's runtime arity (its parameter count).
    pub arity: usize,
}

/// The result of fusing one definition: its rewritten body and the loops the
/// rewrite introduced (empty when nothing fused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuseResult {
    /// The consuming definition with every recognized pipeline replaced by a call
    /// to a synthesized loop.
    pub body: LoweredDef,
    /// The synthesized loops, in deterministic (first-recognized) order.
    pub loops: Vec<FusedLoop>,
}

/// The resolved definition ids of the standard combinators fusion recognizes.
/// Resolved once from the module headers (never from a combinator's body), so it
/// is independent of any combinator-body edit (the firewall).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusionDefs {
    /// `Module.name -> DefId` for each recognized combinator.
    map: FxHashMap<(SeqKind, Comb), DefId>,
}

/// Which sequence type a combinator belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeqKind {
    /// The linked `List`.
    List,
    /// The contiguous `Array`.
    Array,
}

/// A recognized standard combinator (per [`SeqKind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Comb {
    Range,
    Init,
    Repeat,
    Map,
    Filter,
    Foldl,
    Foldr,
    Sum,
    Length,
    All,
    Any,
    Find,
    Member,
}

impl FusionDefs {
    /// Which combinator a `DefId` is, if any.
    fn lookup(&self, def: DefId) -> Option<(SeqKind, Comb)> {
        self.map.iter().find(|&(_, &d)| d == def).map(|(&k, _)| k)
    }
}

/// Resolves the recognized combinators' definition ids from the standard library.
///
/// Reads only the module headers (`module_file`) and forms `DefId`s by name, so it
/// does not depend on any combinator's body — editing `List.map`'s body never
/// changes what fusion recognizes (salsa early cutoff).
#[salsa::tracked]
pub fn fusion_defs(db: &dyn Db) -> Option<Arc<FusionDefs>> {
    let resolve = |module: &str, name: &str| -> Option<DefId> {
        let f = module_file(db, ModuleName(Symbol::intern(module)))?;
        Some(DefId::new(f.source(db), Symbol::intern(name)))
    };
    let mut map = FxHashMap::default();
    let mut add = |seq: SeqKind, comb: Comb, module: &str, name: &str| {
        if let Some(def) = resolve(module, name) {
            map.insert((seq, comb), def);
        }
    };
    for (seq, module) in [(SeqKind::List, "List"), (SeqKind::Array, "Array")] {
        add(seq, Comb::Range, module, "range");
        add(seq, Comb::Map, module, "map");
        add(seq, Comb::Filter, module, "filter");
        add(seq, Comb::Foldl, module, "foldl");
        add(seq, Comb::Foldr, module, "foldr");
        add(seq, Comb::Sum, module, "sum");
        add(seq, Comb::Length, module, "length");
        add(seq, Comb::All, module, "all");
        add(seq, Comb::Any, module, "any");
        add(seq, Comb::Find, module, "find");
        add(seq, Comb::Member, module, "member");
    }
    // Array-only producers.
    add(SeqKind::Array, Comb::Init, "Array", "init");
    add(SeqKind::Array, Comb::Repeat, "Array", "repeat");
    // The `List` module is required for fusion to mean anything; if the standard
    // library is absent, recognize nothing.
    if map.is_empty() {
        return None;
    }
    Some(Arc::new(FusionDefs { map }))
}

/// `name`'s lowered definition with every recognized pipeline fused, plus the
/// synthesized loops the rewrite introduced.
///
/// The back end's view of Core: reference counting reads `.body` (in place of
/// [`helper_inlined`]); the driver reads `.loops` to emit them. Returns the input
/// lowering with no loops when nothing fused (so the common no-pipeline definition
/// is an O(1) early cutoff). Fusion is skipped entirely inside the standard
/// library (so combinators stay tested by their own contracts).
#[salsa::tracked]
pub fn fuse_def(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<FuseResult> {
    let base = helper_inlined(db, file, name);
    let no_fuse = || Arc::new(FuseResult { body: (*base).clone(), loops: Vec::new() });
    if fai_db::is_std_path(file.path(db)) {
        return no_fuse();
    }
    let Some(defs) = fusion_defs(db) else { return no_fuse() };

    let mut cx = Fuser {
        db,
        defs: &defs,
        source: file.source(db),
        consuming: name,
        next_local: crate::inline::next_free_local(&base),
        loops: Vec::new(),
        chain_index: 0,
        changed: false,
    };
    let fns: Vec<CoreFn> = base
        .fns
        .iter()
        .map(|f| CoreFn {
            params: f.params.clone(),
            captures: f.captures.clone(),
            body: cx.rewrite(&f.body, &base.fns),
        })
        .collect();
    // Nothing fused (a chain may rewrite to an unrolled form with no loop, so the
    // loop list alone is not the signal).
    if !cx.changed {
        return no_fuse();
    }
    let loops = std::mem::take(&mut cx.loops);
    let body = prune_dead_fns(LoweredDef {
        def: base.def,
        fns,
        entry_borrowed: base.entry_borrowed.clone(),
        reuse_entry: base.reuse_entry.clone(),
        entry_spread_params: base.entry_spread_params.clone(),
    });
    Arc::new(FuseResult { body, loops })
}

/// The per-definition fusion state.
struct Fuser<'a> {
    db: &'a dyn Db,
    defs: &'a FusionDefs,
    /// The consuming definition's source file.
    source: fai_span::SourceId,
    /// The consuming definition's name (for naming synthesized loops).
    consuming: Symbol,
    /// The next free local slot in the consuming definition (for binding a source).
    next_local: usize,
    /// The loops synthesized so far.
    loops: Vec<FusedLoop>,
    /// A counter naming each synthesized loop within the consuming definition.
    chain_index: usize,
    /// Whether any pipeline was rewritten (a synthesized loop *or* an unrolled
    /// literal, which produces no loop).
    changed: bool,
}

// ---------------------------------------------------------------------------
// Recognition.
// ---------------------------------------------------------------------------

/// A function argument of a stage or consumer: a literal lambda to inline, or a
/// value applied at runtime.
enum FnArg {
    /// A lambda written at the call site (`MakeClosure`): inlined into the loop.
    /// `params` are its parameter slots, `body` its body, and `caps` maps each of
    /// its capture slots to the outer value supplying it (lifted to a loop param).
    Lambda { params: Vec<LocalId>, body: CExpr, caps: Vec<(LocalId, CExpr)> },
    /// Any other function value: passed to the loop and applied via `apply_n`.
    Value(CExpr),
}

/// The source that starts a recognized chain.
enum Source {
    /// `range lo hi` (List or Array): the integers `[lo, hi)`, walked numerically
    /// with no materialized sequence.
    Range { lo: CExpr, hi: CExpr },
    /// `Array.init n f`: index `[0, n)` with element `f i`.
    Init { n: CExpr, f: FnArg },
    /// `Array.repeat n x`: index `[0, n)` with element `x`.
    Repeat { n: CExpr, x: CExpr },
    /// A small syntactic list/array literal with known elements: the chain is
    /// **unrolled** to straight-line code (no loop, no materialized literal). `seq`
    /// is the original literal expression, used as a value-source fallback if the
    /// unrolled form would exceed the node budget.
    Literal { elems: Vec<CExpr>, seq: CExpr },
    /// Any `Array`-typed value, walked by index `[0, length)`.
    ArrayValue { seq: CExpr },
    /// Any `List`-typed value, walked along its spine.
    ListValue { seq: CExpr },
}

/// The most Core nodes an unrolled literal pipeline may expand to before it is
/// left materialized instead (the node-count budget on the unrolled result).
const UNROLL_NODE_BUDGET: usize = 256;

/// A middle stage of a chain.
enum Stage {
    /// `map f`: rebinds the element to `f element` (typed `out` thereafter).
    Map { f: FnArg, out: Ty },
    /// `filter p`: keeps only elements satisfying `p` (the element type is kept).
    Filter(FnArg),
}

/// The consumer that ends a chain (its element type entering it is `elem_ty`).
enum Consumer {
    /// `foldl step init`.
    Foldl { step: FnArg, init: CExpr },
    /// `foldr step init` (drives the loop in reverse; not used for a List value
    /// source, which cannot be indexed — see [`Fuser::source_of`]).
    Foldr { step: FnArg, init: CExpr },
    /// `sum`.
    Sum,
    /// `length`.
    Length,
    /// `all pred`: `true` unless some element fails `pred`.
    All(FnArg),
    /// `any pred`: `false` unless some element satisfies `pred`.
    Any(FnArg),
    /// `find pred`: the first element satisfying `pred`, or `None`.
    Find(FnArg),
    /// `member target`: whether `target` equals some element.
    Member(CExpr),
    /// A terminal `map`/`filter`: build one result sequence. `pred` is `None` for a
    /// terminal `map` (push `f elem`) and `Some` for a terminal `filter` (push the
    /// element when it satisfies the predicate; `f` is then the identity).
    Build { f: FnArg, filter: bool, seq: SeqKind },
}

impl Consumer {
    /// Whether the loop carries an accumulator parameter (a fold/scalar result or
    /// an `Array` builder buffer) versus none (a short-circuit search or a `List`
    /// builder, which builds via tail-recursion-modulo-cons).
    fn has_acc(&self) -> bool {
        match self {
            Consumer::Foldl { .. } | Consumer::Foldr { .. } | Consumer::Sum | Consumer::Length => {
                true
            }
            Consumer::Build { seq, .. } => *seq == SeqKind::Array,
            Consumer::All(_) | Consumer::Any(_) | Consumer::Find(_) | Consumer::Member(_) => false,
        }
    }

    /// Whether the loop iterates in reverse (only a `foldr`).
    fn reverse(&self) -> bool {
        matches!(self, Consumer::Foldr { .. })
    }
}

/// A recognized pipeline.
struct Chain {
    /// The source (a producer or a value sequence).
    source: Source,
    /// The transformers, in application order (source-to-consumer).
    stages: Vec<Stage>,
    /// The consumer.
    consumer: Consumer,
    /// The element type entering the first stage (the source's element type).
    source_elem_ty: Ty,
    /// The element type entering the consumer (after all stages).
    consumer_elem_ty: Ty,
    /// The whole chain's result type.
    result_ty: Ty,
}

impl Fuser<'_> {
    /// Rewrites an expression: if a recognized pipeline is rooted here, replace it
    /// with a call to a synthesized loop; otherwise recurse into the children
    /// (so a pipeline nested elsewhere still fuses).
    fn rewrite(&mut self, e: &CExpr, base_fns: &[CoreFn]) -> CExpr {
        if let Some(chain) = self.recognize(e, base_fns) {
            self.changed = true;
            return self.generate(chain, base_fns);
        }
        self.map_children(e, base_fns)
    }

    /// Recognizes a maximal chain rooted at `e` (a consumer application), or `None`.
    fn recognize(&self, e: &CExpr, base_fns: &[CoreFn]) -> Option<Chain> {
        let (cdef, cargs) = call_target(e)?;
        let (seq, comb) = self.defs.lookup(cdef)?;
        let (consumer, seq_arg) = self.consumer_of(seq, comb, cargs, base_fns)?;
        let consumer_elem_ty = seq_elem(&seq_arg.ty)?;

        // Peel pure transformers from the consumer's sequence argument inward.
        let mut stages = Vec::new();
        let mut cur = seq_arg;
        while let Some((tdef, targs)) = call_target(cur) {
            match self.defs.lookup(tdef) {
                Some((tseq, Comb::Map)) if tseq == seq && targs.len() == 2 => {
                    if !self.arg_pure(&targs[0], 1, base_fns) {
                        break;
                    }
                    let Some(out) = seq_elem(&cur.ty) else { break };
                    stages.push(Stage::Map { f: self.fn_arg(&targs[0], base_fns), out });
                    cur = &targs[1];
                }
                Some((tseq, Comb::Filter)) if tseq == seq && targs.len() == 2 => {
                    if !self.arg_pure(&targs[0], 1, base_fns) {
                        break;
                    }
                    stages.push(Stage::Filter(self.fn_arg(&targs[0], base_fns)));
                    cur = &targs[1];
                }
                _ => break,
            }
        }
        stages.reverse(); // peeled outer-to-inner; apply source-to-consumer

        let source_elem_ty = seq_elem(&cur.ty)?;
        let source = self.source_of(seq, cur, base_fns)?;
        // A `foldr` over a List value source cannot become a single tail loop
        // (no O(1) indexing; deep recursion would overflow). Leave it unfused.
        if consumer.reverse() && matches!(source, Source::ListValue { .. }) {
            return None;
        }
        Some(Chain {
            source,
            stages,
            consumer,
            source_elem_ty,
            consumer_elem_ty,
            result_ty: e.ty.clone(),
        })
    }

    /// The consumer for `(seq, comb)` with arguments `args`, plus the sequence
    /// argument the chain continues from. `None` if `comb` is not a supported
    /// consumer, the arity is wrong, or its element function is effectful.
    fn consumer_of<'b>(
        &self,
        seq: SeqKind,
        comb: Comb,
        args: &'b [CExpr],
        base_fns: &[CoreFn],
    ) -> Option<(Consumer, &'b CExpr)> {
        match comb {
            Comb::Sum if args.len() == 1 => Some((Consumer::Sum, &args[0])),
            Comb::Length if args.len() == 1 => Some((Consumer::Length, &args[0])),
            Comb::Foldl if args.len() == 3 && self.arg_pure(&args[0], 2, base_fns) => Some((
                Consumer::Foldl { step: self.fn_arg(&args[0], base_fns), init: args[1].clone() },
                &args[2],
            )),
            // `foldr` over a List value source cannot fuse to a single tail loop
            // (no indexing, and deep recursion would overflow); recognized only
            // when the source is reversible (handled by `source_of` returning
            // `None` for a List value, which ends recognition).
            Comb::Foldr if args.len() == 3 && self.arg_pure(&args[0], 2, base_fns) => Some((
                Consumer::Foldr { step: self.fn_arg(&args[0], base_fns), init: args[1].clone() },
                &args[2],
            )),
            Comb::All if args.len() == 2 && self.arg_pure(&args[0], 1, base_fns) => {
                Some((Consumer::All(self.fn_arg(&args[0], base_fns)), &args[1]))
            }
            Comb::Any if args.len() == 2 && self.arg_pure(&args[0], 1, base_fns) => {
                Some((Consumer::Any(self.fn_arg(&args[0], base_fns)), &args[1]))
            }
            Comb::Find if args.len() == 2 && self.arg_pure(&args[0], 1, base_fns) => {
                Some((Consumer::Find(self.fn_arg(&args[0], base_fns)), &args[1]))
            }
            Comb::Member if args.len() == 2 => Some((Consumer::Member(args[0].clone()), &args[1])),
            // A terminal `map`/`filter`: the preceding stages fuse into this one
            // builder loop.
            Comb::Map if args.len() == 2 && self.arg_pure(&args[0], 1, base_fns) => Some((
                Consumer::Build { f: self.fn_arg(&args[0], base_fns), filter: false, seq },
                &args[1],
            )),
            Comb::Filter if args.len() == 2 && self.arg_pure(&args[0], 1, base_fns) => Some((
                Consumer::Build { f: self.fn_arg(&args[0], base_fns), filter: true, seq },
                &args[1],
            )),
            _ => None,
        }
    }

    /// Recognizes the source `cur` for a chain of sequence kind `seq`.
    fn source_of(&self, seq: SeqKind, cur: &CExpr, base_fns: &[CoreFn]) -> Option<Source> {
        let _ = base_fns;
        if let Some((pdef, pargs)) = call_target(cur)
            && let Some((pseq, comb)) = self.defs.lookup(pdef)
            && pseq == seq
        {
            match comb {
                Comb::Range if pargs.len() == 2 => {
                    return Some(Source::Range { lo: pargs[0].clone(), hi: pargs[1].clone() });
                }
                Comb::Init if pargs.len() == 2 && self.arg_pure(&pargs[1], 1, base_fns) => {
                    return Some(Source::Init {
                        n: pargs[0].clone(),
                        f: self.fn_arg(&pargs[1], base_fns),
                    });
                }
                Comb::Repeat if pargs.len() == 2 => {
                    return Some(Source::Repeat { n: pargs[0].clone(), x: pargs[1].clone() });
                }
                _ => {}
            }
        }
        // A small, complete syntactic literal is unrolled to straight-line code.
        if let Some(elems) = literal_elems(seq, cur) {
            return Some(Source::Literal { elems, seq: cur.clone() });
        }
        // A value sequence the loop walks (a Local, a call result, a barrier's
        // output): index for an Array, spine for a List.
        match (seq, &cur.ty) {
            (SeqKind::Array, t) if is_seq(t, Con::Array) => {
                Some(Source::ArrayValue { seq: cur.clone() })
            }
            (SeqKind::List, t) if is_seq(t, Con::List) => {
                Some(Source::ListValue { seq: cur.clone() })
            }
            _ => None,
        }
    }

    /// Whether an element-function argument is pure when applied to `arity`
    /// arguments, so its stage may fuse without reordering effects.
    ///
    /// The Core IR's types reliably carry *concrete* effect atoms but erase a
    /// *polymorphic* effect variable to pure, so purity is decided structurally
    /// instead: a literal lambda is pure iff its body performs no capability and
    /// makes no indirect call (a call through a `Local`/captured value — e.g.
    /// `fun acc x -> acc + f x` — cannot be proven pure); a named function is pure
    /// iff its declared/inferred scheme's arrows (up to `arity`) are pure (which
    /// keeps effects, unlike the erased body types). Anything else is conservatively
    /// impure.
    fn arg_pure(&self, arg: &CExpr, arity: usize, base_fns: &[CoreFn]) -> bool {
        match &arg.kind {
            K::MakeClosure { func, .. } => self.expr_pure(&base_fns[func.index()].body),
            K::Global(g) => self.global_apply_pure(*g, arity),
            _ => false,
        }
    }

    /// Whether evaluating `e` performs no capability and makes no unprovable call
    /// (a structural purity walk; building a closure is pure, applying an indirect
    /// value is not).
    fn expr_pure(&self, e: &CExpr) -> bool {
        match &e.kind {
            K::Prim { args, .. } => args.iter().all(|a| self.expr_pure(a)),
            // A foreign call performs a host capability, so a stage containing one
            // is an effect barrier and never fuses across.
            K::Foreign { .. } => false,
            K::App { func, args, .. } => {
                let callee_ok =
                    matches!(&func.kind, K::Global(g) if self.global_apply_pure(*g, args.len()));
                callee_ok && args.iter().all(|a| self.expr_pure(a))
            }
            // Building a closure is pure; its effect (if any) rides its arrow and is
            // checked where the closure is applied.
            K::MakeClosure { .. } | K::Lit(_) | K::Local(_) | K::Global(_) | K::Error => true,
            K::If { cond, then, els } => {
                self.expr_pure(cond) && self.expr_pure(then) && self.expr_pure(els)
            }
            K::Let { value, body, .. } => self.expr_pure(value) && self.expr_pure(body),
            K::MakeData { args, .. } => args.iter().all(|a| self.expr_pure(a)),
            // Spread/LetMany are produced after this pre-count pass; recurse for
            // forward-safety (a spread is component reads, a letmany a call + body).
            K::Spread { components } => components.iter().all(|a| self.expr_pure(a)),
            K::LetMany { value, body, .. } => self.expr_pure(value) && self.expr_pure(body),
            K::DataTag { base, .. } | K::DataField { base, .. } => self.expr_pure(base),
            // Reference-counting / tail-call nodes do not exist in this pre-count
            // body; treat conservatively by recursing where they have children.
            K::Reset { value, body, .. } => self.expr_pure(value) && self.expr_pure(body),
            K::Dup { body, .. } | K::Drop { body, .. } | K::FreeReuse { body, .. } => {
                self.expr_pure(body)
            }
            K::Join { body, .. } | K::HoleStart { body, .. } => self.expr_pure(body),
            K::Recur { args } => args.iter().all(|a| self.expr_pure(a)),
            K::HoleFill { cell, .. } => self.expr_pure(cell),
            K::HoleClose { base, .. } => self.expr_pure(base),
        }
    }

    /// Whether applying the named function `g` to `arity` arguments is pure: every
    /// arrow of its scheme up to `arity` carries the pure effect. Reads the scheme
    /// (the type), which preserves effects (the body types do not), so it is sound
    /// for an effect-polymorphic callee and firewalled from body edits.
    fn global_apply_pure(&self, g: DefId, arity: usize) -> bool {
        let Some(scheme) = fai_types::declared_or_inferred_scheme(self.db, g) else {
            return false;
        };
        let mut ty = &scheme.ty;
        for _ in 0..arity {
            match ty {
                Ty::Arrow(_, to, eff) if eff.is_pure() => ty = to,
                _ => return false,
            }
        }
        true
    }

    /// Classifies a function argument as a lambda to inline or a value to apply.
    ///
    /// A literal lambda is inlined only when its body contains no nested
    /// `MakeClosure`: a nested lambda's lifted function lives in the *consuming*
    /// definition, not the synthesized loop, so splicing its `MakeClosure` into the
    /// loop would dangle the reference. Such a lambda is passed as a value (its
    /// closure, applied via `apply_n`), keeping its lifted functions intact.
    fn fn_arg(&self, arg: &CExpr, base_fns: &[CoreFn]) -> FnArg {
        if let K::MakeClosure { func, captures, .. } = &arg.kind
            && !body_has_closure(&base_fns[func.index()].body)
        {
            let cf = &base_fns[func.index()];
            let caps = cf
                .captures
                .iter()
                .zip(captures)
                .map(|(&slot, &outer)| {
                    let ty = local_type_in(&cf.body, slot).unwrap_or(Ty::Error);
                    (slot, CExpr::new(K::Local(outer), ty))
                })
                .collect();
            FnArg::Lambda { params: cf.params.clone(), body: cf.body.clone(), caps }
        } else {
            FnArg::Value(arg.clone())
        }
    }

    /// Recurses into the children of `e`, rewriting each.
    fn map_children(&mut self, e: &CExpr, base_fns: &[CoreFn]) -> CExpr {
        let go = |s: &mut Self, c: &CExpr| s.rewrite(c, base_fns);
        let kind = match &e.kind {
            K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {
                e.kind.clone()
            }
            K::Prim { op, args } => {
                K::Prim { op: *op, args: args.iter().map(|a| go(self, a)).collect() }
            }
            K::Foreign { symbol, args, marshalled } => K::Foreign {
                symbol: *symbol,
                args: args.iter().map(|a| go(self, a)).collect(),
                marshalled: *marshalled,
            },
            K::App { func, args, reuse, alloc } => K::App {
                func: Box::new(go(self, func)),
                args: args.iter().map(|a| go(self, a)).collect(),
                reuse: reuse.clone(),
                alloc: *alloc,
            },
            K::If { cond, then, els } => K::If {
                cond: Box::new(go(self, cond)),
                then: Box::new(go(self, then)),
                els: Box::new(go(self, els)),
            },
            K::Let { local, value, body } => K::Let {
                local: *local,
                value: Box::new(go(self, value)),
                body: Box::new(go(self, body)),
            },
            K::MakeData { tag, args, reuse, scalars, niche } => K::MakeData {
                tag: *tag,
                args: args.iter().map(|a| go(self, a)).collect(),
                reuse: *reuse,
                scalars: *scalars,
                niche: *niche,
            },
            K::DataTag { base, niche } => {
                K::DataTag { base: Box::new(go(self, base)), niche: *niche }
            }
            K::DataField { base, index, scalar, niche } => K::DataField {
                base: Box::new(go(self, base)),
                index: *index,
                scalar: *scalar,
                niche: *niche,
            },
            // Spread/LetMany are produced after this pre-count pass; reconstructed
            // with recursed children for totality.
            K::Spread { components } => {
                K::Spread { components: components.iter().map(|a| go(self, a)).collect() }
            }
            K::LetMany { locals, value, body } => K::LetMany {
                locals: locals.clone(),
                value: Box::new(go(self, value)),
                body: Box::new(go(self, body)),
            },
            // Reference-counting and tail-call nodes do not exist in the pre-count
            // Core this runs on; reconstructed with recursed children for totality.
            K::Reset { value, token, body } => K::Reset {
                value: Box::new(go(self, value)),
                token: *token,
                body: Box::new(go(self, body)),
            },
            K::FreeReuse { token, body } => {
                K::FreeReuse { token: *token, body: Box::new(go(self, body)) }
            }
            K::Dup { local, body } => K::Dup { local: *local, body: Box::new(go(self, body)) },
            K::Drop { local, body } => K::Drop { local: *local, body: Box::new(go(self, body)) },
            K::Join { params, body } => {
                K::Join { params: params.clone(), body: Box::new(go(self, body)) }
            }
            K::Recur { args } => K::Recur { args: args.iter().map(|a| go(self, a)).collect() },
            K::HoleStart { hole, body } => {
                K::HoleStart { hole: *hole, body: Box::new(go(self, body)) }
            }
            K::HoleFill { hole, cell, field } => {
                K::HoleFill { hole: *hole, cell: Box::new(go(self, cell)), field: *field }
            }
            K::HoleClose { hole, base } => {
                K::HoleClose { hole: *hole, base: Box::new(go(self, base)) }
            }
        };
        CExpr::new(kind, e.ty.clone())
    }
}

// ---------------------------------------------------------------------------
// Small predicates / helpers over types and expressions.
// ---------------------------------------------------------------------------

/// The head and arguments of a saturated call to a top-level definition.
fn call_target(e: &CExpr) -> Option<(DefId, &[CExpr])> {
    if let K::App { func, args, .. } = &e.kind
        && let K::Global(def) = &func.kind
    {
        return Some((*def, args));
    }
    None
}

/// The element type of a `List a`/`Array a` type.
fn seq_elem(ty: &Ty) -> Option<Ty> {
    match ty {
        Ty::App(head, elem) if matches!(head.as_ref(), Ty::Con(Con::List | Con::Array)) => {
            Some((**elem).clone())
        }
        _ => None,
    }
}

/// Whether `ty` is a `con` (List/Array) sequence application.
fn is_seq(ty: &Ty, con: Con) -> bool {
    matches!(ty, Ty::App(head, _) if matches!(head.as_ref(), Ty::Con(c) if *c == con))
}

/// The elements of a *complete* syntactic literal `e` of sequence kind `seq`, or
/// `None` if `e` is not one. A List literal is a `Cons` chain ending in `Nil`; an
/// Array literal is an `ArrayPush` chain over `ArrayWithCapacity`. A chain that
/// ends in anything else (e.g. `x :: xs` with a variable tail) is not a literal.
fn literal_elems(seq: SeqKind, e: &CExpr) -> Option<Vec<CExpr>> {
    match seq {
        SeqKind::List => {
            let mut elems = Vec::new();
            let mut cur = e;
            loop {
                match &cur.kind {
                    K::MakeData { tag, args, .. } if *tag == NIL_TAG && args.is_empty() => {
                        return Some(elems);
                    }
                    K::MakeData { tag, args, .. } if *tag == CONS_TAG && args.len() == 2 => {
                        elems.push(args[0].clone());
                        cur = &args[1];
                    }
                    _ => return None,
                }
            }
        }
        SeqKind::Array => {
            // Collect pushes from the outside in (last element first), then reverse.
            let mut elems = Vec::new();
            let mut cur = e;
            loop {
                match &cur.kind {
                    K::Prim { op: Prim::ArrayWithCapacity, .. } => {
                        elems.reverse();
                        return Some(elems);
                    }
                    K::Prim { op: Prim::ArrayPush, args } if args.len() == 2 => {
                        elems.push(args[1].clone());
                        cur = &args[0];
                    }
                    _ => return None,
                }
            }
        }
    }
}

/// The number of Core nodes in `e` (for the unroll node budget).
fn node_count(e: &CExpr) -> usize {
    let mut n = 0;
    walk_pre(e, &mut |_| n += 1);
    n
}

/// Whether `e` contains a `MakeClosure` (a nested lambda). A lambda whose body
/// does is not inlined into a loop — its lifted function would dangle.
fn body_has_closure(e: &CExpr) -> bool {
    let mut found = false;
    walk_pre(e, &mut |n| {
        if matches!(&n.kind, K::MakeClosure { .. }) {
            found = true;
        }
    });
    found
}

/// The type of the first `Local(l)` use in `e`, if any (to type a lifted capture
/// parameter).
fn local_type_in(e: &CExpr, l: LocalId) -> Option<Ty> {
    let mut found = None;
    walk_pre(e, &mut |n| {
        if found.is_none()
            && let K::Local(x) = &n.kind
            && *x == l
        {
            found = Some(n.ty.clone());
        }
    });
    found
}

/// Visits every subexpression of `e` (pre-order).
fn walk_pre(e: &CExpr, f: &mut impl FnMut(&CExpr)) {
    f(e);
    match &e.kind {
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => {}
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args } => {
            args.iter().for_each(|a| walk_pre(a, f));
        }
        K::App { func, args, .. } => {
            walk_pre(func, f);
            args.iter().for_each(|a| walk_pre(a, f));
        }
        K::If { cond, then, els } => {
            walk_pre(cond, f);
            walk_pre(then, f);
            walk_pre(els, f);
        }
        K::Let { value, body, .. }
        | K::Reset { value, body, .. }
        | K::LetMany { value, body, .. } => {
            walk_pre(value, f);
            walk_pre(body, f);
        }
        K::Spread { components } => components.iter().for_each(|a| walk_pre(a, f)),
        K::DataTag { base, .. } | K::DataField { base, .. } => walk_pre(base, f),
        K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::FreeReuse { body, .. }
        | K::Join { body, .. }
        | K::HoleStart { body, .. } => walk_pre(body, f),
        K::HoleFill { cell, .. } => walk_pre(cell, f),
        K::HoleClose { base, .. } => walk_pre(base, f),
    }
}

// ---------------------------------------------------------------------------
// Generation.
// ---------------------------------------------------------------------------

/// Builds the synthesized loop for `chain`, threading loop-invariant inputs and
/// iteration state as parameters and inlining literal element functions.
struct LoopGen {
    /// The synthetic loop's definition id.
    def: DefId,
    /// The next free loop-local slot.
    next: usize,
    /// Loop parameters, in order (the call passes `call_args` positionally).
    params: Vec<(LocalId, Ty)>,
    /// The call-site argument for each parameter (in the consuming def's space).
    call_args: Vec<CExpr>,
    /// A consuming-def local already lifted to a loop parameter (dedup).
    extern_local: FxHashMap<LocalId, LocalId>,
    /// The iteration-state parameters (advanced on each back-edge).
    iter_locals: Vec<LocalId>,
    /// The accumulator parameter, for an accumulating sink.
    acc_local: Option<LocalId>,
    /// The loop's result type.
    result_ty: Ty,
}

impl LoopGen {
    fn new(def: DefId, result_ty: Ty) -> Self {
        Self {
            def,
            next: 0,
            params: Vec::new(),
            call_args: Vec::new(),
            extern_local: FxHashMap::default(),
            iter_locals: Vec::new(),
            acc_local: None,
            result_ty,
        }
    }

    fn fresh(&mut self) -> LocalId {
        let id = LocalId::from_index(self.next);
        self.next += 1;
        id
    }

    /// Adds a loop parameter with its call-site argument, returning its slot.
    fn add_param(&mut self, ty: Ty, arg: CExpr) -> LocalId {
        let l = self.fresh();
        self.params.push((l, ty));
        self.call_args.push(arg);
        l
    }

    /// Lifts a consuming-def local `outer` to a loop parameter (deduplicated), so a
    /// captured value or a passed function value is threaded in once.
    fn extern_input(&mut self, outer: LocalId, ty: Ty, arg: CExpr) -> LocalId {
        if let Some(&p) = self.extern_local.get(&outer) {
            return p;
        }
        let p = self.add_param(ty, arg);
        self.extern_local.insert(outer, p);
        p
    }

    /// A loop-invariant value usable in the loop body: a literal or global is used
    /// directly; a consuming-def local is lifted to a parameter.
    fn extern_value(&mut self, v: CExpr) -> CExpr {
        match &v.kind {
            K::Global(_) | K::Lit(_) => v,
            K::Local(outer) => {
                local(self.extern_input(*outer, v.ty.clone(), v.clone()), v.ty.clone())
            }
            _ => {
                let p = self.add_param(v.ty.clone(), v.clone());
                local(p, v.ty.clone())
            }
        }
    }

    /// The back-edge: re-enter the loop with advanced iteration state and `new_acc`,
    /// forwarding every invariant parameter unchanged.
    fn recur(&self, advance: &[CExpr], new_acc: Option<CExpr>) -> CExpr {
        let args: Vec<CExpr> = self
            .params
            .iter()
            .map(|(l, ty)| {
                if Some(*l) == self.acc_local {
                    new_acc.clone().expect("acc value for an accumulating loop")
                } else if let Some(k) = self.iter_locals.iter().position(|x| x == l) {
                    advance[k].clone()
                } else {
                    local(*l, ty.clone())
                }
            })
            .collect();
        CExpr::new(
            K::App {
                func: Box::new(global(self.def)),
                args,
                reuse: Vec::new(),
                alloc: ClosureAlloc::Heap,
            },
            self.result_ty.clone(),
        )
    }
}

impl Fuser<'_> {
    /// Generates the loop for `chain` and returns the call replacing the pipeline.
    fn generate(&mut self, chain: Chain, base_fns: &[CoreFn]) -> CExpr {
        let Chain { source, stages, consumer, source_elem_ty, consumer_elem_ty, result_ty } = chain;
        let loop_def = DefId::new(
            self.source,
            Symbol::intern(&format!("fuse#{}#{}", self.consuming.as_str(), self.chain_index)),
        );
        self.chain_index += 1;
        // A `member` target is a loop-invariant value that may itself be a pipeline;
        // rewrite it before it is threaded into the loop or unrolled form.
        let consumer = match consumer {
            Consumer::Member(t) => Consumer::Member(self.rewrite(&t, base_fns)),
            other => other,
        };

        // A small literal source unrolls to straight-line code (no loop). If the
        // unrolled form would exceed the budget, fall back to walking the literal as
        // a value source.
        let source = match source {
            Source::Literal { elems, seq } => {
                if let Some(u) = self.try_unroll(&elems, &stages, &consumer, &result_ty, base_fns) {
                    return u;
                }
                if is_seq(&seq.ty, Con::Array) {
                    Source::ArrayValue { seq }
                } else {
                    Source::ListValue { seq }
                }
            }
            other => other,
        };

        let mut g = LoopGen::new(loop_def, result_ty.clone());

        // The source: invariant inputs, iteration state, the done test, the current
        // element (with its binds), the advance, and a capacity hint for a builder.
        let src = self.build_source(&mut g, source, &source_elem_ty, consumer.reverse(), base_fns);

        // The accumulator parameter, if the consumer accumulates (a fold/scalar
        // result, or an `Array` builder buffer). Searching consumers and `List`
        // builders carry none.
        let acc_local = if consumer.has_acc() {
            let (ty, init) = self.acc_init(&consumer, &result_ty, &src, base_fns);
            let l = g.add_param(ty, init);
            g.acc_local = Some(l);
            Some(l)
        } else {
            None
        };

        // The loop body: `if done then finish else <element binds> <pipeline>`.
        let advance = src.advance.clone();
        let body_tail = self.emit_pipeline(
            &mut g,
            src.elem.clone(),
            consumer_elem_ty,
            &stages,
            &consumer,
            &advance,
            acc_local,
        );
        let mut non_done = body_tail;
        for (l, v) in src.elem_binds.iter().rev() {
            let ty = non_done.ty.clone();
            non_done = CExpr::new(
                K::Let { local: *l, value: Box::new(v.clone()), body: Box::new(non_done) },
                ty,
            );
        }
        let finish = self.emit_finish(&consumer, &result_ty, acc_local);
        let loop_body = if_(src.done.clone(), finish, non_done, result_ty.clone());

        // Assemble the loop definition and its call.
        let params: Vec<LocalId> = g.params.iter().map(|(l, _)| *l).collect();
        let abi = self.loop_abi(&g, &result_ty);
        let arity = params.len();
        let lowered = LoweredDef {
            def: loop_def,
            fns: vec![CoreFn { params, captures: Vec::new(), body: loop_body }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            entry_spread_params: Vec::new(),
        };
        self.loops.push(FusedLoop { lowered, abi, arity });

        let call = CExpr::new(
            K::App {
                func: Box::new(global(loop_def)),
                args: g.call_args.clone(),
                reuse: Vec::new(),
                alloc: ClosureAlloc::Heap,
            },
            result_ty,
        );
        // Wrap any source binding (a value source bound once to avoid re-evaluation).
        let mut result = call;
        for (l, v) in src.outer_binds.into_iter().rev() {
            let ty = result.ty.clone();
            result =
                CExpr::new(K::Let { local: l, value: Box::new(v), body: Box::new(result) }, ty);
        }
        result
    }

    /// The accumulator's type and initial (call-site) value for an accumulating
    /// consumer.
    fn acc_init(
        &mut self,
        consumer: &Consumer,
        result_ty: &Ty,
        src: &SourceShape,
        base_fns: &[CoreFn],
    ) -> (Ty, CExpr) {
        match consumer {
            Consumer::Sum | Consumer::Length => (Ty::int(), int_lit(0)),
            Consumer::Foldl { init, .. } | Consumer::Foldr { init, .. } => {
                (result_ty.clone(), self.rewrite(init, base_fns))
            }
            // An `Array` builder: a fresh buffer pre-sized to the source's length
            // (an upper bound for a filter), so pushes never reallocate for a map.
            Consumer::Build { .. } => (
                result_ty.clone(),
                CExpr::new(
                    K::Prim { op: Prim::ArrayWithCapacity, args: vec![src.capacity.clone()] },
                    result_ty.clone(),
                ),
            ),
            Consumer::All(_) | Consumer::Any(_) | Consumer::Find(_) | Consumer::Member(_) => {
                unreachable!("a searching consumer has no accumulator")
            }
        }
    }

    /// Builds the source driver into `g`, returning the iteration shape. `reverse`
    /// drives a numeric/indexed source high-to-low (for `foldr`).
    ///
    /// Invariant: `call_args`/`capacity`/`outer_binds` are in the *consuming*
    /// definition's space; the loop body (`done`/`elem`/`advance`) is in the loop's
    /// space (reads loop parameters). A numeric bound used in more than one position
    /// is bound to a consuming local first, so it is evaluated once.
    fn build_source(
        &mut self,
        g: &mut LoopGen,
        source: Source,
        elem_ty: &Ty,
        reverse: bool,
        base_fns: &[CoreFn],
    ) -> SourceShape {
        match source {
            Source::Range { lo, hi } => {
                let lo = self.rewrite(&lo, base_fns);
                let hi = self.rewrite(&hi, base_fns);
                // Bind both bounds in the consuming def (each used in several call
                // arguments below) so they are evaluated once.
                let lo_c = self.fresh_consuming();
                let hi_c = self.fresh_consuming();
                let lo_v = local(lo_c, Ty::int());
                let hi_v = local(hi_c, Ty::int());
                let outer_binds = vec![(lo_c, lo), (hi_c, hi)];
                let capacity = prim(Prim::IntSub, vec![hi_v.clone(), lo_v.clone()]);
                let v = g.fresh();
                if reverse {
                    let lo_p = g.add_param(Ty::int(), lo_v);
                    g.params.push((v, Ty::int()));
                    g.call_args.push(prim(Prim::IntSub, vec![hi_v, int_lit(1)]));
                    g.iter_locals.push(v);
                    SourceShape {
                        done: prim(Prim::IntLt, vec![local(v, Ty::int()), local(lo_p, Ty::int())]),
                        elem_binds: Vec::new(),
                        elem: local(v, Ty::int()),
                        advance: vec![prim(Prim::IntSub, vec![local(v, Ty::int()), int_lit(1)])],
                        outer_binds,
                        capacity,
                    }
                } else {
                    let hi_p = g.add_param(Ty::int(), hi_v);
                    g.params.push((v, Ty::int()));
                    g.call_args.push(lo_v);
                    g.iter_locals.push(v);
                    SourceShape {
                        done: prim(Prim::IntGe, vec![local(v, Ty::int()), local(hi_p, Ty::int())]),
                        elem_binds: Vec::new(),
                        elem: local(v, Ty::int()),
                        advance: vec![prim(Prim::IntAdd, vec![local(v, Ty::int()), int_lit(1)])],
                        outer_binds,
                        capacity,
                    }
                }
            }
            Source::Init { n, f } => {
                let n = self.rewrite(&n, base_fns);
                let n_c = self.fresh_consuming();
                let n_v = local(n_c, Ty::int());
                let (i, advance, done) = self.index_iter(g, &n_v, reverse);
                let applied = apply_fn(g, &f, vec![local(i, Ty::int())], elem_ty.clone());
                let elem = g.fresh();
                SourceShape {
                    done,
                    elem_binds: vec![(elem, applied)],
                    elem: local(elem, elem_ty.clone()),
                    advance: vec![advance],
                    outer_binds: vec![(n_c, n)],
                    capacity: n_v,
                }
            }
            Source::Repeat { n, x } => {
                let n = self.rewrite(&n, base_fns);
                let x = self.rewrite(&x, base_fns);
                let n_c = self.fresh_consuming();
                let n_v = local(n_c, Ty::int());
                let x_p = g.add_param(elem_ty.clone(), x);
                let (_i, advance, done) = self.index_iter(g, &n_v, reverse);
                SourceShape {
                    done,
                    elem_binds: Vec::new(),
                    elem: local(x_p, elem_ty.clone()),
                    advance: vec![advance],
                    outer_binds: vec![(n_c, n)],
                    capacity: n_v,
                }
            }
            Source::ArrayValue { seq } => {
                let seq = self.rewrite(&seq, base_fns);
                // Bind the source once (it may be a compound, multi-use expression).
                let s = self.fresh_consuming();
                let seq_local = local(s, seq.ty.clone());
                let seq_p = g.add_param(seq.ty.clone(), seq_local.clone());
                let len_v = prim(Prim::ArrayLength, vec![seq_local.clone()]);
                let (i, advance, done) = self.index_iter(g, &len_v, reverse);
                let elem = g.fresh();
                let get = CExpr::new(
                    K::Prim {
                        op: Prim::ArrayGet,
                        args: vec![local(seq_p, seq.ty.clone()), local(i, Ty::int())],
                    },
                    elem_ty.clone(),
                );
                SourceShape {
                    done,
                    elem_binds: vec![(elem, get)],
                    elem: local(elem, elem_ty.clone()),
                    advance: vec![advance],
                    outer_binds: vec![(s, seq)],
                    capacity: prim(Prim::ArrayLength, vec![seq_local]),
                }
            }
            Source::ListValue { seq } => {
                // Reverse iteration over a List value is ruled out at recognition.
                let seq = self.rewrite(&seq, base_fns);
                let list_ty = seq.ty.clone();
                let cur = g.add_param(list_ty.clone(), seq);
                g.iter_locals.push(cur);
                let head = g.fresh();
                let head_get = data_field(local(cur, list_ty.clone()), 0, elem_ty.clone());
                let tail = data_field(local(cur, list_ty.clone()), 1, list_ty.clone());
                let tag = CExpr::new(
                    K::DataTag { base: Box::new(local(cur, list_ty.clone())), niche: None },
                    Ty::int(),
                );
                SourceShape {
                    done: prim(Prim::Eq, vec![tag, int_lit(i64::from(NIL_TAG))]),
                    elem_binds: vec![(head, head_get)],
                    elem: local(head, elem_ty.clone()),
                    advance: vec![tail],
                    outer_binds: Vec::new(),
                    capacity: int_lit(0),
                }
            }
            // A literal source is unrolled (or converted to a value source) before
            // `build_source` is reached.
            Source::Literal { .. } => unreachable!("a literal source is handled before the loop"),
        }
    }

    /// Allocates the index iterator over `[0, n)` for an indexed/produced source:
    /// the index loop parameter (initialized via `g.call_args`), plus its advance
    /// and done expressions. Counts down from `n-1` when `reverse`. `n` is a
    /// consuming-space bound used for the initial value and the forward done test.
    fn index_iter(&self, g: &mut LoopGen, n: &CExpr, reverse: bool) -> (LocalId, CExpr, CExpr) {
        let i = g.fresh();
        if reverse {
            // i from n-1 downto 0; done when i < 0 (no `n` needed in the body).
            g.params.push((i, Ty::int()));
            g.call_args.push(prim(Prim::IntSub, vec![n.clone(), int_lit(1)]));
            g.iter_locals.push(i);
            (
                i,
                prim(Prim::IntSub, vec![local(i, Ty::int()), int_lit(1)]),
                prim(Prim::IntLt, vec![local(i, Ty::int()), int_lit(0)]),
            )
        } else {
            // i from 0 up to n-1; done when i >= n (n threaded as a loop parameter).
            let n_p = g.add_param(Ty::int(), n.clone());
            g.params.push((i, Ty::int()));
            g.call_args.push(int_lit(0));
            g.iter_locals.push(i);
            (
                i,
                prim(Prim::IntAdd, vec![local(i, Ty::int()), int_lit(1)]),
                prim(Prim::IntGe, vec![local(i, Ty::int()), local(n_p, Ty::int())]),
            )
        }
    }

    /// Emits the non-done branch: apply the stages to `elem`, then the consumer.
    #[allow(clippy::too_many_arguments)]
    fn emit_pipeline(
        &mut self,
        g: &mut LoopGen,
        elem: CExpr,
        elem_ty: Ty,
        stages: &[Stage],
        consumer: &Consumer,
        advance: &[CExpr],
        acc_local: Option<LocalId>,
    ) -> CExpr {
        match stages.split_first() {
            None => self.emit_consume(g, elem, elem_ty, consumer, advance, acc_local),
            Some((Stage::Map { f, out }, rest)) => {
                let applied = apply_fn(g, f, vec![elem], out.clone());
                let y = g.fresh();
                let body = self.emit_pipeline(
                    g,
                    local(y, out.clone()),
                    out.clone(),
                    rest,
                    consumer,
                    advance,
                    acc_local,
                );
                let bty = body.ty.clone();
                CExpr::new(K::Let { local: y, value: Box::new(applied), body: Box::new(body) }, bty)
            }
            Some((Stage::Filter(p), rest)) => {
                let cond = apply_fn(g, p, vec![elem.clone()], Ty::bool());
                let keep = self.emit_pipeline(g, elem, elem_ty, rest, consumer, advance, acc_local);
                let skip = g.recur(advance, acc_local.map(|a| local(a, g.result_ty.clone())));
                if_(cond, keep, skip, g.result_ty.clone())
            }
        }
    }

    /// Emits the consumer's per-element action (the deepest part of the non-done
    /// branch), given the fully-transformed element of type `elem_ty`.
    fn emit_consume(
        &mut self,
        g: &mut LoopGen,
        elem: CExpr,
        elem_ty: Ty,
        consumer: &Consumer,
        advance: &[CExpr],
        acc_local: Option<LocalId>,
    ) -> CExpr {
        let acc_v = || local(acc_local.expect("acc"), g.result_ty.clone());
        match consumer {
            Consumer::Sum => g.recur(advance, Some(prim(Prim::IntAdd, vec![acc_v(), elem]))),
            Consumer::Length => {
                g.recur(advance, Some(prim(Prim::IntAdd, vec![acc_v(), int_lit(1)])))
            }
            Consumer::Foldl { step, .. } => {
                let new = apply_fn(g, step, vec![acc_v(), elem], g.result_ty.clone());
                g.recur(advance, Some(new))
            }
            // `foldr` over a reversed source: accumulate `f elem acc` (element first).
            Consumer::Foldr { step, .. } => {
                let new = apply_fn(g, step, vec![elem, acc_v()], g.result_ty.clone());
                g.recur(advance, Some(new))
            }
            Consumer::All(p) => {
                let cond = apply_fn(g, p, vec![elem], Ty::bool());
                let cont = g.recur(advance, None);
                if_(cond, cont, false_lit(), g.result_ty.clone())
            }
            Consumer::Any(p) => {
                let cond = apply_fn(g, p, vec![elem], Ty::bool());
                let cont = g.recur(advance, None);
                if_(cond, true_lit(), cont, g.result_ty.clone())
            }
            Consumer::Member(target) => {
                let t = g.extern_value(target.clone());
                let cond = prim(Prim::Eq, vec![elem, t]);
                let cont = g.recur(advance, None);
                if_(cond, true_lit(), cont, g.result_ty.clone())
            }
            Consumer::Find(p) => {
                let cond = apply_fn(g, p, vec![elem.clone()], Ty::bool());
                let some = self.option_some(elem, &g.result_ty);
                let cont = g.recur(advance, None);
                if_(cond, some, cont, g.result_ty.clone())
            }
            Consumer::Build { f, filter, seq } => {
                self.emit_build(g, elem, elem_ty, f, *filter, *seq, advance, acc_local)
            }
        }
    }

    /// Emits a terminal builder's per-element action: push (or cons) the produced
    /// element, optionally guarded by a filter predicate.
    #[allow(clippy::too_many_arguments)]
    fn emit_build(
        &mut self,
        g: &mut LoopGen,
        elem: CExpr,
        elem_ty: Ty,
        f: &FnArg,
        filter: bool,
        seq: SeqKind,
        advance: &[CExpr],
        acc_local: Option<LocalId>,
    ) -> CExpr {
        let out_ty = seq_elem(&g.result_ty).unwrap_or(elem_ty);
        // The element to add: `elem` for a terminal filter, `f elem` for a map.
        let (built, keep_cond) = if filter {
            (elem.clone(), Some(apply_fn(g, f, vec![elem], Ty::bool())))
        } else {
            (apply_fn(g, f, vec![elem], out_ty.clone()), None)
        };
        let push_branch = match seq {
            SeqKind::Array => {
                let acc = acc_local.expect("array builder has an acc");
                let pushed = CExpr::new(
                    K::Prim {
                        op: Prim::ArrayPush,
                        args: vec![local(acc, g.result_ty.clone()), built],
                    },
                    g.result_ty.clone(),
                );
                g.recur(advance, Some(pushed))
            }
            SeqKind::List => {
                // Tail-recursion-modulo-cons: `let rec = recur in built :: rec`, so
                // the tail-call transform builds the spine in order, in place.
                let rec = g.fresh();
                let recur = g.recur(advance, None);
                let cons = CExpr::new(
                    K::MakeData {
                        tag: CONS_TAG,
                        args: vec![built, local(rec, g.result_ty.clone())],
                        reuse: None,
                        scalars: 0,
                        niche: None,
                    },
                    g.result_ty.clone(),
                );
                CExpr::new(
                    K::Let { local: rec, value: Box::new(recur), body: Box::new(cons) },
                    g.result_ty.clone(),
                )
            }
        };
        match keep_cond {
            None => push_branch,
            Some(cond) => {
                let skip = g.recur(advance, acc_local.map(|a| local(a, g.result_ty.clone())));
                if_(cond, push_branch, skip, g.result_ty.clone())
            }
        }
    }

    /// The done-branch value (the consumer's finish).
    fn emit_finish(
        &self,
        consumer: &Consumer,
        result_ty: &Ty,
        acc_local: Option<LocalId>,
    ) -> CExpr {
        match consumer {
            Consumer::Sum | Consumer::Length | Consumer::Foldl { .. } | Consumer::Foldr { .. } => {
                local(acc_local.expect("acc"), result_ty.clone())
            }
            Consumer::All(_) => true_lit(),
            Consumer::Any(_) | Consumer::Member(_) => false_lit(),
            Consumer::Find(_) => self.option_none(result_ty),
            Consumer::Build { seq: SeqKind::Array, .. } => {
                local(acc_local.expect("acc"), result_ty.clone())
            }
            Consumer::Build { seq: SeqKind::List, .. } => CExpr::new(
                K::MakeData {
                    tag: NIL_TAG,
                    args: Vec::new(),
                    reuse: None,
                    scalars: 0,
                    niche: None,
                },
                result_ty.clone(),
            ),
        }
    }

    /// `Some elem` of type `result_ty` (a niche `Option`), for `find`.
    fn option_some(&self, elem: CExpr, result_ty: &Ty) -> CExpr {
        let (_, some) = self.option_tags();
        CExpr::new(
            K::MakeData {
                tag: some,
                args: vec![elem],
                reuse: None,
                scalars: 0,
                niche: crate::niche::niche_scheme(self.db, result_ty),
            },
            result_ty.clone(),
        )
    }

    /// `None` of type `result_ty` (a niche `Option`), for `find`.
    fn option_none(&self, result_ty: &Ty) -> CExpr {
        let (none, _) = self.option_tags();
        CExpr::new(
            K::MakeData {
                tag: none,
                args: Vec::new(),
                reuse: None,
                scalars: 0,
                niche: crate::niche::niche_scheme(self.db, result_ty),
            },
            result_ty.clone(),
        )
    }

    /// The prelude `Option`'s `(None, Some)` constructor tags.
    fn option_tags(&self) -> (u32, u32) {
        let resolve = || {
            let prelude = fai_resolve::prelude_module_file(self.db)?;
            let decls = fai_resolve::type_decls(self.db, prelude);
            let none = decls.ctor(Symbol::intern("None"))?.tag;
            let some = decls.ctor(Symbol::intern("Some"))?.tag;
            Some((none, some))
        };
        resolve().unwrap_or((0, 1))
    }

    /// The loop's native ABI: a raw scalar for each `Int`/`Float` parameter, and
    /// for the result its scalar/niche representation (so a direct caller marshals
    /// a niche `Option` from `find` wrapper-free). Direct-callable (register ABI).
    fn loop_abi(&self, g: &LoopGen, result_ty: &Ty) -> FnAbi {
        let ret = match result_ty {
            Ty::Con(Con::Int) => Repr::ScalarInt,
            Ty::Con(Con::Float) => Repr::ScalarFloat,
            _ => match crate::niche::niche_scheme(self.db, result_ty) {
                Some(k) => Repr::Niche(k),
                None => Repr::Uniform,
            },
        };
        FnAbi {
            params: g.params.iter().map(|(_, ty)| repr_of(ty)).collect(),
            ret,
            register_abi: !g.params.is_empty(),
        }
    }

    // -----------------------------------------------------------------------
    // Literal unrolling: straight-line code over a small literal's elements.
    // -----------------------------------------------------------------------

    /// The unrolled (loop-free) form of a chain over a literal's `elems`, or `None`
    /// if it would exceed [`UNROLL_NODE_BUDGET`] (then the caller falls back to
    /// walking the literal as a value source).
    fn try_unroll(
        &mut self,
        elems: &[CExpr],
        stages: &[Stage],
        consumer: &Consumer,
        result_ty: &Ty,
        base_fns: &[CoreFn],
    ) -> Option<CExpr> {
        let mut order: Vec<CExpr> = elems.to_vec();
        if consumer.reverse() {
            order.reverse();
        }
        let expr = if consumer.has_acc() {
            // Thread the accumulator through the elements via nested lets.
            let mut acc = self.unroll_acc_init(consumer, result_ty, elems.len(), base_fns);
            let mut binds: Vec<(LocalId, CExpr)> = Vec::new();
            for ei in order {
                let a = self.fresh_consuming();
                let acc_v = local(a, result_ty.clone());
                binds.push((a, acc));
                acc = self.unroll_acc(ei, stages, consumer, acc_v, result_ty);
            }
            for (l, v) in binds.into_iter().rev() {
                let ty = acc.ty.clone();
                acc = CExpr::new(K::Let { local: l, value: Box::new(v), body: Box::new(acc) }, ty);
            }
            acc
        } else {
            // Build the continuation inward (last element first).
            let mut cont = self.emit_finish(consumer, result_ty, None);
            for ei in order.into_iter().rev() {
                cont = self.unroll_cont(ei, stages, consumer, cont, result_ty);
            }
            cont
        };
        (node_count(&expr) <= UNROLL_NODE_BUDGET).then_some(expr)
    }

    /// The initial accumulator (consuming space) for an unrolled accumulating chain.
    fn unroll_acc_init(
        &mut self,
        consumer: &Consumer,
        result_ty: &Ty,
        len: usize,
        base_fns: &[CoreFn],
    ) -> CExpr {
        match consumer {
            Consumer::Sum | Consumer::Length => int_lit(0),
            Consumer::Foldl { init, .. } | Consumer::Foldr { init, .. } => {
                self.rewrite(init, base_fns)
            }
            Consumer::Build { .. } => CExpr::new(
                K::Prim { op: Prim::ArrayWithCapacity, args: vec![int_lit(len as i64)] },
                result_ty.clone(),
            ),
            _ => unreachable!("a searching consumer has no accumulator"),
        }
    }

    /// Applies the stages then the accumulating consumer to one literal element,
    /// producing the new accumulator (a filter rejects to `acc_v` unchanged).
    fn unroll_acc(
        &mut self,
        elem: CExpr,
        stages: &[Stage],
        consumer: &Consumer,
        acc_v: CExpr,
        result_ty: &Ty,
    ) -> CExpr {
        match stages.split_first() {
            None => self.unroll_step(consumer, acc_v, elem, result_ty),
            Some((Stage::Map { f, out }, rest)) => {
                let y = self.apply_consuming(f, vec![elem], out.clone());
                let q = self.fresh_consuming();
                let body = self.unroll_acc(local(q, out.clone()), rest, consumer, acc_v, result_ty);
                let ty = body.ty.clone();
                CExpr::new(K::Let { local: q, value: Box::new(y), body: Box::new(body) }, ty)
            }
            Some((Stage::Filter(p), rest)) => {
                let cond = self.apply_consuming(p, vec![elem.clone()], Ty::bool());
                let keep = self.unroll_acc(elem, rest, consumer, acc_v.clone(), result_ty);
                if_(cond, keep, acc_v, result_ty.clone())
            }
        }
    }

    /// The accumulating consumer's step on a fully-transformed element.
    fn unroll_step(
        &mut self,
        consumer: &Consumer,
        acc_v: CExpr,
        elem: CExpr,
        result_ty: &Ty,
    ) -> CExpr {
        match consumer {
            Consumer::Sum => prim(Prim::IntAdd, vec![acc_v, elem]),
            Consumer::Length => prim(Prim::IntAdd, vec![acc_v, int_lit(1)]),
            Consumer::Foldl { step, .. } => {
                self.apply_consuming(step, vec![acc_v, elem], result_ty.clone())
            }
            Consumer::Foldr { step, .. } => {
                self.apply_consuming(step, vec![elem, acc_v], result_ty.clone())
            }
            Consumer::Build { f, filter, seq: SeqKind::Array } => {
                let out_ty = seq_elem(result_ty).unwrap_or_else(|| elem.ty.clone());
                if *filter {
                    let cond = self.apply_consuming(f, vec![elem.clone()], Ty::bool());
                    let pushed = CExpr::new(
                        K::Prim { op: Prim::ArrayPush, args: vec![acc_v.clone(), elem] },
                        result_ty.clone(),
                    );
                    if_(cond, pushed, acc_v, result_ty.clone())
                } else {
                    let built = self.apply_consuming(f, vec![elem], out_ty);
                    CExpr::new(
                        K::Prim { op: Prim::ArrayPush, args: vec![acc_v, built] },
                        result_ty.clone(),
                    )
                }
            }
            _ => unreachable!("non-accumulating consumer in unroll_step"),
        }
    }

    /// Applies the stages then a searching/List-builder consumer to one literal
    /// element, with `cont` the result if this element does not terminate/decide.
    fn unroll_cont(
        &mut self,
        elem: CExpr,
        stages: &[Stage],
        consumer: &Consumer,
        cont: CExpr,
        result_ty: &Ty,
    ) -> CExpr {
        match stages.split_first() {
            None => self.unroll_sink(consumer, elem, cont, result_ty),
            Some((Stage::Map { f, out }, rest)) => {
                let y = self.apply_consuming(f, vec![elem], out.clone());
                let q = self.fresh_consuming();
                let body = self.unroll_cont(local(q, out.clone()), rest, consumer, cont, result_ty);
                let ty = body.ty.clone();
                CExpr::new(K::Let { local: q, value: Box::new(y), body: Box::new(body) }, ty)
            }
            Some((Stage::Filter(p), rest)) => {
                let cond = self.apply_consuming(p, vec![elem.clone()], Ty::bool());
                let keep = self.unroll_cont(elem, rest, consumer, cont.clone(), result_ty);
                if_(cond, keep, cont, result_ty.clone())
            }
        }
    }

    /// A searching/List-builder consumer's per-element action on a fully-transformed
    /// element, with `cont` the fall-through result.
    fn unroll_sink(
        &mut self,
        consumer: &Consumer,
        elem: CExpr,
        cont: CExpr,
        result_ty: &Ty,
    ) -> CExpr {
        match consumer {
            Consumer::All(p) => {
                let cond = self.apply_consuming(p, vec![elem], Ty::bool());
                if_(cond, cont, false_lit(), result_ty.clone())
            }
            Consumer::Any(p) => {
                let cond = self.apply_consuming(p, vec![elem], Ty::bool());
                if_(cond, true_lit(), cont, result_ty.clone())
            }
            Consumer::Member(target) => {
                let cond = prim(Prim::Eq, vec![elem, target.clone()]);
                if_(cond, true_lit(), cont, result_ty.clone())
            }
            Consumer::Find(p) => {
                let cond = self.apply_consuming(p, vec![elem.clone()], Ty::bool());
                let some = self.option_some(elem, result_ty);
                if_(cond, some, cont, result_ty.clone())
            }
            Consumer::Build { f, filter, seq: SeqKind::List } => {
                let out_ty = seq_elem(result_ty).unwrap_or_else(|| elem.ty.clone());
                let cons = |built: CExpr, tail: CExpr, ty: &Ty| {
                    CExpr::new(
                        K::MakeData {
                            tag: CONS_TAG,
                            args: vec![built, tail],
                            reuse: None,
                            scalars: 0,
                            niche: None,
                        },
                        ty.clone(),
                    )
                };
                if *filter {
                    let cond = self.apply_consuming(f, vec![elem.clone()], Ty::bool());
                    let kept = cons(elem, cont.clone(), result_ty);
                    if_(cond, kept, cont, result_ty.clone())
                } else {
                    let built = self.apply_consuming(f, vec![elem], out_ty);
                    cons(built, cont, result_ty)
                }
            }
            _ => unreachable!("accumulating consumer in unroll_sink"),
        }
    }

    /// Applies a function argument in the *consuming* definition's space: a literal
    /// lambda is inlined (its captures stay as their in-scope locals, no lifting); a
    /// value is applied via `apply_n`.
    fn apply_consuming(&mut self, f: &FnArg, args: Vec<CExpr>, result_ty: Ty) -> CExpr {
        match f {
            FnArg::Lambda { params, body, caps } => {
                let mut subst: FxHashMap<LocalId, LocalId> = FxHashMap::default();
                for (slot, outer) in caps {
                    if let K::Local(o) = &outer.kind {
                        subst.insert(*slot, *o);
                    }
                }
                let mut binds: Vec<(LocalId, CExpr)> = Vec::new();
                for (p, a) in params.iter().zip(args) {
                    let q = self.fresh_consuming();
                    subst.insert(*p, q);
                    binds.push((q, a));
                }
                let mut next = self.next_local;
                let mut out = remap(body, &mut subst, &mut next);
                self.next_local = next;
                for (l, v) in binds.into_iter().rev() {
                    let ty = out.ty.clone();
                    out = CExpr::new(
                        K::Let { local: l, value: Box::new(v), body: Box::new(out) },
                        ty,
                    );
                }
                out
            }
            FnArg::Value(v) => CExpr::new(
                K::App {
                    func: Box::new(v.clone()),
                    args,
                    reuse: Vec::new(),
                    alloc: ClosureAlloc::Heap,
                },
                result_ty,
            ),
        }
    }

    /// A fresh local in the *consuming* definition (for binding a value source).
    fn fresh_consuming(&mut self) -> LocalId {
        let id = LocalId::from_index(self.next_local);
        self.next_local += 1;
        id
    }
}

/// The iteration shape a source driver produces.
struct SourceShape {
    /// The done test (Bool): when true the loop returns the consumer's finish.
    done: CExpr,
    /// Lets binding the current element (e.g. an `arrayGet`/spine projection).
    elem_binds: Vec<(LocalId, CExpr)>,
    /// The current element (a local after `elem_binds`).
    elem: CExpr,
    /// The advanced iteration state (one per iter parameter, in order).
    advance: Vec<CExpr>,
    /// Lets to wrap the *call* (binding a value source once in the consuming def).
    outer_binds: Vec<(LocalId, CExpr)>,
    /// An upper bound on the result length, for pre-sizing a builder's buffer.
    capacity: CExpr,
}

/// Applies a function argument to `args`: inlines a literal lambda (binding each
/// argument and remapping captures to loop parameters), or applies a value via
/// `apply_n`.
fn apply_fn(g: &mut LoopGen, f: &FnArg, args: Vec<CExpr>, result_ty: Ty) -> CExpr {
    match f {
        FnArg::Lambda { params, body, caps } => {
            let mut subst: FxHashMap<LocalId, LocalId> = FxHashMap::default();
            for (slot, outer) in caps {
                let outer_local = match &outer.kind {
                    K::Local(l) => *l,
                    _ => continue,
                };
                let p = g.extern_input(outer_local, outer.ty.clone(), outer.clone());
                subst.insert(*slot, p);
            }
            // Bind each argument to a fresh loop local (routing representation
            // coercion through a binding, as a call boundary would).
            let mut binds: Vec<(LocalId, CExpr)> = Vec::new();
            for (p, a) in params.iter().zip(args) {
                let q = g.fresh();
                subst.insert(*p, q);
                binds.push((q, a));
            }
            let remapped = remap(body, &mut subst, &mut g.next);
            let mut out = remapped;
            for (l, v) in binds.into_iter().rev() {
                let ty = out.ty.clone();
                out = CExpr::new(K::Let { local: l, value: Box::new(v), body: Box::new(out) }, ty);
            }
            out
        }
        FnArg::Value(v) => {
            let func = match &v.kind {
                K::Global(_) => v.clone(),
                K::Local(outer) => {
                    local(g.extern_input(*outer, v.ty.clone(), v.clone()), v.ty.clone())
                }
                _ => {
                    // An unexpected non-atom function value: thread it as a fresh
                    // parameter keyed on nothing (no dedup).
                    let p = g.add_param(v.ty.clone(), v.clone());
                    local(p, v.ty.clone())
                }
            };
            CExpr::new(
                K::App { func: Box::new(func), args, reuse: Vec::new(), alloc: ClosureAlloc::Heap },
                result_ty,
            )
        }
    }
}

/// Copies `e`, remapping every local through `subst` (allocating a fresh loop slot
/// the first time an unmapped local is seen) and keeping each node's type.
fn remap(e: &CExpr, subst: &mut FxHashMap<LocalId, LocalId>, next: &mut usize) -> CExpr {
    let ty = e.ty.clone();
    let r = |c: &CExpr, subst: &mut FxHashMap<LocalId, LocalId>, next: &mut usize| {
        remap(c, subst, next)
    };
    let kind = match &e.kind {
        K::Local(l) => K::Local(remap_local(*l, subst, next)),
        K::Lit(_) | K::Global(_) | K::Error => e.kind.clone(),
        K::Prim { op, args } => {
            K::Prim { op: *op, args: args.iter().map(|a| r(a, subst, next)).collect() }
        }
        K::MakeData { tag, args, reuse, scalars, niche } => K::MakeData {
            tag: *tag,
            args: args.iter().map(|a| r(a, subst, next)).collect(),
            reuse: reuse.map(|t| remap_local(t, subst, next)),
            scalars: *scalars,
            niche: *niche,
        },
        K::App { func, args, reuse, alloc } => K::App {
            func: Box::new(r(func, subst, next)),
            args: args.iter().map(|a| r(a, subst, next)).collect(),
            reuse: reuse.iter().map(|t| t.map(|l| remap_local(l, subst, next))).collect(),
            alloc: *alloc,
        },
        K::If { cond, then, els } => K::If {
            cond: Box::new(r(cond, subst, next)),
            then: Box::new(r(then, subst, next)),
            els: Box::new(r(els, subst, next)),
        },
        K::Let { local, value, body } => {
            let value = Box::new(r(value, subst, next));
            let local = remap_local(*local, subst, next);
            K::Let { local, value, body: Box::new(r(body, subst, next)) }
        }
        K::DataTag { base, niche } => {
            K::DataTag { base: Box::new(r(base, subst, next)), niche: *niche }
        }
        K::DataField { base, index, scalar, niche } => K::DataField {
            base: Box::new(r(base, subst, next)),
            index: *index,
            scalar: *scalar,
            niche: *niche,
        },
        K::MakeClosure { func, captures, alloc } => K::MakeClosure {
            func: *func,
            captures: captures.iter().map(|c| remap_local(*c, subst, next)).collect(),
            alloc: *alloc,
        },
        // The pre-count Core a lambda body is taken from has no rc/tail-call nodes.
        other => other.clone(),
    };
    CExpr::new(kind, ty)
}

fn remap_local(l: LocalId, subst: &mut FxHashMap<LocalId, LocalId>, next: &mut usize) -> LocalId {
    if let Some(&r) = subst.get(&l) {
        return r;
    }
    let r = LocalId::from_index(*next);
    *next += 1;
    subst.insert(l, r);
    r
}

/// The native representation of a loop parameter type (raw scalars for
/// `Int`/`Float`, uniform otherwise).
fn repr_of(ty: &Ty) -> Repr {
    match ty {
        Ty::Con(Con::Int) => Repr::ScalarInt,
        Ty::Con(Con::Float) => Repr::ScalarFloat,
        _ => Repr::Uniform,
    }
}

// ---------------------------------------------------------------------------
// Dead lifted-function pruning.
// ---------------------------------------------------------------------------

/// Removes lifted functions no `MakeClosure` references (left dead after their only
/// use was inlined), renumbering the survivors' [`FnId`]s.
pub(crate) fn prune_dead_fns(def: LoweredDef) -> LoweredDef {
    if def.fns.len() <= 1 {
        return def;
    }
    // Mark fns reachable from the entry through `MakeClosure` references.
    let mut live = vec![false; def.fns.len()];
    live[0] = true;
    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..def.fns.len() {
            if !live[i] {
                continue;
            }
            collect_make_closures(&def.fns[i].body, &mut |fid| {
                if !live[fid.index()] {
                    live[fid.index()] = true;
                    changed = true;
                }
            });
        }
    }
    if live.iter().all(|&b| b) {
        return def;
    }
    // Renumber: old FnId -> new FnId among the survivors (entry stays index 0).
    let mut remap: FxHashMap<u32, u32> = FxHashMap::default();
    let mut next = 0u32;
    for (i, &keep) in live.iter().enumerate() {
        if keep {
            remap.insert(i as u32, next);
            next += 1;
        }
    }
    let fns: Vec<CoreFn> = def
        .fns
        .into_iter()
        .enumerate()
        .filter(|(i, _)| live[*i])
        .map(|(_, f)| CoreFn {
            params: f.params,
            captures: f.captures,
            body: renumber_fns(&f.body, &remap),
        })
        .collect();
    LoweredDef {
        def: def.def,
        fns,
        entry_borrowed: def.entry_borrowed,
        reuse_entry: def.reuse_entry,
        entry_spread_params: def.entry_spread_params,
    }
}

/// Invokes `f` on every `MakeClosure` function id in `e`.
fn collect_make_closures(e: &CExpr, f: &mut impl FnMut(FnId)) {
    walk_pre(e, &mut |n| {
        if let K::MakeClosure { func, .. } = &n.kind {
            f(*func);
        }
    });
}

/// Rewrites every `MakeClosure` function id in `e` through `remap`.
fn renumber_fns(e: &CExpr, remap: &FxHashMap<u32, u32>) -> CExpr {
    let ty = e.ty.clone();
    let go = |c: &CExpr| renumber_fns(c, remap);
    let kind = match &e.kind {
        K::MakeClosure { func, captures, alloc } => K::MakeClosure {
            func: FnId(*remap.get(&func.0).unwrap_or(&func.0)),
            captures: captures.clone(),
            alloc: *alloc,
        },
        K::Lit(_) | K::Local(_) | K::Global(_) | K::Error => e.kind.clone(),
        K::Prim { op, args } => K::Prim { op: *op, args: args.iter().map(go).collect() },
        K::Foreign { symbol, args, marshalled } => K::Foreign {
            symbol: *symbol,
            args: args.iter().map(go).collect(),
            marshalled: *marshalled,
        },
        K::MakeData { tag, args, reuse, scalars, niche } => K::MakeData {
            tag: *tag,
            args: args.iter().map(go).collect(),
            reuse: *reuse,
            scalars: *scalars,
            niche: *niche,
        },
        K::App { func, args, reuse, alloc } => K::App {
            func: Box::new(go(func)),
            args: args.iter().map(go).collect(),
            reuse: reuse.clone(),
            alloc: *alloc,
        },
        K::If { cond, then, els } => {
            K::If { cond: Box::new(go(cond)), then: Box::new(go(then)), els: Box::new(go(els)) }
        }
        K::Let { local, value, body } => {
            K::Let { local: *local, value: Box::new(go(value)), body: Box::new(go(body)) }
        }
        K::Spread { components } => K::Spread { components: components.iter().map(go).collect() },
        K::LetMany { locals, value, body } => K::LetMany {
            locals: locals.clone(),
            value: Box::new(go(value)),
            body: Box::new(go(body)),
        },
        K::DataTag { base, niche } => K::DataTag { base: Box::new(go(base)), niche: *niche },
        K::DataField { base, index, scalar, niche } => {
            K::DataField { base: Box::new(go(base)), index: *index, scalar: *scalar, niche: *niche }
        }
        K::Reset { value, token, body } => {
            K::Reset { value: Box::new(go(value)), token: *token, body: Box::new(go(body)) }
        }
        K::FreeReuse { token, body } => K::FreeReuse { token: *token, body: Box::new(go(body)) },
        K::Dup { local, body } => K::Dup { local: *local, body: Box::new(go(body)) },
        K::Drop { local, body } => K::Drop { local: *local, body: Box::new(go(body)) },
        K::Join { params, body } => K::Join { params: params.clone(), body: Box::new(go(body)) },
        K::Recur { args } => K::Recur { args: args.iter().map(go).collect() },
        K::HoleStart { hole, body } => K::HoleStart { hole: *hole, body: Box::new(go(body)) },
        K::HoleFill { hole, cell, field } => {
            K::HoleFill { hole: *hole, cell: Box::new(go(cell)), field: *field }
        }
        K::HoleClose { hole, base } => K::HoleClose { hole: *hole, base: Box::new(go(base)) },
    };
    CExpr::new(kind, ty)
}

// ---------------------------------------------------------------------------
// IR builders.
// ---------------------------------------------------------------------------

fn int_lit(n: i64) -> CExpr {
    CExpr::new(K::Lit(Lit::Int(n)), Ty::int())
}

fn true_lit() -> CExpr {
    CExpr::new(K::Lit(Lit::Bool(true)), Ty::bool())
}

fn false_lit() -> CExpr {
    CExpr::new(K::Lit(Lit::Bool(false)), Ty::bool())
}

fn local(l: LocalId, ty: Ty) -> CExpr {
    CExpr::new(K::Local(l), ty)
}

fn global(def: DefId) -> CExpr {
    CExpr::new(K::Global(def), Ty::Error)
}

/// An integer/comparison primitive (the result type follows the operator).
fn prim(op: Prim, args: Vec<CExpr>) -> CExpr {
    let ty = match op {
        Prim::IntLt | Prim::IntLe | Prim::IntGt | Prim::IntGe | Prim::Eq => Ty::bool(),
        _ => Ty::int(),
    };
    CExpr::new(K::Prim { op, args }, ty)
}

fn if_(cond: CExpr, then: CExpr, els: CExpr, ty: Ty) -> CExpr {
    CExpr::new(K::If { cond: Box::new(cond), then: Box::new(then), els: Box::new(els) }, ty)
}

fn data_field(base: CExpr, index: u32, ty: Ty) -> CExpr {
    CExpr::new(
        K::DataField {
            base: Box::new(base),
            index: crate::ir::FieldIndex::Const(index),
            scalar: false,
            niche: None,
        },
        ty,
    )
}

#[cfg(test)]
mod tests {
    //! IR-shape tests: assert *which* pipelines fuse, by inspecting the rewritten
    //! body. A synthesized loop renders as a call to a `@fuse#…` global; an
    //! unrolled literal leaves no combinator call; an unfused chain keeps its
    //! `@map`/`@foldl`/… combinator calls. One focused `#[test]` per decision.

    use fai_db::{Db, FaiDatabase};
    use fai_syntax::Symbol;

    use crate::{fuse_def, pretty_def};

    /// The pretty-printed fused body of `name` in a module `src` (std loaded).
    fn fused(src: &str, name: &str) -> String {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), src.to_owned());
        let file = db.source_file(id).expect("source registered");
        pretty_def(&fuse_def(&db, file, Symbol::intern(name)).body)
    }

    #[test]
    fn array_map_sum_over_range_fuses_to_a_loop() {
        let src = "module M\n\npublic run : Int -> Int\nlet run n = Array.sum (Array.map (fun x -> x * 2) (Array.range 0 n))\n";
        let body = fused(src, "run");
        assert!(body.contains("@fuse#run#0"), "should call a synthesized loop:\n{body}");
        assert!(
            !body.contains("@map") && !body.contains("@sum") && !body.contains("@range"),
            "the combinators are fused away:\n{body}"
        );
    }

    #[test]
    fn list_foldl_over_range_fuses_to_a_loop() {
        let src = "module M\n\npublic run : Int -> Int\nlet run n = List.foldl (fun acc x -> acc + x) 0 (List.range 0 n)\n";
        let body = fused(src, "run");
        assert!(body.contains("@fuse#run#0"), "should call a synthesized loop:\n{body}");
        assert!(!body.contains("@range"), "the list spine is fused away:\n{body}");
    }

    #[test]
    fn shared_value_source_keeps_the_value_materialized() {
        // `xs` is shared, so it stays built (`@range`), while each `Array.sum` over
        // it fuses (`@fuse#…`). The map-output buffer is gone (no second `@map`).
        let src = "module M\n\npublic run : Int -> Int\nlet run n =\n  let xs = Array.range 0 n\n  Array.sum (Array.map (fun x -> x * 2) xs) + Array.sum xs\n";
        let body = fused(src, "run");
        assert!(body.contains("@range"), "the shared value stays materialized:\n{body}");
        assert!(body.contains("@fuse#"), "the adjacent map->sum fuses:\n{body}");
        assert!(!body.contains("@map"), "the map-output buffer is gone:\n{body}");
    }

    #[test]
    fn effectful_stage_is_a_barrier() {
        // The fold step calls `f`, whose effect row is open (`'e`), so the chain is
        // not pure and is left unfused: the combinators remain.
        let src = "module M\n\npublic process : (Int -> Int / 'e) -> List Int -> Int / 'e\nlet process f xs = List.foldl (fun acc x -> acc + f x) 0 (List.map f xs)\n";
        let body = fused(src, "process");
        assert!(body.contains("@foldl"), "an effectful chain is not fused:\n{body}");
        assert!(!body.contains("@fuse#"), "no loop synthesized for an effectful chain:\n{body}");
    }

    #[test]
    fn small_list_literal_unrolls() {
        let src = "module M\n\npublic run : Int\nlet run = List.sum (List.map (fun x -> x + 1) [10, 20, 30])\n";
        let body = fused(src, "run");
        // Unrolled: no loop, and the combinators and the cons-literal are gone.
        assert!(!body.contains("@fuse#"), "a small literal unrolls (no loop):\n{body}");
        assert!(!body.contains("@map") && !body.contains("@sum"), "combinators gone:\n{body}");
    }

    #[test]
    fn nonpipeline_definition_is_unchanged() {
        let src = "module M\n\npublic run : Int -> Int\nlet run n = n + 1\n";
        let body = fused(src, "run");
        assert!(!body.contains("@fuse#"), "no pipeline, no loop:\n{body}");
    }
}

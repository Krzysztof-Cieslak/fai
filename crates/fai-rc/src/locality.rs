//! Read-only, bounded locality diagnostics for the final ownership IR.
//!
//! A proven root has a fresh logical lifetime and has not reached an unknown call
//! or publication boundary. Its descendants need not be local. These reports are
//! not consumed by code generation: they neither change RC nor extend lifetimes.

use std::collections::{BTreeMap, BTreeSet};

use fai_core::ir::{CExpr, ClosureAlloc, CoreFn, ExprKind as K, Lit, LoweredDef, Prim};
use fai_resolve::LocalId;
use rustc_hash::FxHashMap;

mod program;
pub use program::{CallableLocality, ProgramLocality, ReturnProvenance, analyze_program_locality};

const NODE_BUDGET: usize = 8192;
const ORIGIN_BUDGET: usize = 128;
const DEPTH_BUDGET: usize = 128;

/// The observed operation; construction reports are separate from count updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalityOperation {
    /// Explicit ownership acquisition.
    Dup,
    /// Explicit ownership release.
    Drop,
    /// Death-point release into a reuse token.
    Reset,
    /// Construction without a reuse token.
    FreshData,
    /// Construction into a checked token or its fresh-allocation fallback.
    ReusedData,
    /// Closure construction (including static and stack forms).
    Closure,
}

/// Proof outcome, or the first conservative barrier explaining its absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LocalityReason {
    /// Every possible root is a known, still-unpublished allocation lifetime.
    ProvenRoot,
    /// A literal immediate has no reference count.
    Immediate,
    /// An immortal literal/static closure has no mutable reference count.
    Immortal,
    /// Entry parameters and captures have unknown publication histories.
    Input,
    /// A top-level value may run an initializer with an unknown result.
    Global,
    /// An ordinary call may retain, alias or publish reachable values.
    Call,
    /// A native call has no locality contract.
    Foreign,
    /// A known native publication boundary marks the reachable graph.
    Published,
    /// A primitive has no non-publishing reader contract in this diagnostic.
    Primitive,
    /// Field identity/representation is not reconstructed by this first analysis.
    Projection,
    /// Niche/scalar transport does not establish a new physical root.
    Representation,
    /// Branches disagree about root representation or provenance.
    Merge,
    /// An incoming loop value may represent an earlier allocation instance.
    Loop,
    /// Destination-passing construction is outside this analysis's proof domain.
    Destination,
    /// A reset ends the old allocation lifetime; its token is not a live root.
    ResetLifetime,
    /// A nested result shape was abstracted while retaining its possible aliases.
    SummaryDepth,
    /// Bounded proof work was exhausted. All following facts are conservative.
    Limit,
}

/// One operation in a function, identified by position-independent IR preorder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalitySite {
    /// Expression preorder within this function, including non-reportable nodes.
    pub node: usize,
    /// What is being classified.
    pub operation: LocalityOperation,
    /// The referenced local, for explicit RC nodes.
    pub local: Option<LocalId>,
    /// Construction width; not a claim of backend fast-path eligibility.
    pub fields: Option<usize>,
    /// Root-locality outcome, not descendant locality or uniqueness.
    pub reason: LocalityReason,
}

/// Diagnostics for an ordinary/lifted function or a specialized reuse entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionLocality {
    /// Index into `LoweredDef::fns`; `None` identifies the specialized reuse entry.
    pub function: Option<usize>,
    /// All explicit RC/construction sites, ordered by IR preorder.
    pub sites: Vec<LocalitySite>,
    /// Number of expressions visited, including the conservative fallback walk.
    pub nodes: usize,
    /// Whether any work bound forced conservative fallback.
    pub limited: bool,
}

/// An immutable diagnostic result. It is deliberately separate from `LoweredDef`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalityReport {
    /// Ordinary/lifted functions, followed by the optional reuse entry.
    pub functions: Vec<FunctionLocality>,
}

/// Analyze already reference-counted IR without changing it or invoking queries.
/// Unknown calls invalidate every known root reachable through their operands,
/// including function captures. Loop inputs and opaque projections are conservative.
#[must_use]
pub fn analyze_locality(def: &LoweredDef) -> LocalityReport {
    let mut functions: Vec<_> =
        def.fns.iter().enumerate().map(|(i, f)| analyze(f, Some(i))).collect();
    if let Some(entry) = &def.reuse_entry {
        functions.push(analyze(entry, None));
    }
    LocalityReport { functions }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fact {
    roots: BTreeSet<usize>,
    reachable: BTreeSet<usize>,
    reason: Option<LocalityReason>,
}

impl Fact {
    fn unknown(reason: LocalityReason) -> Self {
        Self { roots: BTreeSet::new(), reachable: BTreeSet::new(), reason: Some(reason) }
    }

    fn opaque(reason: LocalityReason, values: &[Self]) -> Self {
        let mut fact = Self::unknown(reason);
        for value in values {
            fact.reachable.extend(&value.reachable);
        }
        fact
    }

    fn merge(mut self, other: Self) -> Self {
        self.roots.extend(other.roots);
        self.reachable.extend(other.reachable);
        if self.reason != other.reason {
            self.reason = Some(LocalityReason::Merge);
        }
        self
    }
}

enum Value {
    One(Fact),
    Many(Vec<Fact>),
    Unreachable,
}

impl Value {
    fn one(self) -> Fact {
        match self {
            Self::One(fact) => fact,
            Self::Many(values) => Fact::opaque(LocalityReason::Representation, &values),
            Self::Unreachable => Fact::unknown(LocalityReason::Representation),
        }
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unreachable, other) | (other, Self::Unreachable) => other,
            (Self::Many(a), Self::Many(b)) if a.len() == b.len() => {
                Self::Many(a.into_iter().zip(b).map(|(a, b)| a.merge(b)).collect())
            }
            (a, b) => Self::One(a.one().merge(b.one())),
        }
    }
}

#[derive(Clone, Default)]
struct State {
    locals: FxHashMap<LocalId, Fact>,
    origins: BTreeMap<usize, Option<LocalityReason>>,
}

impl State {
    fn local(&self, local: LocalId) -> Fact {
        self.locals.get(&local).cloned().unwrap_or_else(|| Fact::unknown(LocalityReason::Input))
    }

    fn invalidate(&mut self, fact: &Fact, reason: LocalityReason) {
        for root in &fact.reachable {
            if let Some(value) = self.origins.get_mut(root) {
                value.get_or_insert(reason);
            }
        }
    }

    fn barrier(&mut self, reason: LocalityReason) {
        for value in self.origins.values_mut() {
            value.get_or_insert(reason);
        }
    }

    fn classify(&self, fact: &Fact) -> LocalityReason {
        if let Some(reason) = fact.reason {
            return reason;
        }
        for root in &fact.roots {
            match self.origins.get(root) {
                Some(None) => {}
                Some(Some(reason)) => return *reason,
                None => return LocalityReason::Limit,
            }
        }
        if fact.roots.is_empty() {
            LocalityReason::Representation
        } else {
            LocalityReason::ProvenRoot
        }
    }

    fn join(&mut self, other: Self) {
        for (local, fact) in other.locals {
            if let Some(current) = self.locals.get_mut(&local) {
                *current = current.clone().merge(fact);
            }
        }
        for (root, reason) in other.origins {
            self.origins
                .entry(root)
                .and_modify(|old| {
                    if old.is_none() {
                        *old = reason;
                    }
                })
                .or_insert(reason);
        }
    }
}

struct Analyzer {
    sites: Vec<LocalitySite>,
    nodes: usize,
    origins: usize,
    limited: bool,
}

fn analyze(function: &CoreFn, index: Option<usize>) -> FunctionLocality {
    let mut cx = Analyzer { sites: Vec::new(), nodes: 0, origins: 0, limited: false };
    cx.eval(&function.body, &mut State::default(), 0);
    cx.sites.sort_by_key(|site| site.node);
    FunctionLocality { function: index, sites: cx.sites, nodes: cx.nodes, limited: cx.limited }
}

impl Analyzer {
    fn site(&mut self, node: usize, e: &CExpr, reason: LocalityReason) {
        let (operation, local, fields) = match &e.kind {
            K::Dup { local, .. } => (LocalityOperation::Dup, Some(*local), None),
            K::Drop { local, .. } => (LocalityOperation::Drop, Some(*local), None),
            K::Reset { value, .. } => (
                LocalityOperation::Reset,
                match value.kind {
                    K::Local(id) => Some(id),
                    _ => None,
                },
                None,
            ),
            K::MakeData { args, reuse, .. } => (
                if reuse.is_some() {
                    LocalityOperation::ReusedData
                } else {
                    LocalityOperation::FreshData
                },
                None,
                Some(args.len()),
            ),
            K::MakeClosure { captures, .. } => {
                (LocalityOperation::Closure, None, Some(captures.len()))
            }
            _ => return,
        };
        self.sites.push(LocalitySite { node, operation, local, fields, reason });
    }

    fn fresh(&mut self, node: usize, fields: &[Fact], state: &mut State) -> Fact {
        if self.limited || self.origins >= ORIGIN_BUDGET {
            self.limited = true;
            state.barrier(LocalityReason::Limit);
            return Fact::opaque(LocalityReason::Limit, fields);
        }
        self.origins += 1;
        state.origins.insert(node, None);
        let mut fact = Fact::opaque(LocalityReason::Representation, fields);
        fact.reason = None;
        fact.roots.insert(node);
        fact.reachable.insert(node);
        fact
    }

    fn fallback(&mut self, e: &CExpr) {
        let mut pending = vec![e];
        while let Some(e) = pending.pop() {
            let node = self.nodes;
            self.nodes += 1;
            self.site(node, e, LocalityReason::Limit);
            pending.extend(children(e).into_iter().rev());
        }
    }

    fn eval(&mut self, e: &CExpr, state: &mut State, depth: usize) -> Value {
        if self.limited || self.nodes >= NODE_BUDGET || depth >= DEPTH_BUDGET {
            self.limited = true;
            state.barrier(LocalityReason::Limit);
            self.fallback(e);
            return Value::One(Fact::unknown(LocalityReason::Limit));
        }
        let node = self.nodes;
        self.nodes += 1;
        let sub = depth + 1;
        match &e.kind {
            K::Local(local) => Value::One(state.local(*local)),
            K::Lit(Lit::Str(_)) => Value::One(Fact::unknown(LocalityReason::Immortal)),
            K::Lit(Lit::Unit | Lit::Bool(_) | Lit::Char(_)) => {
                Value::One(Fact::unknown(LocalityReason::Immediate))
            }
            K::Lit(Lit::Int(value)) if (-(1i64 << 62)..(1i64 << 62)).contains(value) => {
                Value::One(Fact::unknown(LocalityReason::Immediate))
            }
            K::Lit(_) => Value::One(Fact::unknown(LocalityReason::Representation)),
            K::Global(_) => Value::One(Fact::unknown(LocalityReason::Global)),
            K::Error => Value::Unreachable,
            K::Let { local, value, body } => {
                let value = self.eval(value, state, sub).one();
                let old = state.locals.insert(*local, value);
                let result = self.eval(body, state, sub);
                if let Some(old) = old {
                    state.locals.insert(*local, old);
                } else {
                    state.locals.remove(local);
                }
                result
            }
            K::LetMany { locals, value, body } => {
                let value = self.eval(value, state, sub);
                let values = match value {
                    Value::Many(values) if values.len() == locals.len() => values,
                    other => vec![
                        Fact::opaque(LocalityReason::Representation, &[other.one()]);
                        locals.len()
                    ],
                };
                let old: Vec<_> = locals
                    .iter()
                    .zip(values)
                    .map(|(local, value)| (*local, state.locals.insert(*local, value)))
                    .collect();
                let result = self.eval(body, state, sub);
                for (local, old) in old {
                    if let Some(old) = old {
                        state.locals.insert(local, old);
                    } else {
                        state.locals.remove(&local);
                    }
                }
                result
            }
            K::Spread { components } => {
                Value::Many(components.iter().map(|e| self.eval(e, state, sub).one()).collect())
            }
            K::If { cond, then, els } => {
                self.eval(cond, state, sub);
                let mut left = state.clone();
                let a = self.eval(then, &mut left, sub);
                let b = self.eval(els, state, sub);
                match (&a, &b) {
                    (Value::Unreachable, _) => {}
                    (_, Value::Unreachable) => *state = left,
                    _ => state.join(left),
                }
                a.merge(b)
            }
            K::MakeData { args, niche, .. } => {
                let values: Vec<_> = args.iter().map(|e| self.eval(e, state, sub).one()).collect();
                let fact = if niche.is_some() {
                    Fact::opaque(LocalityReason::Representation, &values)
                } else if args.is_empty() {
                    Fact::unknown(LocalityReason::Immediate)
                } else {
                    self.fresh(node, &values, state)
                };
                self.site(node, e, state.classify(&fact));
                Value::One(fact)
            }
            K::MakeClosure { captures, alloc, .. } => {
                let values: Vec<_> = captures.iter().map(|local| state.local(*local)).collect();
                let fact = match alloc {
                    ClosureAlloc::Heap if !captures.is_empty() => self.fresh(node, &values, state),
                    ClosureAlloc::Heap => Fact::unknown(LocalityReason::Representation),
                    ClosureAlloc::Static => Fact::unknown(LocalityReason::Immortal),
                    ClosureAlloc::Stack => Fact::opaque(LocalityReason::Representation, &values),
                };
                self.site(node, e, state.classify(&fact));
                Value::One(fact)
            }
            K::Dup { local, body } | K::Drop { local, body } => {
                self.site(node, e, state.classify(&state.local(*local)));
                self.eval(body, state, sub)
            }
            K::Reset { value, token, body } => {
                let value = self.eval(value, state, sub).one();
                self.site(node, e, state.classify(&value));
                state.invalidate(&value, LocalityReason::ResetLifetime);
                state.locals.insert(*token, Fact::unknown(LocalityReason::ResetLifetime));
                self.eval(body, state, sub)
            }
            K::FreeReuse { body, .. } => self.eval(body, state, sub),
            K::DataTag { base, .. } => {
                self.eval(base, state, sub);
                Value::One(Fact::unknown(LocalityReason::Representation))
            }
            K::DataField { base, .. } => {
                let base = self.eval(base, state, sub).one();
                Value::One(Fact::opaque(LocalityReason::Projection, &[base]))
            }
            K::Prim { op, args } => {
                let values: Vec<_> =
                    args.iter().map(|arg| self.eval(arg, state, sub).one()).collect();
                if reader(*op) {
                    // These primitives return scalars, never an alias or a
                    // container retaining an input graph.
                    return Value::One(Fact::unknown(LocalityReason::Primitive));
                }
                for value in &values {
                    state.invalidate(value, LocalityReason::Primitive);
                }
                Value::One(Fact::opaque(LocalityReason::Primitive, &values))
            }
            K::App { func, args, .. } => {
                let mut values = vec![self.eval(func, state, sub).one()];
                values.extend(args.iter().map(|arg| self.eval(arg, state, sub).one()));
                for value in &values {
                    state.invalidate(value, LocalityReason::Call);
                }
                Value::One(Fact::opaque(LocalityReason::Call, &values))
            }
            K::Foreign { symbol, args, .. } => {
                let reason =
                    if matches!(symbol.as_str(), "fai_send" | "fai_spawn" | "fai_mark_shared") {
                        LocalityReason::Published
                    } else {
                        LocalityReason::Foreign
                    };
                let values: Vec<_> =
                    args.iter().map(|arg| self.eval(arg, state, sub).one()).collect();
                for value in &values {
                    state.invalidate(value, reason);
                }
                Value::One(Fact::opaque(reason, &values))
            }
            K::Join { params, body } => {
                state.barrier(LocalityReason::Loop);
                for param in params {
                    let value = Fact::opaque(LocalityReason::Loop, &[state.local(*param)]);
                    state.locals.insert(*param, value);
                }
                self.eval(body, state, sub)
            }
            K::Recur { args } => {
                for arg in args {
                    let value = self.eval(arg, state, sub).one();
                    state.invalidate(&value, LocalityReason::Loop);
                }
                Value::Unreachable
            }
            K::HoleStart { body, .. } => {
                state.barrier(LocalityReason::Destination);
                self.eval(body, state, sub)
            }
            K::HoleFill { cell, .. } | K::HoleClose { base: cell, .. } => {
                self.eval(cell, state, sub);
                state.barrier(LocalityReason::Destination);
                Value::One(Fact::unknown(LocalityReason::Destination))
            }
        }
    }
}

fn reader(op: Prim) -> bool {
    matches!(
        op,
        Prim::IntAdd
            | Prim::IntSub
            | Prim::IntMul
            | Prim::IntDiv
            | Prim::IntRem
            | Prim::Eq
            | Prim::Compare
            | Prim::Hash
            | Prim::BytesLength
            | Prim::BytesGet
            | Prim::BytesIsUtf8
            | Prim::StringLength
            | Prim::ArrayLength
    )
}

fn children(e: &CExpr) -> Vec<&CExpr> {
    match &e.kind {
        K::Prim { args, .. }
        | K::Foreign { args, .. }
        | K::MakeData { args, .. }
        | K::Recur { args }
        | K::Spread { components: args } => args.iter().collect(),
        K::App { func, args, .. } => std::iter::once(func.as_ref()).chain(args.iter()).collect(),
        K::If { cond, then, els } => vec![cond, then, els],
        K::Let { value, body, .. }
        | K::LetMany { value, body, .. }
        | K::Reset { value, body, .. } => vec![value, body],
        K::FreeReuse { body, .. }
        | K::Dup { body, .. }
        | K::Drop { body, .. }
        | K::Join { body, .. }
        | K::HoleStart { body, .. } => vec![body],
        K::DataTag { base, .. } | K::DataField { base, .. } | K::HoleClose { base, .. } => {
            vec![base]
        }
        K::HoleFill { cell, .. } => vec![cell],
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => Vec::new(),
    }
}

#[cfg(test)]
mod tests;

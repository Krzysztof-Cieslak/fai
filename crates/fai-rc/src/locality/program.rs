//! Bounded call summaries and field provenance for diagnostic locality analysis.
//!
//! Summaries start at the conservative top (all inputs may publish, unknown
//! result). Each round uses only previously sound summaries. No optimistic
//! recursive assumption or caller-specific entry fact becomes a proof.

use std::collections::{BTreeMap, BTreeSet};

use fai_core::ir::{CExpr, ClosureAlloc, CoreFn, ExprKind as K, FieldIndex, Lit, LoweredDef};
use fai_resolve::{DefId, LocalId};
use fai_types::{Con, Ty};
use rustc_hash::FxHashMap;

use super::{
    FunctionLocality, LocalityOperation as Op, LocalityReason as Why, LocalityReport, LocalitySite,
};

const ROUNDS: usize = 12;
const ROOTS: usize = 256;
const PATH: usize = 8;
const RESULT_DEPTH: usize = 3;

/// The origin visible at a callable's result boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReturnProvenance {
    /// A new unpublished root (its fields may still refer to shared values).
    Fresh,
    /// An unchanged input, indexed with captures before ordinary parameters.
    Input(usize),
    /// A constant-index field path from an input.
    Field { input: usize, path: Vec<u32> },
    /// Several alternatives or exploded return components.
    Mixed,
    /// No useful root identity is established.
    Unknown,
}

/// An inferred diagnostic contract. It is not used for ownership or code emission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallableLocality {
    /// Index in the supplied definition slice.
    pub definition: usize,
    /// Lifted-function index, or `None` for its specialized reuse entry.
    pub function: Option<usize>,
    /// Number of leading captured inputs in the publication vector.
    pub captures: usize,
    /// Whether each capture/parameter may reach an unknown or publishing action.
    pub may_publish: Vec<bool>,
    /// A coarse display of the internally retained result graph.
    pub result: ReturnProvenance,
    /// Whether proof work exhausted a per-function bound.
    pub limited: bool,
}

/// Read-only whole-program diagnostic results, in input definition order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramLocality {
    /// Per-site proofs using the inferred call summaries.
    pub definitions: Vec<LocalityReport>,
    /// Per-callable publication and result-origin contracts.
    pub summaries: Vec<CallableLocality>,
    /// Completed conservative propagation rounds.
    pub rounds: usize,
    /// Whether all summaries stabilized within the propagation bound.
    pub converged: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key(usize, Option<usize>);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Fact {
    roots: BTreeSet<usize>,
    reachable: BTreeSet<usize>,
    reason: Option<Why>,
    callable: Option<Key>,
}

impl Fact {
    fn unknown(reason: Why) -> Self {
        Self {
            roots: BTreeSet::new(),
            reachable: BTreeSet::new(),
            reason: Some(reason),
            callable: None,
        }
    }

    fn opaque(reason: Why, values: &[Self]) -> Self {
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
            self.reason = Some(Why::Merge);
        }
        if self.callable != other.callable {
            self.callable = None;
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    One(Fact),
    Many(Vec<Fact>),
    Never,
}

impl Value {
    fn one(self) -> Fact {
        match self {
            Self::One(fact) => fact,
            Self::Many(facts) => Fact::opaque(Why::Representation, &facts),
            Self::Never => Fact::unknown(Why::Representation),
        }
    }

    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Never, other) | (other, Self::Never) => other,
            (Self::Many(a), Self::Many(b)) if a.len() == b.len() => {
                Self::Many(a.into_iter().zip(b).map(|(a, b)| a.merge(b)).collect())
            }
            (a, b) => Self::One(a.one().merge(b.one())),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InputPath {
    input: usize,
    fields: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Root {
    input: Option<InputPath>,
    reason: Option<Why>,
    // Only representation-preserving fields are retained. Unknown fields still
    // keep their possible reachability, so publishing a projection stays safe.
    fields: Option<Vec<Fact>>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct State {
    locals: FxHashMap<LocalId, Fact>,
    roots: BTreeMap<usize, Root>,
}

impl State {
    fn local(&self, local: LocalId) -> Fact {
        self.locals.get(&local).cloned().unwrap_or_else(|| Fact::unknown(Why::Input))
    }

    fn reason(&self, fact: &Fact) -> Why {
        if let Some(reason) = fact.reason {
            return reason;
        }
        if fact.roots.is_empty() {
            return Why::Representation;
        }
        for id in &fact.roots {
            let Some(root) = self.roots.get(id) else {
                return Why::Limit;
            };
            if let Some(reason) = root.reason {
                return reason;
            }
            if root.input.is_some() {
                return Why::Input;
            }
        }
        Why::ProvenRoot
    }

    fn join(&mut self, other: Self) {
        for (local, fact) in other.locals {
            if let Some(old) = self.locals.get_mut(&local) {
                *old = old.clone().merge(fact);
            }
        }
        for (id, root) in other.roots {
            self.roots
                .entry(id)
                .and_modify(|old| {
                    if old.reason.is_none() {
                        old.reason = root.reason;
                    }
                })
                .or_insert(root);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Summary {
    publish: Vec<bool>,
    result: Value,
    roots: BTreeMap<usize, Root>,
    limited: bool,
}

impl Summary {
    fn unknown(inputs: usize) -> Self {
        Self {
            publish: vec![true; inputs],
            result: Value::One(Fact::unknown(Why::Call)),
            roots: BTreeMap::new(),
            limited: false,
        }
    }

    fn provenance(&self) -> ReturnProvenance {
        let Value::One(fact) = &self.result else {
            return ReturnProvenance::Mixed;
        };
        if fact.reason.is_some() {
            return ReturnProvenance::Unknown;
        }
        if fact.roots.len() != 1 {
            return ReturnProvenance::Mixed;
        }
        let Some(root) = self.roots.get(fact.roots.first().expect("one root")) else {
            return ReturnProvenance::Unknown;
        };
        if root.reason.is_some() {
            return ReturnProvenance::Unknown;
        }
        match &root.input {
            None => ReturnProvenance::Fresh,
            Some(path) if path.fields.is_empty() => ReturnProvenance::Input(path.input),
            Some(path) => ReturnProvenance::Field { input: path.input, path: path.fields.clone() },
        }
    }
}

/// Retain bounded result shapes, including every input dependency and aliases
/// to any exposed root. Hidden fresh descendants cannot be named by the caller;
/// an abstracted field still reaches any sibling/input that it might publish.
fn compact_result(result: Value, roots: BTreeMap<usize, Root>) -> (Value, BTreeMap<usize, Root>) {
    let values: Vec<_> = match &result {
        Value::One(value) => vec![value],
        Value::Many(values) => values.iter().collect(),
        Value::Never => Vec::new(),
    };
    let mut keep = BTreeSet::new();
    let mut pending = Vec::new();
    for value in values {
        for id in &value.reachable {
            if roots.get(id).is_some_and(|root| root.input.is_some()) {
                keep.insert(*id);
            }
        }
        pending.extend(value.roots.iter().map(|id| (*id, 0)));
    }
    let mut depths = BTreeMap::new();
    while let Some((id, depth)) = pending.pop() {
        if depth >= RESULT_DEPTH || depths.get(&id).is_some_and(|old| *old <= depth) {
            continue;
        }
        depths.insert(id, depth);
        keep.insert(id);
        if let Some(fields) = roots.get(&id).and_then(|root| root.fields.as_ref()) {
            for value in fields {
                pending.extend(value.roots.iter().map(|id| (*id, depth + 1)));
            }
        }
    }
    let ids: BTreeMap<_, _> = keep.iter().enumerate().map(|(next, old)| (*old, next)).collect();
    let convert = |value: &Fact| {
        let mut converted = value.clone();
        if value.roots.iter().any(|id| !keep.contains(id)) {
            converted.roots.clear();
            converted.reason = Some(Why::SummaryDepth);
            converted.callable = None;
        } else {
            converted.roots = value.roots.iter().map(|id| ids[id]).collect();
        }
        converted.reachable =
            value.reachable.iter().filter_map(|id| ids.get(id).copied()).collect();
        converted
    };
    let compact = keep
        .iter()
        .filter_map(|id| {
            roots.get(id).map(|root| {
                let mut root = root.clone();
                root.fields =
                    root.fields.as_ref().map(|fields| fields.iter().map(&convert).collect());
                (ids[id], root)
            })
        })
        .collect();
    let result = match result {
        Value::One(value) => Value::One(convert(&value)),
        Value::Many(values) => Value::Many(values.iter().map(convert).collect()),
        Value::Never => Value::Never,
    };
    (result, compact)
}

struct Program<'a> {
    defs: &'a [LoweredDef],
    ids: FxHashMap<DefId, usize>,
    keys: Vec<Key>,
}

impl<'a> Program<'a> {
    fn function(&self, key: Key) -> &CoreFn {
        match key.1 {
            Some(index) => &self.defs[key.0].fns[index],
            None => self.defs[key.0].reuse_entry.as_ref().expect("registered reuse entry"),
        }
    }
}

/// Infer sound bounded summaries, then report root locality at explicit RC sites.
/// Incoming roots remain unknown: no observed caller lends an entry-wide proof.
#[must_use]
pub fn analyze_program_locality(defs: &[LoweredDef]) -> ProgramLocality {
    let mut program = Program { defs, ids: FxHashMap::default(), keys: Vec::new() };
    for (i, def) in defs.iter().enumerate() {
        program.ids.insert(def.def, i);
        program.keys.extend((0..def.fns.len()).map(|f| Key(i, Some(f))));
        if def.reuse_entry.is_some() {
            program.keys.push(Key(i, None));
        }
    }
    let mut summaries: FxHashMap<_, _> = program
        .keys
        .iter()
        .map(|&key| {
            let f = program.function(key);
            (key, Summary::unknown(f.captures.len() + f.params.len()))
        })
        .collect();
    let mut rounds = 0;
    let mut converged = false;
    for _ in 0..ROUNDS {
        let next: FxHashMap<_, _> = program
            .keys
            .iter()
            .map(|&key| {
                let (summary, _) = analyze(&program, &summaries, key);
                (key, summary)
            })
            .collect();
        rounds += 1;
        converged = next == summaries;
        summaries = next;
        if converged {
            break;
        }
    }
    let mut definitions = vec![LocalityReport { functions: Vec::new() }; defs.len()];
    let mut contracts = Vec::new();
    for &key in &program.keys {
        let (_, report) = analyze(&program, &summaries, key);
        let summary = &summaries[&key];
        definitions[key.0].functions.push(report);
        contracts.push(CallableLocality {
            definition: key.0,
            function: key.1,
            captures: program.function(key).captures.len(),
            may_publish: summary.publish.clone(),
            result: summary.provenance(),
            limited: summary.limited,
        });
    }
    ProgramLocality { definitions, summaries: contracts, rounds, converged }
}

struct Engine<'a, 'p> {
    program: &'a Program<'p>,
    summaries: &'a FxHashMap<Key, Summary>,
    key: Key,
    sites: Vec<LocalitySite>,
    nodes: usize,
    next_root: usize,
    publish: Vec<bool>,
    limited: bool,
}

fn analyze(
    program: &Program<'_>,
    summaries: &FxHashMap<Key, Summary>,
    key: Key,
) -> (Summary, FunctionLocality) {
    let f = program.function(key);
    let mut engine = Engine {
        program,
        summaries,
        key,
        sites: Vec::new(),
        nodes: 0,
        next_root: 0,
        publish: vec![false; f.captures.len() + f.params.len()],
        limited: false,
    };
    let mut state = State::default();
    for (input, local) in f.captures.iter().chain(&f.params).enumerate() {
        let fact =
            engine.root(&mut state, Some(InputPath { input, fields: Vec::new() }), None, None);
        state.locals.insert(*local, fact);
    }
    let result = engine.eval(&f.body, &mut state, 0);
    engine.sites.sort_by_key(|s| s.node);
    let summary = if engine.limited {
        let mut summary = Summary::unknown(engine.publish.len());
        summary.limited = true;
        summary
    } else {
        let (result, roots) = compact_result(result, state.roots);
        Summary { publish: engine.publish, result, roots, limited: false }
    };
    (
        summary,
        FunctionLocality {
            function: key.1,
            sites: engine.sites,
            nodes: engine.nodes,
            limited: engine.limited,
        },
    )
}

impl Engine<'_, '_> {
    fn root(
        &mut self,
        state: &mut State,
        input: Option<InputPath>,
        reason: Option<Why>,
        fields: Option<Vec<Fact>>,
    ) -> Fact {
        if self.limited || self.next_root >= ROOTS {
            self.limit(state);
            return Fact::opaque(Why::Limit, fields.as_deref().unwrap_or(&[]));
        }
        let id = self.next_root;
        self.next_root += 1;
        let mut fact = Fact::opaque(Why::Representation, fields.as_deref().unwrap_or(&[]));
        fact.reason = None;
        fact.roots.insert(id);
        fact.reachable.insert(id);
        state.roots.insert(id, Root { input, reason, fields });
        fact
    }

    fn limit(&mut self, state: &mut State) {
        self.limited = true;
        self.publish.fill(true);
        for root in state.roots.values_mut() {
            root.reason.get_or_insert(Why::Limit);
        }
    }

    fn invalidate(&mut self, state: &mut State, value: &Fact, reason: Why) {
        for id in &value.reachable {
            if let Some(root) = state.roots.get_mut(id) {
                root.reason.get_or_insert(reason);
                if let Some(input) = &root.input {
                    self.publish[input.input] = true;
                }
            }
        }
    }

    fn barrier(&mut self, state: &mut State, reason: Why) {
        for root in state.roots.values_mut() {
            root.reason.get_or_insert(reason);
            if let Some(input) = &root.input {
                self.publish[input.input] = true;
            }
        }
    }

    fn project(&mut self, state: &mut State, base: &Fact, index: u32) -> Fact {
        if base.reason.is_some() || base.roots.is_empty() {
            return Fact::opaque(Why::Projection, std::slice::from_ref(base));
        }
        let mut result: Option<Fact> = None;
        for id in &base.roots {
            let Some(root) = state.roots.get(id).cloned() else {
                self.limit(state);
                return Fact::unknown(Why::Limit);
            };
            let fact = if root.reason.is_some() {
                Fact::opaque(Why::Projection, std::slice::from_ref(base))
            } else if let Some(mut path) = root.input {
                if path.fields.len() >= PATH {
                    Fact::opaque(Why::Projection, std::slice::from_ref(base))
                } else {
                    path.fields.push(index);
                    let mut fact = self.root(state, Some(path), None, None);
                    // A projection may alias any reachable input node. Retaining
                    // the base's reachability makes publication conservative.
                    fact.reachable.extend(&base.reachable);
                    fact
                }
            } else {
                root.fields
                    .as_ref()
                    .and_then(|fields| fields.get(index as usize))
                    .cloned()
                    .unwrap_or_else(|| Fact::opaque(Why::Projection, std::slice::from_ref(base)))
            };
            result = Some(match result {
                None => fact,
                Some(old) => old.merge(fact),
            });
        }
        result.unwrap_or_else(|| Fact::opaque(Why::Projection, std::slice::from_ref(base)))
    }

    fn instantiate(
        &mut self,
        fact: &Fact,
        roots: &BTreeMap<usize, Root>,
        inputs: &[Fact],
        state: &mut State,
        cache: &mut BTreeMap<usize, Fact>,
        depth: usize,
    ) -> Fact {
        if depth >= 32 || self.limited {
            self.limit(state);
            return Fact::opaque(Why::Limit, inputs);
        }
        let mut mapped = BTreeMap::new();
        for id in fact.roots.union(&fact.reachable) {
            let value = if let Some(value) = cache.get(id) {
                value.clone()
            } else {
                let Some(root) = roots.get(id) else {
                    self.limit(state);
                    return Fact::opaque(Why::Limit, inputs);
                };
                let value = if let Some(path) = &root.input {
                    let mut value = inputs
                        .get(path.input)
                        .cloned()
                        .unwrap_or_else(|| Fact::unknown(Why::Input));
                    for &field in &path.fields {
                        value = self.project(state, &value, field);
                    }
                    value
                } else {
                    let fields = root.fields.as_ref().map(|fields| {
                        fields
                            .iter()
                            .map(|field| {
                                self.instantiate(field, roots, inputs, state, cache, depth + 1)
                            })
                            .collect()
                    });
                    self.root(state, None, root.reason, fields)
                };
                cache.insert(*id, value.clone());
                value
            };
            mapped.insert(*id, value);
        }
        let mut result: Option<Fact> = None;
        for id in &fact.roots {
            let value = mapped[id].clone();
            result = Some(match result {
                None => value,
                Some(old) => old.merge(value),
            });
        }
        let mut result =
            result.unwrap_or_else(|| Fact::unknown(fact.reason.unwrap_or(Why::Representation)));
        if fact.reason.is_some() {
            result.reason = fact.reason;
        }
        // An input alias retains the actual caller's known closure identity.
        // A concrete closure in the summary supplies its own lifted entry.
        result.callable = fact.callable.or(result.callable);
        for id in &fact.reachable {
            result.reachable.extend(&mapped[id].reachable);
        }
        result
    }

    fn invoke(&mut self, key: Key, inputs: &[Fact], state: &mut State) -> Value {
        let Some(summary) = self.summaries.get(&key) else {
            return self.unknown_call(state, inputs);
        };
        if inputs.len() != summary.publish.len() {
            return self.unknown_call(state, inputs);
        }
        for (value, publish) in inputs.iter().zip(&summary.publish) {
            if *publish {
                self.invalidate(state, value, Why::Call);
            }
        }
        let mut cache = BTreeMap::new();
        match &summary.result {
            Value::Never => Value::Never,
            Value::One(value) => {
                Value::One(self.instantiate(value, &summary.roots, inputs, state, &mut cache, 0))
            }
            Value::Many(values) => Value::Many(
                values
                    .iter()
                    .map(|value| {
                        self.instantiate(value, &summary.roots, inputs, state, &mut cache, 0)
                    })
                    .collect(),
            ),
        }
    }

    fn unknown_call(&mut self, state: &mut State, values: &[Fact]) -> Value {
        for value in values {
            self.invalidate(state, value, Why::Call);
        }
        Value::One(Fact::opaque(Why::Call, values))
    }

    fn site(&mut self, node: usize, e: &CExpr, reason: Why) {
        let (operation, local, fields) = match &e.kind {
            K::Dup { local, .. } => (Op::Dup, Some(*local), None),
            K::Drop { local, .. } => (Op::Drop, Some(*local), None),
            K::Reset { value, .. } => {
                (Op::Reset, if let K::Local(local) = value.kind { Some(local) } else { None }, None)
            }
            K::MakeData { args, reuse, .. } => (
                if reuse.is_some() { Op::ReusedData } else { Op::FreshData },
                None,
                Some(args.len()),
            ),
            K::MakeClosure { captures, .. } => (Op::Closure, None, Some(captures.len())),
            _ => return,
        };
        self.sites.push(LocalitySite { node, operation, local, fields, reason });
    }

    fn eval(&mut self, e: &CExpr, state: &mut State, depth: usize) -> Value {
        if self.limited || self.nodes >= super::NODE_BUDGET || depth >= super::DEPTH_BUDGET {
            self.limit(state);
            let mut pending = vec![e];
            while let Some(e) = pending.pop() {
                let node = self.nodes;
                self.nodes += 1;
                self.site(node, e, Why::Limit);
                pending.extend(super::children(e).into_iter().rev());
            }
            return Value::One(Fact::unknown(Why::Limit));
        }
        let node = self.nodes;
        self.nodes += 1;
        let sub = depth + 1;
        match &e.kind {
            K::Local(local) => Value::One(state.local(*local)),
            K::Lit(Lit::Str(_)) => Value::One(Fact::unknown(Why::Immortal)),
            K::Lit(Lit::Unit | Lit::Bool(_) | Lit::Char(_)) => {
                Value::One(Fact::unknown(Why::Immediate))
            }
            K::Lit(Lit::Int(n)) if (-(1i64 << 62)..(1i64 << 62)).contains(n) => {
                Value::One(Fact::unknown(Why::Immediate))
            }
            K::Lit(_) => Value::One(Fact::unknown(Why::Representation)),
            K::Global(def) => {
                let Some(&index) = self.program.ids.get(def) else {
                    return Value::One(Fact::unknown(Why::Global));
                };
                let key = Key(index, Some(0));
                if self.program.function(key).params.is_empty() {
                    return self.invoke(key, &[], state);
                }
                let mut fact = Fact::unknown(Why::Immortal);
                fact.callable = Some(key);
                Value::One(fact)
            }
            K::Error => Value::Never,
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
                    other => vec![Fact::opaque(Why::Representation, &[other.one()]); locals.len()],
                };
                let old: Vec<_> = locals
                    .iter()
                    .zip(values)
                    .map(|(id, value)| (*id, state.locals.insert(*id, value)))
                    .collect();
                let result = self.eval(body, state, sub);
                for (id, old) in old {
                    if let Some(old) = old {
                        state.locals.insert(id, old);
                    } else {
                        state.locals.remove(&id);
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
                // Publication obligations are accumulated in the engine even on
                // non-returning branches; only local continuation facts diverge.
                match (&a, &b) {
                    (Value::Never, _) => {}
                    (_, Value::Never) => *state = left,
                    _ => state.join(left),
                }
                a.merge(b)
            }
            K::MakeData { args, niche, scalars, .. } => {
                let values: Vec<_> = args
                    .iter()
                    .enumerate()
                    .map(|(i, arg)| {
                        let value = self.eval(arg, state, sub).one();
                        if i < 64 && scalars & (1 << i) != 0 || !uniform(&arg.ty) {
                            Fact::opaque(Why::Representation, &[value])
                        } else {
                            value
                        }
                    })
                    .collect();
                let fact = if niche.is_some() {
                    Fact::opaque(Why::Representation, &values)
                } else if args.is_empty() {
                    Fact::unknown(Why::Immediate)
                } else {
                    self.root(state, None, None, Some(values))
                };
                self.site(node, e, state.reason(&fact));
                Value::One(fact)
            }
            K::MakeClosure { func, captures, alloc } => {
                let values = captures.iter().map(|local| state.local(*local)).collect();
                let mut fact = match alloc {
                    ClosureAlloc::Heap if !captures.is_empty() => {
                        self.root(state, None, None, Some(values))
                    }
                    ClosureAlloc::Static => Fact::unknown(Why::Immortal),
                    _ => self.root(state, None, Some(Why::Representation), Some(values)),
                };
                let key = Key(self.key.0, Some(func.index()));
                if func.index() < self.program.defs[self.key.0].fns.len() {
                    fact.callable = Some(key);
                }
                self.site(node, e, state.reason(&fact));
                Value::One(fact)
            }
            K::Dup { local, body } | K::Drop { local, body } => {
                self.site(
                    node,
                    e,
                    if self.limited { Why::Limit } else { state.reason(&state.local(*local)) },
                );
                self.eval(body, state, sub)
            }
            K::Reset { value, token, body } => {
                let value = self.eval(value, state, sub).one();
                self.site(node, e, state.reason(&value));
                for id in &value.roots {
                    if let Some(root) = state.roots.get_mut(id) {
                        root.reason.get_or_insert(Why::ResetLifetime);
                    }
                }
                state.locals.insert(*token, Fact::unknown(Why::ResetLifetime));
                self.eval(body, state, sub)
            }
            K::FreeReuse { body, .. } => self.eval(body, state, sub),
            K::DataTag { base, .. } => {
                self.eval(base, state, sub);
                Value::One(Fact::unknown(Why::Representation))
            }
            K::DataField { base, index, scalar, niche } => {
                let value = self.eval(base, state, sub).one();
                Value::One(
                    if let FieldIndex::Const(index) = index
                        && !scalar
                        && niche.is_none()
                        && uniform(&e.ty)
                    {
                        self.project(state, &value, *index)
                    } else {
                        Fact::opaque(Why::Representation, &[value])
                    },
                )
            }
            K::Prim { op, args } => {
                let values: Vec<_> =
                    args.iter().map(|arg| self.eval(arg, state, sub).one()).collect();
                if super::reader(*op) {
                    Value::One(Fact::unknown(Why::Primitive))
                } else {
                    for value in &values {
                        self.invalidate(state, value, Why::Primitive);
                    }
                    Value::One(Fact::opaque(Why::Primitive, &values))
                }
            }
            K::Foreign { symbol, args, .. } => {
                let why = if matches!(symbol.as_str(), "fai_send" | "fai_spawn" | "fai_mark_shared")
                {
                    Why::Published
                } else {
                    Why::Foreign
                };
                let values: Vec<_> =
                    args.iter().map(|arg| self.eval(arg, state, sub).one()).collect();
                for value in &values {
                    self.invalidate(state, value, why);
                }
                Value::One(Fact::opaque(why, &values))
            }
            K::App { func, args, reuse, .. } => {
                let function = self.eval(func, state, sub).one();
                let mut values: Vec<_> = args
                    .iter()
                    .map(|arg| {
                        let value = self.eval(arg, state, sub).one();
                        if uniform(&arg.ty) {
                            value
                        } else {
                            Fact::opaque(Why::Representation, &[value])
                        }
                    })
                    .collect();
                let mut key = function.callable;
                if !reuse.is_empty() {
                    if let Some(old) = key
                        && self.program.defs[old.0].reuse_entry.is_some()
                    {
                        key = Some(Key(old.0, None));
                        let mut leading = vec![Fact::unknown(Why::ResetLifetime); reuse.len()];
                        leading.extend(values);
                        values = leading;
                    } else {
                        key = None;
                    }
                }
                if let Some(key) = key {
                    let f = self.program.function(key);
                    if f.params.len() == values.len() {
                        let mut inputs = Vec::new();
                        for index in 0..f.captures.len() {
                            inputs.push(self.project(state, &function, index as u32));
                        }
                        inputs.extend(values);
                        let result = self.invoke(key, &inputs, state);
                        return match result {
                            Value::One(value) if !uniform(&e.ty) => {
                                Value::One(Fact::opaque(Why::Representation, &[value]))
                            }
                            other => other,
                        };
                    }
                }
                values.push(function);
                self.unknown_call(state, &values)
            }
            K::Join { params, body } => {
                self.barrier(state, Why::Loop);
                for local in params {
                    state.locals.insert(*local, Fact::opaque(Why::Loop, &[state.local(*local)]));
                }
                self.eval(body, state, sub)
            }
            K::Recur { args } => {
                for arg in args {
                    let value = self.eval(arg, state, sub).one();
                    self.invalidate(state, &value, Why::Loop);
                }
                Value::Never
            }
            K::HoleStart { body, .. } => {
                self.barrier(state, Why::Destination);
                self.eval(body, state, sub)
            }
            K::HoleFill { cell: base, .. } | K::HoleClose { base, .. } => {
                self.eval(base, state, sub);
                self.barrier(state, Why::Destination);
                Value::One(Fact::unknown(Why::Destination))
            }
        }
    }
}

// Niche options and scalar transports may change physical identity at a call or
// data-slot boundary. Nominal/opaque types remain conservative without a backend
// representation contract; matching type spelling is not sufficient evidence.
fn uniform(ty: &Ty) -> bool {
    match ty {
        Ty::Var(_) | Ty::Tuple(_) | Ty::Record(_) | Ty::Interface(_) | Ty::Arrow(..) => true,
        Ty::Con(Con::String | Con::Bytes | Con::List | Con::Array) => true,
        Ty::App(head, _) => uniform(head),
        _ => false,
    }
}

#[cfg(test)]
mod tests;

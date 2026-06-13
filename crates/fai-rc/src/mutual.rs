//! Flattening **mutual** tail recursion by reduction to self-recursion.
//!
//! A group of functions that tail-call one another in a cycle (e.g. `isEven`/
//! `isOdd`) is combined into one synthetic function whose internal calls are
//! *self*-calls carrying a tag that selects which member's body to run:
//!
//! ```text
//! combined(tag, p0…):
//!   if tag == 0 then <member 0's body>   // each group call → combined(target_tag, …)
//!   else …      else <member k's body>
//! ```
//!
//! Because every group-internal tail call becomes a saturated self-call of
//! `combined`, the ordinary reference-counting and tail-call passes turn it into a
//! `Join`/`Recur` loop with the tag carried as a loop parameter — so no new IR or
//! code generation is needed. Each member becomes a thin wrapper that calls
//! `combined` with its tag.
//!
//! The first cut handles **intra-module** groups whose every reference to a group
//! member is a plain (not constructor-wrapped) saturated tail call, where no member
//! is row-polymorphic or has a nested lambda. Cross-module groups and
//! constructor-wrapped ("modulo cons") mutual calls are left as ordinary recursion.

use std::sync::Arc;

use fai_core::ir::{CExpr, ClosureAlloc, CoreFn, ExprKind as K, FieldIndex, Lit, LoweredDef, Prim};
use fai_core::{core, helper_inlined};
use fai_db::{Db, SourceFile};
use fai_resolve::{DefId, LocalId, module_defs};
use fai_syntax::Symbol;
use fai_types::{Con, RecordRow, Ty};
use rustc_hash::{FxHashMap, FxHashSet};

/// One mutual-tail-recursion group eligible for flattening.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The member definitions, sorted by name; a member's index is its tag.
    pub members: Vec<DefId>,
    /// The synthetic combined function's definition id (a `mutual#…` name in the
    /// members' file — it shares no name with any source binding).
    pub combined: DefId,
    /// The combined function's arity: one tag parameter plus the widest member's
    /// real parameter count.
    pub arity: usize,
}

impl Group {
    /// The tag value selecting `member`'s body.
    #[must_use]
    pub fn tag_of(&self, member: DefId) -> Option<usize> {
        self.members.iter().position(|m| *m == member)
    }
}

/// The flattenable mutual-tail-recursion groups of a file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MutualGroups {
    /// The groups, in deterministic (combined-name) order.
    pub groups: Vec<Group>,
}

impl MutualGroups {
    /// The group containing `def` as a member, if any.
    #[must_use]
    pub fn group_of(&self, def: DefId) -> Option<&Group> {
        self.groups.iter().find(|g| g.members.contains(&def))
    }
}

/// The flattenable mutual-tail-recursion groups in `file`.
///
/// Per-file, so a body edit re-runs only this file's analysis; early cutoff on the
/// (small) result keeps the ripple from reaching unrelated code.
#[salsa::tracked]
pub fn mutual_groups(db: &dyn Db, file: SourceFile) -> Arc<MutualGroups> {
    let source = file.source(db);
    let defs = module_defs(db, file);

    // Each same-file definition's lowered arity and offset-evidence count.
    let same_file: FxHashSet<DefId> =
        defs.defs.iter().map(|d| DefId::new(source, d.name)).collect();
    let mut arity: FxHashMap<DefId, usize> = FxHashMap::default();
    for d in &defs.defs {
        let id = DefId::new(source, d.name);
        arity.insert(id, core(db, file, d.name).entry().params.len());
    }

    // The intra-file plain-saturated-tail-call graph.
    let mut edges: FxHashMap<DefId, Vec<DefId>> = FxHashMap::default();
    for d in &defs.defs {
        let id = DefId::new(source, d.name);
        let body = core(db, file, d.name);
        let mut targets = Vec::new();
        collect_tail_calls(&body.entry().body, &same_file, &arity, &mut targets);
        edges.insert(id, targets);
    }

    let mut nodes: Vec<DefId> = defs.defs.iter().map(|d| DefId::new(source, d.name)).collect();
    nodes.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

    let mut groups = Vec::new();
    for mut scc in tarjan_sccs(&nodes, &edges) {
        if scc.len() < 2 {
            continue; // self-recursion (or none) is the ordinary per-definition path
        }
        scc.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let set: FxHashSet<DefId> = scc.iter().copied().collect();

        // Every member must be monomorphic, free of nested lambdas, and reference
        // group members only through plain saturated tail calls.
        let eligible = scc.iter().all(|m| {
            let lowered = core(db, file, m.name);
            let evidence = fai_types::declared_or_inferred_scheme(db, *m)
                .map_or(0, |s| fai_types::evidence_count(&s));
            evidence == 0
                && lowered.fns.len() == 1
                && group_tail_ok(&lowered.entry().body, &set, &arity)
        });
        if !eligible {
            continue;
        }

        let max_real = scc.iter().map(|m| arity.get(m).copied().unwrap_or(0)).max().unwrap_or(0);
        let combined_name =
            format!("mutual#{}", scc.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join("#"));
        let combined = DefId::new(source, Symbol::intern(&combined_name));
        groups.push(Group { members: scc, combined, arity: 1 + max_real });
    }
    groups.sort_by(|a, b| a.combined.name.as_str().cmp(b.combined.name.as_str()));
    Arc::new(MutualGroups { groups })
}

/// Collects the same-file definitions `e` plain-tail-calls (a saturated direct
/// call in tail position), the edges of the tail-call graph.
fn collect_tail_calls(
    e: &CExpr,
    same_file: &FxHashSet<DefId>,
    arity: &FxHashMap<DefId, usize>,
    out: &mut Vec<DefId>,
) {
    match &e.kind {
        K::If { then, els, .. } => {
            collect_tail_calls(then, same_file, arity, out);
            collect_tail_calls(els, same_file, arity, out);
        }
        K::Let { body, .. } => collect_tail_calls(body, same_file, arity, out),
        K::App { func, args, .. } => {
            if let K::Global(def) = &func.kind
                && same_file.contains(def)
                && arity.get(def) == Some(&args.len())
            {
                out.push(*def);
            }
        }
        _ => {}
    }
}

/// Whether `e` (a tail position) references group members only through plain
/// saturated tail calls — so the recursion flows through tail calls alone, never
/// off the tail path nor wrapped in a constructor (the deferred "modulo cons" case).
fn group_tail_ok(e: &CExpr, group: &FxHashSet<DefId>, arity: &FxHashMap<DefId, usize>) -> bool {
    match &e.kind {
        K::If { cond, then, els } => {
            !refs_group(cond, group)
                && group_tail_ok(then, group, arity)
                && group_tail_ok(els, group, arity)
        }
        K::Let { value, body, .. } => {
            !refs_group(value, group) && group_tail_ok(body, group, arity)
        }
        K::App { func, args, .. } => {
            if let K::Global(def) = &func.kind
                && group.contains(def)
            {
                // A tail call to a group member: it must be saturated, and its
                // arguments must not themselves reference a group member.
                arity.get(def) == Some(&args.len()) && args.iter().all(|a| !refs_group(a, group))
            } else {
                // A tail call to a non-group function: fine, as long as nothing in
                // it references a group member.
                !refs_group(e, group)
            }
        }
        // Any other tail (a base, or a constructor) must not reference a group
        // member at all.
        _ => !refs_group(e, group),
    }
}

/// Whether `e` references any group member as a value or call target.
fn refs_group(e: &CExpr, group: &FxHashSet<DefId>) -> bool {
    match &e.kind {
        K::Global(def) => group.contains(def),
        K::Lit(_) | K::Local(_) | K::Error | K::MakeClosure { .. } => false,
        K::Prim { args, .. } | K::MakeData { args, .. } => {
            args.iter().any(|a| refs_group(a, group))
        }
        K::App { func, args, .. } => {
            refs_group(func, group) || args.iter().any(|a| refs_group(a, group))
        }
        K::If { cond, then, els } => {
            refs_group(cond, group) || refs_group(then, group) || refs_group(els, group)
        }
        K::Let { value, body, .. } => refs_group(value, group) || refs_group(body, group),
        K::DataTag { base, .. } | K::DataField { base, .. } => refs_group(base, group),
        // The reference-counting and tail-call nodes do not exist in the pre-count
        // body this analysis runs on.
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Tarjan's strongly-connected components over the tail-call graph.
// ---------------------------------------------------------------------------

fn tarjan_sccs(nodes: &[DefId], edges: &FxHashMap<DefId, Vec<DefId>>) -> Vec<Vec<DefId>> {
    let mut t = Tarjan {
        edges,
        index: 0,
        indices: FxHashMap::default(),
        lowlink: FxHashMap::default(),
        on_stack: FxHashSet::default(),
        stack: Vec::new(),
        components: Vec::new(),
    };
    for &n in nodes {
        if !t.indices.contains_key(&n) {
            t.connect(n);
        }
    }
    t.components
}

struct Tarjan<'a> {
    edges: &'a FxHashMap<DefId, Vec<DefId>>,
    index: usize,
    indices: FxHashMap<DefId, usize>,
    lowlink: FxHashMap<DefId, usize>,
    on_stack: FxHashSet<DefId>,
    stack: Vec<DefId>,
    components: Vec<Vec<DefId>>,
}

impl Tarjan<'_> {
    fn connect(&mut self, v: DefId) {
        self.indices.insert(v, self.index);
        self.lowlink.insert(v, self.index);
        self.index += 1;
        self.stack.push(v);
        self.on_stack.insert(v);
        if let Some(es) = self.edges.get(&v) {
            for &w in es {
                if !self.indices.contains_key(&w) {
                    self.connect(w);
                    let low = self.lowlink[&v].min(self.lowlink[&w]);
                    self.lowlink.insert(v, low);
                } else if self.on_stack.contains(&w) {
                    let low = self.lowlink[&v].min(self.indices[&w]);
                    self.lowlink.insert(v, low);
                }
            }
        }
        if self.lowlink[&v] == self.indices[&v] {
            let mut component = Vec::new();
            loop {
                let w = self.stack.pop().expect("stack non-empty");
                self.on_stack.remove(&w);
                component.push(w);
                if w == v {
                    break;
                }
            }
            self.components.push(component);
        }
    }
}

// ---------------------------------------------------------------------------
// The combine transform.
// ---------------------------------------------------------------------------

/// Builds the synthetic combined function for `group` (pre-reference-counting): a
/// tag dispatch over each member's body, with every group-internal tail call
/// rewritten to a saturated self-call of the combined function. Reference counting
/// and the tail-call pass (run afterward by [`crate::rc_lowered`]) turn its
/// self-calls into a `Join`/`Recur` loop.
#[must_use]
pub fn combined_lowered(db: &dyn Db, file: SourceFile, group: &Group) -> LoweredDef {
    let max_real = group.arity - 1;
    let tag = LocalId::from_index(0);
    let p: Vec<LocalId> = (0..max_real).map(|i| LocalId::from_index(1 + i)).collect();
    let mut next = group.arity;

    let mut branches: Vec<CExpr> = Vec::new();
    for member in &group.members {
        // Splice the fully-inlined member body so a member of a mutual group gets
        // the same wrapper and helper inlining as any other definition. The SCC
        // detection and arities above read the raw `core`; inlining only folds in
        // non-recursive helpers (a fellow member is recursive, so never inlined,
        // leaving the inter-member calls intact for the tag dispatch) and never
        // changes arity, so they are unaffected.
        let lowered = helper_inlined(db, file, member.name);
        let entry = lowered.entry();
        let mut subst: FxHashMap<LocalId, LocalId> = FxHashMap::default();
        for (i, &q) in entry.params.iter().enumerate() {
            subst.insert(q, p[i]);
        }
        branches.push(remap_member(&entry.body, &mut subst, &mut next, group));
    }

    // `if tag == 0 then b0 else if tag == 1 then b1 … else b_last`.
    let mut body = branches.pop().expect("a group has at least one member");
    for (i, branch) in branches.into_iter().enumerate().rev() {
        let cond = CExpr::new(
            K::Prim { op: Prim::Eq, args: vec![local_expr(tag), int_lit(i as i64)] },
            Ty::Error,
        );
        body = CExpr::new(
            K::If { cond: Box::new(cond), then: Box::new(branch), els: Box::new(body) },
            Ty::Error,
        );
    }

    let params = std::iter::once(tag).chain(p).collect();
    LoweredDef {
        def: group.combined,
        fns: vec![CoreFn { params, captures: Vec::new(), body }],
        entry_borrowed: Vec::new(),
        reuse_entry: None,
    }
}

/// Builds `member`'s wrapper: `member(args…) = combined(tag, args…, padding)`. The
/// padding fills the combined function's wider parameter list; the selected
/// member's body never reads it.
#[must_use]
pub fn member_wrapper(db: &dyn Db, file: SourceFile, member: DefId, group: &Group) -> LoweredDef {
    let m_arity = core(db, file, member.name).entry().params.len();
    let tag = group.tag_of(member).expect("member belongs to its group");
    let params: Vec<LocalId> = (0..m_arity).map(LocalId::from_index).collect();
    let mut args = vec![int_lit(tag as i64)];
    args.extend(params.iter().map(|&w| local_expr(w)));
    while args.len() < group.arity {
        args.push(int_lit(0));
    }
    let body = CExpr::new(
        K::App {
            func: Box::new(global(group.combined)),
            args,
            reuse: Vec::new(),
            alloc: ClosureAlloc::Heap,
        },
        Ty::Error,
    );
    LoweredDef {
        def: member,
        fns: vec![CoreFn { params, captures: Vec::new(), body }],
        entry_borrowed: Vec::new(),
        reuse_entry: None,
    }
}

/// Rewrites a member's body into a branch of the combined function: remaps its
/// locals into the combined function's space (parameters to the shared `p_*`
/// slots, the rest to fresh slots) and rewrites every group-internal call to a
/// tagged self-call of the combined function.
fn remap_member(
    e: &CExpr,
    subst: &mut FxHashMap<LocalId, LocalId>,
    next: &mut usize,
    group: &Group,
) -> CExpr {
    // The combined function shares padded positional slots across members, so a
    // slot can hold a `Float` value in one branch and an integer (or padding) in
    // another. It therefore uses the uniform boxed/tagged representation for the
    // unboxed scalars: erasing `Float` and `Int` from each node's type keeps code
    // generation from unboxing a shared slot, so its float primitives fall back to
    // the runtime float calls and its integer primitives take the tagged guarded
    // path (a tagged immediate is a valid uniform word).
    let ty = erase_unboxed(&e.ty);
    match &e.kind {
        K::Local(l) => CExpr::new(K::Local(remap_local(*l, subst, next)), ty),
        K::Lit(_) | K::Global(_) | K::Error => CExpr::new(e.kind.clone(), ty),
        // A mutual group is detected and combined before reference counting, so
        // member bodies carry no forwarded reuse tokens (`reuse` is empty).
        K::App { func, args, reuse, alloc } => {
            if let K::Global(def) = &func.kind
                && let Some(target_tag) = group.tag_of(*def)
            {
                // A group call becomes a saturated self-call of the combined
                // function: `combined(target_tag, args…, padding)`.
                let mut new_args = vec![int_lit(target_tag as i64)];
                new_args.extend(args.iter().map(|a| remap_member(a, subst, next, group)));
                while new_args.len() < group.arity {
                    new_args.push(int_lit(0));
                }
                CExpr::new(
                    K::App {
                        func: Box::new(global(group.combined)),
                        args: new_args,
                        reuse: Vec::new(),
                        alloc: ClosureAlloc::Heap,
                    },
                    ty,
                )
            } else {
                let func = Box::new(remap_member(func, subst, next, group));
                let args = args.iter().map(|a| remap_member(a, subst, next, group)).collect();
                let reuse = reuse.iter().map(|t| t.map(|l| remap_local(l, subst, next))).collect();
                CExpr::new(K::App { func, args, reuse, alloc: *alloc }, ty)
            }
        }
        K::Prim { op, args } => {
            let args = args.iter().map(|a| remap_member(a, subst, next, group)).collect();
            CExpr::new(K::Prim { op: *op, args }, ty)
        }
        K::MakeData { tag, args, reuse, scalars, niche: _ } => {
            // The combined loop is schemeless (uniform ABI), so niche `Option`s are
            // erased to the standard representation here — just as the unboxed scalar
            // types are erased — and the member wrappers convert at their boundary.
            let args = args.iter().map(|a| remap_member(a, subst, next, group)).collect();
            CExpr::new(
                K::MakeData { tag: *tag, args, reuse: *reuse, scalars: *scalars, niche: None },
                ty,
            )
        }
        K::If { cond, then, els } => CExpr::new(
            K::If {
                cond: Box::new(remap_member(cond, subst, next, group)),
                then: Box::new(remap_member(then, subst, next, group)),
                els: Box::new(remap_member(els, subst, next, group)),
            },
            ty,
        ),
        K::Let { local, value, body } => {
            // The value is in the outer scope; remap it before binding `local`.
            let value = Box::new(remap_member(value, subst, next, group));
            let local = remap_local(*local, subst, next);
            let body = Box::new(remap_member(body, subst, next, group));
            CExpr::new(K::Let { local, value, body }, ty)
        }
        K::DataTag { base, niche: _ } => CExpr::new(
            K::DataTag { base: Box::new(remap_member(base, subst, next, group)), niche: None },
            ty,
        ),
        K::DataField { base, index, scalar, niche: _ } => {
            let index = match index {
                FieldIndex::Dyn { base: off, evidence } => {
                    FieldIndex::Dyn { base: *off, evidence: remap_local(*evidence, subst, next) }
                }
                c => *c,
            };
            CExpr::new(
                K::DataField {
                    base: Box::new(remap_member(base, subst, next, group)),
                    index,
                    scalar: *scalar,
                    niche: None,
                },
                ty,
            )
        }
        // Eligible members are free of nested lambdas, and the reference-counting
        // and tail-call nodes do not exist in this pre-count body.
        _ => unreachable!("unexpected node in an eligible mutual-recursion member"),
    }
}

/// Maps `l` to its slot in the combined function's local space, allocating a fresh
/// slot the first time a non-parameter local is seen.
fn remap_local(l: LocalId, subst: &mut FxHashMap<LocalId, LocalId>, next: &mut usize) -> LocalId {
    if let Some(&r) = subst.get(&l) {
        return r;
    }
    let id = LocalId::from_index(*next);
    *next += 1;
    subst.insert(l, id);
    id
}

fn int_lit(n: i64) -> CExpr {
    CExpr::new(K::Lit(Lit::Int(n)), Ty::Error)
}

fn local_expr(l: LocalId) -> CExpr {
    CExpr::new(K::Local(l), Ty::Error)
}

fn global(def: DefId) -> CExpr {
    CExpr::new(K::Global(def), Ty::Error)
}

/// Replaces every unboxed scalar (`Float` and `Int`) in a type with the error
/// marker, so the combined function treats them in the uniform boxed/tagged
/// representation (its shared, padded parameter slots cannot carry an unboxed
/// `f64` and must not carry an untagged `i64`). Recurses through composite types;
/// the marker stays in the same code-generation class as the erased type for a
/// boxed *field* (both release as a boxed child), and an erased local takes the
/// runtime drop fallback (a no-op on an immediate, a free on a boxed cell).
fn erase_unboxed(ty: &Ty) -> Ty {
    match ty {
        Ty::Con(Con::Float | Con::Int) => Ty::Error,
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(erase_unboxed).collect()),
        Ty::Record(row) => Ty::Record(RecordRow {
            fields: row.fields.iter().map(|(l, t)| (*l, erase_unboxed(t))).collect(),
            tail: row.tail,
        }),
        Ty::App(head, arg) => Ty::App(Arc::new(erase_unboxed(head)), Arc::new(erase_unboxed(arg))),
        Ty::Arrow(from, to, e) => Ty::arrow_eff(erase_unboxed(from), erase_unboxed(to), e.clone()),
        other => other.clone(),
    }
}

//! Per-module strongly-connected components of signature-less definitions.
//!
//! Inference's cache unit is a definition or an SCC of mutually-dependent
//! definitions. Because a *signature* lets a caller use the declared type
//! instead of the callee's body, signatures **cut** dependency edges: only edges
//! between signature-less bindings can form a cycle. And since cross-module edges
//! always go through (required) public signatures, such cycles are always
//! *intra-module* — so SCCs are computed per file.
//!
//! Each SCC is canonicalized (sorted) and used as inference's key. A binding with
//! a signature is its own singleton SCC (its body is independently checkable).

use std::sync::Arc;

use fai_db::{Db, SourceFile};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::bodies::resolve;
use crate::ids::DefId;
use crate::module::module_defs;

/// One strongly-connected component: a canonical (sorted) set of definitions
/// that must be inferred together.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Scc {
    /// Member definitions, sorted by name text for a stable key.
    pub members: Vec<DefId>,
}

/// The SCCs of a file's definitions, plus a per-def index into them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleSccs {
    /// The components, in a deterministic order (by first member's name).
    pub sccs: Vec<Scc>,
    /// Maps each definition to the index of its component in `sccs`.
    pub index_of: FxHashMap<DefId, usize>,
}

impl ModuleSccs {
    /// The component containing `def`, if any.
    #[must_use]
    pub fn scc_of(&self, def: DefId) -> Option<&Scc> {
        self.index_of.get(&def).map(|&i| &self.sccs[i])
    }
}

/// The intra-module dependency edges that can form a cycle: a signature-less
/// binding's references to *other signature-less bindings in the same file*.
///
/// Signatured bindings (source or target) are excluded — signatures cut the
/// edge — so the resulting graph's SCCs are exactly the groups that must be
/// inferred together.
#[salsa::tracked]
pub fn def_deps(db: &dyn Db, file: SourceFile) -> Arc<FxHashMap<DefId, Vec<DefId>>> {
    let defs = module_defs(db, file);
    let resolved = resolve(db, file);

    // Which same-file names are signature-less (eligible graph nodes)?
    let sigless: FxHashSet<DefId> = defs
        .defs
        .iter()
        .filter(|d| d.signature.is_none())
        .map(|d| DefId::new(file.source(db), d.name))
        .collect();

    let mut graph: FxHashMap<DefId, Vec<DefId>> = FxHashMap::default();
    for node in &sigless {
        // Each sig-less def's edges = its own references that target a sig-less
        // same-file def (signatures cut edges; cross-module deps go through
        // signatures so never appear here).
        let mut edges: Vec<DefId> = resolved
            .deps_of(*node)
            .iter()
            .copied()
            .filter(|target| target != node && sigless.contains(target))
            .collect();
        edges.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        edges.dedup();
        graph.insert(*node, edges);
    }

    Arc::new(graph)
}

/// The strongly-connected components of `file`'s signature-less definitions,
/// plus every (signatured) binding as its own singleton.
#[salsa::tracked]
pub fn module_sccs(db: &dyn Db, file: SourceFile) -> Arc<ModuleSccs> {
    let defs = module_defs(db, file);
    let graph = def_deps(db, file);

    // Tarjan over the signature-less subgraph.
    let mut tarjan = Tarjan::new(&graph);
    for node in graph.keys() {
        if !tarjan.indices.contains_key(node) {
            tarjan.connect(*node);
        }
    }

    let mut sccs: Vec<Scc> = tarjan
        .components
        .into_iter()
        .map(|mut members| {
            members.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            Scc { members }
        })
        .collect();

    // Signatured bindings are singleton SCCs (not in the cycle graph).
    for d in &defs.defs {
        if d.signature.is_some() {
            sccs.push(Scc { members: vec![DefId::new(file.source(db), d.name)] });
        }
    }

    // Deterministic component order: by the first member's name text.
    sccs.sort_by(|a, b| {
        let an = a.members.first().map(|d| d.name.as_str()).unwrap_or("");
        let bn = b.members.first().map(|d| d.name.as_str()).unwrap_or("");
        an.cmp(bn)
    });

    let mut index_of = FxHashMap::default();
    for (i, scc) in sccs.iter().enumerate() {
        for &member in &scc.members {
            index_of.insert(member, i);
        }
    }

    Arc::new(ModuleSccs { sccs, index_of })
}

/// The definitions in `file` that are part of a recursion cycle: a member of a
/// strongly-connected component of size > 1 (mutual recursion), or a definition
/// that references itself (direct self-recursion).
///
/// Unlike [`module_sccs`] — which deliberately cuts signatured edges and drops
/// self-edges because it bounds *inference* — this is the full intra-file
/// reference graph (every definition is a node, every same-file reference an edge,
/// self-edges kept). It answers "is this definition genuinely recursive", which a
/// transform that must never duplicate a recursive body (the helper inliner) keys
/// on. The result is a small set, so a body edit that does not change the
/// recursion structure rips no dependents (salsa early cutoff).
#[salsa::tracked]
pub fn recursive_defs(db: &dyn Db, file: SourceFile) -> Arc<FxHashSet<DefId>> {
    let defs = module_defs(db, file);
    let resolved = resolve(db, file);
    let source = file.source(db);

    // Every same-file definition is a node; an edge is a reference to another
    // same-file definition — or to itself (self-recursion), which the graph keeps.
    let nodes: FxHashSet<DefId> = defs.defs.iter().map(|d| DefId::new(source, d.name)).collect();
    let mut graph: FxHashMap<DefId, Vec<DefId>> = FxHashMap::default();
    let mut self_loops: FxHashSet<DefId> = FxHashSet::default();
    for &node in &nodes {
        let mut edges: Vec<DefId> = Vec::new();
        for &target in resolved.deps_of(node) {
            if !nodes.contains(&target) {
                continue; // cross-module edges never close an intra-file cycle
            }
            if target == node {
                self_loops.insert(node);
            }
            edges.push(target);
        }
        edges.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        edges.dedup();
        graph.insert(node, edges);
    }

    // Tarjan over the full graph; a component of size > 1 is mutual recursion.
    let mut tarjan = Tarjan::new(&graph);
    // Deterministic visit order (graph key order is unspecified).
    let mut order: Vec<DefId> = graph.keys().copied().collect();
    order.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    for node in order {
        if !tarjan.indices.contains_key(&node) {
            tarjan.connect(node);
        }
    }

    let mut recursive: FxHashSet<DefId> = self_loops;
    for component in tarjan.components {
        if component.len() > 1 {
            recursive.extend(component);
        }
    }
    Arc::new(recursive)
}

/// Tarjan's strongly-connected-components algorithm over the dependency graph.
struct Tarjan<'a> {
    graph: &'a FxHashMap<DefId, Vec<DefId>>,
    index: usize,
    indices: FxHashMap<DefId, usize>,
    lowlink: FxHashMap<DefId, usize>,
    on_stack: FxHashSet<DefId>,
    stack: Vec<DefId>,
    components: Vec<Vec<DefId>>,
}

impl<'a> Tarjan<'a> {
    fn new(graph: &'a FxHashMap<DefId, Vec<DefId>>) -> Self {
        Self {
            graph,
            index: 0,
            indices: FxHashMap::default(),
            lowlink: FxHashMap::default(),
            on_stack: FxHashSet::default(),
            stack: Vec::new(),
            components: Vec::new(),
        }
    }

    fn connect(&mut self, v: DefId) {
        self.indices.insert(v, self.index);
        self.lowlink.insert(v, self.index);
        self.index += 1;
        self.stack.push(v);
        self.on_stack.insert(v);

        if let Some(edges) = self.graph.get(&v) {
            for &w in edges {
                if !self.indices.contains_key(&w) {
                    self.connect(w);
                    let low_w = self.lowlink[&w];
                    let low_v = self.lowlink[&v];
                    self.lowlink.insert(v, low_v.min(low_w));
                } else if self.on_stack.contains(&w) {
                    let idx_w = self.indices[&w];
                    let low_v = self.lowlink[&v];
                    self.lowlink.insert(v, low_v.min(idx_w));
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

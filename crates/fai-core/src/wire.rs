//! A portable, serializable form of lowered definitions, for shipping a program
//! from the warm daemon to an isolated run worker.
//!
//! The wire form mirrors the Core IR but drops everything that is either
//! process-local or unused by code generation: a `Global` becomes a
//! module-qualified [`WireDefId`] (the daemon resolves the module label; the
//! worker re-interns it), local/function ids become plain integers, and node
//! **types are omitted** (codegen ignores them, so the worker rebuilds each node
//! with a placeholder type). [`from_wire`] reconstructs real [`LoweredDef`]s with
//! synthetic [`SourceId`]s, returning the module labels and arities the worker
//! needs to build the backend namer.

use fai_resolve::{DefId, LocalId};
use fai_span::SourceId;
use fai_syntax::Symbol;
use fai_types::Ty;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::ir::{CExpr, CoreFn, ExprKind, FnId, Lit, LoweredDef, Prim};

/// A complete program ready to JIT: an entry definition, the `Runtime` value
/// binding applied to it, and their reachable set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireBundle {
    /// The entry definition (`main`).
    pub entry: WireDefId,
    /// The standard library's `Runtime` value binding, applied to `main` by the
    /// entry trampoline.
    pub runtime: WireDefId,
    /// Every reachable definition, in discovery order.
    pub defs: Vec<WireDef>,
}

/// A portable definition identity: its module label and binding name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WireDefId {
    /// The module's display label (pre-mangling), or a fallback.
    pub module: String,
    /// The binding name.
    pub name: String,
}

/// A lowered definition in wire form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireDef {
    /// The definition's identity.
    pub id: WireDefId,
    /// Its parameter count (the backend's arity).
    pub arity: usize,
    /// Its functions (`fns[0]` is the entry; the rest are lifted lambdas).
    pub fns: Vec<WireFn>,
}

/// One function in wire form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireFn {
    /// Parameter slot indices.
    pub params: Vec<u32>,
    /// Captured slot indices.
    pub captures: Vec<u32>,
    /// The function body.
    pub body: WireExpr,
}

/// A Core expression in wire form (no types).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireExpr {
    /// A literal.
    Lit(Lit),
    /// A local slot.
    Local(u32),
    /// A top-level definition referenced as a value.
    Global(WireDefId),
    /// A saturated primitive application.
    Prim {
        /// The primitive.
        op: Prim,
        /// The operands.
        args: Vec<WireExpr>,
    },
    /// A general application.
    App {
        /// The function value.
        func: Box<WireExpr>,
        /// The arguments.
        args: Vec<WireExpr>,
    },
    /// A conditional.
    If {
        /// The condition.
        cond: Box<WireExpr>,
        /// The then-branch.
        then: Box<WireExpr>,
        /// The else-branch.
        els: Box<WireExpr>,
    },
    /// A local binding.
    Let {
        /// The bound slot.
        local: u32,
        /// The bound value.
        value: Box<WireExpr>,
        /// The continuation.
        body: Box<WireExpr>,
    },
    /// A closure construction for a lifted function.
    MakeClosure {
        /// The lifted function index.
        func: u32,
        /// The captured slots.
        captures: Vec<u32>,
    },
    /// A data construction (constructor/record/tuple).
    MakeData {
        /// The constructor tag.
        tag: u32,
        /// The field values.
        args: Vec<WireExpr>,
    },
    /// Read a data value's tag.
    DataTag(Box<WireExpr>),
    /// Project a data value's field.
    DataField {
        /// The data value.
        base: Box<WireExpr>,
        /// The field index.
        index: u32,
    },
    /// Increment a slot's refcount.
    Dup {
        /// The slot.
        local: u32,
        /// The continuation.
        body: Box<WireExpr>,
    },
    /// Release a slot.
    Drop {
        /// The slot.
        local: u32,
        /// The continuation.
        body: Box<WireExpr>,
    },
    /// A lowering-error placeholder.
    Error,
}

/// Converts a lowered definition to wire form. `module_of` maps any referenced
/// definition to its module label (resolved by the caller, which has the
/// database).
#[must_use]
pub fn def_to_wire(
    lowered: &LoweredDef,
    module_of: &dyn Fn(DefId) -> String,
    arity: usize,
) -> WireDef {
    WireDef {
        id: wire_id(lowered.def, module_of),
        arity,
        fns: lowered.fns.iter().map(|f| fn_to_wire(f, module_of)).collect(),
    }
}

fn wire_id(def: DefId, module_of: &dyn Fn(DefId) -> String) -> WireDefId {
    WireDefId { module: module_of(def), name: def.name.as_str().to_owned() }
}

fn fn_to_wire(f: &CoreFn, module_of: &dyn Fn(DefId) -> String) -> WireFn {
    WireFn {
        params: f.params.iter().map(|p| slot(*p)).collect(),
        captures: f.captures.iter().map(|c| slot(*c)).collect(),
        body: expr_to_wire(&f.body, module_of),
    }
}

fn slot(local: LocalId) -> u32 {
    u32::try_from(local.index()).expect("slot index fits u32")
}

fn expr_to_wire(e: &CExpr, module_of: &dyn Fn(DefId) -> String) -> WireExpr {
    match &e.kind {
        ExprKind::Lit(lit) => WireExpr::Lit(lit.clone()),
        ExprKind::Local(local) => WireExpr::Local(slot(*local)),
        ExprKind::Global(def) => WireExpr::Global(wire_id(*def, module_of)),
        ExprKind::Prim { op, args } => WireExpr::Prim {
            op: *op,
            args: args.iter().map(|a| expr_to_wire(a, module_of)).collect(),
        },
        ExprKind::App { func, args } => WireExpr::App {
            func: Box::new(expr_to_wire(func, module_of)),
            args: args.iter().map(|a| expr_to_wire(a, module_of)).collect(),
        },
        ExprKind::If { cond, then, els } => WireExpr::If {
            cond: Box::new(expr_to_wire(cond, module_of)),
            then: Box::new(expr_to_wire(then, module_of)),
            els: Box::new(expr_to_wire(els, module_of)),
        },
        ExprKind::Let { local, value, body } => WireExpr::Let {
            local: slot(*local),
            value: Box::new(expr_to_wire(value, module_of)),
            body: Box::new(expr_to_wire(body, module_of)),
        },
        ExprKind::MakeClosure { func, captures } => WireExpr::MakeClosure {
            func: func.0,
            captures: captures.iter().map(|c| slot(*c)).collect(),
        },
        ExprKind::MakeData { tag, args } => WireExpr::MakeData {
            tag: *tag,
            args: args.iter().map(|a| expr_to_wire(a, module_of)).collect(),
        },
        ExprKind::DataTag(base) => WireExpr::DataTag(Box::new(expr_to_wire(base, module_of))),
        ExprKind::DataField { base, index } => {
            WireExpr::DataField { base: Box::new(expr_to_wire(base, module_of)), index: *index }
        }
        ExprKind::Dup { local, body } => {
            WireExpr::Dup { local: slot(*local), body: Box::new(expr_to_wire(body, module_of)) }
        }
        ExprKind::Drop { local, body } => {
            WireExpr::Drop { local: slot(*local), body: Box::new(expr_to_wire(body, module_of)) }
        }
        ExprKind::Error => WireExpr::Error,
    }
}

/// Reconstructed program: real [`LoweredDef`]s plus the module labels and
/// arities needed to build the backend namer in a database-free worker.
pub struct Rebuilt {
    /// The reconstructed definitions.
    pub defs: Vec<LoweredDef>,
    /// The entry definition.
    pub entry: DefId,
    /// The `Runtime` value binding applied to the entry.
    pub runtime: DefId,
    /// Synthetic source id → module label.
    pub module_labels: FxHashMap<SourceId, String>,
    /// Definition → arity.
    pub arities: FxHashMap<DefId, usize>,
}

/// Reconstructs real [`LoweredDef`]s from a wire bundle, assigning a synthetic
/// [`SourceId`] per distinct module label (internally consistent so `Global`
/// references, the def list, and the entry all align).
#[must_use]
pub fn from_wire(bundle: &WireBundle) -> Rebuilt {
    let mut sources = SourceAssigner::default();
    let entry = sources.def_id(&bundle.entry);
    let runtime = sources.def_id(&bundle.runtime);

    let mut defs = Vec::with_capacity(bundle.defs.len());
    let mut arities = FxHashMap::default();
    for wire in &bundle.defs {
        let def_id = sources.def_id(&wire.id);
        arities.insert(def_id, wire.arity);
        defs.push(LoweredDef {
            def: def_id,
            fns: wire.fns.iter().map(|f| fn_from_wire(f, &mut sources)).collect(),
        });
    }

    Rebuilt { defs, entry, runtime, module_labels: sources.labels, arities }
}

/// Assigns stable synthetic source ids to module labels as they are seen.
#[derive(Default)]
struct SourceAssigner {
    by_label: FxHashMap<String, SourceId>,
    labels: FxHashMap<SourceId, String>,
}

impl SourceAssigner {
    fn source(&mut self, module: &str) -> SourceId {
        if let Some(id) = self.by_label.get(module) {
            return *id;
        }
        let id = SourceId::new(u32::try_from(self.by_label.len()).expect("module count fits u32"));
        self.by_label.insert(module.to_owned(), id);
        self.labels.insert(id, module.to_owned());
        id
    }

    fn def_id(&mut self, id: &WireDefId) -> DefId {
        DefId::new(self.source(&id.module), Symbol::intern(&id.name))
    }
}

fn fn_from_wire(f: &WireFn, sources: &mut SourceAssigner) -> CoreFn {
    CoreFn {
        params: f.params.iter().map(|&i| LocalId::from_index(i as usize)).collect(),
        captures: f.captures.iter().map(|&i| LocalId::from_index(i as usize)).collect(),
        body: expr_from_wire(&f.body, sources),
    }
}

fn expr_from_wire(e: &WireExpr, sources: &mut SourceAssigner) -> CExpr {
    // Types are unused by codegen; rebuild every node with a placeholder.
    let kind = match e {
        WireExpr::Lit(lit) => ExprKind::Lit(lit.clone()),
        WireExpr::Local(i) => ExprKind::Local(LocalId::from_index(*i as usize)),
        WireExpr::Global(id) => ExprKind::Global(sources.def_id(id)),
        WireExpr::Prim { op, args } => ExprKind::Prim {
            op: *op,
            args: args.iter().map(|a| expr_from_wire(a, sources)).collect(),
        },
        WireExpr::App { func, args } => ExprKind::App {
            func: Box::new(expr_from_wire(func, sources)),
            args: args.iter().map(|a| expr_from_wire(a, sources)).collect(),
        },
        WireExpr::If { cond, then, els } => ExprKind::If {
            cond: Box::new(expr_from_wire(cond, sources)),
            then: Box::new(expr_from_wire(then, sources)),
            els: Box::new(expr_from_wire(els, sources)),
        },
        WireExpr::Let { local, value, body } => ExprKind::Let {
            local: LocalId::from_index(*local as usize),
            value: Box::new(expr_from_wire(value, sources)),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExpr::MakeClosure { func, captures } => ExprKind::MakeClosure {
            func: FnId(*func),
            captures: captures.iter().map(|&i| LocalId::from_index(i as usize)).collect(),
        },
        WireExpr::MakeData { tag, args } => ExprKind::MakeData {
            tag: *tag,
            args: args.iter().map(|a| expr_from_wire(a, sources)).collect(),
        },
        WireExpr::DataTag(base) => ExprKind::DataTag(Box::new(expr_from_wire(base, sources))),
        WireExpr::DataField { base, index } => {
            ExprKind::DataField { base: Box::new(expr_from_wire(base, sources)), index: *index }
        }
        WireExpr::Dup { local, body } => ExprKind::Dup {
            local: LocalId::from_index(*local as usize),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExpr::Drop { local, body } => ExprKind::Drop {
            local: LocalId::from_index(*local as usize),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExpr::Error => ExprKind::Error,
    };
    CExpr::new(kind, Ty::Error)
}

#[cfg(test)]
mod tests {
    use fai_db::{Db, FaiDatabase};

    use super::*;
    use crate::core;
    use crate::pretty::pretty_def;

    /// Lowers `name` from `src`, wires it to a one-def bundle and back, and
    /// returns the original and rebuilt pretty renderings (which must be equal).
    fn wire_and_back(src: &str, name: &str) -> (String, String, Rebuilt) {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), src.to_owned());
        let file = db.source_file(id).unwrap();
        let lowered = core(&db, file, Symbol::intern(name));

        let module_of = |_d: DefId| "M".to_owned();
        let wire = def_to_wire(&lowered, &module_of, lowered.entry().params.len());
        let bundle =
            WireBundle { entry: wire.id.clone(), runtime: wire.id.clone(), defs: vec![wire] };
        let rebuilt = from_wire(&bundle);
        (pretty_def(&lowered), pretty_def(&rebuilt.defs[0]), rebuilt)
    }

    #[test]
    fn round_trip_preserves_structure() {
        let (original, rebuilt, info) = wire_and_back("module M\n\nlet f x = x + 1\n", "f");
        assert_eq!(rebuilt, original);
        assert_eq!(info.arities[&info.entry], 1);
        assert_eq!(info.module_labels[&info.entry.file], "M");
    }

    #[test]
    fn round_trip_control_flow_and_strings() {
        let (original, rebuilt, _) =
            wire_and_back("module M\n\nlet f x = if x < 1 then \"a\" else \"b\" ++ \"c\"\n", "f");
        assert_eq!(rebuilt, original);
        assert!(original.contains("(if"), "expected an if in {original}");
    }

    #[test]
    fn round_trip_closure_with_captures() {
        // A lifted lambda capturing `x` exercises MakeClosure + captures.
        let (original, rebuilt, _) =
            wire_and_back("module M\n\nlet adder x = fun y -> x + y\n", "adder");
        assert_eq!(rebuilt, original);
        assert!(original.contains("closure"), "expected a closure in {original}");
    }

    #[test]
    fn bundle_survives_json_round_trip() {
        // The run worker reads the bundle as JSON from a temp file.
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), "module M\n\nlet f x = x + 1\n".to_owned());
        let file = db.source_file(id).unwrap();
        let lowered = core(&db, file, Symbol::intern("f"));
        let wire = def_to_wire(&lowered, &|_| "M".to_owned(), 1);
        let bundle =
            WireBundle { entry: wire.id.clone(), runtime: wire.id.clone(), defs: vec![wire] };

        let json = serde_json::to_string(&bundle).unwrap();
        let decoded: WireBundle = serde_json::from_str(&json).unwrap();
        let rebuilt = from_wire(&decoded);
        assert_eq!(pretty_def(&rebuilt.defs[0]), pretty_def(&lowered));
    }

    #[test]
    fn distinct_module_labels_get_distinct_source_ids() {
        // Two defs in different modules must reconstruct to different source ids,
        // so their backend symbols never collide.
        let a = WireDef {
            id: WireDefId { module: "A".to_owned(), name: "f".to_owned() },
            arity: 0,
            fns: vec![WireFn { params: vec![], captures: vec![], body: WireExpr::Lit(Lit::Unit) }],
        };
        let b = WireDef {
            id: WireDefId { module: "B".to_owned(), name: "f".to_owned() },
            arity: 0,
            fns: vec![WireFn { params: vec![], captures: vec![], body: WireExpr::Lit(Lit::Unit) }],
        };
        let bundle = WireBundle { entry: a.id.clone(), runtime: a.id.clone(), defs: vec![a, b] };
        let rebuilt = from_wire(&bundle);
        assert_eq!(rebuilt.defs.len(), 2);
        assert_ne!(rebuilt.defs[0].def.file, rebuilt.defs[1].def.file);
        assert_eq!(rebuilt.module_labels.len(), 2);
    }
}

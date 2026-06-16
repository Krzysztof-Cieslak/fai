//! A portable, serializable form of lowered definitions, for shipping a program
//! from the warm daemon to an isolated run worker.
//!
//! The wire form mirrors the Core IR but drops everything that is either
//! process-local or finer than code generation needs: a `Global` becomes a
//! module-qualified [`WireDefId`] (the daemon resolves the module label; the
//! worker re-interns it), and local/function ids become plain integers. Each node
//! still carries its type, but as a [`WireTy`] — a projection of [`Ty`] that
//! keeps only the reference-count-relevant *shape* code generation distinguishes
//! (dropping ADT/interface identity, record labels, and arrow operands), so the
//! worker classifies inlined dup/drop exactly as the warm in-process path does.
//! [`from_wire`] reconstructs real [`LoweredDef`]s with synthetic [`SourceId`]s
//! and marker types, returning the module labels and arities the worker needs to
//! build the backend namer.

use fai_resolve::{AdtRef, DefId, InterfaceRef, LocalId};
use fai_span::SourceId;
use fai_syntax::Symbol;
use fai_types::{Con, RecordRow, RowEnd, RowVarId, Ty, TyVarId};
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::ir::{
    CExpr, ClosureAlloc, CoreFn, ExprKind, FieldIndex, FnAbi, FnId, Lit, LoweredDef, Prim,
};
use crate::niche::NicheKind;

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

/// A complete set of contracts ready to JIT and check in an isolated worker: the
/// reachable definitions (including each contract's synthesized harness/property)
/// plus the list of contract entries to apply, each with the generator
/// configuration it should run with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestWireBundle {
    /// Every reachable definition, in discovery order (includes the contract
    /// entry/property/`Arbitrary` defs).
    pub defs: Vec<WireDef>,
    /// The contract entries to apply, in run order.
    pub contracts: Vec<WireContract>,
}

/// One contract entry in wire form: the harness entry to apply
/// (`Seed -> Int -> Size -> TestResult`) and the generator configuration it runs
/// with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireContract {
    /// The harness entry definition's identity.
    pub id: WireDefId,
    /// The contract's position among the file's contracts (stable identifier).
    pub ordinal: usize,
    /// The initial PRNG seed.
    pub seed: i64,
    /// The number of random trials.
    pub trials: i64,
    /// The maximum generation size.
    pub max_size: i64,
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
    /// Its native calling-convention shape (unboxed-float parameters/result),
    /// computed warm from the signature so the database-free worker marshals
    /// direct calls identically.
    pub abi: FnAbi,
    /// Its functions (`fns[0]` is the entry; the rest are lifted lambdas).
    pub fns: Vec<WireFn>,
    /// Per-entry-parameter borrow flags.
    pub entry_borrowed: Vec<bool>,
    /// The token-taking specialized entry, when this definition accepts forwarded
    /// reuse tokens; its leading parameters are the reuse-token slots.
    pub reuse_entry: Option<WireFn>,
    /// The size class (field count) of each reuse-token slot the specialized entry
    /// accepts, in slot order; empty when there is no `reuse_entry`. The worker
    /// reconstructs the reuse signature (and so the `{base}__reuse` ABI) from this.
    pub reuse_sig: Vec<u32>,
    /// The component slots of each spread (fixed-shape float aggregate) entry
    /// parameter (see [`crate::ir::LoweredDef::entry_spread_params`]); empty
    /// (all-`None`) when the definition has no spread parameters.
    #[serde(default)]
    pub entry_spread_params: Vec<Option<Vec<u32>>>,
    /// The inferred bounds-check-elimination **entry facts** (difference
    /// constraints over its parameters), so the database-free worker elides the
    /// same inline `Array` bounds checks. Empty when none were inferred.
    #[serde(default)]
    pub bounds_entry: crate::bounds::BoundSig,
    /// The inferred bounds-check-elimination **result facts** (its result's
    /// length/bounds relative to its parameters), consulted by a caller. Empty when
    /// none were inferred.
    #[serde(default)]
    pub bounds_result: crate::bounds::ResultSig,
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

/// A field slot in wire form (mirrors [`crate::ir::FieldIndex`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireFieldIndex {
    /// A statically known slot.
    Const(u32),
    /// A row-polymorphic slot: `base` plus the value of the `evidence` slot.
    Dyn {
        /// The statically known preceding-field count.
        base: u32,
        /// The evidence slot.
        evidence: u32,
    },
}

/// How a closure is allocated, in wire form (mirrors [`crate::ir::ClosureAlloc`]).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum WireClosureAlloc {
    /// A shared immortal static closure (no captures).
    Static,
    /// A stack-allocated, non-escaping closure.
    Stack,
    /// A heap-allocated, reference-counted closure.
    Heap,
}

/// Projects an allocation kind to wire form.
fn alloc_to_wire(a: ClosureAlloc) -> WireClosureAlloc {
    match a {
        ClosureAlloc::Static => WireClosureAlloc::Static,
        ClosureAlloc::Stack => WireClosureAlloc::Stack,
        ClosureAlloc::Heap => WireClosureAlloc::Heap,
    }
}

/// Rebuilds an allocation kind from wire form.
fn alloc_from_wire(a: WireClosureAlloc) -> ClosureAlloc {
    match a {
        WireClosureAlloc::Static => ClosureAlloc::Static,
        WireClosureAlloc::Stack => ClosureAlloc::Stack,
        WireClosureAlloc::Heap => ClosureAlloc::Heap,
    }
}

/// A type in wire form: a projection of [`Ty`] that keeps only the
/// reference-count-relevant *shape* code generation distinguishes. ADT/interface
/// identity, record labels, and arrow operand types are dropped;
/// [`reconstruct_ty`] rebuilds a marker [`Ty`] in the same class, so the worker
/// classifies inlined dup/drop exactly as the warm in-process path does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireTy {
    /// A type variable, or any type unknown to the projection: the runtime
    /// reference-count fallback.
    Var,
    /// The error type (also the fallback).
    Error,
    /// `Unit`.
    Unit,
    /// `Int`.
    Int,
    /// `Float`.
    Float,
    /// `Bool`.
    Bool,
    /// `String`.
    Str,
    /// `Char`.
    Char,
    /// A tuple — its element types, for fixed-shape field classification.
    Tuple(Vec<WireTy>),
    /// A record — its field types in canonical (label-sorted) layout order, and
    /// whether the row is closed.
    Record {
        /// The field types, in layout order (labels dropped).
        fields: Vec<WireTy>,
        /// Whether the record row is closed (no open tail).
        closed: bool,
    },
    /// A `List` (maybe-immediate data: `[]` is an immediate).
    List,
    /// An `Array` (always boxed; the runtime drop scans its live elements).
    Array,
    /// A discriminated union (maybe-immediate data: nullary constructors are
    /// immediates).
    Adt,
    /// An interface dictionary (always boxed).
    Interface,
    /// A function value / closure (always boxed).
    Arrow,
}

/// A Core expression in wire form: a [`WireExprKind`] plus its projected type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireExpr {
    /// The expression form.
    pub kind: WireExprKind,
    /// The expression's projected type (see [`WireTy`]).
    pub ty: WireTy,
}

/// The form of a [`WireExpr`] (mirrors [`crate::ir::ExprKind`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireExprKind {
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
    /// A saturated foreign (native) call, named by its runtime symbol (serialized
    /// as a string; the worker re-interns it). The portable peer of
    /// [`ExprKind::Foreign`] — the interned `Symbol` cannot cross the wire.
    Foreign {
        /// The native runtime symbol to call.
        symbol: String,
        /// The operands.
        args: Vec<WireExpr>,
        /// Whether the call uses the marshalled ABI (a user `foreign`).
        marshalled: bool,
    },
    /// A general application.
    App {
        /// The function value.
        func: Box<WireExpr>,
        /// The arguments.
        args: Vec<WireExpr>,
        /// Reuse-token slots forwarded to the callee's token-taking entry (one per
        /// slot; `None` is a null-token pad).
        reuse: Vec<Option<u32>>,
        /// How a partial application built by this call is allocated.
        alloc: WireClosureAlloc,
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
        /// How the closure is allocated.
        alloc: WireClosureAlloc,
    },
    /// The exploded components of a fixed-shape float aggregate (multi-value).
    Spread {
        /// The component values, in canonical field order.
        components: Vec<WireExpr>,
    },
    /// Bind several slots from a multi-result value.
    LetMany {
        /// The bound slots, in order.
        locals: Vec<u32>,
        /// The multi-result value.
        value: Box<WireExpr>,
        /// The continuation.
        body: Box<WireExpr>,
    },
    /// A data construction (constructor/record/tuple).
    MakeData {
        /// The constructor tag.
        tag: u32,
        /// The field values.
        args: Vec<WireExpr>,
        /// An optional reuse-token slot to build into in place.
        reuse: Option<u32>,
        /// The unboxed-`f64` field bitmap (see [`crate::ir::ExprKind::MakeData`]).
        scalars: u64,
        /// The niche `Option` scheme, if any (see [`crate::ir::ExprKind::MakeData`]).
        niche: Option<NicheKind>,
    },
    /// Read a data value's tag.
    DataTag {
        /// The data value.
        base: Box<WireExpr>,
        /// The niche `Option` scheme of `base`, if any.
        niche: Option<NicheKind>,
    },
    /// Project a data value's field.
    DataField {
        /// The data value.
        base: Box<WireExpr>,
        /// The field slot.
        index: WireFieldIndex,
        /// Whether the projected slot is an unboxed `f64`.
        scalar: bool,
        /// The niche `Option` scheme of `base`, if any.
        niche: Option<NicheKind>,
    },
    /// Release a data value for reuse, binding a token.
    Reset {
        /// The data value to release.
        value: Box<WireExpr>,
        /// The reuse-token slot.
        token: u32,
        /// The continuation.
        body: Box<WireExpr>,
    },
    /// Free an unconsumed reuse token (no-op on the null token).
    FreeReuse {
        /// The reuse-token slot.
        token: u32,
        /// The continuation.
        body: Box<WireExpr>,
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
    /// A loop header (join point).
    Join {
        /// The loop-carried slots.
        params: Vec<u32>,
        /// The loop body.
        body: Box<WireExpr>,
    },
    /// A tail back-edge to the enclosing loop.
    Recur {
        /// The new loop-carried values.
        args: Vec<WireExpr>,
    },
    /// Begin destination-passing construction, binding a hole token.
    HoleStart {
        /// The destination token slot.
        hole: u32,
        /// The continuation.
        body: Box<WireExpr>,
    },
    /// Link a cell into the spine and advance the destination.
    HoleFill {
        /// The current destination token slot.
        hole: u32,
        /// The cell to link in.
        cell: Box<WireExpr>,
        /// The recursive field index (the next hole).
        field: u32,
    },
    /// Finish the spine with the base-case value.
    HoleClose {
        /// The destination token slot.
        hole: u32,
        /// The base value.
        base: Box<WireExpr>,
    },
    /// A lowering-error placeholder.
    Error,
}

/// Projects a [`Ty`] to the [`WireTy`] code generation needs (see [`WireTy`]).
#[must_use]
pub fn project_ty(ty: &Ty) -> WireTy {
    match ty {
        Ty::Var(_) => WireTy::Var,
        Ty::Error => WireTy::Error,
        Ty::Unit => WireTy::Unit,
        Ty::Con(Con::Int) => WireTy::Int,
        Ty::Con(Con::Float) => WireTy::Float,
        Ty::Con(Con::Bool) => WireTy::Bool,
        Ty::Con(Con::String) => WireTy::Str,
        Ty::Con(Con::Char) => WireTy::Char,
        Ty::Con(Con::List) => WireTy::List,
        Ty::Con(Con::Array) => WireTy::Array,
        Ty::Adt(_) => WireTy::Adt,
        Ty::Interface(_) => WireTy::Interface,
        // An effect argument is erased and never a value on its own (it appears
        // only as a child of an interface application); the runtime-drop `Var`
        // fallback is always safe.
        Ty::EffectArg(_) => WireTy::Var,
        Ty::Arrow(..) => WireTy::Arrow,
        Ty::Tuple(elems) => WireTy::Tuple(elems.iter().map(project_ty).collect()),
        Ty::Record(row) => WireTy::Record {
            fields: row.fields.iter().map(|(_, t)| project_ty(t)).collect(),
            closed: matches!(row.tail, RowEnd::Closed),
        },
        Ty::App(head, _) => project_app_head(head),
    }
}

/// Projects the head of a type application (`List a`, `Option a`, `Dict k v`, …)
/// to its constructor's [`WireTy`]. An unrecognized head falls back to `Var` (the
/// runtime drop), which is always correct.
fn project_app_head(head: &Ty) -> WireTy {
    match head {
        Ty::Con(Con::List) => WireTy::List,
        Ty::Con(Con::Array) => WireTy::Array,
        Ty::Adt(_) => WireTy::Adt,
        Ty::Interface(_) => WireTy::Interface,
        Ty::App(inner, _) => project_app_head(inner),
        _ => WireTy::Var,
    }
}

/// Rebuilds a marker [`Ty`] from a [`WireTy`] — not the original type, but one in
/// the same code-generation class (identity, labels, and component types are
/// stand-ins), so the worker's dup/drop classifier agrees with the warm path.
#[must_use]
pub fn reconstruct_ty(w: &WireTy) -> Ty {
    match w {
        WireTy::Var => Ty::Var(TyVarId(0)),
        WireTy::Error => Ty::Error,
        WireTy::Unit => Ty::Unit,
        WireTy::Int => Ty::Con(Con::Int),
        WireTy::Float => Ty::Con(Con::Float),
        WireTy::Bool => Ty::Con(Con::Bool),
        WireTy::Str => Ty::Con(Con::String),
        WireTy::Char => Ty::Con(Con::Char),
        WireTy::Tuple(elems) => Ty::Tuple(elems.iter().map(reconstruct_ty).collect()),
        WireTy::Record { fields, closed } => Ty::Record(RecordRow {
            fields: fields
                .iter()
                .enumerate()
                .map(|(i, t)| (Symbol::intern(&format!("_{i}")), reconstruct_ty(t)))
                .collect(),
            tail: if *closed { RowEnd::Closed } else { RowEnd::Open(RowVarId(0)) },
        }),
        WireTy::List => Ty::list(Ty::Error),
        WireTy::Array => Ty::array(Ty::Error),
        WireTy::Adt => Ty::Adt(AdtRef::new(SourceId::new(0), Symbol::intern("_Adt"))),
        WireTy::Interface => {
            Ty::Interface(InterfaceRef::new(SourceId::new(0), Symbol::intern("_Interface")))
        }
        WireTy::Arrow => Ty::arrow(Ty::Error, Ty::Error),
    }
}

/// Converts a lowered definition to wire form. `module_of` maps any referenced
/// definition to its module label (resolved by the caller, which has the
/// database); `abi` is its native calling-convention shape (computed warm from
/// the signature, so the worker marshals direct calls identically); `reuse_sig`
/// is the size class of each reuse-token slot its specialized entry accepts (the
/// worker has no database, so it reconstructs the reuse signature from this).
#[must_use]
pub fn def_to_wire(
    lowered: &LoweredDef,
    module_of: &dyn Fn(DefId) -> String,
    arity: usize,
    abi: FnAbi,
    reuse_sig: Vec<u32>,
    bounds_entry: crate::bounds::BoundSig,
    bounds_result: crate::bounds::ResultSig,
) -> WireDef {
    WireDef {
        id: wire_id(lowered.def, module_of),
        arity,
        abi,
        fns: lowered.fns.iter().map(|f| fn_to_wire(f, module_of)).collect(),
        entry_borrowed: lowered.entry_borrowed.clone(),
        reuse_entry: lowered.reuse_entry.as_ref().map(|f| fn_to_wire(f, module_of)),
        reuse_sig,
        entry_spread_params: lowered
            .entry_spread_params
            .iter()
            .map(|o| o.as_ref().map(|v| v.iter().map(|&l| slot(l)).collect()))
            .collect(),
        bounds_entry,
        bounds_result,
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

fn field_index_to_wire(index: FieldIndex) -> WireFieldIndex {
    match index {
        FieldIndex::Const(n) => WireFieldIndex::Const(n),
        FieldIndex::Dyn { base, evidence } => {
            WireFieldIndex::Dyn { base, evidence: slot(evidence) }
        }
    }
}

fn field_index_from_wire(index: &WireFieldIndex) -> FieldIndex {
    match *index {
        WireFieldIndex::Const(n) => FieldIndex::Const(n),
        WireFieldIndex::Dyn { base, evidence } => {
            FieldIndex::Dyn { base, evidence: LocalId::from_index(evidence as usize) }
        }
    }
}

fn expr_to_wire(e: &CExpr, module_of: &dyn Fn(DefId) -> String) -> WireExpr {
    let kind = match &e.kind {
        ExprKind::Lit(lit) => WireExprKind::Lit(lit.clone()),
        ExprKind::Local(local) => WireExprKind::Local(slot(*local)),
        ExprKind::Global(def) => WireExprKind::Global(wire_id(*def, module_of)),
        ExprKind::Prim { op, args } => WireExprKind::Prim {
            op: *op,
            args: args.iter().map(|a| expr_to_wire(a, module_of)).collect(),
        },
        ExprKind::Foreign { symbol, args, marshalled } => WireExprKind::Foreign {
            symbol: symbol.as_str().to_owned(),
            args: args.iter().map(|a| expr_to_wire(a, module_of)).collect(),
            marshalled: *marshalled,
        },
        ExprKind::App { func, args, reuse, alloc } => WireExprKind::App {
            func: Box::new(expr_to_wire(func, module_of)),
            args: args.iter().map(|a| expr_to_wire(a, module_of)).collect(),
            reuse: reuse.iter().map(|t| t.map(slot)).collect(),
            alloc: alloc_to_wire(*alloc),
        },
        ExprKind::If { cond, then, els } => WireExprKind::If {
            cond: Box::new(expr_to_wire(cond, module_of)),
            then: Box::new(expr_to_wire(then, module_of)),
            els: Box::new(expr_to_wire(els, module_of)),
        },
        ExprKind::Let { local, value, body } => WireExprKind::Let {
            local: slot(*local),
            value: Box::new(expr_to_wire(value, module_of)),
            body: Box::new(expr_to_wire(body, module_of)),
        },
        ExprKind::MakeClosure { func, captures, alloc } => WireExprKind::MakeClosure {
            func: func.0,
            captures: captures.iter().map(|c| slot(*c)).collect(),
            alloc: alloc_to_wire(*alloc),
        },
        ExprKind::Spread { components } => WireExprKind::Spread {
            components: components.iter().map(|a| expr_to_wire(a, module_of)).collect(),
        },
        ExprKind::LetMany { locals, value, body } => WireExprKind::LetMany {
            locals: locals.iter().map(|l| slot(*l)).collect(),
            value: Box::new(expr_to_wire(value, module_of)),
            body: Box::new(expr_to_wire(body, module_of)),
        },
        ExprKind::MakeData { tag, args, reuse, scalars, niche } => WireExprKind::MakeData {
            tag: *tag,
            args: args.iter().map(|a| expr_to_wire(a, module_of)).collect(),
            reuse: reuse.map(slot),
            scalars: *scalars,
            niche: *niche,
        },
        ExprKind::DataTag { base, niche } => {
            WireExprKind::DataTag { base: Box::new(expr_to_wire(base, module_of)), niche: *niche }
        }
        ExprKind::DataField { base, index, scalar, niche } => WireExprKind::DataField {
            base: Box::new(expr_to_wire(base, module_of)),
            index: field_index_to_wire(*index),
            scalar: *scalar,
            niche: *niche,
        },
        ExprKind::Reset { value, token, body } => WireExprKind::Reset {
            value: Box::new(expr_to_wire(value, module_of)),
            token: slot(*token),
            body: Box::new(expr_to_wire(body, module_of)),
        },
        ExprKind::FreeReuse { token, body } => WireExprKind::FreeReuse {
            token: slot(*token),
            body: Box::new(expr_to_wire(body, module_of)),
        },
        ExprKind::Dup { local, body } => {
            WireExprKind::Dup { local: slot(*local), body: Box::new(expr_to_wire(body, module_of)) }
        }
        ExprKind::Drop { local, body } => WireExprKind::Drop {
            local: slot(*local),
            body: Box::new(expr_to_wire(body, module_of)),
        },
        ExprKind::Join { params, body } => WireExprKind::Join {
            params: params.iter().map(|p| slot(*p)).collect(),
            body: Box::new(expr_to_wire(body, module_of)),
        },
        ExprKind::Recur { args } => {
            WireExprKind::Recur { args: args.iter().map(|a| expr_to_wire(a, module_of)).collect() }
        }
        ExprKind::HoleStart { hole, body } => WireExprKind::HoleStart {
            hole: slot(*hole),
            body: Box::new(expr_to_wire(body, module_of)),
        },
        ExprKind::HoleFill { hole, cell, field } => WireExprKind::HoleFill {
            hole: slot(*hole),
            cell: Box::new(expr_to_wire(cell, module_of)),
            field: *field,
        },
        ExprKind::HoleClose { hole, base } => WireExprKind::HoleClose {
            hole: slot(*hole),
            base: Box::new(expr_to_wire(base, module_of)),
        },
        ExprKind::Error => WireExprKind::Error,
    };
    WireExpr { kind, ty: project_ty(&e.ty) }
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
    /// Definition → native calling-convention shape (for direct-call marshalling).
    pub abis: FxHashMap<DefId, FnAbi>,
    /// Definition → inferred bounds-check-elimination entry facts.
    pub bounds_entry: FxHashMap<DefId, crate::bounds::BoundSig>,
    /// Definition → inferred bounds-check-elimination result facts.
    pub bounds_result: FxHashMap<DefId, crate::bounds::ResultSig>,
}

/// Reconstructs real [`LoweredDef`]s from a wire bundle, assigning a synthetic
/// [`SourceId`] per distinct module label (internally consistent so `Global`
/// references, the def list, and the entry all align).
#[must_use]
pub fn from_wire(bundle: &WireBundle) -> Rebuilt {
    let mut sources = SourceAssigner::default();
    let entry = sources.def_id(&bundle.entry);
    let runtime = sources.def_id(&bundle.runtime);
    let d = defs_from_wire(&bundle.defs, &mut sources);
    Rebuilt {
        defs: d.defs,
        entry,
        runtime,
        module_labels: sources.labels,
        arities: d.arities,
        abis: d.abis,
        bounds_entry: d.bounds_entry,
        bounds_result: d.bounds_result,
    }
}

/// Reconstructed contract set: real [`LoweredDef`]s plus the contract entries to
/// apply and the module labels/arities needed to build the backend namer in a
/// database-free worker.
pub struct RebuiltTest {
    /// The reconstructed definitions (harnesses, properties, and callees).
    pub defs: Vec<LoweredDef>,
    /// The contract entries to apply, in run order.
    pub contracts: Vec<TestContract>,
    /// Synthetic source id → module label.
    pub module_labels: FxHashMap<SourceId, String>,
    /// Definition → arity.
    pub arities: FxHashMap<DefId, usize>,
    /// Definition → native calling-convention shape (for direct-call marshalling).
    pub abis: FxHashMap<DefId, FnAbi>,
    /// Definition → inferred bounds-check-elimination entry facts.
    pub bounds_entry: FxHashMap<DefId, crate::bounds::BoundSig>,
    /// Definition → inferred bounds-check-elimination result facts.
    pub bounds_result: FxHashMap<DefId, crate::bounds::ResultSig>,
}

/// A reconstructed contract entry: the harness definition to apply and its
/// generator configuration.
pub struct TestContract {
    /// The harness entry definition.
    pub def: DefId,
    /// The contract's position among its file's contracts.
    pub ordinal: usize,
    /// The initial PRNG seed.
    pub seed: i64,
    /// The number of random trials.
    pub trials: i64,
    /// The maximum generation size.
    pub max_size: i64,
}

/// Reconstructs a [`TestWireBundle`] into real [`LoweredDef`]s and the contract
/// entries to apply, assigning a synthetic [`SourceId`] per distinct module label
/// (so the harness defs, their callees, and the contract entries all align).
#[must_use]
pub fn from_wire_test(bundle: &TestWireBundle) -> RebuiltTest {
    let mut sources = SourceAssigner::default();
    let d = defs_from_wire(&bundle.defs, &mut sources);
    let contracts = bundle
        .contracts
        .iter()
        .map(|c| TestContract {
            def: sources.def_id(&c.id),
            ordinal: c.ordinal,
            seed: c.seed,
            trials: c.trials,
            max_size: c.max_size,
        })
        .collect();
    RebuiltTest {
        defs: d.defs,
        contracts,
        module_labels: sources.labels,
        arities: d.arities,
        abis: d.abis,
        bounds_entry: d.bounds_entry,
        bounds_result: d.bounds_result,
    }
}

/// The result of reconstructing a list of wire definitions: the real
/// [`LoweredDef`]s plus the per-definition side tables a database-free worker needs.
struct DefsFromWire {
    defs: Vec<LoweredDef>,
    arities: FxHashMap<DefId, usize>,
    abis: FxHashMap<DefId, FnAbi>,
    bounds_entry: FxHashMap<DefId, crate::bounds::BoundSig>,
    bounds_result: FxHashMap<DefId, crate::bounds::ResultSig>,
}

/// Reconstructs a list of wire definitions into real [`LoweredDef`]s, recording
/// each definition's arity, ABI, and bounds facts. Shared by the run and test
/// bundle reconstructions.
fn defs_from_wire(wire_defs: &[WireDef], sources: &mut SourceAssigner) -> DefsFromWire {
    let mut defs = Vec::with_capacity(wire_defs.len());
    let mut arities = FxHashMap::default();
    let mut abis = FxHashMap::default();
    let mut bounds_entry = FxHashMap::default();
    let mut bounds_result = FxHashMap::default();
    for wire in wire_defs {
        let def_id = sources.def_id(&wire.id);
        arities.insert(def_id, wire.arity);
        abis.insert(def_id, wire.abi.clone());
        bounds_entry.insert(def_id, wire.bounds_entry.clone());
        bounds_result.insert(def_id, wire.bounds_result.clone());
        defs.push(LoweredDef {
            def: def_id,
            fns: wire.fns.iter().map(|f| fn_from_wire(f, sources)).collect(),
            entry_borrowed: wire.entry_borrowed.clone(),
            reuse_entry: wire.reuse_entry.as_ref().map(|f| fn_from_wire(f, sources)),
            entry_spread_params: wire
                .entry_spread_params
                .iter()
                .map(|o| {
                    o.as_ref().map(|v| v.iter().map(|&i| LocalId::from_index(i as usize)).collect())
                })
                .collect(),
        });
    }
    DefsFromWire { defs, arities, abis, bounds_entry, bounds_result }
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
    let kind = match &e.kind {
        WireExprKind::Lit(lit) => ExprKind::Lit(lit.clone()),
        WireExprKind::Local(i) => ExprKind::Local(LocalId::from_index(*i as usize)),
        WireExprKind::Global(id) => ExprKind::Global(sources.def_id(id)),
        WireExprKind::Prim { op, args } => ExprKind::Prim {
            op: *op,
            args: args.iter().map(|a| expr_from_wire(a, sources)).collect(),
        },
        WireExprKind::Foreign { symbol, args, marshalled } => ExprKind::Foreign {
            symbol: Symbol::intern(symbol),
            args: args.iter().map(|a| expr_from_wire(a, sources)).collect(),
            marshalled: *marshalled,
        },
        WireExprKind::App { func, args, reuse, alloc } => ExprKind::App {
            func: Box::new(expr_from_wire(func, sources)),
            args: args.iter().map(|a| expr_from_wire(a, sources)).collect(),
            reuse: reuse.iter().map(|i| i.map(|i| LocalId::from_index(i as usize))).collect(),
            alloc: alloc_from_wire(*alloc),
        },
        WireExprKind::If { cond, then, els } => ExprKind::If {
            cond: Box::new(expr_from_wire(cond, sources)),
            then: Box::new(expr_from_wire(then, sources)),
            els: Box::new(expr_from_wire(els, sources)),
        },
        WireExprKind::Let { local, value, body } => ExprKind::Let {
            local: LocalId::from_index(*local as usize),
            value: Box::new(expr_from_wire(value, sources)),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExprKind::MakeClosure { func, captures, alloc } => ExprKind::MakeClosure {
            func: FnId(*func),
            captures: captures.iter().map(|&i| LocalId::from_index(i as usize)).collect(),
            alloc: alloc_from_wire(*alloc),
        },
        WireExprKind::Spread { components } => ExprKind::Spread {
            components: components.iter().map(|a| expr_from_wire(a, sources)).collect(),
        },
        WireExprKind::LetMany { locals, value, body } => ExprKind::LetMany {
            locals: locals.iter().map(|&i| LocalId::from_index(i as usize)).collect(),
            value: Box::new(expr_from_wire(value, sources)),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExprKind::MakeData { tag, args, reuse, scalars, niche } => ExprKind::MakeData {
            tag: *tag,
            args: args.iter().map(|a| expr_from_wire(a, sources)).collect(),
            reuse: reuse.map(|i| LocalId::from_index(i as usize)),
            scalars: *scalars,
            niche: *niche,
        },
        WireExprKind::DataTag { base, niche } => {
            ExprKind::DataTag { base: Box::new(expr_from_wire(base, sources)), niche: *niche }
        }
        WireExprKind::DataField { base, index, scalar, niche } => ExprKind::DataField {
            base: Box::new(expr_from_wire(base, sources)),
            index: field_index_from_wire(index),
            scalar: *scalar,
            niche: *niche,
        },
        WireExprKind::Reset { value, token, body } => ExprKind::Reset {
            value: Box::new(expr_from_wire(value, sources)),
            token: LocalId::from_index(*token as usize),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExprKind::FreeReuse { token, body } => ExprKind::FreeReuse {
            token: LocalId::from_index(*token as usize),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExprKind::Dup { local, body } => ExprKind::Dup {
            local: LocalId::from_index(*local as usize),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExprKind::Drop { local, body } => ExprKind::Drop {
            local: LocalId::from_index(*local as usize),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExprKind::Join { params, body } => ExprKind::Join {
            params: params.iter().map(|&i| LocalId::from_index(i as usize)).collect(),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExprKind::Recur { args } => {
            ExprKind::Recur { args: args.iter().map(|a| expr_from_wire(a, sources)).collect() }
        }
        WireExprKind::HoleStart { hole, body } => ExprKind::HoleStart {
            hole: LocalId::from_index(*hole as usize),
            body: Box::new(expr_from_wire(body, sources)),
        },
        WireExprKind::HoleFill { hole, cell, field } => ExprKind::HoleFill {
            hole: LocalId::from_index(*hole as usize),
            cell: Box::new(expr_from_wire(cell, sources)),
            field: *field,
        },
        WireExprKind::HoleClose { hole, base } => ExprKind::HoleClose {
            hole: LocalId::from_index(*hole as usize),
            base: Box::new(expr_from_wire(base, sources)),
        },
        WireExprKind::Error => ExprKind::Error,
    };
    // Each node's projected type is reconstructed as a marker `Ty` in the same
    // code-generation class, so the worker classifies dup/drop as the warm path.
    CExpr::new(kind, reconstruct_ty(&e.ty))
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
        let wire = def_to_wire(
            &lowered,
            &module_of,
            lowered.entry().params.len(),
            FnAbi::default(),
            Vec::new(),
            crate::bounds::BoundSig::default(),
            crate::bounds::ResultSig::default(),
        );
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
    fn round_trip_char_literal() {
        let (original, rebuilt, _) = wire_and_back("module M\n\nlet c = '\\n'\n", "c");
        assert_eq!(rebuilt, original);
        assert!(original.contains("'\\n'"), "expected a char literal in {original}");
    }

    /// Lowers `name`, wires it, serializes to JSON and back (the real wire form),
    /// rebuilds, and returns the original and rebuilt pretty renderings.
    fn wire_json_and_back(src: &str, name: &str) -> (String, String) {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), src.to_owned());
        let file = db.source_file(id).unwrap();
        let lowered = core(&db, file, Symbol::intern(name));

        let module_of = |_d: DefId| "M".to_owned();
        let wire = def_to_wire(
            &lowered,
            &module_of,
            lowered.entry().params.len(),
            FnAbi::default(),
            Vec::new(),
            crate::bounds::BoundSig::default(),
            crate::bounds::ResultSig::default(),
        );
        let json = serde_json::to_string(&wire).unwrap();
        let decoded: WireDef = serde_json::from_str(&json).unwrap();
        let bundle = WireBundle {
            entry: decoded.id.clone(),
            runtime: decoded.id.clone(),
            defs: vec![decoded],
        };
        let rebuilt = from_wire(&bundle);
        (pretty_def(&lowered), pretty_def(&rebuilt.defs[0]))
    }

    #[test]
    fn round_trip_niche_option_b() {
        // A monomorphic `Option Int` is the immediate-payload niche scheme (`~b`):
        // the constructed value, its tag test, and its field projection all carry
        // the scheme, and the JSON wire form must preserve it.
        let (original, rebuilt) = wire_json_and_back(
            "module M\n\nlet f x = match (if x < 1 then None else Some x) with | None -> 0 | Some y -> y\n",
            "f",
        );
        assert_eq!(rebuilt, original);
        assert!(original.contains("~b"), "expected a Scheme-B niche marker in {original}");
    }

    #[test]
    fn round_trip_niche_option_a() {
        // A monomorphic `Option String` has an always-boxed payload, the `~a` niche
        // scheme; the wire form must preserve it too.
        let (original, rebuilt) = wire_json_and_back(
            "module M\n\nlet f x = match (if x < 1 then None else Some \"hi\") with | None -> 0 | Some y -> String.length y\n",
            "f",
        );
        assert_eq!(rebuilt, original);
        assert!(original.contains("~a"), "expected a Scheme-A niche marker in {original}");
    }

    #[test]
    fn round_trip_reset_and_reuse() {
        // Reset + a reuse-tokened construction are inserted by reference counting,
        // so they are built by hand here and round-tripped through the wire form.
        let mut db = FaiDatabase::new();
        let id = db.add_source("M.fai".into(), "module M\n\nlet f x = x\n".to_owned());
        let file = db.source_file(id).unwrap();
        let def = DefId::new(file.source(&db), Symbol::intern("f"));

        let cell = LocalId::from_index(0);
        let token = LocalId::from_index(1);
        let made = CExpr::new(
            ExprKind::MakeData {
                tag: 1,
                args: vec![CExpr::new(ExprKind::Lit(Lit::Int(7)), Ty::Error)],
                reuse: Some(token),
                scalars: 0,
                niche: None,
            },
            Ty::Error,
        );
        let body = CExpr::new(
            ExprKind::Reset {
                value: Box::new(CExpr::new(ExprKind::Local(cell), Ty::Error)),
                token,
                body: Box::new(made),
            },
            Ty::Error,
        );
        let lowered = LoweredDef {
            def,
            fns: vec![CoreFn { params: vec![cell], captures: Vec::new(), body }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            entry_spread_params: Vec::new(),
        };

        let wire = def_to_wire(
            &lowered,
            &|_| "M".to_owned(),
            1,
            FnAbi::default(),
            Vec::new(),
            crate::bounds::BoundSig::default(),
            crate::bounds::ResultSig::default(),
        );
        let json = serde_json::to_string(&wire).unwrap();
        let decoded: WireDef = serde_json::from_str(&json).unwrap();
        let bundle = WireBundle {
            entry: decoded.id.clone(),
            runtime: decoded.id.clone(),
            defs: vec![decoded],
        };
        let rebuilt = from_wire(&bundle);

        let text = pretty_def(&rebuilt.defs[0]);
        assert_eq!(text, pretty_def(&lowered));
        assert!(text.contains("reset %1 = %0"), "expected the reset in {text}");
        assert!(text.contains("data@%1"), "expected the reuse in {text}");
    }

    #[test]
    fn round_trip_reset_and_free_reuse() {
        // A reset whose token is freed (a branch that builds nothing into it) is
        // inserted by reference counting; built by hand here and round-tripped.
        let mut db = FaiDatabase::new();
        let id = db.add_source("M.fai".into(), "module M\n\nlet f x = x\n".to_owned());
        let file = db.source_file(id).unwrap();
        let def = DefId::new(file.source(&db), Symbol::intern("f"));

        let cell = LocalId::from_index(0);
        let token = LocalId::from_index(1);
        let freed = CExpr::new(
            ExprKind::FreeReuse {
                token,
                body: Box::new(CExpr::new(ExprKind::Lit(Lit::Int(7)), Ty::Error)),
            },
            Ty::Error,
        );
        let body = CExpr::new(
            ExprKind::Reset {
                value: Box::new(CExpr::new(ExprKind::Local(cell), Ty::Error)),
                token,
                body: Box::new(freed),
            },
            Ty::Error,
        );
        let lowered = LoweredDef {
            def,
            fns: vec![CoreFn { params: vec![cell], captures: Vec::new(), body }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            entry_spread_params: Vec::new(),
        };

        let wire = def_to_wire(
            &lowered,
            &|_| "M".to_owned(),
            1,
            FnAbi::default(),
            Vec::new(),
            crate::bounds::BoundSig::default(),
            crate::bounds::ResultSig::default(),
        );
        let json = serde_json::to_string(&wire).unwrap();
        let decoded: WireDef = serde_json::from_str(&json).unwrap();
        let bundle = WireBundle {
            entry: decoded.id.clone(),
            runtime: decoded.id.clone(),
            defs: vec![decoded],
        };
        let rebuilt = from_wire(&bundle);

        let text = pretty_def(&rebuilt.defs[0]);
        assert_eq!(text, pretty_def(&lowered));
        assert!(text.contains("free-reuse %1"), "expected the token free in {text}");
    }

    #[test]
    fn round_trip_tail_call_loop() {
        // The loop and destination-hole nodes are inserted by the tail-call
        // transform, so they are built by hand here and round-tripped through the
        // wire form (the daemon ships post-transform definitions).
        let mut db = FaiDatabase::new();
        let id = db.add_source("M.fai".into(), "module M\n\nlet f x = x\n".to_owned());
        let file = db.source_file(id).unwrap();
        let def = DefId::new(file.source(&db), Symbol::intern("f"));

        let xs = LocalId::from_index(0);
        let hole = LocalId::from_index(1);
        let h2 = LocalId::from_index(2);
        let lit = || CExpr::new(ExprKind::Lit(Lit::Int(0)), Ty::Error);
        let cell = CExpr::new(
            ExprKind::MakeData {
                tag: 1,
                args: vec![lit(), lit()],
                reuse: None,
                scalars: 0,
                niche: None,
            },
            Ty::Error,
        );
        // let h2 = holefill hole 1 cell; recur xs h2
        let fill = CExpr::new(
            ExprKind::Let {
                local: h2,
                value: Box::new(CExpr::new(
                    ExprKind::HoleFill { hole, cell: Box::new(cell), field: 1 },
                    Ty::Error,
                )),
                body: Box::new(CExpr::new(
                    ExprKind::Recur {
                        args: vec![
                            CExpr::new(ExprKind::Local(xs), Ty::Error),
                            CExpr::new(ExprKind::Local(h2), Ty::Error),
                        ],
                    },
                    Ty::Error,
                )),
            },
            Ty::Error,
        );
        let close = CExpr::new(ExprKind::HoleClose { hole, base: Box::new(lit()) }, Ty::Error);
        let join = CExpr::new(
            ExprKind::Join {
                params: vec![xs, hole],
                body: Box::new(CExpr::new(
                    ExprKind::If {
                        cond: Box::new(lit()),
                        then: Box::new(close),
                        els: Box::new(fill),
                    },
                    Ty::Error,
                )),
            },
            Ty::Error,
        );
        let body = CExpr::new(ExprKind::HoleStart { hole, body: Box::new(join) }, Ty::Error);
        let lowered = LoweredDef {
            def,
            fns: vec![CoreFn { params: vec![xs], captures: Vec::new(), body }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            entry_spread_params: Vec::new(),
        };

        let wire = def_to_wire(
            &lowered,
            &|_| "M".to_owned(),
            1,
            FnAbi::default(),
            Vec::new(),
            crate::bounds::BoundSig::default(),
            crate::bounds::ResultSig::default(),
        );
        let json = serde_json::to_string(&wire).unwrap();
        let decoded: WireDef = serde_json::from_str(&json).unwrap();
        let bundle = WireBundle {
            entry: decoded.id.clone(),
            runtime: decoded.id.clone(),
            defs: vec![decoded],
        };
        let rebuilt = from_wire(&bundle);

        let text = pretty_def(&rebuilt.defs[0]);
        assert_eq!(text, pretty_def(&lowered));
        assert!(text.contains("holestart %1"), "expected the hole start in {text}");
        assert!(text.contains("(join [%0, %1]"), "expected the loop in {text}");
        assert!(text.contains("holefill %1 1"), "expected the hole fill in {text}");
        assert!(text.contains("holeclose %1"), "expected the hole close in {text}");
        assert!(text.contains("(recur %0 %2)"), "expected the back-edge in {text}");
    }

    #[test]
    fn bundle_survives_json_round_trip() {
        // The run worker reads the bundle as JSON from a temp file.
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), "module M\n\nlet f x = x + 1\n".to_owned());
        let file = db.source_file(id).unwrap();
        let lowered = core(&db, file, Symbol::intern("f"));
        let wire = def_to_wire(
            &lowered,
            &|_| "M".to_owned(),
            1,
            FnAbi::default(),
            Vec::new(),
            crate::bounds::BoundSig::default(),
            crate::bounds::ResultSig::default(),
        );
        let bundle =
            WireBundle { entry: wire.id.clone(), runtime: wire.id.clone(), defs: vec![wire] };

        let json = serde_json::to_string(&bundle).unwrap();
        let decoded: WireBundle = serde_json::from_str(&json).unwrap();
        let rebuilt = from_wire(&decoded);
        assert_eq!(pretty_def(&rebuilt.defs[0]), pretty_def(&lowered));
    }

    #[test]
    fn prim_borrow_decision_survives_wire() {
        // A borrow-sensitive primitive (structural equality) on a boxed operand
        // must keep its borrow decision across the wire: codegen re-derives the
        // borrowed vs owned runtime variant from the operand's type. The wire form
        // carries each node's projected type, so the boxed operand still reads as
        // boxed-rc after the round trip (otherwise codegen would pick the consuming
        // variant and double-free, disagreeing with the drops reference counting
        // inserted).
        let boxed = CExpr::new(ExprKind::Local(LocalId::from_index(0)), Ty::Con(Con::String));
        let eq = CExpr::new(
            ExprKind::Prim { op: Prim::Eq, args: vec![boxed.clone(), boxed] },
            Ty::Error,
        );
        let wire = expr_to_wire(&eq, &|_| "M".to_owned());
        // The operand's projected type is carried on the node (no separate flag).
        match &wire.kind {
            WireExprKind::Prim { args, .. } => {
                assert_eq!(args[0].ty, WireTy::Str, "the boxed operand's type is carried");
            }
            other => panic!("expected a Prim, got {other:?}"),
        }
        let mut sources = SourceAssigner::default();
        let back = expr_from_wire(&wire, &mut sources);
        match &back.kind {
            ExprKind::Prim { op, args } => {
                assert!(op.borrows_operand(&args[0].ty), "first operand reads as boxed-rc");
            }
            other => panic!("expected a Prim, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_foreign_call() {
        // A foreign (native capability) call crosses the wire by symbol *name*: the
        // interned `Symbol` cannot, so it serializes as a string and re-interns in
        // the worker.
        let arg = CExpr::new(ExprKind::Local(LocalId::from_index(0)), Ty::Con(Con::String));
        let call = CExpr::new(
            ExprKind::Foreign {
                symbol: Symbol::intern("fai_console_write_line"),
                args: vec![arg],
                marshalled: false,
            },
            Ty::Unit,
        );
        let wire = expr_to_wire(&call, &|_| "M".to_owned());
        match &wire.kind {
            WireExprKind::Foreign { symbol, args, marshalled } => {
                assert_eq!(symbol, "fai_console_write_line");
                assert_eq!(args.len(), 1);
                assert!(!marshalled);
            }
            other => panic!("expected a Foreign, got {other:?}"),
        }
        let mut sources = SourceAssigner::default();
        let back = expr_from_wire(&wire, &mut sources);
        match &back.kind {
            ExprKind::Foreign { symbol, args, marshalled } => {
                assert_eq!(symbol.as_str(), "fai_console_write_line");
                assert_eq!(args.len(), 1);
                assert!(!marshalled);
            }
            other => panic!("expected a Foreign, got {other:?}"),
        }
    }

    #[test]
    fn test_bundle_survives_json_round_trip() {
        // The test worker reads a TestWireBundle as JSON from a temp file, then
        // applies each listed contract entry.
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), "module M\n\nlet f x = x + 1\n".to_owned());
        let file = db.source_file(id).unwrap();
        let lowered = core(&db, file, Symbol::intern("f"));
        let wire = def_to_wire(
            &lowered,
            &|_| "M".to_owned(),
            1,
            FnAbi::default(),
            Vec::new(),
            crate::bounds::BoundSig::default(),
            crate::bounds::ResultSig::default(),
        );
        let bundle = TestWireBundle {
            contracts: vec![WireContract {
                id: wire.id.clone(),
                ordinal: 0,
                seed: 7,
                trials: 50,
                max_size: 30,
            }],
            defs: vec![wire],
        };

        let json = serde_json::to_string(&bundle).unwrap();
        let decoded: TestWireBundle = serde_json::from_str(&json).unwrap();
        let rebuilt = from_wire_test(&decoded);
        assert_eq!(pretty_def(&rebuilt.defs[0]), pretty_def(&lowered));
        assert_eq!(rebuilt.contracts.len(), 1);
        let c = &rebuilt.contracts[0];
        assert_eq!((c.ordinal, c.seed, c.trials, c.max_size), (0, 7, 50, 30));
        // The contract entry resolves to the same def the bundle compiles.
        assert_eq!(c.def, rebuilt.defs[0].def);
        assert_eq!(rebuilt.arities[&c.def], 1);
    }

    #[test]
    fn distinct_module_labels_get_distinct_source_ids() {
        // Two defs in different modules must reconstruct to different source ids,
        // so their backend symbols never collide.
        let a = WireDef {
            id: WireDefId { module: "A".to_owned(), name: "f".to_owned() },
            arity: 0,
            abi: FnAbi::default(),
            fns: vec![WireFn {
                params: vec![],
                captures: vec![],
                body: WireExpr { kind: WireExprKind::Lit(Lit::Unit), ty: WireTy::Unit },
            }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            reuse_sig: Vec::new(),
            entry_spread_params: Vec::new(),
            bounds_entry: crate::bounds::BoundSig::default(),
            bounds_result: crate::bounds::ResultSig::default(),
        };
        let b = WireDef {
            id: WireDefId { module: "B".to_owned(), name: "f".to_owned() },
            arity: 0,
            abi: FnAbi::default(),
            fns: vec![WireFn {
                params: vec![],
                captures: vec![],
                body: WireExpr { kind: WireExprKind::Lit(Lit::Unit), ty: WireTy::Unit },
            }],
            entry_borrowed: Vec::new(),
            reuse_entry: None,
            reuse_sig: Vec::new(),
            entry_spread_params: Vec::new(),
            bounds_entry: crate::bounds::BoundSig::default(),
            bounds_result: crate::bounds::ResultSig::default(),
        };
        let bundle = WireBundle { entry: a.id.clone(), runtime: a.id.clone(), defs: vec![a, b] };
        let rebuilt = from_wire(&bundle);
        assert_eq!(rebuilt.defs.len(), 2);
        assert_ne!(rebuilt.defs[0].def.file, rebuilt.defs[1].def.file);
        assert_eq!(rebuilt.module_labels.len(), 2);
    }
}

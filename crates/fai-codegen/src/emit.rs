//! Translating Core IR to Cranelift IR.
//!
//! [`compile_def`] declares and defines, for one lowered definition, the code of
//! its entry and lifted functions, the static (immortal) closure that represents
//! it as a value, and the static string literals it uses. The same path drives
//! both back ends (AOT object emission and the in-process JIT) through the
//! [`Module`] trait.
//!
//! A **direct-callable** definition — non-row-polymorphic with at least one
//! parameter — uses a register-passing entry `fn(env, a0, …, aN) -> ret`: a
//! saturated direct call passes its value arguments in registers (a scalar
//! `Float` as an `f64` register), skipping the spilled argument array. Every other
//! function — lifted lambdas, row-polymorphic and nullary entries, reached through
//! `fai_apply_n` — keeps the uniform `fn(env: *const i64, args: *const i64) -> i64`
//! convention: parameters are read from `args`, captures from `env`. The
//! first-class value form always uses the uniform convention, so a register entry
//! is reached for that form through a bridging wrapper (the static closure's code).
//! Values are uniform tagged 64-bit words (except an unboxed scalar `Float`).
//! `Dup` and `Drop` lower to inline reference-count code — a tag-check (elided for
//! a statically always-boxed type), then an in-place increment or a
//! decrement-and-conditional-free — calling the runtime only to reclaim memory
//! (`fai_free` for a boxed leaf, `fai_drop_dead` for a variable-shape cell) or,
//! for a value of unknown (polymorphic) type, falling back to `fai_drop`.

use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::Ieee64;
use cranelift_codegen::ir::{AbiParam, Block, FuncRef, InstBuilder, MemFlags, Value, types};
use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use fai_core::ir::{CExpr, CoreFn, ExprKind, FieldIndex, FnAbi, Lit, LoweredDef, Prim};
use fai_resolve::{DefId, LocalId};
use fai_runtime as rt;
use fai_types::{Con, RowEnd, Ty};
use rustc_hash::{FxHashMap, FxHashSet};

/// Builds the exported code symbol for a definition.
#[must_use]
pub fn code_symbol(namer: &dyn Fn(DefId) -> String, def: DefId) -> String {
    namer(def)
}

/// Builds the exported static-closure symbol for a definition.
#[must_use]
pub fn closure_symbol(namer: &dyn Fn(DefId) -> String, def: DefId) -> String {
    format!("{}__closure", namer(def))
}

/// Compiles one lowered definition into `module`: its functions, its static
/// closure value, and its string literals.
///
/// `arity_of` reports any referenced definition's parameter count, so a
/// reference to a zero-arity binding (a value, not a function) is forced;
/// `signature_of` reports any definition's unboxed-float calling convention, so a
/// direct call marshals float arguments and the result as raw bits.
pub fn compile_def<M: Module>(
    module: &mut M,
    lowered: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    signature_of: &dyn Fn(DefId) -> FnAbi,
) {
    let mut jobs = Vec::new();
    build_def(module, lowered, namer, arity_of, signature_of, &mut jobs);
    for (id, mut ctx) in jobs {
        module.define_function(id, &mut ctx).expect("define function");
    }
}

/// Declares one lowered definition's functions and static closure, defines the
/// closure data, and **builds** (but does not compile or define) each function
/// body, pushing a `(FuncId, Context)` job per body onto `jobs`. The IR building
/// mutates `module` (declaring callees, runtime imports, and string data), so it
/// is serial; the caller compiles the collected jobs — the expensive step —
/// however it likes (the JIT does so in parallel, the AOT path serially).
pub(crate) fn build_def<M: Module>(
    module: &mut M,
    lowered: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    signature_of: &dyn Fn(DefId) -> FnAbi,
    jobs: &mut Vec<(FuncId, Context)>,
) {
    let base = namer(lowered.def);
    let abi = signature_of(lowered.def);
    let uniform_sig = code_signature(module);
    let arity = lowered.entry().params.len();
    // The entry (`fn0`) of a direct-callable definition uses the register ABI;
    // every lifted lambda keeps the uniform array ABI (reached through `apply_n`).
    let entry_sig = entry_signature(module, arity, &abi);

    // Declare every function (entry exported, lifted lambdas local).
    let mut fn_ids = Vec::with_capacity(lowered.fns.len());
    for i in 0..lowered.fns.len() {
        let name = if i == 0 { base.clone() } else { format!("{base}__fn{i}") };
        let linkage = if i == 0 { Linkage::Export } else { Linkage::Local };
        let sig = if i == 0 { &entry_sig } else { &uniform_sig };
        fn_ids.push(module.declare_function(&name, linkage, sig).expect("declare function"));
    }

    // The exported static closure representing the definition as a value.
    let closure_data = module
        .declare_data(&closure_symbol(namer, lowered.def), Linkage::Export, true, false)
        .expect("declare closure data");

    // Build each function body into its own (uncompiled) context.
    for (i, f) in lowered.fns.iter().enumerate() {
        let ctx =
            build_fn(module, f, lowered, namer, arity_of, signature_of, &abi, &fn_ids, &base, i);
        jobs.push((fn_ids[i], ctx));
    }

    // The static closure (the first-class value form, reached via `apply_n`) must
    // use the uniform all-owned, all-boxed ABI. When the entry is a register entry,
    // borrows parameters, or uses the unboxed-float ABI (raw-bits float
    // parameters/result), point the closure at a wrapper that bridges to the entry
    // — marshalling registers, unboxing boxed float arguments, releasing borrowed
    // arguments, and boxing a float result — while direct callers call the
    // (specialized) entry symbol.
    let closure_code = if abi.register_abi || lowered.borrows_any() || !abi.is_uniform() {
        let wrapper = module
            .declare_function(&format!("{base}__owned"), Linkage::Local, &uniform_sig)
            .expect("declare wrapper");
        let ctx = build_owned_wrapper(module, fn_ids[0], &lowered.entry_borrowed, &abi, arity);
        jobs.push((wrapper, ctx));
        wrapper
    } else {
        fn_ids[0]
    };
    define_static_closure(module, closure_data, closure_code, arity as u64);
}

/// Builds the uniform-ABI wrapper bridging the static-closure / `apply_n` path
/// (all arguments boxed and owned, in the `args` array) to a specialized entry,
/// then drops the borrowed (non-float) arguments the entry left untouched and
/// boxes a float result.
///
/// For a **register** entry it loads each boxed argument and passes it in a
/// register — a scalar float unboxed to an `f64` (releasing the box), every other
/// argument the word itself — and calls `entry(env, a0, …)`. For a **uniform**
/// entry (a row-polymorphic or nullary definition that still has an unboxed-float
/// slot) it passes the argument array, replacing each unboxed-float slot with the
/// box's raw bits in a fresh array (releasing the box). Returns the uncompiled
/// context (the caller compiles and defines it).
fn build_owned_wrapper<M: Module>(
    module: &mut M,
    entry: FuncId,
    borrowed: &[bool],
    abi: &FnAbi,
    arity: usize,
) -> Context {
    let mut ctx = module.make_context();
    ctx.func.signature = code_signature(module);
    let mut fbcx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fbcx);
        let block = builder.create_block();
        builder.append_block_params_for_function_params(block);
        builder.switch_to_block(block);
        builder.seal_block(block);
        let env = builder.block_params(block)[0];
        let args = builder.block_params(block)[1];

        let mut drop_sig = module.make_signature();
        drop_sig.params.push(AbiParam::new(types::I64));
        let drop_id =
            module.declare_function("fai_drop", Linkage::Import, &drop_sig).expect("declare drop");
        let drop_ref = module.declare_func_in_func(drop_id, builder.func);
        let float_off = i32::try_from(rt::FLOAT_VALUE_OFFSET).expect("float value offset");
        let entry_ref = module.declare_func_in_func(entry, builder.func);

        let mut result = if abi.register_abi {
            // Register entry: load each boxed argument and pass it in a register —
            // a scalar float unboxed to `f64` (releasing the box), else the word.
            let mut call_args = Vec::with_capacity(arity + 1);
            call_args.push(env);
            for i in 0..arity {
                let offset = i32::try_from(i * 8).expect("arg offset");
                let orig = builder.ins().load(types::I64, MemFlags::trusted(), args, offset);
                let v = if abi.float_param(i) {
                    let bits = builder.ins().load(types::I64, MemFlags::trusted(), orig, float_off);
                    builder.ins().call(drop_ref, &[orig]);
                    builder.ins().bitcast(types::F64, MemFlags::new(), bits)
                } else {
                    orig
                };
                call_args.push(v);
            }
            let call = builder.ins().call(entry_ref, &call_args);
            builder.inst_results(call)[0]
        } else {
            // Uniform entry: pass the argument array. For unboxed-float parameters,
            // replace the boxed argument with its raw bits in a fresh array and
            // release the box; other slots pass through.
            let entry_args = if abi.float_params.iter().any(|&f| f) {
                let size = u32::try_from(arity * 8).expect("array size");
                let slot = builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    size,
                    3,
                ));
                for i in 0..arity {
                    let offset = i32::try_from(i * 8).expect("arg offset");
                    let orig = builder.ins().load(types::I64, MemFlags::trusted(), args, offset);
                    let v = if abi.float_param(i) {
                        let bits =
                            builder.ins().load(types::I64, MemFlags::trusted(), orig, float_off);
                        builder.ins().call(drop_ref, &[orig]);
                        bits
                    } else {
                        orig
                    };
                    builder.ins().stack_store(v, slot, offset);
                }
                builder.ins().stack_addr(types::I64, slot, 0)
            } else {
                args
            };
            let call = builder.ins().call(entry_ref, &[env, entry_args]);
            builder.inst_results(call)[0]
        };

        // Drop the borrowed arguments the entry left untouched; an unboxed-float
        // argument's box was already released above.
        for (i, &borrowed) in borrowed.iter().enumerate() {
            if borrowed && !abi.float_param(i) {
                let offset = i32::try_from(i * 8).expect("arg offset");
                let v = builder.ins().load(types::I64, MemFlags::trusted(), args, offset);
                builder.ins().call(drop_ref, &[v]);
            }
        }

        // Box a float result back into the uniform representation: a register entry
        // returns an `f64`, a uniform entry returns its raw bits.
        if abi.float_return {
            let bits = if abi.register_abi {
                builder.ins().bitcast(types::I64, MemFlags::new(), result)
            } else {
                result
            };
            let mut box_sig = module.make_signature();
            box_sig.params.push(AbiParam::new(types::I64));
            box_sig.returns.push(AbiParam::new(types::I64));
            let box_id = module
                .declare_function("fai_box_float", Linkage::Import, &box_sig)
                .expect("declare box float");
            let box_ref = module.declare_func_in_func(box_id, builder.func);
            let boxed = builder.ins().call(box_ref, &[bits]);
            result = builder.inst_results(boxed)[0];
        }

        builder.ins().return_(&[result]);
        builder.finalize();
    }
    ctx
}

/// The uniform calling convention `fn(env, args) -> i64`: every lifted lambda and
/// every non-direct-callable entry (row-polymorphic or nullary), and the
/// first-class wrapper. Direct-callable entries use [`entry_signature`] instead.
fn code_signature<M: Module>(module: &M) -> cranelift_codegen::ir::Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // env
    sig.params.push(AbiParam::new(types::I64)); // args
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// The calling convention of a definition's entry. A **direct-callable**
/// definition (`abi.register_abi`) passes its value arguments in registers:
/// `fn(env, a0, …, aN) -> ret`, each parameter an `f64` for a scalar `Float` else
/// the uniform `i64` word, and the result likewise. Every other entry keeps the
/// uniform [`code_signature`]. `arity` is the runtime parameter count.
fn entry_signature<M: Module>(
    module: &M,
    arity: usize,
    abi: &FnAbi,
) -> cranelift_codegen::ir::Signature {
    if !abi.register_abi {
        return code_signature(module);
    }
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // env (unused: a top-level entry captures nothing)
    for i in 0..arity {
        let ty = if abi.float_param(i) { types::F64 } else { types::I64 };
        sig.params.push(AbiParam::new(ty));
    }
    let ret = if abi.float_return { types::F64 } else { types::I64 };
    sig.returns.push(AbiParam::new(ret));
    sig
}

/// Builds one function's Cranelift IR into a fresh, **uncompiled** context. The
/// build mutates `module` (declaring callees, runtime imports, and string data),
/// so it is serial; the caller compiles the returned context.
#[allow(clippy::too_many_arguments)]
fn build_fn<M: Module>(
    module: &mut M,
    core_fn: &CoreFn,
    lowered: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    signature_of: &dyn Fn(DefId) -> FnAbi,
    abi: &FnAbi,
    fn_ids: &[FuncId],
    base: &str,
    fn_index: usize,
) -> Context {
    let mut ctx = module.make_context();
    // The register ABI applies only to the entry (`fn0`) of a direct-callable
    // definition; lifted lambdas are reached through `apply_n` with the uniform ABI.
    let is_entry = fn_index == 0;
    let register_entry = is_entry && abi.register_abi;
    ctx.func.signature = if is_entry {
        entry_signature(module, core_fn.params.len(), abi)
    } else {
        code_signature(module)
    };
    let mut fbcx = FunctionBuilderContext::new();

    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fbcx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let env = builder.block_params(entry)[0];
        // A register entry's value parameters follow `env` in registers; a uniform
        // function reads them from the `args` array (the second block parameter).
        let reg_params: Vec<Value> = if register_entry {
            (0..core_fn.params.len()).map(|i| builder.block_params(entry)[i + 1]).collect()
        } else {
            Vec::new()
        };
        let args = if register_entry { env } else { builder.block_params(entry)[1] };

        let mut tr = Translator {
            module,
            builder,
            namer,
            arity_of,
            signature_of,
            fn_ids,
            lowered,
            base,
            fn_index,
            vars: FxHashMap::default(),
            var_tys: FxHashMap::default(),
            f64_locals: FxHashSet::default(),
            runtime: FxHashMap::default(),
            string_counter: 0,
            loop_ctx: None,
            result_slot: None,
        };

        // Record each local's static type up front, so reference-count operations
        // on parameters and captures (not just `let`s) can be specialized.
        collect_local_types(&core_fn.body, &mut tr.var_tys);
        // Decide which locals are represented as an unboxed `f64` (see
        // `f64_locals`), so their Cranelift variables are typed `F64`. The entry's
        // raw-bits float parameters are unboxed by the ABI, so they are included
        // even when otherwise unobserved (e.g. an unused parameter).
        tr.collect_f64_locals(&core_fn.body);
        if is_entry {
            for (i, &p) in core_fn.params.iter().enumerate() {
                if abi.float_param(i) {
                    tr.f64_locals.insert(p.index());
                }
            }
        }

        if register_entry {
            // Register entry: parameters arrive in registers, already in their
            // final representation (an `f64` for a scalar float, else the word). A
            // direct-callable (top-level) entry captures nothing.
            for (i, &p) in core_fn.params.iter().enumerate() {
                tr.define_var(p, reg_params[i]);
            }
            debug_assert!(core_fn.captures.is_empty(), "a register entry captures nothing");
        } else {
            // Uniform function: bind parameters from `args` and captures from `env`:
            //  - a uniform entry's raw-bits float parameter is reinterpreted to `f64`;
            //  - any other boxed-`Float` parameter arrives boxed and owned, so its
            //    box is released after its bits are read;
            //  - a captured float is borrowed (the closure still owns the environment).
            for (i, &p) in core_fn.params.iter().enumerate() {
                let raw = tr.load_slot(args, i);
                let v = if is_entry && abi.float_param(i) {
                    tr.i64_to_f64(raw)
                } else if tr.is_f64_local(p) {
                    tr.owning_unbox(raw)
                } else {
                    raw
                };
                tr.define_var(p, v);
            }
            for (i, &c) in core_fn.captures.iter().enumerate() {
                let raw = tr.load_slot(env, i);
                let v = if tr.is_f64_local(c) { tr.borrowing_unbox(raw) } else { raw };
                tr.define_var(c, v);
            }
        }

        let result = tr.expr(&core_fn.body);
        // The entry returns: an `f64` register for a register float entry; raw float
        // bits for a uniform float entry; otherwise the uniform (boxed) word.
        let ret = if register_entry && abi.float_return {
            tr.f64_return(result)
        } else if is_entry && abi.float_return {
            tr.raw_float_return(result)
        } else {
            tr.boxed_return(result)
        };
        tr.builder.ins().return_(&[ret]);
        tr.builder.finalize();
    }

    ctx
}

/// Defines a definition's immortal static closure:
/// `{ rc = IMMORTAL, descriptor = &CLOSURE_DESC, size, code = &entry, arity, env_count = 0 }`.
fn define_static_closure<M: Module>(module: &mut M, data: DataId, entry: FuncId, arity: u64) {
    let size = rt::CLOSURE_ENV_OFFSET as u64;
    let mut bytes = vec![0u8; rt::CLOSURE_ENV_OFFSET];
    bytes[rt::RC_OFFSET..rt::RC_OFFSET + 8].copy_from_slice(&rt::IMMORTAL_RC.to_le_bytes());
    bytes[rt::SIZE_OFFSET..rt::SIZE_OFFSET + 8].copy_from_slice(&size.to_le_bytes());
    bytes[rt::CLOSURE_ARITY_OFFSET..rt::CLOSURE_ARITY_OFFSET + 8]
        .copy_from_slice(&arity.to_le_bytes());
    // env_count is already zero.

    let mut desc = DataDescription::new();
    desc.define(bytes.into_boxed_slice());
    desc.set_align(8); // a closure value is a tagged pointer; the low bits must be clear.
    // descriptor pointer (offset DESC_OFFSET) → FAI_CLOSURE_DESC.
    let desc_gv = declare_descriptor_in_data(module, &mut desc, "FAI_CLOSURE_DESC");
    desc.write_data_addr(rt::DESC_OFFSET as u32, desc_gv, 0);
    // code pointer (offset CLOSURE_CODE_OFFSET) → the entry function.
    let code_ref = module.declare_func_in_data(entry, &mut desc);
    desc.write_function_addr(rt::CLOSURE_CODE_OFFSET as u32, code_ref);

    module.define_data(data, &desc).expect("define closure data");
}

/// Declares an imported runtime descriptor as a data symbol referenceable from a
/// `DataDescription`.
fn declare_descriptor_in_data<M: Module>(
    module: &mut M,
    desc: &mut DataDescription,
    name: &str,
) -> cranelift_codegen::ir::GlobalValue {
    let id = module.declare_data(name, Linkage::Import, false, false).expect("declare descriptor");
    module.declare_data_in_data(id, desc)
}

/// Per-function translation state.
struct Translator<'a, M: Module> {
    module: &'a mut M,
    builder: FunctionBuilder<'a>,
    namer: &'a dyn Fn(DefId) -> String,
    arity_of: &'a dyn Fn(DefId) -> usize,
    signature_of: &'a dyn Fn(DefId) -> FnAbi,
    fn_ids: &'a [FuncId],
    lowered: &'a LoweredDef,
    base: &'a str,
    fn_index: usize,
    vars: FxHashMap<usize, Variable>,
    /// Each local's static type, where known — collected up front (see
    /// [`collect_local_types`]) from every `Local` use and `let` binding, so
    /// parameters and captures are covered, not just `let`s. Used to specialize
    /// the inlined reference-count operations (immediate no-op, boxed leaf,
    /// fixed-shape cell, variable-shape data, or the runtime fallback).
    var_tys: FxHashMap<usize, Ty>,
    /// Locals represented as an **unboxed** `f64` (Cranelift `F64` variables)
    /// rather than a tagged `i64`: a monomorphic scalar `Float` local. A value of
    /// such a local flows in registers; it is boxed only when it crosses into a
    /// uniform slot (a data field, a closure environment, an `apply_n` argument).
    /// Built from the recorded local types (and, for the entry, its unboxed-float
    /// parameters); see [`Translator::collect_f64_locals`].
    f64_locals: FxHashSet<usize>,
    runtime: FxHashMap<&'static str, FuncRef>,
    string_counter: usize,
    /// The enclosing tail-call loop, while translating a `Join` body: where
    /// `Recur` jumps back and where the loop's result exits.
    loop_ctx: Option<LoopCtx>,
    /// The destination-passing result slot's address (set by `HoleStart`, read by
    /// `HoleClose`), for a loop that builds a spine.
    result_slot: Option<Value>,
}

/// The active tail-call loop being translated.
struct LoopCtx {
    /// The loop header (the `Recur` back-edge target).
    header: Block,
    /// The loop exit, taking the result as its block parameter.
    exit: Block,
    /// The loop-carried locals, reassigned (in order) by each `Recur`.
    params: Vec<LocalId>,
}

impl<M: Module> Translator<'_, M> {
    fn ptr(&self) -> types::Type {
        types::I64
    }

    fn var(&mut self, local: LocalId) -> Variable {
        let key = local.index();
        if let Some(v) = self.vars.get(&key) {
            return *v;
        }
        // A monomorphic scalar `Float` local is an unboxed `f64`; every other local
        // is a tagged 64-bit word.
        let ty = if self.f64_locals.contains(&key) { types::F64 } else { types::I64 };
        let var = self.builder.declare_var(ty);
        self.vars.insert(key, var);
        var
    }

    /// Whether `local` is represented as an unboxed `f64` (see [`Self::f64_locals`]).
    fn is_f64_local(&self, local: LocalId) -> bool {
        self.f64_locals.contains(&local.index())
    }

    /// Records which locals are unboxed `f64`s: a local is unboxed only when
    /// **every** observation of its type is a scalar `Float`. A local seen with
    /// both `Float` and another (or unknown) type — e.g. a contract binder
    /// destructured from a packed tuple via a synthesized, untyped projection but
    /// used as a `Float` in the body — stays boxed, so its variable type and the
    /// value bound into it agree. (The entry's unboxed-`Float` parameters are added
    /// by the raw calling convention; see where the parameter ABI is consulted.)
    fn collect_f64_locals(&mut self, body: &CExpr) {
        let mut float_seen = FxHashSet::default();
        let mut other_seen = FxHashSet::default();
        collect_float_observations(body, &mut float_seen, &mut other_seen);
        self.f64_locals = float_seen.difference(&other_seen).copied().collect();
    }

    /// Whether the Cranelift value `v` is an unboxed `f64`.
    fn is_f64(&self, v: Value) -> bool {
        self.builder.func.dfg.value_type(v) == types::F64
    }

    /// Reinterprets an `f64`'s bits as an `i64` (no conversion).
    fn f64_to_i64(&mut self, f: Value) -> Value {
        self.builder.ins().bitcast(types::I64, MemFlags::new(), f)
    }

    /// Reinterprets an `i64`'s bits as an `f64` (no conversion).
    fn i64_to_f64(&mut self, bits: Value) -> Value {
        self.builder.ins().bitcast(types::F64, MemFlags::new(), bits)
    }

    /// Boxes an unboxed `f64` as a heap `Float` value (a tagged pointer).
    fn box_float(&mut self, f: Value) -> Value {
        let bits = self.f64_to_i64(f);
        self.call1("fai_box_float", bits)
    }

    /// Reads a boxed `Float`'s `f64` value **without** releasing the box (the
    /// caller does not own it — its owner drops it later).
    fn borrowing_unbox(&mut self, boxed: Value) -> Value {
        let off = i32::try_from(rt::FLOAT_VALUE_OFFSET).expect("float value offset");
        let bits = self.builder.ins().load(types::I64, MemFlags::trusted(), boxed, off);
        self.i64_to_f64(bits)
    }

    /// Reads a boxed `Float`'s `f64` value and **releases** the box (the caller
    /// owns it: an `apply_n`/generic-call result, a forced `Float` global, or an
    /// owned boxed parameter), so the unboxed value carries no reference count.
    fn owning_unbox(&mut self, boxed: Value) -> Value {
        let f = self.borrowing_unbox(boxed);
        self.call_drop(boxed);
        f
    }

    /// Coerces a value into the uniform boxed/immediate representation: an unboxed
    /// `f64` is boxed; anything else is already a tagged word.
    fn ensure_boxed(&mut self, v: Value) -> Value {
        if self.is_f64(v) { self.box_float(v) } else { v }
    }

    /// Evaluates `e` and coerces the result into the uniform representation, for a
    /// value flowing into a boxed slot (a data field, environment, or argument).
    fn expr_boxed(&mut self, e: &CExpr) -> Value {
        let v = self.expr(e);
        self.ensure_boxed(v)
    }

    /// Coerces a function body's result for the uniform (boxed) ABI: an unboxed
    /// `f64` result is boxed. (The raw-bits return ABI overrides this where it
    /// applies; see where the return ABI is consulted.)
    fn boxed_return(&mut self, v: Value) -> Value {
        self.ensure_boxed(v)
    }

    /// Coerces a function body's result to an unboxed `f64`, for the register float
    /// return ABI: an unboxed `f64` passes through; a boxed `Float` (the uniform
    /// fallback, e.g. a mutual-recursion member wrapper's combined-call result) is
    /// unboxed first, releasing the box.
    fn f64_return(&mut self, v: Value) -> Value {
        if self.is_f64(v) { v } else { self.owning_unbox(v) }
    }

    /// Coerces a function body's result for the raw-bits float return ABI (a
    /// uniform entry: row-polymorphic or nullary): the unboxed `f64`
    /// ([`Self::f64_return`]) bit-reinterpreted to `i64`.
    fn raw_float_return(&mut self, v: Value) -> Value {
        let f = self.f64_return(v);
        self.f64_to_i64(f)
    }

    fn define_var(&mut self, local: LocalId, value: Value) {
        let var = self.var(local);
        self.builder.def_var(var, value);
    }

    fn use_var(&mut self, local: LocalId) -> Value {
        let var = self.var(local);
        self.builder.use_var(var)
    }

    /// `local`'s known static type, if recorded (see the `var_tys` pre-pass).
    fn var_ty(&self, local: LocalId) -> Option<&Ty> {
        self.var_tys.get(&local.index())
    }

    /// How to inline a `Dup` of `local`: from its static type ([`dup_class`]), or
    /// a tag-checked increment when the type is unknown (the safe default).
    fn dup_plan(&self, local: LocalId) -> DupPlan {
        match self.var_ty(local) {
            Some(ty) => dup_class(ty),
            None => DupPlan::Incr { tag_check: true },
        }
    }

    /// How to inline a `Drop` of `local`: from its static type ([`drop_class`]),
    /// or the runtime drop when the type is unknown (the polymorphic fallback).
    fn drop_plan(&self, local: LocalId) -> DropPlan {
        match self.var_ty(local) {
            Some(ty) => drop_class(ty),
            None => DropPlan::Runtime,
        }
    }

    /// Loads `base[index]` (a tagged word).
    fn load_slot(&mut self, base: Value, index: usize) -> Value {
        let offset = i32::try_from(index * 8).expect("slot offset");
        self.builder.ins().load(types::I64, MemFlags::trusted(), base, offset)
    }

    /// A reference to a runtime function (cached per translation).
    fn runtime(&mut self, name: &'static str, params: usize, returns: bool) -> FuncRef {
        if let Some(r) = self.runtime.get(name) {
            return *r;
        }
        let mut sig = self.module.make_signature();
        for _ in 0..params {
            sig.params.push(AbiParam::new(types::I64));
        }
        if returns {
            sig.returns.push(AbiParam::new(types::I64));
        }
        let id =
            self.module.declare_function(name, Linkage::Import, &sig).expect("declare runtime");
        let r = self.module.declare_func_in_func(id, self.builder.func);
        self.runtime.insert(name, r);
        r
    }

    fn call1(&mut self, name: &'static str, a: Value) -> Value {
        let f = self.runtime(name, 1, true);
        let call = self.builder.ins().call(f, &[a]);
        self.builder.inst_results(call)[0]
    }

    fn call_drop(&mut self, value: Value) {
        let f = self.runtime("fai_drop", 1, false);
        self.builder.ins().call(f, &[value]);
    }

    /// Duplicates `local` (increments its reference count) inline, per its
    /// [`DupPlan`]: an immediate is a no-op, an always-boxed value increments
    /// unconditionally, and any other value guards the increment with a tag-check.
    fn dup_local(&mut self, local: LocalId) {
        match self.dup_plan(local) {
            DupPlan::NoOp => {}
            DupPlan::Incr { tag_check } => self.emit_rc_incr(local, tag_check),
        }
    }

    /// Releases `local` at its last use, per its [`DropPlan`]: a no-op for an
    /// immediate, an unrolled release for a fixed-shape cell, a direct free for a
    /// boxed leaf, a runtime child-release for other data, and the runtime drop as
    /// the fallback for an unknown type.
    fn drop_local(&mut self, local: LocalId) {
        match self.drop_plan(local) {
            DropPlan::NoOp => {}
            DropPlan::Fixed(fields) => self.emit_inline_drop(local, &fields),
            DropPlan::Leaf { tag_check } => {
                self.emit_rc_dec_then(local, tag_check, |s, cell| {
                    // A leaf (Int/Float/String) has no reference-counted children,
                    // so a dead one is reclaimed directly.
                    let free = s.runtime("fai_free", 1, false);
                    s.builder.ins().call(free, &[cell]);
                });
            }
            DropPlan::Data { tag_check } => {
                self.emit_rc_dec_then(local, tag_check, |s, cell| {
                    // A variable-shape cell's children are recovered from its
                    // descriptor and released by the runtime (iteratively, so a
                    // deep structure never overflows the native stack).
                    let f = s.runtime("fai_drop_dead", 1, false);
                    s.builder.ins().call(f, &[cell]);
                });
            }
            DropPlan::Runtime => {
                let v = self.use_var(local);
                self.call_drop(v);
            }
        }
    }

    /// Emits an in-place reference-count increment of `local`. When `tag_check`,
    /// an immediate value (low bit set) skips the increment; an always-boxed type
    /// omits the guard entirely. Leaves the builder in the continuation block.
    fn emit_rc_incr(&mut self, local: LocalId, tag_check: bool) {
        let cell = self.use_var(local);
        let rc_off = i32::try_from(rt::RC_OFFSET).expect("rc offset");
        if !tag_check {
            let rc = self.builder.ins().load(types::I64, MemFlags::trusted(), cell, rc_off);
            let inc = self.builder.ins().iadd_imm(rc, 1);
            self.builder.ins().store(MemFlags::trusted(), inc, cell, rc_off);
            return;
        }
        let incr_b = self.builder.create_block();
        let cont_b = self.builder.create_block();
        // An immediate has its low bit set; a boxed value has it clear.
        let bit = self.builder.ins().band_imm(cell, 1);
        self.builder.ins().brif(bit, cont_b, &[], incr_b, &[]);

        self.builder.switch_to_block(incr_b);
        self.builder.seal_block(incr_b);
        let rc = self.builder.ins().load(types::I64, MemFlags::trusted(), cell, rc_off);
        let inc = self.builder.ins().iadd_imm(rc, 1);
        self.builder.ins().store(MemFlags::trusted(), inc, cell, rc_off);
        self.builder.ins().jump(cont_b, &[]);

        self.builder.switch_to_block(cont_b);
        self.builder.seal_block(cont_b);
    }

    /// Emits the inlined decrement of a boxed value `local` and runs `dead` (which
    /// reclaims the cell) when the count reaches zero. When `tag_check`, an
    /// immediate value skips the whole sequence; an always-boxed type omits the
    /// guard. Leaves the builder in the continuation block.
    fn emit_rc_dec_then(
        &mut self,
        local: LocalId,
        tag_check: bool,
        dead: impl FnOnce(&mut Self, Value),
    ) {
        let cell = self.use_var(local);
        let cont_b = self.builder.create_block();
        let dead_b = self.builder.create_block();

        // Optional immediate guard: an immediate (low bit set) carries no count.
        if tag_check {
            let dec_b = self.builder.create_block();
            let bit = self.builder.ins().band_imm(cell, 1);
            self.builder.ins().brif(bit, cont_b, &[], dec_b, &[]);
            self.builder.switch_to_block(dec_b);
            self.builder.seal_block(dec_b);
        }

        // Decrement the reference count in place, then branch on whether dead.
        let rc_off = i32::try_from(rt::RC_OFFSET).expect("rc offset");
        let rc = self.builder.ins().load(types::I64, MemFlags::trusted(), cell, rc_off);
        let dec = self.builder.ins().iadd_imm(rc, -1);
        self.builder.ins().store(MemFlags::trusted(), dec, cell, rc_off);
        let is_dead = self.builder.ins().icmp_imm(IntCC::Equal, dec, 0);
        self.builder.ins().brif(is_dead, dead_b, &[], cont_b, &[]);

        self.builder.switch_to_block(dead_b);
        self.builder.seal_block(dead_b);
        dead(self, cell);
        self.builder.ins().jump(cont_b, &[]);

        self.builder.switch_to_block(cont_b);
        self.builder.seal_block(cont_b);
    }

    /// Inlines the release of a known monomorphic, fixed-shape data cell (`local`,
    /// a boxed closed-record or tuple value with the given per-field drop classes,
    /// always boxed so no tag-check is needed): when its count reaches zero,
    /// release each boxed field at its constant offset (immediate fields are a
    /// no-op, so they are omitted) and free the cell directly.
    ///
    /// Releasing each boxed child through `fai_drop` (rather than recursing the
    /// inlining) keeps deep structures iterative and the emitted code small. The
    /// cell is freed last: the heap is acyclic, so dropping a child can never
    /// reach the parent, and the field pointers are loaded before the free.
    fn emit_inline_drop(&mut self, local: LocalId, fields: &[FieldDrop]) {
        self.emit_rc_dec_then(local, false, |s, cell| {
            for (i, class) in fields.iter().enumerate() {
                if matches!(class, FieldDrop::Boxed) {
                    let off = i32::try_from(rt::DATA_FIELDS_OFFSET + i * 8).expect("field offset");
                    let field = s.builder.ins().load(types::I64, MemFlags::trusted(), cell, off);
                    s.call_drop(field);
                }
            }
            let free = s.runtime("fai_free", 1, false);
            s.builder.ins().call(free, &[cell]);
        });
    }

    fn expr(&mut self, e: &CExpr) -> Value {
        match &e.kind {
            ExprKind::Lit(lit) => self.literal(lit),
            ExprKind::Local(local) => self.use_var(*local),
            ExprKind::Global(def) => self.global_value(*def, &e.ty),
            ExprKind::Prim { op, args } => self.prim(*op, args, &e.ty),
            ExprKind::App { func, args } => self.application(func, args, &e.ty),
            ExprKind::If { cond, then, els } => self.conditional(cond, then, els),
            ExprKind::Let { local, value, body } => {
                let v = self.expr(value);
                self.define_var(*local, v);
                self.expr(body)
            }
            ExprKind::MakeClosure { func, captures } => self.make_closure(*func, captures),
            ExprKind::MakeData { tag, args, reuse } => self.make_data(*tag, args, *reuse),
            ExprKind::DataTag(base) => {
                let v = self.expr(base);
                self.call1("fai_data_tag", v)
            }
            ExprKind::DataField { base, index } => self.data_field(base, *index, &e.ty),
            ExprKind::Reset { value, token, body } => {
                let v = self.expr(value);
                let tok = self.call1("fai_drop_reuse", v);
                self.define_var(*token, tok);
                self.expr(body)
            }
            ExprKind::Dup { local, body } => {
                // The dup is inlined per the local's static type; see `dup_local`
                // (immediate no-op / unconditional / tag-checked increment).
                self.dup_local(*local);
                self.expr(body)
            }
            ExprKind::Drop { local, body } => {
                let result = self.expr(body);
                // The drop follows the body (its last use); see `drop_local` for
                // the immediate / inlined-cell / runtime-dispatch choice.
                self.drop_local(*local);
                result
            }
            ExprKind::Join { params, body } => self.join(params, body, &e.ty),
            ExprKind::HoleStart { hole, body } => {
                // The result slot holds the head of the spine being built; the hole
                // (destination) starts pointing at it. Both live for the loop.
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    3,
                ));
                let ptr = self.ptr();
                let addr = self.builder.ins().stack_addr(ptr, slot, 0);
                self.result_slot = Some(addr);
                self.define_var(*hole, addr);
                self.expr(body)
            }
            ExprKind::HoleFill { hole, cell, field } => self.hole_fill(*hole, cell, *field),
            // `Recur`/`HoleClose` are terminal and only appear in a `Join` body,
            // translated through `expr_tail`.
            ExprKind::Recur { .. } | ExprKind::HoleClose { .. } => {
                unreachable!("tail-only node reached non-tail code generation")
            }
            // Unreachable for a build that passed the FAI7001 check; yield Unit.
            ExprKind::Error => self.builder.ins().iconst(types::I64, rt::FAI_UNIT),
        }
    }

    fn literal(&mut self, lit: &Lit) -> Value {
        match lit {
            Lit::Int(n) => {
                if fits_immediate(*n) {
                    self.builder.ins().iconst(types::I64, (n << 1) | 1)
                } else {
                    let raw = self.builder.ins().iconst(types::I64, *n);
                    self.call1("fai_box_int", raw)
                }
            }
            Lit::Bool(b) => {
                let v = if *b { 3 } else { 1 };
                self.builder.ins().iconst(types::I64, v)
            }
            Lit::Float(bits) => {
                // A monomorphic `Float` literal is an unboxed `f64`; it is boxed
                // only if it flows into a uniform slot (handled at that boundary).
                self.builder.ins().f64const(Ieee64::with_bits(*bits))
            }
            Lit::Char(c) => {
                // A code point is an immediate, tagged like `Int`/`Bool`: it
                // always fits the 63-bit payload, so no boxing is needed.
                self.builder.ins().iconst(types::I64, ((*c as i64) << 1) | 1)
            }
            Lit::Unit => self.builder.ins().iconst(types::I64, rt::FAI_UNIT),
            Lit::Str(bytes) => self.string_literal(bytes),
        }
    }

    /// Builds a data value: a nullary constructor is an immediate carrying its
    /// tag; an n-ary one builds `{ tag, fields… }` via the runtime — into a reuse
    /// token's memory in place when one is supplied (and the right size), else
    /// freshly allocated. The reuse pass never attaches a token to a nullary
    /// constructor (which allocates nothing).
    fn make_data(&mut self, tag: u32, args: &[CExpr], reuse: Option<LocalId>) -> Value {
        if args.is_empty() {
            debug_assert!(reuse.is_none(), "nullary constructor cannot reuse a cell");
            let imm = (i64::from(tag) << 1) | 1;
            return self.builder.ins().iconst(types::I64, imm);
        }
        // Data fields are uniform `i64` slots, so a float field is boxed in.
        let vals: Vec<Value> = args.iter().map(|a| self.expr_boxed(a)).collect();
        let count = vals.len();
        let ptr = self.spill(&vals);
        let tag_v = self.builder.ins().iconst(types::I64, i64::from(tag));
        let n_v = self.builder.ins().iconst(types::I64, count as i64);
        match reuse {
            Some(token) => {
                let tok = self.use_var(token);
                let f = self.runtime("fai_reuse", 4, true);
                let call = self.builder.ins().call(f, &[tok, tag_v, n_v, ptr]);
                self.builder.inst_results(call)[0]
            }
            None => {
                let f = self.runtime("fai_make_data", 3, true);
                let call = self.builder.ins().call(f, &[tag_v, n_v, ptr]);
                self.builder.inst_results(call)[0]
            }
        }
    }

    /// Projects a field of a data value (consuming `base`). A constant slot is an
    /// immediate; a row-polymorphic slot is `base + evidence` computed at runtime
    /// from a leading offset-evidence parameter.
    fn data_field(&mut self, base: &CExpr, index: FieldIndex, result_ty: &Ty) -> Value {
        // A scalar `Float` field is read **unboxed**: the field box's bits are read
        // in place without duplicating it (a borrow — `base` outlives the read in
        // A-normal form, and dropping `base` later releases the field box once).
        if matches!(result_ty, Ty::Con(Con::Float)) {
            return self.float_data_field(base, index);
        }
        let v = self.expr(base);
        let idx = match index {
            FieldIndex::Const(n) => self.builder.ins().iconst(types::I64, i64::from(n)),
            FieldIndex::Dyn { base: off, evidence } => {
                // Evidence is an immediate `Int` local; read it (a borrow), strip
                // the tag, and add the statically known preceding-field count.
                let ev = self.use_var(evidence);
                let unboxed = self.builder.ins().sshr_imm(ev, 1);
                self.builder.ins().iadd_imm(unboxed, i64::from(off))
            }
        };
        let f = self.runtime("fai_data_field", 2, true);
        let call = self.builder.ins().call(f, &[v, idx]);
        self.builder.inst_results(call)[0]
    }

    /// Reads a scalar `Float` field as an unboxed `f64`: load the field slot's box
    /// pointer at its (constant or evidence-computed) offset, then read the box's
    /// bits without touching its reference count (a borrow).
    fn float_data_field(&mut self, base: &CExpr, index: FieldIndex) -> Value {
        let base_v = self.expr(base);
        let fields_off = i64::try_from(rt::DATA_FIELDS_OFFSET).expect("fields offset");
        let boxed = match index {
            FieldIndex::Const(n) => {
                let off =
                    i32::try_from(rt::DATA_FIELDS_OFFSET + n as usize * 8).expect("field off");
                self.builder.ins().load(types::I64, MemFlags::trusted(), base_v, off)
            }
            FieldIndex::Dyn { base: off, evidence } => {
                let ev = self.use_var(evidence);
                let slot = self.builder.ins().sshr_imm(ev, 1);
                let slot = self.builder.ins().iadd_imm(slot, i64::from(off));
                let byte = self.builder.ins().imul_imm(slot, 8);
                let byte = self.builder.ins().iadd_imm(byte, fields_off);
                let addr = self.builder.ins().iadd(base_v, byte);
                self.builder.ins().load(types::I64, MemFlags::trusted(), addr, 0)
            }
        };
        self.borrowing_unbox(boxed)
    }

    /// Emits an immortal static `String` object and yields its address.
    fn string_literal(&mut self, bytes: &[u8]) -> Value {
        let name = format!("{}__fn{}__str{}", self.base, self.fn_index, self.string_counter);
        self.string_counter += 1;
        let total = rt::STRING_BYTES_OFFSET + bytes.len();
        let mut data = vec![0u8; total];
        data[rt::RC_OFFSET..rt::RC_OFFSET + 8].copy_from_slice(&rt::IMMORTAL_RC.to_le_bytes());
        data[rt::SIZE_OFFSET..rt::SIZE_OFFSET + 8].copy_from_slice(&(total as u64).to_le_bytes());
        data[rt::STRING_LEN_OFFSET..rt::STRING_LEN_OFFSET + 8]
            .copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        data[rt::STRING_BYTES_OFFSET..].copy_from_slice(bytes);

        // Writable: although immortal, `fai_drop` still decrements the reference
        // count in place, so the object must live in writable memory.
        let data_id =
            self.module.declare_data(&name, Linkage::Local, true, false).expect("declare string");
        let mut desc = DataDescription::new();
        desc.define(data.into_boxed_slice());
        desc.set_align(8); // a String value is a tagged pointer; keep the low bits clear.
        let desc_gv = declare_descriptor_in_data(self.module, &mut desc, "FAI_STRING_DESC");
        desc.write_data_addr(rt::DESC_OFFSET as u32, desc_gv, 0);
        self.module.define_data(data_id, &desc).expect("define string");

        let ptr = self.ptr();
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        self.builder.ins().symbol_value(ptr, gv)
    }

    /// A reference to a definition as a value: the static closure for a function,
    /// or — for a zero-arity binding (a value, not a function) — its forced
    /// result (applying the closure to no arguments).
    fn global_value(&mut self, def: DefId, result_ty: &Ty) -> Value {
        let name = closure_symbol(self.namer, def);
        let data_id =
            self.module.declare_data(&name, Linkage::Import, false, false).expect("declare global");
        let ptr = self.ptr();
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        let closure = self.builder.ins().symbol_value(ptr, gv);
        if (self.arity_of)(def) == 0 {
            // Force the value binding: apply the closure to zero arguments.
            let null = self.builder.ins().iconst(types::I64, 0);
            let argc = self.builder.ins().iconst(types::I64, 0);
            let f = self.runtime("fai_apply_n", 3, true);
            let call = self.builder.ins().call(f, &[closure, argc, null]);
            let forced = self.builder.inst_results(call)[0];
            // A forced `Float` value binding comes back boxed and owned; unbox it.
            if matches!(result_ty, Ty::Con(Con::Float)) {
                self.owning_unbox(forced)
            } else {
                forced
            }
        } else {
            closure
        }
    }

    fn prim(&mut self, op: Prim, args: &[CExpr], result_ty: &Ty) -> Value {
        // Float primitives compile to inline machine `f64` ops when their operands
        // are unboxed; a boxed operand (e.g. inside the uniform mutual-recursion
        // combined function) falls back to the out-of-line runtime float call.
        if let Some(v) = self.float_prim(op, args, result_ty) {
            return v;
        }
        // The hot integer/boolean primitives compile to inline machine code with
        // an immediate fast path; everything else — and the boxed/overflow cases
        // of those — falls through to the out-of-line runtime call below.
        if let Some(v) = self.inline_prim(op, args) {
            return v;
        }
        // A float operand of a non-float primitive (e.g. `{ r with x = … }`'s new
        // value) crosses a uniform `i64` boundary, so it is boxed in.
        let vals: Vec<Value> = args.iter().map(|a| self.expr_boxed(a)).collect();
        // An inspect-only primitive whose operands reference counting lent (boxed,
        // reference-counted values) calls the non-consuming runtime variant — the
        // caller drops them at their last use. The borrow decision is the same one
        // `fai-rc` made (`Prim::borrows_operand` on the operand type), so the
        // emitted drops and the runtime's (non-)consumption agree.
        let symbol = match op.borrowed_runtime_symbol() {
            Some(borrowed) if args.first().is_some_and(|a| op.borrows_operand(&a.ty)) => borrowed,
            _ => op.runtime_symbol(),
        };
        // Every primitive (including `Console.writeLine`, which yields Unit)
        // returns a value.
        let f = self.runtime(symbol, op.arity(), true);
        let call = self.builder.ins().call(f, &vals);
        self.builder.inst_results(call)[0]
    }

    /// Compiles a `Float` primitive. With unboxed `f64` operands these are inline
    /// machine instructions (`fadd`, `fcmp`, `sqrt`, `fcvt*`, bit reinterpretation)
    /// with no allocation; a boxed operand or result (the uniform fallback, e.g.
    /// the mutual-recursion combined function) routes to the out-of-line runtime
    /// float call instead. Returns `None` for non-float primitives.
    fn float_prim(&mut self, op: Prim, args: &[CExpr], result_ty: &Ty) -> Option<Value> {
        Some(match op {
            Prim::FloatAdd => self.float_binop(op, args, FloatBinop::Add),
            Prim::FloatSub => self.float_binop(op, args, FloatBinop::Sub),
            Prim::FloatMul => self.float_binop(op, args, FloatBinop::Mul),
            Prim::FloatDiv => self.float_binop(op, args, FloatBinop::Div),
            Prim::FloatLt => self.float_compare_op(op, args, FloatCC::LessThan),
            Prim::FloatLe => self.float_compare_op(op, args, FloatCC::LessThanOrEqual),
            Prim::FloatGt => self.float_compare_op(op, args, FloatCC::GreaterThan),
            Prim::FloatGe => self.float_compare_op(op, args, FloatCC::GreaterThanOrEqual),
            Prim::Sqrt => self.float_sqrt(op, args),
            Prim::IntToFloat => self.int_to_float(op, args, result_ty),
            Prim::FloatToInt => self.float_to_int(op, args),
            Prim::FloatFromBits => self.float_from_bits(op, args, result_ty),
            Prim::FloatToBits => self.float_to_bits(op, args),
            Prim::FloatToString => self.float_to_string(op, args),
            _ => return None,
        })
    }

    /// `+ - * /` on `Float`: inline `f64` arithmetic on unboxed operands; the
    /// runtime float op on boxed operands.
    fn float_binop(&mut self, op: Prim, args: &[CExpr], bop: FloatBinop) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        if self.is_f64(a) && self.is_f64(b) {
            match bop {
                FloatBinop::Add => self.builder.ins().fadd(a, b),
                FloatBinop::Sub => self.builder.ins().fsub(a, b),
                FloatBinop::Mul => self.builder.ins().fmul(a, b),
                FloatBinop::Div => self.builder.ins().fdiv(a, b),
            }
        } else {
            let a = self.ensure_boxed(a);
            let b = self.ensure_boxed(b);
            self.prim_runtime_call(op, &[a, b])
        }
    }

    /// `< <= > >=` on `Float`: inline `fcmp` (tagged `Bool`) on unboxed operands;
    /// the runtime float comparison on boxed operands.
    fn float_compare_op(&mut self, op: Prim, args: &[CExpr], cc: FloatCC) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        if self.is_f64(a) && self.is_f64(b) {
            let c = self.builder.ins().fcmp(cc, a, b);
            self.tag_bool(c)
        } else {
            let a = self.ensure_boxed(a);
            let b = self.ensure_boxed(b);
            self.prim_runtime_call(op, &[a, b])
        }
    }

    /// `sqrt`: inline on an unboxed operand; the runtime `fai_sqrt` on a boxed one.
    fn float_sqrt(&mut self, op: Prim, args: &[CExpr]) -> Value {
        let a = self.expr(&args[0]);
        if self.is_f64(a) {
            self.builder.ins().sqrt(a)
        } else {
            let a = self.ensure_boxed(a);
            self.prim_runtime_call(op, &[a])
        }
    }

    /// `Int.toFloat`: inline `fcvt_from_sint` to an unboxed `f64` when the result
    /// is unboxed; otherwise the runtime `fai_int_to_float` (boxed result).
    fn int_to_float(&mut self, op: Prim, args: &[CExpr], result_ty: &Ty) -> Value {
        if matches!(result_ty, Ty::Con(Con::Float)) {
            let n = self.expr(&args[0]);
            let raw = self.unbox_int_to_raw(n);
            self.builder.ins().fcvt_from_sint(types::F64, raw)
        } else {
            let n = self.expr_boxed(&args[0]);
            self.prim_runtime_call(op, &[n])
        }
    }

    /// `Float.toInt` (truncating, saturating like the runtime's `as i64`): inline
    /// `fcvt_to_sint_sat` on an unboxed operand; the runtime call on a boxed one.
    fn float_to_int(&mut self, op: Prim, args: &[CExpr]) -> Value {
        let a = self.expr(&args[0]);
        if self.is_f64(a) {
            let raw = self.builder.ins().fcvt_to_sint_sat(types::I64, a);
            self.box_or_tag_int(raw)
        } else {
            let a = self.ensure_boxed(a);
            self.prim_runtime_call(op, &[a])
        }
    }

    /// `Float.fromBits`: reinterpret an `Int`'s bits as an unboxed `f64` when the
    /// result is unboxed; otherwise the runtime call (boxed result).
    fn float_from_bits(&mut self, op: Prim, args: &[CExpr], result_ty: &Ty) -> Value {
        if matches!(result_ty, Ty::Con(Con::Float)) {
            let n = self.expr(&args[0]);
            let raw = self.unbox_int_to_raw(n);
            self.i64_to_f64(raw)
        } else {
            let n = self.expr_boxed(&args[0]);
            self.prim_runtime_call(op, &[n])
        }
    }

    /// `Float.toBits`: reinterpret an unboxed `f64`'s bits as an `Int`; the runtime
    /// call on a boxed operand.
    fn float_to_bits(&mut self, op: Prim, args: &[CExpr]) -> Value {
        let a = self.expr(&args[0]);
        if self.is_f64(a) {
            let raw = self.f64_to_i64(a);
            self.box_or_tag_int(raw)
        } else {
            let a = self.ensure_boxed(a);
            self.prim_runtime_call(op, &[a])
        }
    }

    /// `Float.toString`: the runtime renderer takes a boxed `Float`, so an unboxed
    /// operand is boxed in (a cold path — rendering allocates anyway).
    fn float_to_string(&mut self, op: Prim, args: &[CExpr]) -> Value {
        let a = self.expr_boxed(&args[0]);
        self.prim_runtime_call(op, &[a])
    }

    /// Reads an `Int` operand's raw 64-bit value, consuming it: an immediate is
    /// untagged; a boxed (large) `Int` is read from its cell and released.
    fn unbox_int_to_raw(&mut self, n: Value) -> Value {
        let imm_b = self.builder.create_block();
        let box_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);
        let bit = self.builder.ins().band_imm(n, 1);
        self.builder.ins().brif(bit, imm_b, &[], box_b, &[]);

        self.builder.switch_to_block(imm_b);
        self.builder.seal_block(imm_b);
        let imm = self.builder.ins().sshr_imm(n, 1);
        self.builder.ins().jump(merge_b, &[imm.into()]);

        self.builder.switch_to_block(box_b);
        self.builder.seal_block(box_b);
        let off = i32::try_from(rt::INT_VALUE_OFFSET).expect("int value offset");
        let val = self.builder.ins().load(types::I64, MemFlags::trusted(), n, off);
        self.call_drop(n);
        self.builder.ins().jump(merge_b, &[val.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// Tags a raw `i64` as an immediate `Int` when it fits the 63-bit range, else
    /// boxes it (mirroring the runtime's `fai_box_int` boundary).
    fn box_or_tag_int(&mut self, raw: Value) -> Value {
        let box_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);
        // `raw << 1` overflows exactly when `raw` no longer fits the immediate.
        let (shifted, overflow) = self.builder.ins().sadd_overflow(raw, raw);
        let tagged = self.builder.ins().bor_imm(shifted, 1);
        self.builder.ins().brif(overflow, box_b, &[], merge_b, &[tagged.into()]);

        self.builder.switch_to_block(box_b);
        self.builder.seal_block(box_b);
        let boxed = self.call1("fai_box_int", raw);
        self.builder.ins().jump(merge_b, &[boxed.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// Compiles an integer/boolean primitive to inline machine code when its
    /// operands are immediates, or returns `None` for the primitives that stay
    /// out-of-line runtime calls (the float operations, structural/string
    /// operations on boxed values, capabilities).
    ///
    /// The fast path mirrors the runtime (`unbox_int` / operate / `fai_box_int`):
    /// it untags the operands, runs the native operation, and re-tags — branching
    /// to the same runtime call as a fallback whenever an operand is boxed (a
    /// large `Int`) or the result no longer fits the 63-bit immediate. Operands
    /// are evaluated **once, up front, in source order**; the fast and fallback
    /// paths reuse those values.
    ///
    /// In the fast path both operands are immediates, so the operand drops the
    /// primitive would otherwise perform are no-ops and are correctly omitted; a
    /// boxed operand always takes the fallback, which consumes it. Equality and
    /// ordering are inlined only for immediate-representable operand types; other
    /// types keep the structural runtime path (including its operand borrowing).
    fn inline_prim(&mut self, op: Prim, args: &[CExpr]) -> Option<Value> {
        match op {
            Prim::IntAdd => Some(self.inline_arith(op, args, FitsOp::Add)),
            Prim::IntSub => Some(self.inline_arith(op, args, FitsOp::Sub)),
            Prim::IntMul => Some(self.inline_arith(op, args, FitsOp::Mul)),
            Prim::IntDiv => Some(self.inline_divrem(op, args, true)),
            Prim::IntRem => Some(self.inline_divrem(op, args, false)),
            Prim::IntShl => Some(self.inline_arith(op, args, FitsOp::Shl)),
            Prim::IntShr => Some(self.inline_arith(op, args, FitsOp::Shr)),
            Prim::IntShrLogical => Some(self.inline_arith(op, args, FitsOp::Ushr)),
            Prim::IntAnd => Some(self.inline_bitwise(op, args, BitOp::And)),
            Prim::IntOr => Some(self.inline_bitwise(op, args, BitOp::Or)),
            Prim::IntXor => Some(self.inline_bitwise(op, args, BitOp::Xor)),
            Prim::IntComplement => Some(self.inline_complement(op, args)),
            Prim::IntLt => Some(self.inline_cmp(op, args, IntCC::SignedLessThan)),
            Prim::IntLe => Some(self.inline_cmp(op, args, IntCC::SignedLessThanOrEqual)),
            Prim::IntGt => Some(self.inline_cmp(op, args, IntCC::SignedGreaterThan)),
            Prim::IntGe => Some(self.inline_cmp(op, args, IntCC::SignedGreaterThanOrEqual)),
            Prim::Not => Some(self.inline_not(args)),
            Prim::Eq => self.inline_eq(op, args),
            Prim::Compare => self.inline_compare(op, args),
            _ => None,
        }
    }

    /// Strips the immediate tag: an immediate `Int`/`Char`/`Bool` is encoded as
    /// `value << 1 | 1`, so the value is an arithmetic right shift by one.
    fn untag(&mut self, v: Value) -> Value {
        self.builder.ins().sshr_imm(v, 1)
    }

    /// Re-applies the immediate `Int` tag to a raw value (`value << 1 | 1`).
    fn tag_int(&mut self, raw: Value) -> Value {
        let shifted = self.builder.ins().ishl_imm(raw, 1);
        self.builder.ins().bor_imm(shifted, 1)
    }

    /// Tags a Cranelift comparison result (an `I8` `0`/`1`) as a `Bool` immediate
    /// (`false` = `1`, `true` = `3`, i.e. `value << 1 | 1`).
    fn tag_bool(&mut self, cmp: Value) -> Value {
        let wide = self.builder.ins().uextend(types::I64, cmp);
        self.tag_int(wide)
    }

    /// The out-of-line runtime call for a primitive's fallback path (a boxed
    /// operand, or a result that overflowed the immediate). It consumes the
    /// operands exactly as the fast path's drops-omitted-on-immediates would.
    fn prim_runtime_call(&mut self, op: Prim, args: &[Value]) -> Value {
        let f = self.runtime(op.runtime_symbol(), args.len(), true);
        let call = self.builder.ins().call(f, args);
        self.builder.inst_results(call)[0]
    }

    /// Emits the immediate-operand guard. When `tagbits`'s low bit is set (every
    /// operand is an immediate), `fast` runs in a fast block — it produces the
    /// result and jumps to the returned merge block, and may itself branch to the
    /// slow block for a result that overflowed the immediate. Otherwise control
    /// falls to the slow block, which runs `fallback` (the runtime call). Returns
    /// the merged result value, leaving the builder in the merge block.
    fn guard_immediate(
        &mut self,
        tagbits: Value,
        fallback: impl FnOnce(&mut Self) -> Value,
        fast: impl FnOnce(&mut Self, Block, Block),
    ) -> Value {
        let fast_b = self.builder.create_block();
        let slow_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);

        let bit = self.builder.ins().band_imm(tagbits, 1);
        self.builder.ins().brif(bit, fast_b, &[], slow_b, &[]);

        self.builder.switch_to_block(fast_b);
        self.builder.seal_block(fast_b);
        fast(self, slow_b, merge_b);

        // `slow_b` is reached from the guard's else edge and, for an out-of-range
        // fast result, the fast block's branch — both now emitted, so it can seal.
        self.builder.switch_to_block(slow_b);
        self.builder.seal_block(slow_b);
        let res = fallback(self);
        self.builder.ins().jump(merge_b, &[res.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// Inlines an arithmetic or shift primitive: untag, native op, then re-tag
    /// guarded by a 63-bit fit check. `sadd_overflow(r, r)` computes `r << 1` and
    /// flags overflow exactly when `r` no longer fits the immediate (its top two
    /// bits differ) — the precise `fai_box_int` boundary — so an out-of-range
    /// result falls back to the runtime, which boxes it. The native multiply and
    /// shifts wrap like the runtime's `wrapping_mul` / masked shifts (Cranelift
    /// masks a dynamic shift amount modulo the 64-bit width, matching `& 63`).
    fn inline_arith(&mut self, op: Prim, args: &[CExpr], fop: FitsOp) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        let anded = self.builder.ins().band(a, b);
        self.guard_immediate(
            anded,
            |s| s.prim_runtime_call(op, &[a, b]),
            |s, slow, merge| {
                let xa = s.untag(a);
                let xb = s.untag(b);
                let r = match fop {
                    FitsOp::Add => s.builder.ins().iadd(xa, xb),
                    FitsOp::Sub => s.builder.ins().isub(xa, xb),
                    FitsOp::Mul => s.builder.ins().imul(xa, xb),
                    FitsOp::Shl => s.builder.ins().ishl(xa, xb),
                    FitsOp::Shr => s.builder.ins().sshr(xa, xb),
                    FitsOp::Ushr => s.builder.ins().ushr(xa, xb),
                };
                let (shifted, overflow) = s.builder.ins().sadd_overflow(r, r);
                let tagged = s.builder.ins().bor_imm(shifted, 1);
                s.builder.ins().brif(overflow, slow, &[], merge, &[tagged.into()]);
            },
        )
    }

    /// Inlines integer division (`is_div`) or remainder, choosing the shape from
    /// the divisor:
    ///
    /// * A **literal** divisor is always non-negative — a negation lowers to
    ///   `0 - n`, never a negative literal — so the cases below cover every
    ///   constant. `0` keeps the out-of-line runtime call, which raises the located
    ///   division-by-zero fault (a native `sdiv`/`srem` by zero would be a raw
    ///   hardware trap with no message). A positive power of two strength-reduces
    ///   to a shift; any other in-range positive constant divides with the native
    ///   op and **no** zero guard or fit check (the divisor is statically nonzero
    ///   and never `-1`, so the result always fits). Each constant path still
    ///   guards the dividend and falls back to the runtime for a boxed (large) one.
    /// * A **variable** divisor (or a constant too large to be immediate) takes the
    ///   general path: a both-operands-immediate guard, a zero-divisor branch to
    ///   the runtime fault, and — for division — the immediate fit check.
    fn inline_divrem(&mut self, op: Prim, args: &[CExpr], is_div: bool) -> Value {
        if let ExprKind::Lit(Lit::Int(d)) = args[1].kind {
            if d == 0 {
                // A literal zero divisor always faults; keep the out-of-line call
                // so the located message is raised rather than a hardware trap.
                let a = self.expr(&args[0]);
                let b = self.expr(&args[1]);
                return self.prim_runtime_call(op, &[a, b]);
            }
            if d >= 1 && fits_immediate(d) {
                if d > 1 && (d & (d - 1)) == 0 {
                    return self.inline_divrem_pow2(
                        op,
                        args,
                        is_div,
                        i64::from(d.trailing_zeros()),
                    );
                }
                return self.inline_divrem_const(op, args, is_div);
            }
        }
        self.inline_divrem_general(op, args, is_div)
    }

    /// The general division/remainder path (a variable divisor, or a constant too
    /// large to be immediate): a both-operands-immediate guard, then a zero-divisor
    /// branch to the runtime fault, the native `sdiv`/`srem`, and — for division —
    /// the immediate fit check. A boxed operand or a zero divisor takes the runtime
    /// fallback, which consumes both operands and raises the located fault on zero.
    fn inline_divrem_general(&mut self, op: Prim, args: &[CExpr], is_div: bool) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        let anded = self.builder.ins().band(a, b);
        self.guard_immediate(
            anded,
            |s| s.prim_runtime_call(op, &[a, b]),
            |s, slow, merge| {
                let xa = s.untag(a);
                let xb = s.untag(b);
                // Cranelift's sdiv/srem trap on a zero divisor, so the native op
                // must not see one: a zero divisor branches to the runtime call,
                // which raises the located division-by-zero fault.
                let nonzero = s.builder.create_block();
                let is_zero = s.builder.ins().icmp_imm(IntCC::Equal, xb, 0);
                s.builder.ins().brif(is_zero, slow, &[], nonzero, &[]);
                s.builder.switch_to_block(nonzero);
                s.builder.seal_block(nonzero);
                if is_div {
                    let r = s.builder.ins().sdiv(xa, xb);
                    // Immediate operands cannot reach sdiv's own INT_MIN/-1 overflow
                    // trap, but `(-2^62) / -1 = 2^62` overflows the immediate; the
                    // fit check routes that lone case to the fallback, which boxes it.
                    let (shifted, overflow) = s.builder.ins().sadd_overflow(r, r);
                    let tagged = s.builder.ins().bor_imm(shifted, 1);
                    s.builder.ins().brif(overflow, slow, &[], merge, &[tagged.into()]);
                } else {
                    let r = s.builder.ins().srem(xa, xb);
                    // `|a % b| < |b| <= 2^62`, so a remainder always fits; no check.
                    let tagged = s.tag_int(r);
                    s.builder.ins().jump(merge, &[tagged.into()]);
                }
            },
        )
    }

    /// Division/remainder by a constant, statically nonzero, in-range divisor that
    /// is not a power of two (handled by [`Self::inline_divrem_pow2`]). A literal
    /// divisor is non-negative, so it is never `0` (handled in [`Self::inline_divrem`])
    /// nor `-1`: with `|d| >= 1` the quotient and remainder always fit the
    /// immediate, so neither the zero guard nor the division fit check is needed.
    /// Only the dividend is guarded; a boxed dividend falls back to the runtime.
    fn inline_divrem_const(&mut self, op: Prim, args: &[CExpr], is_div: bool) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        self.guard_immediate(
            a,
            |s| s.prim_runtime_call(op, &[a, b]),
            |s, _slow, merge| {
                let xa = s.untag(a);
                // The divisor is a constant, so `sdiv`/`srem` strength-reduce in the
                // backend (e.g. a division by 3 becomes a multiply).
                let xb = s.untag(b);
                let r = if is_div {
                    s.builder.ins().sdiv(xa, xb)
                } else {
                    s.builder.ins().srem(xa, xb)
                };
                let tagged = s.tag_int(r);
                s.builder.ins().jump(merge, &[tagged.into()]);
            },
        )
    }

    /// Strength-reduces division/remainder by a constant power of two `2^k`
    /// (`k >= 1`) to a signed, truncating shift — no zero or overflow guard (the
    /// divisor is a known nonzero power of two and the result always fits). Only
    /// the dividend is guarded; a boxed dividend falls back to the runtime.
    ///
    /// Truncation toward zero needs a bias, since an arithmetic shift floors:
    /// `bias = (x < 0) ? 2^k - 1 : 0`, so `q = (x + bias) >> k` and the remainder
    /// is `x - (q << k)`. The bias is `(x >> 63) >>u (64 - k)` — all-ones to `2^k-1`
    /// for a negative `x`, zero otherwise.
    fn inline_divrem_pow2(&mut self, op: Prim, args: &[CExpr], is_div: bool, k: i64) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]); // the constant divisor; used only by the fallback
        self.guard_immediate(
            a,
            |s| s.prim_runtime_call(op, &[a, b]),
            |s, _slow, merge| {
                let x = s.untag(a);
                let sign = s.builder.ins().sshr_imm(x, 63);
                let bias = s.builder.ins().ushr_imm(sign, 64 - k);
                let adj = s.builder.ins().iadd(x, bias);
                let q = s.builder.ins().sshr_imm(adj, k);
                let r = if is_div {
                    q
                } else {
                    let qk = s.builder.ins().ishl_imm(q, k);
                    s.builder.ins().isub(x, qk)
                };
                let tagged = s.tag_int(r);
                s.builder.ins().jump(merge, &[tagged.into()]);
            },
        )
    }

    /// Inlines a bitwise `and`/`or`/`xor`: untag, native op, re-tag. The result of
    /// two immediates always fits the immediate (the operands' top two bits agree,
    /// so the result's do too), so no fit check is needed; a boxed operand falls
    /// back to the runtime.
    fn inline_bitwise(&mut self, op: Prim, args: &[CExpr], bop: BitOp) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        let anded = self.builder.ins().band(a, b);
        self.guard_immediate(
            anded,
            |s| s.prim_runtime_call(op, &[a, b]),
            |s, _slow, merge| {
                let xa = s.untag(a);
                let xb = s.untag(b);
                let r = match bop {
                    BitOp::And => s.builder.ins().band(xa, xb),
                    BitOp::Or => s.builder.ins().bor(xa, xb),
                    BitOp::Xor => s.builder.ins().bxor(xa, xb),
                };
                let tagged = s.tag_int(r);
                s.builder.ins().jump(merge, &[tagged.into()]);
            },
        )
    }

    /// Inlines bitwise `complement` (unary): untag, `bnot`, re-tag. `!x` of a
    /// 63-bit value is again 63-bit, so no fit check is needed; a boxed operand
    /// falls back to the runtime.
    fn inline_complement(&mut self, op: Prim, args: &[CExpr]) -> Value {
        let a = self.expr(&args[0]);
        self.guard_immediate(
            a,
            |s| s.prim_runtime_call(op, &[a]),
            |s, _slow, merge| {
                let xa = s.untag(a);
                let r = s.builder.ins().bnot(xa);
                let tagged = s.tag_int(r);
                s.builder.ins().jump(merge, &[tagged.into()]);
            },
        )
    }

    /// Inlines an integer comparison: untag, native `icmp`, tag the `Bool` result.
    fn inline_cmp(&mut self, op: Prim, args: &[CExpr], cc: IntCC) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        let anded = self.builder.ins().band(a, b);
        self.guard_immediate(
            anded,
            |s| s.prim_runtime_call(op, &[a, b]),
            |s, _slow, merge| {
                let xa = s.untag(a);
                let xb = s.untag(b);
                let c = s.builder.ins().icmp(cc, xa, xb);
                let tagged = s.tag_bool(c);
                s.builder.ins().jump(merge, &[tagged.into()]);
            },
        )
    }

    /// Inlines boolean `not`. Its operand is always an immediate `Bool`
    /// (`false` = `1`, `true` = `3`), so flipping the value bit is `x ^ 2`; no
    /// guard or fallback is needed.
    fn inline_not(&mut self, args: &[CExpr]) -> Value {
        let b = self.expr(&args[0]);
        self.builder.ins().bxor_imm(b, 2)
    }

    /// Inlines structural equality when the operands are immediate-representable.
    /// `Bool`/`Char`/`Unit` are never boxed, so a bare `icmp eq` suffices (the
    /// injective immediate tag makes word equality value equality). `Int` adds the
    /// immediate guard and the `fai_equal` fallback — a small immediate `Int` is
    /// never equal to a boxed (overflowed) one, so the fallback's mixed case is
    /// already correct. Other types keep the out-of-line structural path.
    fn inline_eq(&mut self, op: Prim, args: &[CExpr]) -> Option<Value> {
        let oty = &args[0].ty;
        if is_immediate_ty(oty) {
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            let c = self.builder.ins().icmp(IntCC::Equal, a, b);
            Some(self.tag_bool(c))
        } else if matches!(oty, Ty::Con(Con::Int)) {
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            let anded = self.builder.ins().band(a, b);
            Some(self.guard_immediate(
                anded,
                |s| s.prim_runtime_call(op, &[a, b]),
                |s, _slow, merge| {
                    let c = s.builder.ins().icmp(IntCC::Equal, a, b);
                    let tagged = s.tag_bool(c);
                    s.builder.ins().jump(merge, &[tagged.into()]);
                },
            ))
        } else if matches!(oty, Ty::Con(Con::Float)) {
            // Unboxed operands: compare raw IEEE-754 bits, exactly matching the
            // runtime's boxed-`Float` equality (so `NaN <> NaN` and `+0.0 <> -0.0`).
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            let ab = self.f64_to_i64(a);
            let bb = self.f64_to_i64(b);
            let c = self.builder.ins().icmp(IntCC::Equal, ab, bb);
            Some(self.tag_bool(c))
        } else {
            None
        }
    }

    /// Inlines structural ordering when the operands are immediate-representable,
    /// producing the same `-1`/`0`/`1` as `fai_compare`. `Bool`/`Char`/`Unit`
    /// compare bare; `Int` adds the guard and the `fai_compare` fallback. Other
    /// types keep the out-of-line structural path.
    fn inline_compare(&mut self, op: Prim, args: &[CExpr]) -> Option<Value> {
        let oty = &args[0].ty;
        if is_immediate_ty(oty) {
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            Some(self.compare_three_way(a, b))
        } else if matches!(oty, Ty::Con(Con::Int)) {
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            let anded = self.builder.ins().band(a, b);
            Some(self.guard_immediate(
                anded,
                |s| s.prim_runtime_call(op, &[a, b]),
                |s, _slow, merge| {
                    let tagged = s.compare_three_way(a, b);
                    s.builder.ins().jump(merge, &[tagged.into()]);
                },
            ))
        } else if matches!(oty, Ty::Con(Con::Float)) {
            // Unboxed operands: the runtime's no-alloc total-order comparison on
            // the raw bits (matches `fai_compare`'s boxed-`Float` `total_cmp`).
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            let ab = self.f64_to_i64(a);
            let bb = self.f64_to_i64(b);
            let f = self.runtime("fai_float_compare_bits", 2, true);
            let call = self.builder.ins().call(f, &[ab, bb]);
            Some(self.builder.inst_results(call)[0])
        } else {
            None
        }
    }

    /// Computes structural ordering of two immediate operands as a tagged
    /// `-1`/`0`/`1`: `(a > b) - (a < b)`, matching the runtime's
    /// `(a >> 1).cmp(b >> 1)`. The two-comparison form cannot overflow (unlike a
    /// direct subtraction), so the result always fits the immediate.
    fn compare_three_way(&mut self, a: Value, b: Value) -> Value {
        let xa = self.untag(a);
        let xb = self.untag(b);
        let gt = self.builder.ins().icmp(IntCC::SignedGreaterThan, xa, xb);
        let lt = self.builder.ins().icmp(IntCC::SignedLessThan, xa, xb);
        let gtw = self.builder.ins().uextend(types::I64, gt);
        let ltw = self.builder.ins().uextend(types::I64, lt);
        let cmp = self.builder.ins().isub(gtw, ltw);
        self.tag_int(cmp)
    }

    fn application(&mut self, func: &CExpr, args: &[CExpr], result_ty: &Ty) -> Value {
        // A saturated application of a known top-level function calls its code
        // symbol directly, passing the value arguments in registers per the callee's
        // ABI, skipping `apply_n` and the static closure. (Top-level functions
        // capture nothing, so the environment is a null pointer.) An
        // over-application direct-calls the saturated prefix and `apply_n`s the rest.
        if let ExprKind::Global(def) = func.kind {
            let arity = (self.arity_of)(def);
            if arity > 0 && args.len() >= arity {
                return if args.len() == arity {
                    self.direct_application(def, args, result_ty)
                } else {
                    self.over_application(def, arity, args, result_ty)
                };
            }
        }
        // Otherwise route through `apply_n`, whose slots are uniform `i64`: float
        // arguments are boxed in and a `Float` result comes back boxed and owned.
        let callee = self.expr(func);
        let vals: Vec<Value> = args.iter().map(|a| self.expr_boxed(a)).collect();
        let boxed = self.apply_n(callee, &vals);
        if matches!(result_ty, Ty::Con(Con::Float)) { self.owning_unbox(boxed) } else { boxed }
    }

    /// Applies an already-evaluated callee value to boxed arguments through the
    /// runtime `fai_apply_n` (the uniform first-class path); yields the boxed result.
    fn apply_n(&mut self, callee: Value, vals: &[Value]) -> Value {
        let args_ptr = self.spill(vals);
        let argc = self.builder.ins().iconst(types::I64, vals.len() as i64);
        let f = self.runtime("fai_apply_n", 3, true);
        let call = self.builder.ins().call(f, &[callee, argc, args_ptr]);
        self.builder.inst_results(call)[0]
    }

    /// Marshals `args` into registers per `def`'s [`FnAbi`] (a scalar-float argument
    /// in an `f64` register, every other as the boxed/immediate word, behind the
    /// leading null environment) and direct-calls it, yielding the raw result (an
    /// `f64` register when the callee returns a scalar float, else the uniform word).
    fn direct_call_value(&mut self, def: DefId, args: &[CExpr]) -> Value {
        let abi = (self.signature_of)(def);
        let null_env = self.builder.ins().iconst(types::I64, 0);
        let mut call_args = Vec::with_capacity(args.len() + 1);
        call_args.push(null_env);
        for (i, a) in args.iter().enumerate() {
            let v = if abi.float_param(i) {
                let v = self.expr(a);
                if self.is_f64(v) { v } else { self.owning_unbox(v) }
            } else {
                self.expr_boxed(a)
            };
            call_args.push(v);
        }
        self.direct_call(def, args.len(), &abi, &call_args)
    }

    /// A saturated direct call to top-level `def`. The raw result
    /// ([`Self::direct_call_value`]) is coerced to `result_ty`'s representation (the
    /// invariant: `f64` iff a scalar `Float`), unboxing a generic callee's boxed
    /// `Float`.
    fn direct_application(&mut self, def: DefId, args: &[CExpr], result_ty: &Ty) -> Value {
        let result = self.direct_call_value(def, args);
        self.as_repr_of(result, result_ty)
    }

    /// An over-application of top-level `def` (`args.len() > arity`): direct-call the
    /// saturated prefix, then apply the surplus arguments to its (function) result
    /// through `apply_n`. The prefix's residual return is a function — never a scalar
    /// `Float` — so its result is the uniform boxed word fed straight to `apply_n`.
    fn over_application(
        &mut self,
        def: DefId,
        arity: usize,
        args: &[CExpr],
        result_ty: &Ty,
    ) -> Value {
        let (prefix, overflow) = args.split_at(arity);
        let f_result = self.direct_call_value(def, prefix);
        let callee = self.ensure_boxed(f_result);
        let vals: Vec<Value> = overflow.iter().map(|a| self.expr_boxed(a)).collect();
        let boxed = self.apply_n(callee, &vals);
        if matches!(result_ty, Ty::Con(Con::Float)) { self.owning_unbox(boxed) } else { boxed }
    }

    /// Coerces `v` to the representation of `ty` (the invariant: `f64` iff a scalar
    /// `Float`, else a tagged `i64`), boxing or owning-unboxing as needed.
    fn as_repr_of(&mut self, v: Value, ty: &Ty) -> Value {
        let want_f64 = matches!(ty, Ty::Con(Con::Float));
        match (want_f64, self.is_f64(v)) {
            (true, true) | (false, false) => v,
            (true, false) => self.owning_unbox(v),
            (false, true) => self.box_float(v),
        }
    }

    /// Calls a direct-callable definition's code symbol directly with `call_args`
    /// (the leading null environment followed by the value arguments in registers).
    /// `arity`/`abi` build the matching register [`entry_signature`].
    fn direct_call(&mut self, def: DefId, arity: usize, abi: &FnAbi, call_args: &[Value]) -> Value {
        let name = code_symbol(self.namer, def);
        let sig = entry_signature(self.module, arity, abi);
        let id = self.module.declare_function(&name, Linkage::Import, &sig).expect("declare code");
        let fref = self.module.declare_func_in_func(id, self.builder.func);
        let call = self.builder.ins().call(fref, call_args);
        self.builder.inst_results(call)[0]
    }

    fn make_closure(&mut self, func: fai_core::ir::FnId, captures: &[LocalId]) -> Value {
        let arity = self.lowered.fns[func.index()].params.len() as i64;
        let code_id = self.fn_ids[func.index()];
        let ptr = self.ptr();
        let fref = self.module.declare_func_in_func(code_id, self.builder.func);
        let code_ptr = self.builder.ins().func_addr(ptr, fref);

        // Build the environment array. The reference-count pass has already
        // duplicated each captured value where it is still live afterward
        // (`MakeClosure` consumes its captures), so the values are stored
        // directly into the env.
        let mut env_vals = Vec::with_capacity(captures.len());
        for &c in captures {
            // Environment slots are uniform `i64`, so a captured float is boxed in.
            let v = self.use_var(c);
            env_vals.push(self.ensure_boxed(v));
        }
        let env_ptr = self.spill(&env_vals);

        let arity_v = self.builder.ins().iconst(types::I64, arity);
        let count_v = self.builder.ins().iconst(types::I64, captures.len() as i64);
        let f = self.runtime("fai_make_closure", 4, true);
        let call = self.builder.ins().call(f, &[code_ptr, arity_v, count_v, env_ptr]);
        self.builder.inst_results(call)[0]
    }

    /// Spills values to a stack array and yields its address (for `apply_n` /
    /// `make_closure`). An empty array yields a null pointer (never read).
    fn spill(&mut self, vals: &[Value]) -> Value {
        if vals.is_empty() {
            return self.builder.ins().iconst(types::I64, 0);
        }
        let size = u32::try_from(vals.len() * 8).expect("array size");
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            size,
            3,
        ));
        for (i, &v) in vals.iter().enumerate() {
            let offset = i32::try_from(i * 8).expect("slot offset");
            self.builder.ins().stack_store(v, slot, offset);
        }
        let ptr = self.ptr();
        self.builder.ins().stack_addr(ptr, slot, 0)
    }

    fn conditional(&mut self, cond: &CExpr, then: &CExpr, els: &CExpr) -> Value {
        let cv = self.expr(cond);
        let false_v = self.builder.ins().iconst(types::I64, 1); // Bool false
        let is_true =
            self.builder.ins().icmp(cranelift_codegen::ir::condcodes::IntCC::NotEqual, cv, false_v);

        let then_b = self.builder.create_block();
        let else_b = self.builder.create_block();
        self.builder.ins().brif(is_true, then_b, &[], else_b, &[]);

        // The merge block's parameter type follows the branches' actual
        // representation (`f64` for an unboxed float, else the uniform word). This
        // is read from the then-branch value rather than the node's static type,
        // because desugared `match` arms are wrapped in `If` nodes typed `Error`.
        self.builder.switch_to_block(then_b);
        self.builder.seal_block(then_b);
        let tv = self.expr(then);
        let merge_b = self.builder.create_block();
        let merge_ty = self.builder.func.dfg.value_type(tv);
        self.builder.append_block_param(merge_b, merge_ty);
        self.builder.ins().jump(merge_b, &[tv.into()]);

        self.builder.switch_to_block(else_b);
        self.builder.seal_block(else_b);
        let ev = self.expr(els);
        // The two branches share a type, so they share a representation — except a
        // desugared `match`'s unreachable fall-through (`<error>`), which is a bare
        // word; reinterpret it to the merge type (its value is never observed).
        let ev = self.coerce_repr(ev, merge_ty);
        self.builder.ins().jump(merge_b, &[ev.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// Reinterprets `v`'s bits to Cranelift type `ty` if they differ. Used only to
    /// reconcile a desugared `match`'s unreachable `<error>` fall-through with a
    /// branch/loop merge of a different representation; the value is never read.
    fn coerce_repr(&mut self, v: Value, ty: types::Type) -> Value {
        let vt = self.builder.func.dfg.value_type(v);
        if vt == ty {
            v
        } else if ty == types::F64 {
            self.i64_to_f64(v)
        } else {
            self.f64_to_i64(v)
        }
    }

    /// Code-generates a tail-call loop: a header block the loop-carried locals flow
    /// into (carried as cranelift variables, so the header is sealed only after its
    /// `Recur` back-edges are emitted), an exit block carrying the loop's result,
    /// and the body translated in tail position.
    ///
    /// The exit block's parameter type is fixed **lazily**, by the first tail value
    /// that jumps to it (see [`Self::jump_to_exit`]), rather than from the loop
    /// node's static type. A desugared `match` wraps its arms in `If` nodes typed
    /// `Error`, so the loop node's recorded type can be unreliable; reading the
    /// actual value's representation (as the `if`-merge does) keeps an unboxed
    /// `f64` loop result from being mistaken for a boxed word.
    fn join(&mut self, params: &[LocalId], body: &CExpr, _result_ty: &Ty) -> Value {
        let header = self.builder.create_block();
        let exit = self.builder.create_block();

        // The loop-carried locals already hold their initial values (parameters
        // and, for a spine-building loop, the hole). Enter the header.
        self.builder.ins().jump(header, &[]);
        self.builder.switch_to_block(header);
        // The header stays unsealed: its `Recur` back-edge predecessors are still
        // to be emitted while translating the body.

        let prev = self.loop_ctx.replace(LoopCtx { header, exit, params: params.to_vec() });
        self.expr_tail(body);
        self.loop_ctx = prev;

        self.builder.seal_block(header);
        // Every reachable loop exits through at least one tail value, which has
        // appended the exit parameter. A loop with no exit (only back-edges) is
        // unreachable past the header; give its exit a uniform parameter so the
        // result read stays well-formed.
        if self.builder.func.dfg.block_params(exit).is_empty() {
            self.builder.append_block_param(exit, types::I64);
        }
        self.builder.switch_to_block(exit);
        self.builder.seal_block(exit);
        self.builder.block_params(exit)[0]
    }

    /// Jumps to the enclosing loop's exit with `v`. The first such jump fixes the
    /// exit parameter's type to `v`'s actual representation (`f64` for an unboxed
    /// float, else the uniform word); later jumps coerce to it. This makes the
    /// loop result's representation follow the tail values rather than a static
    /// type a desugared `match` may have recorded as `Error`.
    fn jump_to_exit(&mut self, v: Value) {
        let exit = self.loop_ctx.as_ref().expect("exit inside a loop").exit;
        let v = if self.builder.func.dfg.block_params(exit).is_empty() {
            let ty = self.builder.func.dfg.value_type(v);
            self.builder.append_block_param(exit, ty);
            v
        } else {
            let exit_ty = self.builder.func.dfg.block_params(exit)[0];
            let exit_ty = self.builder.func.dfg.value_type(exit_ty);
            self.coerce_repr(v, exit_ty)
        };
        self.builder.ins().jump(exit, &[v.into()]);
    }

    /// Links a freshly built cell into the spine through the hole: store the cell
    /// where the destination points, then advance the destination to the cell's
    /// recursive `field` (its next hole). Returns the new destination.
    fn hole_fill(&mut self, hole: LocalId, cell: &CExpr, field: u32) -> Value {
        let cellv = self.expr(cell);
        let dst = self.use_var(hole);
        self.builder.ins().store(MemFlags::trusted(), cellv, dst, 0);
        // A boxed value is its own pointer (low bit clear), so the field address is
        // a constant offset from the cell.
        let offset = rt::DATA_FIELDS_OFFSET + field as usize * 8;
        self.builder.ins().iadd_imm(cellv, i64::try_from(offset).expect("field offset"))
    }

    /// Translates an expression in tail position within a `Join` body: `Recur`
    /// jumps to the loop header, `HoleClose` and plain base values jump to the loop
    /// exit with the loop's result, control flow recurses in tail position, and
    /// binders are emitted with their continuation recursed.
    fn expr_tail(&mut self, e: &CExpr) {
        match &e.kind {
            ExprKind::If { cond, then, els } => {
                let cv = self.expr(cond);
                let false_v = self.builder.ins().iconst(types::I64, 1); // Bool false
                let is_true = self.builder.ins().icmp(
                    cranelift_codegen::ir::condcodes::IntCC::NotEqual,
                    cv,
                    false_v,
                );
                let then_b = self.builder.create_block();
                let else_b = self.builder.create_block();
                self.builder.ins().brif(is_true, then_b, &[], else_b, &[]);
                self.builder.switch_to_block(then_b);
                self.builder.seal_block(then_b);
                self.expr_tail(then);
                self.builder.switch_to_block(else_b);
                self.builder.seal_block(else_b);
                self.expr_tail(els);
            }
            ExprKind::Let { local, value, body } => {
                let v = self.expr(value);
                self.define_var(*local, v);
                self.expr_tail(body);
            }
            ExprKind::Reset { value, token, body } => {
                let v = self.expr(value);
                let tok = self.call1("fai_drop_reuse", v);
                self.define_var(*token, tok);
                self.expr_tail(body);
            }
            ExprKind::Dup { local, body } => {
                self.dup_local(*local);
                self.expr_tail(body);
            }
            ExprKind::Drop { local, body } => {
                // The continuation is terminal, so the drop is emitted before it.
                // Reference counting placed the drop after the local's last use, so
                // neither the back-edge arguments nor the exit value read it.
                self.drop_local(*local);
                self.expr_tail(body);
            }
            ExprKind::Recur { args } => self.recur(args),
            ExprKind::HoleClose { hole, base } => {
                let basev = self.expr(base);
                let dst = self.use_var(*hole);
                self.builder.ins().store(MemFlags::trusted(), basev, dst, 0);
                let slot = self.result_slot.expect("a spine-building loop has a result slot");
                let result = self.builder.ins().load(types::I64, MemFlags::trusted(), slot, 0);
                self.jump_to_exit(result);
            }
            // Any other tail expression is the loop's value (a plain tail-call
            // loop's base case): evaluate it and exit. `jump_to_exit` fixes the
            // exit representation from this value and coerces a later `<error>`.
            _ => {
                let v = self.expr(e);
                self.jump_to_exit(v);
            }
        }
    }

    /// A `Recur` back-edge: evaluate the new loop-carried values (which may read the
    /// current ones), then reassign every loop local and jump to the header.
    fn recur(&mut self, args: &[CExpr]) {
        let vals: Vec<Value> = args.iter().map(|a| self.expr(a)).collect();
        let (header, params) = {
            let ctx = self.loop_ctx.as_ref().expect("recur inside a loop");
            (ctx.header, ctx.params.clone())
        };
        for (param, val) in params.iter().zip(vals) {
            self.define_var(*param, val);
        }
        self.builder.ins().jump(header, &[]);
    }
}

/// Whether `n` fits the 63-bit immediate range.
fn fits_immediate(n: i64) -> bool {
    ((n << 1) >> 1) == n
}

/// Whether values of `ty` are always immediates (so dup/drop are no-ops). `Int`
/// is excluded because it boxes on overflow; only `Bool`, `Unit`, and `Char` are
/// unconditionally immediate.
fn is_immediate_ty(ty: &Ty) -> bool {
    matches!(ty, Ty::Unit | Ty::Con(Con::Bool) | Ty::Con(Con::Char))
}

/// Whether values of `ty` are *always* boxed heap objects (never an immediate),
/// so an inlined dup/drop can safely skip the immediate tag-check. `String` and
/// `Float` are unconditionally heap-allocated; a tuple always has its elements; a
/// non-empty record (closed or open) always has at least one field; interface
/// dictionaries and closures are always boxed. Deliberately excludes `Int` (a
/// small value is an immediate), discriminated unions and `List` (a nullary
/// constructor is an immediate), the empty record, and type variables / unknowns.
fn is_always_boxed_ty(ty: &Ty) -> bool {
    match ty {
        Ty::Con(Con::String | Con::Float) | Ty::Tuple(_) | Ty::Interface(_) | Ty::Arrow(..) => true,
        Ty::Record(row) => !row.fields.is_empty(),
        _ => false,
    }
}

/// Whether `ty` is a boxed *leaf* — a heap object with no reference-counted
/// children — so a dead one is freed directly, with no child release. The boxed
/// `Int`, `Float`, and `String` kinds are leaves.
fn is_leaf_boxed_ty(ty: &Ty) -> bool {
    matches!(ty, Ty::Con(Con::Int | Con::Float | Con::String))
}

/// Whether `ty` is a data type that may also be an immediate — a discriminated
/// union or `List`, whose nullary constructors (`None`, `[]`, …) are immediates —
/// so an inlined drop must tag-check before touching the cell.
fn is_data_maybe_immediate(ty: &Ty) -> bool {
    fn head(ty: &Ty) -> bool {
        match ty {
            Ty::Adt(_) | Ty::Con(Con::List) => true,
            Ty::App(h, _) => head(h),
            _ => false,
        }
    }
    head(ty)
}

/// Records each local's static type from `e` into `out`: the type carried by
/// every `Local` use (so parameters and captures are covered, not just `let`
/// bindings) plus each `let`'s value type. A local's reference-count operations
/// read this map to specialize. `Ty::Error` is skipped, leaving the local to the
/// runtime fallback rather than recording a useless type (e.g. a reuse `Reset`'s
/// synthesized base carries no type).
fn collect_local_types(e: &CExpr, out: &mut FxHashMap<usize, Ty>) {
    let note = |out: &mut FxHashMap<usize, Ty>, local: LocalId, ty: &Ty| {
        if !matches!(ty, Ty::Error) {
            out.insert(local.index(), ty.clone());
        }
    };
    match &e.kind {
        ExprKind::Local(l) => note(out, *l, &e.ty),
        ExprKind::Lit(_) | ExprKind::Global(_) | ExprKind::MakeClosure { .. } | ExprKind::Error => {
        }
        ExprKind::Prim { args, .. } | ExprKind::MakeData { args, .. } => {
            args.iter().for_each(|a| collect_local_types(a, out));
        }
        ExprKind::App { func, args } => {
            collect_local_types(func, out);
            args.iter().for_each(|a| collect_local_types(a, out));
        }
        ExprKind::If { cond, then, els } => {
            collect_local_types(cond, out);
            collect_local_types(then, out);
            collect_local_types(els, out);
        }
        ExprKind::Let { local, value, body } => {
            note(out, *local, &value.ty);
            collect_local_types(value, out);
            collect_local_types(body, out);
        }
        ExprKind::DataTag(base) => collect_local_types(base, out),
        ExprKind::DataField { base, .. } => collect_local_types(base, out),
        ExprKind::Reset { value, body, .. } => {
            collect_local_types(value, out);
            collect_local_types(body, out);
        }
        ExprKind::Dup { body, .. } | ExprKind::Drop { body, .. } => collect_local_types(body, out),
        ExprKind::Join { body, .. } | ExprKind::HoleStart { body, .. } => {
            collect_local_types(body, out);
        }
        ExprKind::Recur { args } => args.iter().for_each(|a| collect_local_types(a, out)),
        ExprKind::HoleFill { cell, .. } => collect_local_types(cell, out),
        ExprKind::HoleClose { base, .. } => collect_local_types(base, out),
    }
}

/// Records, across the body, which locals are observed as a scalar `Float`
/// (`float_seen`) and which are observed as any other (or unknown) type
/// (`other_seen`), at the same points [`collect_local_types`] reads: every
/// `Local` use and each `let` binding. A local is an unboxed `f64` only when it is
/// in `float_seen` and not in `other_seen` (see [`Translator::collect_f64_locals`]).
fn collect_float_observations(
    e: &CExpr,
    float_seen: &mut FxHashSet<usize>,
    other_seen: &mut FxHashSet<usize>,
) {
    fn note(
        local: LocalId,
        ty: &Ty,
        float_seen: &mut FxHashSet<usize>,
        other: &mut FxHashSet<usize>,
    ) {
        if matches!(ty, Ty::Con(Con::Float)) {
            float_seen.insert(local.index());
        } else {
            other.insert(local.index());
        }
    }
    match &e.kind {
        ExprKind::Local(l) => note(*l, &e.ty, float_seen, other_seen),
        ExprKind::Lit(_) | ExprKind::Global(_) | ExprKind::MakeClosure { .. } | ExprKind::Error => {
        }
        ExprKind::Prim { args, .. } | ExprKind::MakeData { args, .. } => {
            args.iter().for_each(|a| collect_float_observations(a, float_seen, other_seen));
        }
        ExprKind::App { func, args } => {
            collect_float_observations(func, float_seen, other_seen);
            args.iter().for_each(|a| collect_float_observations(a, float_seen, other_seen));
        }
        ExprKind::If { cond, then, els } => {
            collect_float_observations(cond, float_seen, other_seen);
            collect_float_observations(then, float_seen, other_seen);
            collect_float_observations(els, float_seen, other_seen);
        }
        ExprKind::Let { local, value, body } => {
            note(*local, &value.ty, float_seen, other_seen);
            collect_float_observations(value, float_seen, other_seen);
            collect_float_observations(body, float_seen, other_seen);
        }
        ExprKind::DataTag(base) | ExprKind::DataField { base, .. } => {
            collect_float_observations(base, float_seen, other_seen);
        }
        ExprKind::Reset { value, body, .. } => {
            collect_float_observations(value, float_seen, other_seen);
            collect_float_observations(body, float_seen, other_seen);
        }
        ExprKind::Dup { body, .. } | ExprKind::Drop { body, .. } => {
            collect_float_observations(body, float_seen, other_seen);
        }
        ExprKind::Join { body, .. } | ExprKind::HoleStart { body, .. } => {
            collect_float_observations(body, float_seen, other_seen);
        }
        ExprKind::Recur { args } => {
            args.iter().for_each(|a| collect_float_observations(a, float_seen, other_seen));
        }
        ExprKind::HoleFill { cell, .. } => {
            collect_float_observations(cell, float_seen, other_seen);
        }
        ExprKind::HoleClose { base, .. } => {
            collect_float_observations(base, float_seen, other_seen);
        }
    }
}

/// The inlined dup strategy for a value of statically known type `ty`: a no-op
/// for an immediate, an unconditional increment for an always-boxed value, else a
/// tag-checked increment (the safe default for `Int`, data, and unknown types).
fn dup_class(ty: &Ty) -> DupPlan {
    // A monomorphic scalar `Float` local is an unboxed `f64`, which carries no
    // reference count. (A `Float` *field* inside a cell stays boxed and is handled
    // by the cell's drop, not here.)
    if is_immediate_ty(ty) || matches!(ty, Ty::Con(Con::Float)) {
        DupPlan::NoOp
    } else {
        DupPlan::Incr { tag_check: !is_always_boxed_ty(ty) }
    }
}

/// The inlined drop strategy for a value of statically known type `ty`. The
/// tag-check is elided only for types that are provably never an immediate
/// ([`is_always_boxed_ty`]); a fixed-shape cell unrolls its field releases, a
/// boxed leaf frees directly, other data releases its children via the runtime,
/// and an unrecognized type falls back to the runtime drop.
fn drop_class(ty: &Ty) -> DropPlan {
    // An unboxed scalar `Float` local carries no reference count (see `dup_class`).
    if is_immediate_ty(ty) || matches!(ty, Ty::Con(Con::Float)) {
        return DropPlan::NoOp;
    }
    if let Some(fields) = fixed_shape_drop(ty, MAX_INLINE_DROP_BOXED_FIELDS) {
        return DropPlan::Fixed(fields);
    }
    let always_boxed = is_always_boxed_ty(ty);
    if is_leaf_boxed_ty(ty) {
        return DropPlan::Leaf { tag_check: !always_boxed };
    }
    if always_boxed || is_data_maybe_immediate(ty) {
        return DropPlan::Data { tag_check: !always_boxed };
    }
    DropPlan::Runtime
}

/// How an inlined `Dup` of a known-typed local increments its reference count.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DupPlan {
    /// An immediate carries no reference count: nothing to do.
    NoOp,
    /// Increment in place, guarded by an immediate tag-check unless `tag_check`
    /// is `false` (a statically always-boxed value).
    Incr {
        /// Whether to guard the increment with an immediate tag-check.
        tag_check: bool,
    },
}

/// How an inlined `Drop` of a known-typed local releases it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DropPlan {
    /// An immediate carries no reference count: nothing to do.
    NoOp,
    /// A fixed-shape record/tuple (always boxed): unroll its per-field release.
    Fixed(Vec<FieldDrop>),
    /// A boxed leaf (Int/Float/String): decrement and, when dead, free directly.
    Leaf {
        /// Whether to guard with an immediate tag-check (true for `Int`).
        tag_check: bool,
    },
    /// A variable-shape data cell (List/union/interface/closure/wide record):
    /// decrement and, when dead, release its children via the runtime, then free.
    Data {
        /// Whether to guard with an immediate tag-check (true for List/union).
        tag_check: bool,
    },
    /// An unknown type: dispatch to the runtime drop (the polymorphic fallback).
    Runtime,
}

/// A fast-path integer operation whose result of two immediates can exceed the
/// 63-bit immediate range, so its re-tag is guarded by an overflow check.
#[derive(Clone, Copy)]
enum FitsOp {
    Add,
    Sub,
    Mul,
    Shl,
    /// Arithmetic (sign-extending) shift right.
    Shr,
    /// Logical (zero-filling) shift right.
    Ushr,
}

/// A fast-path bitwise operation whose result of two immediates always fits the
/// immediate, so its re-tag needs no overflow check.
#[derive(Clone, Copy)]
enum BitOp {
    And,
    Or,
    Xor,
}

/// An unboxed `f64` arithmetic operation.
#[derive(Clone, Copy)]
enum FloatBinop {
    Add,
    Sub,
    Mul,
    Div,
}

/// How a data cell's field is released by an inlined drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldDrop {
    /// A statically-immediate field (no reference count): nothing to release.
    Immediate,
    /// A possibly-boxed field: released with a runtime drop (a no-op at runtime if
    /// it turns out to be immediate, e.g. a small `Int`).
    Boxed,
}

/// The most boxed fields an inlined drop will release before falling back to the
/// runtime path. Each boxed field emits a load and a drop call, so capping the
/// count bounds generated-code growth (the runtime loop handles any width in
/// fixed code); immediate fields are free to skip and do not count.
const MAX_INLINE_DROP_BOXED_FIELDS: usize = 8;

/// If `ty` is a monomorphic, fixed-shape data cell — a non-empty **closed**
/// record or a **tuple** — returns each field's [`FieldDrop`] class in heap
/// layout order (records sorted by label, tuples positional), so the cell's drop
/// can be inlined. Returns `None` for anything whose runtime shape is not a fixed
/// boxed cell of statically-known arity: open (row-polymorphic) records, the
/// empty record (an immediate), discriminated unions and `List` (the field count
/// varies by constructor), interfaces, and the scalar/immediate/function types.
///
/// Also returns `None` when more than `max_boxed` fields are boxed, leaving wide
/// cells to the runtime path.
fn fixed_shape_drop(ty: &Ty, max_boxed: usize) -> Option<Vec<FieldDrop>> {
    let fields: Vec<FieldDrop> = match ty {
        Ty::Tuple(elems) => elems.iter().map(field_drop).collect(),
        // A closed record is exactly its listed fields; an open one has an unknown
        // tail, and an empty one is a tagged immediate (no heap cell).
        Ty::Record(row) if row.tail == RowEnd::Closed && !row.fields.is_empty() => {
            row.fields.iter().map(|(_, t)| field_drop(t)).collect()
        }
        _ => return None,
    };
    let boxed = fields.iter().filter(|c| matches!(c, FieldDrop::Boxed)).count();
    if boxed > max_boxed { None } else { Some(fields) }
}

/// Classifies a field type for an inlined drop: a statically-immediate type needs
/// no release; everything else is released with a runtime drop (which is itself a
/// no-op on a value that turns out to be immediate).
fn field_drop(ty: &Ty) -> FieldDrop {
    if is_immediate_ty(ty) { FieldDrop::Immediate } else { FieldDrop::Boxed }
}

#[cfg(test)]
mod classifier_tests {
    //! Unit tests for [`fixed_shape_drop`] — which static types the inlined-drop
    //! classifier recognizes as fixed-shape data cells, and how it classifies and
    //! width-caps their fields. One focused `#[test]` per case.

    use fai_resolve::AdtRef;
    use fai_span::SourceId;
    use fai_syntax::Symbol;
    use fai_types::{Con, RecordRow, RowEnd, RowVarId, Ty};

    use super::{FieldDrop, fixed_shape_drop};

    use FieldDrop::{Boxed, Immediate};

    /// A high cap, so width never interferes with shape-recognition cases.
    const WIDE: usize = 64;

    fn closed_record(fields: &[(&str, Ty)]) -> Ty {
        Ty::Record(RecordRow {
            fields: fields.iter().map(|(l, t)| (Symbol::intern(l), t.clone())).collect(),
            tail: RowEnd::Closed,
        })
    }

    fn open_record(fields: &[(&str, Ty)]) -> Ty {
        Ty::Record(RecordRow {
            fields: fields.iter().map(|(l, t)| (Symbol::intern(l), t.clone())).collect(),
            tail: RowEnd::Open(RowVarId(0)),
        })
    }

    fn adt(name: &str) -> Ty {
        Ty::Adt(AdtRef::new(SourceId::new(0), Symbol::intern(name)))
    }

    #[test]
    fn tuple_is_a_fixed_shape() {
        let ty = Ty::Tuple(vec![Ty::bool(), Ty::Con(Con::String)]);
        assert_eq!(fixed_shape_drop(&ty, WIDE), Some(vec![Immediate, Boxed]));
    }

    #[test]
    fn closed_record_classifies_fields_in_layout_order() {
        // Fields are stored sorted by label (the heap layout); the classes line up
        // with that order, not source order.
        let ty = closed_record(&[("a", Ty::bool()), ("b", Ty::Con(Con::String))]);
        assert_eq!(fixed_shape_drop(&ty, WIDE), Some(vec![Immediate, Boxed]));
    }

    #[test]
    fn single_field_closed_record_is_a_fixed_shape() {
        let ty = closed_record(&[("x", Ty::Con(Con::String))]);
        assert_eq!(fixed_shape_drop(&ty, WIDE), Some(vec![Boxed]));
    }

    #[test]
    fn mixed_record_classifies_each_field() {
        // `Int` counts as boxed (it overflow-boxes), so it is released, not skipped.
        let ty = closed_record(&[("a", Ty::bool()), ("b", Ty::int()), ("c", Ty::Con(Con::String))]);
        assert_eq!(fixed_shape_drop(&ty, WIDE), Some(vec![Immediate, Boxed, Boxed]));
    }

    #[test]
    fn nested_record_field_is_boxed_not_recursed() {
        // The inner record is itself a fixed shape, but a field is never recursed
        // into: it is released as one boxed child (via the runtime drop).
        let inner = closed_record(&[("x", Ty::int())]);
        let ty = closed_record(&[("inner", inner)]);
        assert_eq!(fixed_shape_drop(&ty, WIDE), Some(vec![Boxed]));
    }

    #[test]
    fn open_record_is_not_specialized() {
        // A row-polymorphic tail means an unknown field count.
        let ty = open_record(&[("a", Ty::int())]);
        assert_eq!(fixed_shape_drop(&ty, WIDE), None);
    }

    #[test]
    fn empty_closed_record_is_not_specialized() {
        // An empty record is a tagged immediate, not a heap cell.
        let ty = closed_record(&[]);
        assert_eq!(fixed_shape_drop(&ty, WIDE), None);
    }

    #[test]
    fn discriminated_union_is_not_specialized() {
        // A union's field count varies by constructor, so it has no fixed shape.
        assert_eq!(fixed_shape_drop(&adt("Shape"), WIDE), None);
        // …including when applied to type arguments (`Option Int`).
        let applied = Ty::App(adt("Option").into(), Ty::int().into());
        assert_eq!(fixed_shape_drop(&applied, WIDE), None);
    }

    #[test]
    fn list_is_not_specialized() {
        assert_eq!(fixed_shape_drop(&Ty::list(Ty::int()), WIDE), None);
    }

    #[test]
    fn string_is_not_specialized() {
        // A boxed leaf, but not a data cell with addressable fields.
        assert_eq!(fixed_shape_drop(&Ty::Con(Con::String), WIDE), None);
    }

    #[test]
    fn int_is_not_specialized() {
        assert_eq!(fixed_shape_drop(&Ty::int(), WIDE), None);
    }

    #[test]
    fn immediate_scalar_is_not_specialized() {
        // Immediates are handled by `is_immediate_ty`, not the cell classifier.
        assert_eq!(fixed_shape_drop(&Ty::bool(), WIDE), None);
    }

    #[test]
    fn type_variable_is_not_specialized() {
        assert_eq!(fixed_shape_drop(&Ty::Var(fai_types::TyVarId(0)), WIDE), None);
    }

    #[test]
    fn function_type_is_not_specialized() {
        assert_eq!(fixed_shape_drop(&Ty::arrow(Ty::int(), Ty::int()), WIDE), None);
    }

    #[test]
    fn width_threshold_rejects_too_many_boxed_fields() {
        let three = Ty::Tuple(vec![Ty::Con(Con::String); 3]);
        assert_eq!(fixed_shape_drop(&three, 2), None, "3 boxed > cap of 2");
        let two = Ty::Tuple(vec![Ty::Con(Con::String); 2]);
        assert!(fixed_shape_drop(&two, 2).is_some(), "2 boxed is within the cap");
    }

    #[test]
    fn width_threshold_ignores_immediate_fields() {
        // Many immediate fields plus a couple of boxed ones: only the boxed count
        // is capped, so a wide all-but-two-immediate record still specializes.
        let mut fields: Vec<(&str, Ty)> =
            vec![("aa", Ty::bool()), ("bb", Ty::bool()), ("cc", Ty::bool()), ("dd", Ty::bool())];
        fields.push(("ee", Ty::Con(Con::String)));
        fields.push(("ff", Ty::Con(Con::String)));
        let ty = closed_record(&fields);
        assert_eq!(
            fixed_shape_drop(&ty, 2),
            Some(vec![Immediate, Immediate, Immediate, Immediate, Boxed, Boxed])
        );
    }
}

#[cfg(test)]
mod wire_projection_tests {
    //! The worker (`fai run`/`fai test`) compiles definitions reconstructed from
    //! the wire bundle, where each node's type is a marker rebuilt from its
    //! [`fai_core::wire::WireTy`] projection. These tests pin the safety-critical
    //! invariant: that round-trip preserves every classification code generation
    //! makes from a type — the inlined dup/drop strategy and the prim borrow
    //! decision — so the worker emits the same reference-count code as the warm
    //! in-process path. A divergence here is a memory-safety bug.

    use std::sync::Arc;

    use fai_core::ir::Prim;
    use fai_core::wire::{project_ty, reconstruct_ty};
    use fai_resolve::{AdtRef, InterfaceRef};
    use fai_span::SourceId;
    use fai_syntax::Symbol;
    use fai_types::{Con, RecordRow, RowEnd, RowVarId, Ty, TyVarId};
    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::{drop_class, dup_class};

    fn adt() -> Ty {
        Ty::Adt(AdtRef::new(SourceId::new(0), Symbol::intern("T")))
    }

    /// A strategy generating types across every code-generation class, including
    /// nested tuples/records and open/closed rows.
    fn arb_ty() -> impl Strategy<Value = Ty> {
        let leaf = prop_oneof![
            Just(Ty::Unit),
            Just(Ty::Con(Con::Int)),
            Just(Ty::Con(Con::Float)),
            Just(Ty::Con(Con::Bool)),
            Just(Ty::Con(Con::String)),
            Just(Ty::Con(Con::Char)),
            Just(Ty::Var(TyVarId(0))),
            Just(Ty::Error),
            Just(adt()),
            Just(Ty::Interface(InterfaceRef::new(SourceId::new(0), Symbol::intern("I")))),
        ];
        leaf.prop_recursive(3, 32, 4, |inner| {
            prop_oneof![
                vec(inner.clone(), 2..4).prop_map(Ty::Tuple),
                (vec(inner.clone(), 0..4), any::<bool>()).prop_map(|(tys, closed)| {
                    let fields = tys
                        .into_iter()
                        .enumerate()
                        .map(|(i, t)| (Symbol::intern(&format!("f{i}")), t))
                        .collect();
                    let tail = if closed { RowEnd::Closed } else { RowEnd::Open(RowVarId(0)) };
                    Ty::Record(RecordRow { fields, tail })
                }),
                inner.clone().prop_map(Ty::list),
                inner.clone().prop_map(|a| Ty::App(Arc::new(adt()), Arc::new(a))),
                (inner.clone(), inner.clone()).prop_map(|(a, b)| Ty::arrow(a, b)),
            ]
        })
    }

    proptest! {
        #[test]
        fn round_trip_preserves_codegen_classification(ty in arb_ty()) {
            let round = reconstruct_ty(&project_ty(&ty));
            prop_assert_eq!(drop_class(&ty), drop_class(&round), "drop class for {:?}", ty);
            prop_assert_eq!(dup_class(&ty), dup_class(&round), "dup class for {:?}", ty);
            // The prim borrow decision (e.g. structural equality) is re-derived from
            // the operand type, so the projection must preserve it too.
            prop_assert_eq!(
                Prim::Eq.borrows_operand(&ty),
                Prim::Eq.borrows_operand(&round),
                "borrow decision for {:?}",
                ty
            );
        }
    }
}

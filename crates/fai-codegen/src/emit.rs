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
use fai_core::NicheKind;
use fai_core::bounds::{BoundSig, Bounds, ResultSig};
use fai_core::ir::{
    CExpr, ClosureAlloc, CoreFn, ExprKind, FieldIndex, FnAbi, FnId, Lit, LoweredDef, Prim, Repr,
};
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

/// Builds the symbol of a definition's token-taking specialized entry (the target
/// of a forwarded saturated direct call).
#[must_use]
pub fn reuse_symbol(namer: &dyn Fn(DefId) -> String, def: DefId) -> String {
    format!("{}__reuse", namer(def))
}

/// Bounds-check-elimination inputs for code generation: a definition's inferred
/// entry facts (difference constraints over its parameters, used to seed the fact
/// graph at the function entry) and each callee's result facts (consulted when a
/// `let` binds a saturated call). `shadow` retains an "elided" check but routes
/// its failure to a distinct abort, so generative testing turns any unsound
/// elision into a loud failure rather than a silent out-of-bounds access.
pub struct Bce<'a> {
    /// A definition's entry-fact signature (facts assumable about its parameters).
    pub entry_of: &'a dyn Fn(DefId) -> BoundSig,
    /// A definition's result-fact signature (its result's length/bounds relative
    /// to its parameters).
    pub result_of: &'a dyn Fn(DefId) -> ResultSig,
    /// Whether to emit the bounds-check-elimination shadow check (testing only).
    pub shadow: bool,
}

fn empty_entry(_: DefId) -> BoundSig {
    BoundSig::default()
}

fn empty_result(_: DefId) -> ResultSig {
    ResultSig::default()
}

impl Bce<'static> {
    /// The no-facts configuration: no entry/result facts and no shadow check, so
    /// every inline `Array` access keeps its bounds check. Used by IR-shape tests
    /// and any caller without the interprocedural fact queries.
    #[must_use]
    pub fn none() -> Self {
        Bce { entry_of: &empty_entry, result_of: &empty_result, shadow: false }
    }
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
    borrows_of: &dyn Fn(DefId) -> Vec<bool>,
    bce: &Bce,
) {
    let mut jobs = Vec::new();
    build_def(module, lowered, namer, arity_of, signature_of, borrows_of, bce, &mut jobs);
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_def<M: Module>(
    module: &mut M,
    lowered: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    signature_of: &dyn Fn(DefId) -> FnAbi,
    borrows_of: &dyn Fn(DefId) -> Vec<bool>,
    bce: &Bce,
    jobs: &mut Vec<(FuncId, Context)>,
) {
    let base = namer(lowered.def);
    let abi = signature_of(lowered.def);
    // The entry's inferred bounds facts (over its source parameters), seeded into
    // the fact graph for `fn0` only; lifted lambdas get no seed.
    let entry_facts = (bce.entry_of)(lowered.def);
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

    // A non-capturing lifted lambda needs no per-activation environment, so it
    // shares one immortal static closure (like a top-level function's value form)
    // rather than allocating a cell at every `MakeClosure`. Declare that data
    // symbol for each such lambda (exported so the definition's reuse entry, a
    // separate object, can reference the same cell; defined below once the bodies
    // are built). A capturing lambda — and the entry (index 0) — is `None`.
    let lambda_closures = lambda_closure_data(module, lowered, &base, Linkage::Export);

    // Build each function body into its own (uncompiled) context.
    for (i, f) in lowered.fns.iter().enumerate() {
        // Seed the entry facts only for the entry function (`fn0`), whose
        // parameters are the definition's source parameters (offset 0).
        let seed = (i == 0).then_some((&entry_facts, 0usize));
        let ctx = build_fn(
            module,
            f,
            lowered,
            namer,
            arity_of,
            signature_of,
            borrows_of,
            &abi,
            &fn_ids,
            &lambda_closures,
            &base,
            i,
            bce,
            seed,
        );
        jobs.push((fn_ids[i], ctx));
    }

    // Define each non-capturing lambda's immortal static closure, now that its
    // code symbol is built: `{ rc = IMMORTAL, &CLOSURE_DESC, size, code, arity,
    // env_count = 0 }` — the same shape as a definition's value closure.
    for (i, data) in lambda_closures.iter().enumerate() {
        if let Some(data) = data {
            define_static_closure(module, *data, fn_ids[i], lowered.fns[i].params.len() as u64);
        }
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
        let ctx =
            build_owned_wrapper(module, fn_ids[0], &lowered.entry_borrowed, &abi, arity, &base);
        jobs.push((wrapper, ctx));
        wrapper
    } else {
        fn_ids[0]
    };
    define_static_closure(module, closure_data, closure_code, arity as u64);
}

/// Declares and builds a definition's token-taking specialized entry
/// (`{base}__reuse`), pushing its uncompiled body onto `jobs`. A no-op for a
/// definition that accepts no forwarded tokens (no [`LoweredDef::reuse_entry`]).
///
/// The entry's calling convention prepends `k` reuse-token registers to the
/// primary entry's value parameters (`fn(env, t0, …, a0, …)`); its body is the
/// forwarded primary body with those tokens threaded into its leftover sinks (set
/// by reference counting). Emitted separately from [`build_def`] so the per-
/// definition primary object stays a pure function of the definition while the
/// driver links a reuse entry only where a caller actually forwards to it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_reuse_object<M: Module>(
    module: &mut M,
    lowered: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    signature_of: &dyn Fn(DefId) -> FnAbi,
    borrows_of: &dyn Fn(DefId) -> Vec<bool>,
    bce: &Bce,
    jobs: &mut Vec<(FuncId, Context)>,
) {
    let Some(reuse_entry) = &lowered.reuse_entry else { return };
    let base = namer(lowered.def);
    let abi = signature_of(lowered.def);
    let source_arity = lowered.entry().params.len();
    let tokens = reuse_entry.params.len().saturating_sub(source_arity);
    // The reuse entry runs the same body as the primary entry, so its source
    // parameters (after the leading reuse tokens) carry the same entry facts.
    let entry_facts = (bce.entry_of)(lowered.def);

    // The specialized ABI: `k` leading uniform (`i64`) token parameters, then the
    // primary entry's value-parameter representations; register-passing.
    let mut params = vec![Repr::Uniform; tokens];
    params.extend(abi.params.iter().cloned());
    let reuse_abi = FnAbi { params, ret: abi.ret.clone(), register_abi: true };

    let uniform_sig = code_signature(module);
    let entry_sig = reuse_entry_signature(module, tokens, source_arity, &abi);

    // The reuse entry is `fn0`; lifted lambdas it references are imported from the
    // primary object (same `{base}__fn{i}` symbols).
    let mut fn_ids = Vec::with_capacity(lowered.fns.len());
    fn_ids.push(
        module
            .declare_function(&reuse_symbol(namer, lowered.def), Linkage::Export, &entry_sig)
            .expect("declare reuse entry"),
    );
    for i in 1..lowered.fns.len() {
        fn_ids.push(
            module
                .declare_function(&format!("{base}__fn{i}"), Linkage::Import, &uniform_sig)
                .expect("declare lambda import"),
        );
    }

    // A non-capturing lambda's immortal static closure is defined in the primary
    // object; the reuse entry imports the same symbol (it shares one cell).
    let lambda_closures = lambda_closure_data(module, lowered, &base, Linkage::Import);

    let ctx = build_fn(
        module,
        reuse_entry,
        lowered,
        namer,
        arity_of,
        signature_of,
        borrows_of,
        &reuse_abi,
        &fn_ids,
        &lambda_closures,
        &base,
        0,
        bce,
        Some((&entry_facts, tokens)),
    );
    jobs.push((fn_ids[0], ctx));
}

/// Block-based unbox of an owned `Int` value to a raw `i64`, releasing a box (the
/// bridging wrapper has no `Translator`, so this mirrors
/// [`Translator::unbox_int_to_raw`] on a bare builder).
fn wrapper_unbox_int_to_raw(builder: &mut FunctionBuilder, drop_ref: FuncRef, v: Value) -> Value {
    let imm_b = builder.create_block();
    let box_b = builder.create_block();
    let merge_b = builder.create_block();
    builder.append_block_param(merge_b, types::I64);
    let bit = builder.ins().band_imm(v, 1);
    builder.ins().brif(bit, imm_b, &[], box_b, &[]);

    builder.switch_to_block(imm_b);
    builder.seal_block(imm_b);
    let imm = builder.ins().sshr_imm(v, 1);
    builder.ins().jump(merge_b, &[imm.into()]);

    builder.switch_to_block(box_b);
    builder.seal_block(box_b);
    let off = i32::try_from(rt::INT_VALUE_OFFSET).expect("int value offset");
    let val = builder.ins().load(types::I64, MemFlags::trusted(), v, off);
    builder.ins().call(drop_ref, &[v]);
    builder.ins().jump(merge_b, &[val.into()]);

    builder.switch_to_block(merge_b);
    builder.seal_block(merge_b);
    builder.block_params(merge_b)[0]
}

/// Block-based tag/box of a raw `i64` to the uniform `Int` representation (mirrors
/// [`Translator::box_or_tag_int`] on a bare builder).
fn wrapper_box_or_tag_int(
    builder: &mut FunctionBuilder,
    box_int_ref: FuncRef,
    raw: Value,
) -> Value {
    let box_b = builder.create_block();
    let merge_b = builder.create_block();
    builder.append_block_param(merge_b, types::I64);
    let (shifted, overflow) = builder.ins().sadd_overflow(raw, raw);
    let tagged = builder.ins().bor_imm(shifted, 1);
    builder.ins().brif(overflow, box_b, &[], merge_b, &[tagged.into()]);

    builder.switch_to_block(box_b);
    builder.seal_block(box_b);
    let call = builder.ins().call(box_int_ref, &[raw]);
    let boxed = builder.inst_results(call)[0];
    builder.ins().jump(merge_b, &[boxed.into()]);

    builder.switch_to_block(merge_b);
    builder.seal_block(merge_b);
    builder.block_params(merge_b)[0]
}

/// Builds the uniform-ABI wrapper bridging the static-closure / `apply_n` path
/// (all arguments boxed and owned, in the `args` array) to a specialized entry,
/// then drops the borrowed (non-unboxed) arguments the entry left untouched and
/// re-boxes/tags an unboxed result.
///
/// For a **register** entry it loads each boxed/tagged argument and passes it in a
/// register — a scalar float unboxed to an `f64` and a monomorphic int untagged to
/// a raw `i64` (both releasing any box), every other argument the word itself — and
/// calls `entry(env, a0, …)`, boxing/tagging an unboxed-float/int result back to
/// the uniform word. For a **uniform** entry (a row-polymorphic or nullary
/// definition that still has an unboxed-float slot — ints stay tagged on the
/// uniform ABI) it passes the argument array, replacing each unboxed-float slot
/// with the box's raw bits in a fresh array (releasing the box). Returns the
/// uncompiled context (the caller compiles and defines it).
fn build_owned_wrapper<M: Module>(
    module: &mut M,
    entry: FuncId,
    borrowed: &[bool],
    abi: &FnAbi,
    arity: usize,
    base: &str,
) -> Context {
    let entry_sig = entry_signature(module, arity, abi);
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
        let fields_off = i32::try_from(rt::DATA_FIELDS_OFFSET).expect("data fields offset");
        let entry_ref = module.declare_func_in_func(entry, builder.func);

        // Niche values the wrapper converted for a borrowed niche parameter; the
        // entry borrows them, so the wrapper drops them after the call.
        let mut lent_niche: Vec<Value> = Vec::new();
        let spread_ret = abi.spread_return().map(<[_]>::len);
        let results = if abi.register_abi {
            // Register entry: load each boxed/tagged argument and pass it in a
            // register — a scalar float unboxed to `f64`, a monomorphic int untagged
            // to a raw `i64` (both releasing any box), a spread aggregate exploded
            // into its component `f64` registers, else the word.
            let mut call_args = Vec::with_capacity(arity + 1);
            call_args.push(env);
            for i in 0..arity {
                let offset = i32::try_from(i * 8).expect("arg offset");
                let orig = builder.ins().load(types::I64, MemFlags::trusted(), args, offset);
                if let Some(reprs) = abi.spread_param(i) {
                    // Explode the boxed aggregate into its scalar-slot `f64`s, then
                    // release the box (an owned argument the entry consumes).
                    for j in 0..reprs.len() {
                        let off = fields_off + i32::try_from(j * 8).expect("field offset");
                        let bits = builder.ins().load(types::I64, MemFlags::trusted(), orig, off);
                        let f = builder.ins().bitcast(types::F64, MemFlags::new(), bits);
                        call_args.push(f);
                    }
                    builder.ins().call(drop_ref, &[orig]);
                    continue;
                }
                let v = if abi.float_param(i) {
                    let bits = builder.ins().load(types::I64, MemFlags::trusted(), orig, float_off);
                    builder.ins().call(drop_ref, &[orig]);
                    builder.ins().bitcast(types::F64, MemFlags::new(), bits)
                } else if abi.int_param(i) {
                    wrapper_unbox_int_to_raw(&mut builder, drop_ref, orig)
                } else if let Some(k) = abi.niche_param(i) {
                    // Convert the standard `Option` argument to the niche encoding the
                    // register entry expects. A borrowed niche parameter's converted
                    // value is owned by the wrapper and dropped after the call.
                    let v = wrapper_std_to_niche(module, &mut builder, k, orig);
                    if borrowed.get(i).copied().unwrap_or(false) {
                        lent_niche.push(v);
                    }
                    v
                } else {
                    orig
                };
                call_args.push(v);
            }
            // The entry signature was rebuilt above; declare the call against it so a
            // multi-result spread return reads all components.
            let _ = &entry_sig;
            let call = builder.ins().call(entry_ref, &call_args);
            builder.inst_results(call).to_vec()
        } else {
            // Uniform entry: pass the argument array. For unboxed-float parameters,
            // replace the boxed argument with its raw bits in a fresh array and
            // release the box; other slots pass through.
            let entry_args = if abi.any_float_param() {
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
            builder.inst_results(call).to_vec()
        };
        let mut result = results[0];

        // Drop the borrowed arguments the entry left untouched; an unboxed-float,
        // untagged-int, niche, or spread argument was already released above / is
        // dropped via `lent_niche`.
        for (i, &borrowed) in borrowed.iter().enumerate() {
            if borrowed
                && !abi.float_param(i)
                && !abi.int_param(i)
                && abi.niche_param(i).is_none()
                && abi.spread_param(i).is_none()
            {
                let offset = i32::try_from(i * 8).expect("arg offset");
                let v = builder.ins().load(types::I64, MemFlags::trusted(), args, offset);
                builder.ins().call(drop_ref, &[v]);
            }
        }
        for v in lent_niche {
            builder.ins().call(drop_ref, &[v]);
        }

        // A spread result: the entry returned N `f64` components; reassemble them
        // into a boxed scalar-slot cell (the in-cell `f64` layout) for the uniform
        // first-class result.
        if let Some(n) = spread_ret {
            let size = u32::try_from(n * 8).expect("array size");
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                size,
                3,
            ));
            for (j, &c) in results.iter().take(n).enumerate() {
                let bits = builder.ins().bitcast(types::I64, MemFlags::new(), c);
                builder.ins().stack_store(bits, slot, i32::try_from(j * 8).expect("slot offset"));
            }
            let ptr = builder.ins().stack_addr(types::I64, slot, 0);
            let scalars = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
            let desc = wrapper_descriptor(module, &mut builder, base, scalars);
            let tag = builder.ins().iconst(types::I64, 0);
            let count = builder.ins().iconst(types::I64, n as i64);
            let mut sig = module.make_signature();
            for _ in 0..4 {
                sig.params.push(AbiParam::new(types::I64));
            }
            sig.returns.push(AbiParam::new(types::I64));
            let id = module
                .declare_function("fai_make_data_scalar", Linkage::Import, &sig)
                .expect("declare make_data_scalar");
            let fref = module.declare_func_in_func(id, builder.func);
            let call = builder.ins().call(fref, &[desc, tag, count, ptr]);
            result = builder.inst_results(call)[0];
        }

        // Box a float result back into the uniform representation: a register entry
        // returns an `f64`, a uniform entry returns its raw bits.
        if abi.float_return() {
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

        // Tag/box a raw int result (a register int entry returns it untagged) back
        // into the uniform representation. Only the register ABI carries raw ints.
        if abi.int_return() {
            let mut box_sig = module.make_signature();
            box_sig.params.push(AbiParam::new(types::I64));
            box_sig.returns.push(AbiParam::new(types::I64));
            let box_id = module
                .declare_function("fai_box_int", Linkage::Import, &box_sig)
                .expect("declare box int");
            let box_int_ref = module.declare_func_in_func(box_id, builder.func);
            result = wrapper_box_or_tag_int(&mut builder, box_int_ref, result);
        }

        // Convert a niche result (a register entry returns the wrapper-free encoding)
        // back to the standard `Option` for the uniform first-class result.
        if let Some(k) = abi.niche_return() {
            result = wrapper_niche_to_std(module, &mut builder, k, result);
        }

        builder.ins().return_(&[result]);
        builder.finalize();
    }
    ctx
}

/// Emits a per-shape data descriptor `{ kind = Data, scalar_bitmap, name = null }`
/// for the first-class wrapper (the bare-builder peer of
/// [`Translator::data_descriptor`]; a distinct symbol from the entry's, but with
/// the same content, since the runtime dispatches on content, not address).
fn wrapper_descriptor<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    base: &str,
    bitmap: u64,
) -> Value {
    let name = format!("{base}__owned__desc{bitmap}");
    let id = module.declare_data(&name, Linkage::Local, false, false).expect("declare descriptor");
    let mut bytes = vec![0u8; 32];
    bytes[0..8].copy_from_slice(&rt::KIND_DATA.to_le_bytes());
    bytes[8..16].copy_from_slice(&bitmap.to_le_bytes());
    let mut desc = DataDescription::new();
    desc.define(bytes.into_boxed_slice());
    desc.set_align(8);
    module.define_data(id, &desc).expect("define descriptor");
    let gv = module.declare_data_in_func(id, builder.func);
    let ptr = module.target_config().pointer_type();
    builder.ins().symbol_value(ptr, gv)
}

/// Calls a one-argument, one-result runtime conversion `name` on `v` in the bare
/// (no-`Translator`) wrapper builder.
fn wrapper_convert<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    name: &str,
    v: Value,
) -> Value {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64));
    sig.returns.push(AbiParam::new(types::I64));
    let id = module.declare_function(name, Linkage::Import, &sig).expect("declare conversion");
    let r = module.declare_func_in_func(id, builder.func);
    let call = builder.ins().call(r, &[v]);
    builder.inst_results(call)[0]
}

/// Converts a standard `Option` to the niche representation of scheme `k` in the
/// wrapper.
fn wrapper_std_to_niche<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    k: NicheKind,
    v: Value,
) -> Value {
    match k {
        NicheKind::A => wrapper_convert(module, builder, "fai_std_to_niche_a", v),
        NicheKind::B => wrapper_convert(module, builder, "fai_std_to_niche_b", v),
    }
}

/// Converts a niche `Option` of scheme `k` back to the standard representation in
/// the wrapper.
fn wrapper_niche_to_std<M: Module>(
    module: &mut M,
    builder: &mut FunctionBuilder,
    k: NicheKind,
    v: Value,
) -> Value {
    match k {
        NicheKind::A => wrapper_convert(module, builder, "fai_niche_a_to_std", v),
        NicheKind::B => wrapper_convert(module, builder, "fai_niche_b_to_std", v),
    }
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
    // One parameter group per runtime parameter (a spread aggregate is N `f64`s);
    // a parameter beyond the ABI's reprs (a body with more parameters than the
    // signature's arrows) is a uniform word, matching the binding loop.
    for i in 0..arity {
        let r = abi.params.get(i).cloned().unwrap_or(Repr::Uniform);
        for t in repr_types(&r) {
            sig.params.push(AbiParam::new(t));
        }
    }
    for t in repr_types(&abi.ret) {
        sig.returns.push(AbiParam::new(t));
    }
    sig
}

/// The Cranelift register type(s) a parameter/result representation occupies: a
/// scalar `Float` is one `f64`; a **spread** float aggregate is N `f64`s (a
/// multi-register parameter / multi-result return); every other representation is
/// one `i64` word.
fn repr_types(r: &Repr) -> Vec<types::Type> {
    match r {
        Repr::ScalarFloat => vec![types::F64],
        Repr::Spread(components) => components.iter().flat_map(repr_types).collect(),
        Repr::Uniform | Repr::ScalarInt | Repr::Niche(_) => vec![types::I64],
    }
}

/// The calling convention of a token-taking specialized entry: `fn(env, t0, …,
/// t_{k-1}, a0, …, aN) -> ret`, where each `t` is an `i64` reuse token and the
/// `a`/`ret` follow the primary entry's `abi` (a scalar `Float` in an `f64`
/// register). Built identically by the callee (defining `{base}__reuse`) and a
/// forwarding caller, so both agree on the signature from the slot count and the
/// callee's ABI.
fn reuse_entry_signature<M: Module>(
    module: &M,
    tokens: usize,
    source_arity: usize,
    abi: &FnAbi,
) -> cranelift_codegen::ir::Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // env (unused: captures nothing)
    for _ in 0..tokens {
        sig.params.push(AbiParam::new(types::I64)); // a reuse token (raw i64)
    }
    for i in 0..source_arity {
        let r = abi.params.get(i).cloned().unwrap_or(Repr::Uniform);
        for t in repr_types(&r) {
            sig.params.push(AbiParam::new(t));
        }
    }
    for t in repr_types(&abi.ret) {
        sig.returns.push(AbiParam::new(t));
    }
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
    borrows_of: &dyn Fn(DefId) -> Vec<bool>,
    abi: &FnAbi,
    fn_ids: &[FuncId],
    lambda_closures: &[Option<DataId>],
    base: &str,
    fn_index: usize,
    bce: &Bce,
    entry_seed: Option<(&BoundSig, usize)>,
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
        // function reads them from the `args` array (the second block parameter). A
        // spread parameter occupies several consecutive registers, so all registers
        // after `env` are collected and walked per the ABI below.
        let reg_params: Vec<Value> =
            if register_entry { builder.block_params(entry)[1..].to_vec() } else { Vec::new() };
        let args = if register_entry { env } else { builder.block_params(entry)[1] };

        let mut tr = Translator {
            module,
            builder,
            namer,
            arity_of,
            signature_of,
            borrows_of,
            fn_ids,
            lambda_closures,
            closure_locals: closure_local_funcs(&core_fn.body),
            lowered,
            base,
            fn_index,
            vars: FxHashMap::default(),
            var_tys: FxHashMap::default(),
            f64_locals: FxHashSet::default(),
            int_locals: FxHashSet::default(),
            raw_int_values: FxHashSet::default(),
            niche_locals: FxHashMap::default(),
            niche_values: FxHashMap::default(),
            runtime: FxHashMap::default(),
            string_counter: 0,
            descriptors: FxHashMap::default(),
            loop_ctx: None,
            result_slot: None,
            bounds: Bounds::new(),
            entry_bounds: Bounds::new(),
            result_facts_of: bce.result_of,
            bce_shadow: bce.shadow,
            pool_heads_base: None,
            array_float_tag: FxHashMap::default(),
            array_tag_cacheable: false,
        };

        // Record each local's static type up front, so reference-count operations
        // on parameters and captures (not just `let`s) can be specialized.
        collect_local_types(&core_fn.body, &mut tr.var_tys);
        // Decide which locals are represented as an unboxed `f64` (see
        // `f64_locals`), so their Cranelift variables are typed `F64`. The entry's
        // raw-bits float parameters are unboxed by the ABI, so they are included
        // even when otherwise unobserved (e.g. an unused parameter).
        tr.collect_f64_locals(&core_fn.body);
        // Decide which locals are untagged raw `Int`s (see `int_locals`).
        tr.collect_int_locals(&core_fn.body);
        // Decide which locals hold a niche `Option` (scrutinees, plus everything a
        // niche value propagates to; see `niche_locals`). The entry's niche
        // parameters seed the propagation so a forwarded niche parameter is not
        // reverted to standard at entry; a lifted lambda (uniform ABI) has none.
        let param_niche: Vec<(usize, NicheKind)> = if is_entry {
            core_fn
                .params
                .iter()
                .enumerate()
                .filter_map(|(i, p)| abi.niche_param(i).map(|k| (p.index(), k)))
                .collect()
        } else {
            Vec::new()
        };
        tr.collect_niche_locals(&core_fn.body, &param_niche);
        if is_entry {
            // Reconcile the entry's parameters to its ABI. A scalar-`Float`
            // parameter is forced unboxed (its `F64` variable). An `Int` parameter
            // the register ABI passes untagged is forced raw; an int-typed parameter
            // the ABI passes tagged — offset evidence, or any int parameter of a
            // uniform entry (which keeps ints tagged) — is forced out, so its in-body
            // uses read the tagged word.
            for (i, &p) in core_fn.params.iter().enumerate() {
                if abi.float_param(i) {
                    tr.f64_locals.insert(p.index());
                }
                if abi.int_param(i) {
                    tr.int_locals.insert(p.index());
                } else if matches!(tr.var_ty(p), Some(Ty::Con(Con::Int))) {
                    tr.int_locals.remove(&p.index());
                }
                // A spread parameter's component locals are unboxed `f64`s, forced
                // in even if some component is unused (each must receive its
                // register); the aggregate slot `p` itself carries no value.
                if abi.spread_param(i).is_some()
                    && let Some(Some(locals)) = lowered.entry_spread_params.get(i)
                {
                    for &c in locals {
                        tr.f64_locals.insert(c.index());
                    }
                }
            }
        }

        if register_entry {
            // Register entry: parameters arrive in registers, already in their final
            // representation (an `f64` for a scalar float, a raw untagged word for an
            // `int_param`, else the boxed/tagged word). A spread parameter occupies
            // several consecutive registers, bound to its component locals. A
            // direct-callable (top-level) entry captures nothing.
            let mut reg = 0usize;
            for (i, &p) in core_fn.params.iter().enumerate() {
                if let Some(reprs) = abi.spread_param(i) {
                    let n = reprs.len();
                    if let Some(Some(locals)) = lowered.entry_spread_params.get(i) {
                        for (j, &c) in locals.iter().enumerate() {
                            tr.define_var(c, reg_params[reg + j]);
                        }
                    }
                    reg += n;
                    continue;
                }
                let v = reg_params[reg];
                reg += 1;
                if abi.int_param(i) {
                    // The register value is already untagged; record it raw.
                    tr.mark_raw(v);
                }
                if let Some(k) = abi.niche_param(i) {
                    // The register value is already the niche encoding; record it so
                    // `define_var` keeps (or, for a non-scrutinee local, releases) it.
                    tr.mark_niche(v, k);
                }
                tr.define_var(p, v);
            }
            debug_assert!(core_fn.captures.is_empty(), "a register entry captures nothing");
        } else {
            // Uniform function: bind parameters from `args` and captures from `env`:
            //  - a uniform entry's raw-bits float parameter is reinterpreted to `f64`;
            //  - any other boxed-`Float` parameter arrives boxed and owned, so its
            //    box is released after its bits are read;
            //  - a raw-`Int` parameter (a lifted lambda whose body uses it as `Int`)
            //    arrives as an owned tagged/boxed word, unboxed to raw (box released);
            //  - a captured float/int is borrowed (the closure still owns the env).
            for (i, &p) in core_fn.params.iter().enumerate() {
                let raw = tr.load_slot(args, i);
                let v = if is_entry && abi.float_param(i) {
                    tr.i64_to_f64(raw)
                } else if tr.is_f64_local(p) {
                    tr.owning_unbox(raw)
                } else if tr.is_int_local(p) {
                    let r = tr.unbox_int_to_raw(raw);
                    tr.mark_raw(r)
                } else {
                    raw
                };
                tr.define_var(p, v);
            }
            for (i, &c) in core_fn.captures.iter().enumerate() {
                let raw = tr.load_slot(env, i);
                let v = if tr.is_f64_local(c) {
                    tr.borrowing_unbox(raw)
                } else if tr.is_int_local(c) {
                    let r = tr.borrow_unbox_int_to_raw(raw);
                    tr.mark_raw(r)
                } else {
                    raw
                };
                tr.define_var(c, v);
            }
        }

        // Seed the bounds-check-elimination fact graph with this entry's inferred
        // facts (mapping each parameter index to its local), so dominating-guard and
        // arithmetic refinement in the body can prove indices in range. The seed is
        // present only for the entry function; the parameter offset skips any leading
        // reuse-token parameters of a specialized entry.
        if let Some((sig, offset)) = entry_seed
            && core_fn.params.len() >= offset
        {
            tr.bounds.seed_entry(sig, &core_fn.params[offset..]);
            tr.entry_bounds = tr.bounds.clone();
        }

        // When the body constructs or grows an `Array`, fetch the thread's
        // free-list heads base once here in the entry block (which dominates the
        // whole body) so the inlined pooled allocation fast path reuses it with no
        // per-allocation call. The base is loop-invariant, so a hot allocation loop
        // pays one `fai_pool_heads` per activation rather than one per cell.
        let uses_array_alloc = body_uses_array_alloc(&core_fn.body);
        if uses_array_alloc {
            let heads = tr.runtime("fai_pool_heads", 0, true);
            let call = tr.builder.ins().call(heads, &[]);
            tr.pool_heads_base = Some(tr.builder.inst_results(call)[0]);
        }
        // A buffer's float self-tag (its descriptor word) is rewritten only by a
        // float `push`'s self-stamp, so a body that allocates or pushes no array
        // never re-tags one: a generic array's self-tag is then loop-invariant and
        // can be computed once instead of on every element touch. Precompute each
        // generic array parameter's self-tag here in the entry block (which
        // dominates the whole body), keyed by the parameter's value so the inline
        // accesses reuse it (see [`Self::array_float_tag`]). A tail loop reassigns
        // its parameters, so its loop-carried arrays are (also) handled per
        // iteration at the `Join` header (see [`Self::join`]).
        tr.array_tag_cacheable = !uses_array_alloc;
        if tr.array_tag_cacheable {
            for &p in &core_fn.params {
                if tr.var_ty(p).is_some_and(is_generic_array) {
                    let base = tr.use_var(p);
                    let tag = tr.array_is_float(base);
                    tr.array_float_tag.insert(base, tag);
                }
            }
        }

        // A spread (fixed-shape float aggregate) result is returned multi-value:
        // the body's tail produces N `f64` components, returned directly with no
        // heap cell (each tail of an `if` returns independently — no merge).
        if let Some(reprs) = abi.spread_return().filter(|_| register_entry) {
            let n = reprs.len();
            tr.spread_return_body(&core_fn.body, n);
            tr.builder.finalize();
            return ctx;
        }
        let result = tr.expr(&core_fn.body);
        // The entry returns: an `f64` register for a register float entry; a raw
        // untagged `i64` for a register int entry; raw float bits for a uniform float
        // entry; otherwise the uniform (boxed/tagged) word (which tags a raw int).
        let ret = if register_entry && abi.float_return() {
            tr.f64_return(result)
        } else if register_entry && abi.int_return() {
            tr.as_raw_int(result)
        } else if let Some(k) = abi.niche_return().filter(|_| register_entry) {
            // A niche return (register ABI only — the `abi` describes the *entry*, so
            // this must not touch a lifted lambda's uniform result) hands back the
            // wrapper-free encoding; keep it niche rather than boxing to standard.
            tr.ensure_niche(result, k)
        } else if is_entry && abi.float_return() {
            tr.raw_float_return(result)
        } else {
            tr.boxed_return(result)
        };
        tr.builder.ins().return_(&[ret]);
        tr.builder.finalize();
    }

    ctx
}

/// Maps each local bound directly to a `MakeClosure` to its lifted function — the
/// closures a saturated application can direct-call (see
/// [`Translator::closure_locals`]). Reference counting preserves the `let f =
/// MakeClosure …` binding, so this scan of the emit-ready body finds them all.
fn closure_local_funcs(body: &CExpr) -> FxHashMap<usize, fai_core::ir::FnId> {
    fn go(e: &CExpr, map: &mut FxHashMap<usize, fai_core::ir::FnId>) {
        match &e.kind {
            ExprKind::Let { local, value, body } => {
                if let ExprKind::MakeClosure { func, .. } = &value.kind {
                    map.insert(local.index(), *func);
                }
                go(value, map);
                go(body, map);
            }
            ExprKind::If { cond, then, els } => {
                go(cond, map);
                go(then, map);
                go(els, map);
            }
            ExprKind::App { func, args, .. } => {
                go(func, map);
                args.iter().for_each(|a| go(a, map));
            }
            ExprKind::Prim { args, .. }
            | ExprKind::MakeData { args, .. }
            | ExprKind::Spread { components: args } => {
                args.iter().for_each(|a| go(a, map));
            }
            ExprKind::Recur { args } => args.iter().for_each(|a| go(a, map)),
            ExprKind::DataTag { base: b, .. } | ExprKind::DataField { base: b, .. } => go(b, map),
            ExprKind::Reset { value, body, .. } | ExprKind::LetMany { value, body, .. } => {
                go(value, map);
                go(body, map);
            }
            ExprKind::FreeReuse { body, .. }
            | ExprKind::Dup { body, .. }
            | ExprKind::Drop { body, .. }
            | ExprKind::Join { body, .. }
            | ExprKind::HoleStart { body, .. } => go(body, map),
            ExprKind::HoleFill { cell, .. } => go(cell, map),
            ExprKind::HoleClose { base, .. } => go(base, map),
            ExprKind::Lit(_)
            | ExprKind::Local(_)
            | ExprKind::Global(_)
            | ExprKind::MakeClosure { .. }
            | ExprKind::Error => {}
        }
    }
    let mut map = FxHashMap::default();
    go(body, &mut map);
    map
}

/// Declares the static-closure data symbol for each lifted function that captures
/// nothing, parallel to a definition's function list (`Some` ⇒ a non-capturing
/// lambda; the entry at index 0 and every capturing lambda are `None`). The
/// caller defines the `Some` entries with [`define_static_closure`] (the primary
/// object) or declares them `Import` (the reuse-entry object, which shares the
/// primary's cells).
fn lambda_closure_data<M: Module>(
    module: &mut M,
    lowered: &LoweredDef,
    base: &str,
    linkage: Linkage,
) -> Vec<Option<DataId>> {
    lowered
        .fns
        .iter()
        .enumerate()
        .map(|(i, f)| {
            (i != 0 && f.captures.is_empty()).then(|| {
                module
                    .declare_data(&format!("{base}__fn{i}__closure"), linkage, true, false)
                    .expect("declare lambda closure data")
            })
        })
        .collect()
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
    /// A callee's per-parameter borrow flags (its `entry_borrowed`). A borrowed
    /// parameter is lent by a direct caller and not dropped by the callee, so the
    /// caller drops a box it freshly created for a scalar argument after the call.
    borrows_of: &'a dyn Fn(DefId) -> Vec<bool>,
    fn_ids: &'a [FuncId],
    /// The static-closure data symbol for each lifted function that captures
    /// nothing (`Some` ⇒ non-capturing), parallel to [`Self::fn_ids`]. A
    /// non-capturing lambda needs no per-activation environment, so its
    /// `MakeClosure` references this shared immortal closure instead of allocating
    /// a cell. A capturing lambda (and the entry, index 0) is `None`.
    lambda_closures: &'a [Option<DataId>],
    /// Each local bound directly to a `MakeClosure`, mapped to its lifted function.
    /// A saturated application of such a local is a **direct call** to that lifted
    /// function (its environment read from the closure cell), skipping the runtime
    /// `apply_n` dispatch — the same machine call `apply_n` would make, minus the
    /// descriptor/arity checks and the indirect code pointer.
    closure_locals: FxHashMap<usize, fai_core::ir::FnId>,
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
    /// Locals represented as an **untagged** `i64` (a raw machine integer, not a
    /// low-bit-tagged immediate or heap box): a monomorphic `Int` local whose
    /// *every* observation is `Int`. A raw int flows in registers/locals and
    /// carries no reference count (its `Dup`/`Drop` are no-ops); it is tagged (or
    /// boxed on >63-bit overflow) only where it crosses a uniform slot. The
    /// Cranelift variable is still `I64` — indistinguishable from a tagged word by
    /// type — so raw-ness of individual values is tracked separately in
    /// [`Self::raw_int_values`]. Built from the recorded local types (and, for the
    /// entry, reconciled to the parameter ABI); see [`Translator::collect_int_locals`].
    int_locals: FxHashSet<usize>,
    /// The Cranelift values currently known to hold a **raw, untagged `Int`** (the
    /// explicit analogue of the free [`Self::is_f64`] type test, which cannot
    /// distinguish a raw `i64` from a tagged one). Every site that produces a raw
    /// int records it here (see [`Self::mark_raw`]); boundary, merge, and inline
    /// arithmetic sites query [`Self::is_raw_int`].
    raw_int_values: FxHashSet<Value>,
    /// Locals that hold a niche `Option` (wrapper-free): the base of a niche-
    /// annotated `DataTag`/`DataField`, which must stay niche for the identity
    /// projection to be correct. Built by [`Translator::collect_niche_locals`]. A
    /// niche value bound to any *other* local is converted to standard at the
    /// binding (uniform slots want the standard representation), so this set need
    /// only capture the match scrutinees.
    niche_locals: FxHashMap<usize, NicheKind>,
    /// The Cranelift values currently known to hold a niche `Option` (the analogue
    /// of [`Self::raw_int_values`]; a niche word is indistinguishable from a
    /// standard one by Cranelift type). Boundary sites query [`Self::niche_of`] to
    /// convert to the standard representation before a value crosses a uniform slot.
    niche_values: FxHashMap<Value, NicheKind>,
    runtime: FxHashMap<&'static str, FuncRef>,
    string_counter: usize,
    /// Per-shape data descriptors emitted for scalar-bearing cells, deduplicated
    /// by their scalar bitmap (one static per distinct bitmap used in this
    /// function). Each is a `{ kind = Data, scalar_bitmap, name = null }` static.
    descriptors: FxHashMap<u64, DataId>,
    /// The enclosing tail-call loop, while translating a `Join` body: where
    /// `Recur` jumps back and where the loop's result exits.
    loop_ctx: Option<LoopCtx>,
    /// The destination-passing result slot's address (set by `HoleStart`, read by
    /// `HoleClose`), for a loop that builds a spine.
    result_slot: Option<Value>,
    /// The in-flight bounds-check-elimination facts at the current program point:
    /// difference constraints established by entry facts (seeded at the entry),
    /// `let`-bound arithmetic/lengths, and dominating guards. An `Array` access
    /// whose index it proves within `0..len` skips its inline bounds check.
    bounds: Bounds,
    /// The bounds facts as seeded at the function entry (the loop invariant), reset
    /// at a `Join` header so each loop body is analyzed from the entry facts.
    entry_bounds: Bounds,
    /// A callee's result facts (length/bounds of its result relative to its
    /// parameters), consulted when a `let` binds a saturated call so the result's
    /// length threads into the bounds graph.
    result_facts_of: &'a dyn Fn(DefId) -> ResultSig,
    /// When set, an elided bounds check is *retained* but routed to a distinct
    /// abort, so generative testing turns any unsound elision into a loud, located
    /// failure instead of a silent out-of-bounds read. Off in normal builds.
    bce_shadow: bool,
    /// The current thread's size-class free-list heads base (the result of one
    /// `fai_pool_heads` call), computed once in the entry block when this function
    /// inlines an `Array` allocation (construction or push-grow) — `None`
    /// otherwise. The base is loop-invariant (execution is single-threaded), and
    /// the entry block dominates the whole body, so the inlined pool pop/push reuse
    /// this single value with no per-allocation call.
    pool_heads_base: Option<Value>,
    /// A generic (type-variable element) array's precomputed float self-tag
    /// (`array_is_float`), keyed by the array's pointer **value** — so a generic
    /// element access reuses one descriptor load per array value instead of
    /// reloading it on every element touch (the hot path of a generic in-place
    /// sort/traversal). Populated where the value is loop-invariant and dominates
    /// its uses: at the function entry for each generic array parameter, and at a
    /// `Join` (tail-loop) header for each loop-carried generic array parameter (its
    /// per-iteration block-parameter value). Keyed by value rather than by local so
    /// it hits through reference-counting aliases (a borrowed read of the parameter
    /// keeps the parameter's own value). Only populated when the body re-tags no
    /// buffer (it performs no `Array` allocation or `push`, the float-`push`
    /// self-stamp being the only writer of a descriptor word).
    array_float_tag: FxHashMap<Value, Value>,
    /// Whether this body re-tags no buffer (no `Array` allocation or `push`), so a
    /// generic array's self-tag is stable and may be cached in
    /// [`Self::array_float_tag`].
    array_tag_cacheable: bool,
}

/// The active tail-call loop being translated.
struct LoopCtx {
    /// The loop header (the `Recur` back-edge target).
    header: Block,
    /// The loop exit, taking the result as its block parameter.
    exit: Block,
    /// The loop-carried locals, reassigned (in order) by each `Recur`.
    params: Vec<LocalId>,
    /// Whether the loop result is a raw untagged `Int`, fixed by the first tail
    /// value that reaches the exit (see [`Translator::jump_to_exit`]); the exit
    /// block parameter is recorded raw accordingly.
    exit_raw: bool,
    /// The loop result's niche `Option` scheme, fixed by the first tail value that
    /// reaches the exit; later tail values are reconciled to it and the exit block
    /// parameter is recorded niche accordingly.
    exit_niche: Option<NicheKind>,
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

    /// Whether `local` is represented as an untagged raw `i64` (see
    /// [`Self::int_locals`]).
    fn is_int_local(&self, local: LocalId) -> bool {
        self.int_locals.contains(&local.index())
    }

    /// Records which locals are untagged raw `i64`s: a local is raw only when
    /// **every** observation of its type is `Int` (the same conservative rule as
    /// [`Self::collect_f64_locals`]). A local seen with both `Int` and another (or
    /// unknown) type stays tagged, so its variable and the values bound into it
    /// agree. Parameters are reconciled afterwards to the entry's `Int` ABI (see
    /// where the parameter ABI is consulted): an `int_param` is forced raw, and a
    /// tagged-`Int` parameter — i.e. offset evidence — is forced out.
    fn collect_int_locals(&mut self, body: &CExpr) {
        let mut int_seen = FxHashSet::default();
        let mut other_seen = FxHashSet::default();
        collect_int_observations(body, &mut int_seen, &mut other_seen);
        self.int_locals = int_seen.difference(&other_seen).copied().collect();
    }

    /// Whether the Cranelift value `v` is an unboxed `f64`.
    fn is_f64(&self, v: Value) -> bool {
        self.builder.func.dfg.value_type(v) == types::F64
    }

    /// Whether the Cranelift value `v` holds a raw, untagged `Int` (recorded in
    /// [`Self::raw_int_values`]). The explicit analogue of [`Self::is_f64`], needed
    /// because a raw `i64` and a tagged immediate share the Cranelift `I64` type.
    fn is_raw_int(&self, v: Value) -> bool {
        self.raw_int_values.contains(&v)
    }

    /// Records `v` as a raw, untagged `Int` and returns it (for chaining at a
    /// raw-producing site).
    fn mark_raw(&mut self, v: Value) -> Value {
        self.raw_int_values.insert(v);
        v
    }

    /// Records `v` as holding a niche `Option` of scheme `k` and returns it.
    fn mark_niche(&mut self, v: Value, k: NicheKind) -> Value {
        self.niche_values.insert(v, k);
        v
    }

    /// The niche scheme `v` holds, if it is a niche `Option` value.
    fn niche_of(&self, v: Value) -> Option<NicheKind> {
        self.niche_values.get(&v).copied()
    }

    /// The niche scheme of `local`, if it is classified niche (see
    /// [`Self::niche_locals`]).
    fn niche_local(&self, local: LocalId) -> Option<NicheKind> {
        self.niche_locals.get(&local.index()).copied()
    }

    /// Records which locals hold a niche `Option`, so [`Self::use_var`] re-marks
    /// their values niche and [`Self::define_var`] keeps them niche rather than
    /// converting to standard. Sources, combined and then propagated to a fixpoint:
    ///
    /// * **use-based** (mandatory) — the base of every niche-annotated
    ///   `DataTag`/`DataField` (a match scrutinee): the annotation drives the tag
    ///   test / identity projection, so the base *must* be niche.
    /// * **parameter seeds** — a parameter the entry ABI passes in the niche
    ///   encoding, so a forwarded niche parameter is not reverted at entry.
    /// * **def-based propagation** — a local bound to (aliasing, branching to, or
    ///   recurring with) a niche-producing value is itself niche; iterated to a
    ///   fixpoint so an alias chain or a loop back-edge converges.
    ///
    /// Classification is **liberal**: keeping a niche value wrapper-free across
    /// every local, branch merge, and loop carry avoids the niche→standard→niche
    /// round trip (whose niche→standard half heap-allocates a `Some` cell). Both
    /// schemes are propagated; over-classifying a local is sound because
    /// [`Self::define_var`] (and the entry) reconcile a standard source to the niche
    /// encoding with a non-allocating conversion.
    fn collect_niche_locals(&mut self, body: &CExpr, param_niche: &[(usize, NicheKind)]) {
        let mut map: FxHashMap<usize, NicheKind> = FxHashMap::default();
        // Mandatory: the base of every niche-annotated tag test / projection.
        self.collect_niche_uses(body, &mut map);
        // Seed parameters the entry ABI passes already in the niche encoding, so a
        // niche parameter forwarded or merged in the body is not reverted to the
        // standard representation at the entry.
        for &(p, k) in param_niche {
            map.entry(p).or_insert(k);
        }
        // Propagate niche-ness liberally to a fixpoint: a local bound to (aliasing,
        // branching to, or recurring with) a niche value is itself niche. Monotone
        // (classifications are only added), so it converges; over-classifying a
        // local is sound because `define_var`/the entry reconcile any standard
        // source to the niche encoding (a non-allocating `ensure_niche`). This keeps
        // a niche `Option` wrapper-free across control-flow merges and loop carries
        // instead of round-tripping through the standard (heap-allocated) form.
        loop {
            let mut changed = false;
            self.collect_niche_defs(body, None, &mut map, &mut changed);
            if !changed {
                break;
            }
        }
        self.niche_locals = map;
    }

    /// The use-based pass: a `Local` base of a niche `DataTag`/`DataField`.
    fn collect_niche_uses(&self, e: &CExpr, out: &mut FxHashMap<usize, NicheKind>) {
        let note =
            |base: &CExpr, niche: Option<NicheKind>, out: &mut FxHashMap<usize, NicheKind>| {
                if let (ExprKind::Local(l), Some(k)) = (&base.kind, niche) {
                    out.insert(l.index(), k);
                }
            };
        match &e.kind {
            ExprKind::DataTag { base, niche } => {
                note(base, *niche, out);
                self.collect_niche_uses(base, out);
            }
            ExprKind::DataField { base, niche, .. } => {
                note(base, *niche, out);
                self.collect_niche_uses(base, out);
            }
            ExprKind::Lit(_)
            | ExprKind::Local(_)
            | ExprKind::Global(_)
            | ExprKind::MakeClosure { .. }
            | ExprKind::Error => {}
            ExprKind::Prim { args, .. }
            | ExprKind::MakeData { args, .. }
            | ExprKind::Recur { args }
            | ExprKind::Spread { components: args } => {
                args.iter().for_each(|a| self.collect_niche_uses(a, out));
            }
            ExprKind::App { func, args, .. } => {
                self.collect_niche_uses(func, out);
                args.iter().for_each(|a| self.collect_niche_uses(a, out));
            }
            ExprKind::If { cond, then, els } => {
                self.collect_niche_uses(cond, out);
                self.collect_niche_uses(then, out);
                self.collect_niche_uses(els, out);
            }
            ExprKind::Let { value, body, .. }
            | ExprKind::Reset { value, body, .. }
            | ExprKind::LetMany { value, body, .. } => {
                self.collect_niche_uses(value, out);
                self.collect_niche_uses(body, out);
            }
            ExprKind::FreeReuse { body, .. }
            | ExprKind::Dup { body, .. }
            | ExprKind::Drop { body, .. }
            | ExprKind::Join { body, .. }
            | ExprKind::HoleStart { body, .. } => self.collect_niche_uses(body, out),
            ExprKind::HoleFill { cell, .. } => self.collect_niche_uses(cell, out),
            ExprKind::HoleClose { base, .. } => self.collect_niche_uses(base, out),
        }
    }

    /// One monotone propagation pass for the def-based classification: a local
    /// bound to (or a loop parameter fed by) a niche-producing value is itself
    /// niche. `join` is the enclosing loop's parameters, so a `Recur` argument
    /// classifies the matching loop-carried parameter. Sets `changed` when it adds
    /// a classification, so [`Self::collect_niche_locals`] can iterate to a fixpoint
    /// (an alias chain or a loop back-edge may need more than one pass).
    fn collect_niche_defs(
        &self,
        e: &CExpr,
        join: Option<&[LocalId]>,
        out: &mut FxHashMap<usize, NicheKind>,
        changed: &mut bool,
    ) {
        let note = |out: &mut FxHashMap<usize, NicheKind>, local: usize, k, changed: &mut bool| {
            if let std::collections::hash_map::Entry::Vacant(slot) = out.entry(local) {
                slot.insert(k);
                *changed = true;
            }
        };
        match &e.kind {
            ExprKind::Let { local, value, body } => {
                if let Some(k) = self.expr_niche_kind(value, out) {
                    note(out, local.index(), k, changed);
                }
                self.collect_niche_defs(value, join, out, changed);
                self.collect_niche_defs(body, join, out, changed);
            }
            ExprKind::If { cond, then, els } => {
                self.collect_niche_defs(cond, join, out, changed);
                self.collect_niche_defs(then, join, out, changed);
                self.collect_niche_defs(els, join, out, changed);
            }
            ExprKind::Recur { args } => {
                // A `Recur` argument flows into the matching loop-carried parameter,
                // so a niche argument makes that parameter niche.
                if let Some(params) = join {
                    for (p, a) in params.iter().zip(args) {
                        if let Some(k) = self.expr_niche_kind(a, out) {
                            note(out, p.index(), k, changed);
                        }
                    }
                }
                args.iter().for_each(|a| self.collect_niche_defs(a, join, out, changed));
            }
            ExprKind::Prim { args, .. }
            | ExprKind::MakeData { args, .. }
            | ExprKind::Spread { components: args } => {
                args.iter().for_each(|a| self.collect_niche_defs(a, join, out, changed));
            }
            ExprKind::App { func, args, .. } => {
                self.collect_niche_defs(func, join, out, changed);
                args.iter().for_each(|a| self.collect_niche_defs(a, join, out, changed));
            }
            ExprKind::DataTag { base, .. } => self.collect_niche_defs(base, join, out, changed),
            ExprKind::DataField { base, .. } => self.collect_niche_defs(base, join, out, changed),
            // A letmany binds scalar-float result components (never a niche Option).
            ExprKind::Reset { value, body, .. } | ExprKind::LetMany { value, body, .. } => {
                self.collect_niche_defs(value, join, out, changed);
                self.collect_niche_defs(body, join, out, changed);
            }
            // A `Join` introduces a fresh loop scope: its body's `Recur`s feed
            // `params`, so recurse with this join's parameters as the context.
            ExprKind::Join { params, body } => {
                self.collect_niche_defs(body, Some(params), out, changed);
            }
            ExprKind::FreeReuse { body, .. }
            | ExprKind::Dup { body, .. }
            | ExprKind::Drop { body, .. }
            | ExprKind::HoleStart { body, .. } => {
                self.collect_niche_defs(body, join, out, changed);
            }
            ExprKind::HoleFill { cell, .. } => self.collect_niche_defs(cell, join, out, changed),
            ExprKind::HoleClose { base, .. } => self.collect_niche_defs(base, join, out, changed),
            ExprKind::Lit(_)
            | ExprKind::Local(_)
            | ExprKind::Global(_)
            | ExprKind::MakeClosure { .. }
            | ExprKind::Error => {}
        }
    }

    /// The niche scheme a sub-expression's result holds, if any: a niche
    /// construction, a niche-returning saturated direct call, an alias of a niche
    /// local, or the result of an `if`/`let` whose value is niche.
    fn expr_niche_kind(&self, e: &CExpr, map: &FxHashMap<usize, NicheKind>) -> Option<NicheKind> {
        match &e.kind {
            ExprKind::MakeData { niche, .. } => *niche,
            ExprKind::Local(l) => map.get(&l.index()).copied(),
            ExprKind::App { func, args, .. } => {
                if let ExprKind::Global(def) = func.kind {
                    let arity = (self.arity_of)(def);
                    if arity > 0 && args.len() == arity {
                        return (self.signature_of)(def).niche_return();
                    }
                }
                None
            }
            ExprKind::If { then, els, .. } => {
                self.expr_niche_kind(then, map).or_else(|| self.expr_niche_kind(els, map))
            }
            ExprKind::Let { body, .. } => self.expr_niche_kind(body, map),
            // A reference-count or reuse wrapper leaves the wrapped value's
            // representation unchanged: the expression's result is the body's value,
            // so see through to it (reference counting wraps branch results in
            // `drop`/`dup`, which must not hide a niche `Option` from classification).
            ExprKind::Dup { body, .. }
            | ExprKind::Drop { body, .. }
            | ExprKind::Reset { body, .. }
            | ExprKind::FreeReuse { body, .. } => self.expr_niche_kind(body, map),
            _ => None,
        }
    }

    /// Converts an owned niche `Option` value to the standard boxed representation
    /// (and forgets its niche-ness). A niche value crossing into a uniform slot
    /// (a data field, closure environment, `apply_n` argument, uniform return)
    /// passes through here.
    fn niche_to_std(&mut self, v: Value, k: NicheKind) -> Value {
        match k {
            NicheKind::A => self.call1("fai_niche_a_to_std", v),
            NicheKind::B => self.call1("fai_niche_b_to_std", v),
        }
    }

    /// Converts an owned standard `Option` value to the niche representation of
    /// scheme `k`, recording it niche. A value read from a uniform slot into a
    /// niche local passes through here.
    fn std_to_niche(&mut self, v: Value, k: NicheKind) -> Value {
        let sym = match k {
            NicheKind::A => "fai_std_to_niche_a",
            NicheKind::B => "fai_std_to_niche_b",
        };
        let r = self.call1(sym, v);
        self.mark_niche(r, k)
    }

    /// Coerces `v` to the niche representation of scheme `k`: a value already niche
    /// `k` passes through; a standard value is converted.
    fn ensure_niche(&mut self, v: Value, k: NicheKind) -> Value {
        if self.niche_of(v) == Some(k) { v } else { self.std_to_niche(v, k) }
    }

    /// Coerces an owned value to a raw, untagged `Int`: a value already known raw
    /// passes through; any other (a tagged immediate or a boxed `Int`) is untagged
    /// or unboxed-and-released ([`Self::unbox_int_to_raw`]) and recorded raw. For an
    /// **owned** int value (a `let`/argument/return result, or a consumed
    /// `apply_n`/forced-global result).
    fn as_raw_int(&mut self, v: Value) -> Value {
        if self.is_raw_int(v) {
            v
        } else {
            let raw = self.unbox_int_to_raw(v);
            self.mark_raw(raw)
        }
    }

    /// Reads an `Int` field/slot word as a raw `i64` **without** releasing it (a
    /// borrow — the owning cell is dropped later): an immediate (low bit set) is
    /// untagged; a boxed (large) `Int` is read from its value field. Mirrors
    /// [`Self::borrowing_unbox`] for floats.
    fn borrow_unbox_int_to_raw(&mut self, v: Value) -> Value {
        let imm_b = self.builder.create_block();
        let box_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);
        let bit = self.builder.ins().band_imm(v, 1);
        self.builder.ins().brif(bit, imm_b, &[], box_b, &[]);

        self.builder.switch_to_block(imm_b);
        self.builder.seal_block(imm_b);
        let imm = self.builder.ins().sshr_imm(v, 1);
        self.builder.ins().jump(merge_b, &[imm.into()]);

        self.builder.switch_to_block(box_b);
        self.builder.seal_block(box_b);
        let off = i32::try_from(rt::INT_VALUE_OFFSET).expect("int value offset");
        let val = self.builder.ins().load(types::I64, MemFlags::trusted(), v, off);
        self.builder.ins().jump(merge_b, &[val.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
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
    /// `f64` is boxed; a raw untagged `Int` is tagged (or boxed on >63-bit
    /// overflow); anything else is already a tagged word.
    fn ensure_boxed(&mut self, v: Value) -> Value {
        if self.is_f64(v) {
            self.box_float(v)
        } else if self.is_raw_int(v) {
            self.box_or_tag_int(v)
        } else if let Some(k) = self.niche_of(v) {
            // A niche `Option` crossing into a uniform slot becomes a standard cell.
            self.niche_to_std(v, k)
        } else {
            v
        }
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

    /// Binds `local` to `value`, the single coercion point reconciling an owned
    /// value's representation with the local's classification. A raw-`Int` local
    /// takes the value untagged ([`Self::as_raw_int`]); a uniform local takes a raw
    /// int tagged/boxed; an `f64` local relies on Cranelift's `F64` type match (a
    /// mismatch panics, surfacing a bug). A no-op in the common cases (raw→raw in a
    /// normal function, tagged→tagged in a combined function); only a
    /// conflicting-observation local actually converts.
    fn define_var(&mut self, local: LocalId, value: Value) {
        let var = self.var(local);
        let value = if let Some(k) = self.niche_local(local) {
            // A niche scrutinee local holds the niche representation; convert a
            // standard incoming value (e.g. from a generic source or a uniform
            // entry parameter).
            self.ensure_niche(value, k)
        } else if self.niche_of(value).is_some() {
            // A niche value bound to a non-niche local crosses into a uniform slot.
            self.ensure_boxed(value)
        } else if self.is_int_local(local) {
            self.as_raw_int(value)
        } else if !self.is_f64_local(local) && self.is_raw_int(value) {
            self.box_or_tag_int(value)
        } else {
            value
        };
        self.builder.def_var(var, value);
    }

    fn use_var(&mut self, local: LocalId) -> Value {
        let var = self.var(local);
        let v = self.builder.use_var(var);
        // A raw-`Int` local's value is untagged; record it so consumers treat it
        // raw (the variable's `I64` type cannot convey this).
        if self.is_int_local(local) {
            self.mark_raw(v);
        }
        // A niche-`Option` local's value is the wrapper-free encoding; record it so
        // boundary sites convert it back to standard before a uniform slot.
        if let Some(k) = self.niche_local(local) {
            self.mark_niche(v, k);
        }
        v
    }

    /// `local`'s known static type, if recorded (see the `var_tys` pre-pass).
    fn var_ty(&self, local: LocalId) -> Option<&Ty> {
        self.var_tys.get(&local.index())
    }

    /// How to inline a `Dup` of `local`: a no-op for a raw untagged `Int` local (it
    /// carries no reference count, and its bit pattern is not a tag — a tag-check
    /// could misfire and dereference the integer as a pointer); otherwise from its
    /// static type ([`dup_class`]), or a tag-checked increment when the type is
    /// unknown (the safe default).
    fn dup_plan(&self, local: LocalId) -> DupPlan {
        if self.is_int_local(local) {
            return DupPlan::NoOp;
        }
        match self.var_ty(local) {
            Some(ty) => dup_class(ty),
            None => DupPlan::Incr { tag_check: true },
        }
    }

    /// How to inline a `Drop` of `local`: a no-op for a raw untagged `Int` local
    /// (see [`Self::dup_plan`]); otherwise from its static type ([`drop_class`]), or
    /// the runtime drop when the type is unknown (the polymorphic fallback).
    fn drop_plan(&self, local: LocalId) -> DropPlan {
        if self.is_int_local(local) {
            return DropPlan::NoOp;
        }
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
        // A niche Scheme-B `Option`'s `None` is the immortal sentinel, which carries
        // no reference count, so its dup must skip it (a `Some` immediate likewise);
        // only a boxed `Some` payload is incremented.
        if self.niche_local(local) == Some(NicheKind::B) {
            self.dup_niche_b(local);
            return;
        }
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
        // A niche Scheme-B `Option`'s `None` is the immortal sentinel, which carries
        // no reference count, so its drop must skip it (a `Some` immediate likewise);
        // only a boxed `Some` payload is released.
        if self.niche_local(local) == Some(NicheKind::B) {
            self.drop_niche_b(local);
            return;
        }
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

    /// Dups a niche Scheme-B `Option` local: skip an immediate (a `Some` whose
    /// payload is itself immediate) and the immortal `None` sentinel — neither
    /// carries a reference count — and increment only a boxed `Some` payload.
    fn dup_niche_b(&mut self, local: LocalId) {
        let cell = self.use_var(local);
        let sentinel = self.runtime_data_addr("FAI_NONE_VALUE");
        let sentinel_b = self.builder.create_block();
        let incr_b = self.builder.create_block();
        let cont_b = self.builder.create_block();
        // An immediate (low bit set) carries no count.
        let bit = self.builder.ins().band_imm(cell, 1);
        self.builder.ins().brif(bit, cont_b, &[], sentinel_b, &[]);

        // A boxed value: the immortal sentinel is skipped, any other (a boxed `Some`
        // payload) is incremented.
        self.builder.switch_to_block(sentinel_b);
        self.builder.seal_block(sentinel_b);
        let is_sentinel = self.builder.ins().icmp(IntCC::Equal, cell, sentinel);
        self.builder.ins().brif(is_sentinel, cont_b, &[], incr_b, &[]);

        self.builder.switch_to_block(incr_b);
        self.builder.seal_block(incr_b);
        let rc_off = i32::try_from(rt::RC_OFFSET).expect("rc offset");
        let rc = self.builder.ins().load(types::I64, MemFlags::trusted(), cell, rc_off);
        let inc = self.builder.ins().iadd_imm(rc, 1);
        self.builder.ins().store(MemFlags::trusted(), inc, cell, rc_off);
        self.builder.ins().jump(cont_b, &[]);

        self.builder.switch_to_block(cont_b);
        self.builder.seal_block(cont_b);
    }

    /// Drops a niche Scheme-B `Option` local: skip an immediate and the immortal
    /// `None` sentinel, and release only a boxed `Some` payload (its children
    /// recovered by kind at zero).
    fn drop_niche_b(&mut self, local: LocalId) {
        let cell = self.use_var(local);
        let sentinel = self.runtime_data_addr("FAI_NONE_VALUE");
        let sentinel_b = self.builder.create_block();
        let dec_b = self.builder.create_block();
        let dead_b = self.builder.create_block();
        let cont_b = self.builder.create_block();
        // An immediate (low bit set) carries no count.
        let bit = self.builder.ins().band_imm(cell, 1);
        self.builder.ins().brif(bit, cont_b, &[], sentinel_b, &[]);

        // A boxed value: the immortal sentinel is skipped.
        self.builder.switch_to_block(sentinel_b);
        self.builder.seal_block(sentinel_b);
        let is_sentinel = self.builder.ins().icmp(IntCC::Equal, cell, sentinel);
        self.builder.ins().brif(is_sentinel, cont_b, &[], dec_b, &[]);

        // A boxed `Some` payload: decrement, and release its children at zero.
        self.builder.switch_to_block(dec_b);
        self.builder.seal_block(dec_b);
        let rc_off = i32::try_from(rt::RC_OFFSET).expect("rc offset");
        let rc = self.builder.ins().load(types::I64, MemFlags::trusted(), cell, rc_off);
        let dec = self.builder.ins().iadd_imm(rc, -1);
        self.builder.ins().store(MemFlags::trusted(), dec, cell, rc_off);
        let is_dead = self.builder.ins().icmp_imm(IntCC::Equal, dec, 0);
        self.builder.ins().brif(is_dead, dead_b, &[], cont_b, &[]);

        self.builder.switch_to_block(dead_b);
        self.builder.seal_block(dead_b);
        let f = self.runtime("fai_drop_dead", 1, false);
        self.builder.ins().call(f, &[cell]);
        self.builder.ins().jump(cont_b, &[]);

        self.builder.switch_to_block(cont_b);
        self.builder.seal_block(cont_b);
    }

    /// Emits an in-place reference-count increment of `local`. When `tag_check`,
    /// an immediate value (low bit set) skips the increment; an always-boxed type
    /// omits the guard entirely. Leaves the builder in the continuation block.
    fn emit_rc_incr(&mut self, local: LocalId, tag_check: bool) {
        let cell = self.use_var(local);
        self.emit_rc_incr_value(cell, tag_check);
    }

    /// Value-keyed core of [`Self::emit_rc_incr`]: increments `cell`'s reference
    /// count in place (with the optional immediate tag-check). Used both for a
    /// local dup and to dup a value with no backing local — an element read from an
    /// array slot. Leaves the builder in the continuation block.
    fn emit_rc_incr_value(&mut self, cell: Value, tag_check: bool) {
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
        self.emit_rc_dec_then_value(cell, tag_check, dead);
    }

    /// Value-keyed core of [`Self::emit_rc_dec_then`]: decrements `cell` and runs
    /// `dead` when it reaches zero. Used both for a local drop and to release a
    /// value with no backing local — the old element overwritten by an in-place
    /// array `set`. Leaves the builder in the continuation block.
    fn emit_rc_dec_then_value(
        &mut self,
        cell: Value,
        tag_check: bool,
        dead: impl FnOnce(&mut Self, Value),
    ) {
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
        let cell = self.use_var(local);
        self.emit_inline_drop_value(cell, fields);
    }

    /// Value-keyed core of [`Self::emit_inline_drop`]: releases the fixed-shape
    /// cell `cell`. Used both for a local drop and to release a value with no
    /// backing local — a record/tuple old element overwritten by an array `set`.
    fn emit_inline_drop_value(&mut self, cell: Value, fields: &[FieldDrop]) {
        self.emit_rc_dec_then_value(cell, false, |s, cell| {
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

    /// Duplicates a uniform-representation value `v` of static type `ty` inline,
    /// for a value with no backing local — an element read from a borrowed array
    /// slot whose returned reference must outlive the array's drop. Mirrors
    /// [`Self::dup_local`]/[`dup_class`]: a no-op for an immediate, an
    /// unconditional increment for an always-boxed value, else a tag-checked one.
    fn dup_value(&mut self, v: Value, ty: &Ty) {
        match dup_class(ty) {
            DupPlan::NoOp => {}
            DupPlan::Incr { tag_check } => self.emit_rc_incr_value(v, tag_check),
        }
    }

    /// Releases a uniform-representation value `v` of static type `ty` inline, for
    /// a value with no backing local — the old element overwritten by an in-place
    /// array `set`. Uses [`uniform_drop_class`] (which, unlike [`drop_class`],
    /// treats a `Float` as the boxed cell it is in a slot rather than a scalar): a
    /// no-op for an immediate, an inlined leaf/fixed-cell/data release, or a
    /// tag-checked runtime drop for an unknown type (so an immediate element — e.g.
    /// an `Int` behind a type variable in a generic `set` — skips the call).
    fn drop_value(&mut self, v: Value, ty: &Ty) {
        match uniform_drop_class(ty) {
            DropPlan::NoOp => {}
            DropPlan::Fixed(fields) => self.emit_inline_drop_value(v, &fields),
            DropPlan::Leaf { tag_check } => {
                self.emit_rc_dec_then_value(v, tag_check, |s, cell| {
                    let free = s.runtime("fai_free", 1, false);
                    s.builder.ins().call(free, &[cell]);
                });
            }
            DropPlan::Data { tag_check } => {
                self.emit_rc_dec_then_value(v, tag_check, |s, cell| {
                    let f = s.runtime("fai_drop_dead", 1, false);
                    s.builder.ins().call(f, &[cell]);
                });
            }
            DropPlan::Runtime => {
                // An unknown type: guard the runtime drop with an immediate
                // tag-check so a generic immediate element (an `Int` behind a type
                // variable in a generic `set`) skips the call entirely.
                let cont_b = self.builder.create_block();
                let drop_b = self.builder.create_block();
                let bit = self.builder.ins().band_imm(v, 1);
                self.builder.ins().brif(bit, cont_b, &[], drop_b, &[]);
                self.builder.switch_to_block(drop_b);
                self.builder.seal_block(drop_b);
                self.call_drop(v);
                self.builder.ins().jump(cont_b, &[]);
                self.builder.switch_to_block(cont_b);
                self.builder.seal_block(cont_b);
            }
        }
    }

    fn expr(&mut self, e: &CExpr) -> Value {
        match &e.kind {
            ExprKind::Lit(lit) => self.literal(lit, &e.ty),
            ExprKind::Local(local) => self.use_var(*local),
            ExprKind::Global(def) => self.global_value(*def, &e.ty),
            ExprKind::Prim { op, args } => self.prim(*op, args, &e.ty),
            ExprKind::App { func, args, reuse, alloc } => {
                self.application(func, args, reuse, *alloc, &e.ty)
            }
            ExprKind::If { cond, then, els } => self.conditional(cond, then, els),
            ExprKind::Let { local, value, body } => {
                let v = self.expr(value);
                self.define_var(*local, v);
                self.bce_transfer(*local, value);
                self.expr(body)
            }
            ExprKind::MakeClosure { func, captures, alloc } => {
                self.make_closure(*func, captures, *alloc)
            }
            ExprKind::MakeData { tag, args, reuse, scalars, niche } => {
                self.make_data(*tag, args, *reuse, *scalars, *niche)
            }
            // A `LetMany` binds a spread-returning call's result components, then
            // continues. A `Spread` in a value position (rare — SROA normally
            // materializes at boxed sinks) reassembles its components into a cell.
            ExprKind::LetMany { locals, value, body } => {
                self.bind_letmany(locals, value);
                self.expr(body)
            }
            ExprKind::Spread { components } => {
                let n = components.len();
                let scalars = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
                self.make_data(0, components, None, scalars, None)
            }
            ExprKind::DataTag { base, niche } => self.data_tag(base, *niche, &e.ty),
            ExprKind::DataField { base, index, scalar, niche } => {
                self.data_field(base, *index, *scalar, *niche, &e.ty)
            }
            ExprKind::Reset { value, token, body } => {
                let v = self.expr(value);
                let tok = self.call1("fai_drop_reuse", v);
                self.define_var(*token, tok);
                self.expr(body)
            }
            ExprKind::FreeReuse { token, body } => {
                // A branch that builds nothing into the token frees its held cell.
                let tok = self.use_var(*token);
                let f = self.runtime("fai_free_reuse", 1, false);
                self.builder.ins().call(f, &[tok]);
                self.expr(body)
            }
            ExprKind::Dup { local, body } => {
                // The dup is inlined per the local's static type; see `dup_local`
                // (immediate no-op / unconditional / tag-checked increment).
                self.dup_local(*local);
                self.expr(body)
            }
            ExprKind::Drop { local, body } => {
                // Release the local *before* its continuation. Reference counting
                // only drops a value that is dead in `body` (the soundness
                // interpreter verifies exactly this), so dropping first is correct
                // and releases the value as early as the model intends — matching
                // the tail path (`expr_tail`) and `Reset`. Dropping after `body`
                // would keep a matched cell (and any boxed field projected from it,
                // e.g. an `Array` threaded through a tuple-returning recursive sort)
                // shared across the continuation, defeating in-place mutation. See
                // `drop_local` for the immediate / inlined-cell / runtime choice.
                self.drop_local(*local);
                self.expr(body)
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

    fn literal(&mut self, lit: &Lit, ty: &Ty) -> Value {
        match lit {
            Lit::Int(n) => {
                if matches!(ty, Ty::Con(Con::Int)) {
                    // A monomorphic `Int` literal is a raw untagged `i64` — no tag and
                    // no boxing, even beyond the 63-bit immediate range (it is
                    // tagged/boxed only where it crosses a uniform slot).
                    let v = self.builder.ins().iconst(types::I64, *n);
                    self.mark_raw(v)
                } else if fits_immediate(*n) {
                    // An erased/uniform context (e.g. a mutual-recursion combined
                    // function): the tagged immediate, the uniform representation.
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
    ///
    /// `scalars` marks which fields are stored as a raw unboxed `f64` rather than a
    /// uniform word. A scalar field's value is written as its raw bits (no boxing),
    /// and the cell carries a per-shape descriptor with that bitmap so the generic
    /// runtime walkers handle the raw slots; an all-uniform cell (bitmap zero) keeps
    /// the shared descriptor and the plain build.
    fn make_data(
        &mut self,
        tag: u32,
        args: &[CExpr],
        reuse: Option<LocalId>,
        scalars: u64,
        niche: Option<NicheKind>,
    ) -> Value {
        // A niche `Some` (either scheme) is its payload in uniform representation —
        // no wrapper cell.
        if let Some(k) = niche
            && !args.is_empty()
        {
            debug_assert!(reuse.is_none(), "a niche `Some` allocates nothing to reuse");
            let v = self.expr_boxed(&args[0]);
            return self.mark_niche(v, k);
        }
        if args.is_empty() {
            debug_assert!(reuse.is_none(), "nullary constructor cannot reuse a cell");
            // A niche `None`: Scheme A is the nullary immediate `1` (the standard
            // encoding); Scheme B is the shared sentinel — its relocatable address,
            // not a runtime call. The sentinel is immortal and never
            // reference-counted, so a `None` is the bare address (no `dup`).
            if niche == Some(NicheKind::B) {
                let s = self.runtime_data_addr("FAI_NONE_VALUE");
                return self.mark_niche(s, NicheKind::B);
            }
            let imm = (i64::from(tag) << 1) | 1;
            let v = self.builder.ins().iconst(types::I64, imm);
            if niche == Some(NicheKind::A) {
                return self.mark_niche(v, NicheKind::A);
            }
            return v;
        }
        // A scalar field is written as raw `f64` bits; a uniform field is boxed in.
        let vals: Vec<Value> = args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                if i < 64 && scalars & (1u64 << i) != 0 {
                    let v = self.expr(a);
                    self.float_field_bits(v)
                } else {
                    self.expr_boxed(a)
                }
            })
            .collect();
        let count = vals.len();
        let ptr = self.spill(&vals);
        let tag_v = self.builder.ins().iconst(types::I64, i64::from(tag));
        let n_v = self.builder.ins().iconst(types::I64, count as i64);
        // A scalar-bearing cell carries a per-shape descriptor; an all-uniform cell
        // uses the shared descriptor (the plain runtime entry points).
        let desc = if scalars != 0 { Some(self.data_descriptor(scalars)) } else { None };
        match (reuse, desc) {
            (Some(token), Some(desc)) => {
                let tok = self.use_var(token);
                let f = self.runtime("fai_reuse_scalar", 5, true);
                let call = self.builder.ins().call(f, &[desc, tok, tag_v, n_v, ptr]);
                self.builder.inst_results(call)[0]
            }
            (Some(token), None) => {
                let tok = self.use_var(token);
                let f = self.runtime("fai_reuse", 4, true);
                let call = self.builder.ins().call(f, &[tok, tag_v, n_v, ptr]);
                self.builder.inst_results(call)[0]
            }
            (None, Some(desc)) => {
                let f = self.runtime("fai_make_data_scalar", 4, true);
                let call = self.builder.ins().call(f, &[desc, tag_v, n_v, ptr]);
                self.builder.inst_results(call)[0]
            }
            (None, None) => {
                let f = self.runtime("fai_make_data", 3, true);
                let call = self.builder.ins().call(f, &[tag_v, n_v, ptr]);
                self.builder.inst_results(call)[0]
            }
        }
    }

    /// Coerces an owned value into the raw `f64` bits stored in a scalar field
    /// slot: an unboxed `f64` is bit-reinterpreted; a boxed `Float` (a generic
    /// value flowing into a concrete scalar field, e.g. a generated test value) has
    /// its bits read and its box released.
    fn float_field_bits(&mut self, v: Value) -> Value {
        if self.is_f64(v) {
            self.f64_to_i64(v)
        } else {
            let off = i32::try_from(rt::FLOAT_VALUE_OFFSET).expect("float value offset");
            let bits = self.builder.ins().load(types::I64, MemFlags::trusted(), v, off);
            self.call_drop(v);
            bits
        }
    }

    /// Emits (once per scalar bitmap) a static data descriptor
    /// `{ kind = Data, scalar_bitmap, name = null }` and yields its address.
    fn data_descriptor(&mut self, bitmap: u64) -> Value {
        let ptr = self.ptr();
        let data_id = if let Some(&id) = self.descriptors.get(&bitmap) {
            id
        } else {
            let name = format!("{}__fn{}__desc{bitmap}", self.base, self.fn_index);
            // Layout mirrors `fai_runtime::Descriptor` (repr C):
            // kind: u64, scalar_bitmap: u64, name_ptr: usize, name_len: usize.
            let mut bytes = vec![0u8; 32];
            bytes[0..8].copy_from_slice(&rt::KIND_DATA.to_le_bytes());
            bytes[8..16].copy_from_slice(&bitmap.to_le_bytes());
            // name_ptr / name_len stay zero (a generated descriptor has no name).
            let id = self
                .module
                .declare_data(&name, Linkage::Local, false, false)
                .expect("declare descriptor");
            let mut desc = DataDescription::new();
            desc.define(bytes.into_boxed_slice());
            desc.set_align(8);
            self.module.define_data(id, &desc).expect("define descriptor");
            self.descriptors.insert(bitmap, id);
            id
        };
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        self.builder.ins().symbol_value(ptr, gv)
    }

    /// The raw slot index of a row-polymorphic field: the statically known
    /// preceding-field count plus the offset-evidence local's value. Evidence is
    /// normally a tagged immediate (untagged here); a raw evidence local (a captured
    /// evidence used as `Int` in a nested lambda) is read directly.
    fn evidence_slot(&mut self, off: u32, evidence: LocalId) -> Value {
        let ev = self.use_var(evidence);
        let base =
            if self.is_int_local(evidence) { ev } else { self.builder.ins().sshr_imm(ev, 1) };
        self.builder.ins().iadd_imm(base, i64::from(off))
    }

    /// Projects a field of a data value (consuming `base`). A constant slot is an
    /// immediate; a row-polymorphic slot is `base + evidence` computed at runtime
    /// from a leading offset-evidence parameter.
    ///
    /// `scalar` marks the slot as a raw unboxed `f64` (a record/tuple/concrete-ADT
    /// `Float` field): its bits are read directly. A non-scalar `Float` result is a
    /// *boxed* `Float` slot (a `List`/polymorphic-ADT element instantiated at
    /// `Float`), unboxed in place; a monomorphic `Int` result is read untagged.
    /// Every read borrows `base` (it outlives the read in A-normal form, and
    /// dropping `base` later releases the field once).
    /// Reads a data value's constructor tag (consuming `base`), as an `Int`. For a
    /// niche Scheme-A `Option` the tag is computed from the encoding — `None` is the
    /// immediate `1` (low bit set), `Some` a boxed pointer (low bit clear), so the
    /// tag is `(v & 1) ^ 1` (0 for `None`, 1 for `Some`) — rather than a header read.
    fn data_tag(&mut self, base: &CExpr, niche: Option<NicheKind>, result_ty: &Ty) -> Value {
        let v = self.expr(base);
        if let Some(k) = niche {
            // The tag is 0 for `None`, 1 for `Some`. Scheme A: `None` is the
            // immediate `1` (low bit set), `Some` a boxed pointer (clear), so the tag
            // is `(v & 1) ^ 1`. Scheme B: `None` is the sentinel, so the tag is
            // `v != sentinel`.
            let raw = match k {
                NicheKind::A => {
                    let lowbit = self.builder.ins().band_imm(v, 1);
                    self.builder.ins().bxor_imm(lowbit, 1)
                }
                NicheKind::B => {
                    let sentinel = self.runtime_data_addr("FAI_NONE_VALUE");
                    let is_some = self.builder.ins().icmp(IntCC::NotEqual, v, sentinel);
                    self.builder.ins().uextend(types::I64, is_some)
                }
            };
            return if matches!(result_ty, Ty::Con(Con::Int)) {
                self.mark_raw(raw)
            } else {
                let shifted = self.builder.ins().ishl_imm(raw, 1);
                self.builder.ins().bor_imm(shifted, 1)
            };
        }
        let tagged = self.call1("fai_data_tag", v);
        // The tag is a small immediate `Int`. Where the node is monomorphic `Int`
        // (the normal match desugaring), deliver it raw so the tag test is a bare
        // comparison against the raw constructor-tag literal; in an erased/uniform
        // context (a combined function) keep it tagged.
        if matches!(result_ty, Ty::Con(Con::Int)) {
            let raw = self.untag(tagged);
            self.mark_raw(raw)
        } else {
            tagged
        }
    }

    fn data_field(
        &mut self,
        base: &CExpr,
        index: FieldIndex,
        scalar: bool,
        niche: Option<NicheKind>,
        result_ty: &Ty,
    ) -> Value {
        if let Some(k) = niche {
            // A niche `Some` projection: the value *is* the payload, so the
            // projection is the identity (no header load). The result is the payload
            // (standard), not a niche value. A Scheme-B `Int` payload is read raw,
            // borrowing the base (the base's later drop releases any box); every
            // other payload is duplicated (it outlives the base's drop), as the
            // generic field read does.
            let v = self.expr(base);
            if k == NicheKind::B && matches!(result_ty, Ty::Con(Con::Int)) {
                let raw = self.borrow_unbox_int_to_raw(v);
                return self.mark_raw(raw);
            }
            return self.call1("fai_dup", v);
        }
        if scalar {
            return self.scalar_float_data_field(base, index);
        }
        if matches!(result_ty, Ty::Con(Con::Float)) {
            return self.float_data_field(base, index);
        }
        if matches!(result_ty, Ty::Con(Con::Int)) {
            return self.int_data_field(base, index);
        }
        let v = self.expr(base);
        let idx = match index {
            FieldIndex::Const(n) => self.builder.ins().iconst(types::I64, i64::from(n)),
            FieldIndex::Dyn { base: off, evidence } => self.evidence_slot(off, evidence),
        };
        let f = self.runtime("fai_data_field", 2, true);
        let call = self.builder.ins().call(f, &[v, idx]);
        self.builder.inst_results(call)[0]
    }

    /// Reads a scalar `Float` field as an unboxed `f64`: the slot holds the raw
    /// bits directly (no box), so load and reinterpret.
    fn scalar_float_data_field(&mut self, base: &CExpr, index: FieldIndex) -> Value {
        let base_v = self.expr(base);
        let addr = self.field_slot_addr(base_v, index);
        let bits = self.builder.ins().load(types::I64, MemFlags::trusted(), addr, 0);
        self.i64_to_f64(bits)
    }

    /// The byte address of a field slot within a data cell at `base_v`.
    fn field_slot_addr(&mut self, base_v: Value, index: FieldIndex) -> Value {
        match index {
            FieldIndex::Const(n) => {
                let off =
                    i64::try_from(rt::DATA_FIELDS_OFFSET + n as usize * 8).expect("field off");
                self.builder.ins().iadd_imm(base_v, off)
            }
            FieldIndex::Dyn { base: off, evidence } => {
                let fields_off = i64::try_from(rt::DATA_FIELDS_OFFSET).expect("fields offset");
                let slot = self.evidence_slot(off, evidence);
                let byte = self.builder.ins().imul_imm(slot, 8);
                let byte = self.builder.ins().iadd_imm(byte, fields_off);
                self.builder.ins().iadd(base_v, byte)
            }
        }
    }

    /// Reads a scalar `Float` field as an unboxed `f64`: load the field slot's box
    /// pointer at its (constant or evidence-computed) offset, then read the box's
    /// bits without touching its reference count (a borrow).
    fn float_data_field(&mut self, base: &CExpr, index: FieldIndex) -> Value {
        let base_v = self.expr(base);
        let addr = self.field_slot_addr(base_v, index);
        let boxed = self.builder.ins().load(types::I64, MemFlags::trusted(), addr, 0);
        self.borrowing_unbox(boxed)
    }

    /// Reads a monomorphic `Int` field as a raw untagged `i64`: load the field slot
    /// word (a tagged immediate or a boxed-`Int` pointer) and untag/unbox it without
    /// releasing it (a borrow — the cell is dropped later).
    fn int_data_field(&mut self, base: &CExpr, index: FieldIndex) -> Value {
        let base_v = self.expr(base);
        let addr = self.field_slot_addr(base_v, index);
        let word = self.builder.ins().load(types::I64, MemFlags::trusted(), addr, 0);
        let raw = self.borrow_unbox_int_to_raw(word);
        self.mark_raw(raw)
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
            // A forced value binding comes back in the uniform representation and
            // owned: a `Float` is unboxed to `f64`, a monomorphic `Int` is untagged
            // to a raw `i64` (releasing any box), both consuming the owned result.
            match result_ty {
                Ty::Con(Con::Float) => self.owning_unbox(forced),
                Ty::Con(Con::Int) => self.as_raw_int(forced),
                _ => forced,
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
        // `Array` length/get/set/push on a statically-recognized array operand
        // compile to inline loads/stores (with an inline bounds check + located
        // abort), removing the per-access call + index unbox + dup/drop. A
        // non-array/erased operand falls through to the runtime call below.
        if let Some(v) = self.array_prim(op, args, result_ty) {
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
        let result = self.builder.inst_results(call)[0];
        // A primitive that yields a scalar `Float` through the uniform runtime ABI
        // (`arrayGet` of a `Float` element) returns a boxed word; unbox it to the
        // `f64` an unboxed-`Float` context expects, mirroring the result coercion a
        // direct call applies to a generic callee's boxed `Float`. This matters once
        // such a primitive appears at a monomorphic-`Float` call site (an inlined
        // `Array.unsafeGet` on an `Array Float`). `Int` needs no coercion — raw and
        // tagged are both `i64`, so there is no Cranelift type mismatch.
        if matches!(result_ty, Ty::Con(Con::Float)) && !self.is_f64(result) {
            self.owning_unbox(result)
        } else {
            result
        }
    }

    /// Applies a binding `local = value` to the bounds-check-elimination fact
    /// graph: a saturated direct call threads the callee's result facts; everything
    /// else (arithmetic, lengths, comparisons, projections, literals) is handled by
    /// the value-shape transfer.
    fn bce_transfer(&mut self, local: LocalId, value: &CExpr) {
        // Peel any reference-count wrappers the rc pass inserted around the value.
        let inner = fai_core::bounds::peel_rc(value);
        if let ExprKind::App { func, args, .. } = &inner.kind
            && let ExprKind::Global(d) = &func.kind
        {
            let sig = (self.result_facts_of)(*d);
            if !sig.is_empty() {
                self.bounds.transfer_call(local, &sig, args);
                return;
            }
        }
        self.bounds.transfer_let(local, value);
    }

    /// Compiles an `Array` length/get/set/push whose operand is a statically
    /// recognized array (`App(Con::Array, elem)`) to inline loads/stores, returning
    /// `None` for any other primitive or an unrecognized operand type (a bare
    /// `Ty::Error`, a non-array prim) — which then takes the runtime call. The
    /// element type drives only the get result representation and the set
    /// old-element release; length/push are element-type agnostic. A type-variable
    /// or erased (`App(Con::Array, Error)`, the combined mutual-recursion function)
    /// element takes the uniform path, so concrete *and* generic sites inline.
    fn array_prim(&mut self, op: Prim, args: &[CExpr], result_ty: &Ty) -> Option<Value> {
        // `Array.withCapacity` (and `empty`/`singleton`/the builders that bottom out
        // in it) inlines the pooled allocation fast path regardless of element type
        // — an array's slots are uniform words — so it is handled before the
        // array-operand gate below: its operand is the capacity `Int`, not an array.
        if op == Prim::ArrayWithCapacity {
            return Some(self.array_with_capacity_inline(args));
        }
        // Gate on the operand being a recognizable array. Only the head survives the
        // object-cache wire form (an `App`'s argument is projected away, so a cached
        // `Array Float` operand reconstructs as `App(Array, Error)`); the *element*
        // representation is therefore read from a wire-preserved standalone type —
        // the get's result type and the set's value-argument type — never the
        // operand's (possibly erased) element.
        array_elem(&args[0].ty)?;
        match op {
            Prim::ArrayLength => Some(self.array_length_inline(args)),
            Prim::ArrayGet => Some(self.array_get_inline(args, result_ty)),
            Prim::ArraySet => Some(self.array_set_inline(args)),
            Prim::ArrayPush => Some(self.array_push_inline(args)),
            _ => None,
        }
    }

    /// Inlines the pooled allocation fast path for an `Array` buffer of `size`
    /// bytes, returning the new cell with its header written (rc = 1, the array
    /// descriptor, the size) but its length field **left unset** for the caller. If
    /// `size`'s class is pooled and its thread-local free list is non-empty, the
    /// recycled cell is popped inline (load the head, store the next-free pointer
    /// back) and the header written inline; otherwise the runtime `fai_alloc_array`
    /// fallback allocates it (system allocator / pool miss). `size` is a raw `i64`
    /// (a byte count), and `self.pool_heads_base` is set (the body pre-scan found
    /// the allocation). Mirrors `alloc_obj`'s pooled path.
    fn inline_alloc_array(&mut self, size: Value) -> Value {
        let base = self.pool_heads_base.expect("pool heads base for an inlined array allocation");
        let max_pooled = i64::try_from(rt::MAX_POOLED_SIZE).expect("max pooled size");

        // Pooled iff `0 < size <= MAX_POOLED_SIZE`; an array size is always at least
        // the element-base offset (> 0), so an unsigned `<=` is the whole gate. The
        // class head slot sits at `base + size` (class * SIZE_STEP == size).
        let pooled = self.builder.ins().icmp_imm(IntCC::UnsignedLessThanOrEqual, size, max_pooled);
        let slot = self.builder.ins().iadd(base, size);

        let pooled_b = self.builder.create_block();
        let fast_b = self.builder.create_block();
        let slow_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);
        self.builder.ins().brif(pooled, pooled_b, &[], slow_b, &[]);

        // Pooled size: take the free list's head if non-empty, else the runtime.
        self.builder.switch_to_block(pooled_b);
        self.builder.seal_block(pooled_b);
        let head = self.builder.ins().load(types::I64, MemFlags::trusted(), slot, 0);
        let hit = self.builder.ins().icmp_imm(IntCC::NotEqual, head, 0);
        self.builder.ins().brif(hit, fast_b, &[], slow_b, &[]);

        // Fast path: pop (store the cell's next-free pointer back as the new head)
        // and write the object header inline.
        self.builder.switch_to_block(fast_b);
        self.builder.seal_block(fast_b);
        let next = self.builder.ins().load(types::I64, MemFlags::trusted(), head, 0);
        self.builder.ins().store(MemFlags::trusted(), next, slot, 0);
        let one = self.builder.ins().iconst(types::I64, 1);
        self.store_field(head, rt::RC_OFFSET, one);
        let desc = self.runtime_data_addr("FAI_ARRAY_DESC");
        self.store_field(head, rt::DESC_OFFSET, desc);
        self.store_field(head, rt::SIZE_OFFSET, size);
        self.note_inline_alloc();
        self.builder.ins().jump(merge_b, &[head.into()]);

        // Slow path: the runtime allocator (unpooled size, or an empty free list).
        self.builder.switch_to_block(slow_b);
        self.builder.seal_block(slow_b);
        let f = self.runtime("fai_alloc_array", 1, true);
        let call = self.builder.ins().call(f, &[size]);
        let allocated = self.builder.inst_results(call)[0];
        self.builder.ins().jump(merge_b, &[allocated.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// Inlines the pooled free path for an `Array` buffer `p` of `size` bytes whose
    /// reference-counted children have already been moved out (the push-grow case),
    /// so it is reclaimed without a child scan. If `size`'s class is pooled the cell
    /// is pushed back onto its free list inline (store the old head into the cell's
    /// first word, make the cell the new head); otherwise the runtime `fai_free`
    /// reclaims it. Mirrors `free_obj`'s pooled path.
    fn inline_free_array(&mut self, p: Value, size: Value) {
        let base = self.pool_heads_base.expect("pool heads base for an inlined array free");
        let max_pooled = i64::try_from(rt::MAX_POOLED_SIZE).expect("max pooled size");

        let pooled = self.builder.ins().icmp_imm(IntCC::UnsignedLessThanOrEqual, size, max_pooled);
        let slot = self.builder.ins().iadd(base, size);

        let fast_b = self.builder.create_block();
        let slow_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.ins().brif(pooled, fast_b, &[], slow_b, &[]);

        // Fast path: push the cell onto its class's free list (intrusive next-pointer
        // in the cell's first word).
        self.builder.switch_to_block(fast_b);
        self.builder.seal_block(fast_b);
        let head = self.builder.ins().load(types::I64, MemFlags::trusted(), slot, 0);
        self.builder.ins().store(MemFlags::trusted(), head, p, 0);
        self.builder.ins().store(MemFlags::trusted(), p, slot, 0);
        self.note_inline_free();
        self.builder.ins().jump(merge_b, &[]);

        // Slow path: the runtime reclaims an unpooled (large) buffer.
        self.builder.switch_to_block(slow_b);
        self.builder.seal_block(slow_b);
        let f = self.runtime("fai_free", 1, false);
        self.builder.ins().call(f, &[p]);
        self.builder.ins().jump(merge_b, &[]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
    }

    /// Records, in a debug build, one heap allocation the inlined fast path made
    /// without calling the runtime allocator, keeping the live-object and
    /// cumulative-allocation counters balanced with the eventual release. Emitted
    /// only under `debug_assertions` (the runtime counters exist only there, and the
    /// compiler and runtime build under one profile), so a release build's fast path
    /// stays call-free.
    fn note_inline_alloc(&mut self) {
        if cfg!(debug_assertions) {
            let f = self.runtime("fai_note_alloc", 0, false);
            self.builder.ins().call(f, &[]);
        }
    }

    /// Records, in a debug build, one heap free the inlined fast path made without
    /// calling the runtime (the counter peer of [`note_inline_alloc`]).
    fn note_inline_free(&mut self) {
        if cfg!(debug_assertions) {
            let f = self.runtime("fai_note_free", 0, false);
            self.builder.ins().call(f, &[]);
        }
    }

    /// `Array.withCapacity`: inline the pooled allocation of an empty array with
    /// room for `cap` elements. The capacity is clamped to non-negative (matching
    /// the runtime's `max(0)`), the byte size derived as `ARRAY_ELEMS_OFFSET +
    /// cap * 8`, the buffer popped from the free list (or the runtime fallback), and
    /// the length initialized to 0. A constant capacity folds the gate and size to
    /// constants. The capacity operand is an `Int` immediate (read raw, not
    /// consumed).
    fn array_with_capacity_inline(&mut self, args: &[CExpr]) -> Value {
        let cap = self.array_index_raw(&args[0]);
        // Clamp to non-negative: a negative capacity becomes 0 (an empty buffer),
        // so the derived size is always a valid pooled-or-large array size.
        let zero = self.builder.ins().iconst(types::I64, 0);
        let cap = self.builder.ins().smax(cap, zero);
        let elems_off = i64::from(u32::try_from(rt::ARRAY_ELEMS_OFFSET).expect("array elems off"));
        let bytes = self.builder.ins().ishl_imm(cap, 3);
        let size = self.builder.ins().iadd_imm(bytes, elems_off);
        let arr = self.inline_alloc_array(size);
        // A fresh array starts empty (length 0).
        self.store_field(arr, rt::ARRAY_LEN_OFFSET, zero);
        arr
    }

    /// `Array.length`: an inline load of the length field as a raw `Int`. The array
    /// is borrowed (length read without consuming), so nothing is dropped.
    fn array_length_inline(&mut self, args: &[CExpr]) -> Value {
        let base = self.expr(&args[0]);
        let off = i32::try_from(rt::ARRAY_LEN_OFFSET).expect("array length offset");
        let len = self.builder.ins().load(types::I64, MemFlags::trusted(), base, off);
        self.mark_raw(len)
    }

    /// Reads the index operand as a raw `i64` without consuming it (matching the
    /// runtime's non-consuming `unbox_int`): a raw value passes through, a tagged
    /// immediate is untagged, a boxed `Int` is read (not released).
    fn array_index_raw(&mut self, idx: &CExpr) -> Value {
        let v = self.expr(idx);
        if self.is_raw_int(v) { v } else { self.borrow_unbox_int_to_raw(v) }
    }

    /// The byte address of element `raw_idx` in array `base`
    /// (`base + ARRAY_ELEMS_OFFSET + raw_idx * 8`), returned with the constant
    /// `ARRAY_ELEMS_OFFSET` to fold into the load/store instruction.
    fn array_elem_addr(&mut self, base: Value, raw_idx: Value) -> (Value, i32) {
        let elem_off = self.builder.ins().ishl_imm(raw_idx, 3);
        let addr = self.builder.ins().iadd(base, elem_off);
        let slot_off = i32::try_from(rt::ARRAY_ELEMS_OFFSET).expect("array elems offset");
        (addr, slot_off)
    }

    /// Tests at runtime whether array `base` stores raw, unboxed `f64` elements
    /// (it carries the float-array descriptor) — the self-tag a float `push`
    /// applied. The descriptor word (offset 8) shares the header cache line the
    /// access already touches, so this is a hot load + compare. Used only on the
    /// generic (type-variable element) path, where the static type cannot say.
    fn array_is_float(&mut self, base: Value) -> Value {
        let desc_off = i32::try_from(rt::DESC_OFFSET).expect("desc offset");
        let desc = self.builder.ins().load(types::I64, MemFlags::trusted(), base, desc_off);
        let float_desc = self.runtime_data_addr("FAI_FLOAT_ARRAY_DESC");
        self.builder.ins().icmp(IntCC::Equal, desc, float_desc)
    }

    /// The float self-tag for a generic array access on `base`: a precomputed
    /// loop-invariant tag when `base` is a cached array value (see
    /// [`Self::array_float_tag`]), reused instead of reloading the descriptor, or a
    /// fresh inline [`Self::array_is_float`] otherwise.
    fn array_float_tag_for(&mut self, base: Value) -> Value {
        if let Some(&tag) = self.array_float_tag.get(&base) {
            return tag;
        }
        self.array_is_float(base)
    }

    /// Tests at runtime whether the uniform value `v` is a boxed `Float` (a
    /// `KIND_FLOAT` cell — every float box carries `FAI_FLOAT_DESC`). The
    /// descriptor load is guarded by an immediate check, since an immediate is not
    /// a pointer. Used by the generic `push` to self-tag an `Array Float` from the
    /// pushed value.
    fn value_is_boxed_float(&mut self, v: Value) -> Value {
        let imm_b = self.builder.create_block();
        let boxed_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I8);
        // An immediate has its low bit set; a boxed pointer has it clear.
        let bit = self.builder.ins().band_imm(v, 1);
        self.builder.ins().brif(bit, imm_b, &[], boxed_b, &[]);

        self.builder.switch_to_block(imm_b);
        self.builder.seal_block(imm_b);
        let no = self.builder.ins().iconst(types::I8, 0);
        self.builder.ins().jump(merge_b, &[no.into()]);

        self.builder.switch_to_block(boxed_b);
        self.builder.seal_block(boxed_b);
        let desc_off = i32::try_from(rt::DESC_OFFSET).expect("desc offset");
        let desc = self.builder.ins().load(types::I64, MemFlags::trusted(), v, desc_off);
        let float_desc = self.runtime_data_addr("FAI_FLOAT_DESC");
        let eq = self.builder.ins().icmp(IntCC::Equal, desc, float_desc);
        self.builder.ins().jump(merge_b, &[eq.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// Stamps array `base`'s descriptor word as the float-array descriptor — the
    /// self-tag a float `push` applies so the generic runtime walkers (drop scan,
    /// equality, ordering, hashing) read its slots as raw `f64`.
    fn stamp_float_array(&mut self, base: Value) {
        let float_desc = self.runtime_data_addr("FAI_FLOAT_ARRAY_DESC");
        self.store_field(base, rt::DESC_OFFSET, float_desc);
    }

    /// `Array.unsafeGet`: an inline bounds-checked slot load. The array is borrowed.
    /// An `Int` element is read raw and a `Float` element to an `f64` (no dup/drop);
    /// any other element is the slot word with an inline tag-checked dup, so the
    /// returned reference outlives the borrowed array's drop. An out-of-bounds index
    /// aborts with the located message (the cold `fai_array_index_panic` branch),
    /// matching the runtime's checked behavior. `elem` is the get's **result type**
    /// (the element type) — a standalone type the wire form preserves, unlike the
    /// operand's projected-away `App` element.
    fn array_get_inline(&mut self, args: &[CExpr], elem: &Ty) -> Value {
        let base = self.expr(&args[0]);
        let raw_idx = self.array_index_raw(&args[1]);

        // The fact graph may prove the index is within `0..len`, so the inline
        // bounds check is redundant (the safe `get`/`set` shape, a `0..length` loop,
        // a hash-bucket mask). Eliding it removes a length load, a compare, and an
        // unreachable abort branch. In shadow mode the check is *kept* but routed to
        // a distinct abort, so generative testing catches any unsound elision.
        let proven = self.index_proven(&args[0], &args[1]);
        if proven && !self.bce_shadow {
            return self.array_load_elem(base, raw_idx, elem);
        }

        let len_off = i32::try_from(rt::ARRAY_LEN_OFFSET).expect("array length offset");
        let len = self.builder.ins().load(types::I64, MemFlags::trusted(), base, len_off);
        // An unsigned compare, so a negative index (a huge unsigned value) is out of
        // bounds, matching the runtime's `usize` cast.
        let in_bounds = self.builder.ins().icmp(IntCC::UnsignedLessThan, raw_idx, len);

        let is_float = matches!(elem, Ty::Con(Con::Float));
        let fast_b = self.builder.create_block();
        let oob_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        let merge_ty = if is_float { types::F64 } else { types::I64 };
        self.builder.append_block_param(merge_b, merge_ty);
        self.builder.ins().brif(in_bounds, fast_b, &[], oob_b, &[]);

        // Out of bounds: the located abort (never returns); a dead value of the
        // merge type satisfies the edge. When `proven` (only reachable in shadow
        // mode), the abort is the distinct soundness-violation panic.
        self.builder.switch_to_block(oob_b);
        self.builder.seal_block(oob_b);
        let panic_sym = if proven { "fai_bce_unsound_panic" } else { "fai_array_index_panic" };
        let panic = self.runtime(panic_sym, 0, false);
        self.builder.ins().call(panic, &[]);
        let dead = if is_float {
            self.builder.ins().f64const(Ieee64::with_float(0.0))
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };
        self.builder.ins().jump(merge_b, &[dead.into()]);

        // In bounds: load the slot and bring the element to its scalar/uniform form.
        self.builder.switch_to_block(fast_b);
        self.builder.seal_block(fast_b);
        let value = self.array_load_elem(base, raw_idx, elem);
        self.builder.ins().jump(merge_b, &[value.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        let result = self.builder.block_params(merge_b)[0];
        // An `Int` element flows raw; a `Float` is the f64 merge param; anything
        // else is the owned uniform word.
        if matches!(elem, Ty::Con(Con::Int)) { self.mark_raw(result) } else { result }
    }

    /// Whether the bounds-check-elimination fact graph proves the `index` atom is
    /// within `0..len(array)`, where `array` is an atom operand (a local).
    fn index_proven(&self, array: &CExpr, index: &CExpr) -> bool {
        matches!(&array.kind, ExprKind::Local(a) if self.bounds.index_in_bounds(*a, index))
    }

    /// Emits a standalone bounds check (shadow mode only) that aborts via the
    /// distinct soundness-violation panic if `raw_idx` is out of `base`'s range, so
    /// an unsound elision surfaces loudly under generative testing.
    fn shadow_bounds_assert(&mut self, base: Value, raw_idx: Value) {
        let len_off = i32::try_from(rt::ARRAY_LEN_OFFSET).expect("array length offset");
        let len = self.builder.ins().load(types::I64, MemFlags::trusted(), base, len_off);
        let in_bounds = self.builder.ins().icmp(IntCC::UnsignedLessThan, raw_idx, len);
        let ok_b = self.builder.create_block();
        let bad_b = self.builder.create_block();
        self.builder.ins().brif(in_bounds, ok_b, &[], bad_b, &[]);
        self.builder.switch_to_block(bad_b);
        self.builder.seal_block(bad_b);
        let panic = self.runtime("fai_bce_unsound_panic", 0, false);
        self.builder.ins().call(panic, &[]);
        self.builder.ins().jump(ok_b, &[]);
        self.builder.switch_to_block(ok_b);
        self.builder.seal_block(ok_b);
    }

    /// Loads element `raw_idx` of array `base` (no bounds check) and brings it to
    /// its scalar/uniform form: a raw `Int`, an unboxed `Float` (read straight from
    /// the raw slot — a float array stores `f64` inline, no box deref), or the slot
    /// word with an inline tag-checked dup (so it outlives the borrowed array's
    /// drop). A type-variable/erased element takes the generic path, which converts
    /// at runtime (re-boxing a raw `f64` slot of a float array).
    fn array_load_elem(&mut self, base: Value, raw_idx: Value, elem: &Ty) -> Value {
        let (addr, slot_off) = self.array_elem_addr(base, raw_idx);
        let word = self.builder.ins().load(types::I64, MemFlags::trusted(), addr, slot_off);
        match elem {
            Ty::Con(Con::Int) => {
                let raw = self.borrow_unbox_int_to_raw(word);
                self.mark_raw(raw)
            }
            // A concrete float array: the slot word *is* the `f64` bits.
            Ty::Con(Con::Float) => self.i64_to_f64(word),
            _ if elem_may_be_float(elem) => self.array_load_elem_generic(base, word, elem),
            _ => {
                self.dup_value(word, elem);
                word
            }
        }
    }

    /// The generic (type-variable/erased element) array read: the static type
    /// cannot say whether the slot is a raw `f64` or a boxed pointer, so branch on
    /// the array's runtime self-tag. A float array's raw slot is boxed into a fresh
    /// owned `Float`; a boxed element is tag-checked-duped so it outlives the
    /// borrowed array. Both yield a uniform word. A precomputed loop-invariant
    /// self-tag for `base` is reused when available (see [`Self::array_float_tag`]).
    fn array_load_elem_generic(&mut self, base: Value, word: Value, elem: &Ty) -> Value {
        let is_float = self.array_float_tag_for(base);
        let box_b = self.builder.create_block();
        let dup_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);
        // The re-box arm holds an allocating call (`fai_box_float`); a generic array
        // is far more often a boxed-element one than an (unboxed) float one, so mark
        // it cold so the backend lays it out of line and keeps the call's clobbers off
        // the hot boxed/immediate read path. With the self-tag hoisted out of the
        // per-element load (see [`Self::array_float_tag`]) this tightens a generic
        // in-place sort/traversal over a non-float array — the common case — at the
        // cost of the rarer generic float-array read taking the out-of-line path.
        self.builder.set_cold_block(box_b);
        self.builder.ins().brif(is_float, box_b, &[], dup_b, &[]);

        // Float array: the slot holds raw `f64` bits; box a fresh owned `Float`.
        self.builder.switch_to_block(box_b);
        self.builder.seal_block(box_b);
        let boxed = self.call1("fai_box_float", word);
        self.builder.ins().jump(merge_b, &[boxed.into()]);

        // Boxed-element array: dup the slot pointer (an immediate dups as a no-op).
        self.builder.switch_to_block(dup_b);
        self.builder.seal_block(dup_b);
        self.dup_value(word, elem);
        self.builder.ins().jump(merge_b, &[word.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// `Array.unsafeSet`: an inline in-place store when the array is uniquely owned
    /// and the index is in bounds; otherwise the runtime `fai_array_set`, which
    /// copies a shared array and aborts (located) on an out-of-bounds index. The
    /// array and the new value are consumed (their ownership flows into the result /
    /// the slot); the overwritten old element is released inline (classified by the
    /// new value's type — the element type, which the wire form preserves as a
    /// standalone type, unlike the operand's projected-away `App` element).
    fn array_set_inline(&mut self, args: &[CExpr]) -> Value {
        let elem = args[2].ty.clone();
        let concrete_float = matches!(elem, Ty::Con(Con::Float));
        let generic = elem_may_be_float(&elem);
        let base = self.expr(&args[0]);
        let raw_idx = self.array_index_raw(&args[1]);
        // A concrete float element is stored as raw `f64` bits; otherwise a uniform
        // word (a generic element converts at runtime in the fast path).
        let value = if concrete_float {
            let v = self.expr(&args[2]);
            self.float_field_bits(v)
        } else {
            self.expr_boxed(&args[2])
        };

        let len_off = i32::try_from(rt::ARRAY_LEN_OFFSET).expect("array length offset");
        let rc_off = i32::try_from(rt::RC_OFFSET).expect("rc offset");
        let rc = self.builder.ins().load(types::I64, MemFlags::trusted(), base, rc_off);
        let unique = self.builder.ins().icmp_imm(IntCC::Equal, rc, 1);

        // When the index is provably in range, the in-place fast path is gated on
        // uniqueness alone; the slow runtime path then handles only the shared-copy
        // case (its own bounds check never fires). In shadow mode the bounds check is
        // re-asserted standalone, routing a violation to the distinct abort.
        let proven = self.index_proven(&args[0], &args[1]);
        if proven && self.bce_shadow {
            self.shadow_bounds_assert(base, raw_idx);
        }
        let fast = if proven {
            unique
        } else {
            let len = self.builder.ins().load(types::I64, MemFlags::trusted(), base, len_off);
            let in_bounds = self.builder.ins().icmp(IntCC::UnsignedLessThan, raw_idx, len);
            self.builder.ins().band(in_bounds, unique)
        };

        let fast_b = self.builder.create_block();
        let slow_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);
        self.builder.ins().brif(fast, fast_b, &[], slow_b, &[]);

        // Fast path: overwrite the slot in place.
        self.builder.switch_to_block(fast_b);
        self.builder.seal_block(fast_b);
        let (addr, slot_off) = self.array_elem_addr(base, raw_idx);
        if concrete_float {
            // Raw `f64` store; the overwritten slot carries no reference count, so
            // there is no old element to release.
            self.builder.ins().store(MemFlags::trusted(), value, addr, slot_off);
        } else if generic {
            // The static type cannot say whether the array is raw `f64`; branch on
            // its self-tag at runtime.
            self.array_set_store_generic(base, addr, slot_off, value, &elem);
        } else {
            // A boxed/`Int` element: store the new word, releasing the old element.
            let old = self.builder.ins().load(types::I64, MemFlags::trusted(), addr, slot_off);
            self.builder.ins().store(MemFlags::trusted(), value, addr, slot_off);
            self.drop_value(old, &elem);
        }
        self.builder.ins().jump(merge_b, &[base.into()]);

        // Slow path: the runtime copies a shared array and aborts on a bad index.
        self.builder.switch_to_block(slow_b);
        self.builder.seal_block(slow_b);
        let tagged_idx = self.box_or_tag_int(raw_idx);
        // The runtime expects a uniform value: a concrete float's raw bits are
        // re-boxed here (the cold uniqueness-loss path) and the runtime re-unboxes
        // into the float array's raw slot.
        let arg = if concrete_float { self.call1("fai_box_float", value) } else { value };
        let f = self.runtime("fai_array_set", 3, true);
        let call = self.builder.ins().call(f, &[base, tagged_idx, arg]);
        let copied = self.builder.inst_results(call)[0];
        self.builder.ins().jump(merge_b, &[copied.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// The generic (type-variable element) in-place set store: branch on the
    /// array's runtime self-tag. A float array stores the boxed value's raw bits
    /// (freeing the box; the old raw slot carries no count); a boxed array stores
    /// the word and releases the old element.
    fn array_set_store_generic(
        &mut self,
        base: Value,
        addr: Value,
        slot_off: i32,
        value: Value,
        elem: &Ty,
    ) {
        let is_float = self.array_float_tag_for(base);
        let float_b = self.builder.create_block();
        let boxed_b = self.builder.create_block();
        let cont_b = self.builder.create_block();
        self.builder.ins().brif(is_float, float_b, &[], boxed_b, &[]);

        // Float array: store the boxed value's raw bits and free the box.
        self.builder.switch_to_block(float_b);
        self.builder.seal_block(float_b);
        let bits = self.float_field_bits(value);
        self.builder.ins().store(MemFlags::trusted(), bits, addr, slot_off);
        self.builder.ins().jump(cont_b, &[]);

        // Boxed array: store the new word, releasing the old element.
        self.builder.switch_to_block(boxed_b);
        self.builder.seal_block(boxed_b);
        let old = self.builder.ins().load(types::I64, MemFlags::trusted(), addr, slot_off);
        self.builder.ins().store(MemFlags::trusted(), value, addr, slot_off);
        self.drop_value(old, elem);
        self.builder.ins().jump(cont_b, &[]);

        self.builder.switch_to_block(cont_b);
        self.builder.seal_block(cont_b);
    }

    /// `Array.push`. The array and value are consumed; capacity is derived from the
    /// allocation size (`(size - ELEMS) / 8`). Three paths:
    ///
    /// - **unique, room to spare** — write the spare slot at `len` and bump the
    ///   length, in place.
    /// - **unique, full** — grow: inline a fresh, larger pooled buffer, move the
    ///   elements over (ownership transfers, no dup/drop), append the new element,
    ///   then reclaim the old buffer inline (its children moved out). The grow
    ///   factor matches the runtime (double, from a base of 4).
    /// - **shared** — the runtime `fai_array_push`, which copies the shared buffer.
    fn array_push_inline(&mut self, args: &[CExpr]) -> Value {
        let concrete_float = matches!(args[1].ty, Ty::Con(Con::Float));
        let generic = elem_may_be_float(&args[1].ty);
        let base = self.expr(&args[0]);
        // A concrete float element is the raw `f64` bits; otherwise a uniform word
        // (a generic element converts at runtime in the in-place / grow paths).
        let value = if concrete_float {
            let v = self.expr(&args[1]);
            self.float_field_bits(v)
        } else {
            self.expr_boxed(&args[1])
        };

        let len_off = i32::try_from(rt::ARRAY_LEN_OFFSET).expect("array length offset");
        let rc_off = i32::try_from(rt::RC_OFFSET).expect("rc offset");
        let size_off = i32::try_from(rt::SIZE_OFFSET).expect("size offset");
        let elems_bytes = i64::try_from(rt::ARRAY_ELEMS_OFFSET).expect("array elems offset");

        let len = self.builder.ins().load(types::I64, MemFlags::trusted(), base, len_off);
        let rc = self.builder.ins().load(types::I64, MemFlags::trusted(), base, rc_off);
        let size = self.builder.ins().load(types::I64, MemFlags::trusted(), base, size_off);
        // cap = (size - ARRAY_ELEMS_OFFSET) / 8
        let usable = self.builder.ins().iadd_imm(size, -elems_bytes);
        let cap = self.builder.ins().ushr_imm(usable, 3);
        let has_room = self.builder.ins().icmp(IntCC::UnsignedLessThan, len, cap);
        let unique = self.builder.ins().icmp_imm(IntCC::Equal, rc, 1);
        // The new length, used by both unique paths; computed here so it dominates
        // them (the entry block dominates the whole append).
        let len1 = self.builder.ins().iadd_imm(len, 1);

        let unique_b = self.builder.create_block();
        let fast_b = self.builder.create_block();
        let grow_b = self.builder.create_block();
        let shared_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);
        self.builder.ins().brif(unique, unique_b, &[], shared_b, &[]);

        // Unique owner: append in place if there is spare capacity, else grow.
        self.builder.switch_to_block(unique_b);
        self.builder.seal_block(unique_b);
        self.builder.ins().brif(has_room, fast_b, &[], grow_b, &[]);

        // Fast path: write the spare slot at `len` and bump the length.
        self.builder.switch_to_block(fast_b);
        self.builder.seal_block(fast_b);
        let (addr, slot_off) = self.array_elem_addr(base, len);
        self.array_push_store(base, addr, slot_off, value, concrete_float, generic);
        self.builder.ins().store(MemFlags::trusted(), len1, base, len_off);
        self.builder.ins().jump(merge_b, &[base.into()]);

        // Grow path (unique but full, so `len == cap`): a fresh buffer of double the
        // capacity (4 from empty), the elements moved over, the new element
        // appended, and the old buffer reclaimed.
        self.builder.switch_to_block(grow_b);
        self.builder.seal_block(grow_b);
        let is_empty = self.builder.ins().icmp_imm(IntCC::Equal, cap, 0);
        let doubled = self.builder.ins().ishl_imm(cap, 1);
        let four = self.builder.ins().iconst(types::I64, 4);
        let new_cap = self.builder.ins().select(is_empty, four, doubled);
        let new_bytes = self.builder.ins().ishl_imm(new_cap, 3);
        let new_size = self.builder.ins().iadd_imm(new_bytes, elems_bytes);
        let grown = self.inline_alloc_array(new_size);
        self.copy_array_elems(base, grown, len);
        let (gaddr, gslot) = self.array_elem_addr(grown, len);
        self.array_push_store(grown, gaddr, gslot, value, concrete_float, generic);
        self.builder.ins().store(MemFlags::trusted(), len1, grown, len_off);
        self.inline_free_array(base, size);
        self.builder.ins().jump(merge_b, &[grown.into()]);

        // Shared: the runtime copies the shared buffer with the new element.
        self.builder.switch_to_block(shared_b);
        self.builder.seal_block(shared_b);
        // The runtime expects a uniform value: a concrete float's raw bits are
        // re-boxed here (the cold uniqueness-loss path) and the runtime self-tags.
        let arg = if concrete_float { self.call1("fai_box_float", value) } else { value };
        let f = self.runtime("fai_array_push", 2, true);
        let call = self.builder.ins().call(f, &[base, arg]);
        let copied = self.builder.inst_results(call)[0];
        self.builder.ins().jump(merge_b, &[copied.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// Writes a pushed element into a slot, self-tagging a float array. A concrete
    /// float element's raw bits are stored and the array stamped; a generic element
    /// branches on whether the pushed value is a boxed float at runtime; a concrete
    /// boxed/`Int` element is stored as-is.
    fn array_push_store(
        &mut self,
        obj: Value,
        addr: Value,
        slot_off: i32,
        value: Value,
        concrete_float: bool,
        generic: bool,
    ) {
        if concrete_float {
            // `value` is the raw `f64` bits.
            self.builder.ins().store(MemFlags::trusted(), value, addr, slot_off);
            self.stamp_float_array(obj);
        } else if generic {
            self.array_push_store_generic(obj, addr, slot_off, value);
        } else {
            self.builder.ins().store(MemFlags::trusted(), value, addr, slot_off);
        }
    }

    /// The generic (type-variable element) push store: a boxed-`Float` value marks
    /// (and self-tags) an `Array Float` — store its raw bits, free the box, and
    /// stamp the buffer; any other value is stored as a uniform word.
    fn array_push_store_generic(&mut self, obj: Value, addr: Value, slot_off: i32, value: Value) {
        let is_float = self.value_is_boxed_float(value);
        let float_b = self.builder.create_block();
        let boxed_b = self.builder.create_block();
        let cont_b = self.builder.create_block();
        self.builder.ins().brif(is_float, float_b, &[], boxed_b, &[]);

        // A boxed float: store its raw bits, freeing the box, and self-tag.
        self.builder.switch_to_block(float_b);
        self.builder.seal_block(float_b);
        let bits = self.float_field_bits(value);
        self.builder.ins().store(MemFlags::trusted(), bits, addr, slot_off);
        self.stamp_float_array(obj);
        self.builder.ins().jump(cont_b, &[]);

        // Any other value: a uniform word.
        self.builder.switch_to_block(boxed_b);
        self.builder.seal_block(boxed_b);
        self.builder.ins().store(MemFlags::trusted(), value, addr, slot_off);
        self.builder.ins().jump(cont_b, &[]);

        self.builder.switch_to_block(cont_b);
        self.builder.seal_block(cont_b);
    }

    /// Emits a loop copying `count` element slots from array `src` to array `dst`
    /// (the words at `ARRAY_ELEMS_OFFSET + i*8` for `i in 0..count`), a plain word
    /// move with no reference-count traffic — used by the push-grow path, where
    /// ownership of the elements transfers from the old buffer to the new. Leaves
    /// the builder positioned at the (sealed) block following the loop.
    fn copy_array_elems(&mut self, src: Value, dst: Value, count: Value) {
        let slot_off = i32::try_from(rt::ARRAY_ELEMS_OFFSET).expect("array elems offset");
        let header = self.builder.create_block();
        self.builder.append_block_param(header, types::I64);
        let body = self.builder.create_block();
        let after = self.builder.create_block();

        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder.ins().jump(header, &[zero.into()]);

        // Header: continue while `i < count`. Sealed only after the body's back-edge
        // is emitted below (its two predecessors are the entry jump and that edge).
        self.builder.switch_to_block(header);
        let i = self.builder.block_params(header)[0];
        let more = self.builder.ins().icmp(IntCC::UnsignedLessThan, i, count);
        self.builder.ins().brif(more, body, &[], after, &[]);

        self.builder.switch_to_block(body);
        self.builder.seal_block(body);
        let byte = self.builder.ins().ishl_imm(i, 3);
        let sa = self.builder.ins().iadd(src, byte);
        let da = self.builder.ins().iadd(dst, byte);
        let w = self.builder.ins().load(types::I64, MemFlags::trusted(), sa, slot_off);
        self.builder.ins().store(MemFlags::trusted(), w, da, slot_off);
        let i1 = self.builder.ins().iadd_imm(i, 1);
        self.builder.ins().jump(header, &[i1.into()]);
        self.builder.seal_block(header);

        self.builder.switch_to_block(after);
        self.builder.seal_block(after);
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
            let raw = self.as_raw_int(n);
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
            // The `Int` result flows on raw (tagged/boxed only at a uniform slot).
            self.mark_raw(raw)
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
            let raw = self.as_raw_int(n);
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
            // The `Int` result flows on raw (tagged/boxed only at a uniform slot).
            self.mark_raw(raw)
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
            Prim::Hash => self.inline_hash(op, args),
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

    /// The out-of-line fallback for an inspect-only primitive (`=`/`compare`)
    /// whose inline immediate fast path missed, selecting the non-consuming
    /// (borrowed) runtime variant when reference counting lent the operands —
    /// exactly the choice [`Self::prim`]'s out-of-line path makes. A type-variable
    /// operand is owned (so the consuming `fai_compare`/`fai_equal` runs and frees
    /// it), a reference-counted union/`List` operand is borrowed (the caller drops
    /// it at its last use); the immediate fast path drops nothing either way.
    fn prim_runtime_call_borrowing(&mut self, op: Prim, borrowed: bool, args: &[Value]) -> Value {
        let symbol = match op.borrowed_runtime_symbol() {
            Some(b) if borrowed => b,
            _ => op.runtime_symbol(),
        };
        let f = self.runtime(symbol, args.len(), true);
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

    /// Emits the bare native arithmetic/shift op on raw operands. The result stays a
    /// raw, untagged `i64` — no tag, no 63-bit fit check, no boxing — so a full
    /// 64-bit value flows on. Wrapping matches the runtime's `wrapping_*`, and
    /// Cranelift masks a dynamic shift amount modulo the 64-bit width (matching the
    /// runtime's `& 63`).
    fn raw_arith(&mut self, fop: FitsOp, xa: Value, xb: Value) -> Value {
        let r = match fop {
            FitsOp::Add => self.builder.ins().iadd(xa, xb),
            FitsOp::Sub => self.builder.ins().isub(xa, xb),
            FitsOp::Mul => self.builder.ins().imul(xa, xb),
            FitsOp::Shl => self.builder.ins().ishl(xa, xb),
            FitsOp::Shr => self.builder.ins().sshr(xa, xb),
            FitsOp::Ushr => self.builder.ins().ushr(xa, xb),
        };
        self.mark_raw(r)
    }

    /// Inlines an arithmetic or shift primitive. With **raw untagged** operands
    /// (the common case in a normal function) it is a single bare native op
    /// ([`Self::raw_arith`]) — no guard, no fit check, no boxing. Otherwise (tagged
    /// operands: a mutual-recursion combined function, or a conflicting-observation
    /// local) it takes the tagged guarded path: untag, native op, then re-tag
    /// guarded by a 63-bit fit check. `sadd_overflow(r, r)` computes `r << 1` and
    /// flags overflow exactly when `r` no longer fits the immediate — the precise
    /// `fai_box_int` boundary — so an out-of-range result falls back to the runtime,
    /// which boxes it.
    fn inline_arith(&mut self, op: Prim, args: &[CExpr], fop: FitsOp) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        if self.is_raw_int(a) && self.is_raw_int(b) {
            return self.raw_arith(fop, a, b);
        }
        let a = self.ensure_boxed(a);
        let b = self.ensure_boxed(b);
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
        let a = self.expr(&args[0]);
        if let ExprKind::Lit(Lit::Int(d)) = args[1].kind {
            if d == 0 {
                // A literal zero divisor always faults; keep the out-of-line call
                // so the located message is raised rather than a hardware trap.
                let b = self.expr(&args[1]);
                let a = self.ensure_boxed(a);
                let b = self.ensure_boxed(b);
                return self.prim_runtime_call(op, &[a, b]);
            }
            if d >= 1 && fits_immediate(d) {
                let k = i64::from(d.trailing_zeros());
                let pow2 = d > 1 && (d & (d - 1)) == 0;
                // A literal divisor is statically nonzero and never `-1`, so a raw
                // dividend divides with no zero/`-1`/fit guards at all.
                if self.is_raw_int(a) {
                    return if pow2 {
                        self.raw_divrem_pow2(a, is_div, k)
                    } else {
                        let b = self.builder.ins().iconst(types::I64, d);
                        self.raw_divrem_const(a, b, is_div)
                    };
                }
                let a = self.ensure_boxed(a);
                let b = self.expr(&args[1]);
                return if pow2 {
                    self.tagged_divrem_pow2(op, a, b, is_div, k)
                } else {
                    self.tagged_divrem_const(op, a, b, is_div)
                };
            }
        }
        // Variable divisor (or a constant too large to be immediate).
        let b = self.expr(&args[1]);
        if self.is_raw_int(a) && self.is_raw_int(b) {
            return self.raw_divrem_general(op, a, b, is_div);
        }
        let a = self.ensure_boxed(a);
        let b = self.ensure_boxed(b);
        self.tagged_divrem_general(op, a, b, is_div)
    }

    /// The raw division/remainder path for a **variable** divisor: a zero-divisor
    /// branch to the runtime located fault, then a `b == -1` branch producing
    /// `0 - a` (division) or `0` (remainder) to match `wrapping_div`/`wrapping_rem`
    /// while dodging Cranelift `sdiv`/`srem`'s `i64::MIN / -1` hardware trap, else
    /// the native op. The result stays a raw `i64` — no fit check (a raw value holds
    /// the full 64-bit result).
    fn raw_divrem_general(&mut self, op: Prim, a: Value, b: Value, is_div: bool) -> Value {
        let merge = self.builder.create_block();
        self.builder.append_block_param(merge, types::I64);
        let nonzero = self.builder.create_block();
        let zero = self.builder.create_block();
        let is_zero = self.builder.ins().icmp_imm(IntCC::Equal, b, 0);
        self.builder.ins().brif(is_zero, zero, &[], nonzero, &[]);

        // Zero divisor: raise the located fault via the runtime, which needs the
        // operands in the uniform representation. The divisor is statically `0` here
        // and the runtime faults on it before using the dividend, so a plain re-tag
        // (no fit check or boxing) suffices; the call aborts, so its result is dead.
        self.builder.switch_to_block(zero);
        self.builder.seal_block(zero);
        let ta = self.tag_int(a);
        let tb = self.tag_int(b);
        let dead = self.prim_runtime_call(op, &[ta, tb]);
        self.builder.ins().jump(merge, &[dead.into()]);

        self.builder.switch_to_block(nonzero);
        self.builder.seal_block(nonzero);
        let neg1 = self.builder.create_block();
        let normal = self.builder.create_block();
        let is_neg1 = self.builder.ins().icmp_imm(IntCC::Equal, b, -1);
        self.builder.ins().brif(is_neg1, neg1, &[], normal, &[]);

        self.builder.switch_to_block(neg1);
        self.builder.seal_block(neg1);
        let special = if is_div {
            // `a / -1 = -a` (wrapping: `i64::MIN / -1 = i64::MIN`).
            let zero_c = self.builder.ins().iconst(types::I64, 0);
            self.builder.ins().isub(zero_c, a)
        } else {
            // `a % -1 = 0`.
            self.builder.ins().iconst(types::I64, 0)
        };
        self.builder.ins().jump(merge, &[special.into()]);

        self.builder.switch_to_block(normal);
        self.builder.seal_block(normal);
        let r = if is_div { self.builder.ins().sdiv(a, b) } else { self.builder.ins().srem(a, b) };
        self.builder.ins().jump(merge, &[r.into()]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        let result = self.builder.block_params(merge)[0];
        self.mark_raw(result)
    }

    /// Raw division/remainder by a constant, statically nonzero, in-range divisor
    /// that is not a power of two: a bare native op (the constant strength-reduces in
    /// the backend), result raw. The divisor is `>= 1`, so it is never `0` nor `-1`.
    fn raw_divrem_const(&mut self, a: Value, b: Value, is_div: bool) -> Value {
        let r = if is_div { self.builder.ins().sdiv(a, b) } else { self.builder.ins().srem(a, b) };
        self.mark_raw(r)
    }

    /// Raw strength-reduced division/remainder by a constant power of two `2^k`
    /// (`k >= 1`) on a raw operand, result raw. Truncation toward zero needs a bias,
    /// since an arithmetic shift floors: `bias = (x < 0) ? 2^k - 1 : 0`, so
    /// `q = (x + bias) >> k` and the remainder is `x - (q << k)`.
    fn raw_divrem_pow2(&mut self, x: Value, is_div: bool, k: i64) -> Value {
        let sign = self.builder.ins().sshr_imm(x, 63);
        let bias = self.builder.ins().ushr_imm(sign, 64 - k);
        let adj = self.builder.ins().iadd(x, bias);
        let q = self.builder.ins().sshr_imm(adj, k);
        let r = if is_div {
            q
        } else {
            let qk = self.builder.ins().ishl_imm(q, k);
            self.builder.ins().isub(x, qk)
        };
        self.mark_raw(r)
    }

    /// The tagged general division/remainder path (a variable divisor with tagged
    /// operands — a combined function or a conflicting-observation local): a
    /// both-operands-immediate guard, then a zero-divisor branch to the runtime
    /// fault, the native `sdiv`/`srem`, and — for division — the immediate fit
    /// check. A boxed operand or a zero divisor takes the runtime fallback, which
    /// consumes both operands and raises the located fault on zero.
    fn tagged_divrem_general(&mut self, op: Prim, a: Value, b: Value, is_div: bool) -> Value {
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

    /// Tagged division/remainder by a constant, statically nonzero, in-range divisor
    /// that is not a power of two (the tagged peer of [`Self::raw_divrem_const`]). A
    /// literal divisor is non-negative, so it is never `0` (handled in
    /// [`Self::inline_divrem`]) nor `-1`: with `|d| >= 1` the quotient and remainder
    /// always fit the immediate, so neither the zero guard nor the fit check is
    /// needed. Only the dividend is guarded; a boxed dividend falls back to the
    /// runtime.
    fn tagged_divrem_const(&mut self, op: Prim, a: Value, b: Value, is_div: bool) -> Value {
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

    /// Tagged strength-reduced division/remainder by a constant power of two `2^k`
    /// (`k >= 1`) (the tagged peer of [`Self::raw_divrem_pow2`]): no zero or overflow
    /// guard. Only the dividend is guarded; a boxed dividend falls back to the
    /// runtime.
    fn tagged_divrem_pow2(&mut self, op: Prim, a: Value, b: Value, is_div: bool, k: i64) -> Value {
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
        if self.is_raw_int(a) && self.is_raw_int(b) {
            let r = match bop {
                BitOp::And => self.builder.ins().band(a, b),
                BitOp::Or => self.builder.ins().bor(a, b),
                BitOp::Xor => self.builder.ins().bxor(a, b),
            };
            return self.mark_raw(r);
        }
        let a = self.ensure_boxed(a);
        let b = self.ensure_boxed(b);
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
        if self.is_raw_int(a) {
            let r = self.builder.ins().bnot(a);
            return self.mark_raw(r);
        }
        let a = self.ensure_boxed(a);
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

    /// Inlines an integer comparison: with raw operands a bare native `icmp` and a
    /// tagged `Bool`; otherwise the tagged guarded path (untag, `icmp`, tag).
    fn inline_cmp(&mut self, op: Prim, args: &[CExpr], cc: IntCC) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        if self.is_raw_int(a) && self.is_raw_int(b) {
            let c = self.builder.ins().icmp(cc, a, b);
            return self.tag_bool(c);
        }
        let a = self.ensure_boxed(a);
        let b = self.ensure_boxed(b);
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
    /// already correct. A [`is_maybe_immediate_ty`] operand (a type variable, or a
    /// possibly-nullary union/`List`/empty record) takes the same immediate guard
    /// over the structural fallback, so a generic `=` whose runtime value is an
    /// immediate (the common case) avoids the call. The always-boxed types keep
    /// the out-of-line structural path.
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
            // Raw operands: a bare `icmp eq` (raw word equality is value equality).
            if self.is_raw_int(a) && self.is_raw_int(b) {
                let c = self.builder.ins().icmp(IntCC::Equal, a, b);
                return Some(self.tag_bool(c));
            }
            let a = self.ensure_boxed(a);
            let b = self.ensure_boxed(b);
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
        } else if is_maybe_immediate_ty(oty) {
            // A type-variable (or possibly-nullary union/`List`/empty-record)
            // operand may be an immediate at runtime. Guard on both being
            // immediate — then word equality is value equality (the tag is
            // injective), inline — and otherwise fall back to the structural
            // runtime call, which a small immediate can never equal a boxed value
            // through, so the mixed case is already correct. The fallback honours
            // the borrow decision reference counting made for this operand type.
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            // A niche `Option` operand cannot be compared on its wrapper-free
            // encoding against a *standard* one of the same type (a niche `Some`
            // is the payload; a standard `Some` is a wrapper cell), and the two
            // representations both occur for one type (e.g. a niche local vs a
            // generic combinator's standard result). Convert both operands to
            // standard temporaries and compare those (consuming them).
            if self.niche_of(a).is_some() || self.niche_of(b).is_some() {
                let a2 = self.comparison_std_operand(a);
                let b2 = self.comparison_std_operand(b);
                return Some(self.prim_runtime_call(op, &[a2, b2]));
            }
            let anded = self.builder.ins().band(a, b);
            let borrowed = op.borrows_operand(oty);
            Some(self.guard_immediate(
                anded,
                move |s| s.prim_runtime_call_borrowing(op, borrowed, &[a, b]),
                |s, _slow, merge| {
                    let c = s.builder.ins().icmp(IntCC::Equal, a, b);
                    let tagged = s.tag_bool(c);
                    s.builder.ins().jump(merge, &[tagged.into()]);
                },
            ))
        } else {
            // A fixed-shape tuple/record of immediate/`Int` (and boxed) fields:
            // compare field-wise inline instead of one structural `fai_equal` over
            // the boxed aggregate. A direct-`Float`-bearing shape is not eligible
            // and keeps the runtime call.
            inline_aggregate_fields(oty, AGG_FIELD_CAP)
                .map(|fields| self.inline_aggregate_eq(args, &fields))
        }
    }

    /// Inlines structural ordering when the operands are immediate-representable,
    /// producing the same `-1`/`0`/`1` as `fai_compare`. `Bool`/`Char`/`Unit`
    /// compare bare; `Int` adds the guard and the `fai_compare` fallback; a
    /// [`is_maybe_immediate_ty`] operand (a type variable, or a possibly-nullary
    /// union/`List`/empty record) takes the same guard over the structural
    /// fallback, so a generic `<`/`>`/`compare` whose runtime value is an immediate
    /// avoids the call. The always-boxed types keep the out-of-line structural
    /// path.
    fn inline_compare(&mut self, op: Prim, args: &[CExpr]) -> Option<Value> {
        let oty = &args[0].ty;
        if is_immediate_ty(oty) {
            // `Bool`/`Char`/`Unit` operands are tagged immediates; untag to raw and
            // produce the raw `-1`/`0`/`1` (this branch never runs in an erased
            // combined function, whose operand types are not immediate).
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            let xa = self.untag(a);
            let xb = self.untag(b);
            Some(self.compare_three_way_raw(xa, xb))
        } else if matches!(oty, Ty::Con(Con::Int)) {
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            // Raw operands: the raw `-1`/`0`/`1` from a bare two-comparison form.
            if self.is_raw_int(a) && self.is_raw_int(b) {
                return Some(self.compare_three_way_raw(a, b));
            }
            let a = self.ensure_boxed(a);
            let b = self.ensure_boxed(b);
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
        } else if is_maybe_immediate_ty(oty) {
            // A type-variable (or possibly-nullary union/`List`/empty-record)
            // operand may be an immediate at runtime. Guard on both being
            // immediate — then the ordering is the raw payload compare (matching
            // the runtime's immediate fast path `(a >> 1).cmp(b >> 1)`), inline —
            // and otherwise fall back to the structural runtime call. The fallback
            // honours the borrow decision reference counting made for this operand
            // type (a type variable owned, a reference-counted union/`List`
            // borrowed); the immediate fast arm drops nothing either way.
            let a = self.expr(&args[0]);
            let b = self.expr(&args[1]);
            // A niche `Option` operand is compared by converting each operand to a
            // standard temporary (consuming them): a niche value and a standard one
            // of the same type (e.g. a niche local vs a generic combinator's
            // standard result) have incomparable encodings, and Scheme A's `None`=`1`
            // is in any case indistinguishable from a tuple's tag.
            if self.niche_of(a).is_some() || self.niche_of(b).is_some() {
                let a2 = self.comparison_std_operand(a);
                let b2 = self.comparison_std_operand(b);
                return Some(self.prim_runtime_call(op, &[a2, b2]));
            }
            let anded = self.builder.ins().band(a, b);
            let borrowed = op.borrows_operand(oty);
            Some(self.guard_immediate(
                anded,
                move |s| s.prim_runtime_call_borrowing(op, borrowed, &[a, b]),
                |s, _slow, merge| {
                    let tagged = s.compare_three_way(a, b);
                    s.builder.ins().jump(merge, &[tagged.into()]);
                },
            ))
        } else {
            // A fixed-shape tuple/record of immediate/`Int` (and boxed) fields:
            // compare lexicographically inline instead of one structural
            // `fai_compare` over the boxed aggregate. A direct-`Float`-bearing shape
            // is not eligible and keeps the runtime call.
            inline_aggregate_fields(oty, AGG_FIELD_CAP)
                .map(|fields| self.inline_aggregate_compare(args, &fields))
        }
    }

    /// Inlines the structural hash when the operand is immediate-representable,
    /// producing the same non-negative immediate `Int` as `fai_hash`: the
    /// splitmix64 finalizer of the value's payload, masked to 62 bits.
    /// `Bool`/`Char`/`Unit` untag to their payload and mix it bare; `Int` adds the
    /// immediate guard and the `fai_hash` fallback (a boxed/overflowed `Int` hashes
    /// its full 64-bit value out of line); a scalar `Float` mixes its raw bits
    /// (matching the runtime's boxed-`Float` hash); a [`is_maybe_immediate_ty`]
    /// operand (a type variable, or a possibly-nullary union/`List`/empty record)
    /// takes the same guard over the structural fallback, so a generic `hash` whose
    /// runtime value is an immediate (the common case — `Int` keys in a generic
    /// `HashDict`) avoids the call. The always-boxed types keep the out-of-line
    /// structural path.
    fn inline_hash(&mut self, op: Prim, args: &[CExpr]) -> Option<Value> {
        let oty = &args[0].ty;
        if is_immediate_ty(oty) {
            // `Bool`/`Char`/`Unit`: untag to the raw payload (matching the
            // runtime's immediate `(v >> 1)`) and mix it; the result is a raw
            // non-negative `Int`.
            let a = self.expr(&args[0]);
            let xa = self.untag(a);
            let p = self.hash_payload_raw(xa);
            Some(self.mark_raw(p))
        } else if matches!(oty, Ty::Con(Con::Int)) {
            let a = self.expr(&args[0]);
            // A raw operand is already the logical payload; mix it directly.
            if self.is_raw_int(a) {
                let p = self.hash_payload_raw(a);
                return Some(self.mark_raw(p));
            }
            // A tagged immediate inlines (untag, mix, re-tag); a boxed (overflowed)
            // `Int` falls back to the consuming `fai_hash`, which hashes its full
            // 64-bit value and frees it.
            let a = self.ensure_boxed(a);
            Some(self.guard_immediate(
                a,
                |s| s.prim_runtime_call(op, &[a]),
                |s, _slow, merge| {
                    let xa = s.untag(a);
                    let p = s.hash_payload_raw(xa);
                    let tagged = s.tag_int(p);
                    s.builder.ins().jump(merge, &[tagged.into()]);
                },
            ))
        } else if matches!(oty, Ty::Con(Con::Float)) {
            // Unboxed operand: reinterpret the `f64` bits as `i64` and mix them,
            // matching the runtime's boxed-`Float` hash (`mix64` of the raw bits),
            // so a `Float` hashes identically whether unboxed here or boxed at the
            // runtime fallback.
            let a = self.expr(&args[0]);
            let bits = self.f64_to_i64(a);
            let p = self.hash_payload_raw(bits);
            Some(self.mark_raw(p))
        } else if is_maybe_immediate_ty(oty) {
            let a = self.expr(&args[0]);
            // A niche `Option` operand must be hashed in its *standard* form: a key
            // stored in a uniform slot (an `Array` slot) is standardized, so
            // hashing the bare niche payload (`Some x` is the payload `x`) would
            // land in a different bucket and never be found. Convert an owned
            // standard temporary and hash that (consuming it), leaving the borrowed
            // operand for its binder to drop — mirroring [`Self::inline_compare`].
            if self.niche_of(a).is_some() {
                let a2 = self.comparison_std_operand(a);
                return Some(self.prim_runtime_call(op, &[a2]));
            }
            // Guard on the operand being an immediate — then mix its untagged
            // payload inline — and otherwise fall back to the structural runtime
            // call, honouring the borrow decision reference counting made for this
            // operand type (a type variable owned, a reference-counted union/`List`
            // borrowed); the immediate fast arm drops nothing either way.
            let borrowed = op.borrows_operand(oty);
            Some(self.guard_immediate(
                a,
                move |s| s.prim_runtime_call_borrowing(op, borrowed, &[a]),
                |s, _slow, merge| {
                    let xa = s.untag(a);
                    let p = s.hash_payload_raw(xa);
                    let tagged = s.tag_int(p);
                    s.builder.ins().jump(merge, &[tagged.into()]);
                },
            ))
        } else {
            None
        }
    }

    /// Produces an **owned** standard-`Option` temporary for `v` to feed a
    /// consuming structural comparison or hash, leaving the (borrowed) operand `v`
    /// untouched: a niche value's payload is duplicated into a fresh `Some` cell
    /// (or `None` passes through), a standard value is duplicated. The consumer
    /// then drops the temporary; the operand's owner drops `v` at its last use.
    fn comparison_std_operand(&mut self, v: Value) -> Value {
        let owned = self.call1("fai_dup", v);
        match self.niche_of(v) {
            Some(k) => self.niche_to_std(owned, k),
            None => owned,
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

    /// As [`Self::compare_three_way`] but on raw operands, producing the raw
    /// `-1`/`0`/`1`: `(a > b) - (a < b)`. The two-comparison form cannot overflow.
    fn compare_three_way_raw(&mut self, xa: Value, xb: Value) -> Value {
        let gt = self.builder.ins().icmp(IntCC::SignedGreaterThan, xa, xb);
        let lt = self.builder.ins().icmp(IntCC::SignedLessThan, xa, xb);
        let gtw = self.builder.ins().uextend(types::I64, gt);
        let ltw = self.builder.ins().uextend(types::I64, lt);
        let cmp = self.builder.ins().isub(gtw, ltw);
        self.mark_raw(cmp)
    }

    /// The splitmix64 avalanche finalizer, emitted inline and **bit-identical** to
    /// the runtime's `mix64` (`crates/fai-runtime/src/lib.rs`) so the inline and
    /// out-of-line hash paths agree (`hash` over an immediate must equal `fai_hash`
    /// over the same boxed value). Operates on the raw 64-bit payload; the shifts
    /// are logical (the runtime mixes a `u64`) and the multiplies wrap.
    fn mix64(&mut self, z: Value) -> Value {
        // z = (z ^ (z >> 30)) * 0xBF58_476D_1CE4_E5B9
        let s = self.builder.ins().ushr_imm(z, 30);
        let z = self.builder.ins().bxor(z, s);
        let c1 = self.builder.ins().iconst(types::I64, 0xBF58_476D_1CE4_E5B9u64 as i64);
        let z = self.builder.ins().imul(z, c1);
        // z = (z ^ (z >> 27)) * 0x94D0_49BB_1331_11EB
        let s = self.builder.ins().ushr_imm(z, 27);
        let z = self.builder.ins().bxor(z, s);
        let c2 = self.builder.ins().iconst(types::I64, 0x94D0_49BB_1331_11EBu64 as i64);
        let z = self.builder.ins().imul(z, c2);
        // z ^ (z >> 31)
        let s = self.builder.ins().ushr_imm(z, 31);
        self.builder.ins().bxor(z, s)
    }

    /// `mix64(payload)` masked to 62 bits — the raw, non-negative hash an immediate
    /// operand produces, matching the runtime's `hash_to_imm(mix64(payload))`
    /// before the immediate tag. The mask keeps the result inside the 63-bit
    /// immediate range (so a subsequent `tag_int` never overflows) and
    /// non-negative (so a container can mask/modulo it directly).
    fn hash_payload_raw(&mut self, payload: Value) -> Value {
        let h = self.mix64(payload);
        self.builder.ins().band_imm(h, 0x3FFF_FFFF_FFFF_FFFF)
    }

    /// Loads field `i`'s uniform slot word from data cell `cell`, borrowing the
    /// cell (no reference-count change; its owner releases it at its last use).
    fn agg_field_word(&mut self, cell: Value, i: usize) -> Value {
        let off = i32::try_from(rt::DATA_FIELDS_OFFSET + i * 8).expect("field offset");
        self.builder.ins().load(types::I64, MemFlags::trusted(), cell, off)
    }

    /// Inlines `=` on a fixed-shape aggregate as a short-circuiting conjunction of
    /// per-field comparisons in heap-layout order (matching `fai_equal`). The cells
    /// are borrowed (loads only); a field that is unequal yields `false`
    /// immediately. Yields a tagged `Bool`.
    fn inline_aggregate_eq(&mut self, args: &[CExpr], fields: &[AggField]) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        let merge = self.builder.create_block();
        self.builder.append_block_param(merge, types::I64);
        // The immediate `Bool` `false` is `(0 << 1) | 1`.
        let false_tag = self.builder.ins().iconst(types::I64, 1);
        let last = fields.len() - 1;
        for (i, kind) in fields.iter().enumerate() {
            let ai = self.agg_field_word(a, i);
            let bi = self.agg_field_word(b, i);
            let eq = self.agg_field_eq(*kind, ai, bi);
            if i == last {
                self.builder.ins().jump(merge, &[eq.into()]);
            } else {
                let cont = self.builder.create_block();
                // `true` iff the value bit (mask 2) is set (`true` = 3, `false` = 1).
                let is_true = self.builder.ins().band_imm(eq, 2);
                self.builder.ins().brif(is_true, cont, &[], merge, &[false_tag.into()]);
                self.builder.switch_to_block(cont);
                self.builder.seal_block(cont);
            }
        }
        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.block_params(merge)[0]
    }

    /// The per-field equality of [`Self::inline_aggregate_eq`], yielding a tagged
    /// `Bool`: an immediate field is a bare word compare; an `Int` field guards on
    /// the immediate fast path with a borrowed `fai_equal_borrowed` fallback; any
    /// other boxed field compares through `fai_equal_borrowed`. Field words are
    /// borrowed from the cell, so every fallback is the non-consuming variant.
    fn agg_field_eq(&mut self, kind: AggField, a: Value, b: Value) -> Value {
        match kind {
            AggField::Immediate => {
                let c = self.builder.ins().icmp(IntCC::Equal, a, b);
                self.tag_bool(c)
            }
            AggField::Int => {
                let anded = self.builder.ins().band(a, b);
                self.guard_immediate(
                    anded,
                    |s| s.prim_runtime_call_borrowing(Prim::Eq, true, &[a, b]),
                    |s, _slow, merge| {
                        let c = s.builder.ins().icmp(IntCC::Equal, a, b);
                        let tagged = s.tag_bool(c);
                        s.builder.ins().jump(merge, &[tagged.into()]);
                    },
                )
            }
            AggField::Boxed => self.prim_runtime_call_borrowing(Prim::Eq, true, &[a, b]),
        }
    }

    /// Inlines structural `compare` on a fixed-shape aggregate as a short-circuiting
    /// lexicographic field comparison in heap-layout order (matching `fai_compare`):
    /// the first non-equal field decides, else `0`. The cells are borrowed. Yields a
    /// tagged `Int` (`-1`/`0`/`1`).
    fn inline_aggregate_compare(&mut self, args: &[CExpr], fields: &[AggField]) -> Value {
        let a = self.expr(&args[0]);
        let b = self.expr(&args[1]);
        let merge = self.builder.create_block();
        self.builder.append_block_param(merge, types::I64);
        // The immediate `Int` `0` (an equal field) is `(0 << 1) | 1`.
        let eq_tag = self.builder.ins().iconst(types::I64, 1);
        let last = fields.len() - 1;
        for (i, kind) in fields.iter().enumerate() {
            let ai = self.agg_field_word(a, i);
            let bi = self.agg_field_word(b, i);
            let cmp = self.agg_field_compare(*kind, ai, bi);
            if i == last {
                self.builder.ins().jump(merge, &[cmp.into()]);
            } else {
                let cont = self.builder.create_block();
                let is_eq = self.builder.ins().icmp(IntCC::Equal, cmp, eq_tag);
                self.builder.ins().brif(is_eq, cont, &[], merge, &[cmp.into()]);
                self.builder.switch_to_block(cont);
                self.builder.seal_block(cont);
            }
        }
        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.block_params(merge)[0]
    }

    /// The per-field ordering of [`Self::inline_aggregate_compare`], yielding a
    /// tagged `-1`/`0`/`1`: an immediate or `Int` field uses the inline three-way
    /// compare (the `Int` field guarding on the immediate fast path with a borrowed
    /// `fai_compare_borrowed` fallback); any other boxed field compares through
    /// `fai_compare_borrowed`. Field words are borrowed, so fallbacks do not consume.
    fn agg_field_compare(&mut self, kind: AggField, a: Value, b: Value) -> Value {
        match kind {
            AggField::Immediate => self.compare_three_way(a, b),
            AggField::Int => {
                let anded = self.builder.ins().band(a, b);
                self.guard_immediate(
                    anded,
                    |s| s.prim_runtime_call_borrowing(Prim::Compare, true, &[a, b]),
                    |s, _slow, merge| {
                        let tagged = s.compare_three_way(a, b);
                        s.builder.ins().jump(merge, &[tagged.into()]);
                    },
                )
            }
            AggField::Boxed => self.prim_runtime_call_borrowing(Prim::Compare, true, &[a, b]),
        }
    }

    fn application(
        &mut self,
        func: &CExpr,
        args: &[CExpr],
        reuse: &[Option<LocalId>],
        alloc: ClosureAlloc,
        result_ty: &Ty,
    ) -> Value {
        // A saturated application of a known top-level function calls its code
        // symbol directly, passing the value arguments in registers per the callee's
        // ABI, skipping `apply_n` and the static closure. (Top-level functions
        // capture nothing, so the environment is a null pointer.) An
        // over-application direct-calls the saturated prefix and `apply_n`s the rest.
        if let ExprKind::Global(def) = func.kind {
            let arity = (self.arity_of)(def);
            if arity > 0 && args.len() >= arity {
                // A forwarded saturated call targets the callee's token-taking
                // `{base}__reuse` entry, passing the reuse tokens in leading
                // registers. Reuse is set only at exactly-saturated direct calls.
                if !reuse.is_empty() && args.len() == arity {
                    return self.direct_reuse_application(def, args, reuse, result_ty);
                }
                // A spread-returning callee reached as a value (rather than bound by
                // a `LetMany`): the result aggregate crosses into a uniform position,
                // so reassemble its components into a boxed cell.
                if args.len() == arity && (self.signature_of)(def).spread_return().is_some() {
                    let comps = self.spread_call_parts(def, args);
                    return self.box_components(&comps);
                }
                return if args.len() == arity {
                    self.direct_application(def, args, result_ty)
                } else {
                    self.over_application(def, arity, args, result_ty)
                };
            }
            // An under-application of a known function builds a partial application.
            // When escape analysis proved it does not outlive the frame, build that
            // cell on the stack instead of the heap.
            if arity > 0 && args.len() < arity && matches!(alloc, ClosureAlloc::Stack) {
                return self.stack_pap(def, args);
            }
        }
        // A saturated application of a local bound to a known `MakeClosure` is a
        // direct call to that lifted function — its environment read from the
        // closure cell, the closure consumed afterward exactly as `apply_n` would —
        // skipping the runtime dispatch (descriptor/arity checks, indirect code).
        if let ExprKind::Local(f) = func.kind
            && let Some(&fnid) = self.closure_locals.get(&f.index())
            && args.len() == self.lowered.fns[fnid.index()].params.len()
        {
            return self.direct_closure_call(f, fnid, args, result_ty);
        }
        // Otherwise route through `apply_n`, whose slots are uniform `i64`: float
        // and raw-int arguments are boxed/tagged in, and a `Float`/`Int` result comes
        // back boxed/tagged and owned, unboxed to its raw representation.
        let callee = self.expr(func);
        let vals: Vec<Value> = args.iter().map(|a| self.expr_boxed(a)).collect();
        let boxed = self.apply_n(callee, &vals);
        match result_ty {
            Ty::Con(Con::Float) => self.owning_unbox(boxed),
            Ty::Con(Con::Int) => self.as_raw_int(boxed),
            _ => boxed,
        }
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

    /// Direct-calls the lifted function a closure local is bound to: its environment
    /// is the closure cell's env region (borrowed during the call), its arguments are
    /// boxed into the uniform slot array, and the closure is dropped afterward — the
    /// same machine call `fai_apply_n` makes for a saturated closure, minus the
    /// dispatch (descriptor/arity checks and the indirect code pointer). The closure
    /// may be static, stack, or heap: the env pointer and the final drop are uniform
    /// across all three.
    fn direct_closure_call(
        &mut self,
        f: LocalId,
        fnid: FnId,
        args: &[CExpr],
        result_ty: &Ty,
    ) -> Value {
        let closure = self.use_var(f);
        let vals: Vec<Value> = args.iter().map(|a| self.expr_boxed(a)).collect();
        let args_ptr = self.spill(&vals);
        let env = self.builder.ins().iadd_imm(closure, rt::CLOSURE_ENV_OFFSET as i64);
        let code_id = self.fn_ids[fnid.index()];
        let fref = self.module.declare_func_in_func(code_id, self.builder.func);
        let call = self.builder.ins().call(fref, &[env, args_ptr]);
        let boxed = self.builder.inst_results(call)[0];
        // Consume the closure exactly as `fai_apply_n` does — after the call, the
        // environment having been borrowed during it.
        self.call_drop(closure);
        match result_ty {
            Ty::Con(Con::Float) => self.owning_unbox(boxed),
            Ty::Con(Con::Int) => self.as_raw_int(boxed),
            _ => boxed,
        }
    }

    /// Marshals `args` into registers per `def`'s [`FnAbi`] (a scalar-float argument
    /// in an `f64` register, a monomorphic-int argument as a raw untagged `i64`,
    /// every other as the boxed/immediate word, behind the leading null environment)
    /// and direct-calls it, yielding the raw result (an `f64` register for a scalar
    /// float, a raw `i64` recorded raw for a monomorphic int, else the uniform word).
    fn direct_call_value(&mut self, def: DefId, args: &[CExpr]) -> Value {
        let abi = (self.signature_of)(def);
        let borrowed = (self.borrows_of)(def);
        let null_env = self.builder.ins().iconst(types::I64, 0);
        let mut call_args = Vec::with_capacity(args.len() + 1);
        call_args.push(null_env);
        // Boxes freshly created from an unboxed scalar for a **borrowed** uniform
        // parameter are caller-owned temporaries the callee inspects but does not
        // drop, so the caller releases them after the call. A spread parameter
        // contributes its N `f64` components (see [`Self::marshal_args`]).
        let mut lent_boxes = Vec::new();
        self.marshal_args(&abi, &borrowed, args, &mut call_args, &mut lent_boxes);
        let result = self.direct_call(def, args.len(), &abi, &call_args);
        for b in lent_boxes {
            self.call_drop(b);
        }
        // A register int result arrives untagged; record it raw so callers treat it
        // so (its `I64` type cannot convey this).
        if abi.int_return() {
            self.mark_raw(result);
        }
        // A niche result arrives as the wrapper-free encoding; record it so callers
        // convert it to standard only where it crosses a uniform slot.
        if let Some(k) = abi.niche_return() {
            self.mark_niche(result, k);
        }
        result
    }

    /// A saturated direct call to top-level `def`. The raw result
    /// ([`Self::direct_call_value`]) is coerced to `result_ty`'s representation (the
    /// invariant: `f64` iff a scalar `Float`, a raw `i64` iff a monomorphic `Int`),
    /// unboxing a generic callee's boxed `Float`/`Int`.
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
        match result_ty {
            Ty::Con(Con::Float) => self.owning_unbox(boxed),
            Ty::Con(Con::Int) => self.as_raw_int(boxed),
            _ => boxed,
        }
    }

    /// Coerces `v` to the representation of `ty` (the invariant: `f64` iff a scalar
    /// `Float`, a raw untagged `i64` iff a monomorphic `Int`, else the uniform
    /// boxed/tagged word), unboxing/untagging or boxing/tagging as needed.
    fn as_repr_of(&mut self, v: Value, ty: &Ty) -> Value {
        match ty {
            Ty::Con(Con::Float) => {
                if self.is_f64(v) {
                    v
                } else {
                    self.owning_unbox(v)
                }
            }
            Ty::Con(Con::Int) => self.as_raw_int(v),
            // A niche value (e.g. a niche-returning direct call's result) stays
            // niche; it is converted to standard only at a uniform-slot boundary.
            _ if self.niche_of(v).is_some() => v,
            _ => self.ensure_boxed(v),
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

    /// A saturated direct call forwarding reuse tokens to the callee's token-taking
    /// `{base}__reuse` entry: the leading null environment, then one register per
    /// token slot (a forwarded token value, or the null token `0` for a padded
    /// slot), then the value arguments in registers per the callee's ABI. The
    /// result is coerced to `result_ty`'s representation, as for a plain direct
    /// call.
    fn direct_reuse_application(
        &mut self,
        def: DefId,
        args: &[CExpr],
        reuse: &[Option<LocalId>],
        result_ty: &Ty,
    ) -> Value {
        let abi = (self.signature_of)(def);
        let borrowed = (self.borrows_of)(def);
        let null_env = self.builder.ins().iconst(types::I64, 0);
        let mut call_args = Vec::with_capacity(1 + reuse.len() + args.len());
        call_args.push(null_env);
        // One leading register per reuse-token slot: the forwarded token value, or
        // the null token for a padded slot (the runtime treats it as "allocate
        // fresh"). Tokens are raw `i64` words, never reference-counted.
        for slot in reuse {
            let v = match slot {
                Some(t) => self.use_var(*t),
                // The null reuse token (`0`): the runtime allocates fresh for it.
                None => self.builder.ins().iconst(types::I64, 0),
            };
            call_args.push(v);
        }
        // The value arguments follow, marshalled exactly as a plain direct call.
        let mut lent_boxes = Vec::new();
        self.marshal_args(&abi, &borrowed, args, &mut call_args, &mut lent_boxes);
        let name = reuse_symbol(self.namer, def);
        let sig = reuse_entry_signature(self.module, reuse.len(), args.len(), &abi);
        let id = self
            .module
            .declare_function(&name, Linkage::Import, &sig)
            .expect("declare reuse entry");
        let fref = self.module.declare_func_in_func(id, self.builder.func);
        let call = self.builder.ins().call(fref, &call_args);
        let result = self.builder.inst_results(call)[0];
        for b in lent_boxes {
            self.call_drop(b);
        }
        if abi.int_return() {
            self.mark_raw(result);
        }
        self.as_repr_of(result, result_ty)
    }

    // -----------------------------------------------------------------------
    // Spread (fixed-shape float aggregate) calling convention.
    // -----------------------------------------------------------------------

    /// Translates the body of a spread-result function, returning its N `f64`
    /// components multi-value at every tail. A tail `if` returns from each branch
    /// directly (no merge); binders are emitted with their continuation recursed.
    fn spread_return_body(&mut self, e: &CExpr, n: usize) {
        match &e.kind {
            ExprKind::Spread { components } => {
                let vals: Vec<Value> = components.iter().map(|c| self.expr_f64(c)).collect();
                self.builder.ins().return_(&vals);
            }
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
                self.spread_return_body(then, n);
                self.builder.switch_to_block(else_b);
                self.builder.seal_block(else_b);
                self.spread_return_body(els, n);
            }
            ExprKind::Let { local, value, body } => {
                let v = self.expr(value);
                self.define_var(*local, v);
                self.bce_transfer(*local, value);
                self.spread_return_body(body, n);
            }
            ExprKind::LetMany { locals, value, body } => {
                self.bind_letmany(locals, value);
                self.spread_return_body(body, n);
            }
            ExprKind::Reset { value, token, body } => {
                let v = self.expr(value);
                let tok = self.call1("fai_drop_reuse", v);
                self.define_var(*token, tok);
                self.spread_return_body(body, n);
            }
            ExprKind::FreeReuse { token, body } => {
                let tok = self.use_var(*token);
                let f = self.runtime("fai_free_reuse", 1, false);
                self.builder.ins().call(f, &[tok]);
                self.spread_return_body(body, n);
            }
            ExprKind::Dup { local, body } => {
                self.dup_local(*local);
                self.spread_return_body(body, n);
            }
            ExprKind::Drop { local, body } => {
                self.drop_local(*local);
                self.spread_return_body(body, n);
            }
            // A boxed FFA value as the tail (defensive: SROA usually emits a
            // `Spread`): explode it into its scalar components and return them.
            _ => {
                let base = self.expr(e);
                let vals = self.explode_boxed(base, n);
                self.call_drop(base);
                self.builder.ins().return_(&vals);
            }
        }
    }

    /// Evaluates `e` to an `f64` (a component value is a scalar float; a boxed
    /// `Float` is unboxed, consuming it).
    fn expr_f64(&mut self, e: &CExpr) -> Value {
        let v = self.expr(e);
        if self.is_f64(v) { v } else { self.owning_unbox(v) }
    }

    /// Reads the N scalar-`f64` fields of a boxed FFA cell `base` (borrowing — the
    /// caller releases `base`).
    fn explode_boxed(&mut self, base: Value, n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| {
                let addr =
                    self.field_slot_addr(base, FieldIndex::Const(u32::try_from(i).unwrap_or(0)));
                let bits = self.builder.ins().load(types::I64, MemFlags::trusted(), addr, 0);
                self.i64_to_f64(bits)
            })
            .collect()
    }

    /// Binds a `LetMany`'s locals to the components a spread-returning call yields.
    fn bind_letmany(&mut self, locals: &[LocalId], value: &CExpr) {
        let results = self.spread_call(value);
        debug_assert_eq!(results.len(), locals.len());
        for (&l, v) in locals.iter().zip(results) {
            self.define_var(l, v);
        }
    }

    /// Direct-calls a saturated spread-returning callee, yielding its N `f64`
    /// result components.
    fn spread_call(&mut self, call: &CExpr) -> Vec<Value> {
        let ExprKind::App { func, args, .. } = &call.kind else {
            unreachable!("spread_call on a non-App")
        };
        let ExprKind::Global(def) = func.kind else {
            unreachable!("spread_call on an indirect call")
        };
        self.spread_call_parts(def, args)
    }

    /// Marshals `args` and direct-calls spread-returning `def`, yielding its N `f64`
    /// result components.
    fn spread_call_parts(&mut self, def: DefId, args: &[CExpr]) -> Vec<Value> {
        let abi = (self.signature_of)(def);
        let borrowed = (self.borrows_of)(def);
        let null_env = self.builder.ins().iconst(types::I64, 0);
        let mut call_args = vec![null_env];
        let mut lent_boxes = Vec::new();
        self.marshal_args(&abi, &borrowed, args, &mut call_args, &mut lent_boxes);
        let n = abi.spread_return().map_or(1, <[_]>::len);
        let results = self.direct_call_n(def, args.len(), &abi, &call_args, n);
        for b in lent_boxes {
            self.call_drop(b);
        }
        results
    }

    /// Reassembles `n` spread component `f64` values into a boxed scalar-slot cell
    /// (the in-cell `f64` layout), used where a spread call's result crosses a
    /// uniform boundary reached without a tracked local.
    fn box_components(&mut self, comps: &[Value]) -> Value {
        let n = comps.len();
        let scalars = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        let bits: Vec<Value> = comps.iter().map(|&c| self.float_field_bits(c)).collect();
        let ptr = self.spill(&bits);
        let tag_v = self.builder.ins().iconst(types::I64, 0);
        let n_v = self.builder.ins().iconst(types::I64, n as i64);
        let desc = self.data_descriptor(scalars);
        let f = self.runtime("fai_make_data_scalar", 4, true);
        let call = self.builder.ins().call(f, &[desc, tag_v, n_v, ptr]);
        self.builder.inst_results(call)[0]
    }

    /// Marshals `args` into `call_args` per the callee `abi`: a spread parameter
    /// contributes its N `f64` components, every other parameter one word (the same
    /// rule as [`Self::direct_call_value`], factored for the spread paths). Boxes
    /// created for a borrowed parameter are pushed to `lent_boxes` (released by the
    /// caller after the call).
    fn marshal_args(
        &mut self,
        abi: &FnAbi,
        borrowed: &[bool],
        args: &[CExpr],
        call_args: &mut Vec<Value>,
        lent_boxes: &mut Vec<Value>,
    ) {
        for (i, a) in args.iter().enumerate() {
            if let Some(reprs) = abi.spread_param(i) {
                for cv in self.spread_arg_values(a, reprs.len()) {
                    call_args.push(cv);
                }
            } else if abi.float_param(i) {
                let v = self.expr(a);
                call_args.push(if self.is_f64(v) { v } else { self.owning_unbox(v) });
            } else if abi.int_param(i) {
                let v = self.expr(a);
                let v = self.as_raw_int(v);
                call_args.push(v);
            } else if let Some(k) = abi.niche_param(i) {
                let v = self.expr(a);
                let v = self.ensure_niche(v, k);
                call_args.push(v);
            } else {
                let raw = self.expr(a);
                let is_borrowed = borrowed.get(i).copied().unwrap_or(false);
                let v = if let Some(k) = self.niche_of(raw) {
                    if is_borrowed {
                        let dup = self.call1("fai_dup", raw);
                        let std = self.niche_to_std(dup, k);
                        lent_boxes.push(std);
                        std
                    } else {
                        self.niche_to_std(raw, k)
                    }
                } else {
                    let boxed = self.ensure_boxed(raw);
                    if boxed != raw && is_borrowed {
                        lent_boxes.push(boxed);
                    }
                    boxed
                };
                call_args.push(v);
            }
        }
    }

    /// The N `f64` component values of a spread call argument: a `Spread` node's
    /// components evaluated, or (defensively) a boxed FFA value exploded.
    fn spread_arg_values(&mut self, a: &CExpr, n: usize) -> Vec<Value> {
        if let ExprKind::Spread { components } = &a.kind {
            components.iter().map(|c| self.expr_f64(c)).collect()
        } else {
            let base = self.expr(a);
            let vals = self.explode_boxed(base, n);
            self.call_drop(base);
            vals
        }
    }

    /// Direct-calls `def` returning its `n` `f64` result components (the multi-result
    /// register convention).
    fn direct_call_n(
        &mut self,
        def: DefId,
        arity: usize,
        abi: &FnAbi,
        call_args: &[Value],
        n: usize,
    ) -> Vec<Value> {
        let name = code_symbol(self.namer, def);
        let sig = entry_signature(self.module, arity, abi);
        let id = self.module.declare_function(&name, Linkage::Import, &sig).expect("declare code");
        let fref = self.module.declare_func_in_func(id, self.builder.func);
        let call = self.builder.ins().call(fref, call_args);
        self.builder.inst_results(call)[..n].to_vec()
    }

    fn make_closure(&mut self, func: FnId, captures: &[LocalId], alloc: ClosureAlloc) -> Value {
        match alloc {
            ClosureAlloc::Static => self.static_closure(func),
            ClosureAlloc::Stack => self.stack_closure(func, captures),
            ClosureAlloc::Heap => self.heap_closure(func, captures),
        }
    }

    /// A non-capturing lambda has no per-activation environment, so it shares one
    /// immortal static closure (declared/defined by `build_def`) rather than
    /// allocating a cell at every evaluation — exactly like a top-level function
    /// referenced as a value. The immortal reference count makes the shared cell's
    /// `dup`/`drop` balance harmlessly (it is never freed).
    fn static_closure(&mut self, func: FnId) -> Value {
        let data = self.lambda_closures[func.index()].expect("non-capturing lambda static closure");
        let ptr = self.ptr();
        let gv = self.module.declare_data_in_func(data, self.builder.func);
        self.builder.ins().symbol_value(ptr, gv)
    }

    /// A capturing lambda that provably does not escape its creating activation:
    /// the closure cell lives in this stack frame instead of the heap. It is laid
    /// out exactly like a heap closure (so `apply_n`, `dup`, and the env scan are
    /// unchanged) but tagged with the stack descriptor, so when its reference count
    /// reaches zero the runtime releases its captures yet does *not* free the cell
    /// (the frame reclaims the slot on return). Escape analysis guarantees no
    /// reference outlives the frame, so the stack pointer never dangles.
    fn stack_closure(&mut self, func: FnId, captures: &[LocalId]) -> Value {
        let arity = self.lowered.fns[func.index()].params.len() as i64;
        let n = captures.len();
        let size = rt::CLOSURE_ENV_OFFSET + n * 8;
        let ptr = self.ptr();

        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            u32::try_from(size).expect("closure cell size"),
            3, // 8-byte alignment: a closure value is a tagged pointer (low bits clear).
        ));
        let addr = self.builder.ins().stack_addr(ptr, slot, 0);

        // Header: rc = 1, descriptor = &FAI_STACK_CLOSURE_DESC, size.
        let one = self.builder.ins().iconst(types::I64, 1);
        self.store_field(addr, rt::RC_OFFSET, one);
        let desc = self.runtime_data_addr("FAI_STACK_CLOSURE_DESC");
        self.store_field(addr, rt::DESC_OFFSET, desc);
        let size_v = self.builder.ins().iconst(types::I64, size as i64);
        self.store_field(addr, rt::SIZE_OFFSET, size_v);

        // Code pointer, arity, env count.
        let code_id = self.fn_ids[func.index()];
        let fref = self.module.declare_func_in_func(code_id, self.builder.func);
        let code_ptr = self.builder.ins().func_addr(ptr, fref);
        self.store_field(addr, rt::CLOSURE_CODE_OFFSET, code_ptr);
        let arity_v = self.builder.ins().iconst(types::I64, arity);
        self.store_field(addr, rt::CLOSURE_ARITY_OFFSET, arity_v);
        let count_v = self.builder.ins().iconst(types::I64, n as i64);
        self.store_field(addr, rt::CLOSURE_ENV_COUNT_OFFSET, count_v);

        // Captured environment slots. The reference-count pass has already
        // duplicated each capture where it is still live afterward (`MakeClosure`
        // consumes its captures). Slots are uniform `i64`, so a captured float is
        // boxed in.
        for (i, &c) in captures.iter().enumerate() {
            let v = self.use_var(c);
            let boxed = self.ensure_boxed(v);
            self.store_field(addr, rt::CLOSURE_ENV_OFFSET + i * 8, boxed);
        }
        addr
    }

    /// A capturing lambda that may escape: a heap-allocated, reference-counted cell
    /// built by the runtime.
    fn heap_closure(&mut self, func: FnId, captures: &[LocalId]) -> Value {
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

    /// Stores `value` at byte `offset` of the object at `addr` (a trusted,
    /// 8-aligned heap/stack cell write).
    fn store_field(&mut self, addr: Value, offset: usize, value: Value) {
        let off = i32::try_from(offset).expect("field offset");
        self.builder.ins().store(MemFlags::trusted(), value, addr, off);
    }

    /// The address of an imported runtime data symbol (e.g. a static descriptor).
    fn runtime_data_addr(&mut self, name: &str) -> Value {
        let ptr = self.ptr();
        let id =
            self.module.declare_data(name, Linkage::Import, false, false).expect("runtime data");
        let gv = self.module.declare_data_in_func(id, self.builder.func);
        self.builder.ins().symbol_value(ptr, gv)
    }

    /// Builds an under-applied call to a known function as a stack-allocated partial
    /// application — the [`stack_closure`](Self::stack_closure) analogue for a PAP.
    /// Escape analysis has proven the partial application does not outlive the frame,
    /// so its cell lives here (tagged `KIND_STACK_PAP`); the runtime applies it,
    /// `dup`s it, and scans its children exactly as for a heap PAP, but releases its
    /// children without freeing the cell when it dies. The target is the callee's
    /// immortal static closure, the stored arguments are owned (uniform `i64`).
    fn stack_pap(&mut self, def: DefId, args: &[CExpr]) -> Value {
        let n = args.len();
        let size = rt::PAP_ARGS_OFFSET + n * 8;
        let ptr = self.ptr();

        // Evaluate the boxed arguments before reserving the cell.
        let arg_vals: Vec<Value> = args.iter().map(|a| self.expr_boxed(a)).collect();

        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            u32::try_from(size).expect("pap cell size"),
            3, // 8-byte alignment: a function value is a tagged pointer.
        ));
        let addr = self.builder.ins().stack_addr(ptr, slot, 0);

        // Header: rc = 1, descriptor = &FAI_STACK_PAP_DESC, size.
        let one = self.builder.ins().iconst(types::I64, 1);
        self.store_field(addr, rt::RC_OFFSET, one);
        let desc = self.runtime_data_addr("FAI_STACK_PAP_DESC");
        self.store_field(addr, rt::DESC_OFFSET, desc);
        let size_v = self.builder.ins().iconst(types::I64, size as i64);
        self.store_field(addr, rt::SIZE_OFFSET, size_v);

        // Target function: the callee's immortal static closure (its value form).
        let name = closure_symbol(self.namer, def);
        let data_id = self
            .module
            .declare_data(&name, Linkage::Import, false, false)
            .expect("pap target closure");
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        let func_val = self.builder.ins().symbol_value(ptr, gv);
        self.store_field(addr, rt::PAP_FUNC_OFFSET, func_val);
        let nargs_v = self.builder.ins().iconst(types::I64, n as i64);
        self.store_field(addr, rt::PAP_NARGS_OFFSET, nargs_v);
        for (i, &v) in arg_vals.iter().enumerate() {
            self.store_field(addr, rt::PAP_ARGS_OFFSET + i * 8, v);
        }
        addr
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
        // Refine the fact graph along each branch with the dominating guard; restore
        // the pre-branch facts afterward (neither branch's added facts survive the
        // merge).
        let saved_bounds = self.bounds.clone();
        self.bounds.refine(cond, true);
        let tv = self.expr(then);
        let merge_b = self.builder.create_block();
        let merge_ty = self.builder.func.dfg.value_type(tv);
        self.builder.append_block_param(merge_b, merge_ty);
        // The merge representation follows the then-branch value (a desugared
        // `match`'s `<error>` fall-through is always in the else position, so the
        // then value is reliable): `f64` for an unboxed float, a raw untagged word
        // for an unboxed int (recorded below), else the uniform word.
        let merge_raw = self.is_raw_int(tv);
        // The merge also follows the then-branch's niche representation; the else
        // branch is reconciled to it below (the two branches share a type, but not
        // necessarily a representation — one may be a niche value and the other a
        // standard one from a generic source).
        let merge_niche = self.niche_of(tv);
        self.builder.ins().jump(merge_b, &[tv.into()]);

        self.builder.switch_to_block(else_b);
        self.builder.seal_block(else_b);
        self.bounds = saved_bounds.clone();
        self.bounds.refine(cond, false);
        let ev = self.expr(els);
        // Reconcile the else value to the merge's representation: to the then
        // branch's niche scheme (converting a standard value), or to standard if the
        // then branch is standard but the else is niche.
        let ev = match merge_niche {
            Some(k) => self.ensure_niche(ev, k),
            None if self.niche_of(ev).is_some() => self.ensure_boxed(ev),
            None => ev,
        };
        // The two branches share a type, so they share a representation — except a
        // desugared `match`'s unreachable fall-through (`<error>`), which is a bare
        // word; reinterpret it to the merge type (its value is never observed). The
        // raw-int distinction is the same Cranelift `I64` type, so it needs no
        // bitcast here — the merge parameter's raw-ness is recorded from the then
        // value.
        let ev = self.coerce_repr(ev, merge_ty);
        self.builder.ins().jump(merge_b, &[ev.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        // Past the merge only the pre-branch facts hold.
        self.bounds = saved_bounds;
        let result = self.builder.block_params(merge_b)[0];
        if merge_raw {
            self.mark_raw(result);
        }
        if let Some(k) = merge_niche {
            self.mark_niche(result, k);
        }
        result
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

        let prev = self.loop_ctx.replace(LoopCtx {
            header,
            exit,
            params: params.to_vec(),
            exit_raw: false,
            exit_niche: None,
        });
        // Precompute each loop-carried generic array parameter's float self-tag here
        // at the header (which dominates the loop body), keyed by the header's
        // per-iteration block-parameter value, so the body's generic element
        // accesses reuse one descriptor load instead of one per element touch. Only
        // when the body re-tags no buffer (see [`Self::array_float_tag`]); the key is
        // this loop's value, so it neither conflicts with the entry cache nor leaks
        // to post-loop code (those use other values).
        if self.array_tag_cacheable {
            let carried: Vec<LocalId> = params
                .iter()
                .copied()
                .filter(|p| self.var_ty(*p).is_some_and(is_generic_array))
                .collect();
            for p in carried {
                let base = self.use_var(p);
                let tag = self.array_is_float(base);
                self.array_float_tag.insert(base, tag);
            }
        }
        // The loop body is analyzed from the entry facts (the inductively-valid loop
        // invariant): the loop-carried parameters are the function's parameters, and
        // the fold proved the entry facts hold at every back-edge.
        self.bounds = self.entry_bounds.clone();
        self.expr_tail(body);
        // Capture the loop result's representation (set by the tail values) before
        // restoring the enclosing loop context.
        let exit_raw = self.loop_ctx.as_ref().is_some_and(|c| c.exit_raw);
        let exit_niche = self.loop_ctx.as_ref().and_then(|c| c.exit_niche);
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
        let result = self.builder.block_params(exit)[0];
        if exit_raw {
            self.mark_raw(result);
        }
        if let Some(k) = exit_niche {
            self.mark_niche(result, k);
        }
        result
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
            // Fix the loop result's representation from this first tail value (a raw
            // untagged int and a niche `Option` are both `I64`, indistinguishable
            // from a tagged word by type).
            let raw = self.is_raw_int(v);
            let niche = self.niche_of(v);
            if let Some(ctx) = self.loop_ctx.as_mut() {
                ctx.exit_raw = raw;
                ctx.exit_niche = niche;
            }
            v
        } else {
            // Reconcile this tail value to the loop result's representation (fixed by
            // the first tail value), then to the exit block parameter's type.
            let exit_niche = self.loop_ctx.as_ref().and_then(|c| c.exit_niche);
            let v = match exit_niche {
                Some(k) => self.ensure_niche(v, k),
                None if self.niche_of(v).is_some() => self.ensure_boxed(v),
                None => v,
            };
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
                let saved_bounds = self.bounds.clone();
                self.builder.switch_to_block(then_b);
                self.builder.seal_block(then_b);
                self.bounds.refine(cond, true);
                self.expr_tail(then);
                self.builder.switch_to_block(else_b);
                self.builder.seal_block(else_b);
                self.bounds = saved_bounds;
                self.bounds.refine(cond, false);
                self.expr_tail(els);
            }
            ExprKind::Let { local, value, body } => {
                let v = self.expr(value);
                self.define_var(*local, v);
                self.bce_transfer(*local, value);
                self.expr_tail(body);
            }
            ExprKind::LetMany { locals, value, body } => {
                self.bind_letmany(locals, value);
                self.expr_tail(body);
            }
            ExprKind::Reset { value, token, body } => {
                let v = self.expr(value);
                let tok = self.call1("fai_drop_reuse", v);
                self.define_var(*token, tok);
                self.expr_tail(body);
            }
            ExprKind::FreeReuse { token, body } => {
                let tok = self.use_var(*token);
                let f = self.runtime("fai_free_reuse", 1, false);
                self.builder.ins().call(f, &[tok]);
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
    // An `Array` (applied as `App(Con::Array, elem)`) is always a heap object —
    // even an empty one — so it never needs the maybe-immediate tag check.
    fn array_head(ty: &Ty) -> bool {
        match ty {
            Ty::Con(Con::Array) => true,
            Ty::App(h, _) => array_head(h),
            _ => false,
        }
    }
    match ty {
        Ty::Con(Con::String | Con::Float) | Ty::Tuple(_) | Ty::Interface(_) | Ty::Arrow(..) => true,
        Ty::Record(row) => !row.fields.is_empty(),
        _ => array_head(ty),
    }
}

/// Whether a value of `ty` *may* be an immediate at runtime even though the type
/// is not statically a known immediate or scalar: a **type variable** (the
/// instantiation is unknown, and is very often `Int`/`Char`/a nullary tag), or a
/// discriminated union / `List` / empty record (whose nullary constructors are
/// immediates). For these, structural `=`/`compare` is lowered with an inline
/// immediate fast path that guards the out-of-line structural fallback, so the
/// common immediate case (e.g. `Int` keys in a generic `Dict`, or sorting an
/// `Int` list) is an inline word compare rather than a runtime call.
///
/// Excludes the statically immediate types (handled bare), the scalar `Int`/
/// `Float` (handled by their own branches), every [`is_always_boxed_ty`] type
/// (always boxed, so the guard would always miss — keep the direct structural
/// call), and `Ty::Error` (an erased mutual-recursion combined function or an
/// ill-typed program — keep the conservative call).
fn is_maybe_immediate_ty(ty: &Ty) -> bool {
    !is_immediate_ty(ty)
        && !is_always_boxed_ty(ty)
        && !matches!(ty, Ty::Con(Con::Int) | Ty::Con(Con::Float) | Ty::Error)
}

/// The most fields a fixed-shape aggregate may have for its `=`/`compare` to be
/// inlined field-wise; a wider cell keeps the out-of-line structural call (the
/// runtime loop handles any width in fixed code). Every field emits inline
/// comparison code, so this bounds generated-code growth.
const AGG_FIELD_CAP: usize = 8;

/// How a fixed-shape aggregate field is compared inline (see
/// [`inline_aggregate_fields`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AggField {
    /// A statically-immediate field (`Bool`/`Char`/`Unit`): the slot word is a
    /// tagged immediate, so `=` is a bare word compare and `compare` untags and
    /// compares the payloads.
    Immediate,
    /// An `Int` field: the slot word is a tagged immediate or a boxed (overflowed)
    /// `Int`, so comparison takes the immediate guard with the borrowed structural
    /// fallback (the scalar-`Int` path, on the borrowed field word).
    Int,
    /// Any other uniform field (a type variable, `String`, `List`, an ADT, or a
    /// nested aggregate): a boxed-or-immediate value word compared through the
    /// borrowing structural runtime call, which dispatches on the field's own
    /// descriptor.
    Boxed,
}

/// Classifies a fixed-shape aggregate operand for inline field-wise comparison,
/// returning each field's [`AggField`] in heap-layout order (tuples positional,
/// records sorted by label), or `None` when the shape is not inline-eligible: a
/// non-aggregate, an open or empty record, a width over `cap`, a field whose type
/// is erased (`Ty::Error`), or a **direct `Float` field**. A monomorphic `Float`
/// field is stored as raw `f64` bits, which can be neither read soundly by static
/// type (a generically-built cell stores the field boxed) nor passed to the
/// structural runtime (which expects a value word, not raw bits) — so a shape with
/// a direct `Float` field keeps the descriptor-driven runtime call. A *nested*
/// aggregate or a `Float` reached through a boxed field is a pointer slot, so it
/// stays eligible and is compared as [`AggField::Boxed`].
fn inline_aggregate_fields(ty: &Ty, cap: usize) -> Option<Vec<AggField>> {
    let field_tys: Vec<&Ty> = match ty {
        Ty::Tuple(elems) => elems.iter().collect(),
        Ty::Record(row) if row.tail == RowEnd::Closed && !row.fields.is_empty() => {
            row.fields.iter().map(|(_, t)| t).collect()
        }
        _ => return None,
    };
    if field_tys.is_empty() || field_tys.len() > cap {
        return None;
    }
    let mut fields = Vec::with_capacity(field_tys.len());
    for t in field_tys {
        let kind = match t {
            // A raw-bits scalar `Float` or an erased field is not inline-eligible.
            Ty::Con(Con::Float) | Ty::Error => return None,
            _ if is_immediate_ty(t) => AggField::Immediate,
            Ty::Con(Con::Int) => AggField::Int,
            _ => AggField::Boxed,
        };
        fields.push(kind);
    }
    Some(fields)
}

/// Whether `ty` is a boxed *leaf* — a heap object with no reference-counted
/// children — so a dead one is freed directly, with no child release. The boxed
/// `Int` and `Float` kinds are leaves. `String` is **not**: a `String` value may be
/// a borrowing slice whose one child is the base it views, so a dead string is
/// released through the child-scanning runtime drop (which is a direct free for the
/// inline representation and releases the base for a slice).
fn is_leaf_boxed_ty(ty: &Ty) -> bool {
    matches!(ty, Ty::Con(Con::Int | Con::Float))
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
        ExprKind::App { func, args, .. } => {
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
        // A spread's components and a letmany's bound locals are scalar `Float`s.
        ExprKind::Spread { components } => {
            components.iter().for_each(|a| collect_local_types(a, out));
        }
        ExprKind::LetMany { locals, value, body } => {
            for l in locals {
                note(out, *l, &Ty::Con(Con::Float));
            }
            collect_local_types(value, out);
            collect_local_types(body, out);
        }
        ExprKind::DataTag { base, .. } => collect_local_types(base, out),
        ExprKind::DataField { base, .. } => collect_local_types(base, out),
        ExprKind::Reset { value, body, .. } => {
            collect_local_types(value, out);
            collect_local_types(body, out);
        }
        ExprKind::FreeReuse { body, .. } => collect_local_types(body, out),
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
        ExprKind::App { func, args, .. } => {
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
        // A spread's components and a letmany's bound locals are scalar `Float`s, so
        // the bound locals are observed as float even if otherwise unused (they must
        // receive an `f64` multi-value result).
        ExprKind::Spread { components } => {
            components.iter().for_each(|a| collect_float_observations(a, float_seen, other_seen));
        }
        ExprKind::LetMany { locals, value, body } => {
            for l in locals {
                note(*l, &Ty::Con(Con::Float), float_seen, other_seen);
            }
            collect_float_observations(value, float_seen, other_seen);
            collect_float_observations(body, float_seen, other_seen);
        }
        ExprKind::DataTag { base, .. } | ExprKind::DataField { base, .. } => {
            collect_float_observations(base, float_seen, other_seen);
        }
        ExprKind::Reset { value, body, .. } => {
            collect_float_observations(value, float_seen, other_seen);
            collect_float_observations(body, float_seen, other_seen);
        }
        ExprKind::FreeReuse { body, .. } => {
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

/// Records, across the body, which locals are observed as `Int` (`int_seen`) and
/// which as any other (or unknown) type (`other_seen`), at the same points
/// [`collect_local_types`] reads: every `Local` use and each `let` binding. A
/// local is an untagged raw `i64` only when it is in `int_seen` and not in
/// `other_seen` (see [`Translator::collect_int_locals`]). Offset-evidence locals
/// used only inside a `FieldIndex::Dyn` (never as a bare `Local` node) are not
/// observed here, so they are not classified raw — and entry evidence parameters
/// are forced out regardless during ABI reconciliation.
fn collect_int_observations(
    e: &CExpr,
    int_seen: &mut FxHashSet<usize>,
    other_seen: &mut FxHashSet<usize>,
) {
    fn note(
        local: LocalId,
        ty: &Ty,
        int_seen: &mut FxHashSet<usize>,
        other: &mut FxHashSet<usize>,
    ) {
        if matches!(ty, Ty::Con(Con::Int)) {
            int_seen.insert(local.index());
        } else {
            other.insert(local.index());
        }
    }
    match &e.kind {
        ExprKind::Local(l) => note(*l, &e.ty, int_seen, other_seen),
        ExprKind::Lit(_) | ExprKind::Global(_) | ExprKind::MakeClosure { .. } | ExprKind::Error => {
        }
        ExprKind::Prim { args, .. } | ExprKind::MakeData { args, .. } => {
            args.iter().for_each(|a| collect_int_observations(a, int_seen, other_seen));
        }
        ExprKind::App { func, args, .. } => {
            collect_int_observations(func, int_seen, other_seen);
            args.iter().for_each(|a| collect_int_observations(a, int_seen, other_seen));
        }
        ExprKind::If { cond, then, els } => {
            collect_int_observations(cond, int_seen, other_seen);
            collect_int_observations(then, int_seen, other_seen);
            collect_int_observations(els, int_seen, other_seen);
        }
        ExprKind::Let { local, value, body } => {
            note(*local, &value.ty, int_seen, other_seen);
            collect_int_observations(value, int_seen, other_seen);
            collect_int_observations(body, int_seen, other_seen);
        }
        // A spread's components and a letmany's bound locals are `Float`, never raw
        // `Int`; recurse (the bound locals are simply not int-observed).
        ExprKind::Spread { components } => {
            components.iter().for_each(|a| collect_int_observations(a, int_seen, other_seen));
        }
        ExprKind::LetMany { value, body, .. } => {
            collect_int_observations(value, int_seen, other_seen);
            collect_int_observations(body, int_seen, other_seen);
        }
        ExprKind::DataTag { base, .. } | ExprKind::DataField { base, .. } => {
            collect_int_observations(base, int_seen, other_seen);
        }
        ExprKind::Reset { value, body, .. } => {
            collect_int_observations(value, int_seen, other_seen);
            collect_int_observations(body, int_seen, other_seen);
        }
        ExprKind::FreeReuse { body, .. } => {
            collect_int_observations(body, int_seen, other_seen);
        }
        ExprKind::Dup { body, .. } | ExprKind::Drop { body, .. } => {
            collect_int_observations(body, int_seen, other_seen);
        }
        ExprKind::Join { body, .. } | ExprKind::HoleStart { body, .. } => {
            collect_int_observations(body, int_seen, other_seen);
        }
        ExprKind::Recur { args } => {
            args.iter().for_each(|a| collect_int_observations(a, int_seen, other_seen));
        }
        ExprKind::HoleFill { cell, .. } => {
            collect_int_observations(cell, int_seen, other_seen);
        }
        ExprKind::HoleClose { base, .. } => {
            collect_int_observations(base, int_seen, other_seen);
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

/// The inlined drop strategy for a value in *uniform* (boxed/tagged) representation
/// — an element read from an array slot, where a `Float` is the boxed cell it is in
/// the slot rather than the unboxed scalar a `Float` *local* is. Identical to
/// [`drop_class`] except a `Float` is released as an always-boxed leaf, not skipped.
fn uniform_drop_class(ty: &Ty) -> DropPlan {
    if matches!(ty, Ty::Con(Con::Float)) {
        return DropPlan::Leaf { tag_check: false };
    }
    drop_class(ty)
}

/// The element type of an `Array` operand (`App(Con::Array, elem)`), or `None` when
/// the type is not a recognizable array — a bare `Ty::Error` operand or a non-array
/// type — so the caller keeps the out-of-line runtime call. An erased
/// `App(Con::Array, Error)` (the combined mutual-recursion function) still matches,
/// with `Error` as its element, handled by the uniform inline path.
fn array_elem(ty: &Ty) -> Option<&Ty> {
    match ty {
        Ty::App(head, elem) if matches!(head.as_ref(), Ty::Con(Con::Array)) => Some(elem),
        _ => None,
    }
}

/// Whether an array element type might be a raw-`f64` slot at runtime — a type
/// variable or an erased element (the combined mutual-recursion function), where
/// the static type cannot decide. A concrete element (`Int`, `Float`, a record,
/// `String`, …) is decided statically, so it never needs the runtime self-tag
/// check. Drives the generic get/set/push paths, which branch on the array's (or
/// pushed value's) descriptor at runtime.
fn elem_may_be_float(elem: &Ty) -> bool {
    matches!(elem, Ty::Var(_) | Ty::Error)
}

/// Whether `ty` is an `Array` whose element might be a raw-`f64` slot at runtime —
/// i.e. a generic-element array (`Array 'a`). Such an array's element access takes
/// the runtime self-tag path, so precomputing its self-tag once pays off.
fn is_generic_array(ty: &Ty) -> bool {
    array_elem(ty).is_some_and(elem_may_be_float)
}

/// Whether this (already lambda-lifted) function body contains an `Array`
/// construction (`ArrayWithCapacity`) or append (`ArrayPush`) primitive, so its
/// code generation inlines a pooled allocation and needs the thread's free-list
/// heads base fetched once at entry (see [`Translator::pool_heads_base`]). A
/// nested lambda's allocations live in its own lifted function, so they do not
/// match here.
fn body_uses_array_alloc(e: &CExpr) -> bool {
    match &e.kind {
        ExprKind::Prim { op, args, .. } => {
            matches!(op, Prim::ArrayWithCapacity | Prim::ArrayPush)
                || args.iter().any(body_uses_array_alloc)
        }
        ExprKind::Lit(_)
        | ExprKind::Local(_)
        | ExprKind::Global(_)
        | ExprKind::MakeClosure { .. }
        | ExprKind::Error => false,
        ExprKind::MakeData { args, .. }
        | ExprKind::Recur { args }
        | ExprKind::Spread { components: args } => args.iter().any(body_uses_array_alloc),
        ExprKind::App { func, args, .. } => {
            body_uses_array_alloc(func) || args.iter().any(body_uses_array_alloc)
        }
        ExprKind::If { cond, then, els } => {
            body_uses_array_alloc(cond) || body_uses_array_alloc(then) || body_uses_array_alloc(els)
        }
        ExprKind::Let { value, body, .. }
        | ExprKind::Reset { value, body, .. }
        | ExprKind::LetMany { value, body, .. } => {
            body_uses_array_alloc(value) || body_uses_array_alloc(body)
        }
        ExprKind::DataTag { base, .. } | ExprKind::DataField { base, .. } => {
            body_uses_array_alloc(base)
        }
        ExprKind::FreeReuse { body, .. }
        | ExprKind::Dup { body, .. }
        | ExprKind::Drop { body, .. }
        | ExprKind::Join { body, .. }
        | ExprKind::HoleStart { body, .. } => body_uses_array_alloc(body),
        ExprKind::HoleFill { cell, .. } => body_uses_array_alloc(cell),
        ExprKind::HoleClose { base, .. } => body_uses_array_alloc(base),
    }
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
    /// A scalar unboxed `f64` field (raw bits, no reference count): nothing to
    /// release.
    Scalar,
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

/// Classifies a field type for an inlined drop: a statically-immediate type and a
/// scalar `Float` (a raw unboxed slot, in a record/tuple) carry no reference count
/// and need no release; everything else is released with a runtime drop (itself a
/// no-op on a value that turns out to be immediate).
fn field_drop(ty: &Ty) -> FieldDrop {
    if is_immediate_ty(ty) {
        FieldDrop::Immediate
    } else if matches!(ty, Ty::Con(Con::Float)) {
        FieldDrop::Scalar
    } else {
        FieldDrop::Boxed
    }
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

    use super::{AggField, FieldDrop, fixed_shape_drop, inline_aggregate_fields};

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

    // --- inline_aggregate_fields: which shapes get inline field-wise comparison --

    use AggField::{Boxed as AggBoxed, Immediate as AggImmediate, Int as AggInt};

    #[test]
    fn int_tuple_is_inline_comparable() {
        let ty = Ty::Tuple(vec![Ty::int(), Ty::int()]);
        assert_eq!(inline_aggregate_fields(&ty, WIDE), Some(vec![AggInt, AggInt]));
    }

    #[test]
    fn record_inline_classes_follow_stored_layout_order() {
        // A real record's fields are stored sorted by label (the heap layout); the
        // classifier reads them in that stored order. Fields supplied here in label
        // order (`a` before `b`) yield classes in the same order.
        let ty = closed_record(&[("a", Ty::bool()), ("b", Ty::int())]);
        assert_eq!(inline_aggregate_fields(&ty, WIDE), Some(vec![AggImmediate, AggInt]));
    }

    #[test]
    fn mixed_immediate_int_boxed_tuple_classifies_each_field() {
        let ty = Ty::Tuple(vec![Ty::bool(), Ty::int(), Ty::Con(Con::String)]);
        assert_eq!(inline_aggregate_fields(&ty, WIDE), Some(vec![AggImmediate, AggInt, AggBoxed]));
    }

    #[test]
    fn direct_float_field_is_not_inline_comparable() {
        // A monomorphic Float field is a raw-bits slot: not inline-eligible.
        let ty = Ty::Tuple(vec![Ty::Con(Con::Float), Ty::int()]);
        assert_eq!(inline_aggregate_fields(&ty, WIDE), None);
    }

    #[test]
    fn nested_aggregate_field_stays_boxed_and_eligible() {
        // A nested tuple is a pointer slot (compared via the structural call), so
        // the outer shape is still eligible.
        let inner = Ty::Tuple(vec![Ty::int(), Ty::int()]);
        let ty = Ty::Tuple(vec![inner, Ty::int()]);
        assert_eq!(inline_aggregate_fields(&ty, WIDE), Some(vec![AggBoxed, AggInt]));
    }

    #[test]
    fn nested_float_aggregate_field_stays_eligible_as_boxed() {
        // A nested Float-bearing aggregate is a pointer slot, so the outer shape is
        // eligible and the nested field is compared structurally (descriptor-driven).
        let inner = Ty::Tuple(vec![Ty::Con(Con::Float), Ty::Con(Con::Float)]);
        let ty = Ty::Tuple(vec![Ty::int(), inner]);
        assert_eq!(inline_aggregate_fields(&ty, WIDE), Some(vec![AggInt, AggBoxed]));
    }

    #[test]
    fn type_variable_field_is_boxed_and_eligible() {
        // A type-variable field is a uniform value word, compared structurally.
        let ty = Ty::Tuple(vec![Ty::Var(fai_types::TyVarId(0)), Ty::int()]);
        assert_eq!(inline_aggregate_fields(&ty, WIDE), Some(vec![AggBoxed, AggInt]));
    }

    #[test]
    fn erased_field_is_not_inline_comparable() {
        let ty = Ty::Tuple(vec![Ty::Error, Ty::int()]);
        assert_eq!(inline_aggregate_fields(&ty, WIDE), None);
    }

    #[test]
    fn open_record_is_not_inline_comparable() {
        let ty = open_record(&[("a", Ty::int())]);
        assert_eq!(inline_aggregate_fields(&ty, WIDE), None);
    }

    #[test]
    fn empty_record_is_not_inline_comparable() {
        assert_eq!(inline_aggregate_fields(&closed_record(&[]), WIDE), None);
    }

    #[test]
    fn non_aggregate_is_not_inline_comparable() {
        assert_eq!(inline_aggregate_fields(&Ty::int(), WIDE), None);
        assert_eq!(inline_aggregate_fields(&Ty::list(Ty::int()), WIDE), None);
        assert_eq!(inline_aggregate_fields(&adt("Color"), WIDE), None);
    }

    #[test]
    fn width_over_cap_is_not_inline_comparable() {
        let three = Ty::Tuple(vec![Ty::int(); 3]);
        assert_eq!(inline_aggregate_fields(&three, 2), None, "3 fields > cap of 2");
        assert!(inline_aggregate_fields(&three, 3).is_some(), "3 fields within cap of 3");
    }
}

#[cfg(test)]
mod wire_projection_proptests {
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

    use super::{AGG_FIELD_CAP, drop_class, dup_class, inline_aggregate_fields};

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
            // The inline-aggregate comparison decision is re-derived from the
            // operand type, so the projection must preserve it: the worker (which
            // compiles from the wire form) must emit the same comparison code as the
            // warm path, or the object cache would be non-deterministic.
            prop_assert_eq!(
                inline_aggregate_fields(&ty, AGG_FIELD_CAP),
                inline_aggregate_fields(&round, AGG_FIELD_CAP),
                "inline-aggregate classification for {:?}",
                ty
            );
        }
    }
}

//! Translating Core IR to Cranelift IR.
//!
//! [`compile_def`] declares and defines, for one lowered definition, the code of
//! its entry and lifted functions, the static (immortal) closure that represents
//! it as a value, and the static string literals it uses. The same path drives
//! both back ends (AOT object emission and the in-process JIT) through the
//! [`Module`] trait.
//!
//! Every compiled function has the runtime calling convention
//! `fn(env: *const i64, args: *const i64) -> i64`: parameters are read from
//! `args`, captures from `env`. Values are uniform tagged 64-bit words; `Dup`
//! and `Drop` lower to runtime calls (no-ops on immediates).

use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{AbiParam, Block, FuncRef, InstBuilder, MemFlags, Value, types};
use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use fai_core::ir::{CExpr, CoreFn, ExprKind, FieldIndex, Lit, LoweredDef, Prim};
use fai_resolve::{DefId, LocalId};
use fai_runtime as rt;
use fai_types::{Con, RowEnd, Ty};
use rustc_hash::FxHashMap;

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
/// reference to a zero-arity binding (a value, not a function) is forced.
pub fn compile_def<M: Module>(
    module: &mut M,
    lowered: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
) {
    let mut jobs = Vec::new();
    build_def(module, lowered, namer, arity_of, &mut jobs);
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
    jobs: &mut Vec<(FuncId, Context)>,
) {
    let base = namer(lowered.def);
    let sig = code_signature(module);

    // Declare every function (entry exported, lifted lambdas local).
    let mut fn_ids = Vec::with_capacity(lowered.fns.len());
    for i in 0..lowered.fns.len() {
        let name = if i == 0 { base.clone() } else { format!("{base}__fn{i}") };
        let linkage = if i == 0 { Linkage::Export } else { Linkage::Local };
        fn_ids.push(module.declare_function(&name, linkage, &sig).expect("declare function"));
    }

    // The exported static closure representing the definition as a value.
    let closure_data = module
        .declare_data(&closure_symbol(namer, lowered.def), Linkage::Export, true, false)
        .expect("declare closure data");

    // Build each function body into its own (uncompiled) context.
    for (i, f) in lowered.fns.iter().enumerate() {
        let ctx = build_fn(module, f, lowered, namer, arity_of, &fn_ids, &base, i);
        jobs.push((fn_ids[i], ctx));
    }

    // The static closure (the first-class value form, reached via `apply_n`) must
    // use the all-owned ABI. When the entry borrows parameters, point the closure
    // at an owned-ABI wrapper that calls the borrowed entry and then releases the
    // borrowed arguments; direct callers call the (borrowed) entry symbol.
    let arity = lowered.entry().params.len() as u64;
    let closure_code = if lowered.borrows_any() {
        let wrapper = module
            .declare_function(&format!("{base}__owned"), Linkage::Local, &sig)
            .expect("declare wrapper");
        let ctx = build_owned_wrapper(module, fn_ids[0], &lowered.entry_borrowed);
        jobs.push((wrapper, ctx));
        wrapper
    } else {
        fn_ids[0]
    };
    define_static_closure(module, closure_data, closure_code, arity);
}

/// Builds the owned-ABI wrapper for a function whose entry borrows parameters: it
/// calls the borrowed entry with the same environment and arguments, then drops
/// the borrowed arguments (which the entry left untouched), and returns the
/// result. Returns the uncompiled context (the caller compiles and defines it).
fn build_owned_wrapper<M: Module>(module: &mut M, entry: FuncId, borrowed: &[bool]) -> Context {
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

        let entry_ref = module.declare_func_in_func(entry, builder.func);
        let call = builder.ins().call(entry_ref, &[env, args]);
        let result = builder.inst_results(call)[0];

        let mut drop_sig = module.make_signature();
        drop_sig.params.push(AbiParam::new(types::I64));
        let drop_id =
            module.declare_function("fai_drop", Linkage::Import, &drop_sig).expect("declare drop");
        let drop_ref = module.declare_func_in_func(drop_id, builder.func);
        for (i, &borrowed) in borrowed.iter().enumerate() {
            if borrowed {
                let offset = i32::try_from(i * 8).expect("arg offset");
                let v = builder.ins().load(types::I64, MemFlags::trusted(), args, offset);
                builder.ins().call(drop_ref, &[v]);
            }
        }
        builder.ins().return_(&[result]);
        builder.finalize();
    }
    ctx
}

/// The calling convention shared by every compiled function.
fn code_signature<M: Module>(module: &M) -> cranelift_codegen::ir::Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // env
    sig.params.push(AbiParam::new(types::I64)); // args
    sig.returns.push(AbiParam::new(types::I64));
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
    fn_ids: &[FuncId],
    base: &str,
    fn_index: usize,
) -> Context {
    let mut ctx = module.make_context();
    ctx.func.signature = code_signature(module);
    let mut fbcx = FunctionBuilderContext::new();

    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fbcx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let env = builder.block_params(entry)[0];
        let args = builder.block_params(entry)[1];

        let mut tr = Translator {
            module,
            builder,
            namer,
            arity_of,
            fn_ids,
            lowered,
            base,
            fn_index,
            vars: FxHashMap::default(),
            var_tys: FxHashMap::default(),
            runtime: FxHashMap::default(),
            string_counter: 0,
            loop_ctx: None,
            result_slot: None,
        };

        // Bind parameters (from `args`) and captures (from `env`).
        for (i, &p) in core_fn.params.iter().enumerate() {
            let v = tr.load_slot(args, i);
            tr.define_var(p, v);
        }
        for (i, &c) in core_fn.captures.iter().enumerate() {
            let v = tr.load_slot(env, i);
            tr.define_var(c, v);
        }

        let result = tr.expr(&core_fn.body);
        tr.builder.ins().return_(&[result]);
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
    fn_ids: &'a [FuncId],
    lowered: &'a LoweredDef,
    base: &'a str,
    fn_index: usize,
    vars: FxHashMap<usize, Variable>,
    /// A local's static type, where known (from `let` value types). Used to
    /// specialize reference-count operations — drops and dups of a
    /// statically-immediate value are runtime no-ops, so they are omitted.
    var_tys: FxHashMap<usize, Ty>,
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
        let var = self.builder.declare_var(types::I64);
        self.vars.insert(key, var);
        var
    }

    fn define_var(&mut self, local: LocalId, value: Value) {
        let var = self.var(local);
        self.builder.def_var(var, value);
    }

    fn use_var(&mut self, local: LocalId) -> Value {
        let var = self.var(local);
        self.builder.use_var(var)
    }

    /// Whether `local`'s known static type is always an immediate (so its values
    /// carry no reference count). Conservatively `false` when the type is unknown.
    fn is_immediate_local(&self, local: LocalId) -> bool {
        self.var_tys.get(&local.index()).is_some_and(is_immediate_ty)
    }

    /// If `local`'s known static type is a monomorphic, fixed-shape data cell —
    /// a closed record or a tuple — returns its fields' drop classes in heap
    /// layout order, so the drop can be inlined instead of dispatched through the
    /// runtime. `None` when the type is unknown (e.g. a parameter, not recorded in
    /// `var_tys`) or not a fixed shape. See [`fixed_shape_drop`].
    fn specialized_drop(&self, local: LocalId) -> Option<Vec<FieldDrop>> {
        fixed_shape_drop(self.var_tys.get(&local.index())?, MAX_INLINE_DROP_BOXED_FIELDS)
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

    /// Releases `local` at its last use: a no-op for a statically-immediate value
    /// (no reference count), an inlined release for a known monomorphic data cell
    /// (skipping the runtime descriptor classification), else a runtime drop.
    fn drop_local(&mut self, local: LocalId) {
        if self.is_immediate_local(local) {
            return;
        }
        if let Some(fields) = self.specialized_drop(local) {
            self.emit_inline_drop(local, &fields);
        } else {
            let v = self.use_var(local);
            self.call_drop(v);
        }
    }

    /// Inlines the release of a known monomorphic data cell (`local`, a boxed
    /// closed-record or tuple value with the given per-field drop classes),
    /// skipping the runtime's descriptor classification: decrement the reference
    /// count, and when it reaches zero release each boxed field at its constant
    /// offset (dropping immediate fields is a no-op, so they are omitted) and free
    /// the cell directly. Leaves the builder in the continuation block.
    ///
    /// Releasing each boxed child through `fai_drop` (rather than recursing the
    /// inlining) keeps deep structures iterative and the emitted code small. The
    /// cell is freed last: the heap is acyclic, so dropping a child can never
    /// reach the parent, and the field pointers are loaded before the free.
    fn emit_inline_drop(&mut self, local: LocalId, fields: &[FieldDrop]) {
        let cell = self.use_var(local);

        // Decrement the reference count in place.
        let rc_off = i32::try_from(rt::RC_OFFSET).expect("rc offset");
        let rc = self.builder.ins().load(types::I64, MemFlags::trusted(), cell, rc_off);
        let dec = self.builder.ins().iadd_imm(rc, -1);
        self.builder.ins().store(MemFlags::trusted(), dec, cell, rc_off);

        // Branch on whether the cell is now dead.
        let dead =
            self.builder.ins().icmp_imm(cranelift_codegen::ir::condcodes::IntCC::Equal, dec, 0);
        let free_b = self.builder.create_block();
        let cont_b = self.builder.create_block();
        self.builder.ins().brif(dead, free_b, &[], cont_b, &[]);

        // Dead: release the boxed fields, then reclaim the cell's memory.
        self.builder.switch_to_block(free_b);
        self.builder.seal_block(free_b);
        for (i, class) in fields.iter().enumerate() {
            if matches!(class, FieldDrop::Boxed) {
                let off = i32::try_from(rt::DATA_FIELDS_OFFSET + i * 8).expect("field offset");
                let field = self.builder.ins().load(types::I64, MemFlags::trusted(), cell, off);
                self.call_drop(field);
            }
        }
        let free = self.runtime("fai_free", 1, false);
        self.builder.ins().call(free, &[cell]);
        self.builder.ins().jump(cont_b, &[]);

        self.builder.switch_to_block(cont_b);
        self.builder.seal_block(cont_b);
    }

    fn expr(&mut self, e: &CExpr) -> Value {
        match &e.kind {
            ExprKind::Lit(lit) => self.literal(lit),
            ExprKind::Local(local) => self.use_var(*local),
            ExprKind::Global(def) => self.global_value(*def),
            ExprKind::Prim { op, args } => self.prim(*op, args),
            ExprKind::App { func, args } => self.application(func, args),
            ExprKind::If { cond, then, els } => self.conditional(cond, then, els),
            ExprKind::Let { local, value, body } => {
                let v = self.expr(value);
                self.var_tys.insert(local.index(), value.ty.clone());
                self.define_var(*local, v);
                self.expr(body)
            }
            ExprKind::MakeClosure { func, captures } => self.make_closure(*func, captures),
            ExprKind::MakeData { tag, args, reuse } => self.make_data(*tag, args, *reuse),
            ExprKind::DataTag(base) => {
                let v = self.expr(base);
                self.call1("fai_data_tag", v)
            }
            ExprKind::DataField { base, index } => self.data_field(base, *index),
            ExprKind::Reset { value, token, body } => {
                let v = self.expr(value);
                let tok = self.call1("fai_drop_reuse", v);
                self.define_var(*token, tok);
                self.expr(body)
            }
            ExprKind::Dup { local, body } => {
                // A statically-immediate value has no reference count, so a dup is
                // a no-op and is omitted.
                if !self.is_immediate_local(*local) {
                    let v = self.use_var(*local);
                    let _ = self.call1("fai_dup", v);
                }
                self.expr(body)
            }
            ExprKind::Drop { local, body } => {
                let result = self.expr(body);
                // The drop follows the body (its last use); see `drop_local` for
                // the immediate / inlined-cell / runtime-dispatch choice.
                self.drop_local(*local);
                result
            }
            ExprKind::Join { params, body } => self.join(params, body),
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
                let raw = self.builder.ins().iconst(types::I64, *bits as i64);
                self.call1("fai_box_float", raw)
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
        let vals: Vec<Value> = args.iter().map(|a| self.expr(a)).collect();
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
    fn data_field(&mut self, base: &CExpr, index: FieldIndex) -> Value {
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
    fn global_value(&mut self, def: DefId) -> Value {
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
            self.builder.inst_results(call)[0]
        } else {
            closure
        }
    }

    fn prim(&mut self, op: Prim, args: &[CExpr]) -> Value {
        // The hot integer/boolean primitives compile to inline machine code with
        // an immediate fast path; everything else — and the boxed/overflow cases
        // of those — falls through to the out-of-line runtime call below.
        if let Some(v) = self.inline_prim(op, args) {
            return v;
        }
        let vals: Vec<Value> = args.iter().map(|a| self.expr(a)).collect();
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

    /// Compiles an integer/boolean primitive to inline machine code when its
    /// operands are immediates, or returns `None` for the primitives that stay
    /// out-of-line runtime calls (division/remainder, the float operations,
    /// structural/string operations on boxed values, capabilities).
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

    fn application(&mut self, func: &CExpr, args: &[CExpr]) -> Value {
        // Direct call: a saturated application of a known top-level function calls
        // its code symbol directly, skipping `apply_n` and the static closure.
        // (Top-level functions capture nothing, so the environment is unused.)
        if let ExprKind::Global(def) = func.kind {
            let arity = (self.arity_of)(def);
            if arity == args.len() && arity > 0 {
                let vals: Vec<Value> = args.iter().map(|a| self.expr(a)).collect();
                let args_ptr = self.spill(&vals);
                return self.direct_call(def, args_ptr);
            }
        }
        let callee = self.expr(func);
        let vals: Vec<Value> = args.iter().map(|a| self.expr(a)).collect();
        let args_ptr = self.spill(&vals);
        let argc = self.builder.ins().iconst(types::I64, vals.len() as i64);
        let f = self.runtime("fai_apply_n", 3, true);
        let call = self.builder.ins().call(f, &[callee, argc, args_ptr]);
        self.builder.inst_results(call)[0]
    }

    /// Calls a top-level definition's code symbol directly with a null environment
    /// (it has no captures) and the spilled argument array.
    fn direct_call(&mut self, def: DefId, args_ptr: Value) -> Value {
        let name = code_symbol(self.namer, def);
        let sig = code_signature(self.module);
        let id = self.module.declare_function(&name, Linkage::Import, &sig).expect("declare code");
        let fref = self.module.declare_func_in_func(id, self.builder.func);
        let null_env = self.builder.ins().iconst(types::I64, 0);
        let call = self.builder.ins().call(fref, &[null_env, args_ptr]);
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
            env_vals.push(self.use_var(c));
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
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);

        self.builder.ins().brif(is_true, then_b, &[], else_b, &[]);

        self.builder.switch_to_block(then_b);
        self.builder.seal_block(then_b);
        let tv = self.expr(then);
        self.builder.ins().jump(merge_b, &[tv.into()]);

        self.builder.switch_to_block(else_b);
        self.builder.seal_block(else_b);
        let ev = self.expr(els);
        self.builder.ins().jump(merge_b, &[ev.into()]);

        self.builder.switch_to_block(merge_b);
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
    }

    /// Code-generates a tail-call loop: a header block the loop-carried locals flow
    /// into (carried as cranelift variables, so the header is sealed only after its
    /// `Recur` back-edges are emitted), an exit block carrying the loop's result,
    /// and the body translated in tail position.
    fn join(&mut self, params: &[LocalId], body: &CExpr) -> Value {
        let header = self.builder.create_block();
        let exit = self.builder.create_block();
        self.builder.append_block_param(exit, types::I64);

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
        self.builder.switch_to_block(exit);
        self.builder.seal_block(exit);
        self.builder.block_params(exit)[0]
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
                self.var_tys.insert(local.index(), value.ty.clone());
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
                if !self.is_immediate_local(*local) {
                    let v = self.use_var(*local);
                    let _ = self.call1("fai_dup", v);
                }
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
                let exit = self.loop_ctx.as_ref().expect("hole close inside a loop").exit;
                self.builder.ins().jump(exit, &[result.into()]);
            }
            // Any other tail expression is the loop's value (a plain tail-call
            // loop's base case): evaluate it and exit.
            _ => {
                let v = self.expr(e);
                let exit = self.loop_ctx.as_ref().expect("tail value inside a loop").exit;
                self.builder.ins().jump(exit, &[v.into()]);
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
        // Immediates are handled by `is_immediate_local`, not the cell classifier.
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

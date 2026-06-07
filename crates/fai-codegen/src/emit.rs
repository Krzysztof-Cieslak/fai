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

use cranelift_codegen::ir::{AbiParam, FuncRef, InstBuilder, MemFlags, Value, types};
use cranelift_codegen::ir::{StackSlotData, StackSlotKind};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{DataDescription, DataId, FuncId, Linkage, Module};
use fai_core::ir::{CExpr, CoreFn, ExprKind, FieldIndex, Lit, LoweredDef, Prim};
use fai_resolve::{DefId, LocalId};
use fai_runtime as rt;
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

    // Define each function body.
    for (i, f) in lowered.fns.iter().enumerate() {
        define_fn(module, fn_ids[i], f, lowered, namer, arity_of, &fn_ids, &base, i);
    }

    define_static_closure(module, closure_data, fn_ids[0], lowered.entry().params.len() as u64);
}

/// The calling convention shared by every compiled function.
fn code_signature<M: Module>(module: &M) -> cranelift_codegen::ir::Signature {
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); // env
    sig.params.push(AbiParam::new(types::I64)); // args
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

#[allow(clippy::too_many_arguments)]
fn define_fn<M: Module>(
    module: &mut M,
    func_id: FuncId,
    core_fn: &CoreFn,
    lowered: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    fn_ids: &[FuncId],
    base: &str,
    fn_index: usize,
) {
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
            runtime: FxHashMap::default(),
            string_counter: 0,
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

    module.define_function(func_id, &mut ctx).expect("define function");
    module.clear_context(&mut ctx);
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
    runtime: FxHashMap<&'static str, FuncRef>,
    string_counter: usize,
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
                let v = self.use_var(*local);
                let _ = self.call1("fai_dup", v);
                self.expr(body)
            }
            ExprKind::Drop { local, body } => {
                let result = self.expr(body);
                let v = self.use_var(*local);
                self.call_drop(v);
                result
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
        let vals: Vec<Value> = args.iter().map(|a| self.expr(a)).collect();
        // Every primitive (including `Console.writeLine`, which yields Unit)
        // returns a value.
        let f = self.runtime(op.runtime_symbol(), op.arity(), true);
        let call = self.builder.ins().call(f, &vals);
        self.builder.inst_results(call)[0]
    }

    fn application(&mut self, func: &CExpr, args: &[CExpr]) -> Value {
        let callee = self.expr(func);
        let vals: Vec<Value> = args.iter().map(|a| self.expr(a)).collect();
        let args_ptr = self.spill(&vals);
        let argc = self.builder.ins().iconst(types::I64, vals.len() as i64);
        let f = self.runtime("fai_apply_n", 3, true);
        let call = self.builder.ins().call(f, &[callee, argc, args_ptr]);
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
}

/// Whether `n` fits the 63-bit immediate range.
fn fits_immediate(n: i64) -> bool {
    ((n << 1) >> 1) == n
}

//! In-process JIT execution via `cranelift-jit`.
//!
//! [`jit_run`] compiles a set of lowered definitions into one JIT module,
//! resolves runtime symbols by address, then runs the entry definition's `main`
//! through the runtime and returns its exit code. The reachable set (including
//! prelude definitions) is computed by the driver.

use cranelift_codegen::Context;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module, ModuleReloc, default_libcall_names};
use fai_core::ir::{FnAbi, LoweredDef};
use fai_resolve::DefId;
use fai_runtime as rt;
use rayon::prelude::*;

use crate::emit::{Bce, build_def, closure_symbol};

/// Registers every runtime symbol the generated code may reference.
fn register_runtime(builder: &mut JITBuilder) {
    macro_rules! sym {
        ($name:literal, $ptr:expr) => {
            builder.symbol($name, $ptr as *const u8);
        };
    }
    sym!("fai_dup", rt::fai_dup);
    sym!("fai_drop", rt::fai_drop);
    sym!("fai_free", rt::fai_free);
    sym!("fai_drop_dead", rt::fai_drop_dead);
    sym!("fai_box_int", rt::fai_box_int);
    sym!("fai_int_add", rt::fai_int_add);
    sym!("fai_int_sub", rt::fai_int_sub);
    sym!("fai_int_mul", rt::fai_int_mul);
    sym!("fai_int_div", rt::fai_int_div);
    sym!("fai_int_rem", rt::fai_int_rem);
    sym!("fai_int_and", rt::fai_int_and);
    sym!("fai_int_or", rt::fai_int_or);
    sym!("fai_int_xor", rt::fai_int_xor);
    sym!("fai_int_shl", rt::fai_int_shl);
    sym!("fai_int_shr", rt::fai_int_shr);
    sym!("fai_int_shr_logical", rt::fai_int_shr_logical);
    sym!("fai_int_complement", rt::fai_int_complement);
    sym!("fai_int_lt", rt::fai_int_lt);
    sym!("fai_int_le", rt::fai_int_le);
    sym!("fai_int_gt", rt::fai_int_gt);
    sym!("fai_int_ge", rt::fai_int_ge);
    sym!("fai_box_float", rt::fai_box_float);
    sym!("fai_float_add", rt::fai_float_add);
    sym!("fai_float_sub", rt::fai_float_sub);
    sym!("fai_float_mul", rt::fai_float_mul);
    sym!("fai_float_div", rt::fai_float_div);
    sym!("fai_float_lt", rt::fai_float_lt);
    sym!("fai_float_le", rt::fai_float_le);
    sym!("fai_float_gt", rt::fai_float_gt);
    sym!("fai_float_ge", rt::fai_float_ge);
    sym!("fai_sqrt", rt::fai_sqrt);
    sym!("fai_float_from_bits", rt::fai_float_from_bits);
    sym!("fai_float_to_bits", rt::fai_float_to_bits);
    sym!("fai_int_to_float", rt::fai_int_to_float);
    sym!("fai_float_to_int", rt::fai_float_to_int);
    sym!("fai_float_to_string", rt::fai_float_to_string);
    sym!("fai_float_compare_bits", rt::fai_float_compare_bits);
    sym!("fai_char_to_string", rt::fai_char_to_string);
    sym!("fai_char_to_code", rt::fai_char_to_code);
    sym!("fai_char_from_code", rt::fai_char_from_code);
    sym!("fai_is_valid_char_code", rt::fai_is_valid_char_code);
    sym!("fai_compare", rt::fai_compare);
    sym!("fai_compare_borrowed", rt::fai_compare_borrowed);
    sym!("fai_hash", rt::fai_hash);
    sym!("fai_hash_borrowed", rt::fai_hash_borrowed);
    sym!("fai_equal", rt::fai_equal);
    sym!("fai_equal_borrowed", rt::fai_equal_borrowed);
    sym!("fai_make_data", rt::fai_make_data);
    sym!("fai_make_data_scalar", rt::fai_make_data_scalar);
    sym!("fai_niche_a_to_std", rt::fai_niche_a_to_std);
    sym!("fai_std_to_niche_a", rt::fai_std_to_niche_a);
    sym!("fai_niche_b_to_std", rt::fai_niche_b_to_std);
    sym!("fai_std_to_niche_b", rt::fai_std_to_niche_b);
    sym!("fai_drop_reuse", rt::fai_drop_reuse);
    sym!("fai_reuse", rt::fai_reuse);
    sym!("fai_reuse_scalar", rt::fai_reuse_scalar);
    sym!("fai_free_reuse", rt::fai_free_reuse);
    sym!("fai_data_tag", rt::fai_data_tag);
    sym!("fai_data_field", rt::fai_data_field);
    sym!("fai_string_concat", rt::fai_string_concat);
    sym!("fai_int_to_string", rt::fai_int_to_string);
    sym!("fai_string_length", rt::fai_string_length);
    sym!("fai_string_length_borrowed", rt::fai_string_length_borrowed);
    sym!("fai_string_contains_borrowed", rt::fai_string_contains_borrowed);
    sym!("fai_to_upper", rt::fai_to_upper);
    sym!("fai_to_upper_borrowed", rt::fai_to_upper_borrowed);
    sym!("fai_to_lower", rt::fai_to_lower);
    sym!("fai_to_lower_borrowed", rt::fai_to_lower_borrowed);
    sym!("fai_trim", rt::fai_trim);
    sym!("fai_trim_borrowed", rt::fai_trim_borrowed);
    sym!("fai_string_contains", rt::fai_string_contains);
    sym!("fai_string_split", rt::fai_string_split);
    sym!("fai_string_split_borrowed", rt::fai_string_split_borrowed);
    sym!("fai_string_join", rt::fai_string_join);
    sym!("fai_string_join_borrowed", rt::fai_string_join_borrowed);
    sym!("fai_array_split", rt::fai_array_split);
    sym!("fai_array_split_borrowed", rt::fai_array_split_borrowed);
    sym!("fai_array_join", rt::fai_array_join);
    sym!("fai_array_join_borrowed", rt::fai_array_join_borrowed);
    sym!("fai_string_substring", rt::fai_string_substring);
    sym!("fai_string_take", rt::fai_string_take);
    sym!("fai_string_drop", rt::fai_string_drop);
    sym!("fai_not", rt::fai_not);
    sym!("fai_console_write_line", rt::fai_console_write_line);
    sym!("fai_clock_now", rt::fai_clock_now);
    sym!("fai_random_next_int", rt::fai_random_next_int);
    sym!("fai_file_read", rt::fai_file_read);
    sym!("fai_file_write", rt::fai_file_write);
    sym!("fai_env_get", rt::fai_env_get);
    sym!("fai_env_args", rt::fai_env_args);
    sym!("fai_record_update", rt::fai_record_update);
    sym!("fai_array_with_capacity", rt::fai_array_with_capacity);
    sym!("fai_alloc_array", rt::fai_alloc_array);
    sym!("fai_pool_heads", rt::fai_pool_heads);
    sym!("fai_note_alloc", rt::fai_note_alloc);
    sym!("fai_note_free", rt::fai_note_free);
    sym!("fai_array_length", rt::fai_array_length);
    sym!("fai_array_length_borrowed", rt::fai_array_length_borrowed);
    sym!("fai_array_get", rt::fai_array_get);
    sym!("fai_array_get_borrowed", rt::fai_array_get_borrowed);
    sym!("fai_array_set", rt::fai_array_set);
    sym!("fai_array_push", rt::fai_array_push);
    sym!("fai_array_index_panic", rt::fai_array_index_panic);
    sym!("fai_bce_unsound_panic", rt::fai_bce_unsound_panic);
    sym!("fai_apply_n", rt::fai_apply_n);
    sym!("fai_make_closure", rt::fai_make_closure);
    sym!("fai_run_main", rt::fai_run_main);
    builder.symbol("FAI_NONE_VALUE", (&raw const rt::FAI_NONE_VALUE).cast());
    builder.symbol("FAI_STRING_DESC", (&raw const rt::FAI_STRING_DESC).cast());
    builder.symbol("FAI_INT_DESC", (&raw const rt::FAI_INT_DESC).cast());
    builder.symbol("FAI_CLOSURE_DESC", (&raw const rt::FAI_CLOSURE_DESC).cast());
    builder.symbol("FAI_STACK_CLOSURE_DESC", (&raw const rt::FAI_STACK_CLOSURE_DESC).cast());
    builder.symbol("FAI_PAP_DESC", (&raw const rt::FAI_PAP_DESC).cast());
    builder.symbol("FAI_STACK_PAP_DESC", (&raw const rt::FAI_STACK_PAP_DESC).cast());
    builder.symbol("FAI_FLOAT_DESC", (&raw const rt::FAI_FLOAT_DESC).cast());
    builder.symbol("FAI_DATA_DESC", (&raw const rt::FAI_DATA_DESC).cast());
    builder.symbol("FAI_ARRAY_DESC", (&raw const rt::FAI_ARRAY_DESC).cast());
    builder.symbol("FAI_FLOAT_ARRAY_DESC", (&raw const rt::FAI_FLOAT_ARRAY_DESC).cast());
}

fn jit_module() -> JITModule {
    let mut flags = settings::builder();
    flags.set("use_colocated_libcalls", "false").expect("flag");
    flags.set("is_pic", "false").expect("flag");
    // Optimize generated code (inlining, better register allocation, redundant
    // load/store elimination) at a modest compile-time cost. Cranelift's
    // optimizations are value-preserving, so this stays correctness-neutral.
    flags.set("opt_level", "speed").expect("flag");
    let isa_builder = cranelift_native::builder().expect("host machine is supported");
    let isa = isa_builder.finish(settings::Flags::new(flags)).expect("isa");
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
    register_runtime(&mut builder);
    JITModule::new(builder)
}

/// Compiles every definition into `module`: builds each function's IR serially
/// (it mutates the module — declaring callees, runtime imports, and string data),
/// then code-generates the function bodies **in parallel** (each
/// `Context::compile` — the expensive legalize/register-allocate/encode step —
/// needs only the shared, read-only ISA), and finally registers the machine code
/// serially. This is the split `Module::define_function` performs internally,
/// with the costly middle step spread across a rayon pool.
fn compile_module(
    module: &mut JITModule,
    defs: &[LoweredDef],
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    signature_of: &dyn Fn(DefId) -> FnAbi,
    borrows_of: &dyn Fn(DefId) -> Vec<bool>,
    bce: &Bce,
) {
    let mut jobs: Vec<(FuncId, Context)> = Vec::new();
    for def in defs {
        build_def(module, def, namer, arity_of, signature_of, borrows_of, bce, &mut jobs);
        // A token-taking specialized entry, when the definition carries one (the
        // driver clears it on definitions no reachable caller forwards to, so this
        // emits a reuse entry only where it is actually used).
        crate::emit::build_reuse_object(
            module,
            def,
            namer,
            arity_of,
            signature_of,
            borrows_of,
            bce,
            &mut jobs,
        );
    }

    // Code-generate each function in parallel; only the read-only ISA is shared.
    {
        let isa = module.isa();
        jobs.par_iter_mut().for_each(|(_, ctx)| {
            ctx.compile(isa, &mut ControlPlane::default()).expect("compile function");
        });
    }

    // Register the compiled machine code into the module (serial — mutates it).
    for (id, ctx) in &jobs {
        let compiled = ctx.compiled_code().expect("function was compiled");
        let alignment = compiled.buffer.alignment as u64;
        let relocs: Vec<ModuleReloc> = compiled
            .buffer
            .relocs()
            .iter()
            .map(|reloc| ModuleReloc::from_mach_reloc(reloc, &ctx.func, *id))
            .collect();
        module
            .define_function_bytes(*id, alignment, compiled.code_buffer(), &relocs)
            .expect("define function bytes");
    }
}

/// A compiled, finalized JIT image kept alive so its code can be called by
/// address. Used to run contracts: build once from the reachable definitions,
/// then fetch each contract's static-closure pointer and apply it via the
/// runtime's `fai_apply_n`.
pub struct JitProgram {
    module: JITModule,
}

impl JitProgram {
    /// Compiles and finalizes `defs` into one JIT image.
    #[must_use]
    pub fn compile(
        defs: &[LoweredDef],
        namer: &dyn Fn(DefId) -> String,
        arity_of: &dyn Fn(DefId) -> usize,
        signature_of: &dyn Fn(DefId) -> FnAbi,
        borrows_of: &dyn Fn(DefId) -> Vec<bool>,
        bce: &Bce,
    ) -> JitProgram {
        let mut module = jit_module();
        compile_module(&mut module, defs, namer, arity_of, signature_of, borrows_of, bce);
        module.finalize_definitions().expect("finalize");
        JitProgram { module }
    }

    /// The address (as an `i64` Fai value) of `def`'s static closure — the value
    /// form through which a definition is applied via `fai_apply_n`.
    #[must_use]
    pub fn closure_value(&mut self, namer: &dyn Fn(DefId) -> String, def: DefId) -> i64 {
        let id = self
            .module
            .declare_data(&closure_symbol(namer, def), Linkage::Export, true, false)
            .expect("closure data");
        let (ptr, _size) = self.module.get_finalized_data(id);
        ptr as i64
    }
}

/// Compiles `defs` and runs the entry definition's `main` against the standard
/// library's `Runtime` value binding, returning its exit code (0 on success, 70
/// on a detected leak).
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn jit_run(
    defs: &[LoweredDef],
    entry: DefId,
    runtime: DefId,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    signature_of: &dyn Fn(DefId) -> FnAbi,
    borrows_of: &dyn Fn(DefId) -> Vec<bool>,
    bce: &Bce,
) -> i32 {
    let mut module = jit_module();
    compile_module(&mut module, defs, namer, arity_of, signature_of, borrows_of, bce);
    module.finalize_definitions().expect("finalize");

    let entry_id = module
        .declare_data(&closure_symbol(namer, entry), Linkage::Export, true, false)
        .expect("entry closure");
    let (entry_ptr, _size) = module.get_finalized_data(entry_id);
    let entry_value = entry_ptr as i64;

    let runtime_id = module
        .declare_data(&closure_symbol(namer, runtime), Linkage::Export, true, false)
        .expect("runtime closure");
    let (runtime_ptr, _size) = module.get_finalized_data(runtime_id);
    let runtime_value = runtime_ptr as i64;

    let code = rt::run_entry(entry_value, runtime_value);
    // Keep the JIT image alive until execution completes, then release it.
    drop(module);
    code
}

//! In-process JIT execution via `cranelift-jit`.
//!
//! [`jit_run`] compiles a set of lowered definitions into one JIT module,
//! resolves runtime symbols by address, then runs the entry definition's `main`
//! through the runtime and returns its exit code. The reachable set (including
//! prelude definitions) is computed by the driver.

use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, default_libcall_names};
use fai_core::ir::LoweredDef;
use fai_resolve::DefId;
use fai_runtime as rt;

use crate::emit::{closure_symbol, compile_def};

/// Registers every runtime symbol the generated code may reference.
fn register_runtime(builder: &mut JITBuilder) {
    macro_rules! sym {
        ($name:literal, $ptr:expr) => {
            builder.symbol($name, $ptr as *const u8);
        };
    }
    sym!("fai_dup", rt::fai_dup);
    sym!("fai_drop", rt::fai_drop);
    sym!("fai_box_int", rt::fai_box_int);
    sym!("fai_int_add", rt::fai_int_add);
    sym!("fai_int_sub", rt::fai_int_sub);
    sym!("fai_int_mul", rt::fai_int_mul);
    sym!("fai_int_div", rt::fai_int_div);
    sym!("fai_int_rem", rt::fai_int_rem);
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
    sym!("fai_int_to_float", rt::fai_int_to_float);
    sym!("fai_float_to_int", rt::fai_float_to_int);
    sym!("fai_float_to_string", rt::fai_float_to_string);
    sym!("fai_compare", rt::fai_compare);
    sym!("fai_equal", rt::fai_equal);
    sym!("fai_make_data", rt::fai_make_data);
    sym!("fai_data_tag", rt::fai_data_tag);
    sym!("fai_data_field", rt::fai_data_field);
    sym!("fai_string_concat", rt::fai_string_concat);
    sym!("fai_int_to_string", rt::fai_int_to_string);
    sym!("fai_string_length", rt::fai_string_length);
    sym!("fai_to_upper", rt::fai_to_upper);
    sym!("fai_to_lower", rt::fai_to_lower);
    sym!("fai_trim", rt::fai_trim);
    sym!("fai_string_contains", rt::fai_string_contains);
    sym!("fai_string_split", rt::fai_string_split);
    sym!("fai_string_join", rt::fai_string_join);
    sym!("fai_not", rt::fai_not);
    sym!("fai_console_write_line", rt::fai_console_write_line);
    sym!("fai_clock_now", rt::fai_clock_now);
    sym!("fai_random_next_int", rt::fai_random_next_int);
    sym!("fai_file_read", rt::fai_file_read);
    sym!("fai_file_write", rt::fai_file_write);
    sym!("fai_env_get", rt::fai_env_get);
    sym!("fai_env_args", rt::fai_env_args);
    sym!("fai_record_update", rt::fai_record_update);
    sym!("fai_apply_n", rt::fai_apply_n);
    sym!("fai_make_closure", rt::fai_make_closure);
    sym!("fai_run_main", rt::fai_run_main);
    builder.symbol("FAI_STRING_DESC", (&raw const rt::FAI_STRING_DESC).cast());
    builder.symbol("FAI_INT_DESC", (&raw const rt::FAI_INT_DESC).cast());
    builder.symbol("FAI_CLOSURE_DESC", (&raw const rt::FAI_CLOSURE_DESC).cast());
    builder.symbol("FAI_PAP_DESC", (&raw const rt::FAI_PAP_DESC).cast());
    builder.symbol("FAI_FLOAT_DESC", (&raw const rt::FAI_FLOAT_DESC).cast());
    builder.symbol("FAI_DATA_DESC", (&raw const rt::FAI_DATA_DESC).cast());
}

fn jit_module() -> JITModule {
    let mut flags = settings::builder();
    flags.set("use_colocated_libcalls", "false").expect("flag");
    flags.set("is_pic", "false").expect("flag");
    let isa_builder = cranelift_native::builder().expect("host machine is supported");
    let isa = isa_builder.finish(settings::Flags::new(flags)).expect("isa");
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());
    register_runtime(&mut builder);
    JITModule::new(builder)
}

/// Compiles `defs` and runs the entry definition's `main` against the standard
/// library's `Runtime` value binding, returning its exit code (0 on success, 70
/// on a detected leak).
#[must_use]
pub fn jit_run(
    defs: &[LoweredDef],
    entry: DefId,
    runtime: DefId,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
) -> i32 {
    let mut module = jit_module();
    for def in defs {
        compile_def(&mut module, def, namer, arity_of);
    }
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

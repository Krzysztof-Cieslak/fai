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
    sym!("fai_equal", rt::fai_equal);
    sym!("fai_string_concat", rt::fai_string_concat);
    sym!("fai_int_to_string", rt::fai_int_to_string);
    sym!("fai_not", rt::fai_not);
    sym!("fai_console_write_line", rt::fai_console_write_line);
    sym!("fai_apply_n", rt::fai_apply_n);
    sym!("fai_make_closure", rt::fai_make_closure);
    sym!("fai_run_main", rt::fai_run_main);
    builder.symbol("FAI_STRING_DESC", (&raw const rt::FAI_STRING_DESC).cast());
    builder.symbol("FAI_INT_DESC", (&raw const rt::FAI_INT_DESC).cast());
    builder.symbol("FAI_CLOSURE_DESC", (&raw const rt::FAI_CLOSURE_DESC).cast());
    builder.symbol("FAI_PAP_DESC", (&raw const rt::FAI_PAP_DESC).cast());
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

/// Compiles `defs` and runs the entry definition's `main`, returning its exit
/// code (0 on success, 70 on a detected leak).
#[must_use]
pub fn jit_run(
    defs: &[LoweredDef],
    entry: DefId,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
) -> i32 {
    let mut module = jit_module();
    for def in defs {
        compile_def(&mut module, def, namer, arity_of);
    }
    module.finalize_definitions().expect("finalize");

    let closure_id = module
        .declare_data(&closure_symbol(namer, entry), Linkage::Export, true, false)
        .expect("entry closure");
    let (ptr, _size) = module.get_finalized_data(closure_id);
    let entry_value = ptr as i64;

    let code = rt::run_entry(entry_value);
    // Keep the JIT image alive until execution completes, then release it.
    drop(module);
    code
}

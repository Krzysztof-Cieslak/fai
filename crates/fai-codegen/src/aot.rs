//! Ahead-of-time object emission via `cranelift-object`.
//!
//! [`object_for_def`] compiles one definition to a relocatable object (cached by
//! the driver); [`main_object`] emits the C `main` trampoline that hands the
//! entry closure to the runtime. The driver links these objects with the runtime
//! archive.

use std::sync::Arc;

use cranelift_codegen::ir::{AbiParam, InstBuilder, types};
use cranelift_codegen::isa::{self, TargetIsa};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{Linkage, Module, default_libcall_names};
use cranelift_object::{ObjectBuilder, ObjectModule};
use fai_core::ir::{FnAbi, LoweredDef};
use fai_resolve::DefId;

use crate::emit::{closure_symbol, compile_def};

/// Builds a position-independent ISA for the host target's object files.
///
/// Objects are emitted for the host triple via `isa::lookup` (a baseline host
/// ISA; the JIT keeps native CPU features). On macOS the detected host OS is
/// `Darwin`, which `cranelift-object` records in Mach-O objects as
/// `PLATFORM_UNKNOWN` — the system linker then refuses them ("unknown
/// platform"). Normalizing it to `MacOSX` makes the objects declare the macOS
/// platform and a minimum version. On other hosts the triple is used as-is.
fn host_isa() -> Arc<dyn TargetIsa> {
    let mut flags = settings::builder();
    flags.set("use_colocated_libcalls", "false").expect("flag");
    flags.set("is_pic", "true").expect("flag");
    // Optimize generated code (inlining, better register allocation, redundant
    // load/store elimination) at a modest compile-time cost. Cranelift's
    // optimizations are value-preserving, so this stays correctness-neutral.
    flags.set("opt_level", "speed").expect("flag");

    let mut triple = target_lexicon::Triple::host();
    if let target_lexicon::OperatingSystem::Darwin(version) = triple.operating_system {
        let version =
            version.or(Some(target_lexicon::DeploymentTarget { major: 11, minor: 0, patch: 0 }));
        triple.operating_system = target_lexicon::OperatingSystem::MacOSX(version);
    }

    let isa_builder = isa::lookup(triple).expect("host target is supported");
    isa_builder.finish(settings::Flags::new(flags)).expect("isa")
}

fn object_module(name: &str) -> ObjectModule {
    let builder =
        ObjectBuilder::new(host_isa(), name, default_libcall_names()).expect("object builder");
    ObjectModule::new(builder)
}

/// Compiles one lowered definition to a relocatable object file.
#[must_use]
pub fn object_for_def(
    lowered: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    signature_of: &dyn Fn(DefId) -> FnAbi,
) -> Vec<u8> {
    let mut module = object_module("fai");
    compile_def(&mut module, lowered, namer, arity_of, signature_of);
    module.finish().emit().expect("emit object")
}

/// Builds (without compiling) each function of `lowered` and returns its Cranelift
/// IR as text, entry first. Used by tests that inspect the emitted code shape
/// (e.g. that a known data cell's drop is inlined rather than dispatched). The IR
/// is read before `Context::compile` legalizes it, so high-level opcodes survive.
#[cfg(test)]
pub(crate) fn function_ir_text(
    lowered: &LoweredDef,
    namer: &dyn Fn(DefId) -> String,
    arity_of: &dyn Fn(DefId) -> usize,
    signature_of: &dyn Fn(DefId) -> FnAbi,
) -> Vec<String> {
    let mut module = object_module("fai_ir_test");
    let mut jobs = Vec::new();
    crate::emit::build_def(&mut module, lowered, namer, arity_of, signature_of, &mut jobs);
    jobs.iter().map(|(_, ctx)| ctx.func.display().to_string()).collect()
}

/// Emits the program's C `main`: it hands the entry definition's static closure
/// and the standard library's `Runtime` value binding to `fai_run_main`,
/// returning its exit code.
#[must_use]
pub fn main_object(entry: DefId, runtime: DefId, namer: &dyn Fn(DefId) -> String) -> Vec<u8> {
    let mut module = object_module("fai_main");

    let mut sig = module.make_signature();
    sig.returns.push(AbiParam::new(types::I32));
    let main_id = module.declare_function("main", Linkage::Export, &sig).expect("declare main");

    let mut run_sig = module.make_signature();
    run_sig.params.push(AbiParam::new(types::I64));
    run_sig.params.push(AbiParam::new(types::I64));
    run_sig.returns.push(AbiParam::new(types::I32));
    let run_id =
        module.declare_function("fai_run_main", Linkage::Import, &run_sig).expect("declare run");

    let entry_id = module
        .declare_data(&closure_symbol(namer, entry), Linkage::Import, true, false)
        .expect("declare entry closure");
    let runtime_id = module
        .declare_data(&closure_symbol(namer, runtime), Linkage::Import, true, false)
        .expect("declare runtime closure");

    let mut ctx = module.make_context();
    ctx.func.signature = sig;
    let mut fbcx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fbcx);
        let block = builder.create_block();
        builder.switch_to_block(block);
        builder.seal_block(block);
        let entry_gv = module.declare_data_in_func(entry_id, builder.func);
        let entry_value = builder.ins().symbol_value(types::I64, entry_gv);
        let runtime_gv = module.declare_data_in_func(runtime_id, builder.func);
        let runtime_value = builder.ins().symbol_value(types::I64, runtime_gv);
        let run = module.declare_func_in_func(run_id, builder.func);
        let call = builder.ins().call(run, &[entry_value, runtime_value]);
        let code = builder.inst_results(call)[0];
        builder.ins().return_(&[code]);
        builder.finalize();
    }
    module.define_function(main_id, &mut ctx).expect("define main");
    module.finish().emit().expect("emit main object")
}

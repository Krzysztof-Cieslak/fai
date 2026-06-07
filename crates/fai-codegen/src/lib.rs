// fai-codegen drives Cranelift and the JIT, which require hand-written unsafe
// (transmuting code pointers, taking symbol addresses). Every such block carries
// a `// SAFETY:` note; the rest is safe.
#![allow(unsafe_code)]

//! Code generation: Core IR to Cranelift IR, with two back ends.
//!
//! One path ([`emit`]) translates a lowered, reference-counted definition to
//! Cranelift IR; [`aot`] emits relocatable objects (cached per definition and
//! linked by the driver) and the C `main` trampoline, while [`jit`] compiles a
//! reachable set in memory and runs it. Symbol names for definitions are
//! supplied by a namer closure (the driver builds it from module names), so this
//! crate stays decoupled from the query database.

mod aot;
mod emit;
mod jit;

pub use aot::{main_object, object_for_def};
pub use emit::{closure_symbol, code_symbol};
pub use jit::{JitProgram, jit_run};

#[cfg(test)]
mod proptests;
#[cfg(test)]
mod reuse_tests;
#[cfg(test)]
mod tests;

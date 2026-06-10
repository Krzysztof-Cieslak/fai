//! Coverage-guided fuzzing of the size-class recycling allocator.
//!
//! Each input is interpreted by `fai_runtime::run_ops` as a sequence of
//! allocate/free operations over sizes that span the pooled classes and the large
//! fallback; the harness checks the allocator's invariants (no aliasing, payload
//! integrity, alignment) after every step, so libFuzzer's mutated inputs plus
//! AddressSanitizer hunt for memory-safety bugs the in-tree property tests might
//! miss.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    fai_runtime::run_ops(data);
});

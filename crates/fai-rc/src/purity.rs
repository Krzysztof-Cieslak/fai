//! Shared Core reorder-safety analysis used by tail-call transformations.

#[cfg(test)]
pub(crate) use fai_core::purity::is_pure_total;
pub(crate) use fai_core::purity::{application_pure_total, op_unsafe_to_reorder};

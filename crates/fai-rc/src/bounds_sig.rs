//! Interprocedural bounds-check-elimination facts: a definition's **entry facts**
//! (difference constraints over its parameters that hold on entry, established by
//! its in-file callers) and **result facts** (its result's length/bounds relative
//! to its parameters, consulted by a caller).
//!
//! Entry facts are *caller-directed*: a private definition's entire caller set is
//! in its own file (cross-file references can only name `public` members), so the
//! facts are a **file-local** fixpoint — the meet over every in-file call site of
//! the facts provable for the arguments. A `public` or first-class-used definition
//! gets no entry facts (its callers are unknown). This keeps `object_code` a pure
//! per-definition unit: a definition's facts depend only on its own module (plus
//! callees' result facts, a callee-directed signature with early cutoff), so the
//! cross-module codegen firewall holds.
//!
//! Result facts are *callee-directed* (like borrow/reuse signatures): a salsa
//! cycle resolved by a monotone fixpoint over the call graph.

use std::sync::Arc;

use fai_core::bounds::{BoundSig, ResultSig};
use fai_core::fuse_def;
use fai_db::{Db, SourceFile};
use fai_syntax::Symbol;

/// A definition's entry-fact signature (constraints over its parameters that hold
/// on entry). Empty for a public or first-class-used definition.
#[salsa::tracked]
pub fn entry_bounds(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<BoundSig> {
    // The body codegen sees (post-inline, post-fusion). The fixpoint below is
    // implemented in a follow-up step; an empty signature is always sound (it only
    // forgoes the interprocedural elisions, leaving the inline checks in place).
    let _ = fuse_def(db, file, name);
    Arc::new(BoundSig::default())
}

/// A definition's result-fact signature (its result's length/bounds relative to
/// its parameters).
#[salsa::tracked]
pub fn result_facts(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<ResultSig> {
    let _ = fuse_def(db, file, name);
    Arc::new(ResultSig::default())
}

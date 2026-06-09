//! Workspace corpora for benchmarks and performance guards.
//!
//! Two kinds of corpus live here so that the wall-clock benches, the
//! deterministic guards, and the language-server benches all agree on the inputs
//! they measure:
//!
//! * [`synthetic`] — a parameterized, byte-deterministic generator (the
//!   `CorpusSpec` knobs), re-exported at the crate root.
//! * [`realworld`] — a fixed, hand-written multi-module application (under
//!   `samples/`) used to benchmark language-server operations on realistic code,
//!   with stable probe positions that link back to their source line.

mod synthetic;

pub mod realworld;

pub use synthetic::{
    CorpusSpec, build_db, edit_comment, edit_core_signature, edit_private_body, edit_public_body,
    generate,
};

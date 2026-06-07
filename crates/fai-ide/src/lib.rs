//! Code intelligence for Fai: the engine behind `fai query` and (later) the LSP.
//!
//! This crate turns the resolution and type queries into the read-only,
//! JSON-shaped answers specified in `docs/CLI.md` §8 (`symbols`, `def`, `refs`,
//! `type`, `docs`, `outline`, `api`, `dependents`). It produces no diagnostics of
//! its own, so it owns no `FAInnnn` codes.
//!
//! Skeleton: the query implementations land incrementally across M2.

pub mod query;
pub mod repr;
pub mod target;

pub use query::{
    ApiResult, CallEdge, CallHierarchyResult, CapsResult, DefResult, DependentsResult, DocsResult,
    ListOpts, OutlineResult, RefsResult, SearchHit, SearchResult, SymbolsResult, TypeResult, api,
    callees, callers, caps, def, dependents, docs, outline, refs, search, symbols, type_at,
};
pub use target::{ResolvedTarget, resolve_target};

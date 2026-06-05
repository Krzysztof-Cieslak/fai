//! The `fai query` command: dispatch to the `fai-ide` code-intelligence engine.
//!
//! The driver owns a thin [`QueryRequest`] so clients (the CLI, later the daemon)
//! map their own argument types onto it without `fai-ide` depending on clap. Each
//! command produces a typed `fai-ide` result, serialized here into a
//! [`QueryResult`] (JSON plus a human rendering).

use fai_ide::ListOpts;
use serde::{Deserialize, Serialize};

use crate::Session;

/// A read-only code-intelligence request (CLI.md §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueryRequest {
    /// List/search symbols (optionally within a module).
    Symbols { module: Option<String>, limit: Option<usize> },
    /// Resolve a target to its definition site(s).
    Def { target: String },
    /// Find all references to a target.
    Refs { target: String, limit: Option<usize> },
    /// The type at a target.
    Type { target: String },
    /// Docs and contracts for a target.
    Docs { target: String },
    /// The outline of a file/module.
    Outline { target: String },
    /// A module's public interface.
    Api { module: String },
    /// Reverse dependencies of a target.
    Dependents { target: String, limit: Option<usize> },
    /// A command that is recognized but not implemented in M2.
    Unsupported { name: String },
}

/// The outcome of a query: its JSON body and whether it succeeded.
#[derive(Debug, Clone)]
pub struct QueryResult {
    /// Pretty-printed JSON body (the stable wire output).
    pub json: String,
    /// A human-readable rendering.
    pub human: String,
    /// Whether the query produced a result (a missing target is `false`).
    pub ok: bool,
}

impl QueryResult {
    fn from_serializable<T: Serialize>(value: &T, ok: bool) -> Self {
        let json = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned());
        // The human rendering is the JSON for now (agents consume JSON); a richer
        // textual form can be added without changing the wire schema.
        QueryResult { human: json.clone(), json, ok }
    }
}

/// Runs a query against `session`.
#[must_use]
pub fn run_query(session: &Session, request: &QueryRequest) -> QueryResult {
    let db = session.db();
    let files = session.select_files(None);
    let resolver = session.resolver();

    match request {
        QueryRequest::Symbols { module, limit } => {
            let r = fai_ide::symbols(
                db,
                &files,
                module.as_deref(),
                &resolver,
                ListOpts { limit: *limit },
            );
            QueryResult::from_serializable(&r, true)
        }
        QueryRequest::Def { target } => {
            let r = fai_ide::def(db, target, &resolver);
            let ok = r.target.is_some();
            QueryResult::from_serializable(&r, ok)
        }
        QueryRequest::Refs { target, limit } => {
            let r = fai_ide::refs(db, &files, target, &resolver, ListOpts { limit: *limit });
            let ok = r.target.is_some();
            QueryResult::from_serializable(&r, ok)
        }
        QueryRequest::Type { target } => {
            let r = fai_ide::type_at(db, target, &resolver);
            let ok = r.target.is_some();
            QueryResult::from_serializable(&r, ok)
        }
        QueryRequest::Docs { target } => {
            let r = fai_ide::docs(db, target, &resolver);
            let ok = r.target.is_some();
            QueryResult::from_serializable(&r, ok)
        }
        QueryRequest::Outline { target } => {
            let r = fai_ide::outline(db, target, &files, &resolver);
            QueryResult::from_serializable(&r, true)
        }
        QueryRequest::Api { module } => {
            let r = fai_ide::api(db, module, &files, &resolver);
            QueryResult::from_serializable(&r, true)
        }
        QueryRequest::Dependents { target, limit } => {
            let r = fai_ide::dependents(db, &files, target, &resolver, ListOpts { limit: *limit });
            let ok = r.target.is_some();
            QueryResult::from_serializable(&r, ok)
        }
        QueryRequest::Unsupported { name } => {
            let body = serde_json::json!({
                "schemaVersion": fai_diagnostics::SCHEMA_VERSION,
                "error": format!("`fai query {name}` is not implemented yet"),
            });
            QueryResult::from_serializable(&body, false)
        }
    }
}

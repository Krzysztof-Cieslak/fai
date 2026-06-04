// salsa's derive macros emit `unsafe impl`s (e.g. for `Update`); we write no
// unsafe by hand. This is the one crate permitted to carry salsa's generated
// unsafe, since it is the sole owner of the engine surface.
#![allow(unsafe_code)]

//! The salsa query-database skeleton.
//!
//! This crate is the single owner of the incremental engine: it is the only
//! crate that depends on `salsa` directly. Everything downstream depends on
//! `fai-db` and uses the engine through the curated re-exports here (the
//! [`prelude`]) plus `#[fai_db::salsa::tracked]`-style attributes, so a version
//! bump or engine swap stays localized.
//!
//! It provides:
//!
//! * [`Db`] — the database trait every query takes as `&dyn Db`.
//! * [`FaiDatabase`] — the concrete database, with a source-file registry and a
//!   query-execution event log used for testing/diagnostics.
//! * [`SourceFile`] — the authoritative source-text input.
//! * [`InternedSymbol`] — a demonstration of salsa interning for derived keys.
//! * [`Diag`] — the diagnostics accumulator deeper phases emit into.
//! * [`DbSpanResolver`] — a [`fai_span::SpanResolver`] backed by the database.

use std::sync::{Arc, Mutex};

use camino::{Utf8Path, Utf8PathBuf};
use fai_diagnostics::Diagnostic;
use fai_span::{ByteOffset, LineIndex, ResolvedSpan, SourceId, Span, SpanResolver};
use rustc_hash::FxHashMap;

pub use salsa;

/// Curated re-exports downstream crates use instead of depending on `salsa`.
pub mod prelude {
    pub use salsa::{Accumulator, Database, Durability, Setter, Update};

    pub use crate::{Db, Diag, FaiDatabase, SourceFile};
}

pub use salsa::{Accumulator, Durability, Setter, Update};

/// The database trait every query operates over.
///
/// Custom (non-query) methods are added sparingly; for now just source-file
/// lookup, which reads the non-salsa registry on [`FaiDatabase`].
#[salsa::db]
pub trait Db: salsa::Database {
    /// Looks up a registered source file by its [`SourceId`], if any.
    fn source_file(&self, id: SourceId) -> Option<SourceFile>;

    /// Returns every registered source file, in [`SourceId`] order.
    fn all_source_files(&self) -> Vec<SourceFile>;
}

/// The authoritative source-text input.
///
/// Text lives here (not in a side structure) so that editing it bumps the salsa
/// revision and drives early cutoff. `path` is stored as a UTF-8 `String`
/// because the engine's change-tracking is defined for `String`.
#[salsa::input(debug)]
pub struct SourceFile {
    /// The file's stable identifier (its index in the source registry), so
    /// queries can build file-qualified [`Span`]s for diagnostics.
    pub source: SourceId,
    /// The file's path (UTF-8).
    #[returns(ref)]
    pub path: String,
    /// The file's full text.
    #[returns(ref)]
    pub text: String,
}

/// An interned string key.
///
/// Demonstrates salsa interning, which later phases use for derived keys (module
/// paths, type keys, …). Identifiers themselves use a separate non-salsa
/// interner to avoid threading the `'db` lifetime through the lexer.
#[salsa::interned(debug)]
pub struct InternedSymbol<'db> {
    /// The interned text.
    #[returns(ref)]
    pub text: String,
}

/// A diagnostic accumulated during a query computation.
///
/// Deeper phases call [`emit`] (or `Diag(..).accumulate(db)`); a caller collects
/// everything emitted under a query with `query::accumulated::<Diag>(db, ..)`.
#[salsa::accumulator]
#[derive(Debug, Clone)]
pub struct Diag(pub Diagnostic);

/// Emits `diagnostic` into the accumulated stream of the current query.
pub fn emit(db: &dyn Db, diagnostic: Diagnostic) {
    Diag(diagnostic).accumulate(db);
}

/// The line-start byte offsets of `file`, memoized.
///
/// Editing text *within* lines leaves this value unchanged, so dependents (e.g.
/// [`line_count`]) are cut off — the early-cutoff property in miniature.
#[salsa::tracked]
pub fn line_starts(db: &dyn Db, file: SourceFile) -> Vec<u32> {
    LineIndex::new(file.text(db)).line_starts().to_vec()
}

/// The number of lines in `file`, derived from [`line_starts`].
#[salsa::tracked]
pub fn line_count(db: &dyn Db, file: SourceFile) -> usize {
    line_starts(db, file).len()
}

/// The concrete database.
///
/// Beyond salsa's [`storage`](salsa::Storage) it holds a registry mapping
/// [`SourceId`]s to [`SourceFile`] inputs (and paths back to ids), plus an
/// optional execution-event log.
#[salsa::db]
#[derive(Clone)]
pub struct FaiDatabase {
    storage: salsa::Storage<Self>,
    files: Vec<SourceFile>,
    ids_by_path: FxHashMap<Utf8PathBuf, SourceId>,
    events: Arc<Mutex<Option<Vec<String>>>>,
}

#[salsa::db]
impl salsa::Database for FaiDatabase {}

#[salsa::db]
impl Db for FaiDatabase {
    fn source_file(&self, id: SourceId) -> Option<SourceFile> {
        self.files.get(id.index()).copied()
    }

    fn all_source_files(&self) -> Vec<SourceFile> {
        self.files.clone()
    }
}

impl Default for FaiDatabase {
    fn default() -> Self {
        Self::new()
    }
}

impl FaiDatabase {
    /// Creates an empty database.
    #[must_use]
    pub fn new() -> Self {
        let events: Arc<Mutex<Option<Vec<String>>>> = Arc::default();
        let storage = salsa::Storage::new(Some(Box::new({
            let events = events.clone();
            move |event| {
                if let salsa::EventKind::WillExecute { .. } = event.kind
                    && let Ok(mut guard) = events.lock()
                    && let Some(log) = guard.as_mut()
                {
                    log.push(format!("{:?}", event.kind));
                }
            }
        })));
        Self { storage, files: Vec::new(), ids_by_path: FxHashMap::default(), events }
    }

    /// Registers `path` with `text`, returning its [`SourceId`].
    ///
    /// Re-registering a known path updates its text in place (reusing the id and
    /// the salsa input, so spans stay valid and dependents re-validate).
    pub fn add_source(&mut self, path: Utf8PathBuf, text: String) -> SourceId {
        if let Some(&id) = self.ids_by_path.get(&path) {
            let file = self.files[id.index()];
            file.set_text(self).to(text);
            return id;
        }
        let id = SourceId::new(u32::try_from(self.files.len()).expect("too many source files"));
        let file = SourceFile::new(&*self, id, path.as_str().to_owned(), text);
        self.files.push(file);
        self.ids_by_path.insert(path, id);
        id
    }

    /// Registers `path` at a given [`Durability`]. Use [`Durability::HIGH`] for
    /// rarely-changing inputs (e.g. the embedded prelude) so dependents are not
    /// needlessly revalidated.
    pub fn add_source_with_durability(
        &mut self,
        path: Utf8PathBuf,
        text: String,
        durability: Durability,
    ) -> SourceId {
        let id = self.add_source(path, text);
        let file = self.files[id.index()];
        let current = file.text(self).clone();
        file.set_text(self).with_durability(durability).to(current);
        id
    }

    /// Enables recording of query-execution events (used by tests and tooling).
    pub fn enable_event_log(&self) {
        if let Ok(mut guard) = self.events.lock()
            && guard.is_none()
        {
            *guard = Some(Vec::new());
        }
    }

    /// Drains and returns the recorded execution events.
    pub fn take_events(&self) -> Vec<String> {
        match self.events.lock() {
            Ok(mut guard) => guard.as_mut().map(std::mem::take).unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }
}

/// A [`SpanResolver`] backed by the database.
///
/// Reuses the memoized [`line_starts`] table to map spans to line/column, then
/// reports the file's registered path.
pub struct DbSpanResolver<'a> {
    db: &'a dyn Db,
}

impl<'a> DbSpanResolver<'a> {
    /// Creates a resolver borrowing `db`.
    #[must_use]
    pub fn new(db: &'a dyn Db) -> Self {
        Self { db }
    }
}

impl SpanResolver for DbSpanResolver<'_> {
    fn resolve(&self, span: Span) -> Option<ResolvedSpan> {
        let file = self.db.source_file(span.source())?;
        let text = file.text(self.db);
        let path = Utf8Path::new(file.path(self.db)).to_owned();
        let len = ByteOffset::from_usize(text.len()).raw();
        let index = LineIndex::from_line_starts(line_starts(self.db, file), len);
        Some(ResolvedSpan {
            path,
            start: index.line_col(text, span.start()),
            end: index.line_col(text, span.end()),
            byte_start: span.start().raw(),
            byte_end: span.end().raw(),
        })
    }
}

#[cfg(test)]
mod tests {
    use fai_diagnostics::DiagnosticCode;
    use fai_span::TextRange;

    use super::*;

    const TEST_CODE: DiagnosticCode = DiagnosticCode::new("FAI0001");

    #[salsa::tracked]
    fn always_warns(db: &dyn Db, file: SourceFile) -> usize {
        let span = Span::new(SourceId::new(0), TextRange::empty(ByteOffset::ZERO));
        emit(db, Diagnostic::error(TEST_CODE, "synthetic", span));
        file.text(db).len()
    }

    #[test]
    fn memoizes_reruns_and_cuts_off_early() {
        let mut db = FaiDatabase::new();
        db.enable_event_log();
        let id = db.add_source("a.fai".into(), "a\nb".to_owned());
        let file = db.source_file(id).unwrap();

        // First evaluation runs both the base and derived query.
        assert_eq!(line_count(&db, file), 2);
        let log = db.take_events();
        assert!(log.iter().any(|e| e.contains("line_starts")), "log: {log:?}");
        assert!(log.iter().any(|e| e.contains("line_count")), "log: {log:?}");

        // Re-query with no input change: fully memoized, nothing executes.
        assert_eq!(line_count(&db, file), 2);
        assert!(db.take_events().is_empty(), "expected memoization");

        // Edit preserving newline positions: `line_starts` re-runs but its value
        // is unchanged, so `line_count` is cut off (early cutoff).
        file.set_text(&mut db).to("x\ny".to_owned());
        assert_eq!(line_count(&db, file), 2);
        let log = db.take_events();
        assert!(log.iter().any(|e| e.contains("line_starts")), "log: {log:?}");
        assert!(!log.iter().any(|e| e.contains("line_count")), "early cutoff failed: {log:?}");

        // Edit changing the line count: both queries re-run.
        file.set_text(&mut db).to("a\nb\nc".to_owned());
        assert_eq!(line_count(&db, file), 3);
        let log = db.take_events();
        assert!(log.iter().any(|e| e.contains("line_count")), "log: {log:?}");
    }

    #[test]
    fn interning_returns_stable_ids() {
        let db = FaiDatabase::new();
        let a1 = InternedSymbol::new(&db, "alpha".to_owned());
        let a2 = InternedSymbol::new(&db, "alpha".to_owned());
        let b = InternedSymbol::new(&db, "beta".to_owned());
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    #[test]
    fn durability_can_be_set_on_inputs() {
        let mut db = FaiDatabase::new();
        let id = db.add_source("dep.fai".into(), "x".to_owned());
        let file = db.source_file(id).unwrap();
        // High durability for rarely-changing inputs (e.g. dependencies).
        file.set_text(&mut db).with_durability(Durability::HIGH).to("y".to_owned());
        assert_eq!(file.text(&db).as_str(), "y");
    }

    #[test]
    fn accumulator_collects_emitted_diagnostics() {
        let mut db = FaiDatabase::new();
        let id = db.add_source("m.fai".into(), "abc".to_owned());
        let file = db.source_file(id).unwrap();
        let diags = always_warns::accumulated::<Diag>(&db, file);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].0.message, "synthetic");
    }

    #[test]
    fn db_span_resolver_maps_positions() {
        let mut db = FaiDatabase::new();
        let id = db.add_source("src/M.fai".into(), "let x =\n  1\n".to_owned());
        let file = db.source_file(id).unwrap();
        let _ = file;
        let resolver = DbSpanResolver::new(&db);
        let span = Span::new(id, TextRange::new(ByteOffset::new(10), ByteOffset::new(11)));
        let resolved = resolver.resolve(span).unwrap();
        assert_eq!(resolved.path, "src/M.fai");
        assert_eq!(resolved.start.line, 2);
        assert_eq!(resolved.start.column, 3);
        assert_eq!(resolved.byte_start, 10);
    }
}

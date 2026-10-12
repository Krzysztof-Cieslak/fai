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
//! * [`PackageEnvironment`] / [`SourceRole`] — package bindings and target-local
//!   source ownership, independent of paths and display names.
//! * [`InternedSymbol`] — a demonstration of salsa interning for derived keys.
//! * [`Diag`] — the diagnostics accumulator deeper phases emit into.
//! * [`DbSpanResolver`] — a [`fai_span::SpanResolver`] backed by the database.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

mod package;
use package::PackageRegistry;
pub use package::{
    PackageEnvironment, PackageId, SourceRole, automatically_visible,
    automatically_visible_sources, dependency_package, package_environment, package_environments,
    package_library_sources,
};

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

/// A single disabled optimization for controlled compiler-allocation experiments.
/// Normal compilation always uses `Enabled`; this is not a source-language mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum AllocationControl {
    /// Apply every ordinary compiler optimization.
    #[default]
    Enabled,
    /// Preserve sequence combinator boundaries.
    NoFusion,
    /// Release data cells normally instead of forwarding reset/reuse tokens.
    NoReuse,
    /// Keep aggregate cells and uniform aggregate call boundaries.
    NoScalarReplacement,
}

#[salsa::input(singleton, debug)]
struct CompilerControls {
    allocation: AllocationControl,
}

/// Tracked diagnostic control, shared by all queries and cloned database handles.
pub fn allocation_control(db: &dyn Db) -> AllocationControl {
    CompilerControls::get(db).allocation(db)
}

/// The database trait every query operates over.
///
/// Source lookup and workspace enumeration read tracked active membership, so
/// callers invalidate when files are added, removed, or reactivated.
#[salsa::db]
pub trait Db: salsa::Database {
    /// Looks up an active source file by its stable [`SourceId`], if any.
    fn source_file(&self, id: SourceId) -> Option<SourceFile>;

    /// Returns every active source file, in [`SourceId`] order.
    fn all_source_files(&self) -> Vec<SourceFile>;

    /// An independent handle to the same database, for running queries in
    /// parallel. salsa databases are `Send` but not `Sync`, so a `&dyn Db`
    /// cannot be shared across threads; instead each worker takes its own clone,
    /// which shares the underlying storage (and thus memoization) — salsa
    /// coordinates concurrent execution. Used with rayon's `map_with` (the seed
    /// is cloned per worker) so the `&dyn Db` seam is preserved.
    fn clone_box(&self) -> Box<dyn Db>;
}

impl Clone for Box<dyn Db> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
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
    /// Loader-controlled origin, independent of the display or filesystem path.
    pub origin: SourceOrigin,
    /// Target-local role; production modules cannot acquire test-only visibility.
    #[returns(ref)]
    pub role: SourceRole,
}

/// The source's semantic origin. Only the embedded-library loader assigns the
/// trusted origin; ordinary disk files remain user code regardless of their name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceOrigin {
    /// A workspace or editor-supplied source file.
    User,
    /// A source embedded in the compiler's standard library.
    StandardLibrary,
    /// A package instantiation supplied by the project loader.
    Package(PackageId),
}

impl SourceFile {
    /// Whether this input was registered as embedded standard-library code.
    #[must_use]
    pub fn is_std(self, db: &dyn Db) -> bool {
        self.origin(db) == SourceOrigin::StandardLibrary
    }
}

/// Active membership indexed by stable SourceId. A tombstone retains its input
/// identity in the database's historical registry but is invisible to queries.
#[salsa::input(singleton, debug)]
struct SourceRegistry {
    #[returns(ref)]
    slots: Vec<Option<SourceFile>>,
}

/// A per-id projection lets an unrelated membership change cut off before
/// queries that only look up an already-known source.
#[salsa::tracked]
fn registered_source(db: &dyn Db, registry: SourceRegistry, id: SourceId) -> Option<SourceFile> {
    registry.slots(db).get(id.index()).copied().flatten()
}

/// The synthetic path namespace of the embedded standard library.
///
/// Used for display only. Semantic classification uses [`SourceFile::origin`],
/// and embedded inputs have their own registry so a disk path cannot replace one.
pub const STD_PATH_PREFIX: &str = "<std>/";

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
/// stable [`SourceId`]s to historical [`SourceFile`] inputs (and paths back to
/// ids), plus an optional execution-event log. Active membership is a separate
/// tracked input; removals retain identities for later reactivation.
///
/// The registry (`files`, `ids_by_path`) lives behind `Arc` so cloning the
/// database — which the daemon does per read request to serve concurrent reads
/// on independent handles — shares it in O(1) rather than copying every path
/// string. Membership mutations update the tracked registry and follow salsa's
/// normal cancellation protocol: old snapshots must finish/drop before the
/// mutation completes. Historical identity maps use copy-on-write storage.
#[salsa::db]
#[derive(Clone)]
pub struct FaiDatabase {
    storage: salsa::Storage<Self>,
    files: Arc<Vec<SourceFile>>,
    ids_by_path: Arc<FxHashMap<Utf8PathBuf, SourceId>>,
    std_ids_by_path: Arc<FxHashMap<Utf8PathBuf, SourceId>>,
    scoped_ids: Arc<FxHashMap<(SourceOrigin, SourceRole, Utf8PathBuf), SourceId>>,
    package_ids: Arc<FxHashMap<String, PackageId>>,
    events: Arc<Mutex<Option<Vec<String>>>>,
}

#[salsa::db]
impl salsa::Database for FaiDatabase {}

#[salsa::db]
impl Db for FaiDatabase {
    fn source_file(&self, id: SourceId) -> Option<SourceFile> {
        registered_source(self, SourceRegistry::get(self), id)
    }

    fn all_source_files(&self) -> Vec<SourceFile> {
        SourceRegistry::get(self).slots(self).iter().flatten().copied().collect()
    }

    fn clone_box(&self) -> Box<dyn Db> {
        Box::new(self.clone())
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
        let db = Self {
            storage,
            files: Arc::default(),
            ids_by_path: Arc::default(),
            std_ids_by_path: Arc::default(),
            scoped_ids: Arc::default(),
            package_ids: Arc::default(),
            events,
        };
        let _ = SourceRegistry::builder(Vec::new()).durability(Durability::HIGH).new(&db);
        let _ = PackageRegistry::builder(Vec::new()).durability(Durability::HIGH).new(&db);
        let _ = CompilerControls::builder(AllocationControl::Enabled)
            .durability(Durability::HIGH)
            .new(&db);
        db
    }

    /// Changes one allocation optimization for a paired diagnostic compilation.
    /// Updating the tracked input invalidates affected IR, ABI, and object queries.
    pub fn set_allocation_control(&mut self, control: AllocationControl) {
        CompilerControls::get(self)
            .set_allocation(self)
            .with_durability(Durability::HIGH)
            .to(control);
    }

    /// Registers `path` with `text`, returning its [`SourceId`].
    ///
    /// Re-registering a known path updates its text in place (reusing the id and
    /// the salsa input, so spans stay valid and dependents re-validate).
    pub fn add_source(&mut self, path: Utf8PathBuf, text: String) -> SourceId {
        self.add_source_with_origin(path, text, SourceOrigin::User)
    }

    /// Registers a source in its origin's separate identity registry. Standard
    /// sources are high-durability; the path never selects or changes origin.
    pub fn add_source_with_origin(
        &mut self,
        path: Utf8PathBuf,
        text: String,
        origin: SourceOrigin,
    ) -> SourceId {
        self.add_source_scoped(path, text, origin, SourceRole::Library)
    }

    /// Registers a source with explicit package and target ownership.
    pub fn add_source_scoped(
        &mut self,
        path: Utf8PathBuf,
        text: String,
        origin: SourceOrigin,
        role: SourceRole,
    ) -> SourceId {
        let durability = match origin {
            SourceOrigin::User | SourceOrigin::Package(_) => Durability::LOW,
            SourceOrigin::StandardLibrary => Durability::HIGH,
        };
        // Historical entries retain their identity after deactivation.
        let existing = self.historical_scoped_id(&path, origin, &role);
        if let Some(id) = existing {
            let file = self.files[id.index()];
            file.set_text(self).with_durability(durability).to(text);
            self.activate_source(id, file);
            return id;
        }
        let id = SourceId::new(u32::try_from(self.files.len()).expect("too many source files"));
        let file = SourceFile::builder(id, path.as_str().to_owned(), text, origin, role.clone())
            .durability(durability)
            .new(&*self);
        // Copy-on-write: only clones the registry while a snapshot still holds it.
        Arc::make_mut(&mut self.files).push(file);
        match (origin, &role) {
            (SourceOrigin::User, SourceRole::Library) => {
                Arc::make_mut(&mut self.ids_by_path).insert(path, id)
            }
            (SourceOrigin::StandardLibrary, SourceRole::Library) => {
                Arc::make_mut(&mut self.std_ids_by_path).insert(path, id)
            }
            _ => Arc::make_mut(&mut self.scoped_ids).insert((origin, role, path), id),
        };
        self.activate_source(id, file);
        id
    }

    fn historical_scoped_id(
        &self,
        path: &Utf8Path,
        origin: SourceOrigin,
        role: &SourceRole,
    ) -> Option<SourceId> {
        match (origin, role) {
            (SourceOrigin::User, SourceRole::Library) => self.ids_by_path.get(path).copied(),
            (SourceOrigin::StandardLibrary, SourceRole::Library) => {
                self.std_ids_by_path.get(path).copied()
            }
            _ => self.scoped_ids.get(&(origin, role.clone(), path.to_owned())).copied(),
        }
    }

    /// Looks up an active file in a particular ownership and source scope.
    #[must_use]
    pub fn id_for_scoped_path(
        &self,
        path: &Utf8Path,
        origin: SourceOrigin,
        role: &SourceRole,
    ) -> Option<SourceId> {
        self.historical_scoped_id(path, origin, role).filter(|&id| self.source_file(id).is_some())
    }

    /// Interns a stable package instantiation key without changing existing ids.
    pub fn register_package(&mut self, key: String) -> PackageId {
        if let Some(&id) = self.package_ids.get(&key) {
            return id;
        }
        let registry = PackageRegistry::get(self);
        let mut entries = registry.entries(self).clone();
        let id = PackageId(u32::try_from(entries.len()).expect("too many package environments"));
        let package =
            PackageEnvironment::builder(id, key.clone(), BTreeMap::new(), BTreeMap::new())
                .durability(Durability::HIGH)
                .new(&*self);
        entries.push(package);
        registry.set_entries(self).with_durability(Durability::HIGH).to(entries);
        Arc::make_mut(&mut self.package_ids).insert(key, id);
        id
    }

    /// Updates dependency aliases; returns whether a registered environment changed.
    pub fn set_package_dependencies(
        &mut self,
        id: PackageId,
        normal: BTreeMap<String, PackageId>,
        development: BTreeMap<String, PackageId>,
    ) -> bool {
        let Some(package) = package_environment(self, id) else {
            return false;
        };
        let mut changed = false;
        if package.dependencies(self) != &normal {
            package.set_dependencies(self).with_durability(Durability::HIGH).to(normal);
            changed = true;
        }
        if package.development(self) != &development {
            package.set_development(self).with_durability(Durability::HIGH).to(development);
            changed = true;
        }
        changed
    }

    /// Copies package environments into an empty scratch database, preserving ids.
    pub fn copy_package_environments(&mut self, from: &dyn Db) {
        for package in package_environments(from) {
            let id = self.register_package(package.key(from).clone());
            assert_eq!(id, package.id(from), "scratch package ids must be preserved");
            self.set_package_dependencies(
                id,
                package.dependencies(from).clone(),
                package.development(from).clone(),
            );
        }
    }

    fn activate_source(&mut self, id: SourceId, file: SourceFile) {
        let registry = SourceRegistry::get(self);
        if registry.slots(self).get(id.index()) == Some(&Some(file)) {
            return;
        }
        let mut slots = registry.slots(self).clone();
        slots.resize(slots.len().max(id.index() + 1), None);
        slots[id.index()] = Some(file);
        registry.set_slots(self).with_durability(Durability::HIGH).to(slots);
    }

    /// Deactivates a source while retaining its stable id and input for a later
    /// re-add. Returns whether an active source was removed.
    pub fn remove_source(&mut self, id: SourceId) -> bool {
        self.remove_sources(std::iter::once(id)) != 0
    }

    /// Deactivates a batch of sources with one membership update. Unknown or
    /// already-removed ids are ignored; returns the number newly deactivated.
    pub fn remove_sources(&mut self, ids: impl IntoIterator<Item = SourceId>) -> usize {
        let registry = SourceRegistry::get(self);
        let active = registry.slots(self);
        let ids: Vec<_> = ids
            .into_iter()
            .filter(|id| active.get(id.index()).is_some_and(Option::is_some))
            .collect();
        if ids.is_empty() {
            return 0;
        }
        let mut slots = registry.slots(self).clone();
        let mut removed = 0;
        for id in ids {
            if let Some(slot) = slots.get_mut(id.index())
                && slot.take().is_some()
            {
                removed += 1;
            }
        }
        if removed != 0 {
            registry.set_slots(self).with_durability(Durability::HIGH).to(slots);
        }
        removed
    }

    /// Looks up the active user [`SourceId`] for `path`, if any. An embedded
    /// source with the same display path is never returned by disk lookup.
    #[must_use]
    pub fn id_for_path(&self, path: &Utf8Path) -> Option<SourceId> {
        self.ids_by_path.get(path).copied().filter(|&id| self.source_file(id).is_some())
    }

    /// Registers `path` at a given [`Durability`]. Use [`Durability::HIGH`] for
    /// rarely-changing user inputs so dependents are not needlessly revalidated.
    /// Durability does not grant standard-library origin.
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

    #[salsa::tracked]
    fn registered_count(db: &dyn Db) -> usize {
        db.all_source_files().len()
    }

    #[salsa::tracked]
    fn source_exists(db: &dyn Db) -> bool {
        db.source_file(SourceId::new(0)).is_some()
    }

    #[test]
    fn adding_a_source_invalidates_cached_membership_queries() {
        let mut db = FaiDatabase::new();
        assert_eq!(registered_count(&db), 0);
        assert!(!source_exists(&db));
        db.add_source("A.fai".into(), "module A\n".into());
        assert_eq!(registered_count(&db), 1);
        assert!(source_exists(&db));
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
    fn membership_addition_cancels_an_old_snapshot() {
        let mut db = FaiDatabase::new();
        db.add_source("a.fai".into(), "a".to_owned());
        let snapshot = db.clone();
        let ready = Arc::new(std::sync::Barrier::new(2));
        let reader_ready = Arc::clone(&ready);
        let reader = std::thread::spawn(move || {
            assert_eq!(registered_count(&snapshot), 1);
            assert!(snapshot.id_for_path(Utf8Path::new("b.fai")).is_none());
            reader_ready.wait();
            while salsa::Cancelled::catch(std::panic::AssertUnwindSafe(|| {
                registered_count(&snapshot)
            }))
            .is_ok()
            {
                std::thread::yield_now();
            }
        });
        ready.wait();
        db.add_source("b.fai".into(), "b".to_owned());
        reader.join().unwrap();
        assert_eq!(registered_count(&db), 2);
    }

    #[test]
    fn removal_and_reactivation_keep_the_source_identity() {
        let mut db = FaiDatabase::new();
        let id = db.add_source("A.fai".into(), "old".into());
        let file = db.source_file(id).unwrap();
        assert!(source_exists(&db));
        assert!(db.remove_source(id));
        assert!(!db.remove_source(id));
        assert_eq!(registered_count(&db), 0);
        assert!(!source_exists(&db));
        assert!(db.id_for_path(Utf8Path::new("A.fai")).is_none());
        let readded = db.add_source("A.fai".into(), "new".into());
        assert_eq!(readded, id);
        assert_eq!(db.source_file(id), Some(file));
        assert_eq!(file.text(&db), "new");
        assert!(source_exists(&db));
    }

    #[test]
    fn text_edits_do_not_invalidate_membership() {
        let mut db = FaiDatabase::new();
        db.add_source("A.fai".into(), "old".into());
        assert_eq!(registered_count(&db), 1);
        db.enable_event_log();
        db.add_source("A.fai".into(), "new".into());
        assert_eq!(registered_count(&db), 1);
        assert!(!db.take_events().iter().any(|e| e.contains("registered_count")));
    }

    #[test]
    fn unrelated_addition_cuts_off_at_source_lookup() {
        let mut db = FaiDatabase::new();
        db.add_source("A.fai".into(), "a".into());
        assert!(source_exists(&db));
        db.enable_event_log();
        db.add_source("B.fai".into(), "b".into());
        assert!(source_exists(&db));
        assert!(!db.take_events().iter().any(|e| e.contains("source_exists")));
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

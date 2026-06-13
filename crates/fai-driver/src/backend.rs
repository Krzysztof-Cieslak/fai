//! The native backend: the content-addressed object cache, reachability from
//! `main`, AOT linking, and the in-process JIT runner.
//!
//! [`object_code`] is the cache unit (one relocatable object per definition);
//! salsa's dependency tracking gives the per-function cache hit — editing one
//! definition's body re-runs only its `object_code`. [`build_native`] computes
//! the closure reachable from `main`, codegens it, and links the cached objects
//! with the embedded runtime archive; [`jit_run_program`] compiles the same
//! closure in memory and runs it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use fai_codegen::{JitProgram, main_object, object_for_def, reuse_object_for_def};
use fai_core::ir::{FnAbi, LoweredDef};
use fai_core::wire::{WireBundle, WireDef, WireDefId, def_to_wire, from_wire};
use fai_core::{core, helper_inlined};
use fai_db::{Db, Diag, SourceFile};
use fai_diagnostics::wire::{DiagnosticWire, to_wire};
use fai_diagnostics::{Diagnostic, SCHEMA_VERSION, Severity, render_human};
use fai_rc::{
    BorrowSig, borrow_signature, combined_lowered, forwards_to, member_wrapper, mutual_groups,
    rc_emit, rc_lowered, reuse_signature,
};
use fai_resolve::{DefId, ModuleName, module_defs, module_name};
use fai_span::SpanResolver;
use fai_syntax::Symbol;
use fai_syntax::ast::{ItemKind, Visibility};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Serialize;

use crate::{LINK_FAILED, NO_ENTRY_POINT, semantic_diagnostics, tooling_span};

/// The runtime static archive, built by `build.rs` and linked into executables.
const RUNTIME_ARCHIVE: &[u8] = include_bytes!(env!("FAI_RUNTIME_ARCHIVE"));

/// The system libraries the runtime archive must be linked against on this host,
/// as reported by `rustc --print native-static-libs` at build time (see
/// `build.rs`). Whitespace-separated, in the linker's own flag syntax
/// (`-lpthread …` for Unix `cc`, `kernel32.lib …` for MSVC `link.exe`).
const RUNTIME_NATIVE_LIBS: &str = env!("FAI_RUNTIME_NATIVE_LIBS");

/// The required entry-point name.
const ENTRY: &str = "main";

/// The standard library's private `Runtime` value binding, applied to `main` by
/// the entry trampoline.
const RUNTIME_VALUE: &str = "defaultRuntime";

/// The `Runtime` value binding's definition, supplied to `main` by the entry
/// trampoline. It is not referenced from `main`'s body (the trampoline injects
/// it), so the backend seeds it as a second reachability root. `None` if the
/// standard library does not define it.
fn runtime_root(db: &dyn Db) -> Option<DefId> {
    let file = fai_resolve::prelude_module_file(db)?;
    let name = Symbol::intern(RUNTIME_VALUE);
    module_defs(db, file).get(name)?;
    Some(DefId::new(file.source(db), name))
}

/// The mangled symbol base for a definition: `fai_<module>_<name>`.
#[must_use]
pub fn symbol_base(db: &dyn Db, def: DefId) -> String {
    mangle(&module_label(db, def), def.name.as_str())
}

/// A definition's module display label (or a fallback), used for mangling.
pub(crate) fn module_label(db: &dyn Db, def: DefId) -> String {
    db.source_file(def.file)
        .and_then(|f| module_name(db, f))
        .map_or_else(|| "M".to_owned(), |ModuleName(s)| s.as_str().to_owned())
}

/// Builds the backend symbol base from a module label and a binding name. Pure,
/// so a database-free worker reconstructs identical names from the wire bundle.
///
/// Both parts are sanitized: the result names a symbol *and* an on-disk object
/// file, so it must be a valid identifier and a valid file name on every OS.
/// Operator definitions (e.g. `>>`, `<>`) carry characters Windows forbids in
/// file names (`<>:"/\|?*`), so each non-alphanumeric byte is escaped as `_xNN`
/// (its hex) — injective, so distinct definitions keep distinct symbols.
pub(crate) fn mangle(module_label: &str, name: &str) -> String {
    format!("fai_{}_{}", sanitize_ident(module_label), sanitize_ident(name))
}

/// Escapes a string to an identifier- and file-name-safe form: ASCII
/// alphanumerics and `_` pass through; every other byte becomes `_xNN`.
fn sanitize_ident(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'_' {
            out.push(b as char);
        } else {
            let _ = write!(out, "_x{b:02x}");
        }
    }
    out
}

/// A definition's syntactic source-parameter count (the `let f a b = …` binders),
/// excluding any offset evidence. Read from the binding, body-edit-stable.
fn source_param_count(db: &dyn Db, file: SourceFile, name: Symbol) -> usize {
    let parsed = fai_syntax::parse(db, file);
    // Locate the binding by its (qualified) name via the paired definitions, so a
    // nested definition is found by its module path rather than the local name.
    module_defs(db, file)
        .get(name)
        .and_then(|d| match &parsed.module.items[d.binding.index()].kind {
            ItemKind::Binding { params, .. } => Some(params.len()),
            _ => None,
        })
        .unwrap_or(0)
}

/// A definition's runtime arity: its source parameters plus the leading offset
/// evidence its (row-polymorphic) type requires. Read from the binding and the
/// signature, both body-edit-stable, so the codegen firewall stays intact.
#[salsa::tracked]
pub fn def_arity(db: &dyn Db, file: SourceFile, name: Symbol) -> usize {
    let source_params = source_param_count(db, file, name);
    let def = DefId::new(file.source(db), name);
    let evidence = fai_types::declared_or_inferred_scheme(db, def)
        .map_or(0, |scheme| fai_types::evidence_count(&scheme));
    source_params + evidence
}

pub(crate) fn arity_of(db: &dyn Db, def: DefId) -> usize {
    db.source_file(def.file).map_or(0, |f| def_arity(db, f, def.name))
}

/// A definition's native calling-convention shape: which runtime parameters carry
/// an unboxed `Float` (raw `f64` bits) and whether the result is an unboxed
/// `Float`. Derived from the *signature* (peeling the syntactic source-parameter
/// count off the type; leading offset-evidence parameters are integers, never
/// floats) so it is body-edit-stable, preserving the codegen firewall: a caller's
/// object depends on a callee's signature, not its body. Tracked (like
/// [`def_arity`]) so its memoization boundary keeps a dependent's recompute
/// independent of unrelated edits.
#[salsa::tracked]
pub fn float_abi(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<FnAbi> {
    let def = DefId::new(file.source(db), name);
    let Some(scheme) = fai_types::declared_or_inferred_scheme(db, def) else {
        return Arc::new(FnAbi::default());
    };
    // A niche `Option` parameter/result is carried wrapper-free across a direct
    // call (both schemes).
    let niche = |ty: &fai_types::Ty| fai_core::niche_scheme(db, ty);
    Arc::new(FnAbi::from_scheme(&scheme, source_param_count(db, file, name), &niche))
}

pub(crate) fn abi_of(db: &dyn Db, def: DefId) -> FnAbi {
    db.source_file(def.file).map_or_else(FnAbi::default, |f| (*float_abi(db, f, def.name)).clone())
}

/// A definition's per-parameter borrow flags — the same [`borrow_signature`] the
/// reference-count pass uses to place a caller's drops — so a direct caller knows
/// which boxed scalar arguments it must release after the call (a borrowed
/// parameter is lent, not consumed). A definition with no source file (a synthetic
/// combined loop) borrows nothing.
pub(crate) fn borrows_of(db: &dyn Db, def: DefId) -> Vec<bool> {
    db.source_file(def.file).map_or_else(Vec::new, |f| borrow_signature(db, f, def.name).0)
}

/// The cached relocatable object for one definition (the content-addressed cache
/// unit; see [`build_native`]).
///
/// Declared LRU-capable (`lru = 0` is unbounded, so the one-shot CLI and tests
/// are unaffected); the long-lived daemon caps it via
/// [`set_object_cache_capacity`] so these large, on-disk-backed object blobs do
/// not accumulate without bound. An evicted entry is re-read from the on-disk
/// cache (or regenerated), so eviction only trades memory for that lookup.
#[salsa::tracked(lru = 0)]
pub fn object_code(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<Vec<u8>> {
    // The emit-ready lowering: reuse tokens forwarded into accepting callees. The
    // primary object never includes the token-taking entry (that is a separate,
    // forward-reachability-gated object), so it stays a pure function of the
    // definition (the cache firewall).
    let lowered = rc_emit(db, file, name);
    let namer = |d: DefId| symbol_base(db, d);
    let arity = |d: DefId| arity_of(db, d);
    let abi = |d: DefId| abi_of(db, d);
    let borrows = |d: DefId| borrows_of(db, d);
    Arc::new(object_for_def(&lowered, &namer, &arity, &abi, &borrows))
}

/// The cached relocatable object holding only a definition's token-taking
/// specialized entry (`{base}__reuse`). A separate cache unit from
/// [`object_code`] so the primary object stays forwarding-independent; linked
/// only where a reachable caller forwards reuse tokens to this definition.
#[salsa::tracked(lru = 0)]
pub fn reuse_object_code(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<Vec<u8>> {
    let lowered = rc_emit(db, file, name);
    let namer = |d: DefId| symbol_base(db, d);
    let arity = |d: DefId| arity_of(db, d);
    let abi = |d: DefId| abi_of(db, d);
    let borrows = |d: DefId| borrows_of(db, d);
    Arc::new(reuse_object_for_def(&lowered, &namer, &arity, &abi, &borrows))
}

/// The definitions a reachable caller forwards reuse tokens to — the definitions
/// whose token-taking specialized entry must be emitted and linked. Computed from
/// the emit-ready lowerings of the reachable set (each forwarding call records its
/// callee), so a definition that accepts tokens but is never forwarded to gets no
/// specialized entry. Deterministically ordered.
fn forward_targets(db: &dyn Db, reachable: &[DefId]) -> Vec<DefId> {
    let mut seen = FxHashSet::default();
    let mut order = Vec::new();
    for &def in reachable {
        let Some(file) = db.source_file(def.file) else { continue };
        for callee in forwards_to(&rc_emit(db, file, def.name)) {
            if seen.insert(callee) {
                order.push(callee);
            }
        }
    }
    order
}

/// Bounds the number of cached [`object_code`] blobs the database keeps in
/// memory (0 = unbounded). The least-recently-used entries above the cap are
/// evicted at the next revision; each is cheaply recoverable from the on-disk
/// cache. Used by the daemon to keep its warm database's footprint bounded.
pub fn set_object_cache_capacity(db: &mut dyn Db, capacity: usize) {
    object_code::set_lru_capacity(db, capacity);
}

/// Whether `file` defines an entry `main`.
fn has_main(db: &dyn Db, file: SourceFile) -> bool {
    module_defs(db, file).get(Symbol::intern(ENTRY)).is_some()
}

/// The definitions reachable from `file`'s `main`, in discovery order. Follows
/// `Global` references in the lowered code (so prelude helpers, which resolution
/// records as builtins, are included).
#[must_use]
pub fn reachable_defs(db: &dyn Db, file: SourceFile) -> Vec<DefId> {
    let entry = DefId::new(file.source(db), Symbol::intern(ENTRY));
    let mut seen = FxHashSet::default();
    let mut order = Vec::new();
    // `main` plus the `Runtime` value binding the entry trampoline forces and
    // applies to it; the latter is not referenced from any reachable body.
    let mut stack = vec![entry];
    if let Some(runtime) = runtime_root(db) {
        stack.push(runtime);
    }
    while let Some(def) = stack.pop() {
        if !seen.insert(def) {
            continue;
        }
        order.push(def);
        if let Some(file) = db.source_file(def.file) {
            for callee in helper_inlined(db, file, def.name).referenced_globals() {
                if !seen.contains(&callee) {
                    stack.push(callee);
                }
            }
        }
    }
    order
}

/// The definitions reachable from a set of root references, in discovery order,
/// excluding a set of synthesized defs that are supplied directly (not via
/// `core`). Used to gather a contract harness's real callees.
#[must_use]
pub(crate) fn reachable_from_roots(
    db: &dyn Db,
    roots: &[DefId],
    exclude: &FxHashSet<DefId>,
) -> Vec<DefId> {
    let mut seen = exclude.clone();
    let mut order = Vec::new();
    let mut stack: Vec<DefId> = roots.iter().copied().filter(|d| !exclude.contains(d)).collect();
    while let Some(def) = stack.pop() {
        if !seen.insert(def) {
            continue;
        }
        order.push(def);
        if let Some(file) = db.source_file(def.file) {
            for callee in helper_inlined(db, file, def.name).referenced_globals() {
                if !seen.contains(&callee) {
                    stack.push(callee);
                }
            }
        }
    }
    order
}

/// The mutual-recursion flattening applied to a reachable set: each member is
/// replaced by a wrapper that calls its group's combined function, and one
/// combined function (a flattened loop) is added per group. The combined
/// functions are not source-backed, so they are reference-counted in memory (via
/// [`rc_lowered`], like contract harnesses) rather than through the cached
/// `object_code` query, and built at assembly time like the `fai_main` trampoline.
struct ProgramGroups {
    /// Reachable group members, each mapped to its (reference-counted) wrapper.
    wrappers: FxHashMap<DefId, LoweredDef>,
    /// The combined loop functions (reference-counted), one per group.
    combined: Vec<LoweredDef>,
    /// The arity of each synthetic combined function (its callers need it to make
    /// a saturated direct call); these definitions have no source binding.
    arity: FxHashMap<DefId, usize>,
}

impl ProgramGroups {
    /// Whether `def` is a group member (so its normal object/def is replaced by a
    /// wrapper).
    fn is_member(&self, def: DefId) -> bool {
        self.wrappers.contains_key(&def)
    }
}

/// Reference-counts an in-memory (non-source-backed) definition with an all-owned
/// signature, the way the combined functions and wrappers are compiled.
fn rc_owned(db: &dyn Db, lowered: &LoweredDef) -> LoweredDef {
    let n = lowered.entry().params.len();
    rc_lowered(db, lowered, &BorrowSig(vec![false; n]))
}

/// Computes the mutual-recursion flattening for a reachable set: the wrappers for
/// reachable group members and the combined loop for each such group.
fn program_groups(db: &dyn Db, reachable: &[DefId]) -> ProgramGroups {
    let mut wrappers = FxHashMap::default();
    let mut combined = Vec::new();
    let mut arity = FxHashMap::default();
    let mut seen = FxHashSet::default();
    for &def in reachable {
        let Some(file) = db.source_file(def.file) else { continue };
        let groups = mutual_groups(db, file);
        let Some(group) = groups.group_of(def) else { continue };
        wrappers.insert(def, rc_owned(db, &member_wrapper(db, file, def, group)));
        if seen.insert(group.combined) {
            combined.push(rc_owned(db, &combined_lowered(db, file, group)));
            arity.insert(group.combined, group.arity);
        }
    }
    ProgramGroups { wrappers, combined, arity }
}

/// Collects the diagnostics that must be clean before codegen: each reachable
/// file's parse/resolve/type diagnostics plus each reachable definition's
/// lowering diagnostics (e.g. unsupported-construct `FAI7001`).
pub(crate) fn precompile_diagnostics(db: &dyn Db, reachable: &[DefId]) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut files = FxHashSet::default();
    for def in reachable {
        let Some(file) = db.source_file(def.file) else { continue };
        let source = file.source(db);
        for d in core::accumulated::<Diag>(db, file, def.name) {
            if d.0.primary.source() == source {
                out.push(d.0.clone());
            }
        }
        if files.insert(source) {
            out.extend(semantic_diagnostics(db, file));
        }
    }
    out.sort_by(|a, b| {
        (a.primary.start().raw(), a.code.as_str()).cmp(&(b.primary.start().raw(), b.code.as_str()))
    });
    out.dedup_by(|a, b| {
        a.code == b.code && a.primary.start() == b.primary.start() && a.message == b.message
    });
    out
}

/// The outcome of a native build.
#[derive(Debug, Clone)]
pub struct BuildOutcome {
    /// The produced executable, if the build succeeded.
    pub artifact: Option<Utf8PathBuf>,
    /// Diagnostics produced (compile errors, or a link failure).
    pub diagnostics: Vec<Diagnostic>,
    /// Whether the build produced an artifact.
    pub ok: bool,
}

/// The JSON envelope for `fai build`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildOutput {
    /// Output schema version.
    pub schema_version: u32,
    /// The produced executable's path, if any.
    pub artifact: Option<String>,
    /// The build's diagnostics, in wire form.
    pub diagnostics: Vec<DiagnosticWire>,
    /// Whether the build succeeded.
    pub ok: bool,
}

impl BuildOutcome {
    /// Builds the JSON wire envelope.
    #[must_use]
    pub fn to_output(&self, resolver: &dyn SpanResolver) -> BuildOutput {
        BuildOutput {
            schema_version: SCHEMA_VERSION,
            artifact: self.artifact.as_ref().map(ToString::to_string),
            diagnostics: to_wire(&self.diagnostics, resolver),
            ok: self.ok,
        }
    }

    /// Renders the outcome for humans (diagnostics, then the artifact path).
    #[must_use]
    pub fn render_human(&self, resolver: &dyn SpanResolver, color: bool) -> String {
        use std::fmt::Write as _;
        let mut out = render_human(&self.diagnostics, resolver, color);
        if let Some(artifact) = &self.artifact {
            let _ = writeln!(out, "built {artifact}");
        }
        out
    }
}

/// Builds (or loads from the content-addressed cache) the relocatable object for
/// each reachable definition, **in parallel across definitions** — each is an
/// independent code generation plus cache lookup. Order is preserved, so the
/// linker input (and the resulting artifact) stays deterministic. Each rayon
/// worker takes its own database handle (a cheap clone sharing the storage and
/// memoization; salsa coordinates concurrent query execution).
fn build_objects(db: &dyn Db, reachable: &[DefId]) -> Vec<(String, Vec<u8>)> {
    reachable
        .par_iter()
        .map_with(db.clone_box(), |dbh, def| {
            let db: &dyn Db = &**dbh;
            let def_file = db.source_file(def.file)?;
            let bytes = crate::cache::load_or_build_object(db, def_file, def.name);
            Some((symbol_base(db, *def), (*bytes).clone()))
        })
        .collect::<Vec<Option<(String, Vec<u8>)>>>()
        .into_iter()
        .flatten()
        .collect()
}

/// Compiles the closure reachable from `file`'s `main` to a native executable at
/// `out`, reusing cached `object_code` for unchanged definitions.
#[must_use]
pub fn build_native(db: &dyn Db, file: SourceFile, out: &Utf8Path) -> BuildOutcome {
    if !has_main(db, file) {
        return BuildOutcome { artifact: None, diagnostics: vec![no_entry_point()], ok: false };
    }
    let reachable = reachable_defs(db, file);
    let diagnostics = precompile_diagnostics(db, &reachable);
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return BuildOutcome { artifact: None, diagnostics, ok: false };
    }

    // Flatten mutual-recursion groups: members compile to wrappers, plus one
    // combined loop per group (built here, like the `fai_main` trampoline, so the
    // cached `object_code` path stays untouched for ordinary definitions).
    let groups = program_groups(db, &reachable);
    let normal: Vec<DefId> = reachable.iter().copied().filter(|d| !groups.is_member(*d)).collect();
    let mut objects = build_objects(db, &normal);
    let namer = |d: DefId| symbol_base(db, d);
    let arity = |d: DefId| groups.arity.get(&d).copied().unwrap_or_else(|| arity_of(db, d));
    // A member's wrapper presents the member's ABI; the synthetic combined loop is
    // schemeless but **direct-called** by member wrappers, so it takes the register
    // ABI with all-boxed (uniform `i64`) slots.
    let abi = |d: DefId| {
        groups.arity.get(&d).map_or_else(|| abi_of(db, d), |&n| FnAbi::register_uniform(n))
    };
    // A synthetic combined loop has no source binding, so it borrows nothing; every
    // other callee reports its real borrow signature.
    let borrows = |d: DefId| {
        if groups.arity.contains_key(&d) { Vec::new() } else { borrows_of(db, d) }
    };
    for (member, wrapper) in &groups.wrappers {
        objects.push((
            symbol_base(db, *member),
            object_for_def(wrapper, &namer, &arity, &abi, &borrows),
        ));
    }
    for combined in &groups.combined {
        objects.push((
            symbol_base(db, combined.def),
            object_for_def(combined, &namer, &arity, &abi, &borrows),
        ));
    }
    // Link a token-taking specialized entry for each definition a reachable caller
    // forwards reuse tokens to (a separate cache unit from the primary object).
    for target in forward_targets(db, &reachable) {
        if let Some(tf) = db.source_file(target.file) {
            objects.push((
                fai_codegen::reuse_symbol(&namer, target),
                (*reuse_object_code(db, tf, target.name)).clone(),
            ));
        }
    }

    let entry = DefId::new(file.source(db), Symbol::intern(ENTRY));
    let runtime = runtime_root(db).expect("standard library defines the Runtime value binding");
    objects.push(("fai_main".to_owned(), main_object(entry, runtime, &namer)));

    match link(&objects, out) {
        Ok(artifact) => BuildOutcome { artifact: Some(artifact), diagnostics, ok: true },
        Err(message) => {
            let mut diagnostics = diagnostics;
            diagnostics.push(Diagnostic::error(LINK_FAILED, message, tooling_span()));
            BuildOutcome { artifact: None, diagnostics, ok: false }
        }
    }
}

/// The outcome of a JIT run.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The program's exit code (or 4 if it failed to compile).
    pub exit_code: i32,
    /// Compile diagnostics, if any.
    pub diagnostics: Vec<Diagnostic>,
}

impl RunOutcome {
    /// Renders any compile diagnostics for humans.
    #[must_use]
    pub fn render_human(&self, resolver: &dyn SpanResolver, color: bool) -> String {
        render_human(&self.diagnostics, resolver, color)
    }
}

/// Exit code for a program that failed to compile.
const COMPILE_ERROR_EXIT: i32 = 4;

/// Compiles the closure reachable from `file`'s `main` and runs it in process,
/// returning its exit code. Used by the isolated `fai run` worker.
#[must_use]
pub fn jit_run_program(db: &dyn Db, file: SourceFile) -> RunOutcome {
    if !has_main(db, file) {
        return RunOutcome { exit_code: COMPILE_ERROR_EXIT, diagnostics: vec![no_entry_point()] };
    }
    let reachable = reachable_defs(db, file);
    let diagnostics = precompile_diagnostics(db, &reachable);
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return RunOutcome { exit_code: COMPILE_ERROR_EXIT, diagnostics };
    }

    // Flatten mutual-recursion groups (members → wrappers, plus a combined loop
    // per group); the member set is consulted in the parallel lowering below.
    let groups = program_groups(db, &reachable);
    let members: FxHashSet<DefId> = groups.wrappers.keys().copied().collect();

    // Lower + reference-count (emit-ready, with reuse forwarding) each ordinary
    // reachable def in parallel (independent queries); the JIT compile that follows
    // is serial (one shared module).
    let mut defs: Vec<LoweredDef> = reachable
        .par_iter()
        .map_with(db.clone_box(), |dbh, def| {
            if members.contains(def) {
                return None; // a group member compiles to its wrapper, added below
            }
            let db: &dyn Db = &**dbh;
            db.source_file(def.file).map(|f| (*rc_emit(db, f, def.name)).clone())
        })
        .collect::<Vec<Option<LoweredDef>>>()
        .into_iter()
        .flatten()
        .collect();
    // Keep a token-taking specialized entry only where a reachable caller forwards
    // to it; clearing it elsewhere stops the JIT emitting an unused entry.
    let targets: FxHashSet<DefId> = forward_targets(db, &reachable).into_iter().collect();
    for d in &mut defs {
        if !targets.contains(&d.def) {
            d.reuse_entry = None;
        }
    }
    defs.extend(groups.wrappers.into_values());
    defs.extend(groups.combined);

    let entry = DefId::new(file.source(db), Symbol::intern(ENTRY));
    let runtime = runtime_root(db).expect("standard library defines the Runtime value binding");
    let namer = |d: DefId| symbol_base(db, d);
    let arity = |d: DefId| groups.arity.get(&d).copied().unwrap_or_else(|| arity_of(db, d));
    // The synthetic combined loop is direct-called by member wrappers (register ABI,
    // all-boxed slots); every other def reports its own ABI.
    let abi = |d: DefId| {
        groups.arity.get(&d).map_or_else(|| abi_of(db, d), |&n| FnAbi::register_uniform(n))
    };
    let borrows = |d: DefId| {
        if groups.arity.contains_key(&d) { Vec::new() } else { borrows_of(db, d) }
    };
    let exit_code = fai_codegen::jit_run(&defs, entry, runtime, &namer, &arity, &abi, &borrows);
    RunOutcome { exit_code, diagnostics }
}

/// A compiled, finalized JIT image of the closure reachable from a file's `main`,
/// retained so a caller can fetch a top-level function's closure value and apply
/// it directly (through [`fai_runtime::apply`]) instead of only running `main`.
///
/// This drives a specific function in process — for example, to measure a
/// function's execution time apart from the cost of compiling it (compile once
/// via [`jit_compile`], then apply many times). The image is kept alive for as
/// long as the value lives, so the fetched closures stay callable.
pub struct CompiledProgram {
    program: JitProgram,
    /// The mangled backend symbol of every compiled definition (the namer that
    /// [`fai_codegen::JitProgram::closure_value`] needs).
    names: FxHashMap<DefId, String>,
    /// The entry file's own top-level definitions, by name. Restricted to that
    /// file so a bare name (e.g. `run`) is unambiguous against standard-library
    /// definitions reachable in the same image.
    entry_defs: FxHashMap<Symbol, DefId>,
}

impl CompiledProgram {
    /// The static-closure value of the entry file's top-level binding `name`,
    /// ready to apply via [`fai_runtime::apply`]. `None` if the file has no such
    /// binding (or it was unreachable from `main` and so not compiled).
    ///
    /// The returned value is a long-lived (immortal) static closure; applying it
    /// consumes one reference, so a caller that applies it repeatedly should
    /// [`fai_runtime::fai_dup`] it before each application.
    pub fn function(&mut self, name: Symbol) -> Option<i64> {
        let def = *self.entry_defs.get(&name)?;
        let Self { program, names, .. } = self;
        let namer = |d: DefId| names[&d].clone();
        Some(program.closure_value(&namer, def))
    }
}

/// Compiles the closure reachable from `file`'s `main` into a retained JIT image
/// (see [`CompiledProgram`]) without running it. `Err` carries the precompile
/// diagnostics (no `main`, or a reachable definition that failed to compile),
/// mirroring [`jit_run_program`]'s error path.
pub fn jit_compile(db: &dyn Db, file: SourceFile) -> Result<CompiledProgram, Vec<Diagnostic>> {
    if !has_main(db, file) {
        return Err(vec![no_entry_point()]);
    }
    // A `jit_compile` image is *fetchable* by name (see [`CompiledProgram::function`]),
    // so it compiles `main`'s closure plus the file's whole public API as additional
    // roots — a public binding stays a standalone function even when it is inlined
    // into (and so dead-code-eliminated from) `main`'s own closure. The minimal AOT
    // path ([`build_native`]) keeps the tighter main-only reachability.
    let source = file.source(db);
    let mut roots = vec![DefId::new(source, Symbol::intern(ENTRY))];
    if let Some(runtime) = runtime_root(db) {
        roots.push(runtime);
    }
    let mut public: Vec<DefId> = module_defs(db, file)
        .defs
        .iter()
        .filter(|d| d.visibility == Visibility::Public)
        .map(|d| DefId::new(source, d.name))
        .collect();
    public.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    roots.extend(public);
    let reachable = reachable_from_roots(db, &roots, &FxHashSet::default());
    let diagnostics = precompile_diagnostics(db, &reachable);
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err(diagnostics);
    }

    // Lower + reference-count (emit-ready, with reuse forwarding) each reachable
    // def in parallel (independent queries), as the JIT runner does, then build one
    // finalized image.
    let mut defs: Vec<LoweredDef> = reachable
        .par_iter()
        .map_with(db.clone_box(), |dbh, def| {
            let db: &dyn Db = &**dbh;
            db.source_file(def.file).map(|f| (*rc_emit(db, f, def.name)).clone())
        })
        .collect::<Vec<Option<LoweredDef>>>()
        .into_iter()
        .flatten()
        .collect();
    let targets: FxHashSet<DefId> = forward_targets(db, &reachable).into_iter().collect();
    for d in &mut defs {
        if !targets.contains(&d.def) {
            d.reuse_entry = None;
        }
    }

    let names: FxHashMap<DefId, String> =
        reachable.iter().map(|&d| (d, symbol_base(db, d))).collect();
    let arities: FxHashMap<DefId, usize> =
        reachable.iter().map(|&d| (d, arity_of(db, d))).collect();
    let entry_defs: FxHashMap<Symbol, DefId> =
        reachable.iter().filter(|d| d.file == source).map(|&d| (d.name, d)).collect();

    let namer = |d: DefId| names[&d].clone();
    let arity = |d: DefId| arities[&d];
    let abi = |d: DefId| abi_of(db, d);
    let borrows = |d: DefId| borrows_of(db, d);
    let program = JitProgram::compile(&defs, &namer, &arity, &abi, &borrows);
    Ok(CompiledProgram { program, names, entry_defs })
}

fn no_entry_point() -> Diagnostic {
    Diagnostic::error(
        NO_ENTRY_POINT,
        format!("no entry point: define `public {ENTRY} : Runtime -> Unit`"),
        tooling_span(),
    )
}

/// The outcome of building a run bundle: the portable program (if it compiled
/// cleanly) and any diagnostics that must be reported first.
#[derive(Debug, Clone)]
pub struct RunBundleResult {
    /// The serializable program, or `None` if there is no `main` or a reachable
    /// definition failed to compile.
    pub bundle: Option<WireBundle>,
    /// Diagnostics produced while preparing the bundle.
    pub diagnostics: Vec<Diagnostic>,
}

/// Builds a portable [`WireBundle`] for the closure reachable from `file`'s
/// `main`, ready to ship to an isolated worker. The front end runs here (warm in
/// the daemon); the worker only reconstructs and JITs.
#[must_use]
pub fn build_run_bundle(db: &dyn Db, file: SourceFile) -> RunBundleResult {
    if !has_main(db, file) {
        return RunBundleResult { bundle: None, diagnostics: vec![no_entry_point()] };
    }
    let reachable = reachable_defs(db, file);
    let diagnostics = precompile_diagnostics(db, &reachable);
    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return RunBundleResult { bundle: None, diagnostics };
    }

    // Flatten mutual-recursion groups (members → wrappers, plus a combined loop
    // per group), so the shipped bundle carries the flattened program.
    let groups = program_groups(db, &reachable);
    let members: FxHashSet<DefId> = groups.wrappers.keys().copied().collect();

    // Definitions a reachable caller forwards reuse tokens to: only these ship
    // their token-taking specialized entry (and its slot classes), so the worker
    // emits a reuse entry exactly where it is used.
    let targets: FxHashSet<DefId> = forward_targets(db, &reachable).into_iter().collect();

    // Lower + reference-count (emit-ready) + serialize each ordinary reachable def
    // in parallel (independent queries), preserving order so the bundle is
    // deterministic.
    let mut defs: Vec<WireDef> = reachable
        .par_iter()
        .map_with(db.clone_box(), |dbh, d| {
            if members.contains(d) {
                return None; // a group member ships as its wrapper, added below
            }
            let db: &dyn Db = &**dbh;
            let def_file = db.source_file(d.file)?;
            let module_of = |x: DefId| module_label(db, x);
            let lowered = rc_emit(db, def_file, d.name);
            // Ship the specialized entry and its slot classes only for a forward
            // target; otherwise drop it so the worker emits no unused entry.
            let (lowered, reuse_sig) = if targets.contains(d) {
                (lowered, reuse_signature(db, def_file, d.name).classes().to_vec())
            } else if lowered.reuse_entry.is_some() {
                let mut owned = (*lowered).clone();
                owned.reuse_entry = None;
                (Arc::new(owned), Vec::new())
            } else {
                (lowered, Vec::new())
            };
            Some(def_to_wire(&lowered, &module_of, arity_of(db, *d), abi_of(db, *d), reuse_sig))
        })
        .collect::<Vec<Option<WireDef>>>()
        .into_iter()
        .flatten()
        .collect();
    let module_of = |x: DefId| module_label(db, x);
    for (member, wrapper) in &groups.wrappers {
        // The wrapper is emitted at the member's symbol, so it presents the
        // member's native ABI to direct callers.
        defs.push(def_to_wire(
            wrapper,
            &module_of,
            arity_of(db, *member),
            abi_of(db, *member),
            Vec::new(),
        ));
    }
    for combined in &groups.combined {
        // The synthetic combined loop shares padded positional slots across members
        // (the uniform boxed representation), but it is **direct-called** by the
        // member wrappers, so it takes the register ABI with all-boxed slots.
        let arity = groups.arity.get(&combined.def).copied().unwrap_or(0);
        defs.push(def_to_wire(
            combined,
            &module_of,
            arity,
            FnAbi::register_uniform(arity),
            Vec::new(),
        ));
    }

    let entry = DefId::new(file.source(db), Symbol::intern(ENTRY));
    let runtime = runtime_root(db).expect("standard library defines the Runtime value binding");
    let bundle = WireBundle {
        entry: WireDefId { module: module_label(db, entry), name: ENTRY.to_owned() },
        runtime: WireDefId { module: module_label(db, runtime), name: RUNTIME_VALUE.to_owned() },
        defs,
    };
    RunBundleResult { bundle: Some(bundle), diagnostics }
}

/// Reconstructs a [`WireBundle`] and JIT-runs its entry, returning the exit code.
/// Runs in the (database-free) worker process; applies any requested resource
/// limits first.
#[must_use]
pub fn jit_run_bundle(bundle: &WireBundle) -> i32 {
    apply_run_limits();
    let rebuilt = from_wire(bundle);
    let labels = rebuilt.module_labels;
    let arities = rebuilt.arities;
    let abis = rebuilt.abis;
    // The bundle carries each definition's borrow flags (its `entry_borrowed`); a
    // direct caller reads them to release boxed scalar arguments lent to a borrowed
    // parameter.
    let borrows: FxHashMap<DefId, Vec<bool>> =
        rebuilt.defs.iter().map(|d| (d.def, d.entry_borrowed.clone())).collect();
    let namer = |d: DefId| mangle(labels.get(&d.file).map_or("M", String::as_str), d.name.as_str());
    let arity = |d: DefId| arities.get(&d).copied().unwrap_or(0);
    let abi = |d: DefId| abis.get(&d).cloned().unwrap_or_default();
    let borrow = |d: DefId| borrows.get(&d).cloned().unwrap_or_default();
    fai_codegen::jit_run(
        &rebuilt.defs,
        rebuilt.entry,
        rebuilt.runtime,
        &namer,
        &arity,
        &abi,
        &borrow,
    )
}

/// Applies self-imposed resource limits from the environment (set by the daemon
/// when supervising a run or test). A CPU-time limit (`FAI_RUN_CPU_SECS`,
/// seconds) is the default guard; a memory cap (`FAI_RUN_AS_BYTES`, bytes) is
/// opt-in. Enforced via `setrlimit` on Unix and a Job Object on Windows; a no-op
/// on other targets.
#[cfg(unix)]
pub(crate) fn apply_run_limits() {
    use nix::sys::resource::{Resource, setrlimit};
    if let Ok(secs) = std::env::var("FAI_RUN_CPU_SECS").map(|v| v.parse::<u64>())
        && let Ok(secs) = secs
    {
        let _ = setrlimit(Resource::RLIMIT_CPU, secs, secs);
    }
    if let Ok(bytes) = std::env::var("FAI_RUN_AS_BYTES").map(|v| v.parse::<u64>())
        && let Ok(bytes) = bytes
    {
        let _ = setrlimit(Resource::RLIMIT_AS, bytes, bytes);
    }
}

/// Assigns the current process to a new Job Object carrying the requested limits
/// — a per-process committed-memory cap (`FAI_RUN_AS_BYTES`) and/or a user-mode
/// CPU-time limit (`FAI_RUN_CPU_SECS`) — which the OS enforces by terminating the
/// process when either is exceeded (the peer of the Unix `setrlimit` path). The
/// job handle is intentionally left open: the assigned process keeps the job and
/// its limits alive, and the OS reclaims both on exit. Best-effort throughout — a
/// failure (e.g. an environment that forbids job nesting) just leaves the worker
/// unbounded by the job, exactly as a failed `setrlimit` would.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn apply_run_limits() {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
        JOB_OBJECT_LIMIT_PROCESS_TIME, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let cpu_secs = std::env::var("FAI_RUN_CPU_SECS").ok().and_then(|v| v.parse::<u64>().ok());
    let mem_bytes = std::env::var("FAI_RUN_AS_BYTES").ok().and_then(|v| v.parse::<u64>().ok());
    if cpu_secs.is_none() && mem_bytes.is_none() {
        return;
    }

    // SAFETY: a null attribute pointer and null name request a new, unnamed job
    // object; the call returns null on failure, which we treat as "no limits".
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return;
    }

    // SAFETY: the struct is plain-old-data (integers and nested integer structs),
    // so an all-zero value is a valid, limit-free starting point.
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    let mut flags = 0u32;
    if let Some(secs) = cpu_secs {
        // PerProcessUserTimeLimit counts user-mode time in 100-nanosecond ticks.
        info.BasicLimitInformation.PerProcessUserTimeLimit = secs.saturating_mul(10_000_000) as i64;
        flags |= JOB_OBJECT_LIMIT_PROCESS_TIME;
    }
    if let Some(bytes) = mem_bytes {
        info.ProcessMemoryLimit = bytes as usize;
        flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    }
    info.BasicLimitInformation.LimitFlags = flags;

    // SAFETY: `info` is fully initialized; we pass its address and exact byte
    // length for the matching extended-limit information class.
    let set = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set == 0 {
        return;
    }

    // SAFETY: GetCurrentProcess yields a pseudo-handle to this process; assigning
    // it to the configured job binds the limits to us.
    unsafe {
        AssignProcessToJobObject(job, GetCurrentProcess());
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn apply_run_limits() {}

/// Writes the objects and the runtime archive to a temporary directory and links
/// them into a native executable, returning the path actually produced (which
/// gains a `.exe` suffix on Windows). Uses the host's system linker — `cc` on
/// Unix, MSVC `link.exe` on Windows — with the runtime's required system
/// libraries (captured by `build.rs`).
fn link(objects: &[(String, Vec<u8>)], out: &Utf8Path) -> Result<Utf8PathBuf, String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "fai-build-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating build directory: {e}"))?;

    // MSVC's `link.exe` wants object inputs named `.obj`; Unix linkers accept any
    // extension. The bytes are host-native objects from Cranelift either way.
    let obj_ext = if cfg!(target_env = "msvc") { "obj" } else { "o" };
    let mut object_paths = Vec::with_capacity(objects.len());
    for (name, bytes) in objects {
        let path = dir.join(format!("{name}.{obj_ext}"));
        std::fs::write(&path, bytes).map_err(|e| format!("writing {}: {e}", path.display()))?;
        object_paths.push(path);
    }
    let archive_name =
        if cfg!(target_env = "msvc") { "fai_runtime.lib" } else { "libfai_runtime.a" };
    let archive = dir.join(archive_name);
    std::fs::write(&archive, RUNTIME_ARCHIVE)
        .map_err(|e| format!("writing runtime archive: {e}"))?;

    // Native executables need the platform's executable extension (`.exe` on
    // Windows, none elsewhere). Respect an extension the caller already gave.
    let exe_ext = std::env::consts::EXE_EXTENSION;
    let target = if !exe_ext.is_empty() && out.extension().is_none() {
        out.with_extension(exe_ext)
    } else {
        out.to_owned()
    };

    let native_libs: Vec<&str> = RUNTIME_NATIVE_LIBS.split_whitespace().collect();
    if cfg!(target_env = "msvc") {
        link_msvc(&object_paths, &archive, &target, &native_libs)?;
    } else {
        link_unix(&object_paths, &archive, &target, &native_libs)?;
    }
    Ok(target)
}

/// Links with a Unix C compiler driver (`$CC`, default `cc`), which supplies the
/// C runtime startup and resolves the runtime's system dependencies.
fn link_unix(
    objects: &[std::path::PathBuf],
    archive: &std::path::Path,
    out: &Utf8Path,
    native_libs: &[&str],
) -> Result<(), String> {
    let linker = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let mut command = std::process::Command::new(&linker);
    command.args(objects).arg(archive).arg("-o").arg(out.as_std_path());
    if native_libs.is_empty() {
        // Fall back to the historic Linux set if the toolchain reported nothing.
        if cfg!(target_os = "linux") {
            command.args(["-lpthread", "-ldl", "-lm"]);
        }
    } else {
        command.args(native_libs);
    }
    let status = command.status().map_err(|e| format!("invoking linker `{linker}`: {e}"))?;
    if !status.success() {
        return Err(format!("linker `{linker}` exited with {status}"));
    }
    Ok(())
}

/// Links with the MSVC linker (`link.exe`, overridable via `$FAI_LINKER`). The
/// runtime archive's objects carry `/DEFAULTLIB` directives for the C runtime, so
/// the CRT entry point (`mainCRTStartup`) finds the emitted `main`; the reported
/// Win32 import libraries cover the rest. Requires the MSVC environment (the
/// `LIB` paths) on `PATH`, as a normal Rust toolchain build already does.
fn link_msvc(
    objects: &[std::path::PathBuf],
    archive: &std::path::Path,
    out: &Utf8Path,
    native_libs: &[&str],
) -> Result<(), String> {
    let linker = std::env::var("FAI_LINKER").unwrap_or_else(|_| "link.exe".to_owned());
    let mut command = std::process::Command::new(&linker);
    command.arg("/NOLOGO").arg("/SUBSYSTEM:CONSOLE").arg(format!("/OUT:{out}"));
    command.args(objects).arg(archive).args(native_libs);
    let status = command.status().map_err(|e| format!("invoking linker `{linker}`: {e}"))?;
    if !status.success() {
        return Err(format!("linker `{linker}` exited with {status}"));
    }
    Ok(())
}

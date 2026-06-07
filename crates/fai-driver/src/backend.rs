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
use fai_codegen::{main_object, object_for_def};
use fai_core::core;
use fai_core::ir::LoweredDef;
use fai_core::wire::{WireBundle, WireDef, WireDefId, def_to_wire, from_wire};
use fai_db::{Db, Diag, SourceFile};
use fai_diagnostics::wire::{DiagnosticWire, to_wire};
use fai_diagnostics::{Diagnostic, SCHEMA_VERSION, Severity, render_human};
use fai_rc::rc;
use fai_resolve::{DefId, ModuleName, module_defs, module_name};
use fai_span::SpanResolver;
use fai_syntax::Symbol;
use fai_syntax::ast::ItemKind;
use rustc_hash::FxHashSet;
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

/// A definition's runtime arity: its source parameters plus the leading offset
/// evidence its (row-polymorphic) type requires. Read from the binding and the
/// signature, both body-edit-stable, so the codegen firewall stays intact.
#[salsa::tracked]
pub fn def_arity(db: &dyn Db, file: SourceFile, name: Symbol) -> usize {
    let parsed = fai_syntax::parse(db, file);
    let source_params = parsed
        .module
        .items
        .iter()
        .find_map(|it| match &it.kind {
            ItemKind::Binding { name: n, params, .. } if *n == name => Some(params.len()),
            _ => None,
        })
        .unwrap_or(0);
    let def = DefId::new(file.source(db), name);
    let evidence = fai_types::declared_or_inferred_scheme(db, def)
        .map_or(0, |scheme| fai_types::evidence_count(&scheme));
    source_params + evidence
}

pub(crate) fn arity_of(db: &dyn Db, def: DefId) -> usize {
    db.source_file(def.file).map_or(0, |f| def_arity(db, f, def.name))
}

/// The cached relocatable object for one definition (the content-addressed cache
/// unit; see [`build_native`]).
#[salsa::tracked]
pub fn object_code(db: &dyn Db, file: SourceFile, name: Symbol) -> Arc<Vec<u8>> {
    let lowered = rc(db, file, name);
    let namer = |d: DefId| symbol_base(db, d);
    let arity = |d: DefId| arity_of(db, d);
    Arc::new(object_for_def(&lowered, &namer, &arity))
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
            for callee in core(db, file, def.name).referenced_globals() {
                if !seen.contains(&callee) {
                    stack.push(callee);
                }
            }
        }
    }
    order
}

/// Collects the diagnostics that must be clean before codegen: each reachable
/// file's parse/resolve/type diagnostics plus each reachable definition's
/// lowering diagnostics (e.g. unsupported-construct `FAI7001`).
fn precompile_diagnostics(db: &dyn Db, reachable: &[DefId]) -> Vec<Diagnostic> {
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

    let mut objects: Vec<(String, Vec<u8>)> = Vec::with_capacity(reachable.len() + 1);
    for def in &reachable {
        let Some(def_file) = db.source_file(def.file) else { continue };
        let bytes = crate::cache::load_or_build_object(db, def_file, def.name);
        objects.push((symbol_base(db, *def), (*bytes).clone()));
    }
    let entry = DefId::new(file.source(db), Symbol::intern(ENTRY));
    let runtime = runtime_root(db).expect("standard library defines the Runtime value binding");
    objects.push(("fai_main".to_owned(), main_object(entry, runtime, &|d| symbol_base(db, d))));

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

    let defs: Vec<LoweredDef> = reachable
        .iter()
        .filter_map(|def| db.source_file(def.file).map(|f| (*rc(db, f, def.name)).clone()))
        .collect();
    let entry = DefId::new(file.source(db), Symbol::intern(ENTRY));
    let runtime = runtime_root(db).expect("standard library defines the Runtime value binding");
    let namer = |d: DefId| symbol_base(db, d);
    let arity = |d: DefId| arity_of(db, d);
    let exit_code = fai_codegen::jit_run(&defs, entry, runtime, &namer, &arity);
    RunOutcome { exit_code, diagnostics }
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

    let module_of = |d: DefId| module_label(db, d);
    let defs: Vec<WireDef> = reachable
        .iter()
        .filter_map(|d| {
            let def_file = db.source_file(d.file)?;
            let lowered = rc(db, def_file, d.name);
            Some(def_to_wire(&lowered, &module_of, arity_of(db, *d)))
        })
        .collect();
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
    let namer = |d: DefId| mangle(labels.get(&d.file).map_or("M", String::as_str), d.name.as_str());
    let arity = |d: DefId| arities.get(&d).copied().unwrap_or(0);
    fai_codegen::jit_run(&rebuilt.defs, rebuilt.entry, rebuilt.runtime, &namer, &arity)
}

/// Applies self-imposed resource limits from the environment (set by the daemon
/// when supervising a run). `RLIMIT_CPU` (seconds) is the default guard;
/// `RLIMIT_AS` (bytes) is opt-in. A no-op off Unix.
#[cfg(unix)]
fn apply_run_limits() {
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

#[cfg(not(unix))]
fn apply_run_limits() {}

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

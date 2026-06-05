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

/// The required entry-point name.
const ENTRY: &str = "main";

/// The mangled symbol base for a definition: `fai_<module>_<name>`.
#[must_use]
pub fn symbol_base(db: &dyn Db, def: DefId) -> String {
    let label = db
        .source_file(def.file)
        .and_then(|f| module_name(db, f))
        .map_or_else(|| "M".to_owned(), |ModuleName(s)| s.as_str().to_owned());
    let sanitized: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    format!("fai_{sanitized}_{}", def.name)
}

/// A definition's parameter count, read from its binding (body-edit-stable, so it
/// keeps the codegen firewall intact).
#[salsa::tracked]
pub fn def_arity(db: &dyn Db, file: SourceFile, name: Symbol) -> usize {
    let parsed = fai_syntax::parse(db, file);
    parsed
        .module
        .items
        .iter()
        .find_map(|it| match &it.kind {
            ItemKind::Binding { name: n, params, .. } if *n == name => Some(params.len()),
            _ => None,
        })
        .unwrap_or(0)
}

fn arity_of(db: &dyn Db, def: DefId) -> usize {
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
    let mut stack = vec![entry];
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
        let bytes = object_code(db, def_file, def.name);
        objects.push((symbol_base(db, *def), (*bytes).clone()));
    }
    let entry = DefId::new(file.source(db), Symbol::intern(ENTRY));
    objects.push(("fai_main".to_owned(), main_object(entry, &|d| symbol_base(db, d))));

    match link(&objects, out) {
        Ok(()) => BuildOutcome { artifact: Some(out.to_owned()), diagnostics, ok: true },
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
    let namer = |d: DefId| symbol_base(db, d);
    let arity = |d: DefId| arity_of(db, d);
    let exit_code = fai_codegen::jit_run(&defs, entry, &namer, &arity);
    RunOutcome { exit_code, diagnostics }
}

fn no_entry_point() -> Diagnostic {
    Diagnostic::error(
        NO_ENTRY_POINT,
        format!("no entry point: define `public {ENTRY} : Runtime -> Unit`"),
        tooling_span(),
    )
}

/// Writes the objects and the runtime archive to a temporary directory and links
/// them into `out` with the system C compiler.
fn link(objects: &[(String, Vec<u8>)], out: &Utf8Path) -> Result<(), String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "fai-build-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating build directory: {e}"))?;

    let mut object_paths = Vec::with_capacity(objects.len());
    for (name, bytes) in objects {
        let path = dir.join(format!("{name}.o"));
        std::fs::write(&path, bytes).map_err(|e| format!("writing {}: {e}", path.display()))?;
        object_paths.push(path);
    }
    let archive = dir.join("libfai_runtime.a");
    std::fs::write(&archive, RUNTIME_ARCHIVE)
        .map_err(|e| format!("writing runtime archive: {e}"))?;

    let linker = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let status = std::process::Command::new(&linker)
        .args(&object_paths)
        .arg(&archive)
        .arg("-o")
        .arg(out.as_std_path())
        .args(["-lpthread", "-ldl", "-lm"])
        .status()
        .map_err(|e| format!("invoking linker `{linker}`: {e}"))?;
    if !status.success() {
        return Err(format!("linker `{linker}` exited with {status}"));
    }
    Ok(())
}

//! A fixed, multi-module sample application used to benchmark language-server
//! operations on realistic code.
//!
//! The modules live under `samples/` (so they are verified by the sample test
//! suite and stay green), and form a small store: `Catalog` (types + an ADT),
//! `Pricing` (a discount ADT over `Catalog.Product`), `Inventory` (record
//! update), `Orders` (cross-module composition), and `Storefront` (an interface
//! instance + a capability `main`). The cross-module references give the
//! language-server benches genuine go-to-definition / find-references / rename
//! targets across files.
//!
//! Each [`Op`] resolves to one or more [`Probe`]s — a byte offset for the
//! offset-addressed `fai-ide` calls plus the 0-based line/column for the LSP
//! wire. A `Probe`'s [`Display`](std::fmt::Display) is `"<path>#L<line>"`, so a
//! divan argument built from it carries the exact source location into the
//! rendered report as a link.

use std::collections::BTreeMap;
use std::fmt;

use camino::Utf8PathBuf;
use fai_db::{Db, FaiDatabase, SourceFile};

/// The app's modules, in dependency order (repo-relative paths under `samples/`).
pub const APP_FILES: &[&str] = &[
    "samples/Catalog.fai",
    "samples/Pricing.fai",
    "samples/Inventory.fai",
    "samples/Orders.fai",
    "samples/Storefront.fai",
];

/// A language-server operation the real-world benches measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `textDocument/hover` — the type at a reference.
    Hover,
    /// `textDocument/definition` — resolve a reference to its definition.
    Definition,
    /// `textDocument/references` — every use of a symbol (cross-file).
    References,
    /// `textDocument/rename` — the workspace edit renaming a symbol.
    Rename,
    /// `textDocument/completion` — candidates at a cursor.
    Completion,
    /// `textDocument/signatureHelp` — the active call's signature.
    SignatureHelp,
    /// `textDocument/documentSymbol` — a file's symbol outline.
    DocumentSymbols,
    /// The edit → `publishDiagnostics` loop over one file.
    Diagnostics,
}

/// A probe position in a real source file.
///
/// `Display` renders `"<path>#L<line>"` (1-based) so the divan argument links to
/// the exact source line in the report.
#[derive(Debug, Clone)]
pub struct Probe {
    /// Repo-relative path (e.g. `samples/Orders.fai`).
    pub path: &'static str,
    /// Byte offset of the probed token (for the `fai-ide` query calls).
    pub offset: u32,
    /// 0-based line of the probe (LSP `position.line`).
    pub line: u32,
    /// 0-based column of the probe (the fixtures are ASCII, so byte == column).
    pub character: u32,
}

impl fmt::Display for Probe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 1-based line for a GitHub `#Lnn` anchor.
        write!(f, "{}#L{}", self.path, self.line + 1)
    }
}

/// The directory the app modules live in.
fn samples_dir() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

/// Reads an app module's source from disk by its repo-relative path.
#[must_use]
pub fn read_file(path: &str) -> String {
    let name = path.strip_prefix("samples/").unwrap_or(path);
    std::fs::read_to_string(samples_dir().join(name))
        .unwrap_or_else(|e| panic!("reading real-world fixture {path}: {e}"))
}

/// The app's `(repo-relative path, source)` pairs, in dependency order.
#[must_use]
pub fn app_sources() -> Vec<(&'static str, String)> {
    APP_FILES.iter().map(|&path| (path, read_file(path))).collect()
}

/// Builds a database with the embedded standard library plus the app modules
/// loaded (keyed by their repo-relative path), warmed and ready to query.
#[must_use]
pub fn load_app() -> (FaiDatabase, BTreeMap<&'static str, SourceFile>) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let mut files = BTreeMap::new();
    for (path, source) in app_sources() {
        let id = db.add_source(Utf8PathBuf::from(path), source);
        files.insert(path, db.source_file(id).unwrap());
    }
    (db, files)
}

/// A new source for an app file with a trailing comment appended — a trivia edit
/// that forces a re-parse and re-check of just that file (the edit → diagnostics
/// loop), leaving every other module cached behind the cross-module firewall.
#[must_use]
pub fn edit(path: &str, revision: u32) -> String {
    let mut src = read_file(path);
    src.push_str(&format!("\n// bench revision {revision}\n"));
    src
}

/// The 0-based `(line, column)` of byte `offset` in `src` (ASCII fixtures).
fn line_col(src: &str, offset: usize) -> (u32, u32) {
    let line = src[..offset].matches('\n').count() as u32;
    let column = offset - src[..offset].rfind('\n').map_or(0, |i| i + 1);
    (line, column as u32)
}

/// The probe at the `occurrence`-th (0-based) appearance of `needle` in `path`.
fn probe_at(path: &'static str, needle: &str, occurrence: usize) -> Probe {
    let src = read_file(path);
    let mut search = 0;
    let mut offset = None;
    for _ in 0..=occurrence {
        let rel = src[search..]
            .find(needle)
            .unwrap_or_else(|| panic!("{path}: probe token {needle:?} (#{occurrence}) not found"));
        offset = Some(search + rel);
        search += rel + needle.len();
    }
    let offset = offset.unwrap();
    let (line, character) = line_col(&src, offset);
    Probe { path, offset: offset as u32, line, character }
}

/// The probe position(s) for `op`. The tokens are distinctive substrings of the
/// fixtures, so the positions stay stable across reformatting.
#[must_use]
pub fn probes(op: Op) -> Vec<Probe> {
    match op {
        // A cross-module reference: hover shows its type, definition jumps to
        // `Pricing.priceOf`.
        Op::Hover | Op::Definition => vec![probe_at("samples/Orders.fai", "Pricing.priceOf", 0)],
        // `priceOf` at its definition: referenced in `Orders`, so this spans files.
        Op::References | Op::Rename => vec![probe_at("samples/Pricing.fai", "priceOf", 0)],
        // Just after `Int.` — member completion on a qualified name.
        Op::Completion => vec![probe_at("samples/Orders.fai", "toFloat", 0)],
        // Inside the `Pricing.priceOf <args>` application.
        Op::SignatureHelp => vec![probe_at("samples/Orders.fai", "line.product", 0)],
        // Position-independent outline of a type-rich module.
        Op::DocumentSymbols => vec![probe_at("samples/Catalog.fai", "module", 0)],
        // The file whose body the diagnostics loop edits.
        Op::Diagnostics => vec![probe_at("samples/Orders.fai", "module", 0)],
    }
}

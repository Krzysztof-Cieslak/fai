//! A fixed, multi-module sample **store application** used to benchmark
//! language-server operations on realistic code.
//!
//! The modules live under `samples/store/` (verified by the `store_app` test
//! suite, so they stay green) and form a small layered store: a `Catalog`
//! **hub** (types, an ADT, and the widely-referenced `label`) that most modules
//! depend on; foundation modules (`Money`, `Address`, `Customer`); domain
//! modules (`Pricing`, `Inventory`, `Tax`, `Shipping`, `Coupon`, `Warehouse`);
//! services (`Orders`, `Checkout`, `Reporting`); and a `Storefront` (an
//! interface instance + a capability `main`). The cross-module references give
//! the language-server benches genuine go-to-definition / find-references /
//! rename targets across files, and the hub's dependency fan-out drives the
//! cross-module propagation scenarios.
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

/// The app's modules, in dependency order (repo-relative paths under
/// `samples/store/`).
pub const APP_FILES: &[&str] = &[
    "samples/store/Money.fai",
    "samples/store/Address.fai",
    "samples/store/Catalog.fai",
    "samples/store/Customer.fai",
    "samples/store/Pricing.fai",
    "samples/store/Inventory.fai",
    "samples/store/Tax.fai",
    "samples/store/Shipping.fai",
    "samples/store/Coupon.fai",
    "samples/store/Warehouse.fai",
    "samples/store/Orders.fai",
    "samples/store/Checkout.fai",
    "samples/store/Reporting.fai",
    "samples/store/Storefront.fai",
];

/// The hub module: `Catalog.Product`/`Category`/`label` are referenced by most
/// of the app, so editing its public API forces a wide cross-module re-check
/// (the propagation scenarios) rather than the firewalled recompute of a private
/// edit.
pub const HUB_FILE: &str = "samples/store/Catalog.fai";

/// The file the `completionItem/resolve` bench resolves documentation for, and
/// the local definition name within it — the documented `Catalog.label` (its
/// `///` doc plus type). Docs are looked up per declaring module, so the file is
/// the hub and the name is unqualified.
pub const COMPLETION_RESOLVE_FILE: &str = "samples/store/Catalog.fai";
/// The definition name resolved by the `completionItem/resolve` bench (see
/// [`COMPLETION_RESOLVE_FILE`]).
pub const COMPLETION_RESOLVE_NAME: &str = "label";

/// The file the code-action scenario edits to introduce a fixable diagnostic.
pub const CODE_ACTION_FILE: &str = "samples/store/Orders.fai";

/// A `workspace/symbol` query matching several user symbols across the app
/// (`total`, `totalOnHand`, …).
pub const WORKSPACE_QUERY: &str = "total";

/// A language-server operation the real-world benches measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `textDocument/hover` — the type (and `///` doc) at a reference.
    Hover,
    /// `textDocument/definition` — resolve a reference to its definition.
    Definition,
    /// `textDocument/references` — every use of a symbol (cross-file).
    References,
    /// `textDocument/rename` — the workspace edit renaming a symbol.
    Rename,
    /// `textDocument/prepareRename` — the renameable range under the cursor.
    PrepareRename,
    /// `textDocument/completion` — candidates at a cursor.
    Completion,
    /// `textDocument/signatureHelp` — the active call's signature.
    SignatureHelp,
    /// `textDocument/documentSymbol` — a file's symbol outline.
    DocumentSymbols,
    /// `textDocument/semanticTokens/full` — a file's semantic-token stream.
    SemanticTokens,
    /// `textDocument/inlayHint` — inferred-type hints over a file.
    InlayHints,
    /// `textDocument/formatting` (and range/on-type) — canonical formatting.
    Formatting,
    /// `textDocument/codeAction` — quick fixes for a range.
    CodeAction,
    /// The edit → `publishDiagnostics` loop over one file.
    Diagnostics,
}

/// A probe position in a real source file.
///
/// `Display` renders `"<path>#L<line>"` (1-based) so the divan argument links to
/// the exact source line in the report.
#[derive(Debug, Clone)]
pub struct Probe {
    /// Repo-relative path (e.g. `samples/store/Orders.fai`).
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

/// A new source for the hub module with the widely-used public `label`'s
/// signature changed (an added parameter) — a breaking API edit that forces
/// every module referencing `Catalog.label` to re-check, exercising the
/// cross-module fan-out (contrast [`edit`], a firewalled trivia edit). `revision`
/// varies the added parameter type so successive edits differ and each forces a
/// genuine re-propagation.
#[must_use]
pub fn edit_hub_signature(revision: u32) -> String {
    let src = read_file(HUB_FILE);
    let added = if revision.is_multiple_of(2) { "String" } else { "Int" };
    src.replacen(
        "public label : Product -> String",
        &format!("public label : Product -> {added} -> String"),
        1,
    )
}

/// A new source for [`CODE_ACTION_FILE`] with an appended binding that uses
/// `label` unqualified — an unbound name (the qualified form is `Catalog.label`),
/// so the server offers a "qualify as `Catalog.label`" quick fix over it. Returns
/// the new source and the 0-based `(line, column)` of the unbound `label`, so the
/// code-action scenario can request actions at exactly that position.
#[must_use]
pub fn edit_unbound_name() -> (String, u32, u32) {
    let mut src = read_file(CODE_ACTION_FILE);
    src.push_str("\npublic labelOf : Line -> String\nlet labelOf line = label line.product\n");
    let offset = src.rfind("label line.product").expect("appended unbound use of `label`");
    let (line, character) = line_col(&src, offset);
    (src, line, character)
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
        // A cross-module reference: hover shows its type (and `///` doc),
        // definition jumps to `Pricing.priceOf`.
        Op::Hover | Op::Definition => {
            vec![probe_at("samples/store/Orders.fai", "Pricing.priceOf", 0)]
        }
        // `label` at its definition (the hot symbol): referenced by `Warehouse`,
        // `Checkout`, `Reporting`, and `Storefront`, so this spans files.
        Op::References | Op::Rename | Op::PrepareRename => {
            vec![probe_at("samples/store/Catalog.fai", "label product", 0)]
        }
        // Just after `Int.` — member completion on a qualified name.
        Op::Completion => vec![probe_at("samples/store/Orders.fai", "toFloat", 0)],
        // Inside the `Pricing.priceOf <args>` application.
        Op::SignatureHelp => vec![probe_at("samples/store/Orders.fai", "line.product", 0)],
        // Position-independent outline of a type-rich module.
        Op::DocumentSymbols => vec![probe_at("samples/store/Catalog.fai", "module", 0)],
        // Whole-file features over a token- and binder-rich module (`Checkout`
        // has records, pipes, lambdas, and several inferred `let` binders).
        Op::SemanticTokens | Op::InlayHints | Op::Formatting => {
            vec![probe_at("samples/store/Checkout.fai", "module", 0)]
        }
        // A clean cursor position (no diagnostic overlaps): the common
        // editor-polls-and-finds-nothing path.
        Op::CodeAction => vec![probe_at("samples/store/Orders.fai", "lineTotal", 0)],
        // The file whose body the diagnostics loop edits.
        Op::Diagnostics => vec![probe_at("samples/store/Orders.fai", "module", 0)],
    }
}

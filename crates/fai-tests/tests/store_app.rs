//! The `samples/store/` corpus is a small, multi-module store application used by
//! the language-server benchmarks (via [`fai_corpus::realworld`]): a `Catalog`
//! hub that many modules depend on, a widely-referenced `Catalog.label`, and a
//! layered graph (foundation → domain → services → presentation) that gives the
//! benches realistic cross-module go-to-definition / references / rename targets
//! and a real dependency fan-out for the cross-module propagation scenarios.
//!
//! It lives in a subdirectory the top-level `samples` tests skip (their traversal
//! is non-recursive), so this is its dedicated coverage: every module must parse
//! cleanly and be canonically formatted, and the app must typecheck as one
//! workspace with no errors.

use std::path::{Path, PathBuf};

use camino::Utf8PathBuf;
use fai_corpus::realworld::{self, Op};
use fai_db::{Db, Diag, FaiDatabase};
use fai_diagnostics::Severity;
use fai_span::SourceId;
use fai_syntax::parse_module;

fn store_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/store")
}

/// The store app's module files, sorted for determinism.
fn store_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(store_dir())
        .expect("samples/store/ directory exists")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("fai"))
        .collect();
    files.sort();
    files
}

/// Canonical formatting of `src`.
fn fmt(src: &str) -> String {
    let parsed = parse_module(SourceId::new(0), src);
    fai_fmt::format(&parsed.module, &parsed.comments, src)
}

#[test]
fn store_modules_parse_clean_and_are_canonically_formatted() {
    let files = store_files();
    assert!(files.len() >= 10, "expected the store app's modules under samples/store/");
    for path in files {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_module(SourceId::new(0), &src);
        let codes: Vec<&str> = parsed.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.is_empty(), "store/{name} should parse cleanly, got {codes:?}");
        let formatted = fai_fmt::format(&parsed.module, &parsed.comments, &src);
        assert_eq!(formatted, src, "store/{name} is not canonically formatted (run `fai fmt`)");
        assert_eq!(fmt(&formatted), formatted, "store/{name} formatting is not idempotent");
    }
}

#[test]
fn store_app_typechecks_with_no_errors() {
    // Load every store module into one workspace (keyed by repo-relative path, as
    // `fai_corpus::realworld` does), plus the embedded standard library, so the
    // cross-module references resolve.
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);

    let mut handles: Vec<(String, fai_db::SourceFile)> = Vec::new();
    for path in store_files() {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        let id = db.add_source(Utf8PathBuf::from(format!("samples/store/{name}")), src);
        handles.push((name, db.source_file(id).unwrap()));
    }

    for (name, file) in &handles {
        let source = file.source(&db);
        let mut codes: Vec<String> = Vec::new();
        for d in fai_resolve::resolve::accumulated::<Diag>(&db, *file) {
            if d.0.primary.source() == source && d.0.severity == Severity::Error {
                codes.push(d.0.code.as_str().to_owned());
            }
        }
        for d in fai_types::check_file::accumulated::<Diag>(&db, *file) {
            if d.0.primary.source() == source && d.0.severity == Severity::Error {
                codes.push(d.0.code.as_str().to_owned());
            }
        }
        assert!(codes.is_empty(), "store/{name} should typecheck with no errors, got {codes:?}");
    }
}

/// The real-world language-server benches address the app by [`realworld::Op`]
/// probes (distinctive substrings). If a fixture edit moves or removes a probed
/// token, `probe_at` panics — but only when the benches run. This guard catches
/// it in the ordinary test run: every op's probe must resolve to an in-bounds
/// offset, and the propagation / code-action edit helpers must produce their
/// expected mutations.
#[test]
fn realworld_probes_and_edits_are_live() {
    let ops = [
        Op::Hover,
        Op::Definition,
        Op::References,
        Op::Rename,
        Op::PrepareRename,
        Op::Completion,
        Op::SignatureHelp,
        Op::DocumentSymbols,
        Op::SemanticTokens,
        Op::InlayHints,
        Op::Formatting,
        Op::CodeAction,
        Op::Diagnostics,
    ];
    for op in ops {
        let probes = realworld::probes(op);
        assert!(!probes.is_empty(), "{op:?} should have at least one probe");
        for p in probes {
            let src = realworld::read_file(p.path);
            assert!((p.offset as usize) < src.len(), "{op:?} probe offset out of bounds");
        }
    }

    // The hub-signature edit changes the source (drives cross-module propagation).
    assert_ne!(
        realworld::edit_hub_signature(0),
        realworld::read_file(realworld::HUB_FILE),
        "hub-signature edit should change the source"
    );
    // The code-action edit introduces the unbound `label` use.
    let (unbound, _, _) = realworld::edit_unbound_name();
    assert!(unbound.contains("label line.product"), "code-action edit adds an unbound `label`");
}

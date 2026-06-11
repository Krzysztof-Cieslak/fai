//! Runs the `.fai` fixture corpus through the type checker.
//!
//! `tests/fixtures/typed/` holds bigger, real-world-ish programs that must
//! typecheck clean (with inline `//~ TYPE` assertions); `tests/fixtures/errors/`
//! holds programs that must report specific diagnostics (`//~ ERROR`, `//~ COUNT`,
//! …). The expectation format lives in `fai_tests::run_annotated`.

use std::path::{Path, PathBuf};

use fai_tests::run_annotated;

fn fixtures(subdir: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(subdir);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {dir:?}: {e}"))
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("fai"))
        .collect();
    files.sort();
    files
}

#[test]
fn typed_fixtures_typecheck_clean() {
    let files = fixtures("typed");
    assert!(!files.is_empty(), "expected typed fixtures");
    for path in files {
        let label = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        run_annotated(&label, &src);
    }
}

#[test]
fn native_fixtures_typecheck_clean() {
    // The runnable fixtures are output-checked elsewhere; they must also
    // typecheck clean, which (with effects required on public) includes
    // declaring the capabilities each program uses.
    let files = fixtures("native");
    assert!(!files.is_empty(), "expected native fixtures");
    for path in files {
        let label = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        run_annotated(&label, &src);
    }
}

#[test]
fn error_fixtures_report_expected_diagnostics() {
    let files = fixtures("errors");
    assert!(!files.is_empty(), "expected error fixtures");
    for path in files {
        let label = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        run_annotated(&label, &src);
    }
}

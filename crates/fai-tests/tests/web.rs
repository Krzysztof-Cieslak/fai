//! Integration coverage for the `packages/web` micro web framework (a library
//! built on the networking stack, not part of the embedded standard library). It
//! is compiled exactly as user code: a `Session` rooted at the package directory
//! loads every `.fai` file under it alongside the embedded std. The framework's
//! own behaviour is covered in-language by its `example` contracts (run here); the
//! end-to-end server is exercised by `examples/Main.fai` under `fai run`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use camino::Utf8PathBuf;
use fai_db::Diag;
use fai_diagnostics::Severity;
use fai_driver::{Session, TestConfig, test};
use fai_span::SourceId;

/// Contract execution allocates through the runtime's process-global object
/// counter, so the leak guard is only meaningful when one run is in flight.
static RUN_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn package_dir() -> Utf8PathBuf {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/web");
    let path = path.canonicalize().expect("packages/web exists");
    Utf8PathBuf::from_path_buf(path).expect("utf8 path")
}

fn session() -> Session {
    Session::open(package_dir()).expect("open packages/web workspace")
}

/// Every framework, example, and spec file is canonically formatted (so the
/// package stays `fai fmt`-clean, like `samples/` and `std/`).
#[test]
fn web_package_is_canonically_formatted() {
    let session = session();
    let db = session.db();
    let files = session.user_files();
    assert!(!files.is_empty(), "expected .fai files under packages/web");
    for file in files {
        let path = file.path(db);
        let src = file.text(db);
        let parsed = fai_syntax::parse_module(SourceId::new(0), src.as_str());
        let codes: Vec<&str> = parsed.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.is_empty(), "{path} has parse errors: {codes:?}");
        let formatted = fai_fmt::format(&parsed.module, &parsed.comments, src.as_str());
        assert_eq!(formatted, src.as_str(), "{path} is not canonically formatted (run `fai fmt`)");
    }
}

/// Every file typechecks with no resolution or type errors.
#[test]
fn web_package_typechecks_clean() {
    let session = session();
    let db = session.db();
    let files = session.user_files();
    assert!(!files.is_empty(), "expected .fai files under packages/web");
    for file in files {
        let path = file.path(db);
        let source = file.source(db);
        let mut codes: Vec<String> = Vec::new();
        for d in fai_resolve::resolve::accumulated::<Diag>(db, file) {
            if d.0.primary.source() == source && d.0.severity == Severity::Error {
                codes.push(d.0.code.as_str().to_owned());
            }
        }
        for d in fai_types::check_file::accumulated::<Diag>(db, file) {
            if d.0.primary.source() == source && d.0.severity == Severity::Error {
                codes.push(d.0.code.as_str().to_owned());
            }
        }
        assert!(codes.is_empty(), "{path} should typecheck with no errors, got {codes:?}");
    }
}

/// Every `example`/`forall` contract across the package runs and passes (the core
/// handler laws and the full routing behaviour over mock requests).
#[test]
fn web_package_contracts_pass() {
    let _g = lock();
    let session = session();
    let db = session.db();
    let files = session.user_files();
    let outcome = test(db, &files, None, TestConfig::default());
    for d in &outcome.diagnostics {
        if d.code.as_str().starts_with("FAI6") {
            let help = d.help.as_deref().map_or(String::new(), |h| format!(" ({h})"));
            println!("    [{}] {}{help}", d.code, d.message);
        }
    }
    let failed = outcome.total - outcome.passed - outcome.not_run;
    assert!(outcome.total > 0, "expected contracts in packages/web");
    assert_eq!(failed, 0, "no web-package contract should fail");
    assert_eq!(outcome.not_run, 0, "every web-package contract should be runnable");
    assert_eq!(outcome.leaked, 0, "web-package contracts leaked objects");
    assert!(outcome.ok, "web-package contracts should pass");
}

//! Integration coverage for the `packages/web` micro web framework (a library
//! built on the networking stack, not part of the embedded standard library). It
//! is compiled exactly as user code: a `Session` rooted at `packages/` loads the
//! framework and its source dependencies alongside the embedded std. The framework's
//! own behaviour is covered in-language by its `example` contracts (run here); the
//! end-to-end server is exercised by `examples/Main.fai` under `fai run`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use camino::Utf8PathBuf;
use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_diagnostics::Severity;
use fai_driver::{Session, TestConfig, test};
use fai_span::SourceId;

/// Contract execution allocates through the runtime's process-global object
/// counter, so the leak guard is only meaningful when one run is in flight.
static RUN_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn packages_dir() -> Utf8PathBuf {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages");
    let path = path.canonicalize().expect("packages exists");
    Utf8PathBuf::from_path_buf(path).expect("utf8 path")
}

fn session() -> Session {
    Session::open(packages_dir()).expect("open source-package workspace")
}

fn web_files(session: &Session) -> Vec<SourceFile> {
    session
        .user_files()
        .into_iter()
        .filter(|file| file.path(session.db()).starts_with("web"))
        .collect()
}

/// Load source packages at runtime so library edits need no Rust recompilation.
fn load_framework(db: &mut FaiDatabase) {
    for package in ["json", "web"] {
        let mut files: Vec<_> = std::fs::read_dir(packages_dir().join(package).join("src"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "fai"))
            .collect();
        files.sort();
        for file in files {
            let name = file.file_name().unwrap().to_str().unwrap();
            db.add_source(
                format!("{package}/{name}").into(),
                std::fs::read_to_string(&file).unwrap(),
            );
        }
    }
}

/// Every framework, example, and spec file is canonically formatted (so the
/// package stays `fai fmt`-clean, like `samples/` and `std/`).
#[test]
fn web_package_is_canonically_formatted() {
    let session = session();
    let db = session.db();
    let files = web_files(&session);
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
    let files = web_files(&session);
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
    let files = web_files(&session);
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

#[test]
fn middleware_headers_reach_the_wire_through_a_router() {
    let _guard = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    load_framework(&mut db);
    let source = r#"module Main
app : Web.HttpHandler 'e
let app = Web.chain [
  Web.setHeader "X-Middleware" "kept",
  Web.addHeader "Set-Cookie" "a=1",
  Web.addHeader "Set-Cookie" "b=2",
  Router.router (Web.notFound "missing") [Router.get [Router.route "/" (Web.text "ok")]]
]
fetch : Runtime -> Int -> String / { Net, Tls }
let fetch r port =
  match Http.get r ("http://127.0.0.1:" ++ Int.toString port ++ "/") with
  | Err e -> e
  | Ok response ->
    let marker = Option.withDefault "lost" (Headers.get "X-Middleware" response.headers)
    let cookies = String.join "," (Headers.getAll "Set-Cookie" response.headers)
    let body = Result.withDefault "body failed" (Http.bodyText response.body)
    marker ++ "|" ++ cookies ++ "|" ++ body
serveThenFetch : Runtime -> Listener -> Int -> Nursery -> Unit / { Concurrency, Console, Net, Tls }
let serveThenFetch r listener port nursery =
  let server = r.concurrency.spawn nursery (fun u -> Web.serveListener r listener app)
  let result = fetch r port
  let stopped = r.concurrency.cancel server
  r.console.writeLine result
public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
let main r =
  match r.net.listen 0 with
  | Err e -> r.console.writeLine e
  | Ok listener -> r.concurrency.scope (serveThenFetch r listener (r.net.localPort listener))
"#;
    let id = db.add_source("Main.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, file);
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "kept|a=1,b=2|ok\n");
}

#[test]
fn decoded_redirect_input_cannot_write_injected_headers() {
    let _guard = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    load_framework(&mut db);
    let source = r#"module Main
app : Web.HttpHandler 'e
let app = Web.redirect (Url.decodeComponent "/safe%0D%0AX-Injected:%20yes")
fetch : Runtime -> Int -> String / { Net }
let fetch r port =
  match r.net.connect "127.0.0.1" port with
  | Err e -> e
  | Ok connection ->
    match r.net.send connection (Bytes.fromString "GET / HTTP/1.1\r\nHost: localhost\r\n\r\n") with
    | Err e -> e
    | Ok sent ->
      match r.net.recv connection 4096 with
      | Err e -> e
      | Ok bytes -> if Bytes.isEmpty bytes then "rejected" else "wrote bytes"
serveThenFetch : Runtime -> Listener -> Int -> Nursery -> Unit / { Concurrency, Console, Net, Tls }
let serveThenFetch r listener port nursery =
  let server = r.concurrency.spawn nursery (fun u -> Web.serveListener r listener app)
  let result = fetch r port
  let stopped = r.concurrency.cancel server
  r.console.writeLine result
public main : Runtime -> Unit / { Concurrency, Console, Net, Tls }
let main r =
  match r.net.listen 0 with
  | Err e -> r.console.writeLine e
  | Ok listener -> r.concurrency.scope (serveThenFetch r listener (r.net.localPort listener))
"#;
    let id = db.add_source("Main.fai".into(), source.into());
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "rejected\n");
}

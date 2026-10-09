//! Native resource identity agrees across operators, aggregates, and hash containers.

use fai_db::Db;
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

#[track_caller]
fn check(effects: &str, body: &str, helpers: &str) {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let body: String = body.lines().map(|l| format!("  {l}\n")).collect();
    let source = format!(
        "module Main\nlet same a b = a = b && compare a b = 0 && HashSet.member b (HashSet.singleton a)\nlet distinct a b = a <> b && compare a b <> 0 && not (HashSet.member b (HashSet.singleton a))\n{helpers}\ncheck : Runtime -> Bool / {{ {effects} }}\nlet check r =\n{body}\npublic main : Runtime -> Unit / {{ Console, {effects} }}\nlet main r = r.console.writeLine (if check r then \"ok\" else \"failed\")\n"
    );
    let id = db.add_source("Main.fai".into(), source);
    fai_runtime::capture_start();
    let outcome = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(outcome.exit_code, 0, "{:?}", outcome.diagnostics);
    assert_eq!(output, "ok\n");
}

#[test]
fn channels_use_resource_identity_in_aggregates_and_sets() {
    check(
        "Concurrency",
        "let a = r.concurrency.channel 1\nlet b = r.concurrency.channel 1\nlet sent = r.concurrency.send a 42\nsame a a && distinct a b && same { value = a } { value = a } && same [a] [a]",
        "",
    );
}

#[test]
fn nurseries_use_resource_identity() {
    check("Concurrency", "r.concurrency.scope (fun n -> same n n)", "");
}

#[test]
fn completed_tasks_keep_their_identity() {
    check(
        "Concurrency",
        "r.concurrency.scope (taskCheck r.concurrency)",
        "taskCheck : Concurrency -> Nursery -> Bool / { Concurrency }\nlet taskCheck c n =\n  let a = c.spawn n (fun u -> 42)\n  let b = c.spawn n (fun u -> 42)\n  let value = c.await a\n  same a a && distinct a b && value = 42\n",
    );
}

#[test]
fn network_listeners_use_resource_identity() {
    check(
        "Net",
        "match (r.net.listen 0, r.net.listen 0) with\n| (Ok a, Ok b) -> same a a && distinct a b\n| _ -> false",
        "",
    );
}

#[test]
fn udp_sockets_use_resource_identity() {
    check(
        "Net",
        "match (r.net.udpBind 0, r.net.udpBind 0) with\n| (Ok a, Ok b) -> same a a && distinct a b\n| _ -> false",
        "",
    );
}

#[test]
fn connected_sockets_use_resource_identity() {
    check(
        "Net",
        "match r.net.listen 0 with\n| Err e -> false\n| Ok listener ->\n  match r.net.connect \"127.0.0.1\" (r.net.localPort listener) with\n  | Err e -> false\n  | Ok a ->\n    match r.net.accept listener with\n    | Err e -> false\n    | Ok b -> same a a && distinct a b",
        "",
    );
}

#[test]
fn tls_sessions_use_resource_identity() {
    check(
        "Tls",
        "match (r.tls.client \"localhost\", r.tls.client \"localhost\") with\n| (Ok a, Ok b) -> same a a && distinct a b\n| _ -> false",
        "",
    );
}

#[test]
fn file_readers_and_writers_keep_their_identity() {
    let path = std::env::temp_dir().join(format!("fai-handle-identity-{}", std::process::id()));
    std::fs::write(&path, "data").unwrap();
    let path_text = path.to_string_lossy();
    check(
        "FileSystem",
        &format!(
            "match (r.fs.openRead {path_text:?}, r.fs.openRead {path_text:?}, r.fs.openAppend {path_text:?}) with\n| (Ok a, Ok b, Ok writer) -> same a a && distinct a b && same writer writer\n| _ -> false"
        ),
        "",
    );
    std::fs::remove_file(path).unwrap();
}

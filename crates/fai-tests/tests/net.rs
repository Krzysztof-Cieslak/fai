//! Network capability end-to-end tests.
//!
//! A program that uses `Net` is compiled in the concurrency-gated mode (it runs on
//! the M:N scheduler with `main` as the root task) so its socket operations can
//! park on the I/O reactor. These run programs in-process through the JIT,
//! capturing console output and asserting a clean, leak-free exit.

use std::sync::Mutex;

use fai_db::Db;
use indoc::indoc;

/// Serializes these tests: the console-capture sink, the live-object counter, and
/// the scheduler/reactor are process-global (see the concurrency suite).
static SERIAL: Mutex<()> = Mutex::new(());

/// Compiles and JIT-runs `src`, returning its captured stdout and exit code.
fn run(src: &str) -> (String, i32) {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Prog.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();

    fai_runtime::capture_start();
    let outcome = fai_driver::jit_run_program(&db, file);
    let out = fai_runtime::capture_take();
    (out, outcome.exit_code)
}

#[test]
fn tcp_loopback_echo() {
    // A server task accepts one connection and echoes the bytes it reads; the
    // client (the root task) connects to the listener's OS-assigned port, sends
    // "ping", and reads the echo back. Exercises listen/localPort/accept/connect/
    // send/recv over `Bytes`, with the capability dispatched from inside a spawn
    // thunk, all parking on the reactor.
    let (out, code) = run(ECHO_PROGRAM);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "ping\n");
}

/// The echo program shared by the JIT and AOT tests.
const ECHO_PROGRAM: &str = indoc! {r#"
    module Prog

    serve : { net : Net | _ } -> Listener -> Unit / { Net }
    let serve env listener =
      match env.net.accept listener with
      | Err e -> ()
      | Ok conn ->
        match env.net.recv conn 64 with
        | Err e -> ()
        | Ok data ->
          let _ = env.net.send conn data
          ()

    talk : { net : Net | _ } -> Int -> String / { Net }
    let talk env port =
      match env.net.connect "127.0.0.1" port with
      | Err e -> "connect: " ++ e
      | Ok conn ->
        match env.net.send conn (Bytes.fromString "ping") with
        | Err e -> "send: " ++ e
        | Ok u ->
          match env.net.recv conn 64 with
          | Err e -> "recv: " ++ e
          | Ok data ->
            match Bytes.toString data with
            | Some s -> s
            | None -> "non-utf8"

    session : { concurrency : Concurrency, net : Net | _ } -> Listener -> Int -> Nursery -> String / { Concurrency, Net }
    let session env listener port nursery =
      let server = env.concurrency.spawn nursery (fun u -> serve env listener)
      talk env port

    public main : Runtime -> Unit / { Concurrency, Console, Net }
    let main runtime =
      match runtime.net.listen 0 with
      | Err e -> runtime.console.writeLine ("listen: " ++ e)
      | Ok listener ->
        let port = runtime.net.localPort listener
        runtime.console.writeLine (runtime.concurrency.scope (session runtime listener port))
"#};

#[test]
fn aot_built_tcp_echo_runs() {
    // The full AOT path: `fai build` produces a native binary that links the
    // reactor and net operations from the runtime archive; the produced executable
    // runs the loopback echo and exits cleanly.
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("fai-net-aot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Prog.fai"), ECHO_PROGRAM).unwrap();
    let exe = dir.join("prog");

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = fai_cli::run(
        [
            "fai",
            "build",
            "--no-daemon",
            "-C",
            dir.to_str().unwrap(),
            "Prog.fai",
            "--out",
            exe.to_str().unwrap(),
        ],
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "AOT build failed: {}", String::from_utf8_lossy(&err));

    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = std::process::Command::new(&produced).output().unwrap();
    assert_eq!(run.status.code(), Some(0), "the networking binary should exit cleanly");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "ping\n");
}

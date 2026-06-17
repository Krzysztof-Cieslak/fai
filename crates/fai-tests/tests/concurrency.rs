//! Concurrency end-to-end tests.
//!
//! A program that uses the `Concurrency` capability is compiled in the
//! concurrency-gated mode (branchful reference counting, runtime allocation) and
//! runs on the M:N scheduler with `main` as the root task. These run programs
//! in-process through the JIT (`jit_run_program`), capturing console output and
//! asserting a clean, **leak-free** exit (the runtime's exit-time live-object
//! check returns 70 on a leak, after the scheduler quiesces).

use std::sync::Mutex;

use fai_db::Db;
use indoc::indoc;

/// Serializes these tests: the console-capture sink, the live-object counter, and
/// the scheduler are process-global, so two captures running at once on the test
/// binary's threads (under `cargo test`) would clobber each other. (Under nextest,
/// each test is its own process and the lock is trivially uncontended.)
static SERIAL: Mutex<()> = Mutex::new(());

/// Compiles and JIT-runs `src` (a self-contained program), returning its captured
/// stdout and exit code.
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
fn fan_out_await_combines_immediate_results() {
    // Two tasks compute immediates; the scope joins them. Nothing boxed is shared,
    // exercising the scheduler + root-task entry + the gated (but here trivial) RC.
    let src = indoc! {r#"
        module Prog

        body : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
        let body env nursery =
          let ta = env.concurrency.spawn nursery (fun u -> 6)
          let tb = env.concurrency.spawn nursery (fun u -> 8)
          env.concurrency.await ta + env.concurrency.await tb

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (runtime.concurrency.scope (body runtime)))
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "14\n");
}

#[test]
fn awaited_boxed_result_crosses_tasks() {
    // The task builds a `String` and returns it; it crosses back to the awaiter's
    // worker (marked shared, atomic reference counting) and is printed — exercising
    // the thread-safe reference counting of a value shared across tasks.
    let src = indoc! {r#"
        module Prog

        body : { concurrency : Concurrency | _ } -> Nursery -> String / { Concurrency }
        let body env nursery =
          let t = env.concurrency.spawn nursery (fun u -> "hello from a task")
          env.concurrency.await t

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          runtime.console.writeLine (runtime.concurrency.scope (body runtime))
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "hello from a task\n");
}

#[test]
fn channel_producer_consumer_sums() {
    // A producer (a spawned task) sends integers over a bounded channel and closes
    // it; the consumer (the scope body) sums until the channel drains. The channel
    // handle is shared between the two tasks.
    let src = indoc! {r#"
        module Prog

        produce : { concurrency : Concurrency | _ } -> Channel Int -> Unit -> Unit / { Concurrency }
        let produce env ch u =
          let a = env.concurrency.send ch 10
          let b = env.concurrency.send ch 20
          let c = env.concurrency.send ch 12
          env.concurrency.close ch

        drain : { concurrency : Concurrency | _ } -> Channel Int -> Int -> Int / { Concurrency }
        let drain env ch acc =
          match env.concurrency.recv ch with
          | None -> acc
          | Some n -> drain env ch (acc + n)

        body : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
        let body env nursery =
          let ch = env.concurrency.channel 2
          let producer = env.concurrency.spawn nursery (produce env ch)
          drain env ch 0

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (runtime.concurrency.scope (body runtime)))
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "42\n");
}

#[test]
fn aot_built_concurrent_program_runs() {
    // The full AOT path: `fai build` (isolated to a temp dir, so only this program
    // and the embedded std load) produces a native binary whose `main` runs as the
    // scheduler root task (`fai_run_main_concurrent`), linking the scheduler into
    // the runtime archive. Confirms the produced executable runs and exits cleanly.
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let src = indoc! {r#"
        module Prog

        body : { concurrency : Concurrency | _ } -> Nursery -> String / { Concurrency }
        let body env nursery =
          let t = env.concurrency.spawn nursery (fun u -> "concurrent build works")
          env.concurrency.await t

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          runtime.console.writeLine (runtime.concurrency.scope (body runtime))
    "#};

    let dir = std::env::temp_dir().join(format!("fai-conc-aot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Prog.fai"), src).unwrap();
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
    assert_eq!(run.status.code(), Some(0), "the concurrent binary should exit cleanly");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "concurrent build works\n");
}

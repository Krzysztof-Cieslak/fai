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

#[test]
fn fan_out_many_tasks_sum_of_squares() {
    // Spawn a task per list element (via `List.map`, which forwards `Concurrency`),
    // then fold the awaits — exercising many tasks and effect propagation through
    // the standard combinators.
    let src = indoc! {r#"
        module Prog

        squares : { concurrency : Concurrency | _ } -> Nursery -> List Int -> List (Task Int) / { Concurrency }
        let squares env nursery xs =
          List.map (fun x -> env.concurrency.spawn nursery (fun u -> x * x)) xs

        sumTasks : { concurrency : Concurrency | _ } -> List (Task Int) -> Int / { Concurrency }
        let sumTasks env tasks = List.foldl (fun acc t -> acc + env.concurrency.await t) 0 tasks

        body : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
        let body env nursery =
          sumTasks env (squares env nursery [1, 2, 3, 4, 5, 6, 7, 8, 9, 10])

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (runtime.concurrency.scope (body runtime)))
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "385\n");
}

#[test]
fn nested_scopes_combine() {
    // A scope whose spawned task opens its own scope, to two levels.
    let src = indoc! {r#"
        module Prog

        inner : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
        let inner env nursery =
          let a = env.concurrency.spawn nursery (fun u -> 3)
          let b = env.concurrency.spawn nursery (fun u -> 4)
          env.concurrency.await a + env.concurrency.await b

        outer : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
        let outer env nursery =
          let t = env.concurrency.spawn nursery (fun u -> env.concurrency.scope (inner env))
          env.concurrency.await t

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (runtime.concurrency.scope (outer runtime)))
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "7\n");
}

#[test]
fn task_returns_a_boxed_list() {
    // A task builds a list and returns it; the list (boxed cons cells) crosses back
    // to the awaiter with atomic reference counting, then is summed.
    let src = indoc! {r#"
        module Prog

        build : { concurrency : Concurrency | _ } -> Nursery -> List Int / { Concurrency }
        let build env nursery =
          let t = env.concurrency.spawn nursery (fun u -> [10, 20, 30, 40])
          env.concurrency.await t

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (List.sum (runtime.concurrency.scope (build runtime))))
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "100\n");
}

#[test]
fn structured_result_is_deterministic_across_runs() {
    // A pure fan-out/join yields the same result regardless of scheduling: run it
    // repeatedly and require identical output every time.
    let src = indoc! {r#"
        module Prog

        body : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
        let body env nursery =
          let ts = List.map (fun x -> env.concurrency.spawn nursery (fun u -> x + 1)) [1, 2, 3, 4, 5, 6, 7, 8]
          List.foldl (fun acc t -> acc + env.concurrency.await t) 0 ts

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (runtime.concurrency.scope (body runtime)))
    "#};
    for _ in 0..16 {
        let (out, code) = run(src);
        assert_eq!(code, 0, "clean, leak-free exit");
        assert_eq!(out, "44\n", "a pure fan-out/join is deterministic");
    }
}

#[test]
fn tasks_each_write_to_the_console() {
    // Several tasks write to the (interleaved) console; output order is unspecified,
    // so assert the set of lines rather than their order. Exercises an effectful
    // (Console-using) thunk crossing to a worker.
    let src = indoc! {r#"
        module Prog

        emit : { concurrency : Concurrency, console : Console | _ } -> Nursery -> Int -> Task Unit / { Concurrency, Console }
        let emit env nursery n = env.concurrency.spawn nursery (fun u -> env.console.writeLine (Int.toString n))

        body : { concurrency : Concurrency, console : Console | _ } -> Nursery -> Unit / { Concurrency, Console }
        let body env nursery =
          let ts = List.map (emit env nursery) [1, 2, 3, 4, 5]
          List.foldl (fun acc t -> env.concurrency.await t) () ts

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime = runtime.concurrency.scope (body runtime)
    "#};
    let (out, code) = run(src);
    assert_eq!(code, 0, "clean, leak-free exit");
    let mut lines: Vec<&str> = out.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, vec!["1", "2", "3", "4", "5"], "every task's line appears exactly once");
}

#[test]
fn tasks_do_file_io_concurrently() {
    // Each task writes a distinct file and reads it back, returning the byte length
    // of its contents; the scope joins them. The blocking file operations run on
    // the blocking pool while the tasks park (so they never stall a worker), and
    // each task dispatches the `fs` capability from inside a spawn thunk (a
    // capability captured by a closure). Four 7-byte payloads sum to 28.
    let dir = std::env::temp_dir().join(format!("fai-conc-fs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |i: usize| dir.join(format!("f{i}")).to_str().unwrap().replace('\\', "/");
    let src = format!(
        r#"module Prog

doFile : {{ fs : FileSystem | _ }} -> String -> Int / {{ FileSystem }}
let doFile env path =
  match env.fs.writeFile path "payload" with
  | Err e -> 0 - 1
  | Ok w ->
    match env.fs.readFile path with
    | Err e -> 0 - 2
    | Ok contents -> String.length contents

one : {{ concurrency : Concurrency, fs : FileSystem | _ }} -> Nursery -> String -> Task Int / {{ Concurrency, FileSystem }}
let one env nursery path = env.concurrency.spawn nursery (fun u -> doFile env path)

body : {{ concurrency : Concurrency, fs : FileSystem | _ }} -> Nursery -> Int / {{ Concurrency, FileSystem }}
let body env nursery =
  let ts = List.map (one env nursery) ["{}", "{}", "{}", "{}"]
  List.foldl (fun acc t -> acc + env.concurrency.await t) 0 ts

public main : Runtime -> Unit / {{ Concurrency, Console, FileSystem }}
let main runtime =
  runtime.console.writeLine (Int.toString (runtime.concurrency.scope (body runtime)))
"#,
        path(0),
        path(1),
        path(2),
        path(3),
    );
    let (out, code) = run(&src);
    assert_eq!(code, 0, "clean, leak-free exit");
    assert_eq!(out, "28\n");
}

#[test]
fn aot_concurrent_program_runs_on_a_single_worker() {
    // Forcing one worker (`FAI_WORKERS=1`) multiplexes every task onto one OS
    // thread via the green-thread scheduler; the structured result is unchanged.
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let src = indoc! {r#"
        module Prog

        body : { concurrency : Concurrency | _ } -> Nursery -> Int / { Concurrency }
        let body env nursery =
          let ts = List.map (fun x -> env.concurrency.spawn nursery (fun u -> x * 2)) [1, 2, 3, 4, 5]
          List.foldl (fun acc t -> acc + env.concurrency.await t) 0 ts

        public main : Runtime -> Unit / { Concurrency, Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (runtime.concurrency.scope (body runtime)))
    "#};
    let dir = std::env::temp_dir().join(format!("fai-conc-1w-{}", std::process::id()));
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
    let run = std::process::Command::new(&produced).env("FAI_WORKERS", "1").output().unwrap();
    assert_eq!(run.status.code(), Some(0), "single-worker run should exit cleanly");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "30\n");
}

//! A pipe releases its producer when a consumer stops early, before joining the scope.

use std::process::Command;

#[track_caller]
fn pipe_result(name: &str, helpers: &str, expression: &str, expected: &str, clock: bool) {
    let dir = std::env::temp_dir().join(format!("fai-pipe-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let effect = if clock { "Clock, Concurrency, Console" } else { "Concurrency, Console" };
    let source = format!(
        "module Main\n{helpers}\npublic main : Runtime -> Unit / {{ {effect} }}\nlet main r = r.console.writeLine (Int.toString ({expression}))\n"
    );
    std::fs::write(dir.join("Main.fai"), source).unwrap();
    let runtime = if cfg!(unix) { std::path::PathBuf::from("/tmp") } else { std::env::temp_dir() }
        .join(format!("fai-pipe-rt-{name}-{}", std::process::id()));
    let output = Command::new(env!("CARGO_BIN_EXE_fai"))
        .args(["run", "-C"])
        .arg(&dir)
        .arg("Main.fai")
        .env("FAI_RUNTIME_DIR", &runtime)
        .env("FAI_CACHE_DIR", dir.join("cache"))
        .env("FAI_RUN_TIMEOUT_MS", "5000")
        .output()
        .unwrap();
    let _ = Command::new(env!("CARGO_BIN_EXE_fai"))
        .args(["daemon", "stop", "-C"])
        .arg(&dir)
        .env("FAI_RUNTIME_DIR", runtime)
        .env("FAI_CACHE_DIR", dir.join("cache"))
        .output();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), format!("{expected}\n"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn consumer_takes_one_and_releases_a_full_producer() {
    pipe_result(
        "one",
        "",
        "Option.withDefault 0 (Async.pipe r.concurrency 1 (Async.produceList r.concurrency [1, 2, 3]) (fun ch -> r.concurrency.recv ch))",
        "1",
        false,
    );
}

#[test]
fn consumer_takes_nothing_and_releases_the_producer() {
    pipe_result(
        "zero",
        "",
        "Async.pipe r.concurrency 1 (Async.produceList r.concurrency [1, 2, 3]) (fun ch -> 42)",
        "42",
        false,
    );
}

#[test]
fn a_full_drain_preserves_order_and_result() {
    pipe_result(
        "drain",
        "",
        "List.sum (Async.pipe r.concurrency 1 (Async.produceList r.concurrency [1, 2, 3]) (Async.collect r.concurrency))",
        "6",
        false,
    );
}

#[test]
fn an_empty_producer_finishes_cleanly() {
    pipe_result(
        "empty",
        "",
        "Option.withDefault 7 (Async.pipe r.concurrency 1 (Async.produceList r.concurrency []) (fun ch -> r.concurrency.recv ch))",
        "7",
        false,
    );
}

#[test]
fn a_consumer_can_close_the_channel_itself() {
    pipe_result(
        "closed",
        "finish : Concurrency -> Channel Int -> Int / { Concurrency }\nlet finish c ch =\n  let closed = c.close ch\n  9\n",
        "Async.pipe r.concurrency 1 (Async.produceList r.concurrency [1, 2, 3]) (finish r.concurrency)",
        "9",
        false,
    );
}

#[test]
fn early_completion_cancels_a_sleeping_producer() {
    pipe_result(
        "sleeping",
        "produce : Runtime -> Channel Int -> Unit / { Clock, Concurrency }\nlet produce r ch =\n  let slept = r.clock.sleep 60000\n  Async.produceList r.concurrency [1, 2, 3] ch\n",
        "Async.pipe r.concurrency 1 (produce r) (fun ch -> 12)",
        "12",
        true,
    );
}

//! Pull-demand and effect timing of lazy stream producers through the run worker.

use std::process::{Command, Output};

fn run(name: &str, source: &str) -> Output {
    let dir = std::env::temp_dir().join(format!("fai-stream-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Main.fai"), source).unwrap();
    Command::new(env!("CARGO_BIN_EXE_fai"))
        .args(["run", "--no-daemon", "-C"])
        .arg(dir)
        .arg("Main.fai")
        .env("FAI_RUN_TIMEOUT_MS", "10000")
        .output()
        .unwrap()
}

#[track_caller]
fn pure_case(name: &str, expression: &str) {
    let source = format!(
        "module Main\npublic main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (if {expression} then \"ok\" else \"failed\")\n"
    );
    let output = run(name, &source);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn iterate_head_does_not_compute_the_next_element() {
    pure_case("head", "Stream.head (Stream.iterate (fun n -> 1 / 0) 42) = Ok (Some 42)");
}

#[test]
fn iterate_take_one_does_not_compute_the_next_element() {
    pure_case(
        "take-one",
        "Stream.toList (Stream.take 1 (Stream.iterate (fun n -> 1 / 0) 42)) = Ok [42]",
    );
}

#[test]
fn taking_no_elements_does_not_run_the_successor() {
    pure_case(
        "take-zero",
        "Stream.toList (Stream.take 0 (Stream.iterate (fun n -> 1 / 0) 42)) = Ok []",
    );
}

#[track_caller]
fn counted_pulls(count: usize) {
    let source = format!(
        r#"module Main
step : Console -> Int -> Int / {{ Console }}
let step console n =
  let _ = console.writeLine "step"
  n + 1
public main : Runtime -> Unit / {{ Console }}
let main r =
  let values = Stream.iterate (step r.console) 0
  let _ = r.console.writeLine "created"
  let result = Stream.toList (Stream.take {count} values)
  r.console.writeLine (Int.toString (List.length (Result.withDefault [] result)))
"#
    );
    let output = run(&format!("count-{count}"), &source);
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = format!("created\n{}{count}\n", "step\n".repeat(count.saturating_sub(1)));
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

#[test]
fn zero_pulls_run_no_effects() {
    counted_pulls(0);
}

#[test]
fn one_pull_runs_no_successor_effects() {
    counted_pulls(1);
}

#[test]
fn four_pulls_run_three_successor_effects() {
    counted_pulls(4);
}

#[test]
fn requesting_the_second_element_does_run_the_successor() {
    let output = run(
        "second-trap",
        "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let result = Stream.head (Stream.drop 1 (Stream.iterate (fun n -> 1 / 0) 42))\n  r.console.writeLine (if result = Ok (Some 42) then \"unexpected\" else \"also unexpected\")\n",
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("integer division by zero"));
}

#[test]
fn a_long_iterate_stream_uses_constant_stack_and_releases_its_tail() {
    pure_case(
        "long",
        "Stream.fold (fun acc x -> acc + x) 0 (Stream.take 100000 (Stream.iterate (fun x -> x + 1) 0)) = Ok 4999950000",
    );
}

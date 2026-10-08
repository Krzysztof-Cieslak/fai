//! Delivered-binary benchmark checks reject wrong answers and later failures.

use std::process::Command;

use fai_tests::benchmark_process::{ExpectedAnswer, spawn_checked};

fn baseline() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_algo-baseline"));
    command.args(["Fib", "10"]);
    command
}

#[test]
fn a_successful_executable_with_the_wrong_answer_is_rejected() {
    let output = spawn_checked(&mut baseline()).unwrap();
    let error = ExpectedAnswer::Int(56).verify("wrong-answer executable", &output).unwrap_err();
    let error = error.to_string();
    assert!(error.contains("expected Int(56)"), "{error}");
    assert!(error.contains("55"), "{error}");
}

#[test]
fn the_registered_oracle_validates_the_delivered_rust_answer() {
    let algorithm = fai_tests::algorithms::by_module("Fib").unwrap();
    let output = spawn_checked(
        Command::new(env!("CARGO_BIN_EXE_algo-baseline"))
            .args([algorithm.module, &algorithm.aot_size.to_string()]),
    )
    .unwrap();
    ExpectedAnswer::for_algorithm(algorithm).verify("Fib rust", &output).unwrap();
}

#[cfg(unix)]
#[test]
fn an_executable_that_succeeds_once_then_fails_is_rejected() {
    let path = std::env::temp_dir().join(format!("fai-benchmark-marker-{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let mut command = Command::new("sh");
    command.args(["-c", "if test -f \"$1\"; then echo later-output; echo later-error >&2; exit 7; fi; : > \"$1\"; echo 55", "fixture"]).arg(&path);
    let first = spawn_checked(&mut command).unwrap();
    ExpectedAnswer::Int(55).verify("first execution", &first).unwrap();
    let error = spawn_checked(&mut command).unwrap_err().to_string();
    std::fs::remove_file(path).unwrap();
    assert!(error.contains("7"), "{error}");
    assert!(error.contains("later-output"), "{error}");
    assert!(error.contains("later-error"), "{error}");
}

#[test]
fn floating_point_rounding_uses_the_shared_tolerance() {
    let mut output = spawn_checked(&mut baseline()).unwrap();
    output.stdout = b"3.14159265\n".to_vec();
    ExpectedAnswer::Float(std::f64::consts::PI).verify("rounded", &output).unwrap();
}

#[test]
fn nan_is_not_a_valid_benchmark_answer() {
    let mut output = spawn_checked(&mut baseline()).unwrap();
    output.stdout = b"NaN\n".to_vec();
    assert!(ExpectedAnswer::Float(0.0).verify("nan", &output).is_err());
}

#[test]
fn extra_output_is_not_silently_ignored() {
    let mut output = spawn_checked(&mut baseline()).unwrap();
    output.stdout = b"55\nextra".to_vec();
    assert!(ExpectedAnswer::Int(55).verify("extra output", &output).is_err());
}

#[test]
fn invalid_utf8_is_reported_with_stderr() {
    let mut output = spawn_checked(&mut baseline()).unwrap();
    output.stdout = vec![0xff];
    output.stderr = b"diagnostic detail".to_vec();
    let error =
        ExpectedAnswer::Int(55).verify("invalid encoding", &output).unwrap_err().to_string();
    assert!(error.contains("diagnostic detail"), "{error}");
}

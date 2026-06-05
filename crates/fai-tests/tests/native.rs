//! End-to-end native tests: compile a program with `fai build` (in process) and
//! run the produced executable, asserting its output and a clean, leak-free exit
//! (the runtime aborts with a nonzero code if any object leaks).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Builds `src` (as `Main.fai`) into a native binary and runs it, returning its
/// `(stdout, exit_code)`.
fn build_and_run(src: &str) -> (String, Option<i32>) {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Main.fai"), src).unwrap();
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
            "Main.fai",
            "--out",
            exe.to_str().unwrap(),
        ],
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "build failed: {}", String::from_utf8_lossy(&err));

    let run = Command::new(&exe).output().unwrap();
    (String::from_utf8_lossy(&run.stdout).into_owned(), run.status.code())
}

fn unique_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fai-native-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn print_main(expr: &str) -> String {
    format!(
        "module Main\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime ({expr})\n"
    )
}

#[test]
fn arithmetic() {
    let (out, code) = build_and_run(&print_main("Int.toString (1 + 2 * 3)"));
    assert_eq!(out, "7\n");
    assert_eq!(code, Some(0));
}

#[test]
fn string_concatenation() {
    let (out, code) = build_and_run(&print_main("\"a\" ++ \"b\" ++ \"c\""));
    assert_eq!(out, "abc\n");
    assert_eq!(code, Some(0));
}

#[test]
fn conditional() {
    let (out, code) = build_and_run(&print_main("if 2 < 1 then \"t\" else \"f\""));
    assert_eq!(out, "f\n");
    assert_eq!(code, Some(0));
}

#[test]
fn cross_definition_call() {
    let src = "module Main\n\nlet double x = x + x\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (Int.toString (double 21))\n";
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
fn higher_order_and_partial_application() {
    let src = "module Main\n\nlet add x y = x + y\n\nlet apply f x = f x\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (Int.toString (apply (add 40) 2))\n";
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
fn hello_sample_builds_and_runs() {
    let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/Hello.fai");
    let src = std::fs::read_to_string(sample).unwrap();
    let (out, code) = build_and_run(&src);
    assert_eq!(out, "Hello, Fai!\n");
    assert_eq!(code, Some(0));
}

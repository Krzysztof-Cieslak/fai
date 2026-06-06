//! End-to-end native tests: compile a program with `fai build` (in process) and
//! run the produced executable, asserting its output and a clean, leak-free exit
//! (the runtime aborts with a nonzero code if any object leaks).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use indoc::{formatdoc, indoc};

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

    // `fai build` appends the platform executable extension (`.exe` on Windows).
    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = Command::new(&produced).output().unwrap();
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
    formatdoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime = Console.writeLine runtime ({expr})
    "#}
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
    let src = indoc! {r#"
        module Main

        let double x = x + x

        public main : Runtime -> Unit
        let main runtime = Console.writeLine runtime (Int.toString (double 21))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
// Known limitation: on aarch64 macOS the AOT-built binary crashes (produces no
// output) when a top-level function is used as a value — i.e. partial
// application of a named function. Direct/saturated calls and the JIT path are
// fine; this is a Mach-O codegen issue with the static-closure code pointer.
#[cfg_attr(
    all(target_os = "macos", target_arch = "aarch64"),
    ignore = "aarch64 macOS AOT: a function used as a value crashes the produced binary"
)]
fn higher_order_and_partial_application() {
    let src = indoc! {r#"
        module Main

        let add x y = x + y

        let apply f x = f x

        public main : Runtime -> Unit
        let main runtime = Console.writeLine runtime (Int.toString (apply (add 40) 2))
    "#};
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

#[test]
fn user_defined_operator_runs() {
    let src = indoc! {r#"
        module Main

        let (+++) a b = a * b + 1

        public main : Runtime -> Unit
        let main runtime = Console.writeLine runtime (Int.toString (2 +++ 3))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "7\n"); // 2 * 3 + 1
    assert_eq!(code, Some(0));
}

#[test]
fn interface_instance_dispatch_runs() {
    let src = indoc! {r#"
        module Main

        interface Greeter =
          greet : String -> String

        let exclaimer = { Greeter with greet name = name ++ "!" }

        public main : Runtime -> Unit
        let main runtime = Console.writeLine runtime (exclaimer.greet "hi")
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "hi!\n");
    assert_eq!(code, Some(0));
}

#[test]
fn interface_instance_captures_state() {
    // The method closure captures the surrounding `n`.
    let src = indoc! {r#"
        module Main

        interface Counter =
          next : Unit -> Int

        let always n = { Counter with next u = n }

        public main : Runtime -> Unit
        let main runtime = Console.writeLine runtime (Int.toString ((always 42).next ()))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
fn builtin_operator_as_value_runs() {
    // `(+)` passed first-class to a fold.
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime =
          Console.writeLine runtime (Int.toString (List.foldl (+) 0 [1, 2, 3, 4]))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "10\n");
    assert_eq!(code, Some(0));
}

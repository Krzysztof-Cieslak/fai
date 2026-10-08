//! End-to-end tests of the `fai` binary's `run` and `build` commands, spawning
//! the real executable (so the `fai run` worker subprocess is exercised).

use std::path::PathBuf;
use std::process::Command;

use indoc::indoc;

fn fai() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fai"))
}

fn workspace(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fai-cli-run-e2e-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (file, contents) in files {
        std::fs::write(dir.join(file), contents).unwrap();
    }
    dir
}

const HELLO: &str = indoc! {r#"
    module Hello

    public main : Runtime -> Unit / { Console }
    let main runtime = runtime.console.writeLine "hi from run"
"#};

#[test]
fn run_prints_via_console_capability() {
    let dir = workspace("run", &[("Hello.fai", HELLO)]);
    let out = fai().args(["run", "--no-daemon", "-C"]).arg(&dir).arg("Hello.fai").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hi from run\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn const_first_argument_trap_prevents_second_argument_effect() {
    let source = indoc! {r#"
        module Main
        public main : Runtime -> Unit / { Console }
        let main r =
          let value = const (1 / 0) (r.console.writeLine "must not run")
          r.console.writeLine (Int.toString value)
    "#};
    let dir = workspace("const-first-trap", &[("Main.fai", source)]);
    let output =
        fai().args(["run", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("division by zero"));
    assert!(output.stdout.is_empty(), "{}", String::from_utf8_lossy(&output.stdout));
}

#[test]
fn const_second_argument_trap_follows_first_argument_effect() {
    let source = indoc! {r#"
        module Main
        public main : Runtime -> Unit / { Console }
        let main r = const (r.console.writeLine "first") (1 / 0)
    "#};
    let dir = workspace("const-second-trap", &[("Main.fai", source)]);
    let output =
        fai().args(["run", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("division by zero"));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "first\n");
}

#[test]
fn local_function_effects_are_latent_through_the_run_bundle() {
    let source = indoc! {r#"
        module Main
        public make : Console -> (String -> Unit / { Console })
        let make c =
          let log s = c.writeLine s
          log
        public main : Runtime -> Unit / { Console }
        let main r =
          let unusedClock u = r.clock.now ()
          let log = make r.console
          let _ = r.console.writeLine "created"
          log "called"
    "#};
    let dir = workspace("local-effects", &[("Main.fai", source)]);
    let output =
        fai().args(["run", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "created\ncalled\n");
}

#[test]
fn runtime_builder_and_open_main_run_in_the_supervised_worker() {
    let source = indoc! {r#"
        module Main
        let initialize nursery = stdConcurrency.await (stdConcurrency.spawn nursery (fun u -> ()))
        let runtime =
          let _ = stdConcurrency.scope initialize
          { a = 1, console = stdConsole, z = 42 }
        public main : { console : Console, z : Int | _ } -> Unit / { Console }
        let main r = r.console.writeLine (Int.toString r.z)
    "#};
    let dir = workspace("entry-adapter", &[("Main.fai", source)]);
    let output =
        fai().args(["run", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
}

#[test]
fn build_produces_a_runnable_binary() {
    let src = indoc! {r#"
        module Calc

        public main : Runtime -> Unit / { Console }
        let main runtime = runtime.console.writeLine (Int.toString (40 + 2))
    "#};
    let dir = workspace("build", &[("Calc.fai", src)]);
    let exe = dir.join("calc");

    let build = fai()
        .args(["build", "--no-daemon", "-C"])
        .arg(&dir)
        .arg("Calc.fai")
        .arg("--out")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(build.status.success(), "build stderr: {}", String::from_utf8_lossy(&build.stderr));

    // `fai build` appends the platform executable extension (`.exe` on Windows).
    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = Command::new(&produced).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n");
    assert_eq!(run.status.code(), Some(0), "the produced binary should exit cleanly");
}

#[track_caller]
fn cancelled_sleeps(native: bool) {
    use std::process::Stdio;
    use std::time::Duration;
    use wait_timeout::ChildExt;

    let source = include_str!("../../../samples/SleepCancellation.fai");
    let dir = workspace(
        if native { "sleep-aot" } else { "sleep-jit" },
        &[("SleepCancellation.fai", source)],
    );
    let mut command = if native {
        let exe = dir.join("sleep-cancellation");
        let built = fai()
            .args(["build", "--no-daemon", "-C"])
            .arg(&dir)
            .arg("SleepCancellation.fai")
            .arg("--out")
            .arg(&exe)
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "{}{}",
            String::from_utf8_lossy(&built.stdout),
            String::from_utf8_lossy(&built.stderr)
        );
        Command::new(exe.with_extension(std::env::consts::EXE_EXTENSION))
    } else {
        let mut command = fai();
        command
            .args(["run", "--no-daemon", "-C"])
            .arg(&dir)
            .arg("SleepCancellation.fai")
            .env("FAI_RUN_TIMEOUT_MS", "5000");
        command
    };
    let mut child = command
        .env("FAI_WORKERS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let finished = child.wait_timeout(Duration::from_secs(15)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        finished && out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"42\n");
}

#[test]
fn cancelled_sleeps_return_in_the_run_worker() {
    cancelled_sleeps(false);
}

#[test]
fn cancelled_sleeps_return_in_native_code() {
    cancelled_sleeps(true);
}

#[track_caller]
fn rejects_integer_literal(command: &str, literal: &str) {
    let source = format!(
        "module Main\nlet value = {literal}\npublic main : Runtime -> Unit\nlet main r = ()\n"
    );
    let dir = workspace(&format!("integer-{command}-{}", literal.len()), &[("Main.fai", &source)]);
    let output = fai()
        .args([command, "--no-daemon", "--message-format=json", "-C"])
        .arg(&dir)
        .arg("Main.fai")
        .output()
        .unwrap();
    assert!(!output.status.success(), "accepted {literal}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["diagnostics"].as_array().unwrap().iter().any(|error| error["code"] == "FAI1005"),
        "{result}"
    );
}

#[test]
fn check_rejects_decimal_integer_overflow() {
    rejects_integer_literal("check", "18446744073709551617");
}
#[test]
fn check_rejects_hexadecimal_integer_overflow() {
    rejects_integer_literal("check", "0x10000000000000001");
}
#[test]
fn check_rejects_octal_integer_overflow() {
    rejects_integer_literal("check", "0o2000000000000000000000");
}
#[test]
fn check_rejects_binary_integer_overflow() {
    rejects_integer_literal("check", &format!("0b1{}", "0".repeat(64)));
}
#[test]
fn build_rejects_integer_overflow() {
    rejects_integer_literal("build", "18446744073709551617");
}

const INTEGER_PATTERNS: &str = r#"module Main
let hex n = match n with | -0x1 -> 1 | _ -> 0
let oct n = match n with | -0o1 -> 1 | _ -> 0
let bin n = match n with | -0b1 -> 1 | _ -> 0
public main : Runtime -> Unit / { Console }
let main r =
  let patterns = hex (-1) + oct (-1) + bin (-1) = 3 && hex 0 + oct 0 + bin 0 = 0
  let boundary = 18446744073709551615 = -1 && 0xffffffffffffffff = -1 && -18446744073709551615 = 1 && 0x8000000000000000 = -9223372036854775808
  r.console.writeLine (if patterns && boundary then "ok" else "wrong integers")
"#;

#[test]
fn negative_radix_patterns_and_full_width_literals_run_through_the_worker() {
    let dir = workspace("integer-pattern-worker", &[("Main.fai", INTEGER_PATTERNS)]);
    let output =
        fai().args(["run", "--no-daemon", "-C"]).arg(dir).arg("Main.fai").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn negative_radix_patterns_and_full_width_literals_run_in_native_code() {
    let dir = workspace("integer-pattern-native", &[("Main.fai", INTEGER_PATTERNS)]);
    let exe = dir.join(format!("program{}", std::env::consts::EXE_SUFFIX));
    let build = fai()
        .args(["build", "--no-daemon", "-C"])
        .arg(&dir)
        .arg("Main.fai")
        .arg("--out")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(build.status.success(), "{}", String::from_utf8_lossy(&build.stderr));
    let output = Command::new(exe).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn float_order_and_negation_survive_the_worker_bundle() {
    let source = indoc! {r#"
        module Main
        public main : Runtime -> Unit / { Console }
        let main runtime =
          let negativeZero = -0.0
          let nan = Float.fromBits 0x7ff8000000001234
          let valid = negativeZero < 0.0 && Float.toBits negativeZero = 0x8000000000000000 && nan >= nan && Float.toBits (-nan) = 0xfff8000000001234
          runtime.console.writeLine (if valid then "ok" else "wrong float semantics")
    "#};
    let dir = workspace("float-bits", &[("Main.fai", source)]);
    let output =
        fai().args(["run", "--no-daemon", "-C"]).arg(&dir).arg("Main.fai").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn run_without_main_reports_no_entry_point() {
    let dir = workspace(
        "nomain",
        &[(
            "M.fai",
            indoc! {r#"
                module M

                let x = 1
            "#},
        )],
    );
    let out = fai().args(["run", "--no-daemon", "-C"]).arg(&dir).arg("M.fai").output().unwrap();
    assert_eq!(out.status.code(), Some(4), "a compile failure exits 4");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("entry point"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn build_json_envelope_reports_the_artifact() {
    let src = indoc! {r#"
        module Calc

        public main : Runtime -> Unit / { Console }
        let main runtime = runtime.console.writeLine "ok"
    "#};
    let dir = workspace("buildjson", &[("Calc.fai", src)]);
    let exe = dir.join("out");
    let output = fai()
        .args(["build", "--message-format=json", "--no-daemon", "-C"])
        .arg(&dir)
        .arg("Calc.fai")
        .arg("--out")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("valid JSON envelope");
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["ok"], true);
    let expected_stem = format!("out{}", std::env::consts::EXE_SUFFIX);
    assert!(value["artifact"].as_str().unwrap().ends_with(&expected_stem));
}

#[test]
fn build_type_error_exits_one_with_json_diagnostic() {
    let src = indoc! {r#"
        module Bad

        public main : Runtime -> Unit / { Console }
        let main runtime = runtime.console.writeLine (1 + 2)
    "#};
    let dir = workspace("buildbad", &[("Bad.fai", src)]);
    let output = fai()
        .args(["build", "--message-format=json", "--no-daemon", "-C"])
        .arg(&dir)
        .arg("Bad.fai")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "a failed build exits 1");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(value["ok"], false);
    assert!(value["artifact"].is_null());
    let codes: Vec<&str> = value["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(codes.iter().any(|c| c.starts_with("FAI3")), "expected a type error, got {codes:?}");
}

/// A program whose `build`, `inc`, and `sumAcc` are all self-tail-recursive over a
/// list far deeper than any native call stack would tolerate under recursion:
/// `build`/`inc` are tail-modulo-cons, `sumAcc` is a plain tail fold. With the loop
/// transform all three run in constant stack and free their input cell-by-cell.
/// `sum (inc (build n)) = n(n+3)/2`; for n = 1_000_000 that is 500001500000.
const DEEP: &str = indoc! {r#"
    module Deep

    let build k = if k <= 0 then [] else k :: build (k - 1)

    let inc xs =
      match xs with
      | [] -> []
      | x :: r -> (x + 1) :: inc r

    let sumAcc acc xs =
      match xs with
      | [] -> acc
      | x :: r -> sumAcc (acc + x) r

    public main : Runtime -> Unit / { Console }
    let main rt = rt.console.writeLine (Int.toString (sumAcc 0 (inc (build 1000000))))
"#};

#[test]
fn deep_tail_recursion_runs_in_constant_stack_via_jit() {
    let dir = workspace("deepjit", &[("Deep.fai", DEEP)]);
    let out = fai().args(["run", "--no-daemon", "-C"]).arg(&dir).arg("Deep.fai").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a deep tail recursion must run cleanly (no overflow, no leak); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "500001500000\n");
}

#[test]
fn deep_tail_recursion_runs_in_constant_stack_via_aot() {
    let dir = workspace("deepaot", &[("Deep.fai", DEEP)]);
    let exe = dir.join("deep");
    let build = fai()
        .args(["build", "--no-daemon", "-C"])
        .arg(&dir)
        .arg("Deep.fai")
        .arg("--out")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(build.status.success(), "build stderr: {}", String::from_utf8_lossy(&build.stderr));
    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = Command::new(&produced).output().unwrap();
    assert_eq!(
        run.status.code(),
        Some(0),
        "the deep native binary must run cleanly; stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout), "500001500000\n");
}

#[test]
fn deep_unconsumed_list_is_dropped_without_overflow() {
    // Builds a very deep list and never consumes it, so it is released *wholesale*
    // at the end of the binding's scope. The iterative drop frees the spine without
    // recursing, so the run exits cleanly (a recursive child release would
    // overflow the native stack here).
    let src = indoc! {r#"
        module Deep

        let build k = if k <= 0 then [] else k :: build (k - 1)

        public main : Runtime -> Unit / { Console }
        let main rt =
          let big = build 1000000
          rt.console.writeLine "built"
    "#};
    let dir = workspace("deepdrop", &[("Deep.fai", src)]);
    let out = fai().args(["run", "--no-daemon", "-C"]).arg(&dir).arg("Deep.fai").output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "dropping a deep list must not overflow; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "built\n");
}

#[test]
fn run_resolves_calls_across_modules() {
    let main = indoc! {r#"
        module Main

        public main : Runtime -> Unit / { Console }
        let main r = r.console.writeLine (Lib.shout "hi")
    "#};
    let lib = indoc! {r#"
        module Lib

        public shout : String -> String
        let shout s = s ++ "!"
    "#};
    let dir = workspace("multi", &[("Main.fai", main), ("Lib.fai", lib)]);
    let out = fai().args(["run", "--no-daemon", "-C"]).arg(&dir).arg("Main.fai").output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hi!\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.status.code(), Some(0));
}

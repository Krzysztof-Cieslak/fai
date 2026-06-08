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

    public main : Runtime -> Unit
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
fn build_produces_a_runnable_binary() {
    let src = indoc! {r#"
        module Calc

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

    public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

        public main : Runtime -> Unit
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

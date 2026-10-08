//! Fused numeric sources agree with materialization at signed-integer boundaries.

use std::process::{Command, Stdio};
use std::time::Duration;

use fai_db::{Db, FaiDatabase};
use wait_timeout::ChildExt;

#[track_caller]
fn check(case: &str, native: bool) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "range_worker", "--nocapture"])
        .env("FAI_RANGE_CASE", case)
        .env("FAI_RANGE_NATIVE", if native { "1" } else { "0" })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let finished = child.wait_timeout(Duration::from_secs(60)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        finished && output.status.success(),
        "{case}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn wrapped_negative_array_count_stays_empty() {
    check("empty", false);
}

#[test]
fn wrapped_positive_array_count_produces_its_elements() {
    check("wrapped", false);
}

#[test]
fn reverse_wrapped_array_range_preserves_order() {
    check("reverse-array", false);
}

#[test]
fn reverse_minimum_list_singleton_terminates() {
    check("reverse-min", false);
}

#[test]
fn reverse_empty_minimum_list_range_terminates() {
    check("reverse-empty", false);
}

#[test]
fn reverse_init_minimum_count_is_empty() {
    check("init-min", false);
}

#[test]
fn reverse_repeat_minimum_count_is_empty() {
    check("repeat-min", false);
}

#[test]
fn forward_maximum_list_singleton_matches_materialization() {
    check("forward-max", false);
}

#[test]
fn ordinary_reverse_range_preserves_fold_order() {
    check("ordinary", false);
}

#[test]
fn native_wrapped_negative_array_count_stays_empty() {
    check("empty", true);
}

#[test]
fn native_reverse_minimum_list_singleton_terminates() {
    check("reverse-min", true);
}

#[test]
fn range_worker() {
    let Ok(case) = std::env::var("FAI_RANGE_CASE") else { return };
    let (body, expected) = match case.as_str() {
        "empty" => (
            "let values = Array.range (-1) max\nlet eager = Array.any (fun x -> true) values\nlet fused = Array.any (fun x -> true) (Array.range (-1) max)\nif fused = eager && not fused then 1 else 0",
            "1",
        ),
        "wrapped" => (
            "let values = Array.range max (0 - max)\nlet eager = Array.sum values\nlet fused = Array.sum (Array.range max (0 - max))\nif fused = eager then fused else 0",
            "-1",
        ),
        "reverse-array" => (
            "let values = Array.range max (0 - max)\nlet fused = Array.foldr (fun x acc -> x :: acc) [] (Array.range max (0 - max))\nif fused = Array.toList values && fused = [max, min] then 1 else 0",
            "1",
        ),
        "reverse-min" => (
            "let values = List.range min (min + 1)\nlet fused = List.foldr (fun x acc -> acc + 1) 0 (List.range min (min + 1))\nif fused = List.length values then fused else 0",
            "1",
        ),
        "reverse-empty" => ("List.foldr (fun x acc -> acc + 1) 0 (List.range min min)", "0"),
        "init-min" => ("Array.foldr (fun x acc -> acc + 1) 0 (Array.init min (fun i -> i))", "0"),
        "repeat-min" => ("Array.foldr (fun x acc -> acc + 1) 0 (Array.repeat min 7)", "0"),
        "forward-max" => (
            "let values = List.range (max - 1) max\nlet fused = List.sum (List.range (max - 1) max)\nif fused = List.sum values then 1 else 0",
            "1",
        ),
        "ordinary" => ("Array.foldr (fun x acc -> x - acc) 0 (Array.range 1 4)", "2"),
        _ => panic!("unknown range case"),
    };
    let body = body.lines().map(|line| format!("  {line}\n")).collect::<String>();
    let source = format!(
        "module Main\npublic run : Int -> Int -> Int\nlet run min max =\n{body}\npublic main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (Int.toString (run (0 - 9223372036854775807 - 1) 9223372036854775807))\n"
    );
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source);
    let file = db.source_file(id).unwrap();
    let output = if std::env::var("FAI_RANGE_NATIVE").as_deref() == Ok("1") {
        let path = std::env::temp_dir().join(format!("fai-fused-range-{}", std::process::id()));
        let path = camino::Utf8PathBuf::from_path_buf(path).unwrap();
        let result = fai_driver::build_native(&db, file, &path);
        let artifact = result.artifact.expect("native range build");
        let mut child =
            Command::new(&artifact).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let finished = child.wait_timeout(Duration::from_secs(10)).unwrap().is_some();
        if !finished {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        std::fs::remove_file(&artifact).unwrap();
        assert!(finished && output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    } else {
        fai_runtime::capture_start();
        let result = fai_driver::jit_run_program(&db, file);
        assert_eq!(result.exit_code, 0);
        fai_runtime::capture_take()
    };
    assert_eq!(output.trim(), expected);
}

//! Fusion must preserve strict evaluation, traps, and termination.

use std::io::BufRead;
use std::process::{Command, Stdio};
use std::time::Duration;

use fai_db::{Db, FaiDatabase};
use wait_timeout::ChildExt;

fn worker(case: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "fusion_worker", "--nocapture"])
        .env("FAI_FUSION_CASE", case)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[track_caller]
fn assert_trap(case: &str, message: &str) {
    let mut child = worker(case);
    let finished = child.wait_timeout(Duration::from_secs(30)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(finished && !output.status.success(), "{case}: unexpected successful return or hang");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn strict_list_map_traps_before_short_circuiting() {
    assert_trap("list-map", "division by zero");
}

#[test]
fn materialized_list_map_has_the_same_trap() {
    assert_trap("eager-list", "division by zero");
}

#[test]
fn strict_array_map_traps_before_short_circuiting() {
    assert_trap("array-map", "division by zero");
}

#[test]
fn literal_elements_are_evaluated_before_search() {
    assert_trap("literal", "division by zero");
}

#[test]
fn strict_array_init_traps_before_search() {
    assert_trap("init", "division by zero");
}

#[test]
fn array_capacity_is_evaluated_before_length() {
    assert_trap("capacity", "division by zero");
}

#[test]
fn producer_trap_precedes_consumer_trap() {
    assert_trap("stage-order", "division by zero");
}

#[test]
fn forcing_a_literal_caf_cannot_be_skipped() {
    assert_trap("literal-caf", "division by zero");
}

#[test]
fn pipe_reduction_does_not_move_divergence_before_a_trap() {
    assert_trap("pipe-trap", "division by zero");
}

#[test]
fn short_circuiting_does_not_skip_a_diverging_map() {
    let mut child = worker("diverge");
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(stdout.read_line(&mut line).unwrap() != 0, "worker never reached execution");
        if line.contains("__FUSION_READY__") {
            break;
        }
    }
    let finished = child.wait_timeout(Duration::from_secs(1)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(!finished, "a diverging map returned: {:?}", output.status);
}

#[test]
fn fusion_worker() {
    let Ok(case) = std::env::var("FAI_FUSION_CASE") else { return };
    let body = match case.as_str() {
        "list-map" => "List.any (fun x -> x = 1) (List.map (fun n -> 1 / n) [1, 0])",
        "eager-list" => {
            "let values = List.map (fun n -> 1 / n) [1, 0]\nList.any (fun x -> x = 1) values"
        }
        "array-map" => "Array.any (fun x -> x = 1) (Array.map (fun n -> 1 / n) [| 1, 0 |])",
        "literal" => "List.any (fun x -> true) [1, 1 / 0]",
        "literal-caf" => "List.any (fun x -> true) [1, bad]",
        "init" => "Array.any (fun x -> true) (Array.init 2 (fun i -> 1 / (1 - i)))",
        "capacity" => "Array.length (Array.withCapacity (1 / 0))",
        "stage-order" => {
            "List.foldl (fun acc x -> Array.unsafeGet 9 [| x |]) 0 (List.map (fun n -> 1 / n) [1, 0])"
        }
        "diverge" => "List.any (fun x -> x = 1) (List.map spin [1, 0])",
        "pipe-trap" => "(1 / 0) |> make ()",
        _ => panic!("unknown fusion case"),
    };
    let body = body.lines().map(|line| format!("  {line}\n")).collect::<String>();
    let source = format!(
        "module Main\nlet spin n = if n = 0 then spin n else 1\nlet bad = 1 / 0\nlet make u =\n  let _ = spin 0\n  fun x -> x\nlet probe u =\n{body}\npublic main : Runtime -> Unit / {{ Console }}\nlet main r =\n  let _ = r.console.writeLine \"__FUSION_READY__\"\n  let _ = probe ()\n  r.console.writeLine \"finished\"\n"
    );
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source);
    let outcome = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    assert_eq!(outcome.exit_code, 0);
}

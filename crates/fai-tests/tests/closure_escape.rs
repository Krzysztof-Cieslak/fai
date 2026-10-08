//! Escaping closures remain valid after their creating frame is overwritten.

use std::process::{Command, Stdio};
use std::time::Duration;

use fai_db::{Db, FaiDatabase};
use wait_timeout::ChildExt;

#[track_caller]
fn check(case: &str, native: bool) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "closure_worker", "--nocapture"])
        .env("FAI_ESCAPE_CASE", case)
        .env("FAI_ESCAPE_NATIVE", if native { "1" } else { "0" })
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
fn inline_partial_closure_survives_its_frame() {
    check("inline", false);
}

#[test]
fn bound_partial_closure_survives_its_frame() {
    check("bound", false);
}

#[test]
fn boxed_captures_survive_the_creating_frame() {
    check("boxed", false);
}

#[test]
fn returned_partial_application_survives_its_frame() {
    check("pap", false);
}

#[test]
fn partial_application_of_a_partial_application_survives() {
    check("nested-pap", false);
}

#[test]
fn conditional_alias_survives_its_frame() {
    check("branch", false);
}

#[test]
fn nested_let_alias_survives_its_frame() {
    check("nested-let", false);
}

#[test]
fn generic_adapter_cannot_retain_a_stack_callback() {
    check("adapter", false);
}

#[test]
fn generic_map_cannot_retain_a_stack_callback() {
    check("map", false);
}

#[test]
fn native_inline_partial_closure_survives_its_frame() {
    check("inline", true);
}

#[test]
fn native_returned_partial_application_survives_its_frame() {
    check("pap", true);
}

#[test]
fn native_conditional_alias_survives_its_frame() {
    check("branch", true);
}

#[test]
fn closure_worker() {
    let Ok(case) = std::env::var("FAI_ESCAPE_CASE") else { return };
    let (body, expected) = match case.as_str() {
        "inline" => ("(fun x y -> seed + x + y) 1", "3731"),
        "bound" => ("let f = fun x y -> seed + x + y\nf 1", "3731"),
        "boxed" => ("let values = [seed]\n(fun x y -> List.sum values + x + y) 1", "3731"),
        "pap" => ("let partial = add seed 1\npartial", "3731"),
        "nested-pap" => ("let first = addThree seed\nlet second = first 1\nsecond 2", "3737"),
        "branch" => (
            "let f = fun x -> seed + x\nlet g = fun x -> seed - x\nlet selected = if seed > 0 then f else g\nselected",
            "3728",
        ),
        "nested-let" => (
            "let f = fun x -> seed + x\nlet g = fun x -> seed - x\nlet selected =\n  if seed > 0 then\n    let alias = f\n    alias\n  else\n    g\nselected",
            "3728",
        ),
        "adapter" => ("Apply.once (fun x y -> seed + x + y) 1", "3731"),
        "map" => (
            "let callbacks = Apply.map (fun x y -> seed + x + y) [1, 2]\nmatch callbacks with\n| f :: rest -> f\n| [] -> fun y -> y",
            "3731",
        ),
        _ => panic!("unknown escape case"),
    };
    let body = body.lines().map(|line| format!("  {line}\n")).collect::<String>();
    let factory = format!(
        "module Factory\nlet add seed x y = seed + x + y\nlet addThree seed x y z = seed + x + y + z\npublic make : Int -> (Int -> Int)\nlet make seed =\n{body}\npublic overwrite : Int -> Int\nlet overwrite seed =\n  let f = fun x -> seed * x\n  List.sum (List.map f [1, 2, 3, 4, 5, 6, 7, 8])\n"
    );
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Factory.fai".into(), factory);
    db.add_source("Apply.fai".into(), "module Apply\npublic once : ('a -> 'b / 'e) -> 'a -> 'b / 'e\nlet once f x = f x\npublic map : ('a -> 'b / 'e) -> List 'a -> List 'b / 'e\nlet map f xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> f x :: map f rest\n".into());
    let id = db.add_source("Main.fai".into(), "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let first = Factory.make 40\n  let second = Factory.make 80\n  let scratch = Factory.overwrite 99\n  r.console.writeLine (Int.toString (first 1 + first 2 + second 1 + scratch))\n".into());
    let file = db.source_file(id).unwrap();
    let output = if std::env::var("FAI_ESCAPE_NATIVE").as_deref() == Ok("1") {
        let path = std::env::temp_dir().join(format!("fai-escape-{}", std::process::id()));
        let path = camino::Utf8PathBuf::from_path_buf(path).unwrap();
        let result = fai_driver::build_native(&db, file, &path);
        assert!(result.ok, "{:?}", result.diagnostics);
        let artifact = result.artifact.unwrap();
        let mut child =
            Command::new(&artifact).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let finished = child.wait_timeout(Duration::from_secs(10)).unwrap().is_some();
        if !finished {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        std::fs::remove_file(artifact).unwrap();
        assert!(finished && output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    } else {
        fai_runtime::capture_start();
        let outcome = fai_driver::jit_run_program(&db, file);
        assert_eq!(outcome.exit_code, 0);
        fai_runtime::capture_take()
    };
    assert_eq!(output.trim(), expected);
}

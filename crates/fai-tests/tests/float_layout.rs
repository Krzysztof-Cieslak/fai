//! Generic and concrete aggregate producers share logical Float field semantics.

use std::process::{Command, Stdio};
use std::time::Duration;

use fai_db::{Db, FaiDatabase};
use wait_timeout::ChildExt;

const MAKER: &str = "module Maker\npublic box : 'a -> { x : 'a, y : Int }\nlet box x = { x = x, y = 0 }\npublic pair : 'a -> 'b -> 'a * 'b\nlet pair a b = (a, b)\npublic first : { x : 'a, y : Int } -> 'a\nlet first r = r.x\npublic equal : { x : 'a, y : Int } -> { x : 'a, y : Int } -> Bool\nlet equal a b = a = b\npublic apply : ('a -> 'b / 'e) -> 'a -> 'b / 'e\nlet apply f x = f x\n";
const READER: &str = "module Reader\npublic record : Float -> { x : Float, y : Int }\nlet record x = { x = x, y = 0 }\npublic x : { x : Float | _ } -> Float\nlet x r = r.x\npublic sum : Float * Float -> Float\nlet sum p =\n  let (x, y) = p\n  x + y\npublic pass : Float * Float -> Float * Float\nlet pass p = p\n";

#[track_caller]
fn check(case: &str, native: bool) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "float_worker", "--nocapture"])
        .env("FAI_FLOAT_LAYOUT_CASE", case)
        .env("FAI_FLOAT_LAYOUT_NATIVE", if native { "1" } else { "0" })
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
fn generic_record_float_projection() {
    check("record", false);
}
#[test]
fn generic_tuple_float_projection() {
    check("tuple", false);
}
#[test]
fn row_polymorphic_float_projection() {
    check("row", false);
}
#[test]
fn generic_projection_from_raw_float_record() {
    check("generic-read", false);
}
#[test]
fn scalar_and_boxed_fields_compare_in_both_orders() {
    check("compare", false);
}
#[test]
fn generic_comparison_accepts_raw_float_fields() {
    check("generic-compare", false);
}
#[test]
fn generic_tuple_crosses_a_spread_parameter_boundary() {
    check("spread", false);
}
#[test]
fn generic_tuple_crosses_a_first_class_wrapper() {
    check("wrapper", false);
}
#[test]
fn generic_tuple_crosses_a_spread_return_boundary() {
    check("return", false);
}
#[test]
fn record_updates_preserve_mixed_layout_and_ownership() {
    check("update", false);
}
#[test]
fn fields_past_the_scalar_bitmap_stay_boxed() {
    check("wide", false);
}
#[test]
fn native_generic_record_float_projection() {
    check("record", true);
}
#[test]
fn native_generic_tuple_crosses_a_first_class_wrapper() {
    check("wrapper", true);
}
#[test]
fn native_scalar_and_boxed_fields_compare_in_both_orders() {
    check("compare", true);
}

#[test]
fn float_worker() {
    let Ok(case) = std::env::var("FAI_FLOAT_LAYOUT_CASE") else { return };
    let (body, expected) = match case.as_str() {
        "record" => ("Float.toString (Maker.box 1.5).x".into(), "1.5"),
        "tuple" => ("let (x, y) = Maker.pair 1.5 2\nFloat.toString (x + Int.toFloat y)".into(), "3.5"),
        "row" => ("Float.toString (Reader.x (Maker.box 1.5) + Reader.x (Reader.record 2.5))".into(), "4.0"),
        "generic-read" => ("Float.toString (Maker.first (Reader.record 1.5))".into(), "1.5"),
        "compare" => ("let a = Maker.box 1.5\nlet b = Reader.record 1.5\nif a = b && b = a && compare a b = 0 && compare b a = 0 then \"equal\" else \"different\"".into(), "equal"),
        "generic-compare" => ("if Maker.equal (Reader.record 1.5) (Maker.box 1.5) then \"equal\" else \"different\"".into(), "equal"),
        "spread" => ("Float.toString (Reader.sum (Maker.pair 1.5 2.5))".into(), "4.0"),
        "wrapper" => ("Float.toString (Maker.apply Reader.sum (Maker.pair 1.5 2.5))".into(), "4.0"),
        "return" => ("Float.toString (Reader.sum (Reader.pass (Maker.pair 1.5 2.5)))".into(), "4.0"),
        "update" => ("let original = Maker.box 1.5\nlet updated = { original with y = 4 }\nlet changed = { original with x = 2.5 }\nFloat.toString (original.x + updated.x + changed.x)".into(), "5.5"),
        "wide" => {
            let fields = (0..65).map(|i| format!("x{i:02} = 1.5")).collect::<Vec<_>>().join(", ");
            (format!("let r = {{ {fields} }}\nFloat.toString r.x64"), "1.5")
        }
        _ => panic!("unknown float layout case"),
    };
    let body: String = body.lines().map(|line| format!("  {line}\n")).collect();
    let source = format!(
        "module Main\nlet probe u =\n{body}\npublic main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (probe ())\n"
    );
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Maker.fai".into(), MAKER.into());
    db.add_source("Reader.fai".into(), READER.into());
    let id = db.add_source("Main.fai".into(), source);
    let file = db.source_file(id).unwrap();
    let output = if std::env::var("FAI_FLOAT_LAYOUT_NATIVE").as_deref() == Ok("1") {
        let path = std::env::temp_dir().join(format!("fai-float-layout-{}", std::process::id()));
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
        let result = fai_driver::jit_run_program(&db, file);
        assert_eq!(result.exit_code, 0);
        fai_runtime::capture_take()
    };
    assert_eq!(output.trim(), expected);
}

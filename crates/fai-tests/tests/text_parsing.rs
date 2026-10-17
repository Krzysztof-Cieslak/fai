//! Public text parsing preserves separators, Unicode, wrapping and call boundaries.
use std::sync::Mutex;

use fai_db::{Db, FaiDatabase};

static SERIAL: Mutex<()> = Mutex::new(());

#[track_caller]
fn check(expression: &str) {
    let _guard = SERIAL.lock().unwrap();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), format!("module Main\npublic main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (if {expression} then \"ok\" else \"wrong\")\n"));
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "ok\n");
}

#[test]
fn a_split_leaves_later_separators_untouched() {
    check("String.splitOnce \":\" \"a:b:c:\" = Some (\"a\", \"b:c:\")");
}

#[test]
fn a_unicode_separator_has_unicode_coordinates() {
    check("String.splitOnce \"😀\" \"é😀z😀\" = Some (\"é\", \"z😀\")");
}

#[test]
fn an_absent_separator_has_no_split() {
    check("String.splitOnce \"::\" \"é:a\" = None");
}

#[test]
fn an_empty_separator_splits_at_the_start() {
    check("String.splitOnce \"\" \"é😀\" = Some (\"\", \"é😀\")");
}

#[test]
fn splitting_only_the_separator_keeps_both_empty_pieces() {
    check("String.splitOnce \"::\" \"::\" = Some (\"\", \"\")");
}

#[test]
fn first_class_splitters_preserve_the_uniform_boundary() {
    check(
        "List.map (String.splitOnce \"=\") [\"a=1=2\", \"none\"] = [Some (\"a\", \"1=2\"), None]",
    );
}

#[test]
fn signed_minimum_parses_without_overflow() {
    check("Int.fromString \"-9223372036854775808\" = Some (0 - 9223372036854775807 - 1)");
}

#[test]
fn positive_decimal_overflow_wraps() {
    check("Int.fromString \"18446744073709551617\" = Some 1");
}

#[test]
fn negative_decimal_overflow_wraps() {
    check("Int.fromString \"-18446744073709551617\" = Some (0 - 1)");
}

#[test]
fn decimal_parsing_rejects_unicode_digits() {
    check("Int.fromString \"１２\" = None");
}

#[test]
fn decimal_parsing_rejects_a_leading_plus() {
    check("Int.fromString \"+1\" = None");
}

#[test]
fn decimal_parsing_rejects_trailing_whitespace() {
    check("Int.fromString \"1 \" = None");
}

#[test]
fn decimal_parsing_rejects_a_lone_minus() {
    check("Int.fromString \"-\" = None");
}

#[test]
fn decimal_parsing_rejects_empty_text() {
    check("Int.fromString \"\" = None");
}

#[test]
fn url_parsing_preserves_additional_query_and_fragment_delimiters() {
    check(
        "Result.map Url.toString (Url.parse \"/café?a=1=2#one#two\") = Ok \"/café?a=1=2#one#two\"",
    );
}

const BOUNDARY_SOURCE: &str = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let split = List.map (String.splitOnce \"😀\") [\"é😀x😀\", \"none\"]\n  let number = Int.fromString \"-18446744073709551617\"\n  r.console.writeLine (if split = [Some (\"é\", \"x😀\"), None] && number = Some (0 - 1) then \"ok\" else \"wrong\")\n";

#[test]
fn worker_boundaries_preserve_text_primitives() {
    let _guard = SERIAL.lock().unwrap();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = db.add_source("Main.fai".into(), BOUNDARY_SOURCE.into());
    let bundle = fai_driver::build_run_bundle(&db, db.source_file(source).unwrap()).bundle.unwrap();
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    fai_runtime::capture_start();
    let code = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((code, output.as_str()), (0, "ok\n"));
}

#[test]
fn native_boundaries_preserve_text_primitives() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = db.add_source("Main.fai".into(), BOUNDARY_SOURCE.into());
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("text")).unwrap();
    let built = fai_driver::build_native(&db, db.source_file(source).unwrap(), &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let output = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn long_decimal_parsing_has_no_per_digit_allocations() {
    let _guard = SERIAL.lock().unwrap();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = db.add_source("Main.fai".into(), "module Main\npublic decode : String -> Option Int\nlet decode text = Int.fromString text\npublic main : Runtime -> Unit\nlet main r = ()\n".into());
    let mut compiled = fai_driver::jit_compile(&db, db.source_file(source).unwrap()).unwrap();
    let decode = compiled.function(fai_syntax::Symbol::intern("decode")).unwrap();
    let input = fai_runtime::make_str(&format!("{}7", "0".repeat(100000)));
    fai_runtime::reset_allocations();
    let result = fai_runtime::apply(decode, &[input]);
    assert!(fai_runtime::allocations() <= 2, "only uniform result boxing may allocate");
    fai_runtime::fai_drop(result);
}

#[test]
fn absent_split_does_not_materialize_strings_or_lists() {
    let _guard = SERIAL.lock().unwrap();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = db.add_source("Main.fai".into(), "module Main\npublic split : String -> Option (String * String)\nlet split text = String.splitOnce \":\" text\npublic main : Runtime -> Unit\nlet main r = ()\n".into());
    let mut compiled = fai_driver::jit_compile(&db, db.source_file(source).unwrap()).unwrap();
    let split = compiled.function(fai_syntax::Symbol::intern("split")).unwrap();
    let input = fai_runtime::make_str("no separator here");
    fai_runtime::reset_allocations();
    let result = fai_runtime::apply(split, &[input]);
    assert_eq!(fai_runtime::allocations(), 0);
    fai_runtime::fai_drop(result);
}

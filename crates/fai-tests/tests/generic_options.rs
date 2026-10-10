//! Generic Option representations preserve concrete and nested boundary values.

use fai_db::{Db, FaiDatabase};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());
const LIBRARY: &str = "module Library\npublic wrap : 'a -> Option 'a\nlet wrap x = Some x\npublic forward : Option 'a -> Option 'a\nlet forward x = x\npublic choose : Bool -> 'a -> Option 'a\nlet choose yes x = if yes then Some x else None\npublic capture : Option 'a -> (Unit -> Option 'a)\nlet capture option = fun _ -> match option with | None -> None | Some x -> Some x\n";

#[track_caller]
fn check(definitions: &str, expression: &str) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Library.fai".into(), LIBRARY.into());
    let source = format!(
        "module Main\n{definitions}\npublic main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (if ({expression}) then \"yes\" else \"no\")\n"
    );
    let id = db.add_source("Main.fai".into(), source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(fai_runtime::capture_take(), "yes\n");
}

#[test]
fn generic_float_result_uses_the_concrete_match_layout() {
    check("", "match Library.wrap 1.25 with | Some x -> x = 1.25 | None -> false");
}

#[test]
fn generic_none_float_result_keeps_its_concrete_discriminant() {
    check("", "match Library.choose false 1.25 with | Some _ -> false | None -> true");
}

#[test]
fn generic_nested_none_keeps_both_option_layers() {
    check(
        "none : Option Int\nlet none = None\n",
        "Library.wrap none = Some None && Library.forward (Some none) <> None",
    );
}

#[test]
fn generic_nested_some_keeps_both_option_layers() {
    check("", "Library.wrap (Some 9223372036854775807) = Some (Some 9223372036854775807)");
}

#[test]
fn generic_function_payload_retains_its_uniform_closure() {
    check("", "(Option.withDefault identity (Library.wrap (fun x -> x + 1))) 7 = 8");
}

#[test]
fn generic_tuple_payload_crosses_both_niche_schemes() {
    check("", "Library.forward (Library.wrap (7, \"value\")) = Some (7, \"value\")");
}

#[test]
fn generic_option_capture_can_be_called_repeatedly() {
    check(
        "let run _ =\n  let call = Library.capture (Library.wrap \"value\")\n  let first = call ()\n  let second = call ()\n  first = Some \"value\" && second = Some \"value\"\n",
        "run ()",
    );
}

#[test]
fn generic_none_capture_can_be_called_repeatedly() {
    check(
        "let run _ =\n  let call = Library.capture (Library.choose false false)\n  call () = None && call () = None\n",
        "run ()",
    );
}

#[test]
fn generic_nested_float_keeps_both_data_layers() {
    check("", "Library.wrap (Library.wrap 1.25) = Some (Some 1.25)");
}

#[test]
fn generic_option_elements_stay_standard_in_collections() {
    check("", "Array.toList [| Library.wrap 1.25, Library.choose false 0.0 |] = [Some 1.25, None]");
}

#[test]
fn repeated_cross_scheme_forwarding_allocates_independently_of_iterations() {
    use fai_runtime as rt;
    use fai_syntax::Symbol;
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Library.fai".into(), LIBRARY.into());
    let source = "module Main\nloop : Int -> Option (Int * String) -> Int\nlet loop n option = if n <= 0 then (match option with | None -> 0 | Some (x, _) -> x) else loop (n - 1) (Library.forward option)\npublic run : Int -> Int\nlet run n = loop n (Some (7, \"kept\"))\npublic main : Runtime -> Unit\nlet main _ = ()\n";
    let id = db.add_source("Main.fai".into(), source.into());
    let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
    let run = program.function(Symbol::intern("run")).unwrap();
    let measure = |count| {
        let baseline = (rt::live_count(), rt::live_bytes());
        rt::reset_allocations();
        let result = rt::apply(rt::fai_dup(run), &[rt::make_int(count)]);
        assert_eq!(rt::read_int(result), 7);
        let allocations = rt::allocations();
        rt::fai_drop(result);
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
        allocations
    };
    let short = measure(8);
    let long = measure(4096);
    assert_eq!(short, long);
    assert!(long <= 2, "only the retained payload may allocate: {long}");
}

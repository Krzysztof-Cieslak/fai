//! Compact and extended data headers across native, JIT, and transported code.

use std::sync::Mutex;

use fai_db::{Db, FaiDatabase};

static LOCK: Mutex<()> = Mutex::new(());

enum Route {
    Jit,
    Native,
    Bundle,
}

#[track_caller]
fn check(source: &str, expected: &str, route: Route) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    let diagnostics = fai_tests::check_source_diagnostics(&db, file);
    assert!(
        !diagnostics.iter().any(|d| d.severity == fai_diagnostics::Severity::Error),
        "{diagnostics:?}"
    );
    match route {
        Route::Native => {
            let temp = tempfile::tempdir().unwrap();
            let path =
                camino::Utf8PathBuf::from_path_buf(temp.path().join("compact-data")).unwrap();
            let built = fai_driver::build_native(&db, file, &path);
            assert!(built.ok, "{:?}", built.diagnostics);
            let output = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
        }
        Route::Jit => {
            fai_runtime::capture_start();
            let result = fai_driver::jit_run_program(&db, file);
            assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
            assert_eq!(fai_runtime::capture_take().trim(), expected);
        }
        Route::Bundle => {
            let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
            let bytes = serde_json::to_vec(&bundle).unwrap();
            let bundle = serde_json::from_slice(&bytes).unwrap();
            fai_runtime::capture_start();
            assert_eq!(fai_driver::jit_run_bundle(&bundle), 0);
            assert_eq!(fai_runtime::capture_take().trim(), expected);
        }
    }
}

const MIXED_FLOATS: &str = r#"module Main
let make a b c d e f g h i = { a = a, b = b, c = c, d = d, e = e, f = f, g = g, h = h, i = i }
let last r = r.i
let setLast r v = { r with i = v }
public main : Runtime -> Unit / { Console }
let main r =
  let packed = make 1.0 2.0 3.0 4.0 5.0 6.0 7.0 8.0 9.0
  let wide = { a = 1.0, b = 2.0, c = 3.0, d = 4.0, e = 5.0, f = 6.0, g = 7.0, h = 8.0, i = 9.0 }
  let changed = setLast packed 10.0
  let moved = setLast (make 1.0 2.0 3.0 4.0 5.0 6.0 7.0 8.0 9.0) 10.0
  let ok = packed = wide && last packed = 9.0 && last changed = 10.0 && changed = moved
  r.console.writeLine (if ok then "yes" else "no")
"#;

#[test]
fn mixed_float_headers_and_updates_run_in_jit() {
    check(MIXED_FLOATS, "yes", Route::Jit);
}

#[test]
fn mixed_float_headers_and_updates_run_natively() {
    check(MIXED_FLOATS, "yes", Route::Native);
}

#[test]
fn mixed_float_headers_survive_bundle_transport() {
    check(MIXED_FLOATS, "yes", Route::Bundle);
}

#[test]
fn row_evidence_addresses_both_header_sizes() {
    let source = "module Main\nlet sum r = r.a + r.p\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let small = { a = 1, p = 2 }\n  let large = { a = 1, b = 2, c = 3, d = 4, e = 5, f = 6, g = 7, h = 8, i = 9, j = 10, k = 11, l = 12, m = 13, n = 14, o = 15, p = 16 }\n  r.console.writeLine (Int.toString (sum small + sum large))\n";
    check(source, "20", Route::Native);
}

#[test]
fn inspected_array_slots_keep_escaped_children_owned() {
    let source = "module Main\ntype Slot = | Empty | Full String\nlet pick i xs =\n  if i < 0 then pick 0 xs else match Array.unsafeGet i xs with | Empty -> if i = 0 then \"empty\" else \"none\" | Full s -> if Array.length xs > i then s else \"no\"\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (pick 0 [| Full (String.join \"\" [\"hello\", \"hello\", \"hello\"]) |])\n";
    check(source, "hellohellohello", Route::Native);
}

#[test]
fn array_update_before_inspection_keeps_the_old_slot_alive() {
    let source = "module Main\ntype Slot = | Full String\nlet field slot = match slot with | Full s -> s\nlet update xs =\n  let old = Array.unsafeGet 0 xs\n  let changed = Array.unsafeSet 0 (Full \"new\") xs\n  field old ++ field (Array.unsafeGet 0 changed)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (update [| Full \"old\" |])\n";
    check(source, "oldnew", Route::Bundle);
}

#[test]
fn inspected_slot_body_edits_match_clean_generation() {
    let source = "module M\ntype Slot = | Empty | Full String\nlet probe i xs = match Array.unsafeGet i xs with | Empty -> Array.length xs + i | Full s -> String.length s + Array.length xs + i\n";
    let edited = source.replace("String.length s +", "String.length s * 2 +");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*fai_rc::rc(
                db,
                db.source_file(files[0]).unwrap(),
                fai_syntax::Symbol::intern("probe"),
            ))
            .clone()
        },
    );
}

#[test]
fn prefix_reversal_runs_natively_without_changing_a_retained_input() {
    let source = "module Main\nlet reverse n xs = List.append (List.reverse (List.take n xs)) (List.drop n xs)\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let xs = [1, 2, 3, 4, 5]\n  let reversed = reverse 3 xs\n  r.console.writeLine (if reversed = [3, 2, 1, 4, 5] && xs = [1, 2, 3, 4, 5] then \"yes\" else \"no\")\n";
    check(source, "yes", Route::Native);
}

#[test]
fn generic_float_prefix_reversal_survives_bundle_transport() {
    let source = "module Main\nlet reverse n xs = List.append (List.reverse (List.take n xs)) (List.drop n xs)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (if reverse 3 [1.5, 2.5, 3.5, 4.5] = [3.5, 2.5, 1.5, 4.5] then \"yes\" else \"no\")\n";
    check(source, "yes", Route::Bundle);
}

#[test]
fn repeated_effectful_prefix_counts_keep_both_evaluations() {
    let source = "module Main\ncount : Console -> Int / { Console }\nlet count console =\n  let _ = console.writeLine \"count\"\n  2\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let xs = [1, 2, 3]\n  let ys = List.append (List.reverse (List.take (count r.console) xs)) (List.drop (count r.console) xs)\n  r.console.writeLine (if ys = [2, 1, 3] then \"yes\" else \"no\")\n";
    check(source, "count\ncount\nyes", Route::Jit);
}

#[test]
fn prefix_count_edits_match_clean_fusion() {
    let source = "module M\nlet reverse n xs = List.append (List.reverse (List.take n xs)) (List.drop n xs)\n";
    let edited = source.replace("List.take n", "List.take (n + 1)");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &edited)]],
        |db, files| {
            (*fai_core::fuse_def(
                db,
                db.source_file(files[0]).unwrap(),
                fai_syntax::Symbol::intern("reverse"),
            ))
            .clone()
        },
    );
}

#[test]
fn constructor_tag_outside_compact_range_uses_extended_layout() {
    let constructors = (0..=2048).map(|i| format!("| C{i} Int")).collect::<Vec<_>>().join(" ");
    let source = format!(
        "module Main\ntype T = {constructors}\nlet get value = match value with | C2048 n -> n | _ -> 0\npublic main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (Int.toString (get (C2048 42)))\n"
    );
    check(&source, "42", Route::Jit);
}

#[test]
fn generic_array_push_does_not_treat_a_data_field_as_a_descriptor() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = "module Main\npublic create : Int -> { x : Float }\nlet create bits = { x = Float.fromBits bits }\npublic wrap : 'a -> Array 'a\nlet wrap value = Array.singleton value\npublic main : Runtime -> Unit\nlet main r = ()\n";
    let id = db.add_source("Main.fai".into(), source.into());
    let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
    let baseline = fai_runtime::live_count();
    let descriptor_bits = std::ptr::addr_of!(fai_runtime::FAI_FLOAT_DESC) as usize as i64;
    let create = program.function(fai_syntax::Symbol::intern("create")).unwrap();
    let value = fai_runtime::apply(create, &[fai_runtime::make_int(descriptor_bits)]);
    let wrap = program.function(fai_syntax::Symbol::intern("wrap")).unwrap();
    let array = fai_runtime::apply(wrap, &[value]);
    let element = fai_runtime::fai_array_get_borrowed(array, fai_runtime::make_int(0));
    assert_eq!(element, value, "the compact data cell remains an ordinary array element");
    let field = fai_runtime::fai_data_field(element, 0);
    assert_eq!(fai_runtime::read_float(field).to_bits(), descriptor_bits as u64);
    fai_runtime::fai_drop(field);
    fai_runtime::fai_drop(element);
    fai_runtime::fai_drop(array);
    assert_eq!(fai_runtime::live_count(), baseline);
}

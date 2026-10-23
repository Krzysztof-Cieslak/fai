//! Diagnostic locality reports preserve ownership IR and survive worker transport.

use fai_db::{Db, FaiDatabase};
use fai_rc::{analyze_locality, analyze_program_locality};
use fai_syntax::Symbol;

const SOURCE: &str = "module M\ntype Cell = | Cell Int Int\nlet visit n cell =\n  match cell with\n  | Cell a b -> if n <= 0 then Cell b a else visit (n - 1) (Cell b a)\npublic main : Runtime -> Unit / { Console }\nlet main runtime =\n  let Cell a b = visit 4 (Cell 2 3)\n  runtime.console.writeLine (Int.toString (a + b))\n";

#[test]
fn reporting_preserves_cached_ir_and_native_objects() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), SOURCE.into());
    let file = db.source_file(id).unwrap();
    let name = Symbol::intern("visit");
    let lowered = fai_rc::rc_emit(&db, file, name);
    let before = (*lowered).clone();
    let object = fai_driver::object_code(&db, file, name, true);
    let first = analyze_locality(&lowered);
    assert_eq!(first, analyze_locality(&lowered));
    assert_eq!(*lowered, before);
    assert_eq!(object, fai_driver::object_code(&db, file, name, true));
}

#[test]
fn worker_roundtrip_preserves_diagnostic_sites() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), SOURCE.into());
    let bundle = fai_driver::build_run_bundle(&db, db.source_file(id).unwrap()).bundle.unwrap();
    let rebuilt = fai_core::wire::from_wire(&bundle);
    let copy = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let transported = fai_core::wire::from_wire(&copy);
    let before: Vec<_> = rebuilt.defs.iter().map(analyze_locality).collect();
    let after: Vec<_> = transported.defs.iter().map(analyze_locality).collect();
    assert_eq!(before, after);
    assert_eq!(
        analyze_program_locality(&rebuilt.defs),
        analyze_program_locality(&transported.defs)
    );
}

#[test]
fn helper_summary_edits_match_clean_whole_program_analysis() {
    const HELPER: &str =
        "module Helper\npublic wrap : String -> String * String\nlet wrap value = (value, value)\n";
    const MAIN: &str = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main runtime =\n  let pair = Helper.wrap (Int.toString 42)\n  let (first, second) = pair\n  runtime.console.writeLine (first ++ second)\n";
    let changed = HELPER.replace("(value, value)", "(value ++ \"!\", value)");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[
            &[("Helper.fai", HELPER), ("Main.fai", MAIN)],
            &[("Helper.fai", &changed), ("Main.fai", MAIN)],
        ],
        |db, files| {
            let bundle =
                fai_driver::build_run_bundle(db, db.source_file(files[1]).unwrap()).bundle.unwrap();
            let before = serde_json::to_vec(&bundle).unwrap();
            let rebuilt = fai_core::wire::from_wire(&bundle);
            let report = analyze_program_locality(&rebuilt.defs);
            assert_eq!(before, serde_json::to_vec(&bundle).unwrap());
            report
        },
    );
}

#[test]
fn body_edits_produce_the_same_report_incrementally_and_clean() {
    let changed = SOURCE.replace("Cell b a", "Cell (a + b) b");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", SOURCE)], &[("M.fai", &changed)]],
        |db, files| {
            let lowered =
                fai_rc::rc_emit(db, db.source_file(files[0]).unwrap(), Symbol::intern("visit"));
            analyze_locality(&lowered)
        },
    );
}

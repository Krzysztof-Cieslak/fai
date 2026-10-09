//! Primitive argument permutations preserve borrowing, ordering and native values.

use fai_db::{Db, FaiDatabase};
use fai_syntax::Symbol;

#[test]
fn indexing_loop_releases_its_owner_only_at_exit() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), "module M\npublic total : Int -> Float -> Array Float -> Float\nlet total i acc xs = if i >= Array.length xs then acc else total (i + 1) (acc + Array.unsafeGet i xs) xs\n".into());
    let result = fai_rc::rc(&db, db.source_file(id).unwrap(), Symbol::intern("total"));
    let owner = result.entry().params[2].index();
    let text = fai_core::pretty_def(&result);
    assert!(!text.contains(&format!("dup %{owner}")), "{text}");
    assert_eq!(text.matches(&format!("drop %{owner}")).count(), 1, "{text}");
}

#[test]
fn native_update_keeps_full_width_values_and_retained_aliases() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), "module M\nlet set x xs = Array.unsafeSet 0 x xs\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let original = [| 1, 2 |]\n  let changed = set 0x8000000000000001 original\n  let copy = Array.unsafeSet 0 (Array.unsafeGet 1 original) original\n  let good = Array.toList original = [1, 2] && Array.unsafeGet 0 changed = 0x8000000000000001 && Array.toList copy = [2, 2]\n  r.console.writeLine (if good then \"ok\" else \"wrong\")\n".into());
    let temp = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(temp.path().join("permuted")).unwrap();
    let built = fai_driver::build_native(&db, db.source_file(id).unwrap(), &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let output = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
}

#[test]
fn operand_edits_match_clean_objects() {
    let before =
        "module M\npublic at : Int -> Array Float -> Float\nlet at i xs = Array.unsafeGet i xs\n";
    let after = before.replace("unsafeGet i", "unsafeGet (i + 1)");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", before)], &[("M.fai", &after)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = Symbol::intern("at");
            (
                fai_core::pretty_def(&fai_rc::rc(db, file, name)),
                (*fai_driver::object_code(db, file, name, false)).clone(),
            )
        },
    );
}

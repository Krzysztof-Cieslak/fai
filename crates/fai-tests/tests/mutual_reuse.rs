//! Flattened mutual recursion does not reference an unemitted specialized entry.

use fai_db::{Db, FaiDatabase};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = include_str!("fixtures/MutualReuse.fai");

fn program() -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("MutualReuse.fai".into(), SOURCE.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[test]
fn worker_bundle_can_enter_a_mutual_group_after_releasing_a_cell() {
    let _guard = LOCK.lock().unwrap();
    let (db, file) = program();
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    fai_runtime::capture_start();
    let code = fai_driver::jit_run_bundle(&bundle);
    let output = fai_runtime::capture_take();
    assert_eq!((code, output.as_str()), (0, "42\n"));
}

#[test]
fn native_program_can_enter_a_mutual_group_after_releasing_a_cell() {
    let (db, file) = program();
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("mutual-reuse")).unwrap();
    let built = fai_driver::build_native(&db, file, &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let output = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"42\n");
}

#[test]
fn changing_mutual_group_membership_matches_clean_reuse_signatures() {
    let changed = SOURCE.replace(
        "let left n = if n <= 0 then Cell 11 31 else right (n - 1)",
        "let left n =\n  if n <= 0 then Cell 11 31 else\n    let Cell a b = right (n - 1)\n    Cell b a",
    );
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", SOURCE)], &[("M.fai", &changed)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = fai_syntax::Symbol::intern("left");
            (fai_rc::reuse_signature(db, file, name), (*fai_rc::rc_emit(db, file, name)).clone())
        },
    );
}

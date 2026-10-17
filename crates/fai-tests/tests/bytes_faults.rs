//! Native and worker byte reads preserve checked indexing over full-width Ints.
use fai_db::{Db, FaiDatabase};

fn source(index: &str, bytes: &str) -> String {
    format!(
        "module Main\npublic main : Runtime -> Unit / {{ Console }}\nlet main runtime = runtime.console.writeLine (Int.toString (Bytes.unsafeGet ({index}) (Bytes.fromList {bytes})))\n"
    )
}

#[track_caller]
fn worker_fault(index: &str, bytes: &str, wire: bool) {
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "byte_fault_worker", "--nocapture"])
        .env("FAI_BYTE_FAULT_SOURCE", source(index, bytes))
        .env("FAI_BYTE_FAULT_WIRE", if wire { "1" } else { "0" })
        .output()
        .unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("Bytes.unsafeGet: index out of bounds"), "{stderr}");
}

#[track_caller]
fn native_fault(index: &str, bytes: &str) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source(index, bytes));
    let file = db.source_file(id).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("bytes")).unwrap();
    let built = fai_driver::build_native(&db, file, &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let result = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("Bytes.unsafeGet: index out of bounds"), "{stderr}");
}

#[test]
fn negative_byte_index_traps_in_jit() {
    worker_fault("0 - 1", "[0, 255]", false);
}

#[test]
fn byte_length_is_not_a_valid_index() {
    worker_fault("2", "[0, 255]", false);
}

#[test]
fn full_width_negative_byte_index_survives_worker_transport() {
    worker_fault("0 - 9223372036854775807 - 1", "[0, 255]", true);
}

#[test]
fn full_width_positive_byte_index_traps_in_native_code() {
    native_fault("9223372036854775807", "[0, 255]");
}

#[test]
fn an_empty_byte_buffer_has_no_valid_native_index() {
    native_fault("0", "[]");
}

#[test]
fn byte_fault_worker() {
    let Ok(text) = std::env::var("FAI_BYTE_FAULT_SOURCE") else { return };
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), text);
    let file = db.source_file(id).unwrap();
    if std::env::var("FAI_BYTE_FAULT_WIRE").as_deref() == Ok("1") {
        let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
        let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
        let _ = fai_driver::jit_run_bundle(&bundle);
    } else {
        let _ = fai_driver::jit_run_program(&db, file);
    }
}

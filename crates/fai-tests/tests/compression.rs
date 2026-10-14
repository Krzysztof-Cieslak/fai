//! Native compression snapshots retain pure value semantics and query firewalls.

use fai_db::Db;

#[test]
fn native_compression_snapshots_are_pure_values() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "CompressionValues.fai".into(),
        r#"module CompressionValues
let options = [| 1, 2, 6, 1024, 1024, 64, 1024, 16 |]
public main : Runtime -> Unit / { Console }
let main runtime =
  match (CompressionEngine.create options, CompressionEngine.create options) with
  | (Ok a, Ok b) ->
    let x = CompressionEngine.step a (Bytes.fromString "hello") [| 0, 0 |]
    let y = CompressionEngine.step b (Bytes.fromString "hello") [| 0, 0 |]
    let equal = a = b && compare a b = 0 && x = y && HashSet.size (HashSet.fromList [a, b]) = 1
    runtime.console.writeLine (if equal then "ok" else "bad")
  | _ -> runtime.console.writeLine "failed"
"#
        .into(),
    );
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "ok\n");
    assert_eq!(fai_runtime::compression::storage_counts(), (0, 0));
}

#[test]
fn compression_bridge_preserves_signature_firewalls() {
    fn setup(body: &str) -> (fai_db::FaiDatabase, fai_db::SourceFile) {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        db.add_source("A.fai".into(), format!("module A\npublic make : Int -> Result CompressionEngine.State CompressionEngine.Error\nlet make n = CompressionEngine.create [| 1, 2, {body}, 1024, 1024, 64, 1024, 16 |]\n"));
        let b = db.add_source(
            "B.fai".into(),
            "module B\npublic check : Int -> Bool\nlet check n = Result.isOk (A.make n)\n".into(),
        );
        let file = db.source_file(b).unwrap();
        (db, file)
    }
    let (mut db, file) = setup("n");
    let name = fai_syntax::Symbol::intern("check");
    let before = fai_types::body_types(&db, file, name);
    db.enable_event_log();
    db.add_source("A.fai".into(), "module A\npublic make : Int -> Result CompressionEngine.State CompressionEngine.Error\nlet make n = CompressionEngine.create [| 1, 2, n + 1, 1024, 1024, 64, 1024, 16 |]\n".into());
    assert_eq!(before, fai_types::body_types(&db, file, name));
    assert!(!db.take_events().iter().any(|event| event.contains("infer_scc_query")));
    let (clean, clean_file) = setup("n + 1");
    assert_eq!(
        fai_types::body_types(&db, file, name),
        fai_types::body_types(&clean, clean_file, name)
    );
}

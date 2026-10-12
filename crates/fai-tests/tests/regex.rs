//! Typed regex primitives preserve purity and native value semantics through JIT.

use fai_db::Db;

#[test]
fn compiled_programs_have_value_semantics_across_native_calls() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "NativeRegex.fai".into(),
        r#"module NativeRegex
let options = [| 2, 65536, 10485760, 2097152, 128, 256, 1 |]
public main : Runtime -> Unit / { Console }
let main r =
  match (RegexEngine.compile "é" options, RegexEngine.compile "é" options) with
  | (Ok a, Ok b) ->
    let found = RegexEngine.findText a "AéB" [| 0, 1, 0, 100 |]
    let equal = a = b && compare a b = 0 && HashSet.size (HashSet.fromList [a, b]) = 1
    r.console.writeLine (if equal && found = Ok [| (1, 3, 1, 2) |] then "ok" else "bad")
  | _ -> r.console.writeLine "failed"
"#
        .into(),
    );
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "ok\n");
}

#[test]
fn native_regex_types_preserve_the_cross_module_firewall() {
    fn setup(body: &str) -> (fai_db::FaiDatabase, fai_db::SourceFile) {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        db.add_source("A.fai".into(), format!("module A\npublic make : String -> Result RegexEngine.Program RegexEngine.Error\nlet make s = RegexEngine.compile {body} [| 2, 65536, 10485760, 2097152, 128, 256, 1 |]\n"));
        let b = db.add_source(
            "B.fai".into(),
            "module B\npublic check : String -> Bool\nlet check s = Result.isOk (A.make s)\n"
                .into(),
        );
        let file = db.source_file(b).unwrap();
        (db, file)
    }
    let (mut db, file) = setup("s");
    let name = fai_syntax::Symbol::intern("check");
    let before = fai_types::body_types(&db, file, name);
    db.enable_event_log();
    db.add_source("A.fai".into(), "module A\n// moved source\npublic make : String -> Result RegexEngine.Program RegexEngine.Error\nlet make s = RegexEngine.compile (s ++ \"a\") [| 2, 65536, 10485760, 2097152, 128, 256, 1 |]\n".into());
    let after = fai_types::body_types(&db, file, name);
    assert_eq!(before, after);
    assert!(!db.take_events().iter().any(|e| e.contains("infer_scc_query")));
    let (clean, clean_file) = setup("(s ++ \"a\")");
    assert_eq!(after, fai_types::body_types(&clean, clean_file, name));
}

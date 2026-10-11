//! Native path values cross the JIT ABI without losing spelling or opacity.

use fai_db::Db;

#[test]
fn native_paths_execute_through_the_jit() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Paths.fai".into(), r#"module Paths
public main : Runtime -> Unit / { Console }
let main r =
  let base = Result.withDefault Path.empty (Path.fromString "a/b")
  let child = Result.withDefault Path.empty (Path.fromString "../c")
  let path = Path.normalize (Path.join base child)
  let expected = Result.withDefault Path.empty (Path.fromString "a/c")
  let roundTrip = Path.fromNativeBytes (Path.toNativeBytes path) = Ok path
  let firstClass = List.map Path.normalize [path, base]
  let ok = path = expected && roundTrip && List.length firstClass = 2 && Array.length (Path.components path) = 2
  r.console.writeLine (if ok then "ok" else "bad")
"#.into());
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "ok\n");
}

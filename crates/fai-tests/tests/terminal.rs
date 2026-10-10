//! Unicode primitives cross the native ABI with correct ownership and arities.

use fai_db::Db;

#[test]
fn graphemes_and_cell_width_execute_through_the_jit() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("TerminalText.fai".into(), r#"module TerminalText
public main : Runtime -> Unit / { Console }
let main r =
  let parts = TextCells.graphemes "a\u{301}👩‍💻界"
  let good = parts = [| "a\u{301}", "👩‍💻", "界" |] && TextCells.width false (String.joinArray "" parts) = 5 && TextCells.width true "·" = 2 && TextCells.width false "·" = 1
  r.console.writeLine (if good then "ok" else "bad")
"#.into());
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "ok\n");
}

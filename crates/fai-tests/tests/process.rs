//! The process host crosses the JIT ABI and validates before native execution.

use fai_db::Db;

#[test]
fn process_host_rejects_invalid_input_through_the_jit() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("ProcessCheck.fai".into(), r#"module ProcessCheck
public type R = { console : Console, process : ProcessHost.Process }
let runtime = { console = stdConsole, process = ProcessHost.native }
public main : R -> Unit / { Console, ProcessHost.Process }
let main r =
  let options = { bufferBytes = 4096, chunkBytes = 1024, drainTimeoutMs = Some 1000, graceMs = 0, pollMs = 5, timeoutMs = None }
  match r.process.start [] options with
  | Err error -> r.console.writeLine (if error.kind = 1 then "ok" else "bad error")
  | Ok session ->
    let report = r.process.close session
    r.console.writeLine "unexpected process"
"#.into());
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "ok\n");
}

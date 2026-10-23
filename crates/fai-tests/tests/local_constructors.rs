//! Constructor reduction preserves full-width values and closure capture scopes.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn resource_program() -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "Main.fai".into(),
        r#"module Main
run : Channel String -> (Unit -> Unit) -> List Int
let run channel observe =
  let pair = (channel, 1)
  let ignored = observe ()
  let answer = match pair with | (_, answer) -> answer
  [answer]
public main : Runtime -> Unit / { Console, Concurrency }
let main r =
  let channel = r.concurrency.channel 1
  let sent = r.concurrency.send channel ("queued-" ++ Int.toString 17)
  let result = run channel (fun _ -> ())
  r.console.writeLine (Int.toString (List.length result))
"#
        .into(),
    );
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[test]
fn eliminated_container_still_finalizes_its_native_channel() {
    let _guard = LOCK.lock().unwrap();
    let (db, file) = resource_program();
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    rt::capture_start();
    let code = fai_driver::jit_run_bundle(&bundle);
    let output = rt::capture_take();
    assert_eq!((code, output.as_str()), (0, "1\n"));
}

#[test]
fn native_eliminated_container_releases_queued_payloads() {
    let (db, file) = resource_program();
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("resources")).unwrap();
    let built = fai_driver::build_native(&db, file, &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let output = std::process::Command::new(built.artifact.unwrap())
        .env("FAI_WORKERS", "2")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"1\n");
}

fn branch_program() -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), r#"module Main
run : Bool -> Bool -> Channel String -> Int -> Float -> (Unit -> Unit) -> Int * Int
let run again yes channel n value observe =
  if again then run false yes channel n value observe else
    let state = if yes then (n, value, channel) else (n + 1, -value, channel)
    let ignored = observe ()
    let (i, f, _) = state
    (i, Float.toBits f)
public main : Runtime -> Unit / { Console, Concurrency }
let main r =
  let channel = r.concurrency.channel 1
  let sent = r.concurrency.send channel ("queued-" ++ Int.toString 17)
  let bits = 9221120237041090626
  let (i, actual) = run true false channel 9223372036854775807 (Float.fromBits bits) (fun _ -> ())
  r.console.writeLine (if i = -9223372036854775808 && actual = Int.xor bits (-9223372036854775808) then "ok" else "wrong")
"#.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[test]
fn worker_branch_components_preserve_full_width_values_and_resources() {
    let _guard = LOCK.lock().unwrap();
    let (db, file) = branch_program();
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    rt::capture_start();
    let code = fai_driver::jit_run_bundle(&bundle);
    let output = rt::capture_take();
    assert_eq!((code, output.as_str()), (0, "ok\n"));
}

#[test]
fn native_branch_components_preserve_full_width_values_and_resources() {
    let (db, file) = branch_program();
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("components")).unwrap();
    let built = fai_driver::build_native(&db, file, &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let output = std::process::Command::new(built.artifact.unwrap())
        .env("FAI_WORKERS", "2")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn constructor_branches_keep_integer_and_float_bits() {
        let _guard = LOCK.lock().unwrap();
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id =
            db.add_source("Main.fai".into(), include_str!("fixtures/LocalConstructors.fai").into());
        let program =
            RefCell::new(fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<i64>(), any::<bool>(), any::<u64>()), |(value, yes, bits)| {
                let baseline = rt::live_count();
                let function = program.borrow_mut().function(Symbol::intern("compute")).unwrap();
                let result =
                    rt::apply(function, &[rt::make_int(i64::from(yes)), rt::make_int(value)]);
                let expected =
                    if yes { value.wrapping_add(value.wrapping_add(1)).wrapping_add(7) } else { 7 };
                prop_assert_eq!(rt::read_int(result), expected);
                rt::fai_drop(result);
                let function = program.borrow_mut().function(Symbol::intern("floatBits")).unwrap();
                let result = rt::apply(function, &[rt::fai_box_float(bits as i64)]);
                prop_assert_eq!(rt::read_int(result) as u64, bits);
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}

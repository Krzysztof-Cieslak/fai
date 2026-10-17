//! Immortal empty Bytes survive native, generic and task boundaries.
use fai_db::{Db, FaiDatabase};

const SOURCE: &str = r#"module Main
let repeat n value =
  if n <= 0 then value
  else repeat (n - 1) (Bytes.concat value (Bytes.slice 1 1 (Bytes.fromString "abc")))
public main : Runtime -> Unit / { Console, Concurrency }
let main runtime =
  let values = [Bytes.empty, repeat 10000 Bytes.empty]
  let (left, right) = Async.parallel2 runtime.concurrency (fun _ -> List.map Bytes.length values) (fun _ -> Bytes.toString Bytes.empty)
  runtime.console.writeLine (if left = [0, 0] && right = Some "" && List.map Bytes.length values = [0, 0] then "ok" else "wrong")
"#;

#[test]
fn serial_empty_buffer_releases_keep_the_static_header_intact() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = db.add_source("Main.fai".into(), "module Main\nlet length bytes = Bytes.length bytes\npublic main : Runtime -> Unit / { Console }\nlet main runtime =\n  let empty = Bytes.empty\n  let count = length empty\n  let text = Bytes.toString empty\n  runtime.console.writeLine (if count = 0 && text = Some \"\" then \"ok\" else \"wrong\")\n".into());
    let temporary = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(temporary.path().join("empty-serial")).unwrap();
    let built = fai_driver::build_native(&db, db.source_file(source).unwrap(), &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let output = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn native_empty_buffers_preserve_immortal_counts() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = db.add_source("Main.fai".into(), SOURCE.into());
    let temporary = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(temporary.path().join("empty")).unwrap();
    let built = fai_driver::build_native(&db, db.source_file(source).unwrap(), &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let output = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

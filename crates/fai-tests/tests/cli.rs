//! Golden snapshots of CLI output, driven in-process through `fai_cli::run`.

use camino::Utf8PathBuf;

/// Runs the CLI in-process, returning `(exit_code, stdout, stderr)`.
fn run(args: &[&str]) -> (i32, String, String) {
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = fai_cli::run(args.iter().copied(), &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

/// An empty, controlled workspace so command output never depends on whatever
/// files happen to be near the test's working directory.
fn empty_workspace() -> Utf8PathBuf {
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .expect("temp dir is UTF-8")
        .join("fai-m0-cli-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn check_json_output() {
    let dir = empty_workspace();
    let (code, out, err) = run(&["fai", "check", "-C", dir.as_str(), "--message-format=json"]);
    assert_eq!(code, 0, "stderr: {err}");
    insta::assert_snapshot!("check_json", out);
}

#[test]
fn check_human_output() {
    let dir = empty_workspace();
    let (code, out, err) = run(&["fai", "check", "-C", dir.as_str(), "--color=never"]);
    assert_eq!(code, 0, "stderr: {err}");
    insta::assert_snapshot!("check_human", out);
}

/// A workspace with a single typed module, for query/check-with-types tests.
fn typed_workspace() -> Utf8PathBuf {
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .expect("temp dir is UTF-8")
        .join("fai-m2-cli-tests");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Calc.fai"),
        "module Calc\n\npublic add : Int -> Int -> Int\nlet add x y = x + y\n",
    )
    .unwrap();
    dir
}

#[test]
fn query_type_json_output() {
    let dir = typed_workspace();
    let (code, out, err) = run(&["fai", "query", "-C", dir.as_str(), "type", "Calc.add"]);
    assert_eq!(code, 0, "stderr: {err}");
    insta::assert_snapshot!("query_type_calc_add", out);
}

#[test]
fn check_reports_type_error() {
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .expect("temp dir is UTF-8")
        .join("fai-m2-cli-typeerr");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Bad.fai"), "module Bad\n\npublic f : Int -> Bool\nlet f x = x + 1\n")
        .unwrap();
    let (code, out, err) =
        run(&["fai", "check", "-C", dir.as_str(), "Bad.fai", "--message-format=json"]);
    assert_eq!(code, 1, "stderr: {err}");
    assert!(out.contains("FAI3004"), "expected FAI3004 in {out}");
}

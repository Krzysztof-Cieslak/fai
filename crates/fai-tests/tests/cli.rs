//! Golden snapshots of CLI output, driven in-process through `fai_cli::run`.

use camino::Utf8PathBuf;
use indoc::indoc;

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
    let (code, out, err) =
        run(&["fai", "check", "--no-daemon", "-C", dir.as_str(), "--message-format=json"]);
    assert_eq!(code, 0, "stderr: {err}");
    insta::assert_snapshot!("check_json", out);
}

#[test]
fn check_human_output() {
    let dir = empty_workspace();
    let (code, out, err) =
        run(&["fai", "check", "--no-daemon", "-C", dir.as_str(), "--color=never"]);
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
        indoc! {r#"
            module Calc

            public add : Int -> Int -> Int
            let add x y = x + y
        "#},
    )
    .unwrap();
    dir
}

#[test]
fn query_type_json_output() {
    let dir = typed_workspace();
    let (code, out, err) =
        run(&["fai", "query", "--no-daemon", "-C", dir.as_str(), "type", "Calc.add"]);
    assert_eq!(code, 0, "stderr: {err}");
    insta::assert_snapshot!("query_type_calc_add", out);
}

#[test]
fn check_reports_type_error() {
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .expect("temp dir is UTF-8")
        .join("fai-m2-cli-typeerr");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Bad.fai"),
        indoc! {r#"
            module Bad

            public f : Int -> Bool
            let f x = x + 1
        "#},
    )
    .unwrap();
    let (code, out, err) = run(&[
        "fai",
        "check",
        "--no-daemon",
        "-C",
        dir.as_str(),
        "Bad.fai",
        "--message-format=json",
    ]);
    assert_eq!(code, 1, "stderr: {err}");
    assert!(out.contains("FAI3004"), "expected FAI3004 in {out}");
}

/// A workspace with a nested module, for nested code-intelligence queries.
fn nested_workspace() -> Utf8PathBuf {
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .expect("temp dir is UTF-8")
        .join("fai-nested-cli-tests");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Nest.fai"),
        indoc! {r#"
            module Nest

            module Inner =
              public val : Int
              let val = 1
        "#},
    )
    .unwrap();
    dir
}

#[test]
fn query_type_of_nested_member_via_dotted_path() {
    let dir = nested_workspace();
    let (code, out, err) =
        run(&["fai", "query", "--no-daemon", "-C", dir.as_str(), "type", "Nest.Inner.val"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("Int"), "expected the nested member's type, got {out}");
}

#[test]
fn query_caps_reports_capability_footprint() {
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir())
        .expect("temp dir is UTF-8")
        .join("fai-caps-cli-tests");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Cap.fai"),
        indoc! {r#"
            module Cap

            public greet : { console : Console | _ } -> String -> Unit
            let greet env name = env.console.writeLine name

            public run : Runtime -> Unit
            let run rt = greet rt "hi"
        "#},
    )
    .unwrap();
    // `greet` requests `console` directly (a parameter capability).
    let (code, out, err) =
        run(&["fai", "query", "--no-daemon", "-C", dir.as_str(), "caps", "Cap.greet"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("\"console\""), "expected the console capability: {out}");
    assert!(out.contains("Console") && out.contains("\"parameter\""), "{out}");
    // `run` takes a full Runtime, so its footprint includes the console too.
    let (code, out, err) =
        run(&["fai", "query", "--no-daemon", "-C", dir.as_str(), "caps", "Cap.run"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("\"console\""), "Runtime exposes console: {out}");
}

#[test]
fn query_search_finds_functions_by_type() {
    // Search covers the standard library; `List.length : List 'a -> Int` is an
    // exact match for the pattern.
    let dir = empty_workspace();
    let (code, out, err) =
        run(&["fai", "query", "--no-daemon", "-C", dir.as_str(), "search", "List 'a -> Int"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("length"), "expected List.length among results: {out}");
}

#[test]
fn query_outline_nests_modules() {
    let dir = nested_workspace();
    let (code, out, err) =
        run(&["fai", "query", "--no-daemon", "-C", dir.as_str(), "outline", "Nest"]);
    assert_eq!(code, 0, "stderr: {err}");
    assert!(out.contains("\"module\""), "outline should mark the nested module: {out}");
    assert!(out.contains("\"children\""), "outline should nest children: {out}");
    assert!(out.contains("val"), "outline should include the nested member: {out}");
}

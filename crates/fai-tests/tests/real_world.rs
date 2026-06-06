//! Advanced real-world end-to-end tests.
//!
//! Each `tests/fixtures/native/*.fai` is a self-contained program in the M3
//! subset (recursion, higher-order functions, closures, arithmetic with
//! overflow boxing, strings, and the Console capability). It declares its
//! expected stdout inline with `//~ OUTPUT <line>` annotations. The runner
//! compiles each program with `fai build` and runs the produced binary,
//! asserting the output and a clean, leak-free exit.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native")
}

/// The expected stdout, assembled from the `//~ OUTPUT` annotation lines.
fn expected_output(src: &str) -> String {
    let mut out = String::new();
    for line in src.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("//~ OUTPUT ") {
            out.push_str(rest);
            out.push('\n');
        } else if trimmed == "//~ OUTPUT" {
            out.push('\n');
        }
    }
    out
}

fn unique_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fai-real-world-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Compiles `src` (named `file_name`) and runs it, returning `(stdout, exit)`.
fn build_and_run(file_name: &str, src: &str) -> (String, Option<i32>) {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(file_name), src).unwrap();
    let exe = dir.join("prog");

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = fai_cli::run(
        [
            "fai",
            "build",
            "--no-daemon",
            "-C",
            dir.to_str().unwrap(),
            file_name,
            "--out",
            exe.to_str().unwrap(),
        ],
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "building {file_name} failed: {}", String::from_utf8_lossy(&err));

    // `fai build` appends the platform executable extension (`.exe` on Windows).
    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = Command::new(&produced).output().unwrap();
    (String::from_utf8_lossy(&run.stdout).into_owned(), run.status.code())
}

/// Reads every native fixture as `(name, source, expected_output)`.
fn fixtures() -> Vec<(String, String, String)> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("native fixtures directory exists")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("fai"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "expected real-world .fai fixtures");
    files
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_str().unwrap().to_owned();
            let src = std::fs::read_to_string(&path).unwrap();
            let expected = expected_output(&src);
            assert!(!expected.is_empty(), "{name} has no //~ OUTPUT annotations");
            (name, src, expected)
        })
        .collect()
}

#[test]
fn real_world_programs_run_under_the_jit() {
    use fai_db::Db;

    for (name, src, expected) in fixtures() {
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(name.clone().into(), src);
        let file = db.source_file(id).unwrap();

        fai_runtime::capture_start();
        let outcome = fai_driver::jit_run_program(&db, file);
        let out = fai_runtime::capture_take();

        assert_eq!(outcome.exit_code, 0, "{name}: JIT run should succeed");
        assert_eq!(out, expected, "{name}: JIT output should match the AOT output");
    }
}

#[test]
fn real_world_programs_build_and_run_correctly() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(fixtures_dir())
        .expect("native fixtures directory exists")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("fai"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "expected real-world .fai fixtures");

    for path in files {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        let expected = expected_output(&src);
        assert!(!expected.is_empty(), "{name} has no //~ OUTPUT annotations");

        let (stdout, exit) = build_and_run(&name, &src);
        assert_eq!(stdout, expected, "{name}: output mismatch");
        assert_eq!(exit, Some(0), "{name}: should exit cleanly with no leaks");
    }
}

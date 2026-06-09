//! Render captured divan benchmark output into a Markdown report (for the CI run
//! summary) and an optional JSON file (an artifact, and a basis for later trend
//! tracking).
//!
//! Usage: `bench-summary [INPUT] [JSON_OUT]`
//!   * `INPUT`    — the captured divan output file; reads stdin when omitted.
//!   * `JSON_OUT` — where to write the parsed JSON; skipped when omitted.
//!
//! The Markdown is appended to the file named by `$GITHUB_STEP_SUMMARY` (so it
//! appears on the run page) or printed to stdout when that variable is unset.
//! Source-location cases link to the host forge via `$GITHUB_SERVER_URL`,
//! `$GITHUB_REPOSITORY`, and `$GITHUB_SHA`. This tool never fails the build: any
//! I/O error degrades to a note on stderr and a success exit.

use std::io::Read as _;

use fai_tests::bench_summary::{LinkBase, Report};

fn main() {
    let mut args = std::env::args().skip(1);
    let input_path = args.next();
    let json_path = args.next();

    let raw = match read_input(input_path.as_deref()) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("bench-summary: could not read input: {err}");
            String::new()
        }
    };

    let report = Report::parse(&raw);
    let markdown = report.to_markdown(&LinkBase::from_env());

    if let Ok(summary) = std::env::var("GITHUB_STEP_SUMMARY") {
        if let Err(err) = append(&summary, &markdown) {
            eprintln!("bench-summary: could not write summary: {err}");
            print!("{markdown}");
        }
    } else {
        print!("{markdown}");
    }

    if let Some(path) = json_path
        && let Err(err) = std::fs::write(&path, report.to_json())
    {
        eprintln!("bench-summary: could not write JSON to {path}: {err}");
    }
}

/// Reads the captured output from `path`, or stdin when `path` is `None`.
fn read_input(path: Option<&str>) -> std::io::Result<String> {
    match path {
        Some(path) => std::fs::read_to_string(path),
        None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            Ok(buf)
        }
    }
}

/// Appends `text` to the file at `path` (the GitHub step summary).
fn append(path: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(text.as_bytes())
}

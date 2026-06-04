//! The `samples/` corpus is the language-by-example tour and a self-hosted check
//! that the docs cannot drift: every implemented-surface file must parse cleanly
//! and be canonically formatted; every other file may only fail because of
//! not-yet-supported constructs (`FAI1030`), never a real syntax error.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use fai_span::SourceId;
use fai_syntax::parse_module;

fn samples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples")
}

fn fmt(src: &str) -> String {
    let parsed = parse_module(SourceId::new(0), src);
    fai_fmt::format(&parsed.module, &parsed.comments, src)
}

#[test]
fn samples_round_trip_or_are_future_surface() {
    let mut clean = BTreeSet::new();
    let mut files: Vec<PathBuf> = std::fs::read_dir(samples_dir())
        .expect("samples/ directory exists")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("fai"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "expected sample .fai files");

    for path in files {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_module(SourceId::new(0), &src);
        let codes: Vec<&str> = parsed.diagnostics.iter().map(|d| d.code.as_str()).collect();

        if codes.is_empty() {
            // Implemented surface: must already be canonical and idempotent.
            let formatted = fai_fmt::format(&parsed.module, &parsed.comments, &src);
            assert_eq!(formatted, src, "{name} is not canonically formatted (run `fai fmt`)");
            assert_eq!(fmt(&formatted), formatted, "{name} formatting is not idempotent");
            clean.insert(name);
        } else {
            // Future surface: the only reason it does not parse is unsupported
            // constructs, not a real syntax error.
            assert!(
                codes.contains(&"FAI1030"),
                "{name} has a real syntax error (no FAI1030): {codes:?}",
            );
        }
    }

    // Implemented-surface modules must stay in the clean, round-tripping set;
    // later milestones add their modules here as features land.
    for expected in [
        "Algebra.fai",
        "Basics.fai",
        "Comments.fai",
        "Funcs.fai",
        "Hello.fai",
        "Locals.fai",
        "Math.fai",
        "Tuples.fai",
    ] {
        assert!(clean.contains(expected), "{expected} should parse cleanly and round-trip");
    }
}

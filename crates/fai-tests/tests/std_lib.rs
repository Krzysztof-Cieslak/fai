//! The `std/` standard library is embedded into the compiler, so it must not
//! drift: every module must parse cleanly and be canonically formatted (so a
//! contributor's edit is checked the same way as the `samples/` tour). Type
//! checking of the embedded library is covered by `fai-types`'
//! `embedded_std_library_typechecks`.

use std::path::{Path, PathBuf};

use fai_span::SourceId;
use fai_syntax::parse_module;

fn std_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std")
}

/// Whether `haystack` contains `name` as a whole word.
fn mentions(haystack: &str, name: &str) -> bool {
    let boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric() && c != '_');
    haystack.match_indices(name).any(|(i, _)| {
        boundary(haystack[..i].chars().next_back())
            && boundary(haystack[i + name.len()..].chars().next())
    })
}

#[test]
fn every_public_std_function_has_an_example() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(std_dir())
        .expect("std/ directory exists")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("fai"))
        .collect();
    files.sort();

    for path in files {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        let examples: Vec<&str> =
            src.lines().map(str::trim_start).filter(|l| l.starts_with("example:")).collect();

        // Public value bindings carry a `public <name> :` signature (a `public
        // type …` declaration is not a function and is skipped).
        for line in src.lines() {
            let Some(rest) = line.trim_start().strip_prefix("public ") else { continue };
            let head = rest.split_whitespace().next().unwrap_or("");
            if head == "type" || !rest.contains(':') {
                continue;
            }
            let fn_name = head.trim_end_matches(':');
            assert!(
                examples.iter().any(|e| mentions(e, fn_name)),
                "{name}: public `{fn_name}` has no `example:` (every std function needs one)"
            );
        }
    }
}

#[test]
fn std_modules_parse_clean_and_are_canonical() {
    let mut files: Vec<PathBuf> = std::fs::read_dir(std_dir())
        .expect("std/ directory exists")
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("fai"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "expected std/ .fai modules");

    for path in files {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        let parsed = parse_module(SourceId::new(0), &src);
        let codes: Vec<&str> = parsed.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.is_empty(), "{name} should parse with no diagnostics, got {codes:?}");

        let formatted = fai_fmt::format(&parsed.module, &parsed.comments, &src);
        assert_eq!(formatted, src, "{name} is not canonically formatted (run `fai fmt std`)");
        let reparsed = parse_module(SourceId::new(0), &formatted);
        let twice = fai_fmt::format(&reparsed.module, &reparsed.comments, &formatted);
        assert_eq!(twice, formatted, "{name} formatting is not idempotent");
    }
}

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

/// Every `.fai` module under `std/`, recursively (so subfolders like
/// `datetime/` are covered by the same gates), sorted for determinism.
fn std_fai_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("std/ directory exists") {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("fai") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&std_dir(), &mut files);
    files.sort();
    files
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
    for path in std_fai_files() {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let src = std::fs::read_to_string(&path).unwrap();
        let examples: Vec<&str> =
            src.lines().map(str::trim_start).filter(|l| l.starts_with("example:")).collect();

        // The public capability instances and the `Runtime` bundle are effectful
        // values, not pure functions: a contract cannot reference a capability
        // (FAI6004), so they cannot carry an `example`. They are exported only so a
        // program can compose its own extended `Runtime`.
        const EXAMPLE_EXEMPT: &[&str] = &[
            "stdConsole",
            "stdClock",
            "stdRandom",
            "stdFs",
            "stdEnv",
            "stdConcurrency",
            "stdNet",
            "stdTls",
            "defaultRuntime",
        ];

        // The capability interface types. A public function that takes (or returns)
        // a capability — e.g. the `Async` concurrency combinators, which thread the
        // `Concurrency` capability — cannot carry a contract either: the contract
        // would have to reference a capability value, which is FAI6004. Such
        // functions are covered by end-to-end tests instead. Keyed off the same rule
        // as FAI6004 (an interface that carries an effect is a capability).
        const CAPABILITY_TYPES: &[&str] = &[
            "Console", "Clock", "Random", "FileSystem", "Env", "Concurrency", "Net", "Tls",
            "Runtime",
        ];

        // Public value bindings carry a `public <name> :` signature (a `public
        // type …` declaration is not a function and is skipped).
        for line in src.lines() {
            let Some(rest) = line.trim_start().strip_prefix("public ") else { continue };
            let head = rest.split_whitespace().next().unwrap_or("");
            if head == "type" || !rest.contains(':') {
                continue;
            }
            let fn_name = head.trim_end_matches(':');
            if EXAMPLE_EXEMPT.contains(&fn_name) {
                continue;
            }
            // A function whose signature names a capability cannot have a (pure)
            // contract — passing the capability would be FAI6004 — so it is exempt.
            if CAPABILITY_TYPES.iter().any(|cap| mentions(rest, cap)) {
                continue;
            }
            assert!(
                examples.iter().any(|e| mentions(e, fn_name)),
                "{name}: public `{fn_name}` has no `example:` (every std function needs one)"
            );
        }
    }
}

#[test]
fn std_modules_parse_clean_and_are_canonical() {
    let files = std_fai_files();
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

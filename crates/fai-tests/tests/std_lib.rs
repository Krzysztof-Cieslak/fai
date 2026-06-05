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

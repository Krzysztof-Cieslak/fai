//! The diagnostic-code catalog: every code across the workspace must be
//! well-formed (`FAInnnn`), globally unique, and documented. This test also
//! *renders* the human catalog (`docs/ERROR_CODES.md`) from the per-crate
//! `CODES` tables, so the published catalog cannot drift from the code.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use fai_diagnostics::CodeInfo;

fn all_codes() -> Vec<CodeInfo> {
    let mut codes = Vec::new();
    codes.extend_from_slice(fai_diagnostics::CODES);
    codes.extend_from_slice(fai_driver::CODES);
    codes.extend_from_slice(fai_syntax::CODES);
    codes.extend_from_slice(fai_resolve::CODES);
    codes.extend_from_slice(fai_types::CODES);
    codes.extend_from_slice(fai_core::CODES);
    codes.extend_from_slice(fai_contracts::CODES);
    codes
}

#[test]
fn codes_are_well_formed_unique_and_documented() {
    let codes = all_codes();
    assert!(!codes.is_empty(), "expected at least one diagnostic code");

    let mut seen = BTreeSet::new();
    for info in &codes {
        assert!(info.code.has_valid_format(), "malformed diagnostic code: {}", info.code);
        assert!(seen.insert(info.code.as_str()), "duplicate diagnostic code: {}", info.code);
        assert!(!info.title.is_empty(), "code {} has an empty title", info.code);
        assert!(
            !info.explanation.trim().is_empty(),
            "code {} has no catalog explanation",
            info.code
        );
    }
}

/// The repository root (two levels up from this crate).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// The heading for a `FAINxxx` range (`n` is the digit after `FAI`).
fn range_heading(digit: u8) -> &'static str {
    match digit {
        b'0' => "FAI0xxx — Tooling, CLI & driver",
        b'1' => "FAI1xxx — Lexing & parsing",
        b'2' => "FAI2xxx — Name resolution & visibility",
        b'3' => "FAI3xxx — Types & rows",
        b'4' => "FAI4xxx — Exhaustiveness & patterns",
        b'5' => "FAI5xxx — Capabilities",
        b'6' => "FAI6xxx — Contracts",
        b'7' => "FAI7xxx — Native backend",
        _ => "Other",
    }
}

/// Renders the error-code catalog markdown from the aggregated codes.
fn render_catalog(codes: &[CodeInfo]) -> String {
    let mut sorted: Vec<&CodeInfo> = codes.iter().collect();
    sorted.sort_by_key(|i| i.code.as_str());

    let mut out = String::new();
    out.push_str("# Fai error-code catalog\n\n");
    out.push_str(
        "<!-- Generated from each crate's `CODES` table by \
         `crates/fai-tests/tests/catalog.rs`. Do not edit by hand; regenerate with \
         `UPDATE_ERROR_CODES=1 cargo test -p fai-tests --test catalog`. -->\n\n",
    );
    out.push_str(
        "Every diagnostic Fai emits carries a stable `FAInnnn` code. Codes are a public, \
         versioned API: they are never renumbered, and are allocated by compiler phase. Each \
         entry below lists the code, its default severity, and what triggers it.\n",
    );

    let mut current: Option<u8> = None;
    for info in sorted {
        let digit = info.code.as_str().as_bytes()[3];
        if current != Some(digit) {
            out.push_str(&format!("\n## {}\n", range_heading(digit)));
            current = Some(digit);
        }
        out.push_str(&format!("\n### {} — {}\n\n", info.code, info.title));
        out.push_str(&format!("**Severity:** {}\n\n", info.default_severity.as_str()));
        out.push_str(info.explanation.trim());
        out.push('\n');
    }
    out
}

#[test]
fn error_code_catalog_is_up_to_date() {
    let expected = render_catalog(&all_codes());
    let path = repo_root().join("docs/ERROR_CODES.md");
    if std::env::var_os("UPDATE_ERROR_CODES").is_some() {
        std::fs::write(&path, &expected).unwrap();
        return;
    }
    let actual = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "docs/ERROR_CODES.md is out of date; regenerate with \
         `UPDATE_ERROR_CODES=1 cargo test -p fai-tests --test catalog`"
    );
}

/// Collects every `FAInnnn` defined as a `pub const … DiagnosticCode::new("…")`
/// in any crate's source (the real code constants, not test fixtures).
fn collect_defined_codes(dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) != Some("target") {
                collect_defined_codes(&path, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            for line in text.lines() {
                if line.contains("pub const")
                    && let Some(code) = extract_code(line)
                {
                    out.insert(code);
                }
            }
        }
    }
}

/// Extracts the `FAInnnn` argument of a `DiagnosticCode::new("…")` on `line`.
fn extract_code(line: &str) -> Option<String> {
    let start = line.find("DiagnosticCode::new(\"")? + "DiagnosticCode::new(\"".len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    let code = &rest[..end];
    code.starts_with("FAI").then(|| code.to_owned())
}

#[test]
fn every_defined_code_is_catalogued() {
    let catalog: BTreeSet<&str> = all_codes().iter().map(|i| i.code.as_str()).collect();
    let mut defined = BTreeSet::new();
    collect_defined_codes(&repo_root().join("crates"), &mut defined);
    assert!(!defined.is_empty(), "expected to find code constant definitions in the sources");
    for code in &defined {
        assert!(
            catalog.contains(code.as_str()),
            "code {code} is defined in source but is missing from any crate's CODES table \
             (and so from the catalog); add a CodeInfo entry for it"
        );
    }
}

#[test]
fn tooling_codes_are_in_the_fai0xxx_range() {
    for info in fai_driver::CODES {
        assert!(
            info.code.as_str().starts_with("FAI0"),
            "tooling code {} should be in the FAI0xxx range",
            info.code
        );
    }
}

#[test]
fn resolve_codes_are_in_the_fai2xxx_range() {
    for info in fai_resolve::CODES {
        assert!(
            info.code.as_str().starts_with("FAI2"),
            "resolve code {} should be in the FAI2xxx range",
            info.code
        );
    }
}

#[test]
fn contract_codes_are_in_the_fai6xxx_range() {
    for info in fai_contracts::CODES {
        assert!(
            info.code.as_str().starts_with("FAI6"),
            "contract code {} should be in the FAI6xxx range",
            info.code
        );
    }
}

#[test]
fn type_codes_are_in_the_type_or_pattern_ranges() {
    // The type system owns types/rows (FAI3xxx), exhaustiveness/patterns
    // (FAI4xxx), and the capability *effect* checks (FAI5xxx).
    for info in fai_types::CODES {
        let code = info.code.as_str();
        assert!(
            code.starts_with("FAI3") || code.starts_with("FAI4") || code.starts_with("FAI5"),
            "type code {} should be in the FAI3xxx, FAI4xxx, or FAI5xxx range",
            info.code
        );
    }
}

//! The diagnostic-code catalog: every code across the workspace must be
//! well-formed (`FAInnnn`) and globally unique.
//!
//! As phase crates start allocating codes, add their `CODES` slices here.

use std::collections::BTreeSet;

use fai_diagnostics::CodeInfo;

fn all_codes() -> Vec<CodeInfo> {
    let mut codes = Vec::new();
    codes.extend_from_slice(fai_diagnostics::CODES);
    codes.extend_from_slice(fai_driver::CODES);
    codes.extend_from_slice(fai_syntax::CODES);
    codes.extend_from_slice(fai_resolve::CODES);
    codes.extend_from_slice(fai_types::CODES);
    codes
}

#[test]
fn codes_are_well_formed_and_unique() {
    let codes = all_codes();
    assert!(!codes.is_empty(), "expected at least one diagnostic code");

    let mut seen = BTreeSet::new();
    for info in &codes {
        assert!(info.code.has_valid_format(), "malformed diagnostic code: {}", info.code);
        assert!(seen.insert(info.code.as_str()), "duplicate diagnostic code: {}", info.code);
        assert!(!info.title.is_empty(), "code {} has an empty title", info.code);
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
fn type_codes_are_in_the_fai3xxx_range() {
    for info in fai_types::CODES {
        assert!(
            info.code.as_str().starts_with("FAI3"),
            "type code {} should be in the FAI3xxx range",
            info.code
        );
    }
}

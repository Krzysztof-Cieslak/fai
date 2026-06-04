//! Diagnostic model, stable `FAInnnn` error codes, and renderers.
//!
//! One model, two renderers: a human renderer ([`render_human`], color-aware)
//! and a JSON wire schema ([`mod@wire`]) behind `--message-format=json`. The
//! JSON schema is a stable, versioned API (see [`SCHEMA_VERSION`]). Spans are
//! resolved to file/line/column through [`fai_span::SpanResolver`], so this
//! crate never depends on the query engine.
//!
//! Diagnostic codes are allocated by phase and owned by each phase crate as a
//! `pub const CODES: &[CodeInfo]` slice; the `fai-tests` crate
//! aggregates them to enforce format and uniqueness.

mod code;
mod diagnostic;
mod order;
mod render;
mod severity;
pub mod wire;

pub use code::{CodeInfo, DiagnosticCode};
pub use diagnostic::{Diagnostic, Label, Suggestion};
pub use render::render_human;
pub use severity::Severity;

/// Version of the JSON output schema (`cli.md` §3). Bumped only on breaking
/// changes; additive fields stay within a major version.
pub const SCHEMA_VERSION: u32 = 1;

/// Diagnostic codes owned by this crate. `fai-diagnostics` defines the code
/// machinery but emits no codes of its own.
pub const CODES: &[CodeInfo] = &[];

#[cfg(test)]
mod tests {
    use fai_span::{ByteOffset, SourceMap, Span, TextRange};

    use super::*;

    const TEST_CODE: DiagnosticCode = DiagnosticCode::new("FAI0001");

    fn fixture() -> (SourceMap, Span) {
        let mut map = SourceMap::new();
        let id = map.add("src/Main.fai".into(), "let x =\n  bad\n".to_owned());
        let span = Span::new(id, TextRange::new(ByteOffset::new(10), ByteOffset::new(13)));
        (map, span)
    }

    #[test]
    fn code_format_validation() {
        assert!(DiagnosticCode::new("FAI0001").has_valid_format());
        assert!(DiagnosticCode::new("FAI6000").has_valid_format());
        assert!(!DiagnosticCode::new("FAI001").has_valid_format());
        assert!(!DiagnosticCode::new("FA10001").has_valid_format());
        assert!(!DiagnosticCode::new("FAIabcd").has_valid_format());
        assert!(!DiagnosticCode::new("XYZ0001").has_valid_format());
    }

    #[test]
    fn severity_strings() {
        assert_eq!(Severity::Error.as_str(), "error");
        assert_eq!(Severity::Warning.as_str(), "warning");
        assert_eq!(Severity::Info.as_str(), "info");
    }

    #[test]
    fn wire_conversion_resolves_positions() {
        let (map, span) = fixture();
        let diag = Diagnostic::error(TEST_CODE, "bad thing", span).with_help("try harder");
        let wire = wire::diagnostic_to_wire(&diag, &map);
        assert_eq!(wire.code, "FAI0001");
        assert_eq!(wire.severity, Severity::Error);
        assert_eq!(wire.primary.file, "src/Main.fai");
        assert_eq!(wire.primary.start.line, 2);
        assert_eq!(wire.primary.start.column, 3);
        assert_eq!(wire.primary.byte_start, 10);
        assert_eq!(wire.primary.byte_end, 13);
        assert_eq!(wire.help.as_deref(), Some("try harder"));
    }

    #[test]
    fn human_render_no_color() {
        let (map, span) = fixture();
        let diag = Diagnostic::error(TEST_CODE, "not implemented", span).with_help("later");
        let out = render_human(std::slice::from_ref(&diag), &map, false);
        assert!(out.contains("error[FAI0001]: not implemented"));
        assert!(out.contains("--> src/Main.fai:2:3"));
        assert!(out.contains("= help: later"));
        // No ANSI escapes when color is disabled.
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn human_render_color_has_ansi() {
        let (map, span) = fixture();
        let diag = Diagnostic::error(TEST_CODE, "boom", span);
        let out = render_human(std::slice::from_ref(&diag), &map, true);
        assert!(out.contains('\x1b'));
    }

    #[test]
    fn deterministic_order_by_position() {
        let mut map = SourceMap::new();
        let id = map.add("a.fai".into(), "0123456789".to_owned());
        let late = Span::new(id, TextRange::new(ByteOffset::new(5), ByteOffset::new(6)));
        let early = Span::new(id, TextRange::new(ByteOffset::new(1), ByteOffset::new(2)));
        let diags = vec![
            Diagnostic::error(TEST_CODE, "late", late),
            Diagnostic::error(TEST_CODE, "early", early),
        ];
        let wire = wire::to_wire(&diags, &map);
        assert_eq!(wire[0].message, "early");
        assert_eq!(wire[1].message, "late");
    }

    #[test]
    fn unknown_span_degrades() {
        use fai_span::SourceId;
        let map = SourceMap::new();
        let span =
            Span::new(SourceId::new(0), TextRange::new(ByteOffset::ZERO, ByteOffset::new(1)));
        let diag = Diagnostic::error(TEST_CODE, "orphan", span);
        let wire = wire::diagnostic_to_wire(&diag, &map);
        assert_eq!(wire.primary.file, "<unknown>");
        assert_eq!(wire.primary.byte_start, 0);
        assert_eq!(wire.primary.byte_end, 1);
    }
}

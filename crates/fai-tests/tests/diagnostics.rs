//! Golden snapshots of the diagnostic renderers (human + JSON wire schema).

use fai_diagnostics::{Diagnostic, DiagnosticCode, Label, render_human, wire};
use fai_span::{ByteOffset, SourceMap, Span, TextRange};

fn fixture() -> (SourceMap, Vec<Diagnostic>) {
    let mut map = SourceMap::new();
    let id = map.add("src/Main.fai".into(), "let x =\n  bad + 1\nlet y = 2\n".to_owned());
    let span = |start: u32, end: u32| {
        Span::new(id, TextRange::new(ByteOffset::new(start), ByteOffset::new(end)))
    };
    let diagnostics = vec![
        Diagnostic::error(DiagnosticCode::new("FAI3001"), "type mismatch", span(10, 13))
            .with_help("expected `Int`")
            .with_label(Label::new(span(0, 3), "in this binding")),
        Diagnostic::warning(DiagnosticCode::new("FAI2001"), "unused binding `y`", span(22, 23)),
    ];
    (map, diagnostics)
}

#[test]
fn human_rendering() {
    let (map, diagnostics) = fixture();
    insta::assert_snapshot!("human", render_human(&diagnostics, &map, false));
}

#[test]
fn json_rendering() {
    let (map, diagnostics) = fixture();
    let json = serde_json::to_string_pretty(&wire::to_wire(&diagnostics, &map)).unwrap();
    insta::assert_snapshot!("json", json);
}

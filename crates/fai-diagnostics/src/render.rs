//! The human-readable renderer.
//!
//! Minimal and hand-rolled: `severity[CODE]: message`, a
//! `-->` location line, then secondary labels, help, and suggestions. Source
//! carets/snippets are deferred. Color is controlled by the caller (`fai-cli`
//! resolves `--color`/`NO_COLOR`/tty); this function just takes a `bool`.

use std::fmt::Write as _;

use fai_span::SpanResolver;

use crate::diagnostic::Diagnostic;
use crate::order::sort_order;
use crate::severity::Severity;

fn sgr(color: bool, codes: &str, text: &str) -> String {
    if color { format!("\x1b[{codes}m{text}\x1b[0m") } else { text.to_owned() }
}

fn severity_sgr(severity: Severity) -> &'static str {
    match severity {
        Severity::Error => "1;31",
        Severity::Warning => "1;33",
        Severity::Info => "1;36",
    }
}

fn location(span: fai_span::Span, resolver: &dyn SpanResolver) -> String {
    match resolver.resolve(span) {
        Some(r) => format!("{}:{}:{}", r.path, r.start.line, r.start.column),
        None => String::from("<unknown>"),
    }
}

/// Renders one diagnostic into `out`.
fn render_one(out: &mut String, diag: &Diagnostic, resolver: &dyn SpanResolver, color: bool) {
    let header = sgr(
        color,
        severity_sgr(diag.severity),
        &format!("{}[{}]", diag.severity.as_str(), diag.code),
    );
    // `write!` to a String is infallible.
    let _ = writeln!(out, "{header}: {}", diag.message);
    let _ = writeln!(out, "  --> {}", location(diag.primary, resolver));
    for label in &diag.secondary {
        let _ = writeln!(out, "  note: {}", label.message);
        let _ = writeln!(out, "   --> {}", location(label.span, resolver));
    }
    if let Some(help) = &diag.help {
        let _ = writeln!(out, "  = {} {help}", sgr(color, "1;32", "help:"));
    }
    for suggestion in &diag.suggestions {
        let _ = writeln!(out, "  = suggestion: replace with `{}`", suggestion.replacement);
    }
}

/// Renders a batch of diagnostics to a human-readable string.
///
/// Diagnostics are ordered deterministically by `(file, byte_start, code)` and
/// separated by a blank line.
#[must_use]
pub fn render_human(diags: &[Diagnostic], resolver: &dyn SpanResolver, color: bool) -> String {
    let mut out = String::new();
    for (n, i) in sort_order(diags, resolver).into_iter().enumerate() {
        if n > 0 {
            out.push('\n');
        }
        render_one(&mut out, &diags[i], resolver, color);
    }
    out
}

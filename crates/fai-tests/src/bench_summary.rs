//! Render divan's textual benchmark output into a Markdown report and a small
//! JSON document.
//!
//! divan has no machine-readable output, so this parses its Unicode tree (the
//! `├─`/`╰─`/`│` drawing and the ` │ `-separated `fastest/slowest/median/mean/
//! samples/iters` columns). Parsing is best-effort and never panics: an
//! unrecognized line is skipped, so a divan format change degrades to a thinner
//! report rather than a failure.
//!
//! A benchmark *case* label that looks like a source location (`<path>.fai#Lnn`,
//! produced by the real-world language-server benches) is linked to its file on
//! the host forge, so the report points at the exact code each row measured.

/// One parsed benchmark measurement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The bench binary / group (e.g. `lsp`, `contracts`).
    pub group: String,
    /// The benchmark function (the first tree level).
    pub bench: String,
    /// The argument label(s) (deeper tree levels), empty for an argument-less
    /// benchmark. For the real-world LSP benches this is a `<path>.fai#Lnn`.
    pub case: String,
    /// The median time, as divan rendered it (e.g. `3.238 ms`).
    pub median: String,
    /// The mean time.
    pub mean: String,
    /// The recorded sample count.
    pub samples: String,
}

/// A parsed benchmark report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Every measured row, in output order.
    pub rows: Vec<Row>,
}

/// The base URL for source links (`<server>/<repo>/blob/<sha>`), if known.
#[derive(Debug, Clone, Default)]
pub struct LinkBase {
    base: Option<String>,
}

impl LinkBase {
    /// Builds the link base from the GitHub Actions environment, or yields a
    /// no-link base when it is absent (local runs render bare labels).
    #[must_use]
    pub fn from_env() -> Self {
        let server =
            std::env::var("GITHUB_SERVER_URL").unwrap_or_else(|_| "https://github.com".to_owned());
        let base = match (std::env::var("GITHUB_REPOSITORY"), std::env::var("GITHUB_SHA")) {
            (Ok(repo), Ok(sha)) if !repo.is_empty() && !sha.is_empty() => {
                Some(format!("{server}/{repo}/blob/{sha}"))
            }
            _ => None,
        };
        Self { base }
    }

    /// Renders a case label as a Markdown table cell, linking source locations.
    fn cell(&self, case: &str) -> String {
        if case.is_empty() {
            return String::new();
        }
        match &self.base {
            Some(base) if case.contains(".fai") => format!("[{}]({base}/{case})", escape(case)),
            _ => escape(case),
        }
    }
}

impl Report {
    /// Parses divan's captured (possibly multi-binary) output.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut rows = Vec::new();
        let mut group = String::new();
        let mut stack: Vec<String> = Vec::new();

        for line in text.lines() {
            if is_header(line) {
                group = first_field(line);
                stack.clear();
                continue;
            }
            let Some((depth, rest)) = branch_split(line) else { continue };
            let name = first_field(rest);
            if name.is_empty() {
                continue;
            }
            stack.truncate(depth);
            stack.push(name);

            // The ` │ `-separated columns after the name: [name+fastest, slowest,
            // median, mean, samples, iters]. A parent node leaves them blank.
            let cols: Vec<&str> = rest.split('│').collect();
            let median = cols.get(2).map(|c| c.trim()).unwrap_or("");
            if median.is_empty() {
                continue;
            }
            let mean = cols.get(3).map(|c| c.trim()).unwrap_or("");
            let samples = cols.get(4).map(|c| c.trim()).unwrap_or("");
            let bench = stack.first().cloned().unwrap_or_default();
            let case = if stack.len() > 1 { stack[1..].join(" / ") } else { String::new() };
            rows.push(Row {
                group: group.clone(),
                bench,
                case,
                median: median.to_owned(),
                mean: mean.to_owned(),
                samples: samples.to_owned(),
            });
        }

        Self { rows }
    }

    /// Renders the report as Markdown: one collapsible table per bench group.
    #[must_use]
    pub fn to_markdown(&self, links: &LinkBase) -> String {
        use std::fmt::Write as _;
        let mut out = String::from("## Benchmark results\n\n");
        if self.rows.is_empty() {
            out.push_str("_No benchmark results were parsed from the divan output._\n");
            return out;
        }

        let mut groups: Vec<&str> = Vec::new();
        for row in &self.rows {
            if !groups.contains(&row.group.as_str()) {
                groups.push(&row.group);
            }
        }

        for group in groups {
            let _ = write!(out, "<details><summary><b>{}</b></summary>\n\n", escape(group));
            out.push_str("| Benchmark | Case | Median | Mean | Samples |\n");
            out.push_str("| --- | --- | --: | --: | --: |\n");
            for row in self.rows.iter().filter(|r| r.group == group) {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} |",
                    escape(&row.bench),
                    links.cell(&row.case),
                    escape(&row.median),
                    escape(&row.mean),
                    escape(&row.samples),
                );
            }
            out.push_str("\n</details>\n\n");
        }
        out
    }

    /// Renders the rows as a compact JSON array (hand-rolled: the crate's
    /// non-dev dependencies do not include a serializer).
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::from("[");
        for (i, row) in self.rows.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"group\":{},\"benchmark\":{},\"case\":{},\"median\":{},\"mean\":{},\"samples\":{}}}",
                json_str(&row.group),
                json_str(&row.bench),
                json_str(&row.case),
                json_str(&row.median),
                json_str(&row.mean),
                json_str(&row.samples),
            ));
        }
        out.push(']');
        out
    }
}

/// Whether `line` is divan's column header (which also names the group).
fn is_header(line: &str) -> bool {
    line.contains("fastest") && line.contains("slowest") && line.contains("median")
}

/// Splits a tree row into its `(depth, text-after-the-branch-marker)`, or `None`
/// when the line carries no `├─`/`╰─` marker (headers, blanks, counter
/// continuation rows, cargo's own lines).
fn branch_split(line: &str) -> Option<(usize, &str)> {
    const MARKER_LEN: usize = "├─ ".len(); // '├','─',' ' = 7 bytes; '╰─ ' is the same
    let pos = line.find("├─ ").or_else(|| line.find("╰─ "))?;
    // Each ancestor level is a 3-character unit (`│  ` or `   `).
    let depth = line[..pos].chars().count() / 3;
    Some((depth, &line[pos + MARKER_LEN..]))
}

/// The first field of `s` (up to the first run of two or more spaces), trimmed.
fn first_field(s: &str) -> String {
    s.split("  ").next().unwrap_or("").trim().to_owned()
}

/// Escapes a value for a Markdown table cell.
fn escape(s: &str) -> String {
    s.replace('|', "\\|")
}

/// Escapes and quotes a string for JSON.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured divan run: one counter-carrying group (with throughput
    /// continuation rows), an argument-less leaf, and a real-world LSP row whose
    /// case is a source location.
    const SAMPLE: &str = "\
     Running benches/inference.rs (target/release/deps/inference-1bf)
Timer precision: 41 ns
inference      fastest       │ slowest       │ median        │ mean          │ samples │ iters
╰─ cold_check                │               │               │               │         │
   ├─ 10       2.164 ms      │ 2.200 ms      │ 2.164 ms      │ 2.164 ms      │ 1       │ 1
   │           31.42 Kitem/s │ 31.42 Kitem/s │ 31.42 Kitem/s │ 31.42 Kitem/s │         │
   ╰─ 200      19.9 ms       │ 19.9 ms       │ 19.9 ms       │ 19.9 ms       │ 1       │ 1
     Running benches/lsp.rs (target/release/deps/lsp-abc)
lsp            fastest       │ slowest       │ median        │ mean          │ samples │ iters
├─ analysis_hover_real                       │               │               │               │         │
│  ╰─ samples/Orders.fai#L6  25.29 µs      │ 26.0 µs       │ 25.29 µs      │ 25.29 µs      │ 1       │ 1
╰─ object_for_def_only       7.0 µs        │ 7.1 µs        │ 7.05 µs       │ 7.05 µs       │ 100     │ 100
";

    #[test]
    fn parses_groups_benches_and_cases() {
        let report = Report::parse(SAMPLE);
        assert_eq!(report.rows.len(), 4, "{:#?}", report.rows);

        // Counter throughput continuation rows are skipped; leaf values land in
        // the right columns.
        let cold10 = &report.rows[0];
        assert_eq!(cold10.group, "inference");
        assert_eq!(cold10.bench, "cold_check");
        assert_eq!(cold10.case, "10");
        assert_eq!(cold10.median, "2.164 ms");
        assert_eq!(cold10.mean, "2.164 ms");
        assert_eq!(cold10.samples, "1");

        assert_eq!(report.rows[1].case, "200");
    }

    #[test]
    fn captures_real_world_case_and_argless_bench() {
        let report = Report::parse(SAMPLE);
        let hover = report.rows.iter().find(|r| r.bench == "analysis_hover_real").unwrap();
        assert_eq!(hover.group, "lsp");
        assert_eq!(hover.case, "samples/Orders.fai#L6");
        assert_eq!(hover.median, "25.29 µs");

        let argless = report.rows.iter().find(|r| r.bench == "object_for_def_only").unwrap();
        assert_eq!(argless.case, "", "an argument-less bench has no case");
        assert_eq!(argless.samples, "100");
    }

    #[test]
    fn links_source_locations_when_base_is_known() {
        let report = Report::parse(SAMPLE);
        let links = LinkBase { base: Some("https://github.com/o/r/blob/abc123".to_owned()) };
        let md = links.cell("samples/Orders.fai#L6");
        assert_eq!(
            md,
            "[samples/Orders.fai#L6](https://github.com/o/r/blob/abc123/samples/Orders.fai#L6)"
        );
        // A non-source case stays plain.
        assert_eq!(links.cell("200"), "200");
        assert_eq!(links.cell(""), "");

        let body = report.to_markdown(&links);
        assert!(body.contains("<details><summary><b>lsp</b></summary>"));
        assert!(body.contains("](https://github.com/o/r/blob/abc123/samples/Orders.fai#L6)"));
    }

    #[test]
    fn no_links_without_base() {
        let links = LinkBase { base: None };
        assert_eq!(links.cell("samples/Orders.fai#L6"), "samples/Orders.fai#L6");
    }

    #[test]
    fn json_is_well_formed_and_escaped() {
        let report = Report::parse(SAMPLE);
        let json = report.to_json();
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
        assert!(json.contains("\"benchmark\":\"analysis_hover_real\""));
        assert!(json.contains("\"case\":\"samples/Orders.fai#L6\""));
    }

    #[test]
    fn empty_input_degrades_gracefully() {
        let report = Report::parse("");
        assert!(report.rows.is_empty());
        assert!(report.to_markdown(&LinkBase::default()).contains("No benchmark results"));
        assert_eq!(report.to_json(), "[]");
    }

    #[test]
    fn garbage_input_does_not_panic() {
        let report = Report::parse("not divan output\n│ ├─ ╰─ random │ unicode\n\u{0}\u{1}");
        // Best-effort: no rows, no panic.
        let _ = report.to_markdown(&LinkBase::default());
        let _ = report.to_json();
    }
}

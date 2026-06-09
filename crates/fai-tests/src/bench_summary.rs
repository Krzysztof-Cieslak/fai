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

    /// Renders the report as Markdown: one collapsible table per bench group,
    /// each followed by a Fai-vs-Rust ratio table when the group pairs `rust`/`fai`
    /// rows (the runtime-comparison benches).
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

            let ratios = self.ratios_for_group(group);
            if !ratios.is_empty() {
                out.push_str("\n**Fai vs Rust** (median; lower ratio is better)\n\n");
                out.push_str("| Benchmark | Variant | Rust | Fai | Fai/Rust |\n");
                out.push_str("| --- | --- | --: | --: | --: |\n");
                for r in ratios {
                    let ratio = match r.ratio {
                        Some(x) => format!("{x:.2}×"),
                        None => "—".to_owned(),
                    };
                    let _ = writeln!(
                        out,
                        "| {} | {} | {} | {} | {} |",
                        escape(&r.bench),
                        escape(&r.variant),
                        escape(&r.rust),
                        escape(&r.fai),
                        escape(&ratio),
                    );
                }
            }

            out.push_str("\n</details>\n\n");
        }
        out
    }

    /// Pairs the `rust`/`fai` rows of `group` into Fai-vs-Rust ratio rows, keyed
    /// by benchmark and variant (the case with the leading `rust`/`fai` segment
    /// removed), in first-seen order. A benchmark whose group has no such pairs
    /// yields nothing, so non-comparison groups are unaffected.
    fn ratios_for_group(&self, group: &str) -> Vec<RatioRow> {
        let mut order: Vec<(String, String)> = Vec::new();
        let mut table: std::collections::HashMap<(String, String), RatioRow> =
            std::collections::HashMap::new();
        for row in self.rows.iter().filter(|r| r.group == group) {
            let Some((side, variant)) = split_side(&row.case) else { continue };
            let key = (row.bench.clone(), variant.to_owned());
            let entry = table.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                RatioRow {
                    bench: row.bench.clone(),
                    variant: variant.to_owned(),
                    rust: String::new(),
                    fai: String::new(),
                    ratio: None,
                }
            });
            if side == "rust" {
                entry.rust = row.median.clone();
            } else {
                entry.fai = row.median.clone();
            }
        }
        order
            .into_iter()
            .filter_map(|key| table.remove(&key))
            .filter(|r| !r.rust.is_empty() && !r.fai.is_empty())
            .map(|mut r| {
                r.ratio = match (parse_duration(&r.rust), parse_duration(&r.fai)) {
                    (Some(rust), Some(fai)) if rust > 0.0 => Some(fai / rust),
                    _ => None,
                };
                r
            })
            .collect()
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

/// One Fai-vs-Rust comparison row (a paired `rust`/`fai` measurement).
#[derive(Debug, Clone, PartialEq)]
struct RatioRow {
    /// The benchmark (algorithm) name.
    bench: String,
    /// The remaining case path after the `rust`/`fai` side (e.g. a size), or empty.
    variant: String,
    /// The Rust median, as divan rendered it.
    rust: String,
    /// The Fai median, as divan rendered it.
    fai: String,
    /// `fai / rust`, or `None` if either median could not be parsed.
    ratio: Option<f64>,
}

/// Splits a case into its leading `rust`/`fai` side and the remaining variant
/// (the rest of the ` / `-separated path), or `None` if it names neither side.
fn split_side(case: &str) -> Option<(&str, &str)> {
    let mut parts = case.splitn(2, " / ");
    let side = parts.next()?;
    if side == "rust" || side == "fai" { Some((side, parts.next().unwrap_or(""))) } else { None }
}

/// Parses a divan duration (`14.51 ms`, `996.8 µs`, `120 ns`, `1.2 s`, …) into
/// nanoseconds. Best-effort: an unrecognized shape yields `None`.
fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim();
    let pos = s.find(char::is_alphabetic)?;
    let value: f64 = s[..pos].trim().parse().ok()?;
    let scale = match s[pos..].trim() {
        "ps" => 1e-3,
        "ns" => 1.0,
        "µs" | "μs" | "us" => 1e3,
        "ms" => 1e6,
        "s" => 1e9,
        _ => return None,
    };
    Some(value * scale)
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

    /// A runtime-comparison group as the `algorithms_jit`/`algorithms_aot` benches
    /// render it: each algorithm a parent node with `fai` and `rust` leaves.
    const COMPARISON: &str = "\
     Running benches/algorithms_jit.rs (target/release/deps/algorithms_jit-1)
algorithms_jit  fastest       │ slowest       │ median        │ mean          │ samples │ iters
├─ fib                        │               │               │               │         │
│  ├─ fai       14.15 ms      │ 15.03 ms      │ 14.51 ms      │ 14.53 ms      │ 31      │ 31
│  ╰─ rust      965.9 µs      │ 1.065 ms      │ 996.8 µs      │ 997.6 µs      │ 100     │ 100
╰─ pi                         │               │               │               │         │
   ├─ fai       8.0 ms        │ 8.2 ms        │ 8.1 ms        │ 8.1 ms        │ 50      │ 50
   ╰─ rust      2.0 ms        │ 2.1 ms        │ 2.0 ms        │ 2.0 ms        │ 100     │ 100
";

    #[test]
    fn parses_seconds_to_nanoseconds() {
        assert_eq!(parse_duration("120 ns"), Some(120.0));
        assert_eq!(parse_duration("996.8 µs"), Some(996_800.0));
        assert_eq!(parse_duration("14.51 ms"), Some(14_510_000.0));
        assert_eq!(parse_duration("1.2 s"), Some(1_200_000_000.0));
        assert_eq!(parse_duration("2 ps"), Some(0.002));
        // The Greek-mu and ASCII spellings of microseconds are both accepted.
        assert_eq!(parse_duration("5 μs"), Some(5_000.0));
        assert_eq!(parse_duration("5 us"), Some(5_000.0));
    }

    #[test]
    fn unparseable_durations_yield_none() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("fast"), None);
        assert_eq!(parse_duration("12 weeks"), None);
    }

    #[test]
    fn pairs_rust_and_fai_into_ratios() {
        let report = Report::parse(COMPARISON);
        let ratios = report.ratios_for_group("algorithms_jit");
        assert_eq!(ratios.len(), 2, "{ratios:#?}");
        // First-seen order is preserved (fib before pi).
        assert_eq!(ratios[0].bench, "fib");
        assert_eq!(ratios[0].variant, "");
        assert_eq!(ratios[0].rust, "996.8 µs");
        assert_eq!(ratios[0].fai, "14.51 ms");
        let ratio = ratios[0].ratio.expect("a ratio");
        assert!((ratio - 14_510_000.0 / 996_800.0).abs() < 1e-6, "{ratio}");
        assert_eq!(ratios[1].bench, "pi");
    }

    #[test]
    fn ratio_table_renders_after_the_group() {
        let report = Report::parse(COMPARISON);
        let md = report.to_markdown(&LinkBase::default());
        assert!(md.contains("**Fai vs Rust**"), "{md}");
        assert!(md.contains("| Benchmark | Variant | Rust | Fai | Fai/Rust |"), "{md}");
        // fib: 14.51 ms / 996.8 µs ≈ 14.56×.
        assert!(md.contains("14.56×"), "{md}");
    }

    #[test]
    fn non_comparison_group_has_no_ratio_table() {
        let report = Report::parse(SAMPLE);
        // The `inference`/`lsp` sample has no rust/fai pairs.
        assert!(report.ratios_for_group("inference").is_empty());
        assert!(!report.to_markdown(&LinkBase::default()).contains("**Fai vs Rust**"));
    }
}

//! Reader-facing reports derived from one validated measurement snapshot.

mod allocation;
mod display;
mod footprint;
mod time;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::{Error, Snapshot};
use crate::bench_summary::{AllocationSample, escape, memory, parse_duration, warm};
use crate::benchmark_contracts::Comparison;

#[derive(Debug)]
struct RatioStatistics {
    count: usize,
    geometric: f64,
    arithmetic: f64,
    p90: f64,
}

fn ratio_statistics(mut values: Vec<f64>) -> Option<RatioStatistics> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite() || *value <= 0.0) {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let count = values.len();
    Some(RatioStatistics {
        count,
        geometric: (values.iter().map(|value| value.ln()).sum::<f64>() / count as f64).exp(),
        arithmetic: values.iter().sum::<f64>() / count as f64,
        p90: values[(count * 9).div_ceil(10) - 1],
    })
}

fn summary_header(text: &mut String) {
    text.push_str("### Summary — all measured inputs\n\n| Compared with | Cases used | Geometric mean | Arithmetic mean | 90th percentile |\n|---|---:|---:|---:|---:|\n");
}

fn summary_row(text: &mut String, side: &str, values: Vec<f64>, total: usize) {
    let side = display::language(side);
    let total = display::number(total);
    if let Some(stats) = ratio_statistics(values) {
        let _ = writeln!(
            text,
            "| {side} | {} / {total} | {:.3}× | {:.3}× | {:.3}× |",
            display::number(stats.count),
            stats.geometric,
            stats.arithmetic,
            stats.p90
        );
    } else {
        let _ = writeln!(text, "| {side} | 0 / {total} | — | — | — |");
    }
}

fn summary_explanation(text: &mut String) {
    text.push_str("\nEach included case has equal weight. The geometric mean summarizes multiplicative ratios; the arithmetic mean averages them directly. The 90th percentile is the ratio at or below which 90% of the included cases fall.\n\n");
}

fn details(text: &mut String, title: &str) {
    let _ = writeln!(text, "<details>\n<summary>{title}</summary>\n");
}

fn end_details(text: &mut String) {
    text.push_str("\n</details>\n\n");
}

fn header(snapshot: &Snapshot, key: &str, title: &str, evidence: &str) -> String {
    let m = &snapshot.manifest;
    let date = m.completed_at.split('T').next().unwrap_or(&m.completed_at);
    let mut text = format!(
        "<!-- fai-performance-tracker:{key} -->\n# {title}\n\n**Measured:** {date} · **Source:** [`{}`](https://github.com/{}/tree/{})",
        &m.measured_revision[..12],
        m.repository,
        m.measured_revision
    );
    if !evidence.is_empty() {
        let _ = write!(text, " · [Data and reproducibility]({evidence})");
    }
    text.push_str("\n\n");
    text
}

fn navigation(text: &mut String) {
    text.push_str("[Shared algorithms](#shared-algorithms) · [Standard-library workloads](#standard-library-workloads) · [Measurement details](#measurement-details)\n\n");
}

fn ratio_guide(text: &mut String, measure: &str) {
    let _ = writeln!(
        text,
        "**Lower ratios favor Fai.** Each ratio is Fai ÷ the comparison language: **0.80× means 20% less {measure}**; **1.20× means 20% more**.\n"
    );
}

fn comparison_class(name: &str) -> Comparison {
    crate::benchmark_contracts::for_module(name).expect("validated workload").comparison
}

fn comparison_heading(class: Comparison) -> &'static str {
    match class {
        Comparison::Kernel => "Shared algorithms",
        Comparison::Application => "Standard-library workloads",
    }
}

fn comparison_description(class: Comparison) -> &'static str {
    match class {
        Comparison::Kernel => {
            "The same algorithms and logical data structures in all three languages. Compiler optimizations and runtime representations may differ."
        }
        Comparison::Application => {
            "The same tasks using each language's collection and library implementations. Results include differences in library design, such as hashing, storage and growth policies."
        }
    }
}

fn finish(snapshot: &Snapshot, key: &str, evidence: &str, method: &str, text: &mut String) {
    let m = &snapshot.manifest;
    text.push_str("## Measurement details\n\n");
    details(text, "Methods, environment and reproducibility");
    let _ = writeln!(text, "{method}\n");
    let get = |key: &str| m.machine.get(key).map(String::as_str).unwrap_or("unspecified");
    let tool = |key: &str| m.toolchains.get(key).map(String::as_str).unwrap_or("unspecified");
    let _ = writeln!(
        text,
        "- **Snapshot:** `{}`\n- **Completed:** {}\n- **Host:** {} / {}\n- **CPU:** {}\n- **Rust compiler:** {}",
        m.run_id,
        m.completed_at,
        get("system"),
        get("machine"),
        get("cpu"),
        tool("rustVersion")
    );
    if key != "perceus" {
        let _ = writeln!(text, "- **OCaml:** {} with Flambda and `-O3`", tool("ocamlVersion"));
    }
    let _ = writeln!(
        text,
        "- **Measurement protocol:** `{}`",
        if key == "memory" { crate::memory_workloads::METHODOLOGY } else { &m.methodology }
    );
    if !evidence.is_empty() {
        let _ = writeln!(
            text,
            "\n[Raw observations, executable identities and reproduction record]({evidence})"
        );
    }
    if key != "perceus" {
        text.push_str("\nBenchmark names link to the measured source. Detailed tables use those source identifiers.\n");
    }
    text.push_str("\nThe evidence includes all inputs, compiler settings, workload versions and implementation notes.\n");
    end_details(text);
    if let Some(notes) = m.notes.get(key).filter(|notes| !notes.is_empty()) {
        details(text, "Follow-up investigations");
        for note in notes {
            let _ = writeln!(text, "- {note}");
        }
        end_details(text);
    }
    text.push_str("---\nGenerated from validated measurements. Earlier reports are linked from the issue comments.\n");
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) { (values[mid - 1] + values[mid]) / 2.0 } else { values[mid] }
}

pub(super) fn reports(
    snapshot: &Snapshot,
    evidence: &str,
) -> Result<BTreeMap<String, String>, Error> {
    if !evidence.is_empty()
        && (!evidence.starts_with("https://")
            || evidence.chars().any(|c| c.is_whitespace() || "()<>".contains(c)))
    {
        return Err(Error("evidence URL must be an HTTPS link without Markdown delimiters".into()));
    }
    let reports = BTreeMap::from([
        ("perceus-memory.md".into(), allocation::report(snapshot, evidence)),
        ("language-memory.md".into(), footprint::report(snapshot, evidence)),
        ("language-runtime.md".into(), time::report(snapshot, evidence)),
    ]);
    if reports.values().any(|text| text.len() > 60_000) {
        return Err(Error(
            "generated body exceeds the publication budget; shorten snapshot notes".into(),
        ));
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_means_describe_the_case_distribution() {
        let stats = ratio_statistics(vec![0.5, 1.0, 2.0, 4.0]).unwrap();
        assert_eq!(stats.count, 4);
        assert!((stats.geometric - 2.0_f64.sqrt()).abs() < 1e-12);
        assert_eq!(stats.arithmetic, 1.875);
        assert_eq!(stats.p90, 4.0);
    }

    #[test]
    fn p90_uses_nearest_rank_without_interpolation() {
        let stats = ratio_statistics((1..=20).map(f64::from).rev().collect()).unwrap();
        assert_eq!(stats.p90, 18.0);
    }

    #[test]
    fn empty_ratios_have_no_average_or_percentile() {
        assert!(ratio_statistics(Vec::new()).is_none());
    }
}

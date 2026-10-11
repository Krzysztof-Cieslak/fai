//! Three current-state reports from one validated measurement snapshot.

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

fn header(snapshot: &Snapshot, key: &str, title: &str, evidence: &str) -> String {
    let m = &snapshot.manifest;
    let mut text = format!(
        "<!-- fai-performance-tracker:{key} -->\n# {title}\n\n**Snapshot:** `{}` · **Completed:** {}\n\n**Measured main revision:** [`{}`](https://github.com/{}/tree/{}) · **Method:** `{}`\n\n",
        m.run_id,
        m.completed_at,
        &m.measured_revision[..12],
        m.repository,
        m.measured_revision,
        if key == "memory" { crate::memory_workloads::METHODOLOGY } else { &m.methodology }
    );
    let get = |key: &str| m.machine.get(key).map(String::as_str).unwrap_or("unspecified");
    let tool = |key: &str| m.toolchains.get(key).map(String::as_str).unwrap_or("unspecified");
    let _ = writeln!(
        text,
        "**Host:** {} / {} · **CPU:** {}\n",
        get("system"),
        get("machine"),
        get("cpu")
    );
    if key == "perceus" {
        let _ = writeln!(
            text,
            "**Compiler:** {} · optimized `allocation-stats` build\n",
            tool("rustVersion")
        );
    } else {
        let _ = writeln!(
            text,
            "**Peers:** {} · OCaml {} (Flambda, `-O3`)\n",
            tool("rustVersion"),
            tool("ocamlVersion")
        );
    }
    if !evidence.is_empty() {
        let _ = writeln!(text, "[Raw data, identities and reproducibility record]({evidence})\n");
    }
    if key != "perceus" {
        text.push_str("[Matched kernels](#matched-kernels) · [Application comparisons](#application-comparisons)\n\n");
    }
    text
}

fn notes(snapshot: &Snapshot, key: &str, text: &mut String) {
    if let Some(notes) = snapshot.manifest.notes.get(key).filter(|notes| !notes.is_empty()) {
        text.push_str("\n## Next investigations\n\n");
        for note in notes {
            let _ = writeln!(text, "- {note}");
        }
    }
    text.push_str("\n---\nGenerated report. Refresh history is linked from the issue comments.\n");
}

fn counter(row: &AllocationSample, field: &str) -> u64 {
    row.counters[field].as_u64().expect("validated allocation counter")
}

fn additional_live(row: &AllocationSample) -> u64 {
    counter(row, "peakLiveBytes") - counter(row, "baselineLiveBytes")
}

fn retained_backing(row: &AllocationSample) -> u64 {
    counter(row, "retainedSlabBytes")
        + counter(row, "retainedMappedBytes")
        + counter(row, "retainedSystemBytes")
}

fn perceus(snapshot: &Snapshot, evidence: &str) -> String {
    let mut text =
        header(snapshot, "perceus", "Perceus memory guarantees and allocation evidence", evidence);
    text.push_str("## Current status\n\n**All required memory guards passed.** Reference counting remains active in every control. Each allocation pair uses identical Fai source, compiler and input; only the named optimization changes.\n\n| Guarantee suite | Passed | Scope |\n|---|---:|---|\n");
    for (name, label, scope) in [
        (
            "perceus_space",
            "Live-space / ownership",
            "Reference balance, reuse, sharing, fixed-state and resource lifetimes",
        ),
        (
            "perceus_storage",
            "Retained storage",
            "Slabs, mappings, repeated work and prompt large-buffer release",
        ),
    ] {
        let step =
            snapshot.manifest.steps.iter().find(|s| s.name == name).expect("validated guard");
        let _ = writeln!(text, "| {label} | {} | {scope} |", step.passed.unwrap_or(0));
    }
    text.push_str("\n## Source-identical allocation controls\n\nArrows are **optimization disabled → enabled**. Values are counts or bytes, not percentages. Additional peak live bytes are measured above the same retained harness owner within each run. Every window finishes with its baseline live objects and bytes. Backing includes owned slabs, mappings and system allocations; retained backing is measured at window end.\n\n| Workload | Input | Disabled control | Allocation requests | Allocated bytes | Additional peak live bytes | Peak backing bytes | Retained backing bytes |\n|---|---:|---|---:|---:|---:|---:|---:|\n");
    let mut pairs = BTreeMap::<_, Vec<_>>::new();
    for row in &snapshot.reports["allocations"].allocations {
        pairs.entry((&row.algorithm, row.size)).or_default().push(row);
    }
    for ((name, size), rows) in pairs {
        let enabled = rows.iter().find(|r| r.control == "enabled").expect("validated pair");
        let disabled = rows.iter().find(|r| r.control != "enabled").expect("validated pair");
        let _ = writeln!(
            text,
            "| {} | {size} | {} | {} → {} | {} → {} | {} → {} | {} → {} | {} → {} |",
            escape(name),
            escape(&disabled.control),
            counter(disabled, "allocations"),
            counter(enabled, "allocations"),
            counter(disabled, "allocatedBytes"),
            counter(enabled, "allocatedBytes"),
            additional_live(disabled),
            additional_live(enabled),
            counter(disabled, "peakBackingBytes"),
            counter(enabled, "peakBackingBytes"),
            retained_backing(disabled),
            retained_backing(enabled)
        );
    }
    text.push_str("\n**Accounting domain:** `fai-object-storage-v1`. Object requests include headers and pooled allocations; reset/reuse avoids a request. Live bytes exclude dead recycling cells, while backing bytes include owned slabs and mappings. Host-language temporaries, allocator arenas and OS rounding are outside these counters. Timing uses separate uninstrumented builds.\n");
    notes(snapshot, "perceus", &mut text);
    text
}

fn footprint(snapshot: &Snapshot, evidence: &str) -> String {
    let mut text =
        header(snapshot, "memory", "Native process memory: Fai / Rust / OCaml", evidence);
    let samples = &snapshot.reports["memory"].footprints;
    let _ = writeln!(
        text,
        "## Current coverage\n\n**{} workloads · {} size/repetition cases · {} fresh processes per side and mode.** Predeclared memory-specific input curves and 1/16 whole-workload repetitions are included. Additional storage fixtures vary elements, keys, nodes, UTF-8 chunks and body counts. Every peer is an isolated, uninstrumented executable. Input units and fixed-state cases are listed below.\n",
        crate::memory_workloads::algorithms().count(),
        samples.len() / 3,
        snapshot.manifest.settings.memory_samples
    );
    text.push_str("The tables report **process footprint**, including code, stacks, allocator retention and GC policy. Each displayed peak and floor is the maximum across fresh processes, in KiB (1,024 bytes). They do not measure comparable cross-language live heaps. Floors use the same executable; peaks are never subtracted or converted into compiler-memory win counts.\n\n");
    text.push_str("## Comparison and floor legend\n\nThe floor is the same executable running its harness without the workload. `Process F/R` is Fai's peak RSS divided by Rust's; `Process F/O` divides it by OCaml's. Lower ratios mean a smaller observed process footprint.\n\n| Label | Meaning |\n|---|---|\n| `matched kernel` | Peers share the reviewed algorithm and logical data structures. Runtime representation and allocator costs remain observable. |\n| `application` | Algorithms, containers or implementation policies differ. Ratios still compare total process footprints, including those choices. |\n| `Floor share F/R/O` | Each language's floor divided by its workload peak, shown as a percentage. A high share describes a baseline-dominated process; it does not suppress ratios. Independent process observations can give a share above 100%. |\n| `F/R: …` or `F/O: …` | Only the named ratio is withheld because its pair is missing, incompatible, instrumented, or has fewer than five observations. |\n\n`—` denotes an unavailable value or withheld ratio, with the reason shown alongside the comparison. Floors are never subtracted. Whole-process RSS includes code, stacks, allocator retention and GC policy; live-heap efficiency and Perceus guarantees require their own evidence.\n\n");
    footprint_section(snapshot, Comparison::Kernel, &mut text);
    footprint_section(snapshot, Comparison::Application, &mut text);
    text.push_str("\nThe evidence's `catalog.json` records each workload version, logical contract and separate Fai/Rust and Fai/OCaml implementation notes. Matching logical work still includes native representation, string storage, allocation and reclamation costs.\n");
    notes(snapshot, "memory", &mut text);
    text
}

fn comparison_class(name: &str) -> Comparison {
    crate::benchmark_contracts::for_module(name).expect("validated workload").comparison
}

fn comparison_heading(class: Comparison) -> &'static str {
    match class {
        Comparison::Kernel => "Matched kernels",
        Comparison::Application => "Application comparisons",
    }
}

fn comparison_description(class: Comparison) -> &'static str {
    match class {
        Comparison::Kernel => {
            "Peers share the reviewed algorithm, logical data and evaluation phases. Native representation, allocation and reclamation costs remain observable."
        }
        Comparison::Application => {
            "These reviewed applications include their chosen container algorithms, library policies and representations. Native hash functions, probing/chaining, growth and traversal choices contribute to the results; per-pair implementation notes are retained in the evidence."
        }
    }
}

fn footprint_section(snapshot: &Snapshot, class: Comparison, text: &mut String) {
    let samples: Vec<_> = snapshot.reports["memory"]
        .footprints
        .iter()
        .filter(|sample| comparison_class(&sample.algorithm) == class)
        .cloned()
        .collect();
    let comparisons = memory::comparisons(&samples);
    debug_assert!(comparisons.iter().all(|case| case.class == Some(class)));
    let workloads = samples.iter().map(|sample| &sample.algorithm).collect::<BTreeSet<_>>().len();
    let _ = writeln!(
        text,
        "## {}\n\n{}\n\n**{workloads} workloads · {} size/repetition cases.** Statistics and tables below cover this category only.\n",
        comparison_heading(class),
        comparison_description(class),
        comparisons.len()
    );
    text.push_str("### Process-RSS ratio summary\n\nRatios are **Fai ÷ peer; lower means a smaller process footprint**. Arithmetic and geometric means use unrounded case ratios with equal weight per workload/input/repetition case. P90 is nearest-rank across case ratios, not a percentile of RSS samples. Only compatible, uninstrumented pairs with sufficient observations contribute; large floor shares remain included.\n\n| Peer | Paired / cases | Geometric mean | Arithmetic mean | P90 |\n|---|---:|---:|---:|---:|\n");
    for peer in ["rust", "ocaml"] {
        let values = comparisons
            .iter()
            .filter_map(|case| if peer == "rust" { case.rust } else { case.ocaml })
            .collect();
        if let Some(stats) = ratio_statistics(values) {
            let _ = writeln!(
                text,
                "| {peer} | {} / {} | {:.3}× | {:.3}× | {:.3}× |",
                stats.count,
                comparisons.len(),
                stats.geometric,
                stats.arithmetic,
                stats.p90
            );
        } else {
            let _ = writeln!(text, "| {peer} | 0 / {} | — | — | — |", comparisons.len());
        }
    }
    text.push('\n');
    text.push_str(&memory::render_footprints(&samples).replacen(
        "### Isolated process footprints",
        "### Peak process footprints",
        1,
    ));
    text.push_str(&memory::render_reclamation(&samples).replacen(
        "## Post-workload resident memory",
        "### Post-workload resident memory",
        1,
    ));
    text.push_str("### Input axes\n\nSizes are fixed before measurement. Fixed-state curves increase work rather than resident data; storage fixtures vary the live dataset itself.\n\n| Workload | Input unit | Scale | Sizes |\n|---|---|---|---|\n");
    let mut cases: Vec<_> = crate::memory_workloads::algorithms()
        .filter(|case| comparison_class(case.module) == class)
        .collect();
    cases.sort_by_key(|case| case.module);
    for case in cases {
        let profile = crate::memory_workloads::profile(case.module).expect("memory profile");
        let sizes = profile.sizes.iter().map(i64::to_string).collect::<Vec<_>>().join(", ");
        let _ =
            writeln!(text, "| {} | {} | {} | {sizes} |", case.module, profile.unit, profile.scale);
    }
    text.push('\n');
    if class == Comparison::Kernel {
        text.push_str("Tree depth d describes 2^d − 1 nonempty nodes in MemoryTree. Each MemoryUtf8 chunk is nine bytes / four Unicode scalars. Array/body inputs count elements.\n\n");
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) { (values[mid - 1] + values[mid]) / 2.0 } else { values[mid] }
}

fn runtime(snapshot: &Snapshot, evidence: &str) -> String {
    let mut text = header(snapshot, "runtime", "Native runtime: Fai / Rust / OCaml", evidence);
    let cases = warm::summarize(&snapshot.reports["warm"].warm);
    let (matched, applications): (Vec<_>, Vec<_>) = cases.iter().partition(|case| case.matched);
    let _ = writeln!(
        text,
        "Warm measurements use {} passes of {} paired observations after calibration and warmup. Matched kernels and application comparisons have their own tables and coverage.\n\n## Matched kernels\n\n{}\n\n**{} workloads · {} size cases.**\n",
        snapshot.manifest.settings.warm_passes,
        snapshot.manifest.settings.warm_samples,
        comparison_description(Comparison::Kernel),
        matched.iter().map(|case| &case.algorithm).collect::<BTreeSet<_>>().len(),
        matched.len()
    );
    text.push_str("### Warm-AOT summary\n\nRatios are **Fai ÷ peer; lower is faster**. Aggregates include only this category's cases that clear the floor gate. Arithmetic and geometric means weight unrounded case ratios equally; p90 is nearest-rank across case ratios, not a latency percentile. These are point estimates for the reviewed workload set.\n\n| Peer | Eligible / matched cases | Geometric mean | Arithmetic mean | P90 |\n|---|---:|---:|---:|---:|\n");
    for peer in ["rust", "ocaml"] {
        let values = matched.iter().filter_map(|case| case.ratio(peer)).collect();
        if let Some(stats) = ratio_statistics(values) {
            let _ = writeln!(
                text,
                "| {peer} | {} / {} | {:.3}× | {:.3}× | {:.3}× |",
                stats.count,
                matched.len(),
                stats.geometric,
                stats.arithmetic,
                stats.p90
            );
        } else {
            let _ = writeln!(text, "| {peer} | 0 / {} | — | — | — |", matched.len());
        }
    }
    text.push_str("\n### Eligibility legend\n\n`F/R` means Fai divided by Rust, and `F/O` means Fai divided by OCaml. Lower ratios mean faster Fai execution. The timing floor is measured using the same harness without the workload.\n\n| Eligibility | Meaning |\n|---|---|\n| `matched kernel` | Peers share the reviewed algorithm and logical data structures, and all clear the timing-floor check. Their ratios qualify for matched-kernel comparisons. |\n| `floor: <languages>` | Each named peer's measured harness floor reaches 10% of its elapsed time. Ratios involving those peers are withheld; another peer comparison may still qualify. |\n\nFor example, `floor: rust` suppresses F/R while F/O may remain; `floor: ocaml` suppresses F/O while F/R may remain; `floor: fai` suppresses both. `—` means an excluded ratio, rather than a missing measurement. Only eligible ratios from these main kernels enter the summary above; supplemental components have their own section.\n\n");
    let mut gaps = Vec::new();
    for case in &matched {
        for peer in ["rust", "ocaml"] {
            if let Some(ratio) = case.ratio(peer).filter(|ratio| *ratio > 1.0) {
                gaps.push((ratio, case.algorithm.as_str(), case.size, peer));
            }
        }
    }
    gaps.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| (a.1, a.2, a.3).cmp(&(b.1, b.2, b.3))));
    if !gaps.is_empty() {
        text.push_str("### Largest eligible matched-kernel ratios\n\n| Workload | Input | Peer | Fai / peer |\n|---|---:|---|---:|\n");
        for (ratio, name, size, peer) in gaps.into_iter().take(6) {
            let _ = writeln!(text, "| {name} | {size} | {peer} | {ratio:.3}× |");
        }
        text.push('\n');
    }
    warm_table(&matched, "### Warm AOT", &mut text);
    end_to_end_table(snapshot, Comparison::Kernel, &mut text);

    let _ = writeln!(
        text,
        "## Application comparisons\n\n{}\n\n**{} workloads · {} size cases.** Absolute timings describe these applications, including their library choices. Floor shares remain visible; these rows do not enter the kernel ratios or rankings.\n",
        comparison_description(Comparison::Application),
        applications.iter().map(|case| &case.algorithm).collect::<BTreeSet<_>>().len(),
        applications.len()
    );
    application_warm_table(&applications, &mut text);
    end_to_end_table(snapshot, Comparison::Application, &mut text);
    warm_table(
        &warm::summarize(&snapshot.reports["components"].warm).iter().collect::<Vec<_>>(),
        "## Matched supplemental components",
        &mut text,
    );
    text.push_str("\nThe evidence's `catalog.json` records each workload version, logical contract and per-pair implementation notes. `validation.json` retains untimed boundary-input answers tied to the measured executables. Corrected workloads require fresh measurements; changes from older workload versions are not compiler speedups.\n");
    notes(snapshot, "runtime", &mut text);
    text
}

fn end_to_end_table(snapshot: &Snapshot, class: Comparison, text: &mut String) {
    text.push_str("### End-to-end AOT\n\nThese absolute medians include process startup, workload, output and shutdown. Each cell is the median of three pass medians, with at least 100 observations per pass. Exact per-pass executable identities are retained. This scope has no harness-floor qualification and contributes no warm aggregates.\n\n| Workload | Input | Fai ms | Rust ms | OCaml ms |\n|---|---:|---:|---:|---:|\n");
    let mut samples = BTreeMap::<_, Vec<_>>::new();
    for name in ["aot-1", "aot-2", "aot-3"] {
        for row in &snapshot.reports[name].rows {
            samples
                .entry((super::validate::normalized(&row.bench), row.case.as_str()))
                .or_default()
                .push(parse_duration(&row.median).expect("validated duration"));
        }
    }
    let mut algorithms: Vec<_> = crate::algorithms::ALGORITHMS
        .iter()
        .filter(|algorithm| comparison_class(algorithm.module) == class)
        .collect();
    algorithms.sort_by_key(|a| a.module);
    for algorithm in algorithms {
        let key = super::validate::normalized(algorithm.module);
        let time = |side| median(samples[&(key.clone(), side)].clone()) / 1_000_000.0;
        let _ = writeln!(
            text,
            "| {} | {} | {:.3} | {:.3} | {:.3} |",
            algorithm.module,
            algorithm.aot_size,
            time("fai"),
            time("rust"),
            time("ocaml")
        );
    }
    text.push('\n');
}

fn warm_time(case: &warm::Case, side: &str) -> String {
    let value = case.sides.get(side).expect("validated comparison side").time / 1000.0;
    if value < 0.001 { "<0.001".into() } else { format!("{value:.3}") }
}

fn application_warm_table(cases: &[&warm::Case], text: &mut String) {
    text.push_str("### Warm AOT\n\nTimes are median-of-pass-medians, in microseconds per invocation. Floor shares are each language's harness time divided by its elapsed time.\n\n| Workload | Input | Fai µs | Rust µs | OCaml µs | Floor share F/R/O |\n|---|---:|---:|---:|---:|---|\n");
    for case in cases {
        let shares = ["fai", "rust", "ocaml"]
            .map(|side| {
                let value = &case.sides[side];
                format!("{:.1}%", 100.0 * value.floor / value.time)
            })
            .join("/");
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} | {} | {shares} |",
            escape(&case.algorithm),
            case.size,
            warm_time(case, "fai"),
            warm_time(case, "rust"),
            warm_time(case, "ocaml")
        );
    }
    text.push('\n');
}

fn warm_table(cases: &[&warm::Case], heading: &str, text: &mut String) {
    let _ = writeln!(
        text,
        "{heading}\n\nTimes are median-of-pass-medians, in microseconds per invocation. Floors are measured, never subtracted. A ratio is suppressed when either side's floor reaches 10% of its elapsed time, with the reason in the final column. Raw floors remain in the evidence.\n\n| Workload | Input | Fai µs | Rust µs | OCaml µs | F/R | F/O | Eligibility |\n|---|---:|---:|---:|---:|---:|---:|---|"
    );
    for case in cases {
        let ratio = |peer| case.ratio(peer).map_or_else(|| "—".into(), |r| format!("{r:.3}×"));
        let limited: Vec<_> = case
            .sides
            .iter()
            .filter(|(_, side)| side.limited())
            .map(|(name, _)| name.as_str())
            .collect();
        let status = if !case.matched {
            "application".into()
        } else if !limited.is_empty() {
            format!("floor: {}", limited.join(", "))
        } else {
            "matched kernel".into()
        };
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} | {} | {} | {} | {status} |",
            escape(&case.algorithm),
            case.size,
            warm_time(case, "fai"),
            warm_time(case, "rust"),
            warm_time(case, "ocaml"),
            ratio("rust"),
            ratio("ocaml")
        );
    }
    text.push('\n');
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
        ("perceus-memory.md".into(), perceus(snapshot, evidence)),
        ("language-memory.md".into(), footprint(snapshot, evidence)),
        ("language-runtime.md".into(), runtime(snapshot, evidence)),
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

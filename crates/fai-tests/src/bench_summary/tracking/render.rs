//! Three current-state reports from one validated measurement snapshot.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::{Error, Snapshot};
use crate::bench_summary::{AllocationSample, escape, memory, parse_duration, warm};

fn header(snapshot: &Snapshot, key: &str, title: &str, evidence: &str) -> String {
    let m = &snapshot.manifest;
    let mut text = format!(
        "<!-- fai-performance-tracker:{key} -->\n# {title}\n\n**Snapshot:** `{}` · **Completed:** {}\n\n**Measured main revision:** [`{}`](https://github.com/{}/tree/{}) · **Method:** `{}`\n\n",
        m.run_id,
        m.completed_at,
        &m.measured_revision[..12],
        m.repository,
        m.measured_revision,
        m.methodology
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
        "## Current coverage\n\n**{} workloads · {} size/repetition cases · {} fresh processes per side and mode.** Both registered input sizes and 1/16 whole-workload repetitions are included. Every peer is an isolated, uninstrumented executable.\n",
        crate::algorithms::ALGORITHMS.len(),
        samples.len() / 3,
        snapshot.manifest.settings.memory_samples
    );
    text.push_str("The tables report **process footprint**, including code, stacks, allocator retention and GC policy. They do not measure comparable cross-language live heaps. Floors use the same executable; peaks are never subtracted or converted into compiler-memory win counts.\n\n");
    text.push_str(&memory::render_footprints(samples).replacen(
        "### Isolated process footprints",
        "## Current measurements",
        1,
    ));
    notes(snapshot, "memory", &mut text);
    text
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) { (values[mid - 1] + values[mid]) / 2.0 } else { values[mid] }
}

fn runtime(snapshot: &Snapshot, evidence: &str) -> String {
    let mut text = header(snapshot, "runtime", "Native runtime: Fai / Rust / OCaml", evidence);
    let cases = warm::summarize(&snapshot.reports["warm"].warm);
    let matched = cases.iter().filter(|case| case.matched).count();
    text.push_str("## Warm-AOT summary\n\nRatios are **Fai ÷ peer; lower is faster**. Aggregates include only reviewed matched-kernel cases that clear the floor gate. They are point estimates for this workload set. Geometric means weight cases equally; p90 is nearest-rank across case ratios, not a latency percentile.\n\n| Peer | Eligible / matched cases | Geometric mean | Arithmetic mean | P90 |\n|---|---:|---:|---:|---:|\n");
    for peer in ["rust", "ocaml"] {
        let mut values: Vec<_> = cases.iter().filter_map(|case| case.ratio(peer)).collect();
        values.sort_by(f64::total_cmp);
        if values.is_empty() {
            let _ = writeln!(text, "| {peer} | 0 / {matched} | — | — | — |");
        } else {
            let geometric =
                (values.iter().map(|n| n.ln()).sum::<f64>() / values.len() as f64).exp();
            let arithmetic = values.iter().sum::<f64>() / values.len() as f64;
            let p90 = values[(values.len() * 9).div_ceil(10) - 1];
            let _ = writeln!(
                text,
                "| {peer} | {} / {matched} | {geometric:.3}× | {arithmetic:.3}× | {p90:.3}× |",
                values.len()
            );
        }
    }
    let _ = writeln!(
        text,
        "\n{} main workload/size cases are measured in total. Application cases retain absolute timings, with no compiler-kernel ratio. Warm measurements use {} passes of {} paired observations after calibration and warmup.\n",
        cases.len(),
        snapshot.manifest.settings.warm_passes,
        snapshot.manifest.settings.warm_samples
    );
    let mut gaps = Vec::new();
    for case in &cases {
        for peer in ["rust", "ocaml"] {
            if let Some(ratio) = case.ratio(peer).filter(|ratio| *ratio > 1.0) {
                gaps.push((ratio, case.algorithm.as_str(), case.size, peer));
            }
        }
    }
    gaps.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| (a.1, a.2, a.3).cmp(&(b.1, b.2, b.3))));
    if !gaps.is_empty() {
        text.push_str("## Largest eligible matched-kernel ratios\n\n| Workload | Input | Peer | Fai / peer |\n|---|---:|---|---:|\n");
        for (ratio, name, size, peer) in gaps.into_iter().take(6) {
            let _ = writeln!(text, "| {name} | {size} | {peer} | {ratio:.3}× |");
        }
        text.push('\n');
    }
    warm_table(&cases, "Main workloads", &mut text);
    warm_table(
        &warm::summarize(&snapshot.reports["components"].warm),
        "Matched supplemental components",
        &mut text,
    );
    text.push_str("## End-to-end AOT\n\nThese absolute medians include process startup, workload, output and shutdown. Each cell is the median of three pass medians, with at least 100 observations per pass. Exact per-pass executable identities are retained. This scope has no harness-floor qualification and contributes no kernel ratios or warm aggregates.\n\n| Workload | Input | Fai ms | Rust ms | OCaml ms |\n|---|---:|---:|---:|---:|\n");
    let mut samples = BTreeMap::<_, Vec<_>>::new();
    for name in ["aot-1", "aot-2", "aot-3"] {
        for row in &snapshot.reports[name].rows {
            samples
                .entry((super::validate::normalized(&row.bench), row.case.as_str()))
                .or_default()
                .push(parse_duration(&row.median).expect("validated duration"));
        }
    }
    let mut algorithms: Vec<_> = crate::algorithms::ALGORITHMS.iter().collect();
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
    notes(snapshot, "runtime", &mut text);
    text
}

fn warm_table(cases: &[warm::Case], title: &str, text: &mut String) {
    let _ = writeln!(
        text,
        "## {title}\n\nTimes are microseconds per invocation. Floors are measured, never subtracted. A suppressed ratio has its reason in the final column.\n\n| Workload | Input | Fai µs | Rust µs | OCaml µs | F/R | F/O | Eligibility |\n|---|---:|---:|---:|---:|---:|---:|---|"
    );
    for case in cases {
        let time = |side| {
            let value = case.sides.get(side).expect("validated comparison side").time / 1000.0;
            if value < 0.001 { "<0.001".into() } else { format!("{value:.3}") }
        };
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
            time("fai"),
            time("rust"),
            time("ocaml"),
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

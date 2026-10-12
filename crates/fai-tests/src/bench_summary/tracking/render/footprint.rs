//! Process-memory summaries with compact overviews and complete input curves.

use super::display::{benchmark, memory as memory_size, number, ratio};
use super::*;
use crate::bench_summary::{FootprintSample, ReclamationSample};

type Case<'a> = memory::FootprintComparison<'a>;

pub(super) fn report(snapshot: &Snapshot, evidence: &str) -> String {
    let mut text = header(snapshot, "memory", "Memory footprint: Fai, Rust and OCaml", evidence);
    text.push_str("Peak RAM used by each complete process (RSS), including program code, the runtime and allocator storage. Memory after execution is available in the details.\n\n");
    ratio_guide(&mut text, "memory");
    navigation(&mut text);
    let comparisons = memory::comparisons(&snapshot.reports["memory"].footprints);
    let repetitions = *snapshot
        .manifest
        .settings
        .memory_repeats
        .iter()
        .min()
        .expect("validated repetition counts");
    for class in [Comparison::Kernel, Comparison::Application] {
        let selected: Vec<_> =
            comparisons.iter().filter(|case| case.class == Some(class)).collect();
        let largest = largest_inputs(&selected);
        let _ = writeln!(
            text,
            "## {}\n\n{}\n\n**Coverage:** {} benchmarks · {} input/repetition cases · {} fresh processes per language and mode.\n",
            comparison_heading(class),
            comparison_description(class),
            number(largest.len()),
            number(selected.len()),
            number(snapshot.manifest.settings.memory_samples)
        );
        summary_header(&mut text);
        for side in ["rust", "ocaml"] {
            summary_row(
                &mut text,
                side,
                selected
                    .iter()
                    .filter_map(|case| if side == "rust" { case.rust } else { case.ocaml })
                    .collect(),
                selected.len(),
            );
        }
        summary_explanation(&mut text);
        let _ = writeln!(
            text,
            "### At a glance\n\nLargest registered input for each benchmark, with **{} invocation{} per process**. The summary above uses **all measured inputs and repetition counts**.\n",
            number(repetitions),
            if repetitions == 1 { "" } else { "s" }
        );
        let overview: Vec<_> = selected
            .iter()
            .copied()
            .filter(|case| largest[case.name] == case.size && case.iterations == repetitions)
            .collect();
        table(&overview, false, &mut text);
        details(&mut text, "All inputs, repetitions and memory details");
        text.push_str("### All input sizes\n\nPeak process memory. Invocations are complete runs of the workload within each fresh process.\n\n");
        table(&selected, true, &mut text);
        residency(&selected, &largest, &mut text);
        inputs(class, &mut text);
        end_details(&mut text);
    }
    let method = format!(
        "Each displayed peak is the maximum across {} fresh processes, in MiB (1 MiB = 1,048,576 bytes). Ratios and summary statistics use the unrounded measurements.\n\nThe same executable also runs its harness without the workload to measure a baseline. Baselines are never subtracted. A large baseline share still represents a valid total process-footprint comparison. Memory ratios require compatible, uninstrumented measurements and at least five observations per language.\n\nA parent observes each process before the workload and after its result is consumed. Garbage collection and allocator policies run normally, without forced collection or trimming. Memory retained after execution can include unreachable values or cached storage; these RSS observations are distinct from the ownership and live-storage guarantees in the Perceus report.",
        snapshot.manifest.settings.memory_samples
    );
    finish(snapshot, "memory", evidence, &method, &mut text);
    display::sources(snapshot, comparisons.iter().map(|case| case.name), &mut text);
    text
}

fn largest_inputs<'a>(cases: &[&Case<'a>]) -> BTreeMap<&'a str, i64> {
    let mut largest = BTreeMap::<_, i64>::new();
    for case in cases {
        largest
            .entry(case.name)
            .and_modify(|size| *size = (*size).max(case.size))
            .or_insert(case.size);
    }
    largest
}

fn row<'a>(case: &Case<'a>, side: &str) -> &'a FootprintSample {
    case.records.iter().copied().find(|row| row.side == side).expect("validated memory peer")
}

fn maximum(values: &[u64]) -> u64 {
    *values.iter().max().expect("validated memory samples")
}

fn table(cases: &[&Case<'_>], detailed: bool, text: &mut String) {
    if detailed {
        text.push_str("| Benchmark | Input size | Invocations | Fai | Rust | OCaml | Memory vs Rust | Memory vs OCaml |\n|---|---:|---:|---:|---:|---:|---:|---:|\n");
    } else {
        text.push_str("| Benchmark | Input size | Fai | Rust | OCaml | Memory vs Rust | Memory vs OCaml |\n|---|---:|---:|---:|---:|---:|---:|\n");
    }
    for case in cases {
        let _ = write!(text, "| {} | {} |", benchmark(case.name, detailed), number(case.size));
        if detailed {
            let _ = write!(text, " {} |", number(case.iterations));
        }
        for side in ["fai", "rust", "ocaml"] {
            let _ = write!(text, " {} |", memory_size(maximum(&row(case, side).samples_kib)));
        }
        let _ = writeln!(text, " {} | {} |", ratio(case.rust), ratio(case.ocaml));
    }
    text.push('\n');
    if cases.iter().any(|case| case.rust.is_none() || case.ocaml.is_none()) {
        text.push_str("**Omitted ratios (—):** the pair has missing, incompatible, instrumented or insufficient observations. It is excluded from the summary.\n\n");
    }
}

fn residency(cases: &[&Case<'_>], largest: &BTreeMap<&str, i64>, text: &mut String) {
    text.push_str("### Before and after execution\n\nLargest input at each repetition count, **MiB throughout**. Arrows show the ready process → the same process after consuming its result. The baseline column shows harness-only peak → after values, in **Fai / Rust / OCaml** order. These are maximum observations; the raw records retain each paired process reading.\n\n| Benchmark | Input size | Invocations | Fai before → after | Rust before → after | OCaml before → after | Baseline peak → after (Fai / Rust / OCaml) |\n|---|---:|---:|---:|---:|---:|---|\n");
    let mib = |value| memory_size(value).trim_end_matches(" MiB").to_owned();
    for case in cases.iter().filter(|case| largest[case.name] == case.size) {
        let current = |side| -> &ReclamationSample {
            row(case, side).reclamation.as_ref().expect("validated before/after readings")
        };
        let _ = write!(
            text,
            "| {} | {} | {} |",
            benchmark(case.name, true),
            number(case.size),
            number(case.iterations)
        );
        for side in ["fai", "rust", "ocaml"] {
            let value = current(side);
            let _ = write!(
                text,
                " {} → {} |",
                mib(maximum(&value.before_kib)),
                mib(maximum(&value.after_kib))
            );
        }
        let baseline = ["fai", "rust", "ocaml"]
            .map(|side| {
                format!(
                    "{} → {}",
                    mib(maximum(&row(case, side).floor_kib)),
                    mib(maximum(&current(side).floor_after_kib))
                )
            })
            .join(" / ");
        let _ = writeln!(text, " {baseline} |");
    }
    text.push('\n');
}

fn inputs(class: Comparison, text: &mut String) {
    text.push_str("### Input definitions\n\nFixed-state cases increase work without growing their resident dataset. Storage cases increase the dataset itself.\n\n| Benchmark | Input unit | What grows | Registered sizes |\n|---|---|---|---|\n");
    let mut cases: Vec<_> = crate::memory_workloads::algorithms()
        .filter(|case| comparison_class(case.module) == class)
        .collect();
    cases.sort_by_key(|case| case.module);
    for case in cases {
        let profile = crate::memory_workloads::profile(case.module).expect("memory profile");
        let sizes = profile.sizes.iter().map(number).collect::<Vec<_>>().join("; ");
        let _ = writeln!(
            text,
            "| {} | {} | {} | {sizes} |",
            benchmark(case.module, true),
            profile.unit,
            profile.scale
        );
    }
    text.push('\n');
    if class == Comparison::Kernel {
        text.push_str("A MemoryTree depth of d means 2^d − 1 nonempty nodes. Each MemoryUtf8 chunk is nine bytes and four Unicode characters. Array/body inputs count elements.\n\n");
    }
}

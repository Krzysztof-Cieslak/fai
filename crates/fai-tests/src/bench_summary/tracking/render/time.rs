//! Runtime overviews, complete timing tables and overhead details.

use super::display::{benchmark, duration, language, number, ratio};
use super::*;

pub(super) fn report(snapshot: &Snapshot, evidence: &str) -> String {
    let mut text =
        header(snapshot, "runtime", "Runtime performance: Fai, Rust and OCaml", evidence);
    text.push_str("Time taken by native compiled programs after startup. Program startup and shutdown are reported separately below.\n\n");
    ratio_guide(&mut text, "time");
    navigation(&mut text);
    let cases = warm::summarize(&snapshot.reports["warm"].warm);
    for class in [Comparison::Kernel, Comparison::Application] {
        let selected: Vec<_> =
            cases.iter().filter(|case| comparison_class(&case.algorithm) == class).collect();
        let workloads = selected.iter().map(|case| &case.algorithm).collect::<BTreeSet<_>>().len();
        let _ = writeln!(
            text,
            "## {}\n\n{}\n\n**Coverage:** {} benchmarks · {} input cases.\n",
            comparison_heading(class),
            comparison_description(class),
            number(workloads),
            number(selected.len())
        );
        let ratios = class == Comparison::Kernel;
        if ratios {
            summary_header(&mut text);
            for side in ["rust", "ocaml"] {
                summary_row(
                    &mut text,
                    side,
                    selected.iter().filter_map(|case| case.ratio(side)).collect(),
                    selected.len(),
                );
            }
            summary_explanation(&mut text);
        } else {
            text.push_str("These timings include the selected libraries' algorithms and policies. They are presented as complete workloads.\n\n");
        }
        let mut largest = BTreeMap::<_, &warm::Case>::new();
        for case in &selected {
            largest
                .entry(case.algorithm.as_str())
                .and_modify(|old| {
                    if old.size < case.size {
                        *old = case;
                    }
                })
                .or_insert(case);
        }
        text.push_str("### At a glance\n\nLargest registered input for each benchmark. The summary above, where present, uses **all measured inputs**.\n\n");
        table(&largest.into_values().collect::<Vec<_>>(), ratios, false, &mut text);
        details(&mut text, "All input sizes and timing overhead");
        text.push_str("### All input sizes\n\n");
        table(&selected, ratios, true, &mut text);
        overhead(&selected, &mut text);
        if ratios {
            gaps(&selected, &mut text);
        }
        end_details(&mut text);
        details(&mut text, "Including program startup and shutdown");
        end_to_end(snapshot, class, &mut text);
        end_details(&mut text);
    }

    text.push_str("## Additional algorithm measurements\n\nThese focused string and sorting measurements are separate from the main summary.\n\n");
    details(&mut text, "String and sorting results");
    let components = warm::summarize(&snapshot.reports["components"].warm);
    let selected: Vec<_> = components.iter().collect();
    table(&selected, true, true, &mut text);
    overhead(&selected, &mut text);
    end_details(&mut text);
    let method = format!(
        "All three languages run native ahead-of-time compiled programs in isolated workers. Inputs are supplied at runtime. Timings are the median of {} pass medians, each with {} paired observations after warmup.\n\nThe timing overhead is measured by running the same harness without the workload. It is never subtracted. A time ratio is omitted when either language's overhead reaches 10% of its elapsed time. Cases used in the summary are counted separately for each comparison language.\n\nStartup/shutdown timings include a fresh process, its workload, output and exit; they contribute no weight to the running-time summary.",
        snapshot.manifest.settings.warm_passes, snapshot.manifest.settings.warm_samples
    );
    finish(snapshot, "runtime", evidence, &method, &mut text);
    display::sources(
        snapshot,
        cases.iter().chain(&components).map(|case| case.algorithm.as_str()),
        &mut text,
    );
    text
}

fn table(cases: &[&warm::Case], ratios: bool, compact: bool, text: &mut String) {
    if ratios {
        text.push_str("| Benchmark | Input size | Fai | Rust | OCaml | Time vs Rust | Time vs OCaml |\n|---|---:|---:|---:|---:|---:|---:|\n");
    } else {
        text.push_str(
            "| Benchmark | Input size | Fai | Rust | OCaml |\n|---|---:|---:|---:|---:|\n",
        );
    }
    for case in cases {
        let time =
            |side| case.sides.get(side).map_or_else(|| "—".into(), |value| duration(value.time));
        let _ = write!(
            text,
            "| {} | {} | {} | {} | {} |",
            benchmark(&case.algorithm, compact),
            number(case.size),
            time("fai"),
            time("rust"),
            time("ocaml")
        );
        if ratios {
            let _ =
                write!(text, " {} | {} |", ratio(case.ratio("rust")), ratio(case.ratio("ocaml")));
        }
        text.push('\n');
    }
    text.push('\n');
    if ratios {
        omitted(cases, text);
    }
}

fn omitted(cases: &[&warm::Case], text: &mut String) {
    let mut reasons = BTreeSet::new();
    for case in cases {
        for side in ["rust", "ocaml"] {
            if case.ratio(side).is_some() {
                continue;
            }
            if !case.sides.contains_key("fai") || !case.sides.contains_key(side) {
                reasons.insert("a comparison language is unavailable");
            } else if !case.complete {
                reasons.insert("the measurements are incomplete");
            } else if !case.matched {
                reasons.insert("the workload does not share a reviewed algorithm");
            } else {
                reasons.insert("measurement overhead reached the 10% limit");
            }
        }
    }
    if !reasons.is_empty() {
        let _ = writeln!(
            text,
            "**Omitted ratios (—):** {}. These comparisons are excluded from the summary; overhead measurements are listed in the details.\n",
            reasons.into_iter().collect::<Vec<_>>().join("; ")
        );
    }
}

fn overhead(cases: &[&warm::Case], text: &mut String) {
    text.push_str("### Timing overhead\n\nShare of elapsed time used by the measurement harness, measured independently. Shared-algorithm ratios require **less than 10%** on both sides.\n\n| Benchmark | Input size | Fai overhead | Rust overhead | OCaml overhead |\n|---|---:|---:|---:|---:|\n");
    for case in cases {
        let share = |side| {
            case.sides
                .get(side)
                .map_or_else(|| "—".into(), |value| display::overhead(value.floor, value.time))
        };
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} | {} |",
            benchmark(&case.algorithm, true),
            number(case.size),
            share("fai"),
            share("rust"),
            share("ocaml")
        );
    }
    text.push('\n');
}

fn gaps(cases: &[&warm::Case], text: &mut String) {
    let mut gaps = Vec::new();
    for case in cases {
        for side in ["rust", "ocaml"] {
            if let Some(ratio) = case.ratio(side).filter(|ratio| *ratio > 1.0) {
                gaps.push((ratio, case.algorithm.as_str(), case.size, side));
            }
        }
    }
    gaps.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| (a.1, a.2, a.3).cmp(&(b.1, b.2, b.3))));
    if gaps.is_empty() {
        return;
    }
    text.push_str("### Largest timing gaps\n\nCases where Fai takes more time, across all measured inputs.\n\n| Benchmark | Input size | Compared with | Relative time |\n|---|---:|---|---:|\n");
    for (value, name, size, side) in gaps.into_iter().take(6) {
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} |",
            benchmark(name, true),
            number(size),
            language(side),
            ratio(Some(value))
        );
    }
    text.push('\n');
}

fn end_to_end(snapshot: &Snapshot, class: Comparison, text: &mut String) {
    text.push_str("### Complete process time\n\nMedian time for one complete process, including startup, the workload, output and shutdown. Each value is the median of three pass medians, with at least 100 observations per pass.\n\n| Benchmark | Input size | Fai | Rust | OCaml |\n|---|---:|---:|---:|---:|\n");
    let mut samples = BTreeMap::<_, Vec<_>>::new();
    for name in ["aot-1", "aot-2", "aot-3"] {
        for row in &snapshot.reports[name].rows {
            samples
                .entry((super::super::validate::normalized(&row.bench), row.case.as_str()))
                .or_default()
                .push(parse_duration(&row.median).expect("validated duration"));
        }
    }
    let mut algorithms: Vec<_> = crate::algorithms::ALGORITHMS
        .iter()
        .filter(|algorithm| comparison_class(algorithm.module) == class)
        .collect();
    algorithms.sort_by_key(|algorithm| algorithm.module);
    for algorithm in algorithms {
        let key = super::super::validate::normalized(algorithm.module);
        let time = |side| duration(median(samples[&(key.clone(), side)].clone()));
        let _ = writeln!(
            text,
            "| {} | {} | {} | {} | {} |",
            benchmark(algorithm.module, true),
            number(algorithm.aot_size),
            time("fai"),
            time("rust"),
            time("ocaml")
        );
    }
    text.push('\n');
}

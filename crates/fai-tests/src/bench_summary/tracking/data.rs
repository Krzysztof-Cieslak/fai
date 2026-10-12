//! Versioned numeric report data, independent of presentation and Markdown.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{REPORT_DATA_VERSION, Snapshot};
use crate::bench_summary::{AllocationSample, memory, parse_duration, warm};
use crate::benchmark_contracts::Comparison;

fn summary(mut values: Vec<f64>, total: usize) -> Value {
    if values.is_empty() {
        return json!({"used": 0, "total": total, "geometricMean": null, "arithmeticMean": null, "p90": null});
    }
    debug_assert!(values.iter().all(|value| value.is_finite() && *value > 0.0));
    values.sort_by(f64::total_cmp);
    let used = values.len();
    json!({
        "used": used, "total": total,
        "geometricMean": (values.iter().map(|value| value.ln()).sum::<f64>() / used as f64).exp(),
        "arithmeticMean": values.iter().sum::<f64>() / used as f64,
        "p90": values[(used * 9).div_ceil(10) - 1],
    })
}

fn class(name: &str) -> Comparison {
    crate::benchmark_contracts::for_module(name).expect("validated workload").comparison
}

fn time_case(case: &warm::Case) -> Value {
    let languages: BTreeMap<_, _> = case
        .sides
        .iter()
        .map(|(name, side)| {
            (name, json!({"timeNs": side.time, "overheadFraction": side.floor / side.time}))
        })
        .collect();
    let omissions: BTreeMap<_, _> = ["rust", "ocaml"]
        .into_iter()
        .filter(|side| case.ratio(side).is_none())
        .map(|side| {
            let reason = if !case.sides.contains_key("fai") || !case.sides.contains_key(side) {
                "missing-peer"
            } else if !case.complete {
                "incomplete"
            } else if !case.matched {
                "different-algorithm"
            } else {
                "overhead"
            };
            (side, reason)
        })
        .collect();
    json!({"benchmark": case.algorithm, "input": case.size,
        "classification": class(&case.algorithm).as_str(), "languages": languages,
        "ratios": {"rust": case.ratio("rust"), "ocaml": case.ratio("ocaml")},
        "omissions": omissions})
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) { (values[mid - 1] + values[mid]) / 2.0 } else { values[mid] }
}

fn end_to_end(snapshot: &Snapshot) -> Vec<Value> {
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
    algorithms.sort_by_key(|case| case.module);
    algorithms
        .into_iter()
        .map(|case| {
            let key = super::validate::normalized(case.module);
            let languages: BTreeMap<_, _> = ["fai", "rust", "ocaml"]
                .into_iter()
                .map(|side| {
                    (
                        side,
                        median(
                            samples
                                .get_mut(&(key.clone(), side))
                                .expect("validated process measurements"),
                        ),
                    )
                })
                .collect();
            json!({"benchmark": case.module, "input": case.aot_size,
            "classification": class(case.module).as_str(), "timesNs": languages})
        })
        .collect()
}

fn maximum(values: &[u64]) -> u64 {
    *values.iter().max().expect("validated memory observations")
}

fn memory_case(case: &memory::FootprintComparison<'_>) -> Value {
    let languages: BTreeMap<_, _> = case.records.iter().map(|row| {
        let resident = row.reclamation.as_ref().expect("validated before/after observations");
        (&row.side, json!({"peakKib": maximum(&row.samples_kib),
            "baselinePeakKib": maximum(&row.floor_kib), "beforeKib": maximum(&resident.before_kib),
            "afterKib": maximum(&resident.after_kib), "baselineAfterKib": maximum(&resident.floor_after_kib)}))
    }).collect();
    json!({"benchmark": case.name, "input": case.size, "invocations": case.iterations,
        "classification": case.class.expect("validated memory class").as_str(),
        "languages": languages, "ratios": {"rust": case.rust, "ocaml": case.ocaml}})
}

fn allocation_counts(row: &AllocationSample) -> Value {
    let n = |name: &str| row.counters[name].as_u64().expect("validated allocation counter");
    json!({"requests": n("allocations"), "allocatedBytes": n("allocatedBytes"),
        "extraPeakLiveBytes": n("peakLiveBytes") - n("baselineLiveBytes"),
        "peakStorageBytes": n("peakBackingBytes"),
        "retainedStorageBytes": n("retainedSlabBytes") + n("retainedMappedBytes") + n("retainedSystemBytes")})
}

fn allocations(snapshot: &Snapshot) -> Vec<Value> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for row in &snapshot.reports["allocations"].allocations {
        groups.entry((&row.algorithm, row.size)).or_default().push(row);
    }
    groups
        .into_iter()
        .map(|((name, size), rows)| {
            let enabled = rows
                .iter()
                .find(|row| row.control == "enabled")
                .expect("validated enabled control");
            let disabled = rows
                .iter()
                .find(|row| row.control != "enabled")
                .expect("validated disabled control");
            json!({"benchmark": name, "input": size, "disabledControl": disabled.control,
            "enabled": allocation_counts(enabled), "disabled": allocation_counts(disabled)})
        })
        .collect()
}

pub(super) fn export(snapshot: &Snapshot) -> Value {
    let times = warm::summarize(&snapshot.reports["warm"].warm);
    let shared: Vec<_> = times.iter().filter(|case| case.matched).collect();
    let time_summaries: BTreeMap<_, _> = ["rust", "ocaml"]
        .into_iter()
        .map(|side| {
            (
                side,
                summary(shared.iter().filter_map(|case| case.ratio(side)).collect(), shared.len()),
            )
        })
        .collect();
    let memory = memory::comparisons(&snapshot.reports["memory"].footprints);
    let memory_summaries: BTreeMap<_, _> = [Comparison::Kernel, Comparison::Application]
        .into_iter()
        .map(|class| {
            let selected: Vec<_> = memory.iter().filter(|case| case.class == Some(class)).collect();
            let sides: BTreeMap<_, _> = ["rust", "ocaml"]
                .into_iter()
                .map(|side| {
                    (
                        side,
                        summary(
                            selected
                                .iter()
                                .filter_map(
                                    |case| if side == "rust" { case.rust } else { case.ocaml },
                                )
                                .collect(),
                            selected.len(),
                        ),
                    )
                })
                .collect();
            (class.as_str(), sides)
        })
        .collect();
    let inputs: Vec<_> = crate::memory_workloads::algorithms()
        .map(|case| {
            let profile = crate::memory_workloads::profile(case.module).expect("memory profile");
            json!({"benchmark": case.module, "classification": class(case.module).as_str(),
            "unit": profile.unit, "scale": profile.scale, "sizes": profile.sizes})
        })
        .collect();
    let guards: BTreeMap<_, _> = ["perceus_space", "perceus_storage"]
        .into_iter()
        .map(|name| {
            let step = snapshot
                .manifest
                .steps
                .iter()
                .find(|step| step.name == name)
                .expect("validated memory guard");
            (name, step.passed.expect("validated guard count"))
        })
        .collect();
    let m = &snapshot.manifest;
    json!({"schemaVersion": REPORT_DATA_VERSION,
        "measurement": {"runId": m.run_id, "repository": m.repository,
            "measuredRevision": m.measured_revision, "completedAt": m.completed_at,
            "methodology": m.methodology, "memoryMethodology": crate::memory_workloads::METHODOLOGY,
            "machine": m.machine, "toolchains": m.toolchains, "settings": m.settings, "notes": m.notes},
        "runtime": {"cases": times.iter().map(time_case).collect::<Vec<_>>(),
            "summary": time_summaries,
            "components": warm::summarize(&snapshot.reports["components"].warm).iter().map(time_case).collect::<Vec<_>>(),
            "endToEnd": end_to_end(snapshot)},
        "memory": {"cases": memory.iter().map(memory_case).collect::<Vec<_>>(), "summaries": memory_summaries, "inputs": inputs},
        "allocations": {"guards": guards, "cases": allocations(snapshot)}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_use_unrounded_equal_weight_ratios() {
        let value = summary(vec![0.5, 1.0, 2.0, 4.0], 5);
        assert_eq!(value["used"], 4);
        assert_eq!(value["total"], 5);
        assert!((value["geometricMean"].as_f64().unwrap() - 2.0_f64.sqrt()).abs() < 1e-12);
        assert_eq!(value["arithmeticMean"], 1.875);
        assert_eq!(value["p90"], 4.0);
    }

    #[test]
    fn p90_uses_nearest_rank_without_interpolation() {
        assert_eq!(summary((1..=20).map(f64::from).rev().collect(), 20)["p90"], 18.0);
    }

    #[test]
    fn empty_ratios_retain_coverage_without_inventing_statistics() {
        let value = summary(Vec::new(), 3);
        assert_eq!(value["used"], 0);
        assert_eq!(value["total"], 3);
        assert!(
            value["geometricMean"].is_null()
                && value["arithmeticMean"].is_null()
                && value["p90"].is_null()
        );
    }
}

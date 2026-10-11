//! Raw warm-AOT samples and floor-aware cross-language summaries.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::{is_side, json_str, ratio_cell};

/// One paired pass of batched native-worker measurements. Durations include the
/// request round trip; compilation, process startup and input setup are excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmSample {
    /// Version of packaging and measurement boundaries.
    pub methodology: String,
    /// Reviewed source-level workload version.
    pub workload_version: String,
    /// Matched kernel or application comparison, fixed before measurement.
    pub comparison: String,
    /// Identity of the complete set of paired executable hashes.
    pub comparison_id: String,
    /// Registered workload name.
    pub algorithm: String,
    /// Runtime-supplied workload size.
    pub size: i64,
    /// `fai`, `rust` or `ocaml`.
    pub side: String,
    /// Identical invocation count used by every side of this case.
    pub batch: u64,
    /// One-based pass index.
    pub pass: u32,
    /// Number of passes expected for a complete comparison.
    pub passes: u32,
    /// Whole-batch elapsed nanoseconds, in observation order.
    pub samples_ns: Vec<u64>,
    /// Same-size harness-floor observations, without the workload.
    pub floor_ns: Vec<u64>,
}

impl WarmSample {
    /// Stable tab-separated record retained in the raw benchmark log.
    #[must_use]
    pub fn to_line(&self) -> String {
        let durations =
            |values: &[u64]| values.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        format!(
            "WARMSTAT2\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.methodology,
            self.workload_version,
            self.comparison,
            self.comparison_id,
            self.algorithm,
            self.size,
            self.side,
            self.batch,
            self.pass,
            self.passes,
            durations(&self.samples_ns),
            durations(&self.floor_ns)
        )
    }

    pub(super) fn parse(line: &str) -> Option<Self> {
        let (metadata, payload) = if let Some(payload) = line.strip_prefix("WARMSTAT2\t") {
            let parts: Vec<_> = payload.splitn(5, '\t').collect();
            let [method, version, comparison, identity, rest] = parts.as_slice() else {
                return None;
            };
            if [method, version, comparison, identity].iter().any(|field| field.is_empty()) {
                return None;
            }
            ([*method, *version, *comparison, *identity], *rest)
        } else {
            (
                ["legacy", "unversioned", "historical", "unidentified"],
                line.strip_prefix("WARMSTAT\t")?,
            )
        };
        let fields: Vec<_> = payload.split('\t').collect();
        let [algorithm, size, side, batch, pass, passes, samples, floor] = fields.as_slice() else {
            return None;
        };
        let parse_times = |text: &str| -> Option<Vec<u64>> {
            let values: Vec<u64> =
                text.split(',').map(str::parse).collect::<Result<_, _>>().ok()?;
            (!values.is_empty() && values.iter().all(|&value| value > 0)).then_some(values)
        };
        let value = Self {
            methodology: metadata[0].into(),
            workload_version: metadata[1].into(),
            comparison: metadata[2].into(),
            comparison_id: metadata[3].into(),
            algorithm: (*algorithm).into(),
            size: size.parse().ok()?,
            side: (*side).into(),
            batch: batch.parse().ok()?,
            pass: pass.parse().ok()?,
            passes: passes.parse().ok()?,
            samples_ns: parse_times(samples)?,
            floor_ns: parse_times(floor)?,
        };
        (!value.algorithm.is_empty()
            && is_side(&value.side)
            && value.size >= 0
            && value.batch > 0
            && value.batch <= crate::benchmark_aot::MAX_BATCH as u64
            && value.pass > 0
            && value.pass <= value.passes
            && value.samples_ns.len() == value.floor_ns.len())
        .then_some(value)
    }

    pub(super) fn json(&self) -> String {
        format!(
            "{{\"methodology\":{},\"workloadVersion\":{},\"comparison\":{},\"comparisonId\":{},\"algorithm\":{},\"size\":{},\"side\":{},\"batch\":{},\"pass\":{},\"passes\":{},\"samplesNs\":{:?},\"floorNs\":{:?}}}",
            json_str(&self.methodology),
            json_str(&self.workload_version),
            json_str(&self.comparison),
            json_str(&self.comparison_id),
            json_str(&self.algorithm),
            self.size,
            json_str(&self.side),
            self.batch,
            self.pass,
            self.passes,
            self.samples_ns,
            self.floor_ns
        )
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

pub(super) struct Side {
    pub time: f64,
    pub floor: f64,
}

impl Side {
    pub fn limited(&self) -> bool {
        self.floor >= self.time * 0.1
    }
}

pub(super) struct Case {
    pub algorithm: String,
    pub size: i64,
    pub batch: u64,
    pub methodology: String,
    pub version: String,
    pub sides: BTreeMap<String, Side>,
    pub complete: bool,
    pub matched: bool,
}

impl Case {
    pub fn ratio(&self, peer: &str) -> Option<f64> {
        let fai = self.sides.get("fai")?;
        let peer = self.sides.get(peer)?;
        (self.complete && self.matched && !fai.limited() && !peer.limited())
            .then_some(fai.time / peer.time)
    }
}

pub(super) fn summarize(samples: &[WarmSample]) -> Vec<Case> {
    let mut cases: BTreeMap<(&str, i64, &str, &str, &str), Vec<&WarmSample>> = BTreeMap::new();
    for sample in samples {
        cases
            .entry((
                &sample.algorithm,
                sample.size,
                &sample.methodology,
                &sample.workload_version,
                &sample.comparison_id,
            ))
            .or_default()
            .push(sample);
    }
    let mut result = Vec::new();
    for ((algorithm, size, methodology, version, _), records) in cases {
        let batch = records[0].batch;
        let passes = records[0].passes;
        let mut complete =
            records.iter().all(|record| record.batch == batch && record.passes == passes);
        let contract = crate::benchmark_contracts::for_module(algorithm);
        let matched = methodology == crate::benchmark_contracts::METHODOLOGY
            && contract.is_some_and(|contract| {
                contract.version == version
                    && contract.comparison == crate::benchmark_contracts::Comparison::Kernel
            })
            && records.iter().all(|record| record.comparison == "matched-kernel");
        let mut sides = BTreeMap::new();
        for side in ["fai", "rust", "ocaml"] {
            let selected: Vec<_> = records.iter().filter(|record| record.side == side).collect();
            if selected.is_empty() {
                continue;
            }
            let indices: BTreeSet<_> = selected.iter().map(|record| record.pass).collect();
            complete &= selected.len() == passes as usize && indices.len() == passes as usize;
            let value = |floor: bool| {
                median(
                    selected
                        .iter()
                        .map(|record| {
                            let values = if floor { &record.floor_ns } else { &record.samples_ns };
                            median(values.iter().map(|&value| value as f64).collect())
                                / record.batch as f64
                        })
                        .collect(),
                )
            };
            sides.insert(side.to_owned(), Side { time: value(false), floor: value(true) });
        }
        complete &= sides.contains_key("fai") && sides.contains_key("rust");
        result.push(Case {
            algorithm: algorithm.into(),
            size,
            batch,
            methodology: methodology.into(),
            version: version.into(),
            sides,
            complete,
            matched,
        });
    }
    result
}

pub(super) fn render(samples: &[WarmSample]) -> String {
    if samples.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "### Warm AOT — qualified kernel comparison\n\nAll sides execute AOT code in isolated persistent workers. Times are median-of-pass-medians **per invocation**, including the batched harness. Startup, input setup and compilation are excluded. Floors are measured, never subtracted. Ratios require matching workload versions/executable sets and a reviewed kernel contract. Application and historical rows retain absolute observations. A ratio is also suppressed when either side's floor is at least 10% of elapsed time or the case is incomplete. Values are point estimates, not statistically established win counts.\n\n| Workload | Size | Batch | Fai ns/call | Rust ns/call | OCaml ns/call | Fai/Rust | Fai/OCaml | Floor F/R/O | Status |\n|---|---:|---:|---:|---:|---:|---:|---:|---|---|\n",
    );
    for case in summarize(samples) {
        let Case { algorithm, size, batch, methodology, version, sides, complete, matched } = &case;
        let time =
            |side| sides.get(side).map_or_else(|| "—".into(), |value| format!("{:.3}", value.time));
        let floor = |side| {
            sides.get(side).map_or_else(
                || "—".into(),
                |value| format!("{:.1}%", 100.0 * value.floor / value.time),
            )
        };
        let ratio = |peer| ratio_cell(case.ratio(peer));
        let limited: Vec<_> = sides
            .iter()
            .filter(|(_, value)| value.limited())
            .map(|(side, _)| side.as_str())
            .collect();
        let status = if !*matched {
            format!(
                "{}: {version}",
                if methodology == "legacy" {
                    "historical/unqualified"
                } else {
                    "application/unqualified"
                }
            )
        } else if !*complete {
            "incomplete/incompatible".into()
        } else if !limited.is_empty() {
            format!("floor-limited: {}", limited.join(", "))
        } else {
            "measured".into()
        };
        let _ = writeln!(
            out,
            "| {} | {size} | {batch} | {} | {} | {} | {} | {} | {}/{}/{} | {status} |",
            super::escape(algorithm),
            time("fai"),
            time("rust"),
            time("ocaml"),
            ratio("rust"),
            ratio("ocaml"),
            floor("fai"),
            floor("rust"),
            floor("ocaml")
        );
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(side: &str, time: u64, floor: u64) -> WarmSample {
        WarmSample {
            methodology: crate::benchmark_contracts::METHODOLOGY.into(),
            workload_version: crate::benchmark_contracts::for_module("Fib").unwrap().version.into(),
            comparison: "matched-kernel".into(),
            comparison_id: "fixture-binaries".into(),
            algorithm: "Fib".into(),
            size: 28,
            side: side.into(),
            batch: 4,
            pass: 1,
            passes: 1,
            samples_ns: vec![time],
            floor_ns: vec![floor],
        }
    }

    #[test]
    fn wire_round_trip_retains_raw_order_and_metadata() {
        let value = WarmSample {
            samples_ns: vec![30, 10, 20],
            floor_ns: vec![2, 3, 1],
            ..sample("fai", 1, 1)
        };
        assert_eq!(WarmSample::parse(&value.to_line()), Some(value));
    }

    #[test]
    fn zero_batch_is_rejected() {
        assert!(WarmSample::parse("WARMSTAT\tFib\t28\tfai\t0\t1\t1\t10\t1").is_none());
    }

    #[test]
    fn mismatched_floor_count_is_rejected() {
        assert!(WarmSample::parse("WARMSTAT\tFib\t28\tfai\t4\t1\t1\t10,20\t1").is_none());
    }

    #[test]
    fn zero_duration_is_rejected() {
        assert!(WarmSample::parse("WARMSTAT\tFib\t28\tfai\t4\t1\t1\t0\t1").is_none());
    }

    #[test]
    fn complete_above_floor_cases_show_ratios() {
        let text = render(&[sample("fai", 10000, 10), sample("rust", 20000, 10)]);
        assert!(
            text.contains("2500.000") && text.contains("0.50×") && text.contains("measured"),
            "{text}"
        );
    }

    #[test]
    fn floor_limited_cases_do_not_claim_parity() {
        let text = render(&[sample("fai", 10000, 10), sample("rust", 10000, 9000)]);
        assert!(text.contains("floor-limited: rust"), "{text}");
        assert!(!text.contains("1.00×"), "{text}");
    }

    #[test]
    fn incompatible_batch_counts_do_not_produce_a_ratio() {
        let text = render(&[
            sample("fai", 10000, 10),
            WarmSample { batch: 8, ..sample("rust", 20000, 10) },
        ]);
        assert!(text.contains("incomplete/incompatible") && !text.contains("1.00×"), "{text}");
    }

    #[test]
    fn partial_passes_do_not_produce_a_ratio() {
        let text = render(&[
            WarmSample { passes: 2, ..sample("fai", 10000, 10) },
            WarmSample { passes: 2, ..sample("rust", 20000, 10) },
        ]);
        assert!(text.contains("incomplete/incompatible") && !text.contains("0.50×"), "{text}");
    }

    #[test]
    fn unknown_language_is_rejected() {
        assert!(WarmSample::parse("WARMSTAT\tFib\t28\tunknown\t4\t1\t1\t10\t1").is_none());
    }

    #[test]
    fn different_executable_sets_never_form_a_comparison() {
        let text = render(&[
            sample("fai", 10000, 10),
            WarmSample { comparison_id: "other-build".into(), ..sample("rust", 20000, 10) },
        ]);
        assert!(!text.contains("0.50×"));
        assert!(text.contains("incomplete/incompatible"));
    }

    #[test]
    fn application_rows_do_not_become_compiler_ratios() {
        let app = |side| WarmSample {
            algorithm: "ListSort".into(),
            comparison: "application".into(),
            ..sample(side, 10000, 10)
        };
        let text = render(&[app("fai"), app("rust")]);
        assert!(text.contains("application/unqualified"));
        assert!(!text.contains("1.00×"));
    }

    #[test]
    fn unversioned_observations_remain_historical() {
        let row = WarmSample::parse("WARMSTAT\tFib\t28\tfai\t4\t1\t1\t10000\t10").unwrap();
        assert!(render(&[row]).contains("historical/unqualified"));
    }

    #[test]
    fn pass_outside_the_declared_run_is_rejected() {
        assert!(WarmSample::parse("WARMSTAT\tFib\t28\tfai\t4\t3\t2\t10\t1").is_none());
    }

    #[test]
    fn report_preserves_raw_warm_samples_as_json() {
        let first = sample("fai", 10000, 10);
        let second = sample("rust", 20000, 10);
        let report = crate::bench_summary::Report::parse(&format!(
            "{}\n{}\n",
            first.to_line(),
            second.to_line()
        ));
        let parsed: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        assert_eq!(parsed["warmAot"][0]["samplesNs"], serde_json::json!([10000]));
        assert_eq!(parsed["warmAot"][1]["batch"], 4);
        assert!(report.to_markdown(&crate::bench_summary::LinkBase::default()).contains("0.50×"));
    }
}

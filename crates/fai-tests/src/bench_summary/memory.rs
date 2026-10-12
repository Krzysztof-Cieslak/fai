//! Versioned process footprints and same-source compiler-allocation experiments.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// Paired current-RSS observations around workload execution, before process exit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReclamationSample {
    /// The actual workload process paused before execution.
    pub before_kib: Vec<u64>,
    /// The same process paused after its workload result has been consumed.
    pub after_kib: Vec<u64>,
    /// Matching no-workload processes paused before execution.
    pub floor_before_kib: Vec<u64>,
    /// Matching no-workload processes paused after execution.
    pub floor_after_kib: Vec<u64>,
}

/// Raw repetitions from one isolated process-footprint comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FootprintSample {
    /// Packaging and protocol version.
    pub methodology: String,
    /// Reviewed workload name.
    pub algorithm: String,
    /// Source-level workload version.
    pub workload_version: String,
    /// Matched kernel or application-level comparison.
    pub comparison: String,
    /// Hash of the complete peer executable set.
    pub comparison_id: String,
    /// Measured peer.
    pub side: String,
    /// Exact measured executable's content hash.
    pub executable_hash: String,
    /// Runtime input size.
    pub size: i64,
    /// Number of whole-workload invocations in each fresh process.
    pub iterations: usize,
    /// Individual peak RSS values, without aggregation or subtraction.
    pub samples_kib: Vec<u64>,
    /// Same executable and arguments, executing the floor instead of the workload.
    pub floor_kib: Vec<u64>,
    /// Allocation counters were compiled into the measured runtime.
    pub instrumented: bool,
    /// Absent in historical self-reported peak-only observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reclamation: Option<ReclamationSample>,
}

impl FootprintSample {
    /// Emits a versioned record that cannot be mistaken for legacy MEMSTAT.
    pub fn to_line(&self) -> String {
        format!(
            "{}\t{}",
            if self.reclamation.is_some() { "MEMSTAT3" } else { "MEMSTAT2" },
            serde_json::to_string(self).unwrap()
        )
    }

    pub(super) fn parse(line: &str) -> Option<Self> {
        let (body, current) = line
            .strip_prefix("MEMSTAT3\t")
            .map(|body| (body, true))
            .or_else(|| line.strip_prefix("MEMSTAT2\t").map(|body| (body, false)))?;
        let value: Self = serde_json::from_str(body).ok()?;
        let reclamation_valid = match (&value.reclamation, current) {
            (None, false) => true,
            (Some(row), true) => {
                let valid = |values: &[u64], peaks: &[u64]| {
                    values.len() == peaks.len()
                        && values.iter().zip(peaks).all(|(&n, &peak)| n > 0 && n <= peak)
                };
                value.methodology == crate::memory_workloads::METHODOLOGY
                    && valid(&row.before_kib, &value.samples_kib)
                    && valid(&row.after_kib, &value.samples_kib)
                    && valid(&row.floor_before_kib, &value.floor_kib)
                    && valid(&row.floor_after_kib, &value.floor_kib)
            }
            _ => false,
        };
        (reclamation_valid
            && super::is_side(&value.side)
            && !value.algorithm.is_empty()
            && !value.methodology.is_empty()
            && !value.workload_version.is_empty()
            && !value.comparison_id.is_empty()
            && !value.executable_hash.is_empty()
            && value.size >= 0
            && value.iterations <= crate::benchmark_aot::MAX_BATCH
            && !value.samples_kib.is_empty()
            && value.samples_kib.len() == value.floor_kib.len()
            && value.samples_kib.iter().chain(&value.floor_kib).all(|&n| n > 0))
        .then_some(value)
    }
}

/// One same-source Fai allocation observation, collected in an optimized build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AllocationSample {
    /// Diagnostic protocol version.
    pub methodology: String,
    /// Workload name.
    pub algorithm: String,
    /// One disabled pass, or enabled.
    pub control: String,
    /// Exact shared workload source hash.
    pub source_hash: String,
    /// Compiler build identity, shared by the paired observations.
    pub compiler_id: String,
    /// Exact executable identity.
    pub executable_hash: String,
    /// Runtime input size.
    pub size: i64,
    /// Number of complete workload invocations.
    pub iterations: usize,
    /// Object and backing statistics retain their explicit accounting domain.
    pub counters: serde_json::Value,
}

impl AllocationSample {
    /// Emits one causal-diagnostic record with exact source and executable identities.
    pub fn to_line(&self) -> String {
        format!("ALLOCSTAT\t{}", serde_json::to_string(self).unwrap())
    }

    pub(super) fn parse(line: &str) -> Option<Self> {
        let value: Self = serde_json::from_str(line.strip_prefix("ALLOCSTAT\t")?).ok()?;
        let counters_valid = [
            "allocations",
            "allocatedBytes",
            "baselineLiveBytes",
            "peakLiveBytes",
            "finalLiveBytes",
            "peakBackingBytes",
            "arrayCopies",
            "stringCopies",
        ]
        .iter()
        .all(|name| value.counters[*name].as_u64().is_some());
        (value.methodology == "allocation-ablation-v1"
            && !value.algorithm.is_empty()
            && !value.source_hash.is_empty()
            && !value.compiler_id.is_empty()
            && !value.executable_hash.is_empty()
            && value.size >= 0
            && value.iterations <= crate::benchmark_aot::MAX_BATCH
            && matches!(
                value.control.as_str(),
                "enabled" | "no-fusion" | "no-reuse" | "no-scalar-replacement"
            )
            && value.counters["domain"] == "fai-object-storage-v1"
            && counters_valid)
            .then_some(value)
    }
}

type FootprintKey<'a> = (&'a str, i64, usize, &'a str, &'a str, &'a str);
type FootprintGroups<'a> = BTreeMap<FootprintKey<'a>, Vec<&'a FootprintSample>>;

fn footprint_groups(samples: &[FootprintSample]) -> FootprintGroups<'_> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for sample in samples {
        groups
            .entry((
                sample.algorithm.as_str(),
                sample.size,
                sample.iterations,
                sample.methodology.as_str(),
                sample.workload_version.as_str(),
                sample.comparison_id.as_str(),
            ))
            .or_default()
            .push(sample);
    }
    groups
}

fn reviewed_contract(sample: &FootprintSample) -> Option<crate::benchmark_contracts::Contract> {
    crate::benchmark_contracts::for_module(&sample.algorithm).filter(|contract| {
        (sample.methodology == crate::benchmark_contracts::METHODOLOGY
            || sample.methodology == crate::memory_workloads::METHODOLOGY)
            && contract.version == sample.workload_version
    })
}

fn paired_ratio(
    records: &[&FootprintSample],
    peer: &str,
    contract: Option<&crate::benchmark_contracts::Contract>,
) -> Result<f64, &'static str> {
    let Some(contract) = contract else { return Err("unreviewed/incompatible") };
    let unique = |side| {
        let mut rows = records.iter().copied().filter(|row| row.side == side);
        let row = rows.next().ok_or("missing peer")?;
        if rows.next().is_some() {
            return Err("incomplete/incompatible");
        }
        Ok(row)
    };
    let fai = unique("fai")?;
    let peer = unique(peer)?;
    if fai.comparison != contract.comparison.as_str()
        || peer.comparison != contract.comparison.as_str()
    {
        return Err("incomplete/incompatible");
    }
    if fai.instrumented || peer.instrumented {
        return Err("instrumented diagnostic");
    }
    if fai.samples_kib.len() < 5 || peer.samples_kib.len() < 5 {
        return Err("insufficient observations");
    }
    super::mem_ratio(fai.samples_kib.iter().copied().max(), peer.samples_kib.iter().copied().max())
        .ok_or("invalid peak")
}

pub(super) struct FootprintComparison<'a> {
    pub name: &'a str,
    pub size: i64,
    pub iterations: usize,
    pub records: Vec<&'a FootprintSample>,
    pub class: Option<crate::benchmark_contracts::Comparison>,
    pub rust: Option<f64>,
    pub ocaml: Option<f64>,
}

pub(super) fn comparisons(samples: &[FootprintSample]) -> Vec<FootprintComparison<'_>> {
    footprint_groups(samples)
        .into_iter()
        .map(|((name, size, iterations, _, _, _), records)| {
            let contract = reviewed_contract(records[0]);
            FootprintComparison {
                name,
                size,
                iterations,
                class: contract.as_ref().map(|c| c.comparison),
                rust: paired_ratio(&records, "rust", contract.as_ref()).ok(),
                ocaml: paired_ratio(&records, "ocaml", contract.as_ref()).ok(),
                records,
            }
        })
        .collect()
}

pub(super) fn render_footprints(samples: &[FootprintSample]) -> String {
    if samples.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "### Isolated process footprints\n\nPeak RSS includes images, stacks, allocator retention and GC policy. Floors are measured in the **same executable** and never subtracted. These are deployment footprints, not live-heap or compiler-win counts. Each ratio requires compatible, uninstrumented observations for that pair and at least five samples per side. Application comparisons and large floor shares retain their total-RSS ratios; the labels describe their scope.\n\n| Workload | Size | Repeats | Fai KiB | Rust KiB | OCaml KiB | Floor F/R/O KiB | Floor share F/R/O | Process F/R | Process F/O | Comparison / omitted ratios |\n|---|---:|---:|---:|---:|---:|---|---|---:|---:|---|\n",
    );
    for ((name, size, iterations, _, _, _), records) in footprint_groups(samples) {
        let mut sides = BTreeMap::new();
        for record in &records {
            sides.insert(record.side.as_str(), *record);
        }
        let peak = |side| sides.get(side).map(|row| *row.samples_kib.iter().max().unwrap());
        let floor = |side| sides.get(side).map(|row| *row.floor_kib.iter().max().unwrap());
        let contract = reviewed_contract(records[0]);
        let mut status = match contract.as_ref().map(|c| c.comparison) {
            Some(crate::benchmark_contracts::Comparison::Kernel) => "matched kernel",
            Some(crate::benchmark_contracts::Comparison::Application) => "application",
            None => "unreviewed/incompatible",
        }
        .to_owned();
        let floor_share = |side| match (floor(side), peak(side)) {
            (Some(floor), Some(peak)) if peak > 0 => {
                format!("{:.1}%", 100.0 * floor as f64 / peak as f64)
            }
            _ => "—".into(),
        };
        let rust = paired_ratio(&records, "rust", contract.as_ref());
        let ocaml = paired_ratio(&records, "ocaml", contract.as_ref());
        for (label, result) in [("F/R", rust), ("F/O", ocaml)] {
            if let Err(reason) = result {
                let _ = write!(status, "; {label}: {reason}");
            }
        }
        let _ = writeln!(
            out,
            "| {} | {size} | {iterations} | {} | {} | {} | {}/{}/{} | {}/{}/{} | {} | {} | {status} |",
            super::escape(name),
            super::kib_cell(peak("fai")),
            super::kib_cell(peak("rust")),
            super::kib_cell(peak("ocaml")),
            super::kib_cell(floor("fai")),
            super::kib_cell(floor("rust")),
            super::kib_cell(floor("ocaml")),
            floor_share("fai"),
            floor_share("rust"),
            floor_share("ocaml"),
            super::ratio_cell(rust.ok()),
            super::ratio_cell(ocaml.ok())
        );
    }
    out.push('\n');
    out
}

pub(super) fn render_reclamation(samples: &[FootprintSample]) -> String {
    let mut largest = BTreeMap::new();
    for row in samples.iter().filter(|row| row.reclamation.is_some()) {
        largest
            .entry((&row.algorithm, &row.methodology, &row.workload_version, &row.comparison_id))
            .and_modify(|n: &mut i64| *n = (*n).max(row.size))
            .or_insert(row.size);
    }
    if largest.is_empty() {
        return String::new();
    }
    let mut groups = BTreeMap::<_, BTreeMap<_, _>>::new();
    for row in samples.iter().filter(|row| {
        largest.get(&(&row.algorithm, &row.methodology, &row.workload_version, &row.comparison_id))
            == Some(&row.size)
    }) {
        if let Some(value) = &row.reclamation {
            groups
                .entry((
                    &row.algorithm,
                    row.size,
                    row.iterations,
                    &row.methodology,
                    &row.workload_version,
                    &row.comparison_id,
                ))
                .or_default()
                .insert(row.side.as_str(), value);
        }
    }
    let mut out = String::from(
        "## Post-workload resident memory\n\nLargest measured input per workload, at each repetition count. Values are maximum current RSS across fresh processes, in KiB. **Before** is the ready harness; **after** is the same process after consuming its result and releasing workload roots, while still alive. **Floor after** is the matching no-workload process. Raw before/peak/after observations remain paired in the evidence. GC and allocator policies run normally, with no forced collection or trimming; after-RSS can therefore retain unreachable objects or recycling storage.\n\n| Workload | Input | Repeats | Before F/R/O KiB | After F/R/O KiB | Floor after F/R/O KiB |\n|---|---:|---:|---|---|---|\n",
    );
    for ((name, size, iterations, _, _, _), sides) in groups {
        let column = |field: fn(&ReclamationSample) -> &[u64]| {
            ["fai", "rust", "ocaml"]
                .map(|side| {
                    sides
                        .get(side)
                        .and_then(|row| field(row).iter().max())
                        .map_or_else(|| "—".into(), u64::to_string)
                })
                .join("/")
        };
        let _ = writeln!(
            out,
            "| {} | {size} | {iterations} | {} | {} | {} |",
            super::escape(name),
            column(|r| &r.before_kib),
            column(|r| &r.after_kib),
            column(|r| &r.floor_after_kib)
        );
    }
    out.push('\n');
    out
}

pub(super) fn render_allocations(samples: &[AllocationSample]) -> String {
    if samples.is_empty() {
        return String::new();
    }
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for sample in samples {
        groups
            .entry((
                &sample.algorithm,
                sample.size,
                sample.iterations,
                &sample.source_hash,
                &sample.compiler_id,
            ))
            .or_default()
            .push(sample);
    }
    let mut out = String::from(
        "### Compiler allocation attribution\n\nPaired rows use the **same source, compiler and runtime input**, changing one compiler control. Optimized instrumented runs measure Fai object requests (including headers), peak live object bytes and allocator backing; they exclude host Rust temporaries and OS rounding. These counters are not cross-language GC-heap ratios. Timing belongs to separate uninstrumented runs.\n\n| Workload | Size | Repeats | Control | Allocations | Allocated bytes | Live bytes baseline / peak / final | Peak backing bytes | Array / string copies | Pair |\n|---|---:|---:|---|---:|---:|---|---:|---|---|\n",
    );
    for ((name, size, repeats, _, _), records) in groups {
        let paired = allocation_pair(&records);
        for row in records {
            let n = |field: &str| row.counters[field].as_u64().unwrap();
            let _ = writeln!(
                out,
                "| {} | {size} | {repeats} | {} | {} | {} | {} / {} / {} | {} | {} / {} | {} |",
                super::escape(name),
                row.control,
                n("allocations"),
                n("allocatedBytes"),
                n("baselineLiveBytes"),
                n("peakLiveBytes"),
                n("finalLiveBytes"),
                n("peakBackingBytes"),
                n("arrayCopies"),
                n("stringCopies"),
                if paired { "same-source control" } else { "unpaired" }
            );
        }
    }
    out.push('\n');
    out
}

pub(super) fn allocation_pair(records: &[&AllocationSample]) -> bool {
    let [a, b] = records else { return false };
    a.algorithm == b.algorithm
        && a.size == b.size
        && a.iterations == b.iterations
        && a.source_hash == b.source_hash
        && a.compiler_id == b.compiler_id
        && ((a.control == "enabled") != (b.control == "enabled"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn footprint(side: &str, peak: u64, floor: u64) -> FootprintSample {
        FootprintSample {
            methodology: crate::benchmark_contracts::METHODOLOGY.into(),
            algorithm: "BinaryTrees".into(),
            workload_version: crate::benchmark_contracts::for_module("BinaryTrees")
                .unwrap()
                .version
                .into(),
            comparison: "matched-kernel".into(),
            comparison_id: "same-peers".into(),
            side: side.into(),
            executable_hash: format!("{side}-binary"),
            size: 21,
            iterations: 1,
            samples_kib: vec![peak; 5],
            floor_kib: vec![floor; 5],
            instrumented: false,
            reclamation: None,
        }
    }

    #[test]
    fn small_process_footprints_keep_ratios_and_visible_floor_shares() {
        let text =
            render_footprints(&[footprint("fai", 2600, 2500), footprint("rust", 3300, 3200)]);
        assert!(text.contains("96.2%/97.0%/—"));
        assert!(text.contains("0.79×"));
        assert!(text.contains("deployment footprints"));
    }

    #[test]
    fn heap_dominated_process_ratios_retain_their_scope() {
        let text =
            render_footprints(&[footprint("fai", 50000, 2000), footprint("rust", 100000, 3000)]);
        assert!(text.contains("0.50×"));
        assert!(text.contains("matched kernel"));
    }

    #[test]
    fn an_ocaml_floor_does_not_hide_any_total_rss_ratio() {
        let text = render_footprints(&[
            footprint("fai", 40000, 3000),
            footprint("rust", 50000, 2000),
            footprint("ocaml", 30000, 4000),
        ]);
        assert!(text.contains("0.80× | 1.33×"));
        assert!(text.contains("7.5%/4.0%/13.3%"));
    }

    #[test]
    fn an_instrumented_third_peer_does_not_hide_the_clean_pair() {
        let text = render_footprints(&[
            footprint("fai", 40000, 3000),
            footprint("rust", 50000, 2000),
            FootprintSample { instrumented: true, ..footprint("ocaml", 30000, 4000) },
        ]);
        assert!(text.contains("0.80× | —"));
        assert!(text.contains("F/O: instrumented diagnostic"));
    }

    #[test]
    fn application_ratios_keep_their_comparison_class() {
        let application = |side, peak| FootprintSample {
            algorithm: "GraphBFS".into(),
            workload_version: crate::benchmark_contracts::for_module("GraphBFS")
                .unwrap()
                .version
                .into(),
            comparison: "application".into(),
            ..footprint(side, peak, 2000)
        };
        let text = render_footprints(&[application("fai", 3000), application("rust", 4000)]);
        assert!(text.contains("0.75×"));
        assert!(text.contains("application; F/O: missing peer"));
    }

    #[test]
    fn an_incomplete_third_peer_does_not_hide_the_clean_pair() {
        let text = render_footprints(&[
            footprint("fai", 40000, 3000),
            footprint("rust", 50000, 2000),
            FootprintSample {
                samples_kib: vec![30000],
                floor_kib: vec![4000],
                ..footprint("ocaml", 30000, 4000)
            },
        ]);
        assert!(text.contains("0.80× | —"));
        assert!(text.contains("F/O: insufficient observations"));
    }

    #[test]
    fn summary_ratios_keep_precision_beyond_display_rounding() {
        let samples = [footprint("fai", 1004, 1000), footprint("rust", 1000, 1000)];
        let cases = comparisons(&samples);
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].rust, Some(1.004));
        assert_eq!(cases[0].ocaml, None);
    }

    #[test]
    fn summary_excludes_only_the_instrumented_pair() {
        let samples = [
            footprint("fai", 40000, 3000),
            footprint("rust", 50000, 2000),
            FootprintSample { instrumented: true, ..footprint("ocaml", 30000, 4000) },
        ];
        let cases = comparisons(&samples);
        assert_eq!(cases[0].rust, Some(0.8));
        assert_eq!(cases[0].ocaml, None);
    }

    #[test]
    fn summary_retains_the_application_comparison_class() {
        let application = |side, peak| FootprintSample {
            algorithm: "GraphBFS".into(),
            workload_version: crate::benchmark_contracts::for_module("GraphBFS")
                .unwrap()
                .version
                .into(),
            comparison: "application".into(),
            ..footprint(side, peak, 2000)
        };
        let samples = [application("fai", 3000), application("rust", 4000)];
        let cases = comparisons(&samples);
        assert_eq!(cases[0].class, Some(crate::benchmark_contracts::Comparison::Application));
        assert_eq!(cases[0].rust, Some(0.75));
    }

    #[test]
    fn changed_workload_versions_cannot_pair() {
        let changed = FootprintSample {
            workload_version: "old-boxed-leaves".into(),
            ..footprint("rust", 100000, 3000)
        };
        let text = render_footprints(&[footprint("fai", 50000, 2000), changed]);
        assert!(!text.contains('×'));
        assert!(text.contains("missing peer") && text.contains("unreviewed/incompatible"));
    }

    #[test]
    fn instrumented_memory_is_not_a_deployed_ratio() {
        let changed = FootprintSample { instrumented: true, ..footprint("fai", 50000, 2000) };
        let text = render_footprints(&[changed, footprint("rust", 100000, 3000)]);
        assert!(!text.contains('×'));
        assert!(text.contains("instrumented diagnostic"));
    }

    #[test]
    fn raw_observations_round_trip_without_aggregation() {
        let row = FootprintSample {
            samples_kib: vec![50, 70, 60],
            floor_kib: vec![5, 4, 6],
            ..footprint("fai", 1, 1)
        };
        assert_eq!(FootprintSample::parse(&row.to_line()), Some(row));
    }

    #[test]
    fn missing_floor_samples_are_rejected() {
        let row = FootprintSample { floor_kib: Vec::new(), ..footprint("fai", 1, 1) };
        assert!(FootprintSample::parse(&row.to_line()).is_none());
    }

    fn synchronized() -> FootprintSample {
        FootprintSample {
            methodology: crate::memory_workloads::METHODOLOGY.into(),
            reclamation: Some(ReclamationSample {
                before_kib: vec![1000; 5],
                after_kib: vec![4000; 5],
                floor_before_kib: vec![1000; 5],
                floor_after_kib: vec![2000; 5],
            }),
            ..footprint("fai", 10000, 2000)
        }
    }

    #[test]
    fn synchronized_observations_round_trip_with_distinct_current_and_peak_rss() {
        let row = synchronized();
        assert!(row.to_line().starts_with("MEMSTAT3\t"));
        assert_eq!(FootprintSample::parse(&row.to_line()), Some(row));
    }

    #[test]
    fn missing_after_observations_are_rejected() {
        let mut row = synchronized();
        row.reclamation.as_mut().unwrap().after_kib.pop();
        assert!(FootprintSample::parse(&row.to_line()).is_none());
    }

    #[test]
    fn current_rss_cannot_exceed_its_own_process_peak() {
        let mut row = synchronized();
        row.reclamation.as_mut().unwrap().before_kib[2] = 10001;
        assert!(FootprintSample::parse(&row.to_line()).is_none());
    }

    #[test]
    fn historical_peaks_do_not_invent_reclamation_observations() {
        let row = footprint("fai", 10000, 2000);
        assert!(row.to_line().starts_with("MEMSTAT2\t"));
        assert_eq!(FootprintSample::parse(&row.to_line()).unwrap().reclamation, None);
        assert!(render_reclamation(&[row]).is_empty());
    }

    fn allocation() -> AllocationSample {
        AllocationSample {
            methodology: "allocation-ablation-v1".into(),
            algorithm: "Fusion".into(),
            control: "enabled".into(),
            source_hash: "source".into(),
            compiler_id: "compiler".into(),
            executable_hash: "executable".into(),
            size: 8,
            iterations: 1,
            counters: serde_json::json!({"domain": "fai-object-storage-v1", "allocations": 0, "allocatedBytes": 0,
                "baselineLiveBytes": 24, "peakLiveBytes": 24, "finalLiveBytes": 24, "peakBackingBytes": 65536,
                "arrayCopies": 0, "stringCopies": 0}),
        }
    }

    #[test]
    fn zero_is_a_valid_measured_allocation_count() {
        let row = allocation();
        assert_eq!(AllocationSample::parse(&row.to_line()), Some(row));
    }

    #[test]
    fn a_gc_heap_cannot_be_mislabeled_as_fai_live_storage() {
        let mut row = allocation();
        row.counters["domain"] = "ocaml-gc-v1".into();
        assert!(AllocationSample::parse(&row.to_line()).is_none());
    }

    #[test]
    fn differing_sources_do_not_establish_compiler_causality() {
        let changed = AllocationSample {
            control: "no-fusion".into(),
            source_hash: "changed-program".into(),
            ..allocation()
        };
        let text = render_allocations(&[allocation(), changed]);
        assert!(text.contains("unpaired"));
        assert!(!text.contains("same-source control"));
    }

    #[test]
    fn duplicate_control_observations_do_not_form_a_causal_pair() {
        let disabled = AllocationSample { control: "no-fusion".into(), ..allocation() };
        let text = render_allocations(&[allocation(), disabled.clone(), disabled]);
        assert!(text.contains("unpaired"));
        assert!(!text.contains("same-source control"));
    }
}

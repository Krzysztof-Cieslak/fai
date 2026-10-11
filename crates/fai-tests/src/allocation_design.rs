//! Same-source fixtures for attributing allocation changes to compiler passes.

use fai_db::AllocationControl;

/// A source-identical compiler experiment with an independent arithmetic answer.
pub struct Case {
    /// Stable report label.
    pub name: &'static str,
    /// Source compiled unchanged for the enabled and disabled pass.
    pub source: &'static str,
    /// Exactly one disabled optimization.
    pub control: AllocationControl,
    /// Stable name of that control in the report.
    pub control_name: &'static str,
    /// Runtime inputs, including a several-orders-of-magnitude space comparison.
    pub inputs: &'static [i64],
    /// Expected full result before the common checksum boundary.
    pub expected: fn(i64) -> i64,
}

/// Compiler-only experiments; there are no cross-language heap ratios here.
pub const CASES: &[Case] = &[
    Case {
        name: "UniqueList",
        source: include_str!("../fixtures/allocation/unique_list.fai"),
        control: AllocationControl::NoReuse,
        control_name: "no-reuse",
        inputs: &[8, 4096],
        expected: |n| 496 + 32 * n,
    },
    Case {
        name: "SharedList",
        source: include_str!("../fixtures/allocation/shared_list.fai"),
        control: AllocationControl::NoReuse,
        control_name: "no-reuse",
        inputs: &[8, 4096],
        expected: |n| 992 + 32 * n,
    },
    Case {
        name: "Fusion",
        source: include_str!("../fixtures/allocation/fusion.fai"),
        control: AllocationControl::NoFusion,
        control_name: "no-fusion",
        inputs: &[4096, 100000],
        expected: |n| n * (3 * n - 1) / 2,
    },
    Case {
        name: "Scalars",
        source: include_str!("../fixtures/allocation/scalars.fai"),
        control: AllocationControl::NoScalarReplacement,
        control_name: "no-scalar-replacement",
        inputs: &[8, 4096],
        expected: |n| 3 * n,
    },
];

/// Decodes the explicit accounting domain emitted by an instrumented Fai runtime.
pub fn counters(stderr: &[u8]) -> Result<serde_json::Value, String> {
    let text = std::str::from_utf8(stderr).map_err(|error| error.to_string())?;
    let records: Vec<_> =
        text.lines().filter_map(|line| line.strip_prefix("fai-allocation-stats: ")).collect();
    if records.len() != 1 {
        return Err(format!("expected one Fai allocation window, got {}", records.len()));
    }
    let record: serde_json::Value =
        serde_json::from_str(records[0]).map_err(|error| error.to_string())?;
    if record["domain"] != "fai-object-storage-v1" {
        return Err("wrong allocation accounting domain".into());
    }
    Ok(record)
}

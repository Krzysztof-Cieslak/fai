//! Strict, reproducible snapshots for the three durable performance trackers.
//!
//! Unlike the best-effort console summary, publication rejects missing records,
//! incompatible sources/builds, corrupt artifacts and incomplete measurements.

mod data;
#[cfg(test)]
mod tests;
mod validate;

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::Report;

/// Version of the retained snapshot and generated report contract.
pub const SCHEMA_VERSION: u32 = 2;

/// Version of the validated, presentation-independent report data.
pub const REPORT_DATA_VERSION: u32 = 1;

/// One immutable file in a snapshot. Hash algorithms are explicit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    /// Logical relative filename, before optional compression.
    pub path: String,
    /// Relative filename of the retained bytes.
    pub stored_path: String,
    /// SHA-256 of the retained file.
    pub sha256: String,
    /// BLAKE3 of its uncompressed contents, matching benchmark identities.
    pub blake3: String,
    /// Uncompressed length.
    pub bytes: u64,
    /// Whether the stored file uses gzip encoding.
    pub gzip: bool,
}

/// A completed command with its raw stdout/stderr and provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Step {
    /// Stable suite name, such as `warm` or `aot-1`.
    pub name: String,
    /// Exact command and arguments.
    pub command: Vec<String>,
    /// Process exit code. Only success can be published.
    pub exit_code: i32,
    /// Raw stdout's logical artifact path.
    pub stdout: String,
    /// Raw stderr's logical artifact path.
    pub stderr: String,
    /// Root of this suite's executable/source artifacts.
    pub artifacts: String,
    /// `timing`, `allocation` or `guards` build.
    pub build: String,
    /// UTC start and finish timestamps of this observation interval.
    pub started_at: String,
    /// UTC finish timestamp.
    pub finished_at: String,
    /// Tests actually passed, for a guard step.
    pub passed: Option<u64>,
}

/// A separately identified compiler build from the same pinned source revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Build {
    /// Build role.
    pub name: String,
    /// Full measured source revision.
    pub revision: String,
    /// Exact build commands, including the report-tool build when needed.
    pub commands: Vec<Vec<String>>,
    /// Cargo optimization level.
    pub optimization: String,
    /// Whether assertions were enabled.
    pub debug_assertions: bool,
    /// Whether Fai allocation accounting was enabled.
    pub instrumented: bool,
}

/// Measurement settings are frozen before the first observation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    /// Passes per warm comparison.
    pub warm_passes: u32,
    /// Paired run/floor observations in each pass.
    pub warm_samples: usize,
    /// Independent end-to-end passes.
    pub aot_passes: u32,
    /// Minimum end-to-end samples per side and pass.
    pub aot_samples: usize,
    /// Fresh processes per footprint side and mode.
    pub memory_samples: usize,
    /// Whole-workload repetition counts in the footprint matrix.
    pub memory_repeats: Vec<usize>,
}

/// Completed measurement snapshot; every report derives from this same manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    /// Manifest format version.
    pub schema_version: u32,
    /// Unique identifier shared by all three reports.
    pub run_id: String,
    /// `complete` only after all required commands finish successfully.
    pub status: String,
    /// Repository owner/name.
    pub repository: String,
    /// Full pinned `main` commit used for every benchmark and compiler build.
    pub measured_revision: String,
    /// Commit supplying the report generator.
    pub reporter_revision: String,
    /// UTC start timestamp.
    pub started_at: String,
    /// UTC completion timestamp.
    pub completed_at: String,
    /// Reviewed measurement methodology.
    pub methodology: String,
    /// Platform, CPU and execution environment, retained verbatim.
    pub machine: BTreeMap<String, String>,
    /// Exact compiler configurations.
    pub toolchains: BTreeMap<String, String>,
    /// Relevant allocator/compiler environment, including explicitly unset values.
    pub environment: BTreeMap<String, Option<String>>,
    /// Measurement settings.
    pub settings: Settings,
    /// Normal, instrumented and guard builds.
    pub builds: Vec<Build>,
    /// Required completed suites.
    pub steps: Vec<Step>,
    /// Checksummed raw data, sources, executables and metadata.
    pub files: Vec<Artifact>,
    /// Optional evidence-linked interpretation, versioned with the snapshot.
    #[serde(default)]
    pub notes: BTreeMap<String, Vec<String>>,
}

/// A validation or rendering failure with an actionable explanation.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(out)
    }
}
impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self(error.to_string())
    }
}

/// Validated inputs for deterministic report generation.
#[derive(Debug)]
pub struct Snapshot {
    manifest: Manifest,
    reports: BTreeMap<String, Report>,
}

impl Snapshot {
    /// Reads and validates a complete retained snapshot before rendering anything.
    pub fn load(directory: &Path) -> Result<Self, Error> {
        validate::load(directory)
    }

    /// Exports full-precision comparisons and statistics for an external renderer.
    pub fn report_data(&self) -> serde_json::Value {
        data::export(self)
    }

    /// The verified source/build identity shared by the reports.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Retains normalized observations alongside the original raw logs.
    pub fn to_json(&self) -> Result<serde_json::Value, Error> {
        let mut reports = BTreeMap::new();
        for (name, report) in &self.reports {
            reports.insert(name, serde_json::from_str::<serde_json::Value>(&report.to_json())?);
        }
        Ok(serde_json::json!({"manifest": self.manifest, "reports": reports}))
    }
}

/// Lists the current source-reviewed matrix for runners and reporting clients.
pub fn catalog() -> serde_json::Value {
    use crate::{
        algorithms, allocation_design, benchmark_contracts, memory_workloads, tail_components,
    };
    let describe = |a: &algorithms::Algorithm| {
        let c = benchmark_contracts::for_module(a.module).expect("registered contract");
        let peers = benchmark_contracts::peer_notes(a.module).expect("registered peer notes");
        serde_json::json!({"name": a.module, "small": a.jit_size, "large": a.aot_size,
            "version": c.version, "comparison": c.comparison.as_str(), "note": c.note,
            "peerNotes": {"rust": peers.rust, "ocaml": peers.ocaml},
            "validationInputs": benchmark_contracts::validation_inputs(a.module)})
    };
    serde_json::json!({"schemaVersion": SCHEMA_VERSION, "methodology": benchmark_contracts::METHODOLOGY,
        "workloads": algorithms::ALGORITHMS.iter().map(describe).collect::<Vec<_>>(),
        "components": tail_components::COMPONENTS.iter().map(describe).collect::<Vec<_>>(),
        "memoryMethodology": memory_workloads::METHODOLOGY,
        "memoryWorkloads": memory_workloads::algorithms().map(|a| {
            let profile = memory_workloads::profile(a.module).expect("memory profile");
            let contract = benchmark_contracts::for_module(a.module).expect("memory contract");
            let peers = benchmark_contracts::peer_notes(a.module).expect("registered peer notes");
            serde_json::json!({"name": a.module, "sizes": profile.sizes, "unit": profile.unit,
                "scale": profile.scale, "version": contract.version,
                "peerNotes": {"rust": peers.rust, "ocaml": peers.ocaml},
                "comparison": contract.comparison.as_str(), "note": contract.note})
        }).collect::<Vec<_>>(),
        "allocations": allocation_design::CASES.iter().map(|c| serde_json::json!({"name": c.name, "control": c.control_name, "inputs": c.inputs})).collect::<Vec<_>>()})
}

/// Computes transport and logical identities for a retained snapshot artifact.
pub fn fingerprint(path: &Path, gzip: bool) -> Result<serde_json::Value, Error> {
    let (sha256, blake3, bytes) = validate::fingerprint(path, gzip)?;
    Ok(serde_json::json!({"sha256": sha256, "blake3": blake3, "bytes": bytes, "gzip": gzip}))
}

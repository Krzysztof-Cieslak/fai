//! Snapshot integrity, coverage and measurement-domain validation.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufReader, Read};
use std::path::{Component, Path};

use sha2::{Digest, Sha256};

use super::{Artifact, Error, Manifest, SCHEMA_VERSION, Snapshot, Step};
use crate::bench_summary::{AllocationSample, FootprintSample, Report, WarmSample};
use crate::{
    algorithms, allocation_design, benchmark_aot, benchmark_contracts, memory_workloads,
    tail_components,
};

fn require(value: bool, message: impl Into<String>) -> Result<(), Error> {
    if value { Ok(()) } else { Err(Error(message.into())) }
}

fn hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len()) && value.bytes().all(|c| c.is_ascii_hexdigit())
}

fn safe_path(root: &Path, name: &str) -> Result<std::path::PathBuf, Error> {
    require(
        !name.is_empty() && Path::new(name).components().all(|c| matches!(c, Component::Normal(_))),
        format!("unsafe artifact path: {name}"),
    )?;
    let path = root.join(name).canonicalize()?;
    require(path.starts_with(root), format!("artifact escapes snapshot: {name}"))?;
    Ok(path)
}

pub(super) fn fingerprint(path: &Path, gzip: bool) -> Result<(String, String, u64), Error> {
    let mut raw = BufReader::new(std::fs::File::open(path)?);
    let mut sha = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = raw.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        sha.update(&buffer[..n]);
    }
    let raw = BufReader::new(std::fs::File::open(path)?);
    let mut reader: Box<dyn Read> =
        if gzip { Box::new(flate2::read::GzDecoder::new(raw)) } else { Box::new(raw) };
    let mut hasher = blake3::Hasher::new();
    let mut count = 0u64;
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        require(count <= 1024 * 1024 * 1024, "artifact exceeds one GiB")?;
        hasher.update(&buffer[..n]);
    }
    Ok((format!("{:x}", sha.finalize()), hasher.finalize().to_hex().to_string(), count))
}

struct Files<'a> {
    root: &'a Path,
    index: BTreeMap<&'a str, &'a Artifact>,
}

impl<'a> Files<'a> {
    fn new(root: &'a Path, artifacts: &'a [Artifact]) -> Result<Self, Error> {
        let mut index = BTreeMap::new();
        let mut stored = BTreeSet::new();
        for artifact in artifacts {
            require(
                index.insert(artifact.path.as_str(), artifact).is_none(),
                format!("duplicate artifact: {}", artifact.path),
            )?;
            require(stored.insert(&artifact.stored_path), "duplicate stored artifact path")?;
            require(
                hex(&artifact.sha256, &[64]) && hex(&artifact.blake3, &[64]),
                "invalid artifact digest",
            )?;
            require(artifact.bytes <= 1024 * 1024 * 1024, "artifact exceeds one GiB")?;
            require(
                Path::new(&artifact.path).components().all(|c| matches!(c, Component::Normal(_))),
                "unsafe logical artifact path",
            )?;
            let path = safe_path(root, &artifact.stored_path)?;
            let (sha, content, count) = fingerprint(&path, artifact.gzip)?;
            require(sha == artifact.sha256, format!("corrupt stored artifact: {}", artifact.path))?;
            require(
                count == artifact.bytes && content == artifact.blake3,
                format!("corrupt logical artifact: {}", artifact.path),
            )?;
        }
        Ok(Self { root, index })
    }

    fn artifact(&self, path: &str) -> Result<&Artifact, Error> {
        self.index.get(path).copied().ok_or_else(|| Error(format!("missing artifact: {path}")))
    }

    fn text(&self, path: &str) -> Result<String, Error> {
        let artifact = self.artifact(path)?;
        require(artifact.bytes <= 16 * 1024 * 1024, "text artifact exceeds 16 MiB")?;
        let file = std::fs::File::open(safe_path(self.root, &artifact.stored_path)?)?;
        let mut text = String::new();
        if artifact.gzip {
            flate2::read::GzDecoder::new(file).read_to_string(&mut text)?;
        } else {
            BufReader::new(file).read_to_string(&mut text)?;
        }
        Ok(text)
    }

    fn json(&self, path: &str) -> Result<serde_json::Value, Error> {
        Ok(serde_json::from_str(&self.text(path)?)?)
    }

    fn source(&self, path: &str, expected: &str) -> Result<(), Error> {
        require(
            self.artifact(path)?.blake3 == blake3::hash(expected.as_bytes()).to_hex().as_str(),
            format!("source mismatch: {path}"),
        )
    }

    fn peers(&self, root: &str, module: &str, mode: &str) -> Result<String, Error> {
        let mut hashes = Vec::new();
        for side in ["fai", "rust", "ocaml"] {
            hashes.push(self.artifact(&format!("{root}/{module}/{side}-{mode}"))?.blake3.as_str());
        }
        Ok(blake3::hash(hashes.join(":").as_bytes()).to_hex().to_string())
    }
}

fn strict_report(text: &str) -> Result<Report, Error> {
    for (i, line) in text.lines().enumerate() {
        let valid = if line.starts_with("WARMSTAT2") {
            WarmSample::parse(line).is_some()
        } else if line.starts_with("MEMSTAT2") || line.starts_with("MEMSTAT3") {
            FootprintSample::parse(line).is_some()
        } else if line.starts_with("ALLOCSTAT") {
            AllocationSample::parse(line).is_some()
        } else {
            !line.starts_with("WARMSTAT\t") && !line.starts_with("MEMSTAT\t")
        };
        require(valid, format!("malformed or legacy record on line {}", i + 1))?;
    }
    Ok(Report::parse(text))
}

fn compiler_id(info: &serde_json::Value) -> Result<&str, Error> {
    require(info["debugAssertions"] == false, "benchmark compiler has debug assertions")?;
    require(
        info["sourceId"].as_str().is_some_and(|value| hex(value, &[64])),
        "invalid compiler source identity",
    )?;
    info["toolBuildId"]
        .as_str()
        .filter(|s| hex(s, &[64]))
        .ok_or_else(|| Error("invalid compiler build identity".into()))
}

fn validate_rust_peer(
    manifest: &Manifest,
    algorithm: &algorithms::Algorithm,
    info: &serde_json::Value,
    executable: &Artifact,
    source: &Artifact,
    methodology: &str,
) -> Result<(), Error> {
    let contract = benchmark_contracts::for_module(algorithm.module).expect("reviewed workload");
    require(
        info["methodology"] == methodology
            && info["workload"] == algorithm.module
            && info["workloadVersion"] == contract.version
            && info["executableHash"] == executable.blake3
            && info["sourceHash"] == source.blake3,
        "Rust peer identity mismatch",
    )?;
    let version = info["compiler"].as_str().unwrap_or_default().trim();
    let library = &info["libraryBuild"];
    require(
        version == manifest.toolchains["rustDetails"]
            && library["compiler"].as_str().unwrap_or_default().trim() == version
            && library["artifact"]["profile"]["opt_level"] == "3"
            && library["artifact"]["profile"]["debug_assertions"] == false
            && library["artifact"]["profile"]["overflow_checks"] == false,
        "Rust peer compiler/profile mismatch",
    )
}

fn validate_ocaml_peer(manifest: &Manifest, info: &str) -> Result<(), Error> {
    require(
        info.lines().any(|line| line == "flags: -O3")
            && info.trim_end().ends_with(&manifest.toolchains["ocamlConfiguration"]),
        "OCaml peer compiler/profile mismatch",
    )
}

fn validate_peer_sources(
    manifest: &Manifest,
    files: &Files<'_>,
    root: &str,
    algorithm: &algorithms::Algorithm,
    entry: benchmark_aot::Entry,
    mode: &str,
) -> Result<(), Error> {
    let folder = format!("{root}/{}", algorithm.module);
    let rust_source = format!("{folder}/rust-{mode}.rs");
    files.source(&rust_source, &crate::native_peer::rust_source(algorithm, entry))?;
    files
        .source(&format!("{folder}/{mode}.ml"), &crate::ocaml::isolated_source(algorithm, entry))?;
    validate_rust_peer(
        manifest,
        algorithm,
        &files.json(&format!("{folder}/rust-compiler-info.json"))?,
        files.artifact(&format!("{folder}/rust-{mode}"))?,
        files.artifact(&rust_source)?,
        if entry == benchmark_aot::Entry::Footprint {
            memory_workloads::METHODOLOGY
        } else {
            &manifest.methodology
        },
    )?;
    validate_ocaml_peer(manifest, &files.text(&format!("{folder}/ocaml-compiler-info.txt"))?)
}

fn validate_warm(
    manifest: &Manifest,
    step: &Step,
    report: &Report,
    files: &Files<'_>,
    components: bool,
) -> Result<String, Error> {
    let algorithms = if components { tail_components::COMPONENTS } else { algorithms::ALGORITHMS };
    let metadata = files.json(&format!("{}/metadata.json", step.artifacts))?;
    require(
        metadata["methodology"] == manifest.methodology
            && metadata["gitRevision"] == manifest.measured_revision
            && metadata["debugAssertions"] == false
            && metadata["inputWindow"] == 32
            && metadata["passes"] == manifest.settings.warm_passes
            && metadata["samplesPerPass"] == manifest.settings.warm_samples,
        "warm build metadata does not match the snapshot",
    )?;
    let compiler = compiler_id(&metadata["faiCompiler"])?;
    let mut expected = BTreeSet::new();
    let mut identities = BTreeMap::new();
    for algorithm in algorithms {
        validate_peer_sources(
            manifest,
            files,
            &step.artifacts,
            algorithm,
            benchmark_aot::Entry::Worker,
            "worker",
        )?;
        files.source(
            &format!("{}/{}/worker.fai", step.artifacts, algorithm.module),
            &benchmark_aot::fai_program(algorithm, benchmark_aot::Entry::Worker),
        )?;
        identities
            .insert(algorithm.module, files.peers(&step.artifacts, algorithm.module, "worker")?);
        let preflight =
            files.json(&format!("{}/{}/validation.json", step.artifacts, algorithm.module))?;
        let inputs = benchmark_contracts::validation_inputs(algorithm.module);
        require(
            preflight["schemaVersion"] == 1 && preflight["inputs"] == serde_json::json!(inputs),
            "warm validation inputs mismatch",
        )?;
        let answers: Vec<_> = inputs
            .iter()
            .map(|&n| crate::benchmark_process::ExpectedAnswer::at_size(algorithm, n))
            .collect();
        for side in ["fai", "rust", "ocaml"] {
            let peer = &preflight["peers"][side];
            require(
                peer["executableHash"]
                    == files
                        .artifact(&format!(
                            "{}/{}/{side}-worker",
                            step.artifacts, algorithm.module
                        ))?
                        .blake3,
                "warm validation executable mismatch",
            )?;
            let actual = peer["answers"]
                .as_array()
                .ok_or_else(|| Error("missing warm validation answers".into()))?;
            require(actual.len() == answers.len(), "incomplete warm validation answers")?;
            for (expected, actual) in answers.iter().zip(actual) {
                expected
                    .verify_line("warm input validation", actual.as_str().unwrap_or_default())
                    .map_err(|error| Error(error.to_string()))?;
            }
        }
        for size in BTreeSet::from([algorithm.jit_size, algorithm.aot_size]) {
            for side in ["fai", "rust", "ocaml"] {
                for pass in 1..=manifest.settings.warm_passes {
                    expected.insert((algorithm.module.to_owned(), size, side.to_owned(), pass));
                }
            }
        }
    }
    let mut observed = BTreeSet::new();
    for sample in &report.warm {
        let contract = benchmark_contracts::for_module(&sample.algorithm)
            .ok_or_else(|| Error("unknown warm workload".into()))?;
        require(
            sample.methodology == manifest.methodology
                && sample.workload_version == contract.version
                && sample.comparison == contract.comparison.as_str(),
            "warm contract mismatch",
        )?;
        require(
            identities.get(sample.algorithm.as_str()) == Some(&sample.comparison_id),
            "warm executable-set mismatch",
        )?;
        require(
            sample.passes == manifest.settings.warm_passes
                && sample.samples_ns.len() == manifest.settings.warm_samples
                && sample.floor_ns.len() == manifest.settings.warm_samples,
            "incomplete warm observations",
        )?;
        require(
            observed.insert((
                sample.algorithm.clone(),
                sample.size,
                sample.side.clone(),
                sample.pass,
            )),
            "duplicate warm pass",
        )?;
    }
    require(
        observed == expected,
        format!(
            "incomplete {} matrix: expected {}, observed {}",
            step.name,
            expected.len(),
            observed.len()
        ),
    )?;
    require(
        super::super::warm::summarize(&report.warm).iter().all(|case| case.complete),
        "incompatible warm pass batches",
    )?;
    Ok(compiler.to_owned())
}

fn validate_memory(
    manifest: &Manifest,
    step: &Step,
    report: &Report,
    files: &Files<'_>,
    compiler: &str,
) -> Result<(), Error> {
    let mut expected = BTreeSet::new();
    let mut identities = BTreeMap::new();
    for algorithm in memory_workloads::algorithms() {
        let name = algorithm.module;
        validate_peer_sources(
            manifest,
            files,
            &step.artifacts,
            algorithm,
            benchmark_aot::Entry::Footprint,
            "memory",
        )?;
        files.source(
            &format!("{}/{name}/memory.fai", step.artifacts),
            &benchmark_aot::fai_program(algorithm, benchmark_aot::Entry::Footprint),
        )?;
        let info = files.json(&format!("{}/{name}/identity.json", step.artifacts))?;
        let profile = memory_workloads::profile(name).expect("memory profile");
        require(
            info["methodology"] == memory_workloads::METHODOLOGY
                && info["memoryProfile"]
                    == serde_json::json!({"unit": profile.unit, "scale": profile.scale, "sizes": profile.sizes}),
            "memory profile mismatch",
        )?;
        require(compiler_id(&info["faiCompiler"])? == compiler, "mixed memory compiler builds")?;
        identities.insert(name, files.peers(&step.artifacts, name, "memory")?);
        for &size in profile.sizes {
            for repeat in &manifest.settings.memory_repeats {
                for side in ["fai", "rust", "ocaml"] {
                    expected.insert((name.to_owned(), size, *repeat, side.to_owned()));
                }
            }
        }
    }
    let mut observed = BTreeSet::new();
    for row in &report.footprints {
        let contract = benchmark_contracts::for_module(&row.algorithm)
            .ok_or_else(|| Error("unknown memory workload".into()))?;
        require(
            row.methodology == memory_workloads::METHODOLOGY
                && row.workload_version == contract.version
                && row.comparison == contract.comparison.as_str(),
            "memory contract mismatch",
        )?;
        require(!row.instrumented, "instrumented binary in process comparison")?;
        require(row.reclamation.is_some(), "missing before/after memory observations")?;
        require(
            identities.get(row.algorithm.as_str()) == Some(&row.comparison_id),
            "memory executable-set mismatch",
        )?;
        require(
            files
                .artifact(&format!("{}/{}/{}-memory", step.artifacts, row.algorithm, row.side))?
                .blake3
                == row.executable_hash,
            "memory executable hash mismatch",
        )?;
        require(
            row.samples_kib.len() == manifest.settings.memory_samples
                && row.floor_kib.len() == manifest.settings.memory_samples,
            "incomplete memory observations",
        )?;
        require(
            observed.insert((row.algorithm.clone(), row.size, row.iterations, row.side.clone())),
            "duplicate memory case",
        )?;
    }
    require(observed == expected, "incomplete process-footprint matrix")
}

fn validate_allocations(
    step: &Step,
    report: &Report,
    files: &Files<'_>,
    source: &serde_json::Value,
) -> Result<(), Error> {
    let metadata = files.json(&format!("{}/compiler-info.json", step.artifacts))?;
    require(metadata["allocationStats"] == true, "allocation accounting is not enabled")?;
    require(
        metadata["compiler"]["sourceId"] == *source,
        "timing and allocation compilers have different sources",
    )?;
    let compiler = compiler_id(&metadata["compiler"])?;
    let mut expected = BTreeSet::new();
    for case in allocation_design::CASES {
        files.source(
            &format!("{}/{}/workload.fai", step.artifacts, case.name),
            &benchmark_aot::fai_program_from(
                case.source,
                "run",
                false,
                benchmark_aot::Entry::Memory,
            ),
        )?;
        for &input in case.inputs {
            for control in ["enabled", case.control_name] {
                expected.insert((case.name.to_owned(), input, control.to_owned()));
            }
        }
    }
    let mut observed = BTreeSet::new();
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for row in &report.allocations {
        require(
            row.compiler_id == compiler && row.iterations == 1,
            "allocation build/input mismatch",
        )?;
        require(
            files.artifact(&format!("{}/{}/workload.fai", step.artifacts, row.algorithm))?.blake3
                == row.source_hash,
            "allocation source hash mismatch",
        )?;
        require(
            files
                .artifact(&format!("{}/{}/{}", step.artifacts, row.algorithm, row.control))?
                .blake3
                == row.executable_hash,
            "allocation executable mismatch",
        )?;
        for key in [
            "baselineLiveObjects",
            "peakLiveObjects",
            "finalLiveObjects",
            "retainedSlabBytes",
            "retainedMappedBytes",
            "retainedSystemBytes",
        ] {
            require(
                row.counters[key].as_u64().is_some(),
                format!("missing allocation counter: {key}"),
            )?;
        }
        require(
            row.counters["finalLiveBytes"] == row.counters["baselineLiveBytes"]
                && row.counters["finalLiveObjects"] == row.counters["baselineLiveObjects"],
            "allocation window retains workload values",
        )?;
        require(
            row.counters["peakLiveBytes"].as_u64() >= row.counters["baselineLiveBytes"].as_u64(),
            "invalid live-byte baseline",
        )?;
        let retained = ["retainedSlabBytes", "retainedMappedBytes", "retainedSystemBytes"]
            .iter()
            .try_fold(0u64, |total, key| total.checked_add(row.counters[*key].as_u64()?));
        require(
            retained.is_some_and(|bytes| {
                bytes <= row.counters["peakBackingBytes"].as_u64().unwrap_or(0)
            }),
            "retained backing exceeds its peak",
        )?;
        require(
            observed.insert((row.algorithm.clone(), row.size, row.control.clone())),
            "duplicate allocation observation",
        )?;
        groups.entry((&row.algorithm, row.size)).or_default().push(row);
    }
    require(observed == expected, "incomplete allocation-control matrix")?;
    require(
        groups.values().all(|rows| super::super::memory::allocation_pair(rows)),
        "incompatible allocation pair",
    )
}

pub(super) fn normalized(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(char::to_lowercase).collect()
}

fn validate_aot(
    manifest: &Manifest,
    step: &Step,
    report: &Report,
    files: &Files<'_>,
    compiler: &str,
) -> Result<(), Error> {
    let names: BTreeMap<_, _> =
        algorithms::ALGORITHMS.iter().map(|a| (normalized(a.module), a)).collect();
    let mut observed = BTreeSet::new();
    for row in &report.rows {
        let algorithm = names
            .get(&normalized(&row.bench))
            .ok_or_else(|| Error(format!("unknown end-to-end workload: {}", row.bench)))?;
        require(
            row.group == "algorithms_aot" && super::super::is_side(&row.case),
            "unexpected end-to-end row",
        )?;
        require(
            observed.insert((algorithm.module, row.case.as_str())),
            "duplicate end-to-end row",
        )?;
        let samples = row
            .samples
            .replace(',', "")
            .parse::<usize>()
            .map_err(|_| Error("invalid end-to-end sample count".into()))?;
        require(samples >= manifest.settings.aot_samples, "insufficient end-to-end observations")?;
        require(
            super::super::parse_duration(&row.median).is_some_and(|n| n.is_finite() && n > 0.0),
            "invalid end-to-end duration",
        )?;
        let info = files
            .json(&format!("{}/{}/{}-once.json", step.artifacts, algorithm.module, row.case))?;
        let contract =
            benchmark_contracts::for_module(algorithm.module).expect("registered contract");
        require(
            info["methodology"] == manifest.methodology
                && info["workloadVersion"] == contract.version
                && info["side"] == row.case,
            "end-to-end metadata mismatch",
        )?;
        let artifact = files
            .artifact(&format!("{}/{}/{}-once", step.artifacts, algorithm.module, row.case))?;
        require(info["executableHash"] == artifact.blake3, "end-to-end executable mismatch")?;
        if row.case == "fai" {
            require(
                compiler_id(&info["details"]["compiler"])? == compiler,
                "mixed end-to-end compiler builds",
            )?;
            files.source(
                &format!("{}/{}/workload.fai", step.artifacts, algorithm.module),
                &benchmark_aot::fai_program(algorithm, benchmark_aot::Entry::Once),
            )?;
        } else if row.case == "rust" {
            let source = format!("{}/{}/workload.rs", step.artifacts, algorithm.module);
            files.source(
                &source,
                &crate::native_peer::rust_source(algorithm, benchmark_aot::Entry::Once),
            )?;
            validate_rust_peer(
                manifest,
                algorithm,
                &info["details"],
                artifact,
                files.artifact(&source)?,
                &manifest.methodology,
            )?;
        } else {
            files.source(
                &format!("{}/{}/workload.ml", step.artifacts, algorithm.module),
                &crate::ocaml::isolated_source(algorithm, benchmark_aot::Entry::Once),
            )?;
            validate_ocaml_peer(
                manifest,
                info["details"]["compilerInfo"].as_str().unwrap_or_default(),
            )?;
        }
    }
    require(observed.len() == algorithms::ALGORITHMS.len() * 3, "incomplete end-to-end matrix")
}

pub(super) fn load(directory: &Path) -> Result<Snapshot, Error> {
    let root = directory.canonicalize()?;
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(root.join("snapshot.json"))?)?;
    require(
        manifest.schema_version == SCHEMA_VERSION && manifest.status == "complete",
        "snapshot is incomplete or has an unsupported schema",
    )?;
    require(
        hex(&manifest.measured_revision, &[40, 64]) && hex(&manifest.reporter_revision, &[40, 64]),
        "invalid source revision",
    )?;
    require(
        manifest.reporter_revision == manifest.measured_revision,
        "reporter must come from the measured revision",
    )?;
    require(
        !manifest.run_id.is_empty()
            && !manifest.started_at.is_empty()
            && !manifest.completed_at.is_empty(),
        "missing snapshot identity/time",
    )?;
    require(
        manifest.repository.split('/').count() == 2
            && manifest.repository.chars().all(|c| c.is_ascii_alphanumeric() || "-_/".contains(c)),
        "invalid repository",
    )?;
    require(
        manifest.methodology == benchmark_contracts::METHODOLOGY,
        "unsupported measurement methodology",
    )?;
    for key in ["system", "machine", "kernel", "cpu", "benchmarkCpu"] {
        require(
            manifest.machine.get(key).is_some_and(|value| !value.is_empty()),
            format!("missing host metadata: {key}"),
        )?;
    }
    for key in
        ["rustVersion", "rustDetails", "ocamlVersion", "ocamlConfiguration", "ocamlExecutable"]
    {
        require(
            manifest.toolchains.get(key).is_some_and(|value| !value.is_empty()),
            format!("missing compiler metadata: {key}"),
        )?;
    }
    require(
        manifest.toolchains["ocamlConfiguration"].lines().any(|line| line == "flambda: true")
            && manifest.toolchains["ocamlConfiguration"]
                .lines()
                .any(|line| line == format!("version: {}", manifest.toolchains["ocamlVersion"])),
        "OCaml version or Flambda configuration is inconsistent",
    )?;
    let settings = &manifest.settings;
    require(
        (3..=16).contains(&settings.warm_passes)
            && (21..=1000).contains(&settings.warm_samples)
            && settings.aot_passes == 3
            && settings.aot_samples >= 100
            && settings.memory_samples >= 5
            && settings.memory_repeats == [1, 16],
        "snapshot does not meet the complete-checkpoint settings",
    )?;
    let builds: BTreeMap<_, _> = manifest.builds.iter().map(|b| (b.name.as_str(), b)).collect();
    require(
        builds.len() == 3 && builds.len() == manifest.builds.len(),
        "missing or duplicate build roles",
    )?;
    for name in ["timing", "allocation", "guards"] {
        let build = builds.get(name).ok_or_else(|| Error(format!("missing build: {name}")))?;
        require(
            build.revision == manifest.measured_revision
                && !build.commands.is_empty()
                && build.commands.iter().all(|command| !command.is_empty()),
            "mixed source revisions or missing build command",
        )?;
        require(
            build.debug_assertions == (name == "guards")
                && build.instrumented == (name != "timing"),
            "incompatible instrumentation/assertion profile",
        )?;
        if name != "guards" {
            require(build.optimization == "3", "benchmark is not release optimized")?;
        }
    }
    let files = Files::new(&root, &manifest.files)?;
    require(
        files.json("catalog.json")? == super::catalog(),
        "reporter and measured workload catalogs differ",
    )?;
    let mut reports = BTreeMap::new();
    let mut steps = BTreeMap::new();
    for step in &manifest.steps {
        require(
            step.exit_code == 0 && !step.command.is_empty(),
            format!("unsuccessful step: {}", step.name),
        )?;
        require(steps.insert(step.name.as_str(), step).is_none(), "duplicate step")?;
        files.artifact(&step.stderr)?;
        let text = files.text(&step.stdout)?;
        reports.insert(step.name.clone(), strict_report(&text)?);
    }
    let required = [
        "warm",
        "components",
        "aot-1",
        "aot-2",
        "aot-3",
        "memory",
        "allocations",
        "perceus_space",
        "perceus_storage",
    ];
    require(
        steps.len() == required.len() && required.iter().all(|name| steps.contains_key(name)),
        "incomplete checkpoint suites",
    )?;
    for name in required {
        let step = steps[name];
        let role = if name == "allocations" {
            "allocation"
        } else if name.starts_with("perceus_") {
            "guards"
        } else {
            "timing"
        };
        require(step.build == role, format!("wrong build for {name}"))?;
    }
    let compiler = validate_warm(&manifest, steps["warm"], &reports["warm"], &files, false)?;
    require(
        validate_warm(&manifest, steps["components"], &reports["components"], &files, true)?
            == compiler,
        "mixed component compiler builds",
    )?;
    validate_memory(&manifest, steps["memory"], &reports["memory"], &files, &compiler)?;
    let warm_metadata = files.json(&format!("{}/metadata.json", steps["warm"].artifacts))?;
    validate_allocations(
        steps["allocations"],
        &reports["allocations"],
        &files,
        &warm_metadata["faiCompiler"]["sourceId"],
    )?;
    for name in ["aot-1", "aot-2", "aot-3"] {
        validate_aot(&manifest, steps[name], &reports[name], &files, &compiler)?;
    }
    for name in ["perceus_space", "perceus_storage"] {
        require(steps[name].passed.is_some_and(|n| n > 0), format!("no executed tests in {name}"))?;
        let text = files.text(&steps[name].stdout)?;
        require(
            text.contains(&format!(
                "test result: ok. {} passed; 0 failed;",
                steps[name].passed.unwrap_or(0)
            )) && text.contains("0 ignored; 0 measured; 0 filtered out"),
            "guard result does not match its log",
        )?;
    }
    Ok(Snapshot { manifest, reports })
}

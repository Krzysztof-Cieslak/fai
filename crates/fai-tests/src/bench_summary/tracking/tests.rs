//! Complete synthetic snapshots and focused publication-contract regressions.

use std::collections::BTreeMap;
use std::io::Write as _;

use sha2::{Digest, Sha256};

use super::*;
use crate::bench_summary::{AllocationSample, FootprintSample, WarmSample};
use crate::{
    algorithms, allocation_design, benchmark_aot as aot, benchmark_contracts as contracts,
    tail_components,
};

struct Fixture {
    directory: tempfile::TempDir,
    manifest: Manifest,
}

impl Fixture {
    fn file(&mut self, name: &str, bytes: &[u8]) -> String {
        let path = self.directory.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        let hash = blake3::hash(bytes).to_hex().to_string();
        self.manifest.files.retain(|file| file.path != name);
        self.manifest.files.push(Artifact {
            path: name.into(),
            stored_path: name.into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            blake3: hash.clone(),
            bytes: bytes.len() as u64,
            gzip: false,
        });
        hash
    }

    fn json(&mut self, name: &str, value: serde_json::Value) {
        self.file(name, &serde_json::to_vec(&value).unwrap());
    }

    fn step(&mut self, name: &str, build: &str, text: &str) {
        let stdout = format!("raw/{name}.stdout");
        let stderr = format!("raw/{name}.stderr");
        self.file(&stdout, text.as_bytes());
        self.file(&stderr, b"");
        self.manifest.steps.push(Step {
            name: name.into(),
            command: vec![name.into()],
            exit_code: 0,
            stdout,
            stderr,
            artifacts: format!("artifacts/{name}"),
            build: build.into(),
            started_at: "2026-10-12T00:00:00Z".into(),
            finished_at: "2026-10-12T00:00:01Z".into(),
            passed: (build == "guards").then_some(2),
        });
    }

    fn compiler() -> serde_json::Value {
        serde_json::json!({"toolBuildId": "c".repeat(64), "sourceId": "d".repeat(64), "debugAssertions": false})
    }

    fn peers(&mut self, root: &str, module: &str, mode: &str) -> (String, Vec<String>) {
        let hashes = ["fai", "rust", "ocaml"]
            .map(|side| {
                self.file(
                    &format!("{root}/{module}/{side}-{mode}"),
                    format!("{root}:{module}:{side}:{mode}").as_bytes(),
                )
            })
            .to_vec();
        let algorithm =
            algorithms::by_module(module).or_else(|| tail_components::by_module(module)).unwrap();
        let entry = match mode {
            "once" => aot::Entry::Once,
            "worker" => aot::Entry::Worker,
            _ => aot::Entry::Memory,
        };
        let source = self.file(
            &format!(
                "{root}/{module}/{}",
                if mode == "once" { "workload.rs".into() } else { format!("rust-{mode}.rs") }
            ),
            crate::native_peer::rust_source(algorithm, entry).as_bytes(),
        );
        self.file(
            &format!(
                "{root}/{module}/{}",
                if mode == "once" { "workload.ml".into() } else { format!("{mode}.ml") }
            ),
            crate::ocaml::isolated_source(algorithm, entry).as_bytes(),
        );
        let contract = contracts::for_module(module).unwrap();
        self.json(&format!("{root}/{module}/rust-compiler-info.json"), serde_json::json!({
            "methodology": contracts::METHODOLOGY, "workload": module, "workloadVersion": contract.version,
            "sourceHash": source, "executableHash": hashes[1], "compiler": "fixture",
            "libraryBuild": {"compiler": "fixture", "artifact": {"profile": {"opt_level": "3", "debug_assertions": false, "overflow_checks": false}}}
        }));
        self.file(
            &format!("{root}/{module}/ocaml-compiler-info.txt"),
            b"flags: -O3\nversion: fixture\nflambda: true\n",
        );
        (blake3::hash(hashes.join(":").as_bytes()).to_hex().to_string(), hashes)
    }

    fn new() -> Self {
        let revision = "a".repeat(40);
        let mut fixture = Self {
            directory: tempfile::tempdir().unwrap(),
            manifest: Manifest {
                schema_version: 1,
                run_id: "fixture-checkpoint".into(),
                status: "complete".into(),
                repository: "example/fai".into(),
                measured_revision: revision.clone(),
                reporter_revision: revision.clone(),
                started_at: "2026-10-12T00:00:00Z".into(),
                completed_at: "2026-10-12T00:00:01Z".into(),
                methodology: contracts::METHODOLOGY.into(),
                machine: ["system", "machine", "kernel", "cpu", "benchmarkCpu"]
                    .map(|key| (key.into(), "fixture".into()))
                    .into(),
                toolchains: [
                    "rustVersion",
                    "rustDetails",
                    "ocamlVersion",
                    "ocamlConfiguration",
                    "ocamlExecutable",
                ]
                .map(|key| (key.into(), "fixture".into()))
                .into(),
                environment: BTreeMap::new(),
                settings: Settings {
                    warm_passes: 3,
                    warm_samples: 21,
                    aot_passes: 3,
                    aot_samples: 100,
                    memory_samples: 5,
                    memory_repeats: vec![1, 16],
                },
                builds: ["timing", "allocation", "guards"]
                    .map(|name| Build {
                        name: name.into(),
                        revision: revision.clone(),
                        commands: vec![vec!["cargo".into()]],
                        optimization: if name == "guards" { "0" } else { "3" }.into(),
                        debug_assertions: name == "guards",
                        instrumented: name != "timing",
                    })
                    .to_vec(),
                steps: Vec::new(),
                files: Vec::new(),
                notes: BTreeMap::new(),
            },
        };
        fixture
            .manifest
            .toolchains
            .insert("ocamlConfiguration".into(), "version: fixture\nflambda: true".into());
        fixture.json("catalog.json", catalog());
        for (name, algorithms) in
            [("warm", algorithms::ALGORITHMS), ("components", tail_components::COMPONENTS)]
        {
            let root = format!("artifacts/{name}");
            fixture.json(&format!("{root}/metadata.json"), serde_json::json!({"methodology": contracts::METHODOLOGY, "gitRevision": revision, "debugAssertions": false, "inputWindow": 32, "passes": 3, "samplesPerPass": 21, "faiCompiler": Self::compiler()}));
            let mut text = String::new();
            for algorithm in algorithms {
                let contract = contracts::for_module(algorithm.module).unwrap();
                let (identity, _) = fixture.peers(&root, algorithm.module, "worker");
                fixture.file(
                    &format!("{root}/{}/worker.fai", algorithm.module),
                    aot::fai_program(algorithm, aot::Entry::Worker).as_bytes(),
                );
                for size in [algorithm.jit_size, algorithm.aot_size] {
                    for (side, duration) in [("fai", 10000), ("rust", 20000), ("ocaml", 25000)] {
                        for pass in 1..=3 {
                            let row = WarmSample {
                                methodology: contracts::METHODOLOGY.into(),
                                workload_version: contract.version.into(),
                                comparison: contract.comparison.as_str().into(),
                                comparison_id: identity.clone(),
                                algorithm: algorithm.module.into(),
                                size,
                                side: side.into(),
                                batch: 1,
                                pass,
                                passes: 3,
                                samples_ns: vec![duration; 21],
                                floor_ns: vec![1; 21],
                            };
                            text.push_str(&row.to_line());
                            text.push('\n');
                        }
                    }
                }
            }
            fixture.step(name, "timing", &text);
        }
        let mut memory = String::new();
        for algorithm in algorithms::ALGORITHMS {
            let contract = contracts::for_module(algorithm.module).unwrap();
            let root = "artifacts/memory";
            let (identity, hashes) = fixture.peers(root, algorithm.module, "memory");
            fixture.file(
                &format!("{root}/{}/memory.fai", algorithm.module),
                aot::fai_program(algorithm, aot::Entry::Memory).as_bytes(),
            );
            fixture.json(
                &format!("{root}/{}/identity.json", algorithm.module),
                serde_json::json!({"faiCompiler": Self::compiler()}),
            );
            for size in [algorithm.jit_size, algorithm.aot_size] {
                for iterations in [1, 16] {
                    for (index, side) in ["fai", "rust", "ocaml"].into_iter().enumerate() {
                        let row = FootprintSample {
                            methodology: contracts::METHODOLOGY.into(),
                            algorithm: algorithm.module.into(),
                            workload_version: contract.version.into(),
                            comparison: contract.comparison.as_str().into(),
                            comparison_id: identity.clone(),
                            side: side.into(),
                            executable_hash: hashes[index].clone(),
                            size,
                            iterations,
                            samples_kib: vec![50000; 5],
                            floor_kib: vec![1000; 5],
                            instrumented: false,
                        };
                        memory.push_str(&row.to_line());
                        memory.push('\n');
                    }
                }
            }
        }
        fixture.step("memory", "timing", &memory);
        fixture.json(
            "artifacts/allocations/compiler-info.json",
            serde_json::json!({"compiler": Self::compiler(), "allocationStats": true}),
        );
        let mut allocation = String::new();
        for case in allocation_design::CASES {
            let source = fixture.file(
                &format!("artifacts/allocations/{}/workload.fai", case.name),
                aot::fai_program_from(case.source, "run", false, aot::Entry::Memory).as_bytes(),
            );
            for control in ["enabled", case.control_name] {
                let executable = fixture.file(
                    &format!("artifacts/allocations/{}/{control}", case.name),
                    format!("{}:{control}", case.name).as_bytes(),
                );
                for &size in case.inputs {
                    let count = if control == "enabled" { 0 } else { 1000 };
                    let row = AllocationSample {
                        methodology: "allocation-ablation-v1".into(),
                        algorithm: case.name.into(),
                        control: control.into(),
                        source_hash: source.clone(),
                        compiler_id: "c".repeat(64),
                        executable_hash: executable.clone(),
                        size,
                        iterations: 1,
                        counters: serde_json::json!({"domain": "fai-object-storage-v1", "allocations": count, "allocatedBytes": count*24, "baselineLiveObjects": 1, "peakLiveObjects": 2, "finalLiveObjects": 1, "baselineLiveBytes": 24, "peakLiveBytes": 48, "finalLiveBytes": 24, "peakBackingBytes": 65536, "retainedSlabBytes": 65536, "retainedMappedBytes": 0, "retainedSystemBytes": 0, "arrayCopies": 0, "stringCopies": 0}),
                    };
                    allocation.push_str(&row.to_line());
                    allocation.push('\n');
                }
            }
        }
        fixture.step("allocations", "allocation", &allocation);
        for pass in 1..=3 {
            let name = format!("aot-{pass}");
            let root = format!("artifacts/{name}");
            let mut text =
                "algorithms_aot  fastest │ slowest │ median │ mean │ samples │ iters\n".to_owned();
            for algorithm in algorithms::ALGORITHMS {
                let contract = contracts::for_module(algorithm.module).unwrap();
                let (_, hashes) = fixture.peers(&root, algorithm.module, "once");
                fixture.file(
                    &format!("{root}/{}/workload.fai", algorithm.module),
                    aot::fai_program(algorithm, aot::Entry::Once).as_bytes(),
                );
                text.push_str(&format!("├─ {}\n", algorithm.module));
                for (index, side) in ["fai", "rust", "ocaml"].into_iter().enumerate() {
                    let details = match side {
                        "fai" => serde_json::json!({"compiler": Self::compiler()}),
                        "rust" => serde_json::from_slice(
                            &std::fs::read(fixture.directory.path().join(format!(
                                "{root}/{}/rust-compiler-info.json",
                                algorithm.module
                            )))
                            .unwrap(),
                        )
                        .unwrap(),
                        _ => {
                            serde_json::json!({"compilerInfo": "flags: -O3\nversion: fixture\nflambda: true\n"})
                        }
                    };
                    fixture.json(&format!("{root}/{}/{side}-once.json", algorithm.module), serde_json::json!({"methodology": contracts::METHODOLOGY, "workloadVersion": contract.version, "side": side, "executableHash": hashes[index], "details": details}));
                    text.push_str(&format!(
                        "│  ├─ {side}  1 ms │ 2 ms │ 1 ms │ 1 ms │ 100 │ 100\n"
                    ));
                }
            }
            fixture.step(&name, "timing", &text);
        }
        for name in ["perceus_space", "perceus_storage"] {
            fixture.step(
                name,
                "guards",
                "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n",
            );
        }
        fixture.save();
        fixture
    }

    fn save(&self) {
        std::fs::write(
            self.directory.path().join("snapshot.json"),
            serde_json::to_vec(&self.manifest).unwrap(),
        )
        .unwrap();
    }

    fn rewrite(&mut self, name: &str, f: impl FnOnce(String) -> String) {
        let old = std::fs::read_to_string(self.directory.path().join(name)).unwrap();
        self.file(name, f(old).as_bytes());
        self.save();
    }

    fn load(&self) -> Result<Snapshot, Error> {
        Snapshot::load(self.directory.path())
    }
}

#[test]
fn complete_snapshot_renders_three_deterministic_bodies() {
    let fixture = Fixture::new();
    let snapshot = fixture.load().unwrap();
    let a = snapshot.render("https://example.test/evidence").unwrap();
    assert_eq!(a, snapshot.render("https://example.test/evidence").unwrap());
    assert_eq!(a.len(), 3);
    assert!(a.values().all(|text| text.contains("fixture-checkpoint") && text.len() < 60000));
    assert!(a["language-runtime.md"].contains("0.500×"));
    assert!(a["perceus-memory.md"].contains("1000 → 0"));
    assert!(!a["language-memory.md"].contains("memory wins"));
}

#[test]
fn current_report_presentation_is_stable() {
    let fixture = Fixture::new();
    let reports = fixture.load().unwrap().render("https://example.test/evidence").unwrap();
    insta::assert_snapshot!("tracking_perceus", reports["perceus-memory.md"]);
    insta::assert_snapshot!(
        "tracking_runtime_header",
        reports["language-runtime.md"].split("## Main workloads").next().unwrap()
    );
    insta::assert_snapshot!(
        "tracking_memory_header",
        reports["language-memory.md"].split("## Current measurements").next().unwrap()
    );
}

#[test]
fn missing_warm_case_is_rejected() {
    let mut fixture = Fixture::new();
    fixture.rewrite("raw/warm.stdout", |text| text.lines().skip(1).collect::<Vec<_>>().join("\n"));
    assert!(fixture.load().unwrap_err().to_string().contains("incomplete warm matrix"));
}

#[test]
fn malformed_records_do_not_silently_disappear() {
    let mut fixture = Fixture::new();
    fixture.rewrite("raw/warm.stdout", |text| format!("WARMSTAT2\tbroken\n{text}"));
    assert!(fixture.load().unwrap_err().to_string().contains("malformed"));
}

#[test]
fn a_changed_executable_set_cannot_pair() {
    let mut fixture = Fixture::new();
    fixture.rewrite("raw/warm.stdout", |text| {
        let first = text.lines().next().unwrap();
        let identity = first.split('\t').nth(4).unwrap();
        text.replacen(identity, &"f".repeat(64), 1)
    });
    assert!(fixture.load().unwrap_err().to_string().contains("executable-set"));
}

#[test]
fn corrupt_retained_bytes_are_rejected() {
    let fixture = Fixture::new();
    std::fs::write(fixture.directory.path().join("artifacts/warm/Fib/fai-worker"), b"changed")
        .unwrap();
    assert!(fixture.load().is_err());
}

#[test]
fn compressed_artifacts_retain_their_logical_identity() {
    let mut fixture = Fixture::new();
    let name = "artifacts/warm/Fib/fai-worker";
    let path = fixture.directory.path().join(name);
    let bytes = std::fs::read(&path).unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&bytes).unwrap();
    let compressed = encoder.finish().unwrap();
    let artifact = fixture.manifest.files.iter_mut().find(|a| a.path == name).unwrap();
    artifact.gzip = true;
    artifact.stored_path.push_str(".gz");
    artifact.sha256 = format!("{:x}", Sha256::digest(&compressed));
    std::fs::write(fixture.directory.path().join(&artifact.stored_path), compressed).unwrap();
    std::fs::remove_file(path).unwrap();
    fixture.save();
    assert!(fixture.load().is_ok());
}

#[test]
fn artifact_paths_cannot_escape_the_snapshot() {
    let mut fixture = Fixture::new();
    fixture.manifest.files[0].stored_path = "../outside".into();
    fixture.save();
    assert!(fixture.load().unwrap_err().to_string().contains("unsafe artifact path"));
}

#[test]
fn failed_steps_cannot_publish() {
    let mut fixture = Fixture::new();
    fixture.manifest.steps[0].exit_code = 1;
    fixture.save();
    assert!(fixture.load().unwrap_err().to_string().contains("unsuccessful"));
}

#[test]
fn mixed_source_revisions_cannot_publish() {
    let mut fixture = Fixture::new();
    fixture.manifest.builds[1].revision = "b".repeat(40);
    fixture.save();
    assert!(fixture.load().unwrap_err().to_string().contains("mixed source revisions"));
}

#[test]
fn instrumented_timing_build_is_rejected() {
    let mut fixture = Fixture::new();
    fixture.manifest.builds[0].instrumented = true;
    fixture.save();
    assert!(fixture.load().unwrap_err().to_string().contains("instrumentation"));
}

#[test]
fn a_different_ocaml_configuration_cannot_be_presented_as_the_pinned_peer() {
    let mut fixture = Fixture::new();
    fixture.rewrite("artifacts/warm/Fib/ocaml-compiler-info.txt", |text| {
        text.replace("flambda: true", "flambda: false")
    });
    assert!(
        fixture.load().unwrap_err().to_string().contains("OCaml peer compiler/profile mismatch")
    );
}

#[test]
fn a_different_rust_compiler_cannot_be_presented_as_the_pinned_peer() {
    let mut fixture = Fixture::new();
    fixture.rewrite("artifacts/memory/Fib/rust-compiler-info.json", |text| {
        text.replace("\"compiler\":\"fixture\"", "\"compiler\":\"different\"")
    });
    assert!(
        fixture.load().unwrap_err().to_string().contains("Rust peer compiler/profile mismatch")
    );
}

#[test]
fn changed_peer_sources_cannot_reuse_the_old_measurement_identity() {
    let mut fixture = Fixture::new();
    fixture.rewrite("artifacts/aot-1/Fib/workload.rs", |text| format!("{text}\n// changed"));
    assert!(fixture.load().unwrap_err().to_string().contains("source mismatch"));
}

#[test]
fn stale_allocation_compiler_sources_cannot_pair_with_new_timings() {
    let mut fixture = Fixture::new();
    fixture.rewrite("artifacts/allocations/compiler-info.json", |text| {
        text.replace(&"d".repeat(64), &"e".repeat(64))
    });
    assert!(fixture.load().unwrap_err().to_string().contains("different sources"));
}

#[test]
fn filtered_out_memory_guards_prevent_publication() {
    let mut fixture = Fixture::new();
    fixture.rewrite("raw/perceus_space.stdout", |text| {
        text.replace("0 filtered out", "1 filtered out")
    });
    assert!(fixture.load().unwrap_err().to_string().contains("guard result"));
}

#[test]
fn allocation_windows_must_release_workload_owners() {
    let mut fixture = Fixture::new();
    fixture.rewrite("raw/allocations.stdout", |text| {
        text.replacen("\"finalLiveBytes\":24", "\"finalLiveBytes\":48", 1)
    });
    assert!(fixture.load().unwrap_err().to_string().contains("retains workload"));
}

#[test]
fn floor_limited_measurements_remain_visible_without_ratios() {
    let mut fixture = Fixture::new();
    fixture.rewrite("raw/warm.stdout", |text| {
        text.lines()
            .map(|line| {
                let mut row = WarmSample::parse(line).unwrap();
                if row.algorithm == "Fib" && row.side == "rust" {
                    row.floor_ns = row.samples_ns.clone();
                }
                row.to_line()
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    let reports = fixture.load().unwrap().render("").unwrap();
    assert!(
        reports["language-runtime.md"]
            .contains("| Fib | 28 | 10.000 | 20.000 | 25.000 | — | 0.400× | floor: rust |")
    );
}

//! Isolated native process footprints across dataset sizes and repetition counts.

fn main() {
    #[cfg(target_os = "linux")]
    measure::run();
    #[cfg(not(target_os = "linux"))]
    eprintln!("algorithms_mem: Linux /proc footprint measurement is unavailable");
}

#[cfg(target_os = "linux")]
mod measure {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use fai_db::Db;
    use fai_tests::algorithms::{ALGORITHMS, Algorithm, Oracle};
    use fai_tests::bench_summary::FootprintSample;
    use fai_tests::benchmark_aot::{self as aot, Entry};
    use fai_tests::benchmark_contracts::{self as contracts, METHODOLOGY};
    use fai_tests::benchmark_process::{ExpectedAnswer, spawn_checked};
    use fai_tests::native_peer;

    struct Config {
        modules: BTreeSet<String>,
        inputs: BTreeSet<i64>,
        sizes: String,
        repeats: Vec<usize>,
        samples: usize,
        test: bool,
        list: bool,
    }

    impl Config {
        fn read() -> Self {
            let mut result = Self {
                modules: BTreeSet::new(),
                inputs: BTreeSet::new(),
                sizes: "both".into(),
                repeats: vec![1, 16],
                samples: 5,
                test: false,
                list: false,
            };
            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--bench" => {}
                    "--module" => {
                        result.modules.insert(args.next().expect("module"));
                    }
                    "--input" => {
                        result.inputs.insert(
                            args.next().expect("input size").parse().expect("integer size"),
                        );
                    }
                    "--size" => result.sizes = args.next().expect("small, large or both"),
                    "--repeats" => {
                        result.repeats = args
                            .next()
                            .expect("comma-separated repetition counts")
                            .split(',')
                            .map(|s| s.parse().expect("repetition count"))
                            .collect()
                    }
                    "--samples" => {
                        result.samples =
                            args.next().expect("sample count").parse().expect("sample count")
                    }
                    "--test" => result.test = true,
                    "--list" => result.list = true,
                    "--help" | "-h" => {
                        println!(
                            "algorithms_mem [--module NAME] [--size small|large|both] [--input N] [--repeats 1,16] [--samples 5] [--test] [--list]"
                        );
                        std::process::exit(0);
                    }
                    _ => panic!("unknown memory argument: {arg}"),
                }
            }
            assert!(result.samples > 0 && !result.repeats.is_empty());
            assert!(result.repeats.iter().all(|&n| n <= aot::MAX_BATCH));
            assert!(result.inputs.iter().all(|n| (0..=aot::MAX_SIZE).contains(n)));
            assert!(matches!(result.sizes.as_str(), "small" | "large" | "both"));
            assert!(
                result.modules.iter().all(|name| ALGORITHMS.iter().any(|a| a.module == name)),
                "unknown memory workload"
            );
            if result.test {
                result.samples = 1;
            }
            result
        }

        fn inputs(&self, algorithm: &Algorithm) -> BTreeSet<i64> {
            if !self.inputs.is_empty() {
                return self.inputs.clone();
            }
            match self.sizes.as_str() {
                "small" => BTreeSet::from([algorithm.jit_size]),
                "large" => BTreeSet::from([algorithm.aot_size]),
                _ => BTreeSet::from([algorithm.jit_size, algorithm.aot_size]),
            }
        }
    }

    struct Peer {
        side: &'static str,
        executable: PathBuf,
        hash: String,
        instrumented: bool,
    }

    fn measure(
        peer: &Peer,
        size: i64,
        repeats: usize,
        floor: bool,
        expected: ExpectedAnswer,
    ) -> u64 {
        let mut command = Command::new(&peer.executable);
        command
            .args([
                size.to_string(),
                repeats.to_string(),
                if floor { "floor" } else { "run" }.into(),
            ])
            .env("FAI_REPORT_RSS", "1")
            .env_remove("FAI_REPORT_ALLOCATIONS");
        let output = spawn_checked(&mut command).unwrap();
        expected.verify(peer.side, &output).unwrap();
        String::from_utf8(output.stderr)
            .unwrap()
            .lines()
            .find_map(|line| {
                line.strip_prefix("fai-peak-rss-kib:").and_then(|value| value.trim().parse().ok())
            })
            .filter(|&value| value > 0)
            .expect("native peer must report a positive peak RSS")
    }

    fn build(algorithm: &Algorithm, folder: &Path) -> Vec<Peer> {
        std::fs::create_dir_all(folder).unwrap();
        let source = aot::fai_program(algorithm, Entry::Memory);
        std::fs::write(folder.join("memory.fai"), &source).unwrap();
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(format!("{}.fai", algorithm.module).into(), source);
        let fai = camino::Utf8PathBuf::from_path_buf(folder.join("fai-memory")).unwrap();
        let outcome = fai_driver::build_native(&db, db.source_file(id).unwrap(), &fai);
        assert!(outcome.ok, "{}: {:?}", algorithm.module, outcome.diagnostics);
        let rust = native_peer::rust(algorithm, Entry::Memory);
        let rust_target = folder.join("rust-memory");
        std::fs::copy(&rust, &rust_target).unwrap();
        std::fs::copy(rust.with_file_name("peer.rs"), folder.join("rust-memory.rs")).unwrap();
        std::fs::copy(
            rust.with_file_name("compiler-info.json"),
            folder.join("rust-compiler-info.json"),
        )
        .unwrap();
        let mut peers = vec![
            Peer {
                side: "fai",
                hash: native_peer::hash(fai.as_std_path()),
                executable: fai.into_std_path_buf(),
                instrumented: fai_runtime::ALLOCATION_STATS_ENABLED,
            },
            Peer {
                side: "rust",
                hash: native_peer::hash(&rust_target),
                executable: rust_target,
                instrumented: false,
            },
        ];
        if let Some(ocaml) = fai_tests::ocaml::isolated(algorithm, Entry::Memory) {
            let target = folder.join("ocaml-memory");
            std::fs::copy(&ocaml, &target).unwrap();
            std::fs::copy(ocaml.with_file_name("baseline.ml"), folder.join("memory.ml")).unwrap();
            std::fs::copy(
                ocaml.with_file_name("compiler-info.txt"),
                folder.join("ocaml-compiler-info.txt"),
            )
            .unwrap();
            peers.push(Peer {
                side: "ocaml",
                hash: native_peer::hash(&target),
                executable: target,
                instrumented: false,
            });
        }
        let contract = contracts::for_module(algorithm.module).unwrap();
        let environment: std::collections::BTreeMap<_, _> =
            ["GLIBC_TUNABLES", "MALLOC_ARENA_MAX", "OCAMLRUNPARAM", "OCAMLPARAM", "RUSTFLAGS"]
                .into_iter()
                .map(|key| (key, std::env::var(key).ok()))
                .collect();
        let identity = serde_json::json!({
            "methodology": METHODOLOGY, "algorithm": algorithm.module,
            "hashAlgorithm": "blake3", "faiCompiler": fai_driver::build_info(),
            "environment": environment,
            "workloadVersion": contract.version, "comparison": contract.comparison.as_str(), "contract": contract.note,
            "peers": peers.iter().map(|p| serde_json::json!({"side": p.side, "path": p.executable, "hash": p.hash, "instrumented": p.instrumented})).collect::<Vec<_>>(),
        });
        std::fs::write(folder.join("identity.json"), serde_json::to_vec_pretty(&identity).unwrap())
            .unwrap();
        println!("AOTARTIFACT\t{identity}");
        peers
    }

    pub fn run() {
        let config = Config::read();
        assert!(
            !cfg!(debug_assertions) || config.test,
            "use an optimized build for memory comparisons"
        );
        let temporary = tempfile::Builder::new().prefix("fai-memory-").tempdir().unwrap();
        let directory = std::env::var_os("FAI_BENCH_ARTIFACT_DIR")
            .map_or_else(|| temporary.path().to_path_buf(), PathBuf::from);
        let directory = std::env::current_dir().unwrap().join(directory);
        for algorithm in ALGORITHMS
            .iter()
            .filter(|a| config.modules.is_empty() || config.modules.contains(a.module))
        {
            if config.list {
                for size in config.inputs(algorithm) {
                    println!("{} / n={size} / repeats={:?}", algorithm.module, config.repeats);
                }
                continue;
            }
            let peers = build(algorithm, &directory.join(algorithm.module));
            let comparison_id = blake3::hash(
                peers.iter().map(|p| p.hash.as_str()).collect::<Vec<_>>().join(":").as_bytes(),
            )
            .to_hex()
            .to_string();
            let contract = contracts::for_module(algorithm.module).unwrap();
            for size in config.inputs(algorithm) {
                let answer = ExpectedAnswer::at_size(algorithm, size);
                let floor = match algorithm.oracle {
                    Oracle::Int(_) => ExpectedAnswer::Int(size),
                    Oracle::Float(_) => ExpectedAnswer::Float(size as f64),
                };
                for &iterations in &config.repeats {
                    let expected = aot::checksum(&[answer], iterations);
                    let floor_expected = aot::checksum(&[floor], iterations);
                    let mut observations: Vec<Vec<u64>> =
                        peers.iter().map(|_| Vec::new()).collect();
                    let mut floors = observations.clone();
                    for sample in 0..config.samples {
                        for offset in 0..peers.len() {
                            let index = (sample + offset) % peers.len();
                            let peer = &peers[index];
                            let (peak, base) = if sample.is_multiple_of(2) {
                                let base = measure(peer, size, iterations, true, floor_expected);
                                (measure(peer, size, iterations, false, expected), base)
                            } else {
                                let peak = measure(peer, size, iterations, false, expected);
                                (peak, measure(peer, size, iterations, true, floor_expected))
                            };
                            observations[index].push(peak);
                            floors[index].push(base);
                        }
                    }
                    for (index, peer) in peers.iter().enumerate() {
                        println!(
                            "{}",
                            FootprintSample {
                                methodology: METHODOLOGY.into(),
                                algorithm: algorithm.module.into(),
                                workload_version: contract.version.into(),
                                comparison: contract.comparison.as_str().into(),
                                comparison_id: comparison_id.clone(),
                                side: peer.side.into(),
                                executable_hash: peer.hash.clone(),
                                size,
                                iterations,
                                samples_kib: std::mem::take(&mut observations[index]),
                                floor_kib: std::mem::take(&mut floors[index]),
                                instrumented: peer.instrumented,
                            }
                            .to_line()
                        );
                    }
                }
            }
        }
    }
}

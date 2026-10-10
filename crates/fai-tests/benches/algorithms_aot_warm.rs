//! Paired warm execution of real AOT programs with runtime input windows.
//!
//! One common batch size is calibrated per case. Every timed reply is checked;
//! raw batch and harness-floor samples are emitted as WARMSTAT records. No floor
//! subtraction or filtering is applied. The report suppresses floor-limited ratios.

fn main() {
    #[cfg(not(windows))]
    native::run();
}

#[cfg(not(windows))]
mod native {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use fai_db::Db;
    use fai_tests::algorithms::{ALGORITHMS, Algorithm, Oracle};
    use fai_tests::bench_summary::WarmSample;
    use fai_tests::benchmark_aot::{self as aot, Entry};
    use fai_tests::benchmark_process::{ExpectedAnswer, Worker};

    const INPUT_WINDOW: usize = 32;
    const TARGET_BATCH: Duration = Duration::from_millis(5);
    const SLOW_BATCH: Duration = Duration::from_millis(100);

    struct Config {
        samples: usize,
        passes: u32,
        modules: BTreeSet<String>,
        sizes: String,
        test: bool,
        list: bool,
    }

    impl Config {
        fn read() -> Self {
            let mut result = Self {
                samples: 21,
                passes: 3,
                modules: BTreeSet::new(),
                sizes: "both".into(),
                test: false,
                list: false,
            };
            let mut args = std::env::args().skip(1);
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--bench" => {}
                    "--list" => result.list = true,
                    "--test" => {
                        result.test = true;
                        result.samples = 1;
                        result.passes = 1;
                    }
                    "--samples" => {
                        result.samples = args
                            .next()
                            .expect("sample count")
                            .parse()
                            .expect("positive sample count")
                    }
                    "--passes" => {
                        result.passes =
                            args.next().expect("pass count").parse().expect("positive pass count")
                    }
                    "--module" => {
                        result.modules.insert(args.next().expect("module name"));
                    }
                    "--size" => result.sizes = args.next().expect("small, large or both"),
                    "--help" | "-h" => {
                        println!(
                            "algorithms_aot_warm [--samples N] [--passes N] [--module NAME] [--size small|large|both] [--list] [--test]"
                        );
                        std::process::exit(0);
                    }
                    _ => panic!("unknown warm AOT option: {arg}"),
                }
            }
            assert!(result.samples > 0 && result.passes > 0);
            assert!(matches!(result.sizes.as_str(), "small" | "large" | "both"));
            assert!(
                result
                    .modules
                    .iter()
                    .all(|name| ALGORITHMS.iter().any(|algorithm| algorithm.module == name)),
                "unknown workload"
            );
            result
        }

        fn sizes(&self, algorithm: &Algorithm) -> BTreeSet<i64> {
            match self.sizes.as_str() {
                "small" => BTreeSet::from([algorithm.jit_size]),
                "large" => BTreeSet::from([algorithm.aot_size]),
                _ => BTreeSet::from([algorithm.jit_size, algorithm.aot_size]),
            }
        }
    }

    struct Peer {
        side: &'static str,
        worker: Worker,
    }

    fn elapsed(peer: &mut Peer, command: &str, expected: ExpectedAnswer) -> u64 {
        let start = Instant::now();
        let answer =
            peer.worker.request(command).unwrap_or_else(|error| panic!("{}: {error}", peer.side));
        let nanos = u64::try_from(start.elapsed().as_nanos()).expect("batch duration fits u64");
        expected.verify_line(peer.side, &answer).unwrap();
        nanos.max(1)
    }

    fn median(values: &mut [u64]) -> u64 {
        values.sort_unstable();
        values[values.len() / 2]
    }

    fn calibrate(peers: &mut [Peer], values: &[ExpectedAnswer], test: bool) -> usize {
        if test {
            return 1;
        }
        let mut batch = 1;
        loop {
            let expected = aot::checksum(values, batch);
            let command = format!("run {batch}");
            let mut fastest = u64::MAX;
            let mut slowest = 0;
            for peer in &mut *peers {
                let mut samples = [0; 3];
                for sample in &mut samples {
                    *sample = elapsed(peer, &command, expected);
                }
                let duration = median(&mut samples);
                fastest = fastest.min(duration);
                slowest = slowest.max(duration);
            }
            if Duration::from_nanos(fastest) >= TARGET_BATCH
                || Duration::from_nanos(slowest) >= SLOW_BATCH
                || batch == aot::MAX_BATCH
            {
                return batch;
            }
            batch = (batch * 2).min(aot::MAX_BATCH);
        }
    }

    fn build_fai(algorithm: &Algorithm, directory: &Path) -> PathBuf {
        let source = aot::fai_program(algorithm, Entry::Worker);
        std::fs::write(directory.join("worker.fai"), &source).unwrap();
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(format!("{}.fai", algorithm.module).into(), source);
        let path = camino::Utf8PathBuf::from_path_buf(directory.join("fai-worker")).unwrap();
        let built = fai_driver::build_native(&db, db.source_file(id).unwrap(), &path);
        assert!(built.ok, "{}: {:?}", algorithm.module, built.diagnostics);
        built.artifact.unwrap().into_std_path_buf()
    }

    fn measure(
        algorithm: &Algorithm,
        size: i64,
        fai: &Path,
        rust_path: &Path,
        ocaml: Option<&Path>,
        config: &Config,
    ) {
        let inputs = vec![size; INPUT_WINDOW];
        let configuration = inputs.iter().map(i64::to_string).collect::<Vec<_>>().join(" ");
        let value = ExpectedAnswer::at_size(algorithm, size);
        let values = vec![value; INPUT_WINDOW];
        let floor_value = match algorithm.oracle {
            Oracle::Int(_) => ExpectedAnswer::Int(size),
            Oracle::Float(_) => ExpectedAnswer::Float(size as f64),
        };
        let floors = vec![floor_value; INPUT_WINDOW];
        let mut rust = Command::new(rust_path);
        rust.arg(algorithm.module);
        let mut peers = vec![
            Peer { side: "fai", worker: Worker::start(Command::new(fai), &configuration).unwrap() },
            Peer { side: "rust", worker: Worker::start(rust, &configuration).unwrap() },
        ];
        if let Some(ocaml) = ocaml {
            peers.push(Peer {
                side: "ocaml",
                worker: Worker::start(Command::new(ocaml), &configuration).unwrap(),
            });
        }
        for peer in &mut peers {
            value.verify_line(peer.side, &peer.worker.request("value 0").unwrap()).unwrap();
            value
                .verify_line(
                    peer.side,
                    &peer.worker.request(&format!("value {}", INPUT_WINDOW - 1)).unwrap(),
                )
                .unwrap();
        }
        let batch = calibrate(&mut peers, &values, config.test);
        let expected = aot::checksum(&values, batch);
        let expected_floor = aot::checksum(&floors, batch);
        let command = format!("run {batch}");
        let floor_command = format!("floor {batch}");
        if config.test {
            for peer in &mut peers {
                elapsed(peer, &command, expected);
                elapsed(peer, &floor_command, expected_floor);
            }
            for peer in peers {
                peer.worker.finish().unwrap();
            }
            println!("validated {} / n={size}", algorithm.module);
            return;
        }
        println!(
            "WARMMETA\t{}",
            serde_json::json!({
                "methodology": "warm-aot-batched-v1",
                "algorithm": algorithm.module, "size": size, "batch": batch,
                "inputWindow": INPUT_WINDOW, "targetBatchMs": TARGET_BATCH.as_millis(),
                "slowBatchMs": SLOW_BATCH.as_millis(), "debugAssertions": cfg!(debug_assertions),
                "samplesPerPass": config.samples, "passes": config.passes,
                "scope": "warm AOT batch round trip; floor measured without subtraction",
            })
        );
        for peer in &mut peers {
            for _ in 0..2 {
                elapsed(peer, &command, expected);
                elapsed(peer, &floor_command, expected_floor);
            }
        }
        for pass in 1..=config.passes {
            let mut samples: Vec<Vec<u64>> =
                peers.iter().map(|_| Vec::with_capacity(config.samples)).collect();
            let mut floor_samples = samples.clone();
            for sample in 0..config.samples {
                let mut order: Vec<_> = (0..peers.len()).collect();
                order.rotate_left((sample + pass as usize) % peers.len());
                if (sample + pass as usize).is_multiple_of(2) {
                    order.reverse();
                }
                for index in order {
                    let peer = &mut peers[index];
                    let (duration, floor) = if (sample + pass as usize).is_multiple_of(2) {
                        let floor = elapsed(peer, &floor_command, expected_floor);
                        (elapsed(peer, &command, expected), floor)
                    } else {
                        let duration = elapsed(peer, &command, expected);
                        (duration, elapsed(peer, &floor_command, expected_floor))
                    };
                    samples[index].push(duration);
                    floor_samples[index].push(floor);
                }
            }
            for (index, peer) in peers.iter().enumerate() {
                let row = WarmSample {
                    algorithm: algorithm.module.into(),
                    size,
                    side: peer.side.into(),
                    batch: batch as u64,
                    pass,
                    passes: config.passes,
                    samples_ns: std::mem::take(&mut samples[index]),
                    floor_ns: std::mem::take(&mut floor_samples[index]),
                };
                println!("{}", row.to_line());
            }
        }
        for peer in peers {
            peer.worker.finish().unwrap();
        }
    }

    pub(super) fn run() {
        let config = Config::read();
        let selected: Vec<_> = ALGORITHMS
            .iter()
            .filter(|algorithm| {
                config.modules.is_empty() || config.modules.contains(algorithm.module)
            })
            .collect();
        if config.list {
            for algorithm in selected {
                for size in config.sizes(algorithm) {
                    println!("{} / n={size}", algorithm.module);
                }
            }
            return;
        }
        assert!(
            !cfg!(debug_assertions) || config.test,
            "use cargo bench for optimized warm comparisons"
        );
        let temporary = tempfile::tempdir().unwrap();
        let directory = std::env::var_os("FAI_BENCH_ARTIFACT_DIR")
            .map_or_else(|| temporary.path().to_path_buf(), PathBuf::from);
        let directory = std::env::current_dir().unwrap().join(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let rust = directory.join("rust-worker");
        std::fs::copy(env!("CARGO_BIN_EXE_algo-worker"), &rust).unwrap();
        std::fs::write(directory.join("rust-worker.rs"), include_str!("../src/bin/algo-worker.rs"))
            .unwrap();
        std::fs::write(
            directory.join("rust-worker-support.rs"),
            include_str!("../src/benchmark_aot.rs"),
        )
        .unwrap();
        let metadata = serde_json::json!({
            "methodology": "warm-aot-batched-v1", "gitRevision": std::env::var("GITHUB_SHA").ok(),
            "debugAssertions": cfg!(debug_assertions), "rustflags": std::env::var("RUSTFLAGS").ok(),
            "encodedRustflags": std::env::var("CARGO_ENCODED_RUSTFLAGS").ok(),
            "ocamlCompiler": std::env::var("FAI_BENCH_OCAMLOPT").ok(),
            "inputWindow": INPUT_WINDOW, "samplesPerPass": config.samples, "passes": config.passes,
        });
        std::fs::write(
            directory.join("metadata.json"),
            serde_json::to_vec_pretty(&metadata).unwrap(),
        )
        .unwrap();
        for algorithm in selected {
            let folder = directory.join(algorithm.module);
            std::fs::create_dir_all(&folder).unwrap();
            let fai = build_fai(algorithm, &folder);
            let ocaml = fai_tests::ocaml::worker_baseline(algorithm).map(|path| {
                let target = folder.join("ocaml-worker");
                std::fs::copy(&path, &target).unwrap();
                std::fs::copy(
                    path.with_file_name("compiler-info.txt"),
                    folder.join("ocaml-compiler-info.txt"),
                )
                .unwrap();
                std::fs::copy(path.with_file_name("baseline.ml"), folder.join("worker.ml"))
                    .unwrap();
                target
            });
            println!(
                "AOTARTIFACT\t{}",
                serde_json::json!({"algorithm": algorithm.module, "fai": fai, "rust": rust, "ocaml": ocaml})
            );
            for size in config.sizes(algorithm) {
                measure(algorithm, size, &fai, &rust, ocaml.as_deref(), &config);
            }
        }
    }
}

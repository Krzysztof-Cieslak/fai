//! Memory comparison of the *delivered binaries*: the peak resident set size of
//! a `fai build` native executable vs the Rust release binary, running the same
//! AOT workload. The companion `algorithms_aot` bench compares their wall-clock
//! time; this is the memory side of the same "delivered binaries" experiment.
//!
//! Unlike the divan benches, this is not a timing loop — peak memory is not a
//! per-iteration measurement. Each binary is run a few times with
//! `FAI_REPORT_RSS` set; both the Fai runtime and the `algo-baseline` binary then
//! print their peak RSS (Linux `/proc/self/status` `VmHWM`) to stderr, and the
//! maximum across runs is reported as a `MEMSTAT` line that the `bench-summary`
//! tool renders into a "Fai vs Rust — peak RSS" table (divan's parser ignores the
//! line, so it is safe in the shared benchmark output stream).
//!
//! Peak RSS includes fixed process overhead (the linked runtime/std, code pages,
//! allocator slack), so the small-heap workloads (fib, collatz, pi) are dominated
//! by that floor; the heap-heavy ones (map_sum, merge_sort, binary_trees) carry
//! the real signal. As with the timing benches the boxed, reference-counted
//! representation makes this a progress metric, not a fair fight.
//!
//! Linux-only: peak RSS is read from `/proc`, and the build/link + spawn path is
//! skipped on Windows (it would need the MSVC environment, mirroring
//! `algorithms_aot`). The bench still compiles everywhere so `--all-targets`
//! keeps it from bitrotting, and the Benchmarks workflow runs on Linux. Run with
//! `cargo bench -p fai-tests --bench algorithms_mem`.

fn main() {
    #[cfg(not(windows))]
    measure::run();
}

#[cfg(not(windows))]
mod measure {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    use camino::Utf8PathBuf;
    use fai_db::{Db, FaiDatabase};
    use fai_driver::build_native;
    use fai_tests::algorithms::{ALGORITHMS, Algorithm};

    /// How many times each binary runs. Peak RSS is a deterministic high-water
    /// mark, so a handful of repeats and the maximum absorb any scheduler or
    /// allocator jitter.
    const RUNS: usize = 5;

    /// Measures and reports the peak RSS of each algorithm's two delivered
    /// binaries, or explains why it cannot on this platform.
    pub fn run() {
        if !cfg!(target_os = "linux") {
            println!(
                "algorithms_mem: peak RSS is read from /proc (Linux-only); skipping on this platform"
            );
            return;
        }
        let baseline = env!("CARGO_BIN_EXE_algo-baseline");
        // The OCaml baseline is compiled once (or absent, when `ocamlopt` is not
        // installed — then its rows are simply skipped, like the Rust/Fai split).
        let ocaml = fai_tests::ocaml::baseline();
        for algo in ALGORITHMS {
            let exe = build_fai_binary(algo);
            let fai = peak_rss(|| Command::new(&exe));
            let _ = std::fs::remove_file(&exe);

            let size = algo.aot_size.to_string();
            let rust = peak_rss(|| {
                let mut cmd = Command::new(baseline);
                cmd.args([algo.module, size.as_str()]);
                cmd
            });
            let ocaml = ocaml.map(|exe| {
                peak_rss(|| {
                    let mut cmd = Command::new(exe);
                    cmd.args([algo.module, size.as_str()]);
                    cmd
                })
            });

            // Sentinel lines `bench-summary` parses into the memory table.
            if let Some(kib) = fai {
                println!("MEMSTAT\t{}\tfai\t{kib}", algo.module);
            }
            if let Some(kib) = rust {
                println!("MEMSTAT\t{}\trust\t{kib}", algo.module);
            }
            if let Some(Some(kib)) = ocaml {
                println!("MEMSTAT\t{}\tocaml\t{kib}", algo.module);
            }
        }
    }

    /// Runs `make()` `RUNS` times with `FAI_REPORT_RSS` set, parsing the peak RSS
    /// each binary prints to stderr and returning the maximum (the stable peak),
    /// or `None` if no run reported one.
    fn peak_rss(make: impl Fn() -> Command) -> Option<u64> {
        let mut peak: Option<u64> = None;
        for _ in 0..RUNS {
            let mut cmd = make();
            cmd.env("FAI_REPORT_RSS", "1");
            let output = cmd.output().expect("spawn benchmark binary");
            assert!(output.status.success(), "benchmark binary exited with {:?}", output.status);
            if let Some(kib) = parse_rss(&output.stderr) {
                peak = Some(peak.map_or(kib, |p| p.max(kib)));
            }
        }
        peak
    }

    /// Parses the `fai-peak-rss-kib: <n>` line both binaries print to stderr.
    fn parse_rss(stderr: &[u8]) -> Option<u64> {
        let text = std::str::from_utf8(stderr).ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("fai-peak-rss-kib:") {
                return rest.trim().parse().ok();
            }
        }
        None
    }

    /// A unique temporary path for a built executable.
    fn unique_exe(module: &str) -> Utf8PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp dir is UTF-8");
        dir.join(format!(
            "fai-algo-mem-{module}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Links the algorithm's sample (with its baked workload size) into a native
    /// executable, returning the path actually produced.
    fn build_fai_binary(algo: &Algorithm) -> Utf8PathBuf {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source(format!("{}.fai", algo.module).into(), algo.source().to_owned());
        let file = db.source_file(id).expect("sample source registered");
        let outcome = build_native(&db, file, &unique_exe(algo.module));
        outcome
            .artifact
            .unwrap_or_else(|| panic!("{} failed to build a native executable", algo.module))
    }
}

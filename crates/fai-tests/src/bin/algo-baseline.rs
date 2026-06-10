//! The Rust side of the subprocess AOT benchmark: run one algorithm at one size
//! and print the result.
//!
//! The Rust-vs-Fai AOT bench spawns this against the `fai build` binary so it
//! compares a delivered Rust release binary to a delivered Fai binary
//! (process startup, linking, and all). Printing the result keeps the
//! computation from being optimized away, matching the Fai binary's `main`.
//!
//! Usage: `algo-baseline <module> <n>`. With `FAI_REPORT_RSS` set, it also prints
//! its peak resident set size to stderr (the same line the Fai runtime emits), so
//! the memory comparison harness can measure both binaries the same way.

use fai_tests::algorithms::{Oracle, by_module};

fn main() {
    let mut args = std::env::args().skip(1);
    let module = args.next().expect("usage: algo-baseline <module> <n>");
    let n: i64 = args
        .next()
        .expect("usage: algo-baseline <module> <n>")
        .parse()
        .expect("n must be an integer");
    let algo = by_module(&module).unwrap_or_else(|| panic!("unknown algorithm module: {module}"));
    match algo.oracle {
        // Print the Float the same way Fai's `Float.toString` does (`{:?}`), so a
        // reader comparing the two binaries' output sees the same formatting.
        Oracle::Int(f) => println!("{}", f(n)),
        Oracle::Float(f) => println!("{:?}", f(n)),
    }
    // Self-report peak memory in the same format the Fai runtime uses, so the
    // memory comparison measures both delivered binaries identically.
    if std::env::var_os("FAI_REPORT_RSS").is_some()
        && let Some(kib) = fai_tests::peak_rss_kib()
    {
        eprintln!("fai-peak-rss-kib: {kib}");
    }
}

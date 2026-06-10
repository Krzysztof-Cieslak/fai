//! Builds the Fai runtime as a static archive and records its path so the driver
//! can embed it (via `include_bytes!`) and link it into AOT executables.
//!
//! `fai-runtime` is intentionally dependency-free (std only), so a single
//! `$RUSTC` invocation produces the archive — no nested `cargo`, no unstable
//! artifact dependencies. The same invocation reports, via
//! `--print native-static-libs`, the system libraries the archive must be linked
//! against on this host; the driver passes those to the platform linker instead
//! of hard-coding a Linux-only set.
//!
//! The archive is always optimized (`-O`), but its `debug_assertions` is set to
//! match the profile building the driver: the runtime's leak counters are
//! compiled in only under `debug_assertions`, so mirroring it keeps the native
//! executables' end-of-run leak check working under `cargo test` while a
//! release/bench build links a counter-free (faster) runtime. Cargo exposes the
//! driver's setting to this script as `CARGO_CFG_DEBUG_ASSERTIONS`.

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let runtime_src = Path::new(&manifest).join("../fai-runtime/src/lib.rs");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let target = std::env::var("TARGET").unwrap_or_default();
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());

    // Name the archive for the host linker's convention: MSVC's `link.exe`
    // consumes `.lib`, every other linker consumes `.a`. (The bytes are a
    // host-native static archive either way; only the consumer's name matters.)
    let archive_name = if target.contains("windows") && target.contains("msvc") {
        "fai_runtime.lib"
    } else {
        "libfai_runtime.a"
    };
    let archive = Path::new(&out_dir).join(archive_name);

    // Match the archive's debug assertions to the driver's, so the runtime's
    // leak counters are present exactly when the rest of the toolchain has them.
    // Cargo sets CARGO_CFG_DEBUG_ASSERTIONS iff the crate being built (the driver)
    // has them on (debug/test); it is absent for a release/bench build.
    let debug_assertions = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();
    let debug_assertions_flag =
        if debug_assertions { "debug-assertions=on" } else { "debug-assertions=off" };

    let output = Command::new(&rustc)
        .args([
            "--edition",
            "2024",
            "--crate-type",
            "staticlib",
            "--crate-name",
            "fai_runtime",
            "-O",
            "-C",
            debug_assertions_flag,
            "--print",
            "native-static-libs",
        ])
        .arg(&runtime_src)
        .arg("-o")
        .arg(&archive)
        .output()
        .expect("failed to invoke rustc for the runtime archive");
    assert!(
        output.status.success(),
        "building the fai-runtime static archive failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // rustc prints `note: native-static-libs: <libs>` to stderr. Capture the
    // list so the driver links the runtime with exactly what std needs on this
    // platform (e.g. `-lpthread -ldl -lm` on Linux, `-lSystem` on macOS, the CRT
    // and Win32 import libs on Windows).
    let stderr = String::from_utf8_lossy(&output.stderr);
    let native_libs = stderr
        .lines()
        .find_map(|line| line.split_once("native-static-libs:"))
        .map(|(_, libs)| libs.trim().to_owned())
        .unwrap_or_default();

    println!("cargo:rerun-if-changed={}", runtime_src.display());
    // Rebuild the archive if the driver's debug-assertions setting flips within a
    // profile (separate profiles already get separate out dirs).
    println!("cargo:rerun-if-env-changed=CARGO_CFG_DEBUG_ASSERTIONS");
    println!("cargo:rustc-env=FAI_RUNTIME_ARCHIVE={}", archive.display());
    println!("cargo:rustc-env=FAI_RUNTIME_NATIVE_LIBS={native_libs}");
}

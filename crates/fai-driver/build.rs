//! Builds the Fai runtime as a static archive and records its path so the driver
//! can embed it (via `include_bytes!`) and link it into AOT executables.
//!
//! `fai-runtime` has a few native dependencies (the M:N scheduler's stackful
//! coroutines, work-stealing deques, and IO reactor), so a single `$RUSTC`
//! invocation can no longer produce the archive. Instead a **nested `cargo`**
//! build compiles the crate as a `staticlib` (which bundles its Rust
//! dependencies into the one archive) into a **private target directory** under
//! `OUT_DIR` — a directory distinct from the outer build's, so its build lock
//! never deadlocks against the `cargo` invocation running this script. The same
//! `cargo rustc` invocation reports, via `--print native-static-libs`, the system
//! libraries the archive must be linked against on this host; the driver passes
//! those to the platform linker instead of hard-coding a set.
//!
//! The archive is always optimized (the release profile), but its
//! `debug_assertions` is set to match the profile building the driver: the
//! runtime's leak counters are compiled in only under `debug_assertions`, so
//! mirroring it keeps the native executables' end-of-run leak check working under
//! `cargo test` while a release/bench build links a counter-free (faster)
//! runtime. Cargo exposes the driver's setting as `CARGO_CFG_DEBUG_ASSERTIONS`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let runtime_dir = Path::new(&manifest).join("../fai-runtime");
    let runtime_manifest = runtime_dir.join("Cargo.toml");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    // A private target directory for the nested build, so its lock is independent
    // of the outer `cargo`'s lock on the shared target dir (a nested build into the
    // same directory would deadlock).
    let nested_target = Path::new(&out_dir).join("runtime-archive");

    // Match the archive's debug assertions to the driver's. Cargo sets
    // CARGO_CFG_DEBUG_ASSERTIONS iff the crate being built (the driver) has them on
    // (debug/test); it is absent for a release/bench build. The archive is always
    // built with the optimized release profile; only `debug-assertions` flips.
    let debug_assertions = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_some();

    let mut cmd = Command::new(&cargo);
    cmd.arg("rustc")
        .arg("--manifest-path")
        .arg(&runtime_manifest)
        .arg("--lib")
        .arg("--crate-type")
        .arg("staticlib")
        .arg("--release")
        .arg("--target-dir")
        .arg(&nested_target)
        // Optimized, with debug-assertions mirroring the driver's profile.
        .env(
            "CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS",
            if debug_assertions { "true" } else { "false" },
        )
        // Force color off for the nested build: we parse the `native-static-libs`
        // note out of stderr, and a CI that sets `CARGO_TERM_COLOR=always` would
        // otherwise embed ANSI escapes in the library names, breaking the link.
        .env("CARGO_TERM_COLOR", "never")
        // Pass `--print native-static-libs` through to the final rustc so we learn
        // exactly which system libraries the archive needs on this host.
        .arg("--")
        .arg("--print")
        .arg("native-static-libs");

    let output = cmd.output().expect("failed to invoke cargo for the runtime archive");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "building the fai-runtime static archive failed:\n{stderr}");

    // Name the archive for the host linker's convention: MSVC's `link.exe`
    // consumes `.lib`, every other linker consumes `.a`. Cargo writes the staticlib
    // under `<target-dir>/release/`.
    let target = std::env::var("TARGET").unwrap_or_default();
    let archive_name = if target.contains("windows") && target.contains("msvc") {
        "fai_runtime.lib"
    } else {
        "libfai_runtime.a"
    };
    let archive = nested_target.join("release").join(archive_name);
    assert!(
        archive.exists(),
        "the runtime archive was not produced at {}\n{stderr}",
        archive.display()
    );

    // rustc prints `note: native-static-libs: <libs>` to stderr. Capture the list
    // so the driver links the runtime with exactly what it needs on this platform
    // (e.g. `-lpthread -ldl -lm` on Linux, `-lSystem` on macOS, the CRT + Win32
    // import libs on Windows).
    let native_libs = stderr
        .lines()
        .find_map(|line| line.split_once("native-static-libs:"))
        .map(|(_, libs)| strip_ansi(libs.trim()))
        .unwrap_or_default();

    // Re-run when any runtime source, its manifest, or the lockfile changes (a
    // dependency bump or a new module must rebuild the archive).
    rerun_if_changed_recursively(&runtime_dir.join("src"));
    println!("cargo:rerun-if-changed={}", runtime_manifest.display());
    println!("cargo:rerun-if-changed={}", Path::new(&manifest).join("../../Cargo.lock").display());
    // Rebuild the archive if the driver's debug-assertions setting flips within a
    // profile (separate profiles already get separate out dirs).
    println!("cargo:rerun-if-env-changed=CARGO_CFG_DEBUG_ASSERTIONS");
    println!("cargo:rustc-env=FAI_RUNTIME_ARCHIVE={}", archive.display());
    println!("cargo:rustc-env=FAI_RUNTIME_NATIVE_LIBS={native_libs}");
}

/// Removes ANSI escape sequences (`ESC [ … m`) from `s`. The linker library list
/// must be plain text; a stray escape would become part of a `-l<name>` token and
/// the linker would fail to find the (garbled) library.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == 0x1b {
            // Skip until the terminating letter of the escape sequence.
            for c in bytes.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(b as char);
        }
    }
    out
}

/// Emits `cargo:rerun-if-changed` for every file under `dir`, so editing any
/// runtime source (not just `lib.rs`) rebuilds the archive. `rerun-if-changed` on
/// a directory alone misses content edits to existing files, so each file is
/// listed individually.
fn rerun_if_changed_recursively(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        println!("cargo:rerun-if-changed={}", dir.display());
        return;
    };
    let mut stack: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    while let Some(path) = stack.pop() {
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&path) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

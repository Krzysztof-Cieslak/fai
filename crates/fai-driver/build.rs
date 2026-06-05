//! Builds the Fai runtime as a static archive and records its path so the driver
//! can embed it (via `include_bytes!`) and link it into AOT executables.
//!
//! `fai-runtime` is intentionally dependency-free (std only), so a single
//! `$RUSTC` invocation produces the archive — no nested `cargo`, no unstable
//! artifact dependencies.

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let runtime_src = Path::new(&manifest).join("../fai-runtime/src/lib.rs");
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let archive = Path::new(&out_dir).join("libfai_runtime.a");
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());

    let status = Command::new(&rustc)
        .args([
            "--edition",
            "2024",
            "--crate-type",
            "staticlib",
            "--crate-name",
            "fai_runtime",
            "-O",
        ])
        .arg(&runtime_src)
        .arg("-o")
        .arg(&archive)
        .status()
        .expect("failed to invoke rustc for the runtime archive");
    assert!(status.success(), "building the fai-runtime static archive failed");

    println!("cargo:rerun-if-changed={}", runtime_src.display());
    println!("cargo:rustc-env=FAI_RUNTIME_ARCHIVE={}", archive.display());
}

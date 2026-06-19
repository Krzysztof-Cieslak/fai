//! Embeds the standard-library sources into the compiler.
//!
//! The `.fai` modules under the workspace `std/` directory (recursively,
//! including subdirectories like `datetime/`) are read at build time and emitted
//! as a `STD_SOURCES` table of `(name, source text)` pairs, sorted by name for
//! determinism. The name is the path relative to `std/` with forward slashes
//! (e.g. `datetime/Instant.fai`), so files in subfolders keep a stable,
//! platform-independent, collision-free name. `fai-types` then loads them into
//! the query database as synthetic high-durability inputs.

use std::path::{Path, PathBuf};
use std::{env, fs};

/// Recursively collects every `.fai` file under `dir`, naming each by its path
/// relative to `base` (forward-slashed). Emits a `rerun-if-changed` for each
/// directory and file so adding, removing, or editing a module re-runs the build.
fn collect(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    println!("cargo:rerun-if-changed={}", dir.display());
    for entry in fs::read_dir(dir).expect("read std/ directory") {
        let path = entry.expect("read std/ entry").path();
        if path.is_dir() {
            collect(&path, base, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("fai") {
            let rel = path.strip_prefix(base).expect("std file is under std/");
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_str().expect("UTF-8 path component"))
                .collect::<Vec<_>>()
                .join("/");
            println!("cargo:rerun-if-changed={}", path.display());
            out.push((name, path));
        }
    }
}

fn main() {
    // `crates/fai-types` → workspace root → `std`.
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let std_dir = manifest.join("../../std").canonicalize().expect("std/ directory must exist");

    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    collect(&std_dir, &std_dir, &mut sources);
    // Deterministic order so the embedded table (and load order) never depends on
    // the filesystem's directory iteration order.
    sources.sort_by(|a, b| a.0.cmp(&b.0));

    let mut generated = String::from(
        "/// The embedded standard-library sources: `(file name, source text)`, \
         sorted by file name.\npub static STD_SOURCES: &[(&str, &str)] = &[\n",
    );
    for (name, path) in &sources {
        generated.push_str(&format!(
            "    ({name:?}, include_str!({:?})),\n",
            path.to_str().expect("UTF-8 path")
        ));
    }
    generated.push_str("];\n");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("std_sources.rs");
    fs::write(&out, generated).expect("write generated std_sources.rs");
}

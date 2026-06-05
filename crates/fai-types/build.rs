//! Embeds the standard-library sources into the compiler.
//!
//! The `.fai` modules under the workspace `std/` directory are read at build
//! time and emitted as a `STD_SOURCES` table of `(file name, source text)`
//! pairs, sorted by name for determinism. `fai-types` then loads them into the
//! query database as synthetic high-durability inputs.

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    // `crates/fai-types` → workspace root → `std`.
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let std_dir = manifest.join("../../std").canonicalize().expect("std/ directory must exist");

    println!("cargo:rerun-if-changed={}", std_dir.display());

    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(&std_dir).expect("read std/ directory") {
        let path = entry.expect("read std/ entry").path();
        if path.extension().and_then(|e| e.to_str()) == Some("fai") {
            let name = path.file_name().unwrap().to_str().expect("UTF-8 file name").to_owned();
            println!("cargo:rerun-if-changed={}", path.display());
            sources.push((name, path));
        }
    }
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

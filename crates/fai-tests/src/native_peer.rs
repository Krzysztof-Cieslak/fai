//! Isolated native Rust peers with explicit build and executable identities.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use camino::Utf8PathBuf;
use object::{Object, ObjectSymbol};

use crate::algorithms::{Algorithm, Oracle};
use crate::benchmark_aot::Entry;
use crate::benchmark_contracts::{self, METHODOLOGY};

struct Library {
    directory: tempfile::TempDir,
    archive: PathBuf,
    compiler: OsString,
    version: String,
}

fn library() -> &'static Library {
    static LIBRARY: OnceLock<Library> = OnceLock::new();
    LIBRARY.get_or_init(|| {
        let directory = tempfile::Builder::new().prefix("fai-native-peers-").tempdir().unwrap();
        let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fai-bench-workloads/Cargo.toml");
        let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
        command.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
        command.args(["build", "--release", "--locked", "--lib", "--message-format=json"])
            .arg("--manifest-path").arg(&manifest)
            .arg("--target-dir").arg(directory.path().join("target"));
        let output = command.output().expect("build standalone Rust workload library");
        assert!(output.status.success(), "{command:?}: {}", String::from_utf8_lossy(&output.stderr));
        let records: Vec<serde_json::Value> = String::from_utf8(output.stdout).unwrap().lines()
            .filter_map(|line| serde_json::from_str(line).ok()).collect();
        let record = records.iter().find(|record| record["reason"] == "compiler-artifact" && record["target"]["name"] == "fai_bench_workloads").expect("workload library artifact");
        assert_eq!(record["profile"]["opt_level"], "3", "native peers require opt-level=3");
        assert_eq!(record["profile"]["debug_assertions"], false, "native peers require release assertions");
        assert_eq!(record["profile"]["overflow_checks"], false, "native peers require wrapping release arithmetic");
        let archive = record["filenames"].as_array().unwrap().iter()
            .filter_map(serde_json::Value::as_str).find(|name| name.ends_with(".rlib")).map(PathBuf::from).expect("Rust workload rlib");
        let version = Command::new(&compiler).current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).arg("-vV").output().expect("Rust compiler version");
        assert!(version.status.success(), "Rust compiler version failed");
        let version = String::from_utf8(version.stdout).unwrap();
        std::fs::write(directory.path().join("library-build.json"), serde_json::to_vec_pretty(&serde_json::json!({
            "command": format!("{command:?}"), "compiler": version, "artifact": record,
            "rustflags": std::env::var_os("RUSTFLAGS"), "encodedRustflags": std::env::var_os("CARGO_ENCODED_RUSTFLAGS"),
        })).unwrap()).unwrap();
        Library { directory, archive, compiler, version }
    })
}

/// Builds one statically selected workload, reusing only a compiler-free library.
#[must_use]
pub fn rust(algorithm: &Algorithm, entry: Entry) -> Utf8PathBuf {
    static BINARIES: Mutex<BTreeMap<(String, Entry), Utf8PathBuf>> = Mutex::new(BTreeMap::new());
    let mut binaries = BINARIES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let key = (algorithm.module.to_owned(), entry);
    if let Some(binary) = binaries.get(&key) {
        return binary.clone();
    }
    let library = library();
    let name = format!("{}-{entry:?}", algorithm.module);
    let directory = library.directory.path().join(name);
    std::fs::create_dir_all(&directory).unwrap();
    let source = directory.join("peer.rs");
    std::fs::write(&source, rust_source(algorithm, entry)).unwrap();
    let binary = directory.join(format!("peer{}", std::env::consts::EXE_SUFFIX));
    let mut command = Command::new(&library.compiler);
    command.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    let parent = library.archive.parent().unwrap();
    let dependencies = if parent.file_name().is_some_and(|name| name == "deps") {
        parent.to_path_buf()
    } else {
        parent.join("deps")
    };
    command
        .arg(&source)
        .args([
            "--edition=2024",
            "--crate-name=fai_bench_peer",
            "-C",
            "opt-level=3",
            "-C",
            "debug-assertions=no",
            "-C",
            "overflow-checks=no",
        ])
        .arg("--extern")
        .arg(format!("fai_bench_workloads={}", library.archive.display()))
        .arg("-L")
        .arg(format!("dependency={}", dependencies.display()))
        .arg("-o")
        .arg(&binary);
    let output = command.output().expect("compile isolated Rust peer");
    assert!(output.status.success(), "{command:?}: {}", String::from_utf8_lossy(&output.stderr));
    verify_isolated(&binary).unwrap();
    let contract = benchmark_contracts::for_module(algorithm.module).unwrap();
    std::fs::write(directory.join("compiler-info.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "methodology": METHODOLOGY, "workload": algorithm.module, "workloadVersion": contract.version,
        "hashAlgorithm": "blake3",
        "comparison": contract.comparison.as_str(), "contract": contract.note,
        "compiler": library.version, "command": format!("{command:?}"),
        "sourceHash": hash(&source), "executableHash": hash(&binary), "libraryHash": hash(&library.archive),
        "libraryBuild": serde_json::from_slice::<serde_json::Value>(&std::fs::read(library.directory.path().join("library-build.json")).unwrap()).unwrap(),
    })).unwrap()).unwrap();
    let binary = Utf8PathBuf::from_path_buf(binary).unwrap();
    binaries.insert(key, binary.clone());
    binary
}

/// Source contains a direct function reference, never a workload-name dispatcher.
#[must_use]
pub fn rust_source(algorithm: &Algorithm, entry: Entry) -> String {
    let contract = benchmark_contracts::for_module(algorithm.module).expect("reviewed workload");
    let kind = match algorithm.oracle {
        Oracle::Int(_) => "Int",
        Oracle::Float(_) => "Float",
    };
    let call = match entry {
        Entry::Once => "run_once(oracle)?;",
        Entry::Memory => "memory_process(oracle)?;",
        Entry::Worker => "serve(oracle, std::io::stdin().lock(), std::io::stdout().lock())?;",
    };
    format!(
        "use fai_bench_workloads::{{algorithms::Oracle, benchmark_aot::*}};\nfn main() -> Result<(), Box<dyn std::error::Error>> {{\nlet oracle = Oracle::{kind}(fai_bench_workloads::{});\n{call}\nOk(())\n}}\n",
        contract.rust_entry
    )
}

/// Stable content identity used by benchmark artifacts and comparison records.
#[must_use]
pub fn hash(path: &Path) -> String {
    blake3::hash(&std::fs::read(path).expect("benchmark artifact")).to_hex().to_string()
}

/// Retains an end-to-end executable and its provenance when an artifact
/// directory is configured. The returned path is the executable to measure.
#[must_use]
pub fn retain_once(
    algorithm: &Algorithm,
    side: &str,
    executable: &Path,
    details: serde_json::Value,
) -> Utf8PathBuf {
    let directory = std::env::var_os("FAI_BENCH_ARTIFACT_DIR");
    retain_in(directory.as_deref().map(Path::new), algorithm, side, executable, details)
}

fn retain_in(
    directory: Option<&Path>,
    algorithm: &Algorithm,
    side: &str,
    executable: &Path,
    details: serde_json::Value,
) -> Utf8PathBuf {
    let Some(directory) = directory else {
        return Utf8PathBuf::from_path_buf(executable.to_path_buf())
            .expect("UTF-8 executable path");
    };
    assert!(matches!(side, "fai" | "rust" | "ocaml"), "known benchmark side");
    let folder = std::env::current_dir().unwrap().join(directory).join(algorithm.module);
    std::fs::create_dir_all(&folder).unwrap();
    let destination = folder.join(format!("{side}-once"));
    std::fs::copy(executable, &destination).expect("retain measured executable");
    let contract = benchmark_contracts::for_module(algorithm.module).expect("reviewed workload");
    let metadata = serde_json::json!({"methodology": METHODOLOGY, "algorithm": algorithm.module,
        "workloadVersion": contract.version, "comparison": contract.comparison.as_str(),
        "side": side, "executableHash": hash(&destination), "details": details});
    std::fs::write(
        folder.join(format!("{side}-once.json")),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
    if side == "fai" {
        std::fs::write(
            folder.join("workload.fai"),
            crate::benchmark_aot::fai_program(algorithm, Entry::Once),
        )
        .unwrap();
    } else {
        let name = if side == "rust" { "peer.rs" } else { "baseline.ml" };
        std::fs::copy(
            executable.with_file_name(name),
            folder.join(if side == "rust" { "workload.rs" } else { "workload.ml" }),
        )
        .expect("retain peer source");
    }
    Utf8PathBuf::from_path_buf(destination).expect("UTF-8 artifact path")
}

/// Rejects compiler-linked peers before they can contribute measurements.
pub fn verify_isolated(path: &Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    let file = object::File::parse(bytes.as_slice()).map_err(|error| error.to_string())?;
    if file.symbols().next().is_none() {
        return Err("peer has no inspectable symbol table".into());
    }
    let symbols: Vec<_> = file
        .symbols()
        .chain(file.dynamic_symbols())
        .filter_map(|symbol| {
            let name = symbol.name().ok()?;
            ["fai_tests", "fai_db", "fai_types", "fai_resolve", "fai_syntax", "salsa", "inventory"]
                .iter()
                .any(|forbidden| name.contains(forbidden))
                .then(|| name.to_owned())
        })
        .collect();
    if symbols.is_empty() {
        Ok(())
    } else {
        Err(format!("compiler-linked native peer: {symbols:?}"))
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    #[test]
    fn retained_end_to_end_metadata_identifies_the_actual_executable() {
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("program");
        std::fs::write(&original, b"fixture executable bytes").unwrap();
        let output = directory.path().join("retained");
        let algorithm = crate::algorithms::by_module("Fib").unwrap();
        let retained = retain_in(
            Some(&output),
            algorithm,
            "fai",
            &original,
            serde_json::json!({"compiler": "fixture"}),
        );
        assert_eq!(std::fs::read(&retained).unwrap(), std::fs::read(&original).unwrap());
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output.join("Fib/fai-once.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["executableHash"], hash(retained.as_std_path()));
        assert_eq!(metadata["side"], "fai");
        assert_eq!(metadata["details"]["compiler"], "fixture");
        assert_eq!(
            std::fs::read_to_string(output.join("Fib/workload.fai")).unwrap(),
            crate::benchmark_aot::fai_program(algorithm, Entry::Once)
        );
    }
}

//! Builds the OCaml side of the subprocess runtime/memory comparison.
//!
//! The OCaml baseline (`ocaml/baseline.ml`) is a third delivered binary in the
//! `algorithms_aot`/`algorithms_mem` benches, alongside the Fai `fai build`
//! executable and the Rust `algo-baseline`. It is compiled once with `ocamlopt -O3`
//! into a native executable the benches spawn as `baseline <module> <n>` (the
//! OCaml twin of `algo-baseline`), so the comparison pits a delivered, natively
//! compiled OCaml binary against the Fai and Rust ones.
//!
//! The toolchain is optional: when `ocamlopt` is not on `PATH`, [`baseline`]
//! yields `None`. `FAI_BENCH_OCAMLOPT` selects a particular compiler; an invalid
//! explicit selection fails rather than silently skipping the comparison.
//! The benchmark workflow pins a Flambda-enabled compiler. Each build retains
//! its compiler configuration and flags alongside the executable.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;

/// The OCaml baseline source, embedded so it can be written to a scratch
/// directory and compiled wherever the benches run.
const SOURCE: &str = include_str!("../ocaml/baseline.ml");

/// The compiled OCaml baseline executable, or `None` when `ocamlopt` is
/// unavailable.
///
/// Compiled once per process; subsequent calls return the cached result. Panics
/// if `ocamlopt` is present but the source fails to compile — a real bug, surfaced
/// loudly rather than skipped.
#[must_use]
pub fn baseline() -> Option<&'static Utf8PathBuf> {
    static BASELINE: OnceLock<Option<Utf8PathBuf>> = OnceLock::new();
    BASELINE.get_or_init(|| build("baseline", SOURCE)).as_ref()
}

/// The persistent matched-tree worker and untimed OCaml shape validator.
pub fn tree_baseline() -> Option<&'static Utf8PathBuf> {
    static BASELINE: OnceLock<Option<Utf8PathBuf>> = OnceLock::new();
    BASELINE.get_or_init(|| build("tree_lookup", include_str!("../ocaml/tree_lookup.ml"))).as_ref()
}

/// A persistent native worker with one statically selected registered workload.
#[must_use]
pub fn worker_baseline(algorithm: &crate::algorithms::Algorithm) -> Option<Utf8PathBuf> {
    let definitions = if crate::tail_components::by_module(algorithm.module).is_some() {
        include_str!("../ocaml/tail_components.ml")
    } else {
        SOURCE.split_once("\nlet () =").expect("OCaml baseline entry").0
    };
    let worker = include_str!("../ocaml/worker.ml").replace("HARNESS_MODULE", algorithm.module);
    build(&format!("worker-{}", algorithm.module), &format!("{definitions}\n{worker}"))
}

/// Probes a compiler, allowing only an absent implicit default to be skipped.
fn configuration(compiler: &OsStr, explicit: bool) -> Option<String> {
    let output = match Command::new(compiler).arg("-config").output() {
        Ok(output) => output,
        Err(error) if !explicit && error.kind() == ErrorKind::NotFound => return None,
        Err(error) => panic!("could not run selected OCaml compiler {compiler:?}: {error}"),
    };
    assert!(
        output.status.success(),
        "OCaml compiler {compiler:?} failed to report its configuration:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8(output.stdout).expect("OCaml compiler configuration is UTF-8"))
}

/// Compiles an OCaml benchmark or validation fixture with `-O3` in a scratch
/// directory, retaining compiler metadata beside it. Returns `None` only when
/// the implicit `ocamlopt` is absent; an explicit `FAI_BENCH_OCAMLOPT` must work.
#[must_use]
pub fn build(name: &str, contents: &str) -> Option<Utf8PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let selected = std::env::var_os("FAI_BENCH_OCAMLOPT");
    let compiler = Path::new(selected.as_deref().unwrap_or_else(|| OsStr::new("ocamlopt")));
    // Relative paths containing a directory must survive changing to the build
    // directory. Bare program names still resolve through PATH.
    let compiler = if compiler.components().count() > 1 {
        std::env::current_dir().expect("read working directory").join(compiler)
    } else {
        compiler.to_path_buf()
    };
    let config = configuration(compiler.as_os_str(), selected.is_some())?;
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir().join(format!(
        "fai-ocaml-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )))
    .expect("temp dir is UTF-8");
    std::fs::create_dir_all(&dir).expect("create OCaml scratch dir");
    let source = dir.join("baseline.ml");
    std::fs::write(&source, contents).expect("write OCaml baseline source");
    std::fs::write(
        dir.join("compiler-info.txt"),
        format!(
            "compiler: {compiler:?}\nflags: -O3\nOCAMLPARAM: {:?}\nOCAMLRUNPARAM: {:?}\n{config}",
            std::env::var_os("OCAMLPARAM"),
            std::env::var_os("OCAMLRUNPARAM")
        ),
    )
    .expect("write OCaml compiler configuration");

    // ocamlopt emits its .cmi/.cmx/.o artifacts in the working directory, so
    // compile from the scratch dir to keep them out of the workspace.
    let output = Command::new(&compiler)
        .current_dir(&dir)
        .args(["-O3", "baseline.ml", "-o", "baseline"])
        .output()
        .expect("run ocamlopt");
    assert!(
        output.status.success(),
        "OCaml baseline failed to compile:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(dir.join("baseline"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_implicit_compiler_is_optional() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-ocamlopt");
        assert!(configuration(missing.as_os_str(), false).is_none());
    }

    #[test]
    #[should_panic(expected = "could not run selected OCaml compiler")]
    fn an_absent_explicit_compiler_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing-ocamlopt");
        let _ = configuration(missing.as_os_str(), true);
    }

    #[test]
    fn same_named_builds_keep_their_own_executables() {
        let Some(first) = build("separate", "let () = print_endline \"first\"\n") else { return };
        let second = build("separate", "let () = print_endline \"second\"\n").unwrap();
        assert_ne!(first, second);
        assert_eq!(Command::new(first).output().unwrap().stdout, b"first\n");
        assert_eq!(Command::new(second).output().unwrap().stdout, b"second\n");
    }

    #[track_caller]
    fn validate(name: &str, assertion: &str) {
        let source = format!("{SOURCE}\nlet () = {assertion}\n");
        let Some(binary) = build(name, &source) else { return };
        let output = Command::new(binary).args(["GraphBFS", "4"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.stdout, b"4\n");
    }

    #[test]
    fn bfs_materializes_every_node_and_ordered_edge() {
        validate(
            "graph-shape",
            "let rows = Hashtbl.fold (fun k v acc -> (k, v) :: acc) (graph_adjacency 4) [] in assert (List.sort compare rows = [(0, [1; 1; 2]); (1, [2; 3; 1]); (2, [3; 1; 0]); (3, [0; 3; 3])])",
        );
    }

    #[test]
    fn bfs_traversal_reads_the_stored_graph() {
        validate(
            "graph-traversal",
            "let graph = Hashtbl.create 4 in Hashtbl.add graph 0 [7]; Hashtbl.add graph 7 []; Hashtbl.add graph 10 [11]; assert (graph_reachable graph = 2)",
        );
    }
}

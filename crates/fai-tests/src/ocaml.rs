//! Builds the OCaml side of the subprocess runtime/memory comparison.
//!
//! The OCaml baseline (`ocaml/baseline.ml`) is a third delivered binary in the
//! `algorithms_aot`/`algorithms_mem` benches, alongside the Fai `fai build`
//! executable and the Rust `algo-baseline`. It is compiled once with `ocamlopt`
//! into a native executable the benches spawn as `baseline <module> <n>` (the
//! OCaml twin of `algo-baseline`), so the comparison pits a delivered, natively
//! compiled OCaml binary against the Fai and Rust ones.
//!
//! The toolchain is optional: when `ocamlopt` is not on `PATH`, [`baseline`]
//! yields `None` and the callers skip the OCaml rows, so `cargo bench` works
//! without OCaml installed. The Benchmarks workflow installs OCaml, so the
//! comparison is populated there. A present-but-broken toolchain (the embedded
//! source fails to compile) is a loud panic, not a silent skip.

use std::process::Command;
use std::sync::OnceLock;

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
    BASELINE.get_or_init(build).as_ref()
}

/// Whether an `ocamlopt` native compiler is available on `PATH`.
fn ocamlopt_available() -> bool {
    Command::new("ocamlopt").arg("-version").output().is_ok_and(|out| out.status.success())
}

/// Compiles the embedded OCaml baseline into a native executable in a scratch
/// directory, returning its path (or `None` when `ocamlopt` is absent).
fn build() -> Option<Utf8PathBuf> {
    if !ocamlopt_available() {
        return None;
    }
    let dir = Utf8PathBuf::from_path_buf(
        std::env::temp_dir().join(format!("fai-ocaml-baseline-{}", std::process::id())),
    )
    .expect("temp dir is UTF-8");
    std::fs::create_dir_all(&dir).expect("create OCaml scratch dir");
    let source = dir.join("baseline.ml");
    std::fs::write(&source, SOURCE).expect("write OCaml baseline source");

    // ocamlopt emits its .cmi/.cmx/.o artifacts in the working directory, so
    // compile from the scratch dir to keep them out of the workspace.
    let output = Command::new("ocamlopt")
        .current_dir(&dir)
        .args(["baseline.ml", "-o", "baseline"])
        .output()
        .expect("run ocamlopt");
    assert!(
        output.status.success(),
        "OCaml baseline failed to compile:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(dir.join("baseline"))
}

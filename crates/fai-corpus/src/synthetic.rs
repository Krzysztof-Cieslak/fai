//! A deterministic synthetic-workspace generator for benchmarks and the
//! performance guard tests.
//!
//! The generated corpus is *structurally* deterministic (no randomness), so a
//! given [`CorpusSpec`] always yields byte-identical sources. It is shaped to
//! exercise the parts of the front end whose cost and incrementality we care
//! about:
//!
//! * a shared `Core` module of public, **signatured** functions that every other
//!   module references via a qualified `Core.fN` call — exercising the
//!   cross-module firewall (a private edit elsewhere must not re-check these);
//! * many leaf modules, each mixing public signatured functions with **private,
//!   signature-less** helpers — exercising per-def inference, SCCs, and early
//!   cutoff.
//!
//! When [`CorpusSpec::contracts_per_module`] is non-zero each leaf module also
//! carries `example`/`forall` contracts over its own public functions, so the
//! same generator drives the `fai test` (edit → re-run contracts) benchmarks.
//!
//! The [`edit_*`](edit_private_body) helpers define, in one place, what a given
//! kind of edit looks like, so benches and guards agree on it.

use camino::Utf8PathBuf;
use fai_db::{Db, FaiDatabase, SourceFile};

/// Parameters controlling the size and shape of a generated corpus.
#[derive(Debug, Clone, Copy)]
pub struct CorpusSpec {
    /// Number of leaf modules (in addition to the shared `Core` module).
    pub modules: usize,
    /// Public signatured functions generated per leaf module.
    pub public_defs_per_module: usize,
    /// Private signature-less helpers generated per leaf module.
    pub private_defs_per_module: usize,
    /// Nesting depth of each generated function body (number of chained `let`s).
    pub body_depth: usize,
    /// How many public functions the shared `Core` module exposes.
    pub core_defs: usize,
    /// Contracts (`example`/`forall`) generated per leaf module. Zero leaves the
    /// corpus contract-free (the default for the inference benches and guards).
    pub contracts_per_module: usize,
}

impl CorpusSpec {
    /// A tiny corpus (fast; for unit-style guards).
    #[must_use]
    pub fn tiny() -> Self {
        Self {
            modules: 4,
            public_defs_per_module: 2,
            private_defs_per_module: 1,
            body_depth: 2,
            core_defs: 4,
            contracts_per_module: 0,
        }
    }

    /// A corpus sized by a single "number of leaf modules" knob, with a fixed
    /// per-module shape — convenient for scaling benches/guards.
    #[must_use]
    pub fn with_modules(modules: usize) -> Self {
        Self {
            modules,
            public_defs_per_module: 4,
            private_defs_per_module: 2,
            body_depth: 3,
            core_defs: 8,
            contracts_per_module: 0,
        }
    }

    /// Like [`with_modules`](Self::with_modules), but every leaf module also
    /// carries one `example` and one `forall` contract over its own public
    /// functions — the corpus the `fai test` benchmarks edit and re-run.
    #[must_use]
    pub fn with_modules_and_contracts(modules: usize) -> Self {
        Self { contracts_per_module: 2, ..Self::with_modules(modules) }
    }

    /// The total number of top-level value bindings the spec generates.
    #[must_use]
    pub fn total_defs(self) -> usize {
        self.core_defs + self.modules * (self.public_defs_per_module + self.private_defs_per_module)
    }
}

/// The shared `Core` module's source: `core_defs` public signatured functions.
fn core_source(spec: &CorpusSpec) -> String {
    let mut s = String::from("module Core\n\n");
    for i in 0..spec.core_defs {
        // A simple, fully-typed Int -> Int function.
        s.push_str(&format!("public f{i} : Int -> Int\n"));
        s.push_str(&format!("let f{i} x = x + {i}\n\n"));
    }
    s
}

/// The name of the `index`-th leaf module.
fn module_name(index: usize) -> String {
    format!("M{index}")
}

/// One leaf module's source.
fn leaf_source(spec: &CorpusSpec, index: usize) -> String {
    let name = module_name(index);
    let mut s = format!("module {name}\n\n");

    // Public signatured functions. Each one references the shared `Core` module
    // (via a qualified call) so the dependency graph has real cross-module edges.
    for i in 0..spec.public_defs_per_module {
        let core_target = i % spec.core_defs.max(1);
        s.push_str(&format!("public g{i} : Int -> Int\n"));
        s.push_str(&format!("let g{i} x =\n"));
        s.push_str(&body(spec.body_depth, &format!("Core.f{core_target} x")));
    }

    // Private, signature-less helpers (inference must compute their types).
    for i in 0..spec.private_defs_per_module {
        s.push_str(&format!("let h{i} x =\n"));
        s.push_str(&body(spec.body_depth, "x + 1"));
    }

    // Contracts over this module's own public functions. `g{j} x = Core.f{j %
    // core_defs} x = x + (j % core_defs)`, so `g{j} 0 = j % core_defs` is an
    // exact `example`; the `forall` is reflexive (always true). Both stay green
    // under the value-preserving edits below.
    for k in 0..spec.contracts_per_module {
        // Pair an example and a forall on the same function (so the value-
        // preserving `edit_public_body` to `g0` re-synthesizes both).
        let j = (k / 2) % spec.public_defs_per_module.max(1);
        if k % 2 == 0 {
            let expected = j % spec.core_defs.max(1);
            s.push_str(&format!("example: g{j} 0 = {expected}\n"));
        } else {
            s.push_str(&format!("forall x: g{j} x = g{j} x\n"));
        }
    }

    s
}

/// A function body: `body_depth` chained `let`s ending in `tail`, indented two
/// spaces (the offside block under `let name x =`).
fn body(depth: usize, tail: &str) -> String {
    let mut s = String::new();
    let mut prev = String::from("x");
    for d in 0..depth {
        s.push_str(&format!("  let t{d} = {prev} + {d}\n"));
        prev = format!("t{d}");
    }
    s.push_str(&format!("  {tail} + {prev} - {prev}\n\n"));
    s
}

/// Generates the corpus as `(path, source)` pairs, `Core.fai` first.
#[must_use]
pub fn generate(spec: &CorpusSpec) -> Vec<(String, String)> {
    let mut files = Vec::with_capacity(spec.modules + 1);
    files.push(("Core.fai".to_owned(), core_source(spec)));
    for index in 0..spec.modules {
        files.push((format!("{}.fai", module_name(index)), leaf_source(spec, index)));
    }
    files
}

/// Builds a database seeded with the generated corpus (plus the prelude),
/// returning it and the leaf [`SourceFile`]s in generation order (`Core` first).
#[must_use]
pub fn build_db(spec: &CorpusSpec) -> (FaiDatabase, Vec<SourceFile>) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let mut files = Vec::new();
    for (path, source) in generate(spec) {
        let id = db.add_source(Utf8PathBuf::from(path), source);
        files.push(db.source_file(id).unwrap());
    }
    (db, files)
}

// ── Edits (shared by benches and guards) ─────────────────────────────────────

/// A new source for leaf module `index` with private helper `h0`'s body changed
/// (a private-body edit: must not re-check other modules).
#[must_use]
pub fn edit_private_body(spec: &CorpusSpec, index: usize, revision: u32) -> String {
    let original = leaf_source(spec, index);
    // The first private helper's tail starts with `x + 1 +` (unique to private
    // helpers). Splice a value-preserving term so the body genuinely changes,
    // forcing re-inference of just that def's chain.
    assert!(spec.private_defs_per_module >= 1, "need a private helper to edit");
    original.replacen("x + 1 +", &format!("x + 1 + {revision} - {revision} +"), 1)
}

/// A new source for leaf module `index` with public function `g0`'s body changed
/// in a value-preserving way (a public-body edit: re-lowers `g0` and forces its
/// contracts to be re-synthesized, while keeping them green). This is the edit
/// the `fai test` (edit → re-run contracts) benchmarks apply.
#[must_use]
pub fn edit_public_body(spec: &CorpusSpec, index: usize, revision: u32) -> String {
    let original = leaf_source(spec, index);
    // The first chained `let` of every body is `  let t0 = x + 0`; the first
    // occurrence belongs to `g0` (public functions are generated first). Adding
    // `+ R - R` preserves the value, so `g0 0` is unchanged and its contracts
    // still hold.
    original.replacen(
        "  let t0 = x + 0\n",
        &format!("  let t0 = x + 0 + {revision} - {revision}\n"),
        1,
    )
}

/// A new source for the shared `Core` module with `f0`'s *signature* changed
/// (a public-signature edit: must invalidate dependents).
#[must_use]
pub fn edit_core_signature(spec: &CorpusSpec) -> String {
    core_source(spec).replacen(
        "public f0 : Int -> Int\nlet f0 x = x + 0\n",
        "public f0 : Int -> Int -> Int\nlet f0 x = x + 0\n",
        1,
    )
}

/// A new source for leaf module `index` with a comment inserted (a trivia edit:
/// must trigger zero semantic recompute via early cutoff at the item tree).
#[must_use]
pub fn edit_comment(spec: &CorpusSpec, index: usize, revision: u32) -> String {
    let original = leaf_source(spec, index);
    let name = module_name(index);
    original.replacen(
        &format!("module {name}\n"),
        &format!("module {name}\n// trivia revision {revision}\n"),
        1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_change_the_source() {
        let spec = CorpusSpec::tiny();
        assert_ne!(leaf_source(&spec, 0), edit_private_body(&spec, 0, 1));
        assert_ne!(core_source(&spec), edit_core_signature(&spec));
        assert_ne!(leaf_source(&spec, 0), edit_comment(&spec, 0, 1));
    }

    #[test]
    fn public_body_edit_changes_source_but_keeps_g0_value() {
        let spec = CorpusSpec::with_modules_and_contracts(4);
        let edited = edit_public_body(&spec, 0, 7);
        assert_ne!(leaf_source(&spec, 0), edited);
        // The splice is value-preserving, so `g0`'s `example` literal is intact.
        assert!(edited.contains("example: g0 0 = 0"));
        assert!(edited.contains("let t0 = x + 0 + 7 - 7"));
    }

    #[test]
    fn contracts_are_emitted_only_when_requested() {
        assert!(!leaf_source(&CorpusSpec::with_modules(4), 0).contains("example:"));
        let with = leaf_source(&CorpusSpec::with_modules_and_contracts(4), 0);
        assert!(with.contains("example: g0 0 = 0"));
        assert!(with.contains("forall x: g0 x = g0 x"));
    }

    #[test]
    fn generation_is_deterministic() {
        let spec = CorpusSpec::with_modules(5);
        assert_eq!(generate(&spec), generate(&spec));
        let spec = CorpusSpec::with_modules_and_contracts(5);
        assert_eq!(generate(&spec), generate(&spec));
    }
}

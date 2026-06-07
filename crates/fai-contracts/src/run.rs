//! Running a compiled contract harness in-process.
//!
//! The driver compiles the synthesized harness ([`crate::synthesize`]) into a JIT
//! image and hands us the entry's closure value; [`run_contract`] applies it with
//! `(seed, trials, maxSize)` and decodes the `Test.TestResult` it returns
//! (`Passed` / `Failed counterexample`). Every value is released, so the runtime's
//! live-object count returns to its baseline (a soundness guard the driver
//! asserts after a run).

use fai_runtime::{Value, apply, data_tag_of, fai_data_field, fai_drop, make_int, read_string};

/// The `Test.TestResult` tag for `Passed` (declaration order: `Passed | Failed`).
const PASSED_TAG: i64 = 0;

/// The result of running one contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractOutcome {
    /// Whether the contract held.
    pub passed: bool,
    /// The rendered counterexample for a failing `forall` (empty for an example).
    pub counterexample: Option<String>,
}

/// Applies a contract harness `entry` (a closure value of arity 3) with the given
/// configuration and decodes its `TestResult`.
#[must_use]
pub fn run_contract(entry: Value, seed: i64, trials: i64, max_size: i64) -> ContractOutcome {
    let args = [make_int(seed), make_int(trials), make_int(max_size)];
    let result = apply(entry, &args);
    let outcome = if data_tag_of(result) == PASSED_TAG {
        ContractOutcome { passed: true, counterexample: None }
    } else {
        // `Failed String`: field 0 is the rendered counterexample (empty for a
        // failed example).
        let shown = fai_data_field(result, 0);
        let bytes = read_string(shown);
        fai_drop(shown);
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let counterexample = if text.is_empty() { None } else { Some(text) };
        ContractOutcome { passed: false, counterexample }
    };
    fai_drop(result);
    outcome
}

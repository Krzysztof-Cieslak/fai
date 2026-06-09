//! Contracts: collecting, synthesizing, and running `example`/`forall` checks.
//!
//! [`contracts`] enumerates a file's contract declarations (with the binder
//! names, the subject binding they describe, and their source text). [`synth`]
//! turns one contract into a pair of lowered definitions — a *property* function
//! and a zero-or-three-argument *harness* entry that calls the standard library's
//! `Test` driver with a type-directed `Arbitrary` composed from `Test`
//! combinators. [`run`] applies a compiled harness and decodes its `TestResult`.
//!
//! Diagnostics live in the `FAI6xxx` range: [`CONTRACT_FAILED`] (an `example`
//! that did not hold / a `forall` counterexample), [`CONTRACT_NOT_RUNNABLE`]
//! (a contract whose binders cannot be generated, e.g. a function-typed binder),
//! and [`CONTRACT_IMPURE`] (a contract body that references a host capability).

mod arb;
mod run;
mod synth;

use fai_db::{Db, SourceFile};
use fai_diagnostics::{CodeInfo, DiagnosticCode, Severity};
use fai_span::TextRange;
use fai_syntax::Symbol;
use fai_syntax::ast::{ItemKind, PatKind};

pub use run::{ContractOutcome, run_contract};
pub use synth::{NotRunnable, SynthContract, synthesize};

/// A contract body references a host capability and so is impure. The constant
/// is defined in `fai-diagnostics` (it is emitted by the type checker, which
/// cannot depend on this crate) and re-exported here so the contracts layer
/// owns every `FAI6xxx` name; its catalog entry is in [`CODES`].
pub use fai_diagnostics::CONTRACT_IMPURE;

/// A contract failed: an `example` evaluated to false, or a `forall` found a
/// counterexample.
pub const CONTRACT_FAILED: DiagnosticCode = DiagnosticCode::new("FAI6001");

/// A contract cannot be run: a binder's type has no value generator (a
/// function-typed binder, an unsupported type, or too many binders).
pub const CONTRACT_NOT_RUNNABLE: DiagnosticCode = DiagnosticCode::new("FAI6002");

/// A contract aborted at runtime before producing a result: a generated input
/// drove the body into a runtime trap (e.g. division by zero), or it did not
/// finish within the time limit.
pub const CONTRACT_ABORTED: DiagnosticCode = DiagnosticCode::new("FAI6003");

/// A contract cannot be run: a binder's type has no finite value, so generation
/// would never terminate (every constructor is recursive, with no base case).
pub const CONTRACT_NON_GROUNDABLE: DiagnosticCode = DiagnosticCode::new("FAI6005");

/// A contract cannot be run: more than one user-defined `Arbitrary` generator
/// matches a binder's type, so the override to use is ambiguous.
pub const CONTRACT_AMBIGUOUS_GENERATOR: DiagnosticCode = DiagnosticCode::new("FAI6006");

/// Diagnostic codes owned by the contracts layer (the `FAI6xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: CONTRACT_FAILED,
        title: "contract failed",
        default_severity: Severity::Error,
        explanation: "An `example`/`forall` contract did not hold. `fai check` evaluates closed \
                      `example` contracts and reports a failing one here; `fai test` runs the rest \
                      (every `example` and `forall`), reporting a `forall` failure with the shrunk \
                      counterexample (binder names and rendered values) in the help.",
    },
    CodeInfo {
        code: CONTRACT_NOT_RUNNABLE,
        title: "contract cannot be run",
        default_severity: Severity::Error,
        explanation: "A contract cannot be exercised because a binder's type has no value \
                      generator — a function-typed binder, a row-polymorphic (open) record, or \
                      too many binders.",
    },
    CodeInfo {
        code: CONTRACT_ABORTED,
        title: "contract aborted at runtime",
        default_severity: Severity::Error,
        explanation: "The contract aborted while being checked: a generated input drove the body \
                      into a runtime trap (e.g. integer division by zero), or it did not finish \
                      within the time limit. Each contract runs in an isolated worker, so the \
                      abort fails only this contract — the rest of the run continues.",
    },
    CodeInfo {
        code: CONTRACT_IMPURE,
        title: "impure contract",
        default_severity: Severity::Error,
        explanation: "An `example`/`forall` contract references a host capability — `Console`, \
                      `Clock`, `Random`, `FileSystem`, `Env`, or the `Runtime` that bundles them. \
                      Contracts are checked by `fai check` and run by `fai test`, so they must be \
                      deterministic and pure and cannot reach a capability. Express the law over \
                      pure values instead.",
    },
    CodeInfo {
        code: CONTRACT_NON_GROUNDABLE,
        title: "binder type has no finite value",
        default_severity: Severity::Error,
        explanation: "A `forall` binder's type cannot be generated because it has no finite value: \
                      every constructor is recursive, with no base case to terminate generation \
                      (e.g. `type S = Cons Int S`, or a mutually-recursive group where no member \
                      bottoms out). Add a non-recursive constructor, or supply a custom \
                      `Arbitrary` for the type.",
    },
    CodeInfo {
        code: CONTRACT_AMBIGUOUS_GENERATOR,
        title: "ambiguous custom generator",
        default_severity: Severity::Error,
        explanation: "More than one top-level `Arbitrary` value matches a binder's type, so which \
                      one overrides the synthesized generator is ambiguous. Keep a single \
                      `Arbitrary` for the type in the contract's module.",
    },
];

/// Whether a contract is an `example` or a `forall`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContractKind {
    /// A constant `example: e`.
    Example,
    /// A universally-quantified `forall binders: e`.
    Forall,
}

impl ContractKind {
    /// The keyword spelling.
    #[must_use]
    pub fn keyword(self) -> &'static str {
        match self {
            ContractKind::Example => "example",
            ContractKind::Forall => "forall",
        }
    }
}

/// A collected contract declaration.
#[derive(Debug, Clone)]
pub struct ContractInfo {
    /// Position among the file's contracts (the key for the lowering queries).
    pub ordinal: usize,
    /// `example` or `forall`.
    pub kind: ContractKind,
    /// The `forall` binder names, in order (empty for an `example`).
    pub binders: Vec<Symbol>,
    /// The top-level binding this contract describes (the most recent one above
    /// it), if any.
    pub subject: Option<Symbol>,
    /// The contract's source span.
    pub span: TextRange,
    /// The contract's source text.
    pub source: String,
}

/// Collects every contract in `file`, in source order, each tagged with the
/// binding it describes (the nearest preceding top-level value binding).
#[must_use]
pub fn contracts(db: &dyn Db, file: SourceFile) -> Vec<ContractInfo> {
    let parsed = fai_syntax::parse(db, file);
    let module = &parsed.module;
    let text = file.text(db);

    let mut out = Vec::new();
    let mut subject: Option<Symbol> = None;
    let mut ordinal = 0;
    for item in &module.items {
        match &item.kind {
            ItemKind::Binding { name, .. } => subject = Some(*name),
            ItemKind::Example { .. } | ItemKind::Forall { .. } => {
                let (kind, binders) = match &item.kind {
                    ItemKind::Example { .. } => (ContractKind::Example, Vec::new()),
                    ItemKind::Forall { binders, .. } => (
                        ContractKind::Forall,
                        binders.iter().filter_map(|&p| binder_name(module, p)).collect(),
                    ),
                    _ => unreachable!(),
                };
                let span = item.span;
                let source = slice(text, span);
                out.push(ContractInfo { ordinal, kind, binders, subject, span, source });
                ordinal += 1;
            }
            _ => {}
        }
    }
    out
}

/// The variable name bound by a `forall` binder pattern (always a `Var`).
fn binder_name(module: &fai_syntax::ast::Module, pat: fai_syntax::ast::PatId) -> Option<Symbol> {
    match module.pat(pat).kind {
        PatKind::Var(name) => Some(name),
        _ => None,
    }
}

/// The source text of a span.
fn slice(text: &str, span: TextRange) -> String {
    let start = span.start().raw() as usize;
    let end = span.end().raw() as usize;
    text.get(start..end).unwrap_or("").to_owned()
}

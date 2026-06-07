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
//! that did not hold / a `forall` counterexample) and [`CONTRACT_NOT_RUNNABLE`]
//! (a contract whose binders cannot be generated, e.g. a function-typed binder).

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

/// A contract failed: an `example` evaluated to false, or a `forall` found a
/// counterexample.
pub const CONTRACT_FAILED: DiagnosticCode = DiagnosticCode::new("FAI6001");

/// A contract cannot be run: a binder's type has no value generator (a
/// function-typed binder, an unsupported type, or too many binders).
pub const CONTRACT_NOT_RUNNABLE: DiagnosticCode = DiagnosticCode::new("FAI6002");

/// Diagnostic codes owned by the contracts layer (the `FAI6xxx` range).
pub const CODES: &[CodeInfo] = &[
    CodeInfo {
        code: CONTRACT_FAILED,
        title: "contract failed",
        default_severity: Severity::Error,
        explanation: "An `example`/`forall` contract did not hold when `fai test` ran it. The \
                      help shows the shrunk counterexample (binder names and rendered values).",
    },
    CodeInfo {
        code: CONTRACT_NOT_RUNNABLE,
        title: "contract cannot be run",
        default_severity: Severity::Error,
        explanation: "A contract cannot be exercised because a binder's type has no value \
                      generator — a function-typed binder, an unsupported type (e.g. `Char`), or \
                      too many binders.",
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

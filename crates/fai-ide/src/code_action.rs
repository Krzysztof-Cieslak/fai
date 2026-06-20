//! Quick fixes / code actions at a range (LSP `textDocument/codeAction`).
//!
//! Two sources feed the actions offered for the diagnostics overlapping a range:
//!
//! - **machine-applicable suggestions** a diagnostic already carries (e.g. the
//!   inferred signature for a `public` binding that lacks one) become a one-edit
//!   quick fix; and
//! - **name-resolution failures** (an unbound or ambiguous bare name) are turned
//!   into "qualify as `Module.name`" fixes, one per module that publicly exports
//!   that name — the qualified form Fai requires for cross-module access.

use fai_db::{Db, SourceFile};
use fai_diagnostics::Diagnostic;
use fai_span::{SpanResolver, TextRange};
use fai_syntax::Symbol;
use serde::Serialize;

use crate::repr::SpanJson;
use crate::target::module_label;

/// A single text replacement within a file.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CodeActionEdit {
    /// The span to replace.
    pub span: SpanJson,
    /// The replacement text.
    pub new_text: String,
}

/// One offered code action: a titled set of edits.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CodeAction {
    /// The human-readable action title.
    pub title: String,
    /// The action kind (always `"quickfix"` for now).
    pub kind: String,
    /// The edits the action applies.
    pub edits: Vec<CodeActionEdit>,
}

/// The code actions offered for the diagnostics overlapping `[start, end)` in
/// `file`. `files` bounds the workspace searched for qualification candidates.
#[must_use]
pub fn code_actions_at(
    db: &dyn Db,
    files: &[SourceFile],
    file: SourceFile,
    start: u32,
    end: u32,
    resolver: &dyn SpanResolver,
) -> Vec<CodeAction> {
    let mut actions = Vec::new();
    for diag in file_diagnostics(db, file) {
        if !overlaps(diag.primary.range(), start, end) {
            continue;
        }
        // Each machine-applicable suggestion is a one-edit quick fix.
        for suggestion in &diag.suggestions {
            if let Some(span) = SpanJson::resolve(suggestion.span, resolver) {
                actions.push(CodeAction {
                    title: suggestion_title(diag.code.as_str()),
                    kind: "quickfix".to_owned(),
                    edits: vec![CodeActionEdit { span, new_text: suggestion.replacement.clone() }],
                });
            }
        }
        // An unbound or ambiguous name can be fixed by qualifying it.
        if matches!(diag.code.as_str(), "FAI2001" | "FAI2002") {
            actions.extend(qualify_actions(db, files, file, &diag, resolver));
        }
    }
    actions
}

/// A title for a suggestion-derived action, specialized by diagnostic code.
fn suggestion_title(code: &str) -> String {
    match code {
        "FAI3003" => "Add the inferred signature".to_owned(),
        _ => "Apply suggested fix".to_owned(),
    }
}

/// "Qualify as `Module.name`" actions for the bare name a resolution diagnostic
/// points at — one per module (standard library included, the prelude-private
/// `Prim` excluded) that publicly exports a value or constructor of that name.
fn qualify_actions(
    db: &dyn Db,
    _files: &[SourceFile],
    file: SourceFile,
    diag: &Diagnostic,
    resolver: &dyn SpanResolver,
) -> Vec<CodeAction> {
    let range = diag.primary.range();
    let text = file.text(db);
    let Some(name) = text.get(range.start().to_usize()..range.end().to_usize()) else {
        return Vec::new();
    };
    let sym = Symbol::intern(name);
    let Some(span) = SpanJson::resolve(diag.primary, resolver) else {
        return Vec::new();
    };

    let mut modules: Vec<String> = Vec::new();
    for candidate in db.all_source_files() {
        if candidate.source(db) == file.source(db) {
            continue; // the name is unbound *here*, so this file is no candidate
        }
        let label = module_label(db, candidate);
        if label == fai_resolve::intrinsics::PRIM_MODULE {
            continue; // `Prim` is reachable only inside the standard library
        }
        let interface = fai_resolve::module_interface(db, candidate);
        let mut found = interface.get(sym).is_some() || interface.has_ctor(sym);
        // An `internal` member is qualifiable only from a same-origin file
        // (std vs. user today), matching what name resolution will accept.
        if !found && fai_db::is_std_path(file.path(db)) == fai_db::is_std_path(candidate.path(db)) {
            let internal = fai_resolve::module_internal_interface(db, candidate);
            found = internal.get(sym).is_some() || internal.has_ctor(sym);
        }
        if found {
            modules.push(label);
        }
    }
    modules.sort();
    modules.dedup();

    modules
        .into_iter()
        .map(|module| CodeAction {
            title: format!("Qualify as `{module}.{name}`"),
            kind: "quickfix".to_owned(),
            edits: vec![CodeActionEdit {
                span: span.clone(),
                new_text: format!("{module}.{name}"),
            }],
        })
        .collect()
}

/// Whether `range` overlaps the half-open `[start, end)` (a zero-width request
/// range at a position still matches the diagnostic it sits within).
fn overlaps(range: TextRange, start: u32, end: u32) -> bool {
    range.start().raw() <= end && start <= range.end().raw()
}

/// The diagnostics whose primary span lies in `file` (parse, resolution, and
/// type-check phases), gathered from the salsa accumulators — the same set
/// `fai check` reports, so the suggestions match.
fn file_diagnostics(db: &dyn Db, file: SourceFile) -> Vec<Diagnostic> {
    let source = file.source(db);
    let mut out: Vec<Diagnostic> = Vec::new();
    out.extend(
        fai_syntax::parse::accumulated::<fai_db::Diag>(db, file).into_iter().map(|d| d.0.clone()),
    );
    out.extend(
        fai_resolve::resolve::accumulated::<fai_db::Diag>(db, file)
            .into_iter()
            .map(|d| d.0.clone()),
    );
    out.extend(
        fai_types::check_file::accumulated::<fai_db::Diag>(db, file)
            .into_iter()
            .map(|d| d.0.clone()),
    );
    out.retain(|d| d.primary.source() == source);
    out
}

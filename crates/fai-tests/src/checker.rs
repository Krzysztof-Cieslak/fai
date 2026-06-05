//! A reusable harness for type-checking whole `.fai` sources in tests.
//!
//! Two entry points:
//!
//! * [`check_source`] — load a single module, run resolution + inference +
//!   contracts, and return a [`CheckOutcome`] (per-binding rendered types and the
//!   file's own error/warning diagnostics).
//! * [`run_annotated`] — parse inline expectation annotations from a `.fai` source
//!   and assert them, so fixture files are self-describing.
//!
//! ## Annotation format
//!
//! Annotations live in line comments anywhere in the file:
//!
//! ```text
//! //~ TYPE name : Int -> Int      -- assert binding `name`'s type renders thus
//! //~ LOCAL f.x : Int             -- assert the inferred type of local `x` in `f`
//! //~ ERROR FAI3004               -- assert at least one error with this code
//! //~ WARN  FAI2010               -- assert at least one warning with this code
//! //~ COUNT ERROR 2               -- assert the exact number of error diagnostics
//! //~ CLEAN                       -- assert the file has no error diagnostics
//! ```
//!
//! `LOCAL` assertions check **inferred** types of parameters, `let`-bound
//! locals, and lambda binders inside a function body — the part of inference
//! that public-signature assertions (which report the *declared* scheme) miss.
//!
//! A file with no `ERROR`/`COUNT`/`CLEAN` annotation is required to be clean.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_diagnostics::{Diagnostic, Severity};
use fai_syntax::Symbol;

/// The result of type-checking one module source.
pub struct CheckOutcome {
    /// The database the source was loaded into (kept alive for further queries).
    pub db: FaiDatabase,
    /// The checked file.
    pub file: SourceFile,
    /// Rendered type scheme of every top-level binding, by name.
    pub types: BTreeMap<String, String>,
    /// Error- and warning-severity diagnostics that belong to this file.
    pub diagnostics: Vec<Diagnostic>,
}

impl CheckOutcome {
    /// The codes of the file's diagnostics, in emission order.
    #[must_use]
    pub fn codes(&self) -> Vec<String> {
        self.diagnostics.iter().map(|d| d.code.as_str().to_owned()).collect()
    }

    /// The number of error-severity diagnostics.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.diagnostics.iter().filter(|d| d.severity == Severity::Error).count()
    }

    /// Whether the file has any error-severity diagnostic.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.error_count() > 0
    }

    /// Whether a diagnostic with `code` was reported.
    #[must_use]
    pub fn has_code(&self, code: &str) -> bool {
        self.diagnostics.iter().any(|d| d.code.as_str() == code)
    }

    /// The inferred type of local `var` in function `func`'s body, rendered with
    /// canonical names in isolation. `None` if the function or local is absent.
    #[must_use]
    pub fn local_type(&self, func: &str, var: &str) -> Option<String> {
        fai_types::def_local_types(&self.db, self.file, Symbol::intern(func))
            .into_iter()
            .find(|(name, _)| name == var)
            .map(|(_, ty)| fai_types::render_canonical(&ty))
    }
}

/// Loads `source` as a single module (alongside the embedded prelude) and checks
/// it, returning the per-binding types and this file's diagnostics.
#[must_use]
pub fn check_source(source: &str) -> CheckOutcome {
    check_named("Test.fai", &[("Test.fai", source)])
}

/// Loads a multi-file workspace and checks the file named `primary`.
#[must_use]
pub fn check_named(primary: &str, files: &[(&str, &str)]) -> CheckOutcome {
    let mut db = FaiDatabase::new();
    fai_types::prelude::load_prelude(&mut db);
    let mut primary_id = None;
    for (path, text) in files {
        let id = db.add_source(Utf8PathBuf::from(*path), (*text).to_owned());
        if *path == primary {
            primary_id = Some(id);
        }
    }
    let file = db.source_file(primary_id.expect("primary file present")).unwrap();
    let source_id = file.source(&db);

    // Per-binding types.
    let defs = fai_resolve::module_defs(&db, file);
    let mut types = BTreeMap::new();
    for d in &defs.defs {
        let scheme = fai_types::def_type(&db, file, d.name);
        types.insert(d.name.as_str().to_owned(), fai_types::render_scheme(&scheme));
    }

    // This file's own diagnostics (resolution + types), de-duplicated.
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    for diag in fai_syntax::parse::accumulated::<Diag>(&db, file) {
        if diag.0.primary.source() == source_id {
            diagnostics.push(diag.0.clone());
        }
    }
    for diag in fai_resolve::resolve::accumulated::<Diag>(&db, file) {
        if diag.0.primary.source() == source_id {
            diagnostics.push(diag.0.clone());
        }
    }
    for diag in fai_types::check_file::accumulated::<Diag>(&db, file) {
        if diag.0.primary.source() == source_id {
            diagnostics.push(diag.0.clone());
        }
    }
    dedup(&mut diagnostics);

    CheckOutcome { db, file, types, diagnostics }
}

fn dedup(diagnostics: &mut Vec<Diagnostic>) {
    let mut seen = std::collections::HashSet::new();
    diagnostics.retain(|d| {
        seen.insert((
            d.code.as_str().to_owned(),
            d.primary.start().raw(),
            d.primary.end().raw(),
            d.message.clone(),
        ))
    });
}

/// The rendered type of a single binding (panics if absent).
#[must_use]
pub fn type_of(source: &str, name: &str) -> String {
    let outcome = check_source(source);
    outcome
        .types
        .get(name)
        .cloned()
        .unwrap_or_else(|| panic!("no binding `{name}` (have {:?})", outcome.types.keys()))
}

/// The inferred types of the *local* bindings in `fn_name`'s body, keyed by
/// variable name. Exercises local inference directly (parameters, `let` locals,
/// lambda binders), independent of any declared signature.
///
/// The rendered types share **one** variable numbering, so a variable shared
/// between locals (e.g. tuple-destructuring components, `p : 'a * 'b` with
/// `a : 'a` and `b : 'b`) renders consistently. To read a single local's type in
/// isolation (with canonical `'a`-first naming) use [`local_type`].
///
/// Loads `source` as a single module alongside the prelude. If a local name is
/// shadowed, the last binding with that name wins; tests should use distinct
/// names to inspect each.
#[must_use]
pub fn local_types(source: &str, fn_name: &str) -> std::collections::BTreeMap<String, String> {
    raw_local_types(source, fn_name)
        .into_iter()
        .map(|(name, ty)| (name, fai_types::render(&ty, &fai_types::VarNames::new())))
        .collect()
}

/// The inferred type of one local variable in `fn_name`'s body, rendered with
/// canonical names in isolation (panics if absent).
#[must_use]
pub fn local_type(source: &str, fn_name: &str, local: &str) -> String {
    let locals = raw_local_types(source, fn_name);
    locals
        .iter()
        .find(|(name, _)| name == local)
        .map(|(_, ty)| fai_types::render_canonical(ty))
        .unwrap_or_else(|| {
            let names: Vec<&str> = locals.iter().map(|(n, _)| n.as_str()).collect();
            panic!("no local `{local}` in `{fn_name}` (have {names:?})")
        })
}

/// The raw `(name, Ty)` locals for `fn_name`, sharing one variable numbering.
fn raw_local_types(source: &str, fn_name: &str) -> Vec<(String, fai_types::Ty)> {
    let mut db = FaiDatabase::new();
    fai_types::prelude::load_prelude(&mut db);
    let id = db.add_source("Test.fai".into(), source.to_owned());
    let file = db.source_file(id).unwrap();
    fai_types::def_local_types(&db, file, Symbol::intern(fn_name))
}

/// A parsed expectation annotation.
#[derive(Debug, PartialEq, Eq)]
enum Expect {
    Type { name: String, rendered: String },
    Local { func: String, var: String, rendered: String },
    Error(String),
    Warn(String),
    Count { severity: Severity, n: usize },
    Clean,
}

fn parse_annotations(source: &str) -> Vec<Expect> {
    let mut out = Vec::new();
    for line in source.lines() {
        let Some(idx) = line.find("//~") else { continue };
        let rest = line[idx + 3..].trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let kind = parts.next().unwrap_or("").trim();
        let arg = parts.next().unwrap_or("").trim();
        match kind {
            "TYPE" => {
                if let Some((name, rendered)) = arg.split_once(':') {
                    out.push(Expect::Type {
                        name: name.trim().to_owned(),
                        rendered: rendered.trim().to_owned(),
                    });
                }
            }
            "LOCAL" => {
                if let Some((path, rendered)) = arg.split_once(':')
                    && let Some((func, var)) = path.trim().split_once('.')
                {
                    out.push(Expect::Local {
                        func: func.trim().to_owned(),
                        var: var.trim().to_owned(),
                        rendered: rendered.trim().to_owned(),
                    });
                }
            }
            "ERROR" => out.push(Expect::Error(arg.to_owned())),
            "WARN" => out.push(Expect::Warn(arg.to_owned())),
            "CLEAN" => out.push(Expect::Clean),
            "COUNT" => {
                let mut cp = arg.splitn(2, char::is_whitespace);
                let sev = cp.next().unwrap_or("");
                let n: usize = cp.next().unwrap_or("0").trim().parse().unwrap_or(0);
                let severity = if sev.eq_ignore_ascii_case("ERROR") {
                    Severity::Error
                } else {
                    Severity::Warning
                };
                out.push(Expect::Count { severity, n });
            }
            _ => {}
        }
    }
    out
}

/// Runs the inline expectations in `source` against the checker, panicking with a
/// descriptive message on the first failure. `label` names the fixture.
pub fn run_annotated(label: &str, source: &str) {
    let outcome = check_source(source);
    let expects = parse_annotations(source);
    let expects_diagnostics = expects.iter().any(|e| {
        matches!(e, Expect::Error(_) | Expect::Count { .. } | Expect::Clean | Expect::Warn(_))
    });

    for expect in &expects {
        match expect {
            Expect::Type { name, rendered } => {
                let got = outcome.types.get(name).unwrap_or_else(|| {
                    panic!("[{label}] no binding `{name}` (have {:?})", outcome.types.keys())
                });
                assert_eq!(
                    got, rendered,
                    "[{label}] type of `{name}`: expected `{rendered}`, got `{got}`"
                );
            }
            Expect::Local { func, var, rendered } => {
                let got = outcome
                    .local_type(func, var)
                    .unwrap_or_else(|| panic!("[{label}] no local `{var}` in `{func}`"));
                assert_eq!(
                    &got, rendered,
                    "[{label}] type of local `{func}.{var}`: expected `{rendered}`, got `{got}`"
                );
            }
            Expect::Error(code) => assert!(
                outcome.has_code(code),
                "[{label}] expected error {code}, got {:?}",
                outcome.codes()
            ),
            Expect::Warn(code) => assert!(
                outcome.has_code(code),
                "[{label}] expected warning {code}, got {:?}",
                outcome.codes()
            ),
            Expect::Count { severity, n } => {
                let got = outcome.diagnostics.iter().filter(|d| d.severity == *severity).count();
                assert_eq!(
                    got,
                    *n,
                    "[{label}] expected {n} {severity:?} diagnostics, got {got}: {:?}",
                    outcome.codes()
                );
            }
            Expect::Clean => assert!(
                !outcome.has_errors(),
                "[{label}] expected no errors, got {:?}",
                outcome.codes()
            ),
        }
    }

    // A fixture with no diagnostic-related annotation must be clean.
    if !expects_diagnostics {
        assert!(
            !outcome.has_errors(),
            "[{label}] fixture without error annotations must be clean, got {:?}",
            outcome.codes()
        );
    }
}

/// Convenience for building a symbol (used by tests that poke the db directly).
#[must_use]
pub fn sym(name: &str) -> Symbol {
    Symbol::intern(name)
}

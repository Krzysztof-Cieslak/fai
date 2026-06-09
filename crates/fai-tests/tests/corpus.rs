//! The benchmark/guard corpora stay valid: the synthetic generator's
//! contract-enabled output and the real-world sample app both typecheck clean,
//! and every language-server probe position resolves (so a fixture edit that
//! moves a probe token is caught here rather than silently mis-measuring).

use fai_corpus::CorpusSpec;
use fai_corpus::realworld::{self, Op};
use fai_db::{Db, DbSpanResolver, SourceFile};
use fai_diagnostics::Severity;
use fai_tests::check_source_diagnostics;

/// The error codes (if any) belonging to `file`.
fn errors(db: &dyn Db, file: SourceFile) -> Vec<String> {
    check_source_diagnostics(db, file)
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.code.as_str().to_owned())
        .collect()
}

#[test]
fn generated_contract_corpus_typechecks_clean() {
    let spec = CorpusSpec::with_modules_and_contracts(3);
    let (db, files) = fai_corpus::build_db(&spec);
    for &file in &files {
        assert!(errors(&db, file).is_empty(), "{}: {:?}", file.path(&db), errors(&db, file));
    }
}

#[test]
fn realworld_app_typechecks_clean_on_its_own() {
    // Loaded with only itself and the prelude (not the whole samples corpus), so
    // this proves the app is self-contained — the workspace the roundtrip benches
    // stand a server over.
    let (db, files) = realworld::load_app();
    for (path, &file) in &files {
        assert!(errors(&db, file).is_empty(), "{path}: {:?}", errors(&db, file));
    }
}

#[test]
fn realworld_probes_are_in_bounds() {
    let (db, files) = realworld::load_app();
    for op in [
        Op::Hover,
        Op::Definition,
        Op::References,
        Op::Rename,
        Op::Completion,
        Op::SignatureHelp,
        Op::DocumentSymbols,
        Op::Diagnostics,
    ] {
        for probe in realworld::probes(op) {
            let file = files.get(probe.path).copied().unwrap_or_else(|| panic!("{}", probe.path));
            let len = file.text(&db).len() as u32;
            assert!(probe.offset < len, "{op:?} probe {probe} offset {} >= {len}", probe.offset);
        }
    }
}

#[test]
fn realworld_hover_and_definition_probes_resolve() {
    // The two most fundamental probes must actually resolve, so the benches
    // measure real work (and a fixture change that breaks them trips here).
    let (db, files) = realworld::load_app();
    let resolver = DbSpanResolver::new(&db);

    let hover = &realworld::probes(Op::Hover)[0];
    let hfile = files.get(hover.path).copied().unwrap();
    assert!(
        fai_ide::hover_at(&db, hfile, hover.offset, &resolver).ty.is_some(),
        "hover probe {hover} should resolve to a type"
    );

    let def = &realworld::probes(Op::Definition)[0];
    let dfile = files.get(def.path).copied().unwrap();
    assert!(
        !fai_ide::definition_at(&db, dfile, def.offset, &resolver).definitions.is_empty(),
        "definition probe {def} should resolve to a definition"
    );
}

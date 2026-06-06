//! The cross-module firewall: editing a private body re-checks only that body;
//! editing a public signature re-checks its dependents and nothing more.
//!
//! Correctness is checked by incremental-vs-clean agreement; precision (that work
//! was actually skipped) is checked by asserting which queries executed via the
//! event log.

use fai_db::{Db, FaiDatabase};
use fai_tests::assert_incremental_matches_clean;
use fai_types::{check_file, def_type};
use indoc::{formatdoc, indoc};

/// Two modules: B calls A.inc through its public signature.
fn a_src(body: &str) -> &'static str {
    Box::leak(
        formatdoc! {r#"
            module A

            public inc : Int -> Int
            let inc x = {body}

            let secret = 7
        "#}
        .into_boxed_str(),
    )
}

#[test]
fn private_body_edit_does_not_recheck_dependent_inference() {
    let mut db = FaiDatabase::new();
    db.add_source("A.fai".into(), a_src("x + 1").to_owned());
    let b = db.add_source(
        "B.fai".into(),
        indoc! {r#"
                module B

                public two : Int
                let two = A.inc 1
            "#}
        .to_owned(),
    );
    let b_file = db.source_file(b).unwrap();

    // Prime both modules fully.
    check_file(&db, b_file);
    let two = fai_syntax::Symbol::intern("two");
    let before = def_type(&db, b_file, two);

    db.enable_event_log();

    // Edit A's PRIVATE body (`secret`): A's public interface is unchanged.
    db.add_source(
        "A.fai".into(),
        indoc! {r#"
            module A

            public inc : Int -> Int
            let inc x = x + 1

            let secret = 999
        "#}
        .to_owned(),
    );

    // Recompute B's `two` type. It must come from cache: no inference query may
    // execute, because B depends only on A.inc's *signature*, which is unchanged.
    let after = def_type(&db, b_file, two);
    let log = db.take_events();

    assert_eq!(before, after, "B.two's type must be unchanged");
    assert!(
        !log.iter().any(|e| e.contains("infer_scc_query")),
        "no inference should re-run when recomputing B after an A private-body edit: {log:?}"
    );
}

#[test]
fn public_signature_edit_invalidates_dependent_inference() {
    let mut db = FaiDatabase::new();
    db.add_source("A.fai".into(), a_src("x + 1").to_owned());
    let b = db.add_source(
        "B.fai".into(),
        indoc! {r#"
                module B

                public two : Int
                let two = A.inc 1
            "#}
        .to_owned(),
    );
    let b_file = db.source_file(b).unwrap();

    check_file(&db, b_file);
    let two = fai_syntax::Symbol::intern("two");
    let _ = def_type(&db, b_file, two);

    db.enable_event_log();

    // Change A.inc's public SIGNATURE: B applies `A.inc 1`, so B's inference must
    // re-run (and now mismatch, since inc no longer returns Int from one arg).
    db.add_source(
        "A.fai".into(),
        indoc! {r#"
            module A

            public inc : Int -> Int -> Int
            let inc x = x + 1

            let secret = 7
        "#}
        .to_owned(),
    );
    let _ = def_type(&db, b_file, two);
    let log = db.take_events();

    assert!(
        log.iter().any(|e| e.contains("infer_scc_query")),
        "B's inference must re-run after A's public-signature change: {log:?}"
    );
}

#[test]
fn def_type_matches_clean_across_edits() {
    let a_inc = indoc! {r#"
        module A

        public inc : Int -> Int
        let inc x = x + 1
    "#};
    let a_inc_with_z = indoc! {r#"
        module A

        public inc : Int -> Int
        let inc x = x + 1

        let z = 0
    "#};
    let b_two_1 = indoc! {r#"
        module B

        public two : Int
        let two = A.inc 1
    "#};
    let b_two_2 = indoc! {r#"
        module B

        public two : Int
        let two = A.inc 2
    "#};
    let revisions: &[&[(&str, &str)]] = &[
        &[("A.fai", a_inc), ("B.fai", b_two_1)],
        // Edit A's private detail (add a private binding): B unchanged.
        &[("A.fai", a_inc_with_z), ("B.fai", b_two_1)],
        // Edit B's body.
        &[("A.fai", a_inc_with_z), ("B.fai", b_two_2)],
    ];
    assert_incremental_matches_clean(revisions, |db, ids| {
        ids.iter()
            .map(|&id| {
                let file = db.source_file(id).unwrap();
                // Summarize every def's type as a stable string.
                let defs = fai_resolve::module_defs(db, file);
                let mut out: Vec<String> = defs
                    .defs
                    .iter()
                    .map(|d| {
                        format!(
                            "{}: {}",
                            d.name,
                            fai_types::render_scheme(&def_type(db, file, d.name))
                        )
                    })
                    .collect();
                out.sort();
                out
            })
            .collect::<Vec<_>>()
    });
}

#[test]
fn private_body_edit_keeps_interface_value_stable() {
    // The firewall's foundation: A's module_interface is identical before/after a
    // private-body edit, so any value keyed on it is reused.
    let mut db = FaiDatabase::new();
    let a = db.add_source("A.fai".into(), a_src("x + 1").to_owned());
    let a_file = db.source_file(a).unwrap();
    let before = fai_resolve::module_interface(&db, a_file);

    db.add_source(
        "A.fai".into(),
        indoc! {r#"
            module A

            public inc : Int -> Int
            let inc x = x * 2

            let secret = 1
        "#}
        .to_owned(),
    );
    let after = fai_resolve::module_interface(&db, a_file);
    assert_eq!(before, after, "private-body edit must not change A's interface");
}

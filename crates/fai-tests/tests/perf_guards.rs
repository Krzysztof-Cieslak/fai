//! Deterministic performance guards.
//!
//! These assert the *incrementality* properties the architecture promises, using
//! the query-execution event log (a deterministic count of which salsa queries
//! re-ran) rather than wall-clock time — so they are immune to CI noise. They are
//! the regression gate for performance; the wall-clock benches (`benches/`) are
//! for local profiling.
//!
//! The headline property: the work to re-check after a localized edit is
//! independent of total workspace size (the cross-module firewall).

use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use fai_db::{Db, FaiDatabase, Setter, SourceFile};
use fai_driver::{Session, check, object_code};
use fai_syntax::Symbol;
use fai_tests::corpus::{self, CorpusSpec};
use fai_types::check_file;
use indoc::{formatdoc, indoc};

/// Type-checks every file (driving inference) so the database is fully warmed.
fn check_all(db: &FaiDatabase, files: &[SourceFile]) {
    for &file in files {
        check_file(db, file);
    }
}

/// Counts recorded `WillExecute` events whose key contains `needle`.
fn count(events: &[String], needle: &str) -> usize {
    events.iter().filter(|e| e.contains(needle)).count()
}

/// Builds and warms a corpus, then records the queries that re-run after applying
/// `(path, source)` and re-checking everything. Returns the number of
/// `infer_scc_query` executions (the inference work done by the edit).
fn infer_runs_after_edit(modules: usize, path: &str, source: String) -> usize {
    let spec = CorpusSpec::with_modules(modules);
    let (mut db, files) = corpus::build_db(&spec);
    check_all(&db, &files);

    db.enable_event_log();
    db.add_source(path.into(), source);
    check_all(&db, &files);
    count(&db.take_events(), "infer_scc_query")
}

#[test]
fn private_body_edit_work_is_independent_of_workspace_size() {
    // Editing one module's private body re-infers only that module's own
    // definitions — the same amount of work whether the workspace has 10 modules
    // or 100. This is the cross-module firewall, the headline perf property.
    let spec = CorpusSpec::with_modules(1);
    let defs_in_one_module = spec.public_defs_per_module + spec.private_defs_per_module;

    let small = infer_runs_after_edit(
        10,
        "M3.fai",
        corpus::edit_private_body(&CorpusSpec::with_modules(10), 3, 1),
    );
    let large = infer_runs_after_edit(
        100,
        "M3.fai",
        corpus::edit_private_body(&CorpusSpec::with_modules(100), 3, 1),
    );

    assert_eq!(small, large, "firewall: private-body recompute must not grow with workspace size");
    assert_eq!(
        small, defs_in_one_module,
        "a private-body edit should re-infer exactly the edited module's defs ({defs_in_one_module})"
    );
}

#[test]
fn comment_edit_does_not_recheck_other_modules() {
    // A trivia (comment) edit shifts byte offsets, so the edited file's own
    // bodies are re-inferred (body-level cutoff is deferred — see PLAN.md D27),
    // but the cross-module firewall still protects every *other* module: the work
    // stays constant regardless of workspace size.
    let small = infer_runs_after_edit(
        10,
        "M3.fai",
        corpus::edit_comment(&CorpusSpec::with_modules(10), 3, 1),
    );
    let large = infer_runs_after_edit(
        100,
        "M3.fai",
        corpus::edit_comment(&CorpusSpec::with_modules(100), 3, 1),
    );

    assert_eq!(
        small, large,
        "a comment edit must not re-check other modules as the workspace grows"
    );
    let spec = CorpusSpec::with_modules(1);
    assert_eq!(small, spec.public_defs_per_module + spec.private_defs_per_module);
}

#[test]
fn public_signature_edit_invalidates_dependents_but_not_everything() {
    // Editing a public signature must invalidate its dependents (so the count
    // grows with how many modules depend on it) — but NOT independent definitions
    // (the private helpers, which don't reference `Core`, must be untouched).
    let small_spec = CorpusSpec::with_modules(10);
    let large_spec = CorpusSpec::with_modules(100);

    let small = infer_runs_after_edit(10, "Core.fai", corpus::edit_core_signature(&small_spec));
    let large = infer_runs_after_edit(100, "Core.fai", corpus::edit_core_signature(&large_spec));

    // Dependents scale with the workspace (a `g0` in every module depends on f0).
    assert!(large > small, "more modules ⇒ more dependents re-checked ({small} vs {large})");
    // But far fewer than *every* definition: the private helpers never re-infer.
    assert!(
        large < large_spec.total_defs(),
        "a signature edit must not re-infer every definition ({large} vs {} total)",
        large_spec.total_defs()
    );
}

#[test]
fn nested_private_body_edit_does_not_recheck_dependents() {
    // The cross-module firewall holds across nesting: editing a nested *private*
    // body re-infers only the edited file's definitions; another file that uses a
    // public nested member (through its signature) is not re-checked.
    let mut db = FaiDatabase::new();
    let lib = db.add_source(
        "Lib.fai".into(),
        indoc! {r#"
            module Lib

            module Inner =
              let helper x = x + 1

              public bump : Int -> Int
              let bump x = helper x
        "#}
        .to_owned(),
    );
    let user = db.add_source(
        "User.fai".into(),
        indoc! {r#"
            module User

            public use : Int -> Int
            let use x = Lib.Inner.bump x
        "#}
        .to_owned(),
    );
    let libf = db.source_file(lib).unwrap();
    let userf = db.source_file(user).unwrap();
    check_file(&db, libf);
    check_file(&db, userf);

    db.enable_event_log();
    // Edit only the nested private helper's body (no signature change).
    libf.set_text(&mut db).to(indoc! {r#"
        module Lib

        module Inner =
          let helper x = x + 2

          public bump : Int -> Int
          let bump x = helper x
    "#}
    .to_owned());
    check_file(&db, libf);
    check_file(&db, userf);
    let events = db.take_events();

    // The edited file re-infers; `User` does not (it uses `bump`'s signature).
    let total = count(&events, "infer_scc_query");
    assert_eq!(total, 2, "only Lib.Inner's two defs should re-infer, got {total}: {events:?}");
}

#[test]
fn cold_check_is_linear_in_workspace_size() {
    // From a cold database, the number of inference queries scales linearly with
    // the number of definitions (each def/SCC is inferred once), not super-
    // linearly.
    fn cold_infer(modules: usize) -> (usize, usize) {
        let spec = CorpusSpec::with_modules(modules);
        let (db, files) = corpus::build_db(&spec);
        db.enable_event_log();
        check_all(&db, &files);
        (spec.total_defs(), count(&db.take_events(), "infer_scc_query"))
    }

    let (defs10, infer10) = cold_infer(10);
    let (defs100, infer100) = cold_infer(100);

    // Every definition is inferred at least once.
    assert!(infer10 >= defs10, "cold check should infer every def ({infer10} < {defs10})");
    assert!(infer100 >= defs100);
    // Linear, not quadratic: doubling-by-10x the defs must not 100x the work.
    let ratio = infer100 as f64 / infer10 as f64;
    let def_ratio = defs100 as f64 / defs10 as f64;
    assert!(
        ratio < def_ratio * 2.0,
        "cold inference should scale ~linearly: query ratio {ratio:.1} vs def ratio {def_ratio:.1}"
    );
}

/// Builds `Main` (calling `Helper.helper`), `Helper`, and `fillers` independent
/// modules; warms every definition's `object_code`; edits `Helper`'s body; then
/// recompiles everything and returns how many objects were re-emitted.
fn object_code_runs_after_helper_edit(fillers: usize) -> usize {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let helper_id = db.add_source(
        "Helper.fai".into(),
        indoc! {r#"
            module Helper

            public helper : Int -> Int
            let helper x = x + 1
        "#}
        .to_owned(),
    );
    let main_id = db.add_source(
        "Main.fai".into(),
        indoc! {r#"
            module Main

            public main : Runtime -> Unit
            let main r = r.console.writeLine (Int.toString (Helper.helper 1))
        "#}
        .to_owned(),
    );
    let mut filler: Vec<(SourceFile, Symbol)> = Vec::new();
    for i in 0..fillers {
        let src = formatdoc! {r#"
            module F{i}

            public g{i} : Int -> Int
            let g{i} x = x + {i}
        "#};
        let id = db.add_source(format!("F{i}.fai").into(), src);
        filler.push((db.source_file(id).unwrap(), Symbol::intern(&format!("g{i}"))));
    }

    let main = db.source_file(main_id).unwrap();
    let helper = db.source_file(helper_id).unwrap();
    let warm = |db: &FaiDatabase| {
        object_code(db, main, Symbol::intern("main"));
        object_code(db, helper, Symbol::intern("helper"));
        for (f, g) in &filler {
            object_code(db, *f, *g);
        }
    };
    warm(&db);

    db.enable_event_log();
    helper.set_text(&mut db).to(indoc! {r#"
        module Helper

        public helper : Int -> Int
        let helper x = x + 2
    "#}
    .to_owned());
    warm(&db);
    count(&db.take_events(), "object_code")
}

#[test]
fn codegen_firewall_is_independent_of_workspace_size() {
    // Editing one module's body re-emits only that module's object — the same
    // amount of work whether the rest of the workspace has 5 modules or 50.
    let small = object_code_runs_after_helper_edit(5);
    let large = object_code_runs_after_helper_edit(50);
    assert_eq!(small, large, "codegen firewall: re-codegen must not grow with workspace size");
    assert_eq!(small, 1, "only the edited module's object is recompiled");
}

/// A temp workspace seeded with two clean modules; returns its directory.
fn sync_workspace() -> Utf8PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).unwrap().join(format!(
        "fai-sync-guard-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("A.fai"),
        indoc! {r#"
            module A

            public a : Int -> Int
            let a x = x + 1
        "#},
    )
    .unwrap();
    std::fs::write(
        dir.join("B.fai"),
        indoc! {r#"
            module B

            public b : Int -> Int
            let b x = x + 2
        "#},
    )
    .unwrap();
    dir
}

#[test]
fn content_preserving_resync_reinfers_nothing() {
    // Rewriting a file with byte-identical content bumps its mtime but not its
    // hash, so the stat-gated, hash-confirmed sync must not touch the salsa input
    // — a subsequent check re-infers nothing (the early-cutoff firewall over the
    // daemon's file-state sync).
    let dir = sync_workspace();
    let mut session = Session::open(dir.clone()).unwrap();
    let _ = check(session.db(), &session.select_files(None)); // warm

    session.enable_event_log();
    // Identical bytes, fresh mtime.
    std::fs::write(
        dir.join("A.fai"),
        indoc! {r#"
            module A

            public a : Int -> Int
            let a x = x + 1
        "#},
    )
    .unwrap();
    session.sync_from_disk().unwrap();
    let _ = check(session.db(), &session.select_files(None));

    assert_eq!(
        count(&session.take_events(), "infer_scc_query"),
        0,
        "a content-preserving resync must re-infer nothing"
    );
}

#[test]
fn editing_content_reinfers_the_changed_module() {
    // The contrast: a genuine content change must be picked up and re-inferred.
    let dir = sync_workspace();
    let mut session = Session::open(dir.clone()).unwrap();
    let _ = check(session.db(), &session.select_files(None)); // warm

    session.enable_event_log();
    std::fs::write(
        dir.join("A.fai"),
        indoc! {r#"
            module A

            public a : Int -> Int
            let a x = x + 999
        "#},
    )
    .unwrap();
    session.sync_from_disk().unwrap();
    let _ = check(session.db(), &session.select_files(None));

    assert!(
        count(&session.take_events(), "infer_scc_query") >= 1,
        "a real edit must re-infer the changed module"
    );
}

#[test]
fn warm_reverify_with_no_edit_is_free() {
    // Re-checking an unchanged, already-warmed workspace must run *no* inference
    // (full memoization).
    let spec = CorpusSpec::with_modules(20);
    let (db, files) = corpus::build_db(&spec);
    check_all(&db, &files);
    db.enable_event_log();
    check_all(&db, &files);
    assert_eq!(
        count(&db.take_events(), "infer_scc_query"),
        0,
        "warm re-check must be fully memoized"
    );
}

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
use fai_corpus::{self as corpus, CorpusSpec};
use fai_db::{Db, DbSpanResolver, FaiDatabase, Setter, SourceFile};
use fai_driver::{Session, check, object_code};
use fai_syntax::Symbol;
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
    // bodies are re-inferred (body-level cutoff is deferred — future work),
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

// ── the language-server multi-file firewall ──────────────────────────────────
//
// The server answers per open file: diagnostics for the file being edited, and
// hover / go-to-definition at a position in it. These guards assert the
// cross-module firewall holds for that multi-file workflow — editing one module
// leaves the *others'* analysis fully cached — which the wall-clock LSP benches
// (`benches/lsp.rs`) cannot prove, since validation and allocation noise drift
// the warm latency a little even when the recomputed work is constant.
//
// The work is observed through the underlying salsa queries: `check_file` (the
// inference behind a file's diagnostics) and `body_types` (what hover and
// go-to-definition read). A `_after_editing(target)` helper edits one module and
// then serves a *fixed probe* module (`M7`), so each guard checks both the
// firewall (a foreign edit ⇒ zero work for the probe) and its own non-vacuity
// (editing the probe itself ⇒ work), at two workspace sizes.

/// The byte offset of a cross-module reference (`Core.fN`) in a leaf module.
fn core_reference(db: &FaiDatabase, file: SourceFile) -> u32 {
    file.text(db).find("f0 x").expect("a leaf body calls Core.f0") as u32
}

/// The probe module every guard serves (must exist at all tested sizes).
const PROBE: &str = "M7.fai";
const PROBE_INDEX: usize = 7;

#[test]
fn diagnostics_for_one_file_are_unaffected_by_edits_to_another() {
    // The inference behind the probe's diagnostics re-runs only when the probe
    // itself changed: a private-body edit to *another* module re-infers nothing
    // for the probe (the firewall), at any workspace size, while editing the
    // probe re-infers exactly its own defs (so the guard is not vacuous).
    fn probe_infer_after_editing(modules: usize, target: usize) -> usize {
        let spec = CorpusSpec::with_modules(modules);
        let (mut db, files) = corpus::build_db(&spec);
        check_all(&db, &files); // warm every file's inference
        let probe = files.iter().copied().find(|f| f.path(&db).as_str() == PROBE).unwrap();

        db.enable_event_log();
        db.add_source(format!("M{target}.fai").into(), corpus::edit_private_body(&spec, target, 1));
        check_file(&db, probe);
        count(&db.take_events(), "infer_scc_query")
    }

    let defs_in_one = {
        let one = CorpusSpec::with_modules(1);
        one.public_defs_per_module + one.private_defs_per_module
    };
    for &modules in &[10usize, 100] {
        assert_eq!(
            probe_infer_after_editing(modules, 3),
            0,
            "editing another module must not re-infer the probe ({modules} modules)"
        );
        assert_eq!(
            probe_infer_after_editing(modules, PROBE_INDEX),
            defs_in_one,
            "editing the probe itself re-infers exactly its defs ({modules} modules)"
        );
    }
}

#[test]
fn hover_and_definition_on_one_file_are_unaffected_by_edits_to_another() {
    // Hover and go-to-definition read `body_types`; serving them for the probe
    // after a private-body edit to *another* module recomputes none of it (the
    // firewall), at any workspace size, while editing the probe does (non-vacuity).
    fn probe_bodytypes_after_editing(modules: usize, target: usize) -> usize {
        let spec = CorpusSpec::with_modules(modules);
        let (mut db, files) = corpus::build_db(&spec);
        check_all(&db, &files);
        let probe = files.iter().copied().find(|f| f.path(&db).as_str() == PROBE).unwrap();
        let offset = core_reference(&db, probe);
        // Warm the probe's hover/definition so a later call is a pure cache hit.
        let _ = fai_ide::hover_at(&db, probe, offset, &DbSpanResolver::new(&db));
        let _ = fai_ide::definition_at(&db, probe, offset, &DbSpanResolver::new(&db));

        db.enable_event_log();
        db.add_source(format!("M{target}.fai").into(), corpus::edit_private_body(&spec, target, 1));
        let _ = fai_ide::hover_at(&db, probe, offset, &DbSpanResolver::new(&db));
        let _ = fai_ide::definition_at(&db, probe, offset, &DbSpanResolver::new(&db));
        count(&db.take_events(), "body_types")
    }

    for &modules in &[10usize, 100] {
        assert_eq!(
            probe_bodytypes_after_editing(modules, 3),
            0,
            "editing another module must not recompute the probe's hover/def ({modules} modules)"
        );
        assert!(
            probe_bodytypes_after_editing(modules, PROBE_INDEX) > 0,
            "editing the probe itself recomputes its hover/def ({modules} modules)"
        );
    }
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

            public main : Runtime -> Unit / { Console }
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
        object_code(db, main, Symbol::intern("main"), false);
        object_code(db, helper, Symbol::intern("helper"), false);
        for (f, g) in &filler {
            object_code(db, *f, *g, false);
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

/// Builds `Lib` (an eligible helper `mk` plus a public `top` that calls it, so
/// `top` inlines `mk` *within the module*), `Main` (which calls `Lib.top` across a
/// module boundary, where the inliner never reaches), and `fillers` independent
/// modules; warms every object; edits `mk`'s body (its signature unchanged); then
/// recompiles and returns how many objects were re-emitted.
fn helper_inline_object_reruns(fillers: usize) -> usize {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let lib_id = db.add_source(
        "Lib.fai".into(),
        indoc! {r#"
            module Lib

            mk : Int -> Int
            let mk x = x + x

            public top : Int -> Int
            let top x = mk x + 1
        "#}
        .to_owned(),
    );
    let main_id = db.add_source(
        "Main.fai".into(),
        indoc! {r#"
            module Main

            public main : Runtime -> Unit / { Console }
            let main r = r.console.writeLine (Int.toString (Lib.top 1))
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

    let lib = db.source_file(lib_id).unwrap();
    let main = db.source_file(main_id).unwrap();
    let warm = |db: &FaiDatabase| {
        object_code(db, main, Symbol::intern("main"), false);
        object_code(db, lib, Symbol::intern("top"), false);
        object_code(db, lib, Symbol::intern("mk"), false);
        for (f, g) in &filler {
            object_code(db, *f, *g, false);
        }
    };
    warm(&db);

    db.enable_event_log();
    // Edit `mk`'s body, preserving its signature. `top` inlines `mk`, so `top`
    // re-emits; `Main` calls `top` across a module boundary (the inliner never
    // crosses it) and depends only on `top`'s signature, so it is cut off.
    lib.set_text(&mut db).to(indoc! {r#"
        module Lib

        mk : Int -> Int
        let mk x = x + x + x

        public top : Int -> Int
        let top x = mk x + 1
    "#}
    .to_owned());
    warm(&db);
    count(&db.take_events(), "object_code")
}

#[test]
fn helper_inliner_preserves_the_cross_module_firewall() {
    // Intra-module helper inlining must not ripple across a module boundary: editing
    // a module whose body folds in a helper re-emits only that module's objects, the
    // same whether the workspace has 5 other modules or 50. (`Main`, which calls the
    // module's public function across the boundary, and the fillers stay cached.)
    let small = helper_inline_object_reruns(5);
    let large = helper_inline_object_reruns(50);
    assert_eq!(small, large, "intra-module inlining must not grow re-codegen with workspace size");
    assert_eq!(small, 2, "only the edited module's own objects (mk + top) recompile");
}

/// Builds `Sum` (a module whose private `loop` over an `Array Int` index elides
/// its bounds check via the file-local entry facts), `Main` (calling it), and
/// `fillers` independent modules; warms every object; edits a *filler*; then
/// recompiles and returns how many `object_code` queries re-ran. The entry-fact
/// inference is file-local, so an edit to an unrelated module must not re-emit the
/// array module's object.
fn bce_firewall_object_reruns(fillers: usize) -> usize {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let sum_id = db.add_source(
        "Sum.fai".into(),
        indoc! {r#"
            module Sum

            go : Int -> Int -> Array Int -> Int
            let go i acc xs =
              if i >= Array.length xs then acc else go (i + 1) (acc + Array.unsafeGet i xs) xs

            public total : Array Int -> Int
            let total xs = go 0 0 xs
        "#}
        .to_owned(),
    );
    let main_id = db.add_source(
        "Main.fai".into(),
        indoc! {r#"
            module Main

            public main : Runtime -> Unit / { Console }
            let main r = r.console.writeLine (Int.toString (Sum.total (Array.range 0 5)))
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

    let sum = db.source_file(sum_id).unwrap();
    let main = db.source_file(main_id).unwrap();
    let warm = |db: &FaiDatabase| {
        object_code(db, main, Symbol::intern("main"), false);
        object_code(db, sum, Symbol::intern("total"), false);
        object_code(db, sum, Symbol::intern("go"), false);
        for (f, g) in &filler {
            object_code(db, *f, *g, false);
        }
    };
    warm(&db);

    db.enable_event_log();
    // Edit the *first* filler's body; the array module's entry facts are file-local,
    // so its objects must not re-emit.
    let (f0, _) = filler[0];
    f0.set_text(&mut db).to(indoc! {r#"
        module F0

        public g0 : Int -> Int
        let g0 x = x + 100
    "#}
    .to_owned());
    warm(&db);
    count(&db.take_events(), "object_code")
}

#[test]
fn bounds_check_elimination_preserves_the_cross_module_firewall() {
    // The file-local entry-fact inference must not ripple across a module boundary:
    // editing an unrelated module re-emits only that module's object (one), the same
    // whether the workspace has 5 other modules or 50 — the array module's elision
    // facts depend only on its own file.
    let small = bce_firewall_object_reruns(5);
    let large = bce_firewall_object_reruns(50);
    assert_eq!(small, large, "BCE firewall must not grow re-codegen with workspace size");
    assert_eq!(small, 1, "only the edited filler's own object recompiles");
}

/// Builds `Helper.helper`, `Probe.probe` (which only *forwards* its parameter to
/// `Helper.helper`, so inter-procedural inference makes `borrow_signature(probe)`
/// depend on `borrow_signature(helper)`), and `fillers` independent modules; warms
/// every definition's `object_code`; edits `Helper`'s body either preserving its
/// borrow signature (`x + 1` -> `x + 2`) or changing it (`x + 1` -> `0`, dropping
/// the use of `x` so the parameter becomes borrowed); then recompiles and returns
/// how many `borrow_signature` and `object_code` queries re-ran.
fn borrow_firewall_reruns(fillers: usize, sig_changing: bool) -> (usize, usize) {
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
    let probe_id = db.add_source(
        "Probe.fai".into(),
        indoc! {r#"
            module Probe

            public probe : Int -> Int
            let probe x = Helper.helper x
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

    let probe = db.source_file(probe_id).unwrap();
    let helper = db.source_file(helper_id).unwrap();
    let warm = |db: &FaiDatabase| {
        object_code(db, probe, Symbol::intern("probe"), false);
        object_code(db, helper, Symbol::intern("helper"), false);
        for (f, g) in &filler {
            object_code(db, *f, *g, false);
        }
    };
    warm(&db);

    db.enable_event_log();
    let edited = if sig_changing {
        // `x` is now unused, so `helper` borrows it ([false] -> [true]).
        indoc! {r#"
            module Helper

            public helper : Int -> Int
            let helper x = 0
        "#}
    } else {
        // Borrow signature is unchanged ([false]): `x` is still inspected.
        indoc! {r#"
            module Helper

            public helper : Int -> Int
            let helper x = x + 2
        "#}
    };
    helper.set_text(&mut db).to(edited.to_owned());
    warm(&db);
    let events = db.take_events();
    (count(&events, "borrow_signature"), count(&events, "object_code"))
}

#[test]
fn borrow_signature_firewall_is_independent_of_workspace_size() {
    // A callee-body edit that does NOT change the callee's borrow signature re-runs
    // only the callee's own `borrow_signature`/`object_code`; the forwarding
    // caller is cut off (early cutoff on the small `BorrowSig` value) — the same
    // work whether the workspace has 5 modules or 50.
    let small = borrow_firewall_reruns(5, false);
    let large = borrow_firewall_reruns(50, false);
    assert_eq!(small, large, "borrow firewall must not grow with workspace size");
    assert_eq!(
        small,
        (1, 1),
        "a sig-preserving callee edit re-runs only the callee (caller cut off)"
    );
}

#[test]
fn borrow_signature_change_ripples_only_to_forwarding_caller() {
    // Non-vacuity: an edit that *changes* the callee's borrow signature does re-run
    // the forwarding caller's `borrow_signature` and `object_code` — and only those
    // (the callee plus the one caller), independent of workspace size. This is the
    // bounded firewall widening: it fires exactly on borrow-signature-changing
    // edits.
    let small = borrow_firewall_reruns(5, true);
    let large = borrow_firewall_reruns(50, true);
    assert_eq!(small, large, "the ripple must not grow with workspace size");
    assert_eq!(
        small,
        (2, 2),
        "a sig-changing callee edit re-runs the callee and the forwarding caller"
    );
}

/// Builds `Helper.sink` (a recursive — so un-inlined — record builder that accepts
/// a forwarded reuse token) and `Probe.probe` (which forwards a freed record into
/// `Helper.sink`, so `rc_emit(probe)` depends on `reuse_signature(sink)`), plus
/// `fillers` independent modules; warms every definition's `object_code`; edits
/// `Helper`'s body either preserving its reuse signature (the base case
/// `{ a = 0, b = 0 }` -> `{ a = 1, b = 1 }`, still `[2]`) or changing it (the base
/// case -> the pre-built `zero` global, dropping the construction so it accepts no
/// token, `[]`); then recompiles and returns how many `reuse_signature` and
/// `object_code` queries re-ran.
fn reuse_firewall_reruns(fillers: usize, sig_changing: bool) -> (usize, usize) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let helper_id = db.add_source(
        "Helper.fai".into(),
        indoc! {r#"
            module Helper

            public type R2 = { a : Int, b : Int }

            zero : R2
            let zero = { a = 0, b = 0 }

            public sink : Int -> R2
            let sink x =
              if x <= 0 then { a = 0, b = 0 }
              else
                let inner = sink (x - 1)
                { a = inner.a + 1, b = x }
        "#}
        .to_owned(),
    );
    let probe_id = db.add_source(
        "Probe.fai".into(),
        indoc! {r#"
            module Probe

            public probe : Helper.R2 -> Bool -> Helper.R2
            let probe p flag =
              match p with
              | { a, b } -> if flag then { a = b, b = a } else Helper.sink a
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

    let probe = db.source_file(probe_id).unwrap();
    let helper = db.source_file(helper_id).unwrap();
    let warm = |db: &FaiDatabase| {
        object_code(db, probe, Symbol::intern("probe"), false);
        object_code(db, helper, Symbol::intern("sink"), false);
        for (f, g) in &filler {
            object_code(db, *f, *g, false);
        }
    };
    warm(&db);

    db.enable_event_log();
    let edited = if sig_changing {
        // The base case returns the pre-built global, so `sink` constructs nothing
        // and accepts no token ([2] -> []).
        indoc! {r#"
            module Helper

            public type R2 = { a : Int, b : Int }

            zero : R2
            let zero = { a = 0, b = 0 }

            public sink : Int -> R2
            let sink x =
              if x <= 0 then zero
              else
                let inner = sink (x - 1)
                { a = inner.a + 1, b = x }
        "#}
    } else {
        // The reuse signature is unchanged ([2]): the base case still builds an R2.
        indoc! {r#"
            module Helper

            public type R2 = { a : Int, b : Int }

            zero : R2
            let zero = { a = 0, b = 0 }

            public sink : Int -> R2
            let sink x =
              if x <= 0 then { a = 1, b = 1 }
              else
                let inner = sink (x - 1)
                { a = inner.a + 1, b = x }
        "#}
    };
    helper.set_text(&mut db).to(edited.to_owned());
    warm(&db);
    let events = db.take_events();
    (count(&events, "reuse_signature"), count(&events, "object_code"))
}

#[test]
fn reuse_signature_firewall_is_independent_of_workspace_size() {
    // A callee-body edit that does NOT change the callee's reuse signature re-runs
    // only the callee's own `reuse_signature`/`object_code`; the forwarding caller
    // is cut off (early cutoff on the small `ReuseSig` value) — the same work
    // whether the workspace has 5 modules or 50.
    let small = reuse_firewall_reruns(5, false);
    let large = reuse_firewall_reruns(50, false);
    assert_eq!(small, large, "reuse firewall must not grow with workspace size");
    assert_eq!(
        small,
        (1, 1),
        "a sig-preserving callee edit re-runs only the callee (caller cut off)"
    );
}

#[test]
fn reuse_signature_change_ripples_only_to_forwarding_caller() {
    // Non-vacuity: an edit that *changes* the callee's reuse signature does re-run
    // the forwarding caller's analysis and object — but the **expensive** recompiled
    // object code stays bounded to the callee plus the one forwarding caller (two
    // objects), independent of workspace size. (The cheap `reuse_signature` analysis
    // can be re-verified for an unrelated definition at larger scale — a benign
    // salsa memo re-validation that never reaches code generation — so the firewall
    // is asserted on the object-code recompute, the work that actually matters.)
    let small = reuse_firewall_reruns(5, true);
    let large = reuse_firewall_reruns(50, true);
    assert_eq!(small.1, large.1, "object-code recompute must not grow with workspace size");
    assert_eq!(small.1, 2, "a sig-changing callee edit recompiles the callee and the one caller");
    assert!(small.0 >= 2, "the callee and the forwarding caller both re-analyze (non-vacuity)");
}

/// Builds `Dep` and `Caller` (which calls `Dep`'s definition) plus `fillers`
/// independent modules; warms every definition's `object_code`; edits `Dep`'s body
/// either preserving its intrinsic-inliner classification (a non-wrapper helper
/// `x + 1` -> `x + 2`, still not an eta-prim-wrapper) or changing it (a wrapper
/// `a + b` -> `a - b`, whose recognized primitive flips `IntAdd` -> `IntSub`); then
/// recompiles and returns how many `object_code` queries re-ran. Both edits leave
/// the borrow signature unchanged, so only the inliner dependency is exercised.
fn inliner_firewall_object_reruns(fillers: usize, wrapper_op_change: bool) -> usize {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let dep_src = if wrapper_op_change {
        "module Dep\n\npublic dep : Int -> Int -> Int\nlet dep a b = a + b\n"
    } else {
        "module Dep\n\npublic dep : Int -> Int\nlet dep x = x + 1\n"
    };
    let dep_id = db.add_source("Dep.fai".into(), dep_src.to_owned());
    let caller_src = if wrapper_op_change {
        // A saturated call to the wrapper, which the inliner replaces with the
        // wrapper's primitive — so `Caller` depends on `prim_wrapper(dep)`.
        "module Caller\n\npublic call : Int -> Int\nlet call x = Dep.dep x x\n"
    } else {
        "module Caller\n\npublic call : Int -> Int\nlet call x = Dep.dep x\n"
    };
    let caller_id = db.add_source("Caller.fai".into(), caller_src.to_owned());
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

    let caller = db.source_file(caller_id).unwrap();
    let dep = db.source_file(dep_id).unwrap();
    let warm = |db: &FaiDatabase| {
        object_code(db, caller, Symbol::intern("call"), false);
        object_code(db, dep, Symbol::intern("dep"), false);
        for (f, g) in &filler {
            object_code(db, *f, *g, false);
        }
    };
    warm(&db);

    db.enable_event_log();
    let edited = if wrapper_op_change {
        "module Dep\n\npublic dep : Int -> Int -> Int\nlet dep a b = a - b\n"
    } else {
        "module Dep\n\npublic dep : Int -> Int\nlet dep x = x + 2\n"
    };
    dep.set_text(&mut db).to(edited.to_owned());
    warm(&db);
    count(&db.take_events(), "object_code")
}

#[test]
fn inliner_firewall_is_independent_of_workspace_size() {
    // Editing a non-wrapper callee's body re-runs `prim_wrapper(dep)` (the new
    // dependency the inliner adds at `Caller`), which still reports "not a wrapper"
    // — so the result is unchanged and `Caller` is cut off. Only the callee's own
    // object is recompiled, the same whether the workspace has 5 modules or 50.
    let small = inliner_firewall_object_reruns(5, false);
    let large = inliner_firewall_object_reruns(50, false);
    assert_eq!(small, large, "inliner firewall must not grow with workspace size");
    assert_eq!(small, 1, "a classification-preserving callee edit recompiles only the callee");
}

#[test]
fn inliner_wrapper_change_ripples_only_to_direct_caller() {
    // Non-vacuity: changing a user wrapper's recognized primitive (`+` -> `-`)
    // changes `prim_wrapper(dep)`, so the caller that inlines it re-runs its
    // `object_code` too — and only that one caller (plus the callee), independent of
    // workspace size. This is the bounded firewall widening for the inliner.
    let small = inliner_firewall_object_reruns(5, true);
    let large = inliner_firewall_object_reruns(50, true);
    assert_eq!(small, large, "the ripple must not grow with workspace size");
    assert_eq!(small, 2, "a wrapper-primitive change re-runs the callee and the inlining caller");
}

/// Warms four definitions' `object_code` under the given cache capacity, bumps a
/// revision (so any over-capacity blobs are evicted), then re-accesses all four
/// and returns how many had to be regenerated.
fn object_code_reruns_after_eviction(capacity: usize) -> usize {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let mut defs: Vec<(SourceFile, Symbol)> = Vec::new();
    for i in 0..4 {
        let src = formatdoc! {r#"
            module F{i}

            public g{i} : Int -> Int
            let g{i} x = x + {i}
        "#};
        let id = db.add_source(format!("F{i}.fai").into(), src);
        defs.push((db.source_file(id).unwrap(), Symbol::intern(&format!("g{i}"))));
    }
    // A separate input edited to bump the revision (which triggers LRU eviction).
    let bump_id =
        db.add_source("Bump.fai".into(), "module Bump\n\npublic v : Int\nlet v = 0\n".into());
    let bump = db.source_file(bump_id).unwrap();

    fai_driver::set_object_cache_capacity(&mut db, capacity);
    for (f, g) in &defs {
        object_code(&db, *f, *g, false);
    }

    bump.set_text(&mut db).to("module Bump\n\npublic v : Int\nlet v = 1\n".into());

    db.enable_event_log();
    for (f, g) in &defs {
        object_code(&db, *f, *g, false);
    }
    count(&db.take_events(), "object_code")
}

#[test]
fn bounded_object_cache_evicts_then_recomputes() {
    // Unbounded (capacity 0): the warm blobs survive the revision — nothing is
    // regenerated. This is the one-shot CLI / test default, so it is unaffected.
    assert_eq!(
        object_code_reruns_after_eviction(0),
        0,
        "an unbounded object cache keeps every blob memoized across a revision"
    );
    // Bounded (capacity 1): the least-recently-used blobs are evicted at the
    // revision and must be regenerated on next access (the daemon's memory bound).
    assert!(
        object_code_reruns_after_eviction(1) > 0,
        "a bounded object cache evicts and regenerates over-capacity blobs"
    );
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

// ── Intra-inference complexity guards ────────────────────────────────────────
// The firewall guards above count whole-query re-runs; these count the solver's
// *internal* structural work (via the thread-local instrumentation counters) to
// gate the inference solver's asymptotic complexity. They are deterministic
// (inference is single-threaded per definition), so doubling the input must at
// most ~double the work — a regression to quadratic (a ~4x jump) trips the 3x
// bound. Sizes stay small so the deeply-nested chain does not overflow the
// (debug) stack.

/// Infers definition `f` in a fresh database (so the query runs, uncached) and
/// returns the solver-work counters it accumulated.
fn solver_counts(src: String) -> fai_types::perf::Counters {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src);
    let file = db.source_file(id).unwrap();
    fai_types::perf::reset();
    let _ = fai_types::def_type(&db, file, Symbol::intern("f"));
    fai_types::perf::snapshot()
}

#[test]
fn generalization_work_is_linear_in_block_size() {
    // A block of `n` generalized value-`let`s. Rank-based generalization quantifies
    // by binding level, so the free-variable work is linear in `n`; the previous
    // environment-free-variable recomputation was O(n^2).
    let src = |n: usize| {
        let mut s = String::from("module M\n\nlet f =\n");
        for i in 0..n {
            s.push_str(&format!("  let a{i} = []\n"));
        }
        s.push_str("  0\n");
        s
    };
    let small = solver_counts(src(100)).free_var_visits;
    let large = solver_counts(src(200)).free_var_visits;
    assert!(small > 0, "expected free-variable collection work");
    assert!(
        large <= small * 3,
        "generalization free-var visits must stay sub-quadratic (n=100: {small}, n=200: {large})"
    );
}

#[test]
fn chain_resolution_work_is_linear() {
    // A left-nested arithmetic chain builds a result-variable chain; path
    // compression keeps re-resolving it linear (it was O(n^2)).
    let src = |n: usize| {
        let terms = (0..n).map(|i| format!("x + {i}")).collect::<Vec<_>>().join(" + ");
        format!("module M\n\nlet f x = {terms}\n")
    };
    let small = solver_counts(src(100)).resolve_clones;
    let large = solver_counts(src(200)).resolve_clones;
    assert!(small > 0, "expected resolution work");
    assert!(
        large <= small * 3,
        "chain resolution clones must stay sub-quadratic (n=100: {small}, n=200: {large})"
    );
}

/// Warms a fusing user definition's `object_code`, edits the body of a standard
/// combinator it deforests (a behavior-preserving tweak inside `Array.mapInto`),
/// and returns how many `object_code` queries re-ran.
fn fusion_firewall_object_reruns(edit_user: bool) -> usize {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let user = db.add_source(
        "M.fai".into(),
        "module M\n\npublic run : Int -> Int\nlet run n = Array.sum (Array.map (fun x -> x * 2) (Array.range 0 n))\n".into(),
    );
    let m = db.source_file(user).unwrap();
    object_code(&db, m, Symbol::intern("run"), false);

    db.enable_event_log();
    if edit_user {
        // Non-vacuity: editing the fusing definition's own body recompiles it.
        m.set_text(&mut db).to(
            "module M\n\npublic run : Int -> Int\nlet run n = Array.sum (Array.map (fun x -> x * 3) (Array.range 0 n))\n".into(),
        );
    } else {
        // Edit `Array.mapInto`'s body (the combinator the pipeline deforests):
        // `if i >= n` -> `if n <= i`, behavior-preserving. Recognition is by
        // definition id, so the fused user object must not re-run.
        let array = db
            .all_source_files()
            .into_iter()
            .find(|f| f.path(&db).ends_with("Array.fai"))
            .expect("std Array module");
        let edited = array
            .text(&db)
            .replace("if i >= n then acc else mapInto", "if n <= i then acc else mapInto");
        assert_ne!(&edited, array.text(&db), "the combinator-body edit must change Array.fai");
        array.set_text(&mut db).to(edited);
    }
    object_code(&db, m, Symbol::intern("run"), false);
    count(&db.take_events(), "object_code")
}

#[test]
fn fusion_is_firewalled_from_combinator_body_edits() {
    // Editing a deforested combinator's body never recompiles the fused user
    // object (recognition is by definition id, not the combinator's body).
    assert_eq!(fusion_firewall_object_reruns(false), 0, "fused object is body-independent");
    // Non-vacuity: the user definition's own body edit does recompile it.
    assert_eq!(fusion_firewall_object_reruns(true), 1, "editing the fusing body recompiles it");
}

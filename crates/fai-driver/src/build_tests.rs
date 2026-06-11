//! Driver backend tests: native build-and-run, entry-point errors, and the
//! per-definition object-code cache hit.

use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use fai_db::{Db, FaiDatabase, Setter, SourceFile};
use indoc::indoc;

use crate::{build_native, object_code, reachable_defs};

fn db_with(files: &[(&str, &str)]) -> (FaiDatabase, Vec<SourceFile>) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let mut handles = Vec::new();
    for (path, text) in files {
        let id = db.add_source((*path).into(), (*text).to_owned());
        handles.push(db.source_file(id).unwrap());
    }
    (db, handles)
}

fn temp_exe() -> Utf8PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp dir is UTF-8").join(format!(
        "fai-driver-test-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn count(events: &[String], needle: &str) -> usize {
    events.iter().filter(|e| e.contains(needle)).count()
}

#[test]
fn builds_and_runs_native_executable() {
    let src = indoc! {r#"
        module Hello

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine ("Hello, " ++ "Fai!")
    "#};
    let (db, files) = db_with(&[("Hello.fai", src)]);
    let exe = temp_exe();
    let outcome = build_native(&db, files[0], &exe);
    assert!(outcome.ok, "build failed: {:?}", outcome.diagnostics);

    let output = std::process::Command::new(exe.as_std_path()).output().expect("run executable");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Hello, Fai!\n");
    assert_eq!(output.status.code(), Some(0), "clean exit (no leaks)");
}

#[test]
fn missing_main_is_an_error() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            let x = 1
        "#},
    )]);
    let exe = temp_exe();
    let outcome = build_native(&db, files[0], &exe);
    assert!(!outcome.ok);
    assert!(outcome.diagnostics.iter().any(|d| d.code.as_str() == "FAI0004"));
}

#[test]
fn unsupported_construct_blocks_the_build() {
    // A reachable comparison operator used as a first-class value is outside the
    // native subset and fails.
    let src = indoc! {r#"
        module M

        public lt : Int -> Int -> Bool
        let lt = (<)

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (if lt 1 2 then "lt" else "ge")
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let exe = temp_exe();
    let outcome = build_native(&db, files[0], &exe);
    assert!(!outcome.ok);
    assert!(
        outcome.diagnostics.iter().any(|d| d.code.as_str() == "FAI7001"),
        "expected FAI7001, got {:?}",
        outcome.diagnostics.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
    );
}

#[test]
fn reachability_includes_used_definitions_and_excludes_unused() {
    let src = indoc! {r#"
        module M

        let used x = x + 1

        let unused x = x + 2

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (used 1))
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let names: Vec<String> =
        reachable_defs(&db, files[0]).iter().map(|d| d.name.as_str().to_owned()).collect();
    assert!(names.contains(&"main".to_owned()));
    assert!(names.contains(&"used".to_owned()));
    assert!(!names.contains(&"unused".to_owned()), "unused defs are not reachable");
}

#[test]
fn builds_and_runs_a_cross_module_program() {
    let main = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (Helper.triple 14))
    "#};
    let helper = indoc! {r#"
        module Helper

        public triple : Int -> Int
        let triple x = x * 3
    "#};
    let (db, files) = db_with(&[("Main.fai", main), ("Helper.fai", helper)]);
    let exe = temp_exe();
    let outcome = build_native(&db, files[0], &exe);
    assert!(outcome.ok, "build failed: {:?}", outcome.diagnostics);

    let output = std::process::Command::new(exe.as_std_path()).output().expect("run");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "42\n");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn comment_edit_recompiles_no_objects() {
    use fai_db::Setter;
    // Core IR is position-independent (no spans/comments), so a trivia edit
    // re-lowers the edited definition but produces an identical LoweredDef, which
    // cuts off before codegen: neither the edited module's object nor its
    // dependents' objects are re-emitted.
    let main = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (Helper.helper 1))
    "#};
    let helper_v1 = indoc! {r#"
        module Helper

        public helper : Int -> Int
        let helper x = x + 1
    "#};
    let helper_v2 = indoc! {r#"
        module Helper

        // an added comment shifts offsets only
        public helper : Int -> Int
        let helper x = x + 1
    "#};
    let (mut db, files) = db_with(&[("Main.fai", main), ("Helper.fai", helper_v1)]);
    let _ = object_code(&db, files[0], fai_syntax::Symbol::intern("main"));
    let _ = object_code(&db, files[1], fai_syntax::Symbol::intern("helper"));

    db.enable_event_log();
    files[1].set_text(&mut db).to(helper_v2.to_owned());
    let _ = object_code(&db, files[0], fai_syntax::Symbol::intern("main"));
    let _ = object_code(&db, files[1], fai_syntax::Symbol::intern("helper"));
    assert_eq!(
        count(&db.take_events(), "object_code"),
        0,
        "a comment edit must not re-emit any object code"
    );
}

#[test]
fn type_error_blocks_the_build() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (1 + 2)
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let exe = temp_exe();
    let outcome = build_native(&db, files[0], &exe);
    assert!(!outcome.ok);
    assert!(
        outcome.diagnostics.iter().any(|d| d.code.as_str().starts_with("FAI3")),
        "expected a type error, got {:?}",
        outcome.diagnostics.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
    );
    assert!(!exe.as_std_path().exists(), "no artifact on a failed build");
}

#[test]
fn division_by_zero_aborts_at_runtime() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (10 / 0))
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let exe = temp_exe();
    let outcome = build_native(&db, files[0], &exe);
    assert!(outcome.ok, "division by zero is a runtime fault, not a compile error");

    let output = std::process::Command::new(exe.as_std_path()).output().expect("run");
    assert!(!output.status.success(), "the program should fault");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("division by zero"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn division_by_a_variable_zero_aborts_at_runtime() {
    // A variable (non-literal) zero divisor exercises the inlined zero-divisor
    // guard branching to the runtime fallback — distinct from the constant `10/0`
    // bare-call path above, and the only thing that drives that branch (the
    // property tests exclude a zero divisor).
    let src = indoc! {r#"
        module M

        divide : Int -> Int -> Int
        let divide a b = a / b

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (divide 10 0))
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let exe = temp_exe();
    let outcome = build_native(&db, files[0], &exe);
    assert!(outcome.ok, "division by zero is a runtime fault, not a compile error");

    let output = std::process::Command::new(exe.as_std_path()).output().expect("run");
    assert!(!output.status.success(), "the program should fault");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("division by zero"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn remainder_by_zero_aborts_at_runtime() {
    // The remainder fault path mirrors division: a zero divisor aborts with the
    // located message rather than a silent wrong answer or a raw hardware trap.
    let src = indoc! {r#"
        module M

        rem : Int -> Int -> Int
        let rem a b = a % b

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (rem 10 0))
    "#};
    let (db, files) = db_with(&[("M.fai", src)]);
    let exe = temp_exe();
    let outcome = build_native(&db, files[0], &exe);
    assert!(outcome.ok, "remainder by zero is a runtime fault, not a compile error");

    let output = std::process::Command::new(exe.as_std_path()).output().expect("run");
    assert!(!output.status.success(), "the program should fault");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("by zero"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn object_code_is_deterministic() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (1 + 2))
    "#};
    let object = |contents: &str| {
        let (db, files) = db_with(&[("M.fai", contents)]);
        (*object_code(&db, files[0], fai_syntax::Symbol::intern("main"))).clone()
    };
    assert_eq!(object(src), object(src), "the same source must produce identical object bytes");
}

#[test]
fn editing_a_definition_recompiles_its_object() {
    use fai_db::Setter;
    let v1 = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString 1)
    "#};
    let v2 = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString 2)
    "#};
    let (mut db, files) = db_with(&[("M.fai", v1)]);
    let file = files[0];
    let _ = object_code(&db, file, fai_syntax::Symbol::intern("main"));

    db.enable_event_log();
    file.set_text(&mut db).to(v2.to_owned());
    let _ = object_code(&db, file, fai_syntax::Symbol::intern("main"));
    assert_eq!(count(&db.take_events(), "object_code"), 1, "an edited definition is recompiled");
}

#[test]
fn editing_one_module_reuses_cached_objects_for_the_others() {
    // Main calls Helper.helper. Editing Helper's *body* must re-run only
    // Helper.helper's object_code; Main.main's stays cached (the cross-module
    // firewall, now at the codegen layer).
    let main = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (Helper.helper 41))
    "#};
    let helper_v1 = indoc! {r#"
        module Helper

        public helper : Int -> Int
        let helper x = x + 1
    "#};
    let helper_v2 = indoc! {r#"
        module Helper

        public helper : Int -> Int
        let helper x = x + 2
    "#};
    let (mut db, files) = db_with(&[("Main.fai", main), ("Helper.fai", helper_v1)]);
    let (main_file, helper_file) = (files[0], files[1]);

    // Warm the cache for both definitions.
    let _ = object_code(&db, main_file, fai_syntax::Symbol::intern("main"));
    let _ = object_code(&db, helper_file, fai_syntax::Symbol::intern("helper"));

    // Edit Helper's body, then recompute both objects.
    db.enable_event_log();
    helper_file.set_text(&mut db).to(helper_v2.to_owned());
    let _ = object_code(&db, main_file, fai_syntax::Symbol::intern("main"));
    let _ = object_code(&db, helper_file, fai_syntax::Symbol::intern("helper"));
    let events = db.take_events();

    assert_eq!(
        count(&events, "object_code"),
        1,
        "only Helper.helper's object should be recompiled; Main.main stays cached"
    );
}

#[test]
fn jit_compile_applies_a_named_function() {
    let (db, files) = db_with(&[(
        "M.fai",
        indoc! {r#"
            module M

            public double : Int -> Int
            let double x = x + x

            public main : Runtime -> Unit
            let main rt = rt.console.writeLine (Int.toString (double 21))
        "#},
    )]);

    let mut program = crate::jit_compile(&db, files[0]).expect("compiles");
    let closure = program.function(fai_syntax::Symbol::intern("double")).expect("double exists");
    // Applying consumes one reference of the immortal static closure; dup so the
    // image's closure stays balanced (also exercises the repeat-apply contract).
    let result = fai_runtime::apply(fai_runtime::fai_dup(closure), &[fai_runtime::make_int(21)]);
    let n = fai_runtime::read_int(result);
    fai_runtime::fai_drop(result);
    assert_eq!(n, 42, "double 21 = 42 via the fetched closure");

    // A binding the file does not define yields `None`.
    assert!(program.function(fai_syntax::Symbol::intern("missing")).is_none());
}

#[test]
fn jit_compile_without_main_is_an_error() {
    let (db, files) = db_with(&[("M.fai", "module M\n\npublic x : Int\nlet x = 1\n")]);
    let Err(err) = crate::jit_compile(&db, files[0]) else { panic!("expected a no-main error") };
    assert!(err.iter().any(|d| d.code == crate::NO_ENTRY_POINT), "reports the no-entry diagnostic");
}

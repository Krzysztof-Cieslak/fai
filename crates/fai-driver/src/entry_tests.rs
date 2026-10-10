//! Validation and execution of the runtime-to-main boundary.

use camino::Utf8PathBuf;
use fai_db::{Db, FaiDatabase, Setter, SourceFile};

fn database(source: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

#[track_caller]
fn invalid(source: &str, declaration: &str, message: &str) {
    let (db, file) = database(source);
    let error = crate::entry::prepare(&db, file).expect_err("invalid entry");
    assert_eq!(error.code, crate::INVALID_ENTRY_POINT);
    assert_eq!(error.message, message);
    assert_eq!(error.primary.range().start().raw() as usize, source.find(declaration).unwrap());
    assert_eq!(
        error.primary.range().end().raw() as usize,
        source.find(declaration).unwrap() + declaration.len()
    );
}

#[test]
fn main_cannot_expect_string_from_the_default_runtime() {
    invalid(
        "module Main\npublic main : String -> Unit / { Console }\nlet main text = stdConsole.writeLine text\n",
        "let main text = stdConsole.writeLine text",
        "the selected runtime value does not match `main`'s argument type",
    );
}

#[test]
fn runtime_builder_cannot_require_arguments() {
    invalid(
        "module Main\nlet runtime x = defaultRuntime\npublic main : Runtime -> Unit\nlet main r = ()\n",
        "let runtime x = defaultRuntime",
        "the runtime builder must be a zero-argument value with no unresolved offset evidence",
    );
}

#[test]
fn main_must_be_a_function() {
    invalid(
        "module Main\npublic main : Int\nlet main = 0\n",
        "let main = 0",
        "`main` must accept one runtime value and return `Unit`",
    );
}

#[test]
fn main_cannot_leave_a_second_argument_unapplied() {
    invalid(
        "module Main\npublic main : Runtime -> Int -> Unit\nlet main r x = ()\n",
        "let main r x = ()",
        "`main` must accept one runtime value and return `Unit`",
    );
}

#[test]
fn main_must_return_unit() {
    invalid(
        "module Main\npublic main : Runtime -> Int\nlet main r = 42\n",
        "let main r = 42",
        "`main` must accept one runtime value and return `Unit`",
    );
}

#[test]
fn every_compilation_path_rejects_an_incompatible_runtime() {
    let (db, file) =
        database("module Main\nlet runtime = 0\npublic main : Runtime -> Unit\nlet main r = ()\n");
    let run = crate::jit_run_program(&db, file);
    assert_eq!(run.exit_code, 4);
    assert_eq!(run.diagnostics[0].code, crate::INVALID_ENTRY_POINT);
    let bundle = crate::build_run_bundle(&db, file);
    assert!(bundle.bundle.is_none());
    assert_eq!(bundle.diagnostics[0].code, crate::INVALID_ENTRY_POINT);
    let compile = crate::jit_compile(&db, file).err().expect("invalid retained image");
    assert_eq!(compile[0].code, crate::INVALID_ENTRY_POINT);
}

const OPEN_MAIN: &str = "module Main\npublic main : { console : Console | _ } -> Unit / { Console }\nlet main r = r.console.writeLine \"open\"\n";
const NESTED_MAIN: &str = "module Main\nlet runtime = { extra = 1, inner = { a = 2, console = stdConsole } }\npublic main : { inner : { console : Console | _ } | _ } -> Unit / { Console }\nlet main r = r.inner.console.writeLine \"nested\"\n";
const BUILDER: &str = "module Main\nlet initialize nursery = stdConcurrency.await (stdConcurrency.spawn nursery (fun u -> ()))\nlet runtime =\n  let _ = stdConcurrency.scope initialize\n  { console = stdConsole }\npublic main : { console : Console } -> Unit / { Console }\nlet main r = r.console.writeLine \"initialized\"\n";

#[track_caller]
fn native(source: &str, expected: &str) {
    use wait_timeout::ChildExt;

    let (db, file) = database(source);
    let directory = tempfile::tempdir().unwrap();
    let out = Utf8PathBuf::from_path_buf(directory.path().join("program")).unwrap();
    let result = crate::build_native(&db, file, &out);
    assert!(result.ok, "{:?}", result.diagnostics);
    let mut child = std::process::Command::new(result.artifact.unwrap())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let finished = child.wait_timeout(std::time::Duration::from_secs(30)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(finished && output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), expected);
}

#[test]
fn open_main_receives_its_default_runtime_offsets() {
    native(OPEN_MAIN, "open");
}

#[test]
fn nested_open_main_receives_each_row_offset() {
    native(NESTED_MAIN, "nested");
}

#[test]
fn runtime_initialization_runs_inside_the_scheduler() {
    native(BUILDER, "initialized");
}

#[test]
fn row_polymorphic_launch_adapter_is_shipped_in_the_bundle() {
    let (db, file) = database(OPEN_MAIN);
    let result = crate::build_run_bundle(&db, file);
    let bundle = result.bundle.expect("open entry compiles");
    assert_eq!(bundle.entry.name, "entry#main");
    assert!(bundle.defs.iter().any(|def| def.id == bundle.entry && def.arity == 1));
}

#[test]
fn runtime_builder_effects_enable_concurrent_codegen() {
    let (db, file) = database(BUILDER);
    assert!(crate::entry::prepare(&db, file).unwrap_or_else(|d| panic!("{d:?}")).concurrent);
    assert!(crate::build_run_bundle(&db, file).bundle.unwrap().concurrent);
}

#[test]
fn callable_main_value_is_supported() {
    native(
        "module Main\ngreet : Runtime -> Unit / { Console }\nlet greet r = r.console.writeLine \"alias\"\npublic main : Runtime -> Unit / { Console }\nlet main = greet\n",
        "alias",
    );
}

#[test]
fn instantiated_main_effects_enable_the_scheduler() {
    let source = "module Main\nlet initialize nursery = stdConcurrency.await (stdConcurrency.spawn nursery (fun u -> ()))\nlet runtime = fun u -> stdConcurrency.scope initialize\npublic main : (Unit -> Unit / 'e) -> Unit / 'e\nlet main f = f ()\n";
    let (db, file) = database(source);
    assert!(crate::entry::prepare(&db, file).unwrap().concurrent);
}

#[test]
fn runtime_layout_edits_update_only_the_adapter_and_match_clean() {
    let before_source = "module Main\nlet runtime = { a = 1, console = stdConsole }\npublic main : { console : Console | _ } -> Unit / { Console }\nlet main r = r.console.writeLine \"layout\"\n";
    let (mut db, file) = database(before_source);
    let before = crate::entry::prepare(&db, file).unwrap();
    let after_source = before_source.replace("a = 1", "z = 1");
    db.add_source("Main.fai".into(), after_source.clone());
    let after = crate::entry::prepare(&db, file).unwrap();
    assert_ne!(before.adapter, after.adapter);
    let (clean, clean_file) = database(&after_source);
    assert_eq!(after, crate::entry::prepare(&clean, clean_file).unwrap());
}

const PROJECTED: &str = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let _ = r.console.writeLine \"first\"\n  r.console.writeLine \"second\"\n";

#[test]
fn projected_default_binds_each_real_field_once() {
    let (db, file) = database(PROJECTED);
    let launch = crate::entry::prepare(&db, file).unwrap();
    let (adapter, runtime) = crate::entry::projected_default(&db, file, &launch).unwrap();
    let body = fai_core::pretty_def(&adapter);
    assert_eq!(body.matches("@stdConsole").count(), 1, "{body}");
    assert!(!body.contains("@stdClock") && !body.contains("@stdTls"), "{body}");
    assert!(matches!(runtime.entry().body.kind, fai_core::ExprKind::Lit(fai_core::Lit::Unit)));
    native(PROJECTED, "first\nsecond");
}

#[test]
fn projected_main_keeps_its_synthesized_pipeline() {
    let source = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (Array.sum (Array.range 0 1000)))\n";
    let (db, file) = database(source);
    let launch = crate::entry::prepare(&db, file).unwrap();
    let (adapter, _) = crate::entry::projected_default(&db, file, &launch).unwrap();
    assert!(
        adapter.referenced_globals().iter().any(|def| def.name.as_str().starts_with("fuse#")),
        "{}",
        fai_core::pretty_def(&adapter)
    );
    native(source, "499500");
}

#[test]
fn projected_default_preserves_multiple_capability_fields() {
    let source = "module Main\npublic main : Runtime -> Unit / { Clock, Console, Env }\nlet main r =\n  let _ = r.clock.sleep 0\n  r.console.writeLine (Int.toString (List.length (r.env.args ())))\n";
    let (db, file) = database(source);
    let launch = crate::entry::prepare(&db, file).unwrap();
    let (adapter, _) = crate::entry::projected_default(&db, file, &launch).unwrap();
    let body = fai_core::pretty_def(&adapter);
    assert!(body.find("@stdClock").unwrap() < body.find("@stdConsole").unwrap(), "{body}");
    assert!(body.find("@stdConsole").unwrap() < body.find("@stdEnv").unwrap(), "{body}");
    native(source, "0");
}

#[test]
fn whole_runtime_forwarding_keeps_the_normal_launcher() {
    let source = "module Main\nforward : Int -> Runtime -> Unit / { Console }\nlet forward n r = if n <= 0 then r.console.writeLine \"forwarded\" else forward (n - 1) r\npublic main : Runtime -> Unit / { Console }\nlet main r = forward 1 r\n";
    let (db, file) = database(source);
    let launch = crate::entry::prepare(&db, file).unwrap();
    assert!(crate::entry::projected_default(&db, file, &launch).is_none());
    native(source, "forwarded");
}

#[test]
fn custom_runtime_keeps_the_selected_builder() {
    let source = "module Main\nlet runtime = defaultRuntime\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine \"custom\"\n";
    let (db, file) = database(source);
    let launch = crate::entry::prepare(&db, file).unwrap();
    assert!(crate::entry::projected_default(&db, file, &launch).is_none());
    native(source, "custom");
}

#[test]
fn an_unused_trapping_field_initializer_cannot_be_removed() {
    let (mut db, file) = database(PROJECTED);
    let original = crate::entry::prepare(&db, file).unwrap();
    assert!(crate::entry::projected_default(&db, file, &original).is_some());
    let prelude = fai_resolve::module_file(
        &db,
        fai_resolve::ModuleName(fai_syntax::Symbol::intern("Prelude")),
    )
    .unwrap();
    let edited = prelude.text(&db).replace(
        "let stdRandom = { Random with nextInt n = randomNextInt n }",
        "let stdRandom =\n  let failure = 1 / 0\n  { Random with nextInt n = randomNextInt (n + failure) }",
    );
    prelude.set_text(&mut db).to(edited);
    let launch = crate::entry::prepare(&db, file).unwrap();
    assert!(crate::entry::projected_default(&db, file, &launch).is_none());
    let directory = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(directory.path().join("program")).unwrap();
    let result = crate::build_native(&db, file, &path);
    assert!(result.ok, "{:?}", result.diagnostics);
    let output = std::process::Command::new(result.artifact.unwrap()).output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("division by zero"));
}

#[test]
fn projected_entry_edits_match_clean_specialization() {
    let (mut db, file) = database(PROJECTED);
    let launch = crate::entry::prepare(&db, file).unwrap();
    let before = crate::entry::projected_default(&db, file, &launch);
    let changed = PROJECTED
        .replace("let _ = r.console.writeLine \"first\"", "let _ = r.clock.sleep 0")
        .replace("{ Console }", "{ Clock, Console }");
    db.add_source("Main.fai".into(), changed.clone());
    let launch = crate::entry::prepare(&db, file).unwrap();
    let after = crate::entry::projected_default(&db, file, &launch);
    assert_ne!(before, after);
    let (clean, clean_file) = database(&changed);
    let clean_launch = crate::entry::prepare(&clean, clean_file).unwrap();
    assert_eq!(after, crate::entry::projected_default(&clean, clean_file, &clean_launch));
}

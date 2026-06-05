//! Driver backend tests: native build-and-run, entry-point errors, and the
//! per-definition object-code cache hit.

use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use fai_db::{Db, FaiDatabase, Setter, SourceFile};

use crate::{build_native, object_code};

fn db_with(files: &[(&str, &str)]) -> (FaiDatabase, Vec<SourceFile>) {
    let mut db = FaiDatabase::new();
    fai_types::prelude::load_prelude(&mut db);
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
    let src = "module Hello\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (\"Hello, \" ++ \"Fai!\")\n";
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
    let (db, files) = db_with(&[("M.fai", "module M\n\nlet x = 1\n")]);
    let exe = temp_exe();
    let outcome = build_native(&db, files[0], &exe);
    assert!(!outcome.ok);
    assert!(outcome.diagnostics.iter().any(|d| d.code.as_str() == "FAI0004"));
}

#[test]
fn unsupported_construct_blocks_the_build() {
    // A reachable definition using a tuple (outside the native subset) fails.
    let src = "module M\n\nlet pair = (1, 2)\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (intToString pair)\n";
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
fn editing_one_module_reuses_cached_objects_for_the_others() {
    // Main calls Helper.helper. Editing Helper's *body* must re-run only
    // Helper.helper's object_code; Main.main's stays cached (the cross-module
    // firewall, now at the codegen layer).
    let main = "module Main\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (intToString (Helper.helper 41))\n";
    let helper_v1 = "module Helper\n\npublic helper : Int -> Int\nlet helper x = x + 1\n";
    let helper_v2 = "module Helper\n\npublic helper : Int -> Int\nlet helper x = x + 2\n";
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

//! Regression tests: the recursive `List` operations run in bounded native
//! stack space, so they handle long lists without overflowing.
//!
//! `List.length` folds with a tail-recursive accumulator and `List.foldr` folds
//! left over the reversed list, so both compile to loops; `List.sort`'s own
//! recursion is only logarithmically deep once `length` no longer recurses per
//! cell. Each program here builds a list far longer than the native stack could
//! hold as call frames and exits cleanly (exit code 0, which also means the
//! runtime's end-of-run leak check passed).
//!
//! The runtime's console sink and live-object counter are process-global, so the
//! cases serialize on [`LOCK`].

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_driver::jit_run_program;
use fai_runtime as rt;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Compiles and JIT-runs `src` through the driver, returning `(exit_code, output)`.
fn run(src: &str) -> (i32, String) {
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    rt::capture_start();
    let outcome = jit_run_program(&db, file);
    let out = rt::capture_take();
    (outcome.exit_code, out)
}

/// Wraps an expression that consumes a long list in a `main` that prints it.
fn program(expr: &str) -> String {
    format!(
        "module M\n\npublic main : Runtime -> Unit / {{ Console }}\nlet main rt = rt.console.writeLine (Int.toString ({expr}))\n"
    )
}

/// A length whose call frames would, unguarded, exceed the native stack.
const LONG: i64 = 200_000;
/// Sorting recurses only logarithmically, but each level once called `length`;
/// this length would overflow if `length` still recursed per cell.
const SORTED: i64 = 100_000;

#[test]
fn length_handles_a_very_long_list() {
    let (code, out) = run(&program(&format!("List.length (List.range 0 {LONG})")));
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), LONG.to_string());
}

#[test]
fn foldr_handles_a_very_long_list() {
    // Sum of 0..LONG-1 = LONG*(LONG-1)/2.
    let expected = LONG * (LONG - 1) / 2;
    let (code, out) =
        run(&program(&format!("List.foldr (fun x acc -> x + acc) 0 (List.range 0 {LONG})")));
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), expected.to_string());
}

#[test]
fn sort_handles_a_long_list() {
    let (code, out) = run(&program(&format!("List.length (List.sort (List.range 0 {SORTED}))")));
    assert_eq!(code, 0, "clean (leak-free) exit");
    assert_eq!(out.trim(), SORTED.to_string());
}

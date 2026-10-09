//! Merging unique lists reuses consumed cells rather than retained child aliases.

use std::sync::Mutex;

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

const SOURCE: &str = r#"module Main
public build : Int -> List Int
let build offset = List.map (fun i -> i * 2 + offset) (List.range 0 50)
public merge : List Int -> List Int -> List Int
let merge xs ys =
  match xs with
  | [] -> ys
  | x :: xt ->
    match ys with
    | [] -> xs
    | y :: yt -> if x <= y then x :: merge xt ys else y :: merge xs yt
public verify : List Int -> Int
let verify xs = if xs = List.range 0 100 then 1 else 0
public unchanged : Int -> List Int -> Int
let unchanged offset xs = if xs = build offset then 1 else 0
public main : Runtime -> Unit
let main r = ()
"#;

fn call(program: &mut fai_driver::CompiledProgram, name: &str, args: &[rt::Value]) -> rt::Value {
    let function = program.function(Symbol::intern(name)).unwrap();
    rt::apply(rt::fai_dup(function), args)
}

#[track_caller]
fn merge_allocations(shared: bool) -> i64 {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), SOURCE.into());
    let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
    let baseline = rt::live_count();
    let left = call(&mut program, "build", &[rt::make_int(0)]);
    let right = call(&mut program, "build", &[rt::make_int(1)]);
    let args = if shared { [rt::fai_dup(left), rt::fai_dup(right)] } else { [left, right] };
    rt::reset_allocations();
    let merged = call(&mut program, "merge", &args);
    let allocations = rt::allocations();
    if !shared {
        assert_eq!(merged, left, "the first selected cell supplies the result head");
    }
    assert_eq!(rt::read_int(call(&mut program, "verify", &[merged])), 1);
    if shared {
        assert_eq!(rt::read_int(call(&mut program, "unchanged", &[rt::make_int(0), left])), 1);
        assert_eq!(rt::read_int(call(&mut program, "unchanged", &[rt::make_int(1), right])), 1);
    }
    assert_eq!(rt::live_count(), baseline);
    allocations
}

#[test]
fn unique_list_merge_allocates_no_cells() {
    assert_eq!(merge_allocations(false), 0);
}

#[test]
fn shared_list_merge_copies_only_the_rebuilt_prefix() {
    assert_eq!(merge_allocations(true), 99);
}

#[test]
fn merge_edits_match_clean_reference_counting() {
    let edited = SOURCE.replace("x <= y", "x < y");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("Main.fai", SOURCE)], &[("Main.fai", &edited)]],
        |db, files| {
            (*fai_rc::rc(db, db.source_file(files[0]).unwrap(), Symbol::intern("merge"))).clone()
        },
    );
}

#[test]
fn adding_a_data_alias_matches_clean_reference_counting() {
    let edited = SOURCE.replace("  match xs with", "  let alias = xs\n  match alias with");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("Main.fai", SOURCE)], &[("Main.fai", &edited)]],
        |db, files| {
            (*fai_rc::rc(db, db.source_file(files[0]).unwrap(), Symbol::intern("merge"))).clone()
        },
    );
}

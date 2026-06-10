//! Correctness of the weight-balanced `std/Dict` and `std/Set` against Rust's
//! `BTreeMap`/`BTreeSet` reference, under sorted, reverse-sorted, and scrambled
//! insertion orders (the cases that stress balancing) and at a scale a linear
//! chain could not handle in time. Each Fai program folds the final tree in
//! ascending key order into an order-sensitive checksum and prints it with the
//! size; the Rust reference computes the same from a `BTreeMap`/`BTreeSet`, so a
//! mismatch in content, values, or order is caught. The large key sequences are
//! built with `List.range` (not list literals, which the front end lowers
//! recursively), matching the standard-library stack-safety tests.
//!
//! The runtime's console sink and live-object counter are process-global, so the
//! cases serialize on [`LOCK`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_driver::jit_run_program;
use fai_runtime as rt;

static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Compiles and JIT-runs `src`, returning its trimmed stdout (and asserting a
/// clean, leak-free exit).
#[track_caller]
fn run(src: &str) -> String {
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).expect("source registered");
    rt::capture_start();
    let outcome = jit_run_program(&db, file);
    let out = rt::capture_take();
    assert_eq!(outcome.exit_code, 0, "clean (leak-free) exit:\n{src}");
    out.trim().to_owned()
}

/// The order-sensitive checksum the Fai programs compute, folding ascending:
/// `acc' = acc*31 + k + v` (wrapping, matching `Int`).
fn dict_checksum(entries: &BTreeMap<i64, i64>) -> String {
    let sum = entries
        .iter()
        .fold(0i64, |acc, (k, v)| acc.wrapping_mul(31).wrapping_add(*k).wrapping_add(*v));
    format!("{sum} {}", entries.len())
}

fn set_checksum(entries: &BTreeSet<i64>) -> String {
    let sum = entries.iter().fold(0i64, |acc, &x| acc.wrapping_mul(31).wrapping_add(x));
    format!("{sum} {}", entries.len())
}

/// A scrambled, duplicate-containing key sequence of length `n` (a multiplicative
/// hash modulo `n`), matching the Fai expression below.
fn scrambled(n: i64) -> Vec<i64> {
    (0..n).map(|k| (k * 2_654_435_761 + 12345) % n).collect()
}
const SCRAMBLE_FAI: &str =
    "List.map (fun k -> (k * 2654435761 + 12345) % SCRAMBLE_N) (List.range 0 SCRAMBLE_N)";

// --- Dict ------------------------------------------------------------------

/// Builds a `Dict` from the keys produced by the Fai expression `keys_expr`
/// (value = key + 1, later duplicates win), folds it ascending into the
/// checksum, and checks it (and the size) against a `BTreeMap` of the same keys.
#[track_caller]
fn check_dict(keys_expr: &str, keys: &[i64]) {
    let src = format!(
        "module M\n\npublic main : Runtime -> Unit\nlet main rt =\n  \
         let d = Dict.fromList (List.map (fun k -> (k, k + 1)) ({keys_expr}))\n  \
         rt.console.writeLine (Int.toString (Dict.foldl (fun k v acc -> acc * 31 + k + v) 0 d) ++ \" \" ++ Int.toString (Dict.size d))\n"
    );
    let reference: BTreeMap<i64, i64> = keys.iter().map(|&k| (k, k + 1)).collect();
    assert_eq!(run(&src), dict_checksum(&reference), "Dict from {keys_expr}");
}

#[test]
fn dict_sorted_insertion_matches_btreemap() {
    check_dict("List.range 0 2000", &(0..2000).collect::<Vec<_>>());
}

#[test]
fn dict_reverse_insertion_matches_btreemap() {
    check_dict("List.reverse (List.range 0 2000)", &(0..2000).collect::<Vec<_>>());
}

#[test]
fn dict_scrambled_insertion_matches_btreemap() {
    check_dict(&SCRAMBLE_FAI.replace("SCRAMBLE_N", "1500"), &scrambled(1500));
}

#[test]
fn dict_small_and_empty_match_btreemap() {
    check_dict("[]", &[]);
    check_dict("[42]", &[42]);
    check_dict("[3, 1, 2, 1, 3]", &[3, 1, 2, 1, 3]);
}

#[test]
fn dict_set_ops_match_btreemap() {
    let a: BTreeMap<i64, i64> = (0..400).map(|k| (k, k + 1)).collect();
    let b: BTreeMap<i64, i64> = (200..600).map(|k| (k, k + 7)).collect();
    let cases: [(&str, &str, BTreeMap<i64, i64>); 3] = [
        // `union`/`intersection` are left-biased (keep `a`'s value).
        ("union", "Dict.union", b.iter().chain(a.iter()).map(|(&k, &v)| (k, v)).collect()),
        (
            "intersection",
            "Dict.intersection",
            a.iter().filter(|(k, _)| b.contains_key(k)).map(|(&k, &v)| (k, v)).collect(),
        ),
        (
            "difference",
            "Dict.difference",
            a.iter().filter(|(k, _)| !b.contains_key(k)).map(|(&k, &v)| (k, v)).collect(),
        ),
    ];
    for (op, fai, result) in cases {
        let src = format!(
            "module M\n\npublic main : Runtime -> Unit\nlet main rt =\n  \
             let da = Dict.fromList (List.map (fun k -> (k, k + 1)) (List.range 0 400))\n  \
             let db = Dict.fromList (List.map (fun k -> (k, k + 7)) (List.range 200 600))\n  \
             let d = {fai} da db\n  \
             rt.console.writeLine (Int.toString (Dict.foldl (fun k v acc -> acc * 31 + k + v) 0 d) ++ \" \" ++ Int.toString (Dict.size d))\n"
        );
        assert_eq!(run(&src), dict_checksum(&result), "Dict.{op}");
    }
}

// --- Set -------------------------------------------------------------------

/// Builds a `Set` from `keys_expr`, folds it ascending into the checksum, and
/// checks it (and the size) against a `BTreeSet` of the same keys.
#[track_caller]
fn check_set(keys_expr: &str, keys: &[i64]) {
    let src = format!(
        "module M\n\npublic main : Runtime -> Unit\nlet main rt =\n  \
         let s = Set.fromList ({keys_expr})\n  \
         rt.console.writeLine (Int.toString (Set.foldl (fun x acc -> acc * 31 + x) 0 s) ++ \" \" ++ Int.toString (Set.size s))\n"
    );
    let reference: BTreeSet<i64> = keys.iter().copied().collect();
    assert_eq!(run(&src), set_checksum(&reference), "Set from {keys_expr}");
}

#[test]
fn set_sorted_reverse_scrambled_match_btreeset() {
    check_set("List.range 0 2000", &(0..2000).collect::<Vec<_>>());
    check_set("List.reverse (List.range 0 2000)", &(0..2000).collect::<Vec<_>>());
    check_set(&SCRAMBLE_FAI.replace("SCRAMBLE_N", "1500"), &scrambled(1500));
}

#[test]
fn set_small_and_empty_match_btreeset() {
    check_set("[]", &[]);
    check_set("[42]", &[42]);
    check_set("[3, 1, 2, 1, 3]", &[3, 1, 2, 1, 3]);
}

#[test]
fn set_ops_match_btreeset() {
    let a: BTreeSet<i64> = (0..400).collect();
    let b: BTreeSet<i64> = (200..600).collect();
    let cases: [(&str, &str, BTreeSet<i64>); 3] = [
        ("union", "Set.union", a.union(&b).copied().collect()),
        ("intersection", "Set.intersection", a.intersection(&b).copied().collect()),
        ("difference", "Set.difference", a.difference(&b).copied().collect()),
    ];
    for (op, fai, result) in cases {
        let src = format!(
            "module M\n\npublic main : Runtime -> Unit\nlet main rt =\n  \
             let s = {fai} (Set.fromList (List.range 0 400)) (Set.fromList (List.range 200 600))\n  \
             rt.console.writeLine (Int.toString (Set.foldl (fun x acc -> acc * 31 + x) 0 s) ++ \" \" ++ Int.toString (Set.size s))\n"
        );
        assert_eq!(run(&src), set_checksum(&result), "Set.{op}");
    }
}

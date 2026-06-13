//! Correctness of the open-addressing `std/HashDict` and `std/HashSet` against
//! Rust's `HashMap`/`HashSet` reference, under sorted, reverse-sorted, and
//! scrambled insertion orders (which stress probing and resizing) and at a scale
//! that forces many doublings. Each Fai program folds the final table into an
//! **order-independent** checksum (a commutative sum, since hash iteration order is
//! unspecified) and prints it with the size; the Rust reference computes the same
//! from a `HashMap`/`HashSet`, so a mismatch in content, values, or count is
//! caught. Removal is exercised too (the backward-shift path), and the harness
//! asserts a clean, leak-free exit. Large key sequences use `List.range` (not list
//! literals, which the front end lowers recursively).
//!
//! The runtime's console sink and live-object counter are process-global, so the
//! cases serialize on [`LOCK`].

use std::collections::{HashMap, HashSet};
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

/// JIT-runs `src` and returns its trimmed stdout plus the number of array
/// copy-on-write copies it performed (the uniqueness-loss signal). The counter is
/// process-global, so the whole sequence runs under one lock acquisition.
#[track_caller]
fn run_array_copies(src: &str) -> (String, i64) {
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).expect("source registered");
    rt::capture_start();
    let before = rt::array_copies();
    let outcome = jit_run_program(&db, file);
    let copies = rt::array_copies() - before;
    let out = rt::capture_take();
    assert_eq!(outcome.exit_code, 0, "clean (leak-free) exit:\n{src}");
    (out.trim().to_owned(), copies)
}

/// The order-independent checksum the Fai programs compute over a map: the
/// commutative sum `Σ (k * 1000003 + v)` (wrapping, matching `Int`), with the size.
fn dict_checksum(entries: &HashMap<i64, i64>) -> String {
    let sum = entries
        .iter()
        .fold(0i64, |acc, (&k, &v)| acc.wrapping_add(k.wrapping_mul(1_000_003).wrapping_add(v)));
    format!("{sum} {}", entries.len())
}

/// The order-independent checksum over a set: `Σ (x * 1000003)`, with the size.
fn set_checksum(entries: &HashSet<i64>) -> String {
    let sum = entries.iter().fold(0i64, |acc, &x| acc.wrapping_add(x.wrapping_mul(1_000_003)));
    format!("{sum} {}", entries.len())
}

/// A scrambled, duplicate-containing key sequence of length `n` (a multiplicative
/// hash modulo `n`), matching the Fai expression below.
fn scrambled(n: i64) -> Vec<i64> {
    (0..n).map(|k| (k * 2_654_435_761 + 12345) % n).collect()
}
const SCRAMBLE_FAI: &str =
    "List.map (fun k -> (k * 2654435761 + 12345) % SCRAMBLE_N) (List.range 0 SCRAMBLE_N)";

const DICT_FOLD: &str = "HashDict.foldl (fun k v acc -> acc + k * 1000003 + v) 0 d";
const SET_FOLD: &str = "HashSet.foldl (fun x acc -> acc + x * 1000003) 0 s";

// --- HashDict --------------------------------------------------------------

/// Builds a `HashDict` from `keys_expr` (value = key + 1, later duplicates win),
/// folds it commutatively into the checksum, and checks it (and the size) against a
/// `HashMap` of the same keys.
#[track_caller]
fn check_dict(keys_expr: &str, keys: &[i64]) {
    let src = format!(
        "module M\n\npublic main : Runtime -> Unit / {{ Console }}\nlet main rt =\n  \
         let d = HashDict.fromList (List.map (fun k -> (k, k + 1)) ({keys_expr}))\n  \
         rt.console.writeLine (Int.toString ({DICT_FOLD}) ++ \" \" ++ Int.toString (HashDict.size d))\n"
    );
    let reference: HashMap<i64, i64> = keys.iter().map(|&k| (k, k + 1)).collect();
    assert_eq!(run(&src), dict_checksum(&reference), "HashDict from {keys_expr}");
}

#[test]
fn dict_sorted_insertion_matches_hashmap() {
    check_dict("List.range 0 2000", &(0..2000).collect::<Vec<_>>());
}

#[test]
fn dict_reverse_insertion_matches_hashmap() {
    check_dict("List.reverse (List.range 0 2000)", &(0..2000).collect::<Vec<_>>());
}

#[test]
fn dict_scrambled_insertion_matches_hashmap() {
    check_dict(&SCRAMBLE_FAI.replace("SCRAMBLE_N", "1500"), &scrambled(1500));
}

#[test]
fn dict_small_and_empty_match_hashmap() {
    check_dict("[]", &[]);
    check_dict("[42]", &[42]);
    check_dict("[3, 1, 2, 1, 3]", &[3, 1, 2, 1, 3]);
}

#[test]
fn dict_remove_half_matches_hashmap() {
    // Build 0..2000, then remove 0..1000 (the backward-shift path at scale),
    // leaving 1000..2000.
    let src = format!(
        "module M\n\npublic main : Runtime -> Unit / {{ Console }}\nlet main rt =\n  \
         let full = HashDict.fromList (List.map (fun k -> (k, k + 1)) (List.range 0 2000))\n  \
         let d = List.foldl (fun acc k -> HashDict.remove k acc) full (List.range 0 1000)\n  \
         rt.console.writeLine (Int.toString ({DICT_FOLD}) ++ \" \" ++ Int.toString (HashDict.size d))\n"
    );
    let reference: HashMap<i64, i64> = (1000..2000).map(|k| (k, k + 1)).collect();
    assert_eq!(run(&src), dict_checksum(&reference), "HashDict after removing the first half");
}

#[test]
fn dict_set_ops_match_hashmap() {
    let a: HashMap<i64, i64> = (0..400).map(|k| (k, k + 1)).collect();
    let b: HashMap<i64, i64> = (200..600).map(|k| (k, k + 7)).collect();
    let cases: [(&str, &str, HashMap<i64, i64>); 3] = [
        // `union`/`intersection` are left-biased (keep `a`'s value).
        ("union", "HashDict.union", b.iter().chain(a.iter()).map(|(&k, &v)| (k, v)).collect()),
        (
            "intersection",
            "HashDict.intersection",
            a.iter().filter(|(k, _)| b.contains_key(k)).map(|(&k, &v)| (k, v)).collect(),
        ),
        (
            "difference",
            "HashDict.difference",
            a.iter().filter(|(k, _)| !b.contains_key(k)).map(|(&k, &v)| (k, v)).collect(),
        ),
    ];
    for (op, fai, result) in cases {
        let src = format!(
            "module M\n\npublic main : Runtime -> Unit / {{ Console }}\nlet main rt =\n  \
             let da = HashDict.fromList (List.map (fun k -> (k, k + 1)) (List.range 0 400))\n  \
             let db = HashDict.fromList (List.map (fun k -> (k, k + 7)) (List.range 200 600))\n  \
             let d = {fai} da db\n  \
             rt.console.writeLine (Int.toString ({DICT_FOLD}) ++ \" \" ++ Int.toString (HashDict.size d))\n"
        );
        assert_eq!(run(&src), dict_checksum(&result), "HashDict.{op}");
    }
}

#[test]
fn dict_unique_build_updates_in_place() {
    // A dict threaded through a tail recursion stays uniquely owned, so every
    // insert mutates the backing array in place — no copy-on-write — even as the
    // table doubles several times (the rehash builds a fresh array, not a copy).
    let src = "module M\n\n\
         go : Int -> HashDict Int Int -> HashDict Int Int\n\
         let go i d = if i >= 2000 then d else go (i + 1) (HashDict.insert i i d)\n\n\
         public main : Runtime -> Unit / { Console }\n\
         let main rt = rt.console.writeLine (Int.toString (HashDict.size (go 0 HashDict.empty)))\n";
    let (out, copies) = run_array_copies(src);
    assert_eq!(out, "2000");
    assert_eq!(copies, 0, "a unique HashDict build does no copy-on-write array copies");
}

#[test]
fn dict_shared_insert_copies() {
    // Inserting into a dict still referenced elsewhere must copy the backing array
    // (value semantics), so the copy counter rises — the in-place build above is
    // the unique-owner fast path, not a leak of mutation.
    let src = "module M\n\n\
         go : Int -> HashDict Int Int -> HashDict Int Int\n\
         let go i d = if i >= 50 then d else go (i + 1) (HashDict.insert i i d)\n\n\
         public main : Runtime -> Unit / { Console }\n\
         let main rt =\n  \
           let base = go 0 HashDict.empty\n  \
           let a = HashDict.insert 5000 1 base\n  \
           let b = HashDict.insert 6000 2 base\n  \
           rt.console.writeLine (Int.toString (HashDict.size a + HashDict.size b))\n";
    let (out, copies) = run_array_copies(src);
    assert_eq!(out, "102");
    assert!(copies > 0, "inserting into a shared HashDict copies its array");
}

// --- HashSet ---------------------------------------------------------------

/// Builds a `HashSet` from `keys_expr`, folds it commutatively into the checksum,
/// and checks it (and the size) against a `HashSet` of the same keys.
#[track_caller]
fn check_set(keys_expr: &str, keys: &[i64]) {
    let src = format!(
        "module M\n\npublic main : Runtime -> Unit / {{ Console }}\nlet main rt =\n  \
         let s = HashSet.fromList ({keys_expr})\n  \
         rt.console.writeLine (Int.toString ({SET_FOLD}) ++ \" \" ++ Int.toString (HashSet.size s))\n"
    );
    let reference: HashSet<i64> = keys.iter().copied().collect();
    assert_eq!(run(&src), set_checksum(&reference), "HashSet from {keys_expr}");
}

#[test]
fn set_sorted_reverse_scrambled_match_hashset() {
    check_set("List.range 0 2000", &(0..2000).collect::<Vec<_>>());
    check_set("List.reverse (List.range 0 2000)", &(0..2000).collect::<Vec<_>>());
    check_set(&SCRAMBLE_FAI.replace("SCRAMBLE_N", "1500"), &scrambled(1500));
}

#[test]
fn set_small_and_empty_match_hashset() {
    check_set("[]", &[]);
    check_set("[42]", &[42]);
    check_set("[3, 1, 2, 1, 3]", &[3, 1, 2, 1, 3]);
}

#[test]
fn set_remove_half_matches_hashset() {
    let src = format!(
        "module M\n\npublic main : Runtime -> Unit / {{ Console }}\nlet main rt =\n  \
         let full = HashSet.fromList (List.range 0 2000)\n  \
         let s = List.foldl (fun acc k -> HashSet.remove k acc) full (List.range 0 1000)\n  \
         rt.console.writeLine (Int.toString ({SET_FOLD}) ++ \" \" ++ Int.toString (HashSet.size s))\n"
    );
    let reference: HashSet<i64> = (1000..2000).collect();
    assert_eq!(run(&src), set_checksum(&reference), "HashSet after removing the first half");
}

#[test]
fn set_ops_match_hashset() {
    let a: HashSet<i64> = (0..400).collect();
    let b: HashSet<i64> = (200..600).collect();
    let cases: [(&str, &str, HashSet<i64>); 3] = [
        ("union", "HashSet.union", a.union(&b).copied().collect()),
        ("intersection", "HashSet.intersection", a.intersection(&b).copied().collect()),
        ("difference", "HashSet.difference", a.difference(&b).copied().collect()),
    ];
    for (op, fai, result) in cases {
        let src = format!(
            "module M\n\npublic main : Runtime -> Unit / {{ Console }}\nlet main rt =\n  \
             let s = {fai} (HashSet.fromList (List.range 0 400)) (HashSet.fromList (List.range 200 600))\n  \
             rt.console.writeLine (Int.toString ({SET_FOLD}) ++ \" \" ++ Int.toString (HashSet.size s))\n"
        );
        assert_eq!(run(&src), set_checksum(&result), "HashSet.{op}");
    }
}

// --- Niche `Option`-typed keys ---------------------------------------------
//
// `Option String` is niche-encoded (Scheme A: `Some s` is the bare string
// pointer, `None` an immediate), so these exercise hashing and membership of
// niche keys end-to-end: a key is standardized when stored in a uniform `Array`
// slot, so a lookup must hash it the same way. The inline hash path's niche
// carve-out is what keeps the two consistent (its unit pin is the codegen
// `hash_niche_option_uses_standard_form` test); here the whole container must
// agree against a Rust reference.

#[test]
fn set_option_string_keys_round_trip() {
    // A `Some`/`None` mix with a duplicate `Some "a"`: four distinct keys, every
    // inserted key found, an absent `Some` and the absent... (there is no second
    // `None`) reported absent.
    let src = "module M\n\n\
         public main : Runtime -> Unit / { Console }\n\
         let main rt =\n  \
           let s = HashSet.fromList [Some \"a\", Some \"b\", None, Some \"a\", Some \"c\"]\n  \
           let present =\n    \
             HashSet.member (Some \"a\") s && HashSet.member (Some \"b\") s\n      \
             && HashSet.member None s && HashSet.member (Some \"c\") s\n  \
           let absent = not (HashSet.member (Some \"z\") s)\n  \
           rt.console.writeLine\n    \
             (Int.toString (HashSet.size s) ++ \" \" ++ (if present && absent then \"ok\" else \"bad\"))\n";
    assert_eq!(run(src), "4 ok", "HashSet of Option String keys round-trips");
}

#[test]
fn dict_option_string_keys_round_trip() {
    // `Some "a"` inserted twice (later value wins), `None` a key in its own right,
    // an absent key falling back to the default.
    let src = "module M\n\n\
         public main : Runtime -> Unit / { Console }\n\
         let main rt =\n  \
           let d = HashDict.fromList [(Some \"a\", 1), (Some \"b\", 2), (None, 3), (Some \"a\", 9)]\n  \
           let va = HashDict.getOr 0 (Some \"a\") d\n  \
           let vn = HashDict.getOr 0 None d\n  \
           let vz = HashDict.getOr 0 (Some \"z\") d\n  \
           rt.console.writeLine\n    \
             (Int.toString (HashDict.size d) ++ \" \" ++ Int.toString va\n      \
              ++ \" \" ++ Int.toString vn ++ \" \" ++ Int.toString vz)\n";
    // size 3 (Some a, Some b, None); a -> 9 (later wins); None -> 3; z -> 0 default.
    assert_eq!(run(src), "3 9 3 0", "HashDict of Option String keys round-trips");
}

#[test]
fn set_option_string_keys_scale() {
    // 500 distinct `Some "<n>"` keys (forcing several doublings/rehashes of niche
    // keys), plus probing for present and absent keys including `None`.
    let src = "module M\n\n\
         public main : Runtime -> Unit / { Console }\n\
         let main rt =\n  \
           let s = HashSet.fromList (List.map (fun k -> Some (Int.toString k)) (List.range 0 500))\n  \
           let present =\n    \
             HashSet.member (Some \"0\") s && HashSet.member (Some \"250\") s\n      \
             && HashSet.member (Some \"499\") s\n  \
           let absent = not (HashSet.member (Some \"500\") s) && not (HashSet.member None s)\n  \
           rt.console.writeLine\n    \
             (Int.toString (HashSet.size s) ++ \" \" ++ (if present && absent then \"ok\" else \"bad\"))\n";
    assert_eq!(run(src), "500 ok", "HashSet of Option String keys scales");
}

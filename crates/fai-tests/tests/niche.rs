//! End-to-end tests of the niche `Option` representation (a monomorphic
//! `Option` whose `Some` carries no wrapper cell). These exercise the
//! representation through code generation and reference counting: correctness of
//! construction/match across direct, mutually-recursive, and tail-loop calls;
//! that a niche `Some` allocates nothing; and — safety-critically — the boundary
//! conversions where a niche value crosses into a uniform slot (a generic
//! combinator, a `List`, a first-class call, a structural comparison), each of
//! which must convert to and from the standard boxed representation without a
//! leak or a corrupt read.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_driver::jit_run_program;
use fai_runtime as rt;

// Each run installs a process-global stdout capture and the allocation counters,
// so the runs are serialized.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Compiles and JIT-runs `src`, returning `(exit_code, stdout, allocations)`. An
/// exit code of 0 implies a leak-free run (the runtime aborts otherwise).
fn run(src: &str) -> (i32, String, i64) {
    let _g = lock();
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).expect("source registered");
    rt::capture_start();
    rt::reset_allocations();
    let outcome = jit_run_program(&db, file);
    let allocs = rt::allocations();
    let out = rt::capture_take();
    (outcome.exit_code, out.trim().to_owned(), allocs)
}

/// Wraps `body` (an `Int` expression) plus `defs` in a `main` that prints it, and
/// asserts a clean (leak-free) exit and the expected output.
#[track_caller]
fn outputs(defs: &str, body: &str, expect: &str) {
    let src = format!(
        "module M\n\n{defs}\n\npublic main : Runtime -> Unit / {{ Console }}\nlet main rt = rt.console.writeLine (Int.toString ({body}))\n"
    );
    let (code, out, _) = run(&src);
    assert_eq!(code, 0, "clean (leak-free) exit; output:\n{out}");
    assert_eq!(out, expect, "output");
}

#[test]
fn niche_some_and_none_nonrecursive() {
    let defs = "mk : Int -> Option (Int * Int)\n\
                let mk n = if n > 0 then Some (n, n + 1) else None\n\
                use : Option (Int * Int) -> Int\n\
                let use o = match o with | Some (a, b) -> a + b | None -> -1";
    outputs(defs, "use (mk 5)", "11");
    outputs(defs, "use (mk 0)", "-1");
}

#[test]
fn niche_threaded_through_mutual_recursion() {
    // `f`/`g` are mutually recursive and both return a niche `Option`; the niche
    // must survive the combined-loop/member-wrapper boundary.
    let defs = "f : Int -> Option (Int * Int)\n\
                let f n = if n <= 0 then Some (n, n + 10) else g (n - 1)\n\
                g : Int -> Option (Int * Int)\n\
                let g n = if n = 1 then None else f (n - 1)";
    outputs(defs, "match f 5 with | Some (a, b) -> a + b | None -> -1", "8");
    outputs(defs, "match f 6 with | Some (a, b) -> a + b | None -> -1", "-1");
}

#[test]
fn niche_threaded_through_a_tail_loop() {
    // A tail-recursive (loop) function returning a niche `Option`: the loop exit
    // must carry the niche representation.
    let defs = "go : Int -> Int -> Option (Int * Int)\n\
                let go n acc = if n <= 0 then Some (acc, n) else go (n - 1) (acc + n)";
    outputs(defs, "match go 5 0 with | Some (a, b) -> a + b | None -> -1", "15");
}

#[test]
fn niche_some_allocates_no_wrapper() {
    // Build and immediately destructure `Some (i, i)` 1000 times. Only the tuple
    // payload allocates; the niche `Some` adds none — so the count stays near 1000
    // (one per iteration), not ~2000 (a wrapper cell each).
    let src = "module M\n\
        mk : Int -> Option (Int * Int)\n\
        let mk i = Some (i, i)\n\
        sum : Int -> Int -> Int\n\
        let sum i acc =\n\
        \x20 if i <= 0 then acc else (match mk i with | Some (a, b) -> sum (i - 1) (acc + a + b) | None -> acc)\n\
        public main : Runtime -> Unit / { Console }\n\
        let main rt = rt.console.writeLine (Int.toString (sum 1000 0))\n";
    let (code, out, allocs) = run(src);
    assert_eq!(code, 0, "clean exit");
    assert_eq!(out, "1001000");
    assert!(allocs < 1500, "a niche `Some` allocates no wrapper; allocs={allocs} (expected ~1000)");
}

#[test]
fn niche_converts_through_generic_combinator() {
    // A niche `Option (Int * Int)` passed to the generic `Option.map` (which sees
    // a standard boxed `Option`): converted to standard at the call, back to niche
    // on the result, then matched.
    let defs = "mk : Int -> Option (Int * Int)\n\
                let mk n = Some (n, n)\n\
                bump : Option (Int * Int) -> Option (Int * Int)\n\
                let bump o = Option.map (fun p -> match p with | (a, b) -> (a + 1, b + 1)) o";
    outputs(defs, "match bump (mk 5) with | Some (a, b) -> a + b | None -> -1", "12");
}

#[test]
fn niche_converts_into_and_out_of_a_list() {
    // Niche `Option`s stored in a `List` (a uniform slot) become standard; reading
    // them back and matching converts as needed. Exercises the niche<->standard
    // boundary in both directions, leak-free.
    let defs = "mk : Int -> Option (Int * Int)\n\
                let mk n = if n % 2 = 0 then Some (n, n) else None\n\
                build : Int -> List (Option (Int * Int))\n\
                let build n = if n <= 0 then [] else mk n :: build (n - 1)\n\
                total : List (Option (Int * Int)) -> Int\n\
                let total xs = List.foldl (fun acc o -> match o with | Some (a, b) -> acc + a + b | None -> acc) 0 xs";
    outputs(defs, "total (build 6)", "24");
}

#[test]
fn niche_used_first_class() {
    // Using a niche-returning function first-class (through `apply_n`) forces the
    // value through the standard-ABI wrapper, which converts the result.
    let defs = "mk : Int -> Option (Int * Int)\n\
                let mk n = Some (n, n + 1)\n\
                apply : (Int -> Option (Int * Int)) -> Int -> Option (Int * Int)\n\
                let apply f x = f x";
    outputs(defs, "match apply mk 5 with | Some (a, b) -> a + b | None -> -1", "11");
}

#[test]
fn niche_b_int_payload() {
    // Scheme B: an `Option Int` (immediate payload) threaded monomorphically.
    let defs = "mk : Int -> Option Int\n\
                let mk n = if n > 0 then Some n else None\n\
                use : Option Int -> Int\n\
                let use o = match o with | Some x -> x * 2 | None -> -1";
    outputs(defs, "use (mk 7)", "14");
    outputs(defs, "use (mk 0)", "-1");
}

#[test]
fn niche_b_list_payload() {
    // Scheme B with a `List` payload: `None`=sentinel, `Some []`=immediate, `Some
    // (x :: xs)`=a cons pointer — all distinct.
    let defs = "mk : Int -> Option (List Int)\n\
                let mk n = if n > 0 then Some [n, n + 1] else (if n = 0 then Some [] else None)\n\
                use : Option (List Int) -> Int\n\
                let use o = match o with | Some xs -> List.length xs | None -> -1";
    outputs(defs, "use (mk 3)", "2");
    outputs(defs, "use (mk 0)", "0");
    outputs(defs, "use (mk (0 - 1))", "-1");
}

#[test]
fn niche_b_equality_and_ordering() {
    // Scheme B equality and ordering work directly on the niche encoding (the
    // `KIND_NONE` sentinel), with no conversion.
    let defs = "a : Option Int\n\
                let a = Some 1\n\
                b : Option Int\n\
                let b = Some 2\n\
                n : Option Int\n\
                let n = None";
    outputs(defs, "if a = a then 1 else 0", "1");
    outputs(defs, "if a = b then 1 else 0", "0");
    outputs(defs, "if a = n then 1 else 0", "0");
    outputs(defs, "if n = n then 1 else 0", "1");
    outputs(defs, "if n < a then 1 else 0", "1");
    outputs(defs, "if a < b then 1 else 0", "1");
    outputs(defs, "if b < a then 1 else 0", "0");
    outputs(defs, "if a < n then 1 else 0", "0");
}

#[test]
fn niche_b_converts_through_generic() {
    // A Scheme-B niche `Option Int` through generic `Option.withDefault` and
    // `Option.map` (converted to standard at the boundary, back on return).
    let defs = "mk : Int -> Option Int\n\
                let mk n = if n > 0 then Some n else None\n\
                bump : Option Int -> Option Int\n\
                let bump o = Option.map (fun x -> x + 100) o";
    outputs(defs, "Option.withDefault 0 (mk 9)", "9");
    outputs(defs, "Option.withDefault 0 (mk 0)", "0");
    outputs(defs, "match bump (mk 5) with | Some x -> x | None -> -1", "105");
}

#[test]
fn niche_equality_and_ordering() {
    // Structural `=` works directly on the niche representation; ordering converts
    // to standard. Both must agree with the standard semantics.
    let defs = "a : Option (Int * Int)\n\
                let a = Some (1, 2)\n\
                b : Option (Int * Int)\n\
                let b = Some (1, 3)\n\
                n : Option (Int * Int)\n\
                let n = None";
    outputs(defs, "if a = a then 1 else 0", "1");
    outputs(defs, "if a = b then 1 else 0", "0");
    outputs(defs, "if a = n then 1 else 0", "0");
    outputs(defs, "if n = n then 1 else 0", "1");
    // Ordering: None < Some, and Some compares by payload.
    outputs(defs, "if n < a then 1 else 0", "1");
    outputs(defs, "if a < b then 1 else 0", "1");
    outputs(defs, "if b < a then 1 else 0", "0");
    outputs(defs, "if a < a then 1 else 0", "0");
}

#[test]
fn niche_int_threaded_loop_allocates_independently_of_iterations() {
    // A niche `Option Int` threaded through a fallible-division chain, an `orElse`
    // fallback, and a summing loop (the shape of the OptionEval benchmark). The
    // niche `Some` carries no cell, and the niche representation is preserved
    // across the `match` merges and the loop carry, so the loop allocates a fixed
    // amount regardless of the iteration count. A per-iteration niche/standard
    // round-trip (which heap-allocates a `Some` wrapper each iteration) would make
    // the allocation count grow with `n`.
    let defs = r#"safeDiv : Int -> Int -> Option Int
let safeDiv a b = if b = 0 then None else Some (a / b)
orElse : Option Int -> Option Int -> Option Int
let orElse a b =
  match a with
  | None -> b
  | Some x -> Some x
evalChain : Int -> Option Int
let evalChain i =
  match safeDiv (i * i) (i % 3) with
  | None -> None
  | Some x ->
    match safeDiv x (i % 4) with
    | None -> None
    | Some y -> safeDiv (x + y) (i % 5)
evalAt : Int -> Option Int
let evalAt i = orElse (evalChain i) (evalChain (i + 1))
sumOk : Int -> Int -> Int -> Int
let sumOk i n acc =
  if i >= n then
    acc
  else
    match evalAt i with
    | None -> sumOk (i + 1) n acc
    | Some v -> sumOk (i + 1) n (acc + v)"#;
    let prog = |n: i64| {
        format!(
            "module M\n\n{defs}\n\npublic main : Runtime -> Unit / {{ Console }}\nlet main rt = rt.console.writeLine (Int.toString (sumOk 0 {n} 0))\n"
        )
    };
    let (code_a, _, allocs_a) = run(&prog(200));
    let (code_b, _, allocs_b) = run(&prog(400));
    assert_eq!(code_a, 0, "clean (leak-free) exit at n=200");
    assert_eq!(code_b, 0, "clean (leak-free) exit at n=400");
    assert_eq!(
        allocs_a, allocs_b,
        "a niche Option Int threaded through a loop must allocate independently of \
         the iteration count (no per-iteration niche/standard round-trip): \
         {allocs_a} at n=200 vs {allocs_b} at n=400"
    );
}

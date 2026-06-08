//! End-to-end reuse, in-place update, and borrowing tests.
//!
//! These compile and JIT-run real programs and compare the cumulative heap
//! allocation count between a *unique* and a *shared* version of the same
//! computation. Reuse is invisible to the result but visible here: rebuilding a
//! unique data structure recycles its cells in place (no fresh allocations),
//! while a shared one must copy. Each program also asserts a clean, leak-free
//! exit and the correct output, so a reuse bug shows up as corruption, a leak, or
//! an allocation that should not have happened.

use indoc::formatdoc;

use crate::tests::run_counted;

/// A program with `build`/`sum`/`len` list helpers plus the injected `defs`,
/// whose `main` prints `Int.toString (use (build n))`. `use_body` references the
/// list parameter `xs`.
fn prog(defs: &str, use_body: &str, n: i32) -> String {
    formatdoc! {r#"
        module M

        let build k = if k <= 0 then [] else k :: build (k - 1)

        let sum xs =
          match xs with
          | [] -> 0
          | x :: rest -> x + sum rest

        let len xs =
          match xs with
          | [] -> 0
          | _ :: rest -> 1 + len rest

        {defs}

        let use xs = {use_body}

        public main : Runtime -> Unit
        let main rt = rt.console.writeLine (Int.toString (use (build {n})))
    "#}
}

/// Runs `src`, asserting a clean exit and `expect` output (trimmed); returns the
/// cumulative allocation count.
#[track_caller]
fn allocs(src: &str, expect: &str) -> i64 {
    let (code, out, a) = run_counted(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{src}");
    assert_eq!(out.trim(), expect, "output:\n{src}");
    a
}

/// Runs `src`, asserting a clean exit and `expect` output (trimmed).
#[track_caller]
fn outputs(src: &str, expect: &str) {
    let (code, out) = crate::tests::run(src);
    assert_eq!(code, 0, "clean (leak-free) exit:\n{src}");
    assert_eq!(out.trim(), expect, "output:\n{src}");
}

// ===========================================================================
// List rebuilders: a unique spine is recycled in place; a shared one is copied.
// The shared version allocates exactly one fresh cons cell per element (50).
// ===========================================================================

const INC: &str =
    "let inc xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> (x + 1) :: inc rest";
const DBL: &str =
    "let dbl xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> (x * 2) :: dbl rest";
const NEG: &str =
    "let neg xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> (0 - x) :: neg rest";
const SET7: &str = "let set7 xs =\n  match xs with\n  | [] -> []\n  | _ :: rest -> 7 :: set7 rest";
const KEEP: &str = "let keep xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> if x > 0 then x :: keep rest else keep rest";
const REV: &str = "let rev acc xs =\n  match xs with\n  | [] -> acc\n  | x :: rest -> rev (x :: acc) rest\n\nlet reverse xs = rev [] xs";

#[test]
fn reuse_inc_recycles_unique_copies_shared() {
    let u = allocs(&prog(INC, "sum (inc xs)", 50), "1325");
    let s = allocs(&prog(INC, "sum (inc xs) + sum xs", 50), "2600");
    assert_eq!(s - u, 50, "unique inc recycles 50 cons cells (u={u}, s={s})");
}

#[test]
fn reuse_double_recycles_unique_copies_shared() {
    let u = allocs(&prog(DBL, "sum (dbl xs)", 50), "2550");
    let s = allocs(&prog(DBL, "sum (dbl xs) + sum xs", 50), "3825");
    assert_eq!(s - u, 50, "unique dbl recycles 50 cons cells (u={u}, s={s})");
}

#[test]
fn reuse_negate_recycles_unique_copies_shared() {
    let u = allocs(&prog(NEG, "sum (neg xs)", 50), "-1275");
    let s = allocs(&prog(NEG, "sum (neg xs) + sum xs", 50), "0");
    assert_eq!(s - u, 50, "unique neg recycles 50 cons cells (u={u}, s={s})");
}

#[test]
fn reuse_sethead_recycles_unique_copies_shared() {
    let u = allocs(&prog(SET7, "sum (set7 xs)", 50), "350");
    let s = allocs(&prog(SET7, "sum (set7 xs) + sum xs", 50), "1625");
    assert_eq!(s - u, 50, "unique set7 recycles 50 cons cells (u={u}, s={s})");
}

#[test]
fn reuse_filter_keep_all_recycles_unique_copies_shared() {
    // `build` yields only positives, so `keep` retains every element, rebuilding
    // (and thus recycling) each cons cell of a unique spine.
    let u = allocs(&prog(KEEP, "sum (keep xs)", 50), "1275");
    let s = allocs(&prog(KEEP, "sum (keep xs) + sum xs", 50), "2550");
    assert_eq!(s - u, 50, "unique filter recycles 50 cons cells (u={u}, s={s})");
}

#[test]
fn reuse_reverse_recycles_unique_copies_shared() {
    let u = allocs(&prog(REV, "sum (reverse xs)", 50), "1275");
    let s = allocs(&prog(REV, "sum (reverse xs) + sum xs", 50), "2550");
    assert_eq!(s - u, 50, "unique reverse recycles 50 cons cells (u={u}, s={s})");
}

// ===========================================================================
// Reuse through standard-library combinators (compiled `std/` code).
// ===========================================================================

#[test]
fn reuse_std_map_recycles_unique_copies_shared() {
    let u = allocs(&prog("", "sum (List.map (fun x -> x + 1) xs)", 50), "1325");
    let s = allocs(&prog("", "sum (List.map (fun x -> x + 1) xs) + sum xs", 50), "2600");
    assert_eq!(s - u, 50, "List.map recycles a unique spine (u={u}, s={s})");
}

#[test]
fn reuse_std_filter_keep_all_recycles_unique_copies_shared() {
    let u = allocs(&prog("", "sum (List.filter (fun x -> x > 0) xs)", 50), "1275");
    let s = allocs(&prog("", "sum (List.filter (fun x -> x > 0) xs) + sum xs", 50), "2550");
    assert_eq!(s - u, 50, "List.filter recycles a unique spine (u={u}, s={s})");
}

// ===========================================================================
// Borrowing: a borrowed inspector preserves uniqueness, so a later rebuild still
// recycles in place; a *consuming* second use shares the list and forces a copy.
// ===========================================================================

#[test]
fn borrowed_inspector_read_twice_runs_clean() {
    // `count` reads its list twice through the borrowing `len`; it must run
    // cleanly and total 2n without leaking (the borrow lends the list to each
    // call rather than duplicating it).
    let src = prog("let count xs = len xs + len xs", "count xs", 50);
    outputs(&src, "100");
}

#[test]
fn borrow_alongside_rebuild_runs_clean() {
    // `inc xs` rebuilds the list while `len xs` reads the same list: the read
    // keeps the spine alive, so this must still total correctly and exit cleanly
    // (reuse cannot fire here, but ownership stays balanced).
    outputs(&prog(INC, "sum (inc xs) + len xs", 50), "1375"); // 1325 + 50
}

#[test]
fn equality_borrows_its_operand_so_reuse_still_fires() {
    // `=` on a boxed list inspects (borrows) its operands rather than consuming
    // them, so comparing `xs` leaves it uniquely owned and the following `inc`
    // still recycles the spine in place. If `=` consumed `xs`, the comparison
    // would duplicate it (sharing the spine) and `inc` would copy — 50 extra
    // allocations. The compared and plain forms therefore allocate identically.
    let plain = allocs(&prog(INC, "sum (inc xs)", 50), "1325");
    let compared = allocs(&prog(INC, "if xs = xs then sum (inc xs) else 0", 50), "1325");
    assert_eq!(plain, compared, "comparing xs borrows it (plain={plain}, compared={compared})");
}

#[test]
fn string_reader_borrows_its_operand() {
    // `String.length` reads (borrows) its operand; reading a string twice must
    // run cleanly and leak-free without duplicating it.
    let src = formatdoc! {r#"
        module M

        let twice s = String.length s + String.length s

        public main : Runtime -> Unit
        let main rt = rt.console.writeLine (Int.toString (twice "abcde"))
    "#};
    outputs(&src, "10");
}

// ===========================================================================
// Record update: a unique record is overwritten in place; a shared one is copied.
// ===========================================================================

#[test]
fn record_update_three_field_in_place_is_constant() {
    // Repeated `{ rec with … }` over a uniquely-owned record overwrites in place,
    // so the allocation count does not grow with the number of updates.
    let prog = |k: i32| {
        formatdoc! {r#"
            module M

            type R = {{ a : Int, b : Int, c : Int }}

            bumpN : Int -> R -> R
            let bumpN k rec =
              if k <= 0 then rec else bumpN (k - 1) {{ rec with c = rec.c + 1 }}

            getC : R -> Int
            let getC rec = rec.c

            public main : Runtime -> Unit
            let main rt = rt.console.writeLine (Int.toString (getC (bumpN {k} {{ a = 0, b = 0, c = 0 }})))
        "#}
    };
    let a = allocs(&prog(50), "50");
    let b = allocs(&prog(100), "100");
    assert_eq!(a, b, "in-place 3-field update is allocation-count independent of k (a={a}, b={b})");
}

#[test]
fn record_update_row_polymorphic_output() {
    // A row-polymorphic `{ rec with n = … }` update threads an offset evidence;
    // it must compute the right field value and exit cleanly.
    let src = formatdoc! {r#"
        module M

        bump : {{ n : Int | 'r }} -> {{ n : Int | 'r }}
        let bump rec = {{ rec with n = rec.n + 1 }}

        public main : Runtime -> Unit
        let main rt =
          let r = bump {{ n = 41, tag = 1 }}
          rt.console.writeLine (Int.toString r.n)
    "#};
    outputs(&src, "42");
}

#[test]
fn record_update_copies_when_shared() {
    let prog = |body: &str| {
        formatdoc! {r#"
            module M

            type R = {{ a : Int, n : Int }}

            bump : R -> R
            let bump rec = {{ rec with n = rec.n + 1 }}

            getN : R -> Int
            let getN rec = rec.n

            use : R -> Int
            let use rec = {body}

            public main : Runtime -> Unit
            let main rt = rt.console.writeLine (Int.toString (use {{ a = 0, n = 10 }}))
        "#}
    };
    let u = allocs(&prog("getN (bump rec)"), "11");
    let s = allocs(&prog("getN (bump rec) + getN rec"), "21");
    assert_eq!(s - u, 1, "a shared record update copies once (u={u}, s={s})");
}

// ===========================================================================
// Correctness under reuse: rebuilds produce the right values and exit cleanly.
// ===========================================================================

#[test]
fn correct_inc_output() {
    outputs(&prog(INC, "sum (inc xs)", 10), "65"); // sum (2..=11)
}

#[test]
fn correct_double_output() {
    outputs(&prog(DBL, "sum (dbl xs)", 10), "110"); // 2 * 55
}

#[test]
fn correct_reverse_output() {
    outputs(&prog(REV, "sum (reverse xs)", 10), "55");
}

#[test]
fn correct_filter_output() {
    let defs = "let evens xs =\n  match xs with\n  | [] -> []\n  | x :: rest -> if (x % 2) = 0 then x :: evens rest else evens rest";
    outputs(&prog(defs, "sum (evens xs)", 10), "30"); // 2+4+6+8+10
}

#[test]
fn correct_append_output() {
    let defs =
        "let append xs ys =\n  match xs with\n  | [] -> ys\n  | x :: rest -> x :: append rest ys";
    outputs(&prog(defs, "sum (append xs xs)", 10), "110");
}

#[test]
fn correct_map_closure_capture_output() {
    // A closure capturing `n` drives the rebuild; the captured value is read on
    // every element and released once.
    let defs = "let addN n xs = List.map (fun x -> x + n) xs";
    outputs(&prog(defs, "sum (addN 100 xs)", 10), "1055"); // 55 + 10*100
}

#[test]
fn correct_adt_tree_rebuild_output() {
    let src = formatdoc! {r#"
        module M

        type Tree = | Leaf Int | Node Tree Tree

        let incT t =
          match t with
          | Leaf n -> Leaf (n + 1)
          | Node l r -> Node (incT l) (incT r)

        let sumT t =
          match t with
          | Leaf n -> n
          | Node l r -> sumT l + sumT r

        public main : Runtime -> Unit
        let main rt =
          let t = Node (Node (Leaf 1) (Leaf 2)) (Leaf 3)
          rt.console.writeLine (Int.toString (sumT (incT t)))
    "#};
    outputs(&src, "9"); // (2+3+4)
}

#[test]
fn correct_option_rebuild_output() {
    let src = formatdoc! {r#"
        module M

        type Opt = | Non | Som Int

        let bump o =
          match o with
          | Non -> Non
          | Som x -> Som (x + 1)

        let get d o =
          match o with
          | Non -> d
          | Som x -> x

        public main : Runtime -> Unit
        let main rt = rt.console.writeLine (Int.toString (get 0 (bump (Som 41))))
    "#};
    outputs(&src, "42");
}

#[test]
fn correct_record_update_output() {
    let src = formatdoc! {r#"
        module M

        type P = {{ x : Int, y : Int }}

        let shift p = {{ p with x = p.x + 1 }}

        public main : Runtime -> Unit
        let main rt =
          let p = shift {{ x = 41, y = 0 }}
          rt.console.writeLine (Int.toString p.x)
    "#};
    outputs(&src, "42");
}

#[test]
fn correct_nested_reuse_chain_output() {
    // inc then dbl over the same unique spine: both rebuilds recycle in place.
    outputs(&prog(&format!("{INC}\n\n{DBL}"), "sum (dbl (inc xs))", 10), "130"); // 2*(55+10)
}

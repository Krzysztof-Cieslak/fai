//! A large suite of concrete, hand-written reference-count soundness cases.
//!
//! Every case is its own `#[test]`, so the test runner reports each program
//! individually. Each program is run through the soundness oracle
//! ([`crate::tests::check_program`], which also rejects ill-typed programs so no
//! case passes vacuously); reuse cases additionally pin the reset/reuse marker.

use indoc::indoc;

use crate::tests::{check_program, rc_checked};

/// Asserts `name` in `src` typechecks and is reference-count sound.
#[track_caller]
fn sound(src: &str, name: &str) {
    if let Err(e) = check_program(src, name) {
        panic!("rc unsound for `{name}`: {e}\n{src}");
    }
}

/// Asserts `name` is sound and emits a reset+reuse opportunity.
#[track_caller]
fn reuses(src: &str, name: &str) {
    sound(src, name);
    let out = rc_checked(src, name);
    assert!(out.contains("data@%"), "expected reuse for `{name}`:\n{out}");
}

/// Asserts `name` is sound and emits no reuse token (inspector or fresh build).
#[track_caller]
fn no_reuse(src: &str, name: &str) {
    sound(src, name);
    let out = rc_checked(src, name);
    assert!(!out.contains("data@%"), "unexpected reuse for `{name}`:\n{out}");
    assert!(!out.contains("reset %"), "unexpected reset for `{name}`:\n{out}");
}

/// Asserts `name` is sound and updates a record in place (`recordUpdate`).
#[track_caller]
fn updates_in_place(src: &str, name: &str) {
    sound(src, name);
    let out = rc_checked(src, name);
    assert!(out.contains("recordUpdate"), "expected in-place update for `{name}`:\n{out}");
}

#[test]
fn arith_add() {
    sound(
        indoc! {r#"
            module M

            let f x y = x + y
        "#},
        "f",
    );
}

#[test]
fn arith_sub() {
    sound(
        indoc! {r#"
            module M

            let f x y = x - y
        "#},
        "f",
    );
}

#[test]
fn arith_mul() {
    sound(
        indoc! {r#"
            module M

            let f x y = x * y
        "#},
        "f",
    );
}

#[test]
fn arith_div() {
    sound(
        indoc! {r#"
            module M

            let f x y = x / y
        "#},
        "f",
    );
}

#[test]
fn arith_mod() {
    sound(
        indoc! {r#"
            module M

            let f x y = x % y
        "#},
        "f",
    );
}

#[test]
fn arith_add1() {
    sound(
        indoc! {r#"
            module M

            let f x = x + 1
        "#},
        "f",
    );
}

#[test]
fn arith_sub1() {
    sound(
        indoc! {r#"
            module M

            let f x = x - 1
        "#},
        "f",
    );
}

#[test]
fn arith_mul2() {
    sound(
        indoc! {r#"
            module M

            let f x = x * 2
        "#},
        "f",
    );
}

#[test]
fn arith_div2() {
    sound(
        indoc! {r#"
            module M

            let f x = x / 2
        "#},
        "f",
    );
}

#[test]
fn arith_mod2() {
    sound(
        indoc! {r#"
            module M

            let f x = x % 2
        "#},
        "f",
    );
}

#[test]
fn arith_one_plus() {
    sound(
        indoc! {r#"
            module M

            let f x = 1 + x
        "#},
        "f",
    );
}

#[test]
fn arith_hundred_minus() {
    sound(
        indoc! {r#"
            module M

            let f x = 100 - x
        "#},
        "f",
    );
}

#[test]
fn arith_x_plus_x() {
    sound(
        indoc! {r#"
            module M

            let f x = x + x
        "#},
        "f",
    );
}

#[test]
fn arith_x_times_x() {
    sound(
        indoc! {r#"
            module M

            let f x = x * x
        "#},
        "f",
    );
}

#[test]
fn arith_xx_minus_x() {
    sound(
        indoc! {r#"
            module M

            let f x = (x + x) - x
        "#},
        "f",
    );
}

#[test]
fn arith_xy_mul_z() {
    sound(
        indoc! {r#"
            module M

            let f x y z = (x + y) * z
        "#},
        "f",
    );
}

#[test]
fn arith_x_add_yz() {
    sound(
        indoc! {r#"
            module M

            let f x y z = x + (y * z)
        "#},
        "f",
    );
}

#[test]
fn arith_xy_div_z() {
    sound(
        indoc! {r#"
            module M

            let f x y z = (x - y) / z
        "#},
        "f",
    );
}

#[test]
fn arith_xy_mod_z() {
    sound(
        indoc! {r#"
            module M

            let f x y z = (x * y) % z
        "#},
        "f",
    );
}

#[test]
fn arith_sum4() {
    sound(
        indoc! {r#"
            module M

            let f x y z = (x + y) + (z + x)
        "#},
        "f",
    );
}

#[test]
fn arith_mix4() {
    sound(
        indoc! {r#"
            module M

            let f x y z = (x * y) - (z * x)
        "#},
        "f",
    );
}

#[test]
fn arith_poly1() {
    sound(
        indoc! {r#"
            module M

            let f x = ((x + 1) * (x - 1)) + x
        "#},
        "f",
    );
}

#[test]
fn arith_poly2() {
    sound(
        indoc! {r#"
            module M

            let f x = (x * x) - (2 * x)
        "#},
        "f",
    );
}

#[test]
fn arith_square() {
    sound(
        indoc! {r#"
            module M

            let square x = x * x
        "#},
        "square",
    );
}

#[test]
fn arith_cube() {
    sound(
        indoc! {r#"
            module M

            let cube x = (x * x) * x
        "#},
        "cube",
    );
}

#[test]
fn arith_neg() {
    sound(
        indoc! {r#"
            module M

            let neg x = 0 - x
        "#},
        "neg",
    );
}

#[test]
fn arith_double() {
    sound(
        indoc! {r#"
            module M

            let double x = x + x
        "#},
        "double",
    );
}

#[test]
fn arith_avg() {
    sound(
        indoc! {r#"
            module M

            let avg a b = (a + b) / 2
        "#},
        "avg",
    );
}

#[test]
fn arith_abs() {
    sound(
        indoc! {r#"
            module M

            let abs n = if n < 0 then 0 - n else n
        "#},
        "abs",
    );
}

#[test]
fn arith_sign() {
    sound(
        indoc! {r#"
            module M

            let sign n = if n < 0 then 0 - 1 else if n > 0 then 1 else 0
        "#},
        "sign",
    );
}

#[test]
fn cmp_eq() {
    sound(
        indoc! {r#"
            module M

            let f x y = x = y
        "#},
        "f",
    );
}

#[test]
fn cmp_ne() {
    sound(
        indoc! {r#"
            module M

            let f x y = x <> y
        "#},
        "f",
    );
}

#[test]
fn cmp_lt() {
    sound(
        indoc! {r#"
            module M

            let f x y = x < y
        "#},
        "f",
    );
}

#[test]
fn cmp_le() {
    sound(
        indoc! {r#"
            module M

            let f x y = x <= y
        "#},
        "f",
    );
}

#[test]
fn cmp_gt() {
    sound(
        indoc! {r#"
            module M

            let f x y = x > y
        "#},
        "f",
    );
}

#[test]
fn cmp_ge() {
    sound(
        indoc! {r#"
            module M

            let f x y = x >= y
        "#},
        "f",
    );
}

#[test]
fn cmp_if_eq() {
    sound(
        indoc! {r#"
            module M

            let f x y = if x = y then 1 else 0
        "#},
        "f",
    );
}

#[test]
fn cmp_if_ne() {
    sound(
        indoc! {r#"
            module M

            let f x y = if x <> y then x else y
        "#},
        "f",
    );
}

#[test]
fn cmp_if_lt() {
    sound(
        indoc! {r#"
            module M

            let f x y = if x < y then y else x
        "#},
        "f",
    );
}

#[test]
fn cmp_if_le() {
    sound(
        indoc! {r#"
            module M

            let f x y = if x <= y then x else y
        "#},
        "f",
    );
}

#[test]
fn cmp_if_gt() {
    sound(
        indoc! {r#"
            module M

            let f x y = if x > y then x else y
        "#},
        "f",
    );
}

#[test]
fn cmp_if_ge() {
    sound(
        indoc! {r#"
            module M

            let f x y = if x >= y then 0 else 1
        "#},
        "f",
    );
}

#[test]
fn cmp_max3() {
    sound(
        indoc! {r#"
            module M

            let max3 a b c = if a > b then (if a > c then a else c) else (if b > c then b else c)
        "#},
        "max3",
    );
}

#[test]
fn cmp_clamp() {
    sound(
        indoc! {r#"
            module M

            let clamp n = if n < 0 then 0 else if n > 10 then 10 else n
        "#},
        "clamp",
    );
}

#[test]
fn bool_and() {
    sound(
        indoc! {r#"
            module M

            let f a b = a && b
        "#},
        "f",
    );
}

#[test]
fn bool_or() {
    sound(
        indoc! {r#"
            module M

            let f a b = a || b
        "#},
        "f",
    );
}

#[test]
fn bool_not() {
    sound(
        indoc! {r#"
            module M

            let f a = not a
        "#},
        "f",
    );
}

#[test]
fn bool_nand() {
    sound(
        indoc! {r#"
            module M

            let f a b = not (a && b)
        "#},
        "f",
    );
}

#[test]
fn bool_andor() {
    sound(
        indoc! {r#"
            module M

            let f a b c = (a && b) || c
        "#},
        "f",
    );
}

#[test]
fn bool_orand() {
    sound(
        indoc! {r#"
            module M

            let f a b c = a && (b || c)
        "#},
        "f",
    );
}

#[test]
fn bool_lt_and() {
    sound(
        indoc! {r#"
            module M

            let f x y = (x < y) && (y < 100)
        "#},
        "f",
    );
}

#[test]
fn bool_eq_or() {
    sound(
        indoc! {r#"
            module M

            let f x y = (x = y) || (x < y)
        "#},
        "f",
    );
}

#[test]
fn bool_not_eq() {
    sound(
        indoc! {r#"
            module M

            let f x y = not (x = y)
        "#},
        "f",
    );
}

#[test]
fn bool_between() {
    sound(
        indoc! {r#"
            module M

            let between lo hi x = (lo <= x) && (x <= hi)
        "#},
        "between",
    );
}

#[test]
fn bool_guard() {
    sound(
        indoc! {r#"
            module M

            let f x = if (x < 0) || (x > 9) then 0 else x
        "#},
        "f",
    );
}

#[test]
fn list_inspect_len() {
    sound(
        indoc! {r#"
            module M

            let len xs =
              match xs with
              | [] -> 0
              | _ :: r -> 1 + len r
        "#},
        "len",
    );
}

#[test]
fn list_inspect_sum() {
    sound(
        indoc! {r#"
            module M

            let sum xs =
              match xs with
              | [] -> 0
              | x :: r -> x + sum r
        "#},
        "sum",
    );
}

#[test]
fn list_inspect_product() {
    sound(
        indoc! {r#"
            module M

            let product xs =
              match xs with
              | [] -> 1
              | x :: r -> x * product r
        "#},
        "product",
    );
}

#[test]
fn list_inspect_isempty() {
    sound(
        indoc! {r#"
            module M

            let isEmpty xs =
              match xs with
              | [] -> true
              | _ :: _ -> false
        "#},
        "isEmpty",
    );
}

#[test]
fn list_inspect_first() {
    sound(
        indoc! {r#"
            module M

            let first xs =
              match xs with
              | [] -> 0
              | x :: _ -> x
        "#},
        "first",
    );
}

#[test]
fn list_inspect_last() {
    sound(
        indoc! {r#"
            module M

            let last xs =
              match xs with
              | [] -> 0
              | x :: r ->
                match r with
                | [] -> x
                | _ :: _ -> last r
        "#},
        "last",
    );
}

#[test]
fn list_inspect_maxl() {
    sound(
        indoc! {r#"
            module M

            let maxl xs =
              match xs with
              | [] -> 0
              | x :: r ->
                let m = maxl r
                if x > m then x else m
        "#},
        "maxl",
    );
}

#[test]
fn list_inspect_minl() {
    sound(
        indoc! {r#"
            module M

            let minl xs =
              match xs with
              | [] -> 0
              | x :: r ->
                let m = minl r
                if x < m then x else m
        "#},
        "minl",
    );
}

#[test]
fn list_inspect_elem() {
    sound(
        indoc! {r#"
            module M

            let elem v xs =
              match xs with
              | [] -> false
              | x :: r -> if x = v then true else elem v r
        "#},
        "elem",
    );
}

#[test]
fn list_inspect_countpos() {
    sound(
        indoc! {r#"
            module M

            let countPos xs =
              match xs with
              | [] -> 0
              | x :: r -> if x > 0 then 1 + countPos r else countPos r
        "#},
        "countPos",
    );
}

#[test]
fn list_build_inc() {
    sound(
        indoc! {r#"
            module M

            let inc xs =
              match xs with
              | [] -> []
              | x :: r -> (x + 1) :: inc r
        "#},
        "inc",
    );
}

#[test]
fn list_build_dec() {
    sound(
        indoc! {r#"
            module M

            let dec xs =
              match xs with
              | [] -> []
              | x :: r -> (x - 1) :: dec r
        "#},
        "dec",
    );
}

#[test]
fn list_build_dbl() {
    sound(
        indoc! {r#"
            module M

            let dbl xs =
              match xs with
              | [] -> []
              | x :: r -> (x * 2) :: dbl r
        "#},
        "dbl",
    );
}

#[test]
fn list_build_squares() {
    sound(
        indoc! {r#"
            module M

            let squares xs =
              match xs with
              | [] -> []
              | x :: r -> (x * x) :: squares r
        "#},
        "squares",
    );
}

#[test]
fn list_build_zero() {
    sound(
        indoc! {r#"
            module M

            let zero xs =
              match xs with
              | [] -> []
              | _ :: r -> 0 :: zero r
        "#},
        "zero",
    );
}

#[test]
fn list_build_idlist() {
    sound(
        indoc! {r#"
            module M

            let idList xs =
              match xs with
              | [] -> []
              | x :: r -> x :: idList r
        "#},
        "idList",
    );
}

#[test]
fn list_build_keeppos() {
    sound(
        indoc! {r#"
            module M

            let keepPos xs =
              match xs with
              | [] -> []
              | x :: r -> if x > 0 then x :: keepPos r else keepPos r
        "#},
        "keepPos",
    );
}

#[test]
fn list_build_dropneg() {
    sound(
        indoc! {r#"
            module M

            let dropNeg xs =
              match xs with
              | [] -> []
              | x :: r -> if x < 0 then dropNeg r else x :: dropNeg r
        "#},
        "dropNeg",
    );
}

#[test]
fn list_build_take() {
    sound(
        indoc! {r#"
            module M

            let take n xs =
              match xs with
              | [] -> []
              | x :: r -> if n <= 0 then [] else x :: take (n - 1) r
        "#},
        "take",
    );
}

#[test]
fn list_build_drop() {
    sound(
        indoc! {r#"
            module M

            let drop n xs =
              match xs with
              | [] -> []
              | x :: r -> if n <= 0 then x :: r else drop (n - 1) r
        "#},
        "drop",
    );
}

#[test]
fn list_build_rev() {
    sound(
        indoc! {r#"
            module M

            let rev acc xs =
              match xs with
              | [] -> acc
              | x :: r -> rev (x :: acc) r
        "#},
        "rev",
    );
}

#[test]
fn list_build_append() {
    sound(
        indoc! {r#"
            module M

            let append xs ys =
              match xs with
              | [] -> ys
              | x :: r -> x :: append r ys
        "#},
        "append",
    );
}

#[test]
fn list_build_replicate() {
    sound(
        indoc! {r#"
            module M

            let replicate n x =
              if n <= 0 then [] else x :: replicate (n - 1) x
        "#},
        "replicate",
    );
}

#[test]
fn list_build_zipsum() {
    sound(
        indoc! {r#"
            module M

            let zipSum xs ys =
              match xs with
              | [] -> []
              | x :: rx ->
                match ys with
                | [] -> []
                | y :: ry -> (x + y) :: zipSum rx ry
        "#},
        "zipSum",
    );
}

#[test]
fn list_build_enumfrom() {
    sound(
        indoc! {r#"
            module M

            let enumFrom n xs =
              match xs with
              | [] -> []
              | _ :: r -> n :: enumFrom (n + 1) r
        "#},
        "enumFrom",
    );
}

#[test]
fn list2d_concat() {
    sound(
        indoc! {r#"
            module M

            let append xs ys =
              match xs with
              | [] -> ys
              | x :: r -> x :: append r ys

            let concat xss =
              match xss with
              | [] -> []
              | xs :: r -> append xs (concat r)
        "#},
        "concat",
    );
}

#[test]
fn list2d_lengths() {
    sound(
        indoc! {r#"
            module M

            let lengths xss =
              match xss with
              | [] -> []
              | xs :: r -> sumLen xs :: lengths r

            let sumLen xs =
              match xs with
              | [] -> 0
              | _ :: r -> 1 + sumLen r
        "#},
        "lengths",
    );
}

#[test]
fn list2d_heads() {
    sound(
        indoc! {r#"
            module M

            let heads xss =
              match xss with
              | [] -> []
              | xs :: r ->
                match xs with
                | [] -> heads r
                | x :: _ -> x :: heads r
        "#},
        "heads",
    );
}

#[test]
fn ho_apply() {
    sound(
        indoc! {r#"
            module M

            let apply f x = f x
        "#},
        "apply",
    );
}

#[test]
fn ho_twice() {
    sound(
        indoc! {r#"
            module M

            let twice f x = f (f x)
        "#},
        "twice",
    );
}

#[test]
fn ho_thrice() {
    sound(
        indoc! {r#"
            module M

            let thrice f x = f (f (f x))
        "#},
        "thrice",
    );
}

#[test]
fn ho_compose() {
    sound(
        indoc! {r#"
            module M

            let compose f g x = f (g x)
        "#},
        "compose",
    );
}

#[test]
fn ho_flip() {
    sound(
        indoc! {r#"
            module M

            let flip f x y = f y x
        "#},
        "flip",
    );
}

#[test]
fn ho_on2() {
    sound(
        indoc! {r#"
            module M

            let on2 f a b = f a b
        "#},
        "on2",
    );
}

#[test]
fn ho_adder() {
    sound(
        indoc! {r#"
            module M

            let adder x = fun y -> x + y
        "#},
        "adder",
    );
}

#[test]
fn ho_const2() {
    sound(
        indoc! {r#"
            module M

            let const2 x = fun y -> x
        "#},
        "const2",
    );
}

#[test]
fn ho_make3() {
    sound(
        indoc! {r#"
            module M

            let make3 x y = fun z -> (x + y) + z
        "#},
        "make3",
    );
}

#[test]
fn ho_map() {
    sound(
        indoc! {r#"
            module M

            let map f xs =
              match xs with
              | [] -> []
              | x :: r -> f x :: map f r
        "#},
        "map",
    );
}

#[test]
fn ho_filter() {
    sound(
        indoc! {r#"
            module M

            let filter p xs =
              match xs with
              | [] -> []
              | x :: r -> if p x then x :: filter p r else filter p r
        "#},
        "filter",
    );
}

#[test]
fn ho_foldl() {
    sound(
        indoc! {r#"
            module M

            let foldl f acc xs =
              match xs with
              | [] -> acc
              | x :: r -> foldl f (f acc x) r
        "#},
        "foldl",
    );
}

#[test]
fn ho_foldr() {
    sound(
        indoc! {r#"
            module M

            let foldr f z xs =
              match xs with
              | [] -> z
              | x :: r -> f x (foldr f z r)
        "#},
        "foldr",
    );
}

#[test]
fn ho_all() {
    sound(
        indoc! {r#"
            module M

            let all p xs =
              match xs with
              | [] -> true
              | x :: r -> if p x then all p r else false
        "#},
        "all",
    );
}

#[test]
fn ho_any() {
    sound(
        indoc! {r#"
            module M

            let any p xs =
              match xs with
              | [] -> false
              | x :: r -> if p x then true else any p r
        "#},
        "any",
    );
}

#[test]
fn ho_count() {
    sound(
        indoc! {r#"
            module M

            let count p xs =
              match xs with
              | [] -> 0
              | x :: r -> if p x then 1 + count p r else count p r
        "#},
        "count",
    );
}

#[test]
fn adt_enum_code() {
    sound(
        indoc! {r#"
            module M

            type Color = | Red | Green | Blue

            let code c =
              match c with
              | Red -> 0
              | Green -> 1
              | Blue -> 2
        "#},
        "code",
    );
}

#[test]
fn adt_enum_next() {
    sound(
        indoc! {r#"
            module M

            type Color = | Red | Green | Blue

            let next c =
              match c with
              | Red -> Green
              | Green -> Blue
              | Blue -> Red
        "#},
        "next",
    );
}

#[test]
fn adt_enum_turn() {
    sound(
        indoc! {r#"
            module M

            type Dir = | N | E | S | W

            let turn d =
              match d with
              | N -> E
              | E -> S
              | S -> W
              | W -> N
        "#},
        "turn",
    );
}

#[test]
fn adt_payload_eval() {
    sound(
        indoc! {r#"
            module M

            type T = | A Int | B Int Int

            let eval t =
              match t with
              | A x -> x
              | B x y -> x + y
        "#},
        "eval",
    );
}

#[test]
fn adt_payload_swap() {
    sound(
        indoc! {r#"
            module M

            type T = | A Int | B Int Int

            let swap t =
              match t with
              | A x -> A x
              | B x y -> B y x
        "#},
        "swap",
    );
}

#[test]
fn adt_payload_bump() {
    sound(
        indoc! {r#"
            module M

            type T = | A Int | B Int Int

            let bump t =
              match t with
              | A x -> A (x + 1)
              | B x y -> B (x + 1) (y + 1)
        "#},
        "bump",
    );
}

#[test]
fn adt_payload_mka() {
    sound(
        indoc! {r#"
            module M

            type T = | A Int | B Int Int

            let mkA n = A n
        "#},
        "mkA",
    );
}

#[test]
fn adt_payload_mkb() {
    sound(
        indoc! {r#"
            module M

            type T = | A Int | B Int Int

            let mkB n = B n n
        "#},
        "mkB",
    );
}

#[test]
fn adt_payload_area() {
    sound(
        indoc! {r#"
            module M

            type Shape =
              | Circle Int
              | Rect Int Int

            let area s =
              match s with
              | Circle r -> (3 * r) * r
              | Rect w h -> w * h
        "#},
        "area",
    );
}

#[test]
fn adt_payload_merge() {
    sound(
        indoc! {r#"
            module M

            type E = | L Int | R Int

            let merge e =
              match e with
              | L x -> x
              | R y -> y
        "#},
        "merge",
    );
}

#[test]
fn adt_opt_getor() {
    sound(
        indoc! {r#"
            module M

            type Opt = | Non | Som Int

            let getOr d o =
              match o with
              | Non -> d
              | Som x -> x
        "#},
        "getOr",
    );
}

#[test]
fn adt_opt_map() {
    sound(
        indoc! {r#"
            module M

            type Opt = | Non | Som Int

            let mapO o =
              match o with
              | Non -> Non
              | Som x -> Som (x + 1)
        "#},
        "mapO",
    );
}

#[test]
fn adt_opt_issome() {
    sound(
        indoc! {r#"
            module M

            type Opt = | Non | Som Int

            let isSome o =
              match o with
              | Non -> false
              | Som _ -> true
        "#},
        "isSome",
    );
}

#[test]
fn adt_opt_orelse() {
    sound(
        indoc! {r#"
            module M

            type Opt = | Non | Som Int

            let orElse a b =
              match a with
              | Non -> b
              | Som _ -> a
        "#},
        "orElse",
    );
}

#[test]
fn tree_sumt() {
    sound(
        indoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            let sumT t =
              match t with
              | Leaf n -> n
              | Node l r -> sumT l + sumT r
        "#},
        "sumT",
    );
}

#[test]
fn tree_deptht() {
    sound(
        indoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            let depthT t =
              match t with
              | Leaf _ -> 1
              | Node l r ->
                let dl = depthT l
                let dr = depthT r
                1 + (if dl > dr then dl else dr)
        "#},
        "depthT",
    );
}

#[test]
fn tree_mirror() {
    sound(
        indoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            let mirror t =
              match t with
              | Leaf n -> Leaf n
              | Node l r -> Node (mirror r) (mirror l)
        "#},
        "mirror",
    );
}

#[test]
fn tree_inct() {
    sound(
        indoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            let incT t =
              match t with
              | Leaf n -> Leaf (n + 1)
              | Node l r -> Node (incT l) (incT r)
        "#},
        "incT",
    );
}

#[test]
fn tree_countleaves() {
    sound(
        indoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            let countLeaves t =
              match t with
              | Leaf _ -> 1
              | Node l r -> countLeaves l + countLeaves r
        "#},
        "countLeaves",
    );
}

#[test]
fn expr_eval() {
    sound(
        indoc! {r#"
            module M

            type Expr =
              | Lit Int
              | Add Expr Expr
              | Mul Expr Expr
              | Neg Expr

            let eval e =
              match e with
              | Lit n -> n
              | Add a b -> eval a + eval b
              | Mul a b -> eval a * eval b
              | Neg a -> 0 - eval a
        "#},
        "eval",
    );
}

#[test]
fn tuple_pair() {
    sound(
        indoc! {r#"
            module M

            let pair a b = (a, b)
        "#},
        "pair",
    );
}

#[test]
fn tuple_triple() {
    sound(
        indoc! {r#"
            module M

            let triple a b c = (a, b, c)
        "#},
        "triple",
    );
}

#[test]
fn tuple_fst() {
    sound(
        indoc! {r#"
            module M

            let fst p =
              match p with
              | (a, _) -> a
        "#},
        "fst",
    );
}

#[test]
fn tuple_snd() {
    sound(
        indoc! {r#"
            module M

            let snd p =
              match p with
              | (_, b) -> b
        "#},
        "snd",
    );
}

#[test]
fn tuple_addt() {
    sound(
        indoc! {r#"
            module M

            let addT p =
              match p with
              | (a, b) -> a + b
        "#},
        "addT",
    );
}

#[test]
fn tuple_swap() {
    sound(
        indoc! {r#"
            module M

            let swap p =
              let (a, b) = p
              (b, a)
        "#},
        "swap",
    );
}

#[test]
fn tuple_dup() {
    sound(
        indoc! {r#"
            module M

            let dup x = (x, x)
        "#},
        "dup",
    );
}

#[test]
fn tuple_onpair() {
    sound(
        indoc! {r#"
            module M

            let onPair f p =
              match p with
              | (a, b) -> f a b
        "#},
        "onPair",
    );
}

#[test]
fn tuple_first3() {
    sound(
        indoc! {r#"
            module M

            let firstOfThree t =
              match t with
              | (a, _, _) -> a
        "#},
        "firstOfThree",
    );
}

#[test]
fn record_mk() {
    sound(
        indoc! {r#"
            module M

            type P = { x : Int, y : Int }

            let mk a = { x = a, y = a + 1 }
        "#},
        "mk",
    );
}

#[test]
fn record_shift() {
    sound(
        indoc! {r#"
            module M

            type P = { x : Int, y : Int }

            let shift p = { p with x = p.x + 1 }
        "#},
        "shift",
    );
}

#[test]
fn record_scale() {
    sound(
        indoc! {r#"
            module M

            type P = { x : Int, y : Int }

            let scale k p = { p with x = p.x * k, y = p.y * k }
        "#},
        "scale",
    );
}

#[test]
fn record_sump() {
    sound(
        indoc! {r#"
            module M

            public type P = { x : Int, y : Int }

            public sumP : P -> Int
            let sumP p = p.x + p.y
        "#},
        "sumP",
    );
}

#[test]
fn record_getx() {
    sound(
        indoc! {r#"
            module M

            public type P = { x : Int, y : Int }

            public getX : P -> Int
            let getX p = p.x
        "#},
        "getX",
    );
}

#[test]
fn record_mag() {
    sound(
        indoc! {r#"
            module M

            public type P = { x : Int, y : Int }

            public mag : P -> Int
            let mag p =
              match p with
              | { x, y } -> x + y
        "#},
        "mag",
    );
}

#[test]
fn record_getx_row() {
    sound(
        indoc! {r#"
            module M

            public getX : { x : Int | 'r } -> Int
            let getX rec = rec.x
        "#},
        "getX",
    );
}

#[test]
fn record_sumxy_row() {
    sound(
        indoc! {r#"
            module M

            public sumXY : { x : Int, y : Int | 'r } -> Int
            let sumXY rec = rec.x + rec.y
        "#},
        "sumXY",
    );
}

#[test]
fn record_bump_row() {
    sound(
        indoc! {r#"
            module M

            public bump : { n : Int | 'r } -> { n : Int | 'r }
            let bump rec = { rec with n = rec.n + 1 }
        "#},
        "bump",
    );
}

#[test]
fn record_setx_row() {
    sound(
        indoc! {r#"
            module M

            public setX : { x : Int | 'r } -> Int -> { x : Int | 'r }
            let setX rec v = { rec with x = v }
        "#},
        "setX",
    );
}

#[test]
fn string_shout() {
    sound(
        indoc! {r#"
            module M

            let shout s = s ++ "!"
        "#},
        "shout",
    );
}

#[test]
fn string_greet() {
    sound(
        indoc! {r#"
            module M

            let greet name = "Hello, " ++ name
        "#},
        "greet",
    );
}

#[test]
fn string_wrap() {
    sound(
        indoc! {r#"
            module M

            let wrap s = "[" ++ s ++ "]"
        "#},
        "wrap",
    );
}

#[test]
fn string_join2() {
    sound(
        indoc! {r#"
            module M

            let join2 a b = a ++ ", " ++ b
        "#},
        "join2",
    );
}

#[test]
fn string_twice() {
    sound(
        indoc! {r#"
            module M

            let twice s = s ++ s
        "#},
        "twice",
    );
}

#[test]
fn string_banner() {
    sound(
        indoc! {r#"
            module M

            let banner s = "== " ++ s ++ " =="
        "#},
        "banner",
    );
}

#[test]
fn string_label() {
    sound(
        indoc! {r#"
            module M

            let label n = "n = " ++ Int.toString n
        "#},
        "label",
    );
}

#[test]
fn cap_exclaim() {
    sound(
        indoc! {r#"
            module M

            interface Greeter =
              greet : String -> String

            let exclaim = { Greeter with greet name = name ++ "!" }
        "#},
        "exclaim",
    );
}

#[test]
fn cap_useg() {
    sound(
        indoc! {r#"
            module M

            interface Greeter =
              greet : String -> String

            let useG g = g.greet "hi"
        "#},
        "useG",
    );
}

#[test]
fn cap_total() {
    sound(
        indoc! {r#"
            module M

            interface Pair =
              fst : Unit -> Int
              snd : Unit -> Int

            let inst = { Pair with fst u = 1, snd u = 2 }

            let total p = p.fst () + p.snd ()
        "#},
        "total",
    );
}

#[test]
fn cap_main() {
    sound(
        indoc! {r#"
            module M

            public main : Runtime -> Unit
            let main r = r.console.writeLine "hi"
        "#},
        "main",
    );
}

#[test]
fn cap_announce() {
    sound(
        indoc! {r#"
            module M

            public announce : { console : Console | 'r } -> String -> Unit
            let announce env msg = env.console.writeLine msg
        "#},
        "announce",
    );
}

#[test]
fn cap_greetall() {
    sound(
        indoc! {r#"
            module M

            public greetAll : { console : Console | 'r } -> Unit
            let greetAll env =
              let _ = env.console.writeLine "a"
              env.console.writeLine "b"
        "#},
        "greetAll",
    );
}

#[test]
fn mixed_let1() {
    sound(
        indoc! {r#"
            module M

            let f a =
              let b = a + 1
              let c = b + a
              b + c
        "#},
        "f",
    );
}

#[test]
fn mixed_let2() {
    sound(
        indoc! {r#"
            module M

            let f x =
              let a = x + 1
              let b = (a + x) * a
              let c = (a + b) - x
              ((a + b) + c) + x
        "#},
        "f",
    );
}

#[test]
fn mixed_firstsome() {
    sound(
        indoc! {r#"
            module M

            type Opt = | Non | Som Int

            let firstSome xs =
              match xs with
              | [] -> Non
              | x :: r -> if x > 0 then Som x else firstSome r
        "#},
        "firstSome",
    );
}

#[test]
fn mixed_tolist() {
    sound(
        indoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            let toList t =
              match t with
              | Leaf n -> n :: []
              | Node l r -> append (toList l) (toList r)

            let append xs ys =
              match xs with
              | [] -> ys
              | x :: r -> x :: append r ys
        "#},
        "toList",
    );
}

#[test]
fn mixed_partitionsum() {
    sound(
        indoc! {r#"
            module M

            let partitionSum xs =
              match xs with
              | [] -> (0, 0)
              | x :: r ->
                let (pos, neg) = partitionSum r
                if x < 0 then (pos, neg + x) else (pos + x, neg)
        "#},
        "partitionSum",
    );
}

#[test]
fn mixed_shiftall() {
    sound(
        indoc! {r#"
            module M

            type P = { x : Int, y : Int }

            let shiftAll ps =
              match ps with
              | [] -> []
              | p :: r -> { p with x = p.x + 1 } :: shiftAll r
        "#},
        "shiftAll",
    );
}

#[test]
fn reuse_pos_inc() {
    reuses(
        indoc! {r#"
            module M

            let inc xs =
              match xs with
              | [] -> []
              | x :: r -> (x + 1) :: inc r
        "#},
        "inc",
    );
}

#[test]
fn reuse_pos_dec() {
    reuses(
        indoc! {r#"
            module M

            let dec xs =
              match xs with
              | [] -> []
              | x :: r -> (x - 1) :: dec r
        "#},
        "dec",
    );
}

#[test]
fn reuse_pos_squares() {
    reuses(
        indoc! {r#"
            module M

            let squares xs =
              match xs with
              | [] -> []
              | x :: r -> (x * x) :: squares r
        "#},
        "squares",
    );
}

#[test]
fn reuse_pos_zero() {
    reuses(
        indoc! {r#"
            module M

            let zero xs =
              match xs with
              | [] -> []
              | _ :: r -> 0 :: zero r
        "#},
        "zero",
    );
}

#[test]
fn reuse_pos_swap() {
    reuses(
        indoc! {r#"
            module M

            type T = | A Int | B Int Int

            let swap t =
              match t with
              | A x -> A x
              | B x y -> B y x
        "#},
        "swap",
    );
}

#[test]
fn reuse_pos_mapo() {
    reuses(
        indoc! {r#"
            module M

            type Opt = | Non | Som Int

            let mapO o =
              match o with
              | Non -> Non
              | Som x -> Som (x + 1)
        "#},
        "mapO",
    );
}

#[test]
fn reuse_pos_inct() {
    reuses(
        indoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            let incT t =
              match t with
              | Leaf n -> Leaf (n + 1)
              | Node l r -> Node (incT l) (incT r)
        "#},
        "incT",
    );
}

#[test]
fn reuse_neg_len() {
    no_reuse(
        indoc! {r#"
            module M

            let len xs =
              match xs with
              | [] -> 0
              | _ :: r -> 1 + len r
        "#},
        "len",
    );
}

#[test]
fn reuse_neg_sum() {
    no_reuse(
        indoc! {r#"
            module M

            let sum xs =
              match xs with
              | [] -> 0
              | x :: r -> x + sum r
        "#},
        "sum",
    );
}

#[test]
fn reuse_neg_product() {
    no_reuse(
        indoc! {r#"
            module M

            let product xs =
              match xs with
              | [] -> 1
              | x :: r -> x * product r
        "#},
        "product",
    );
}

#[test]
fn reuse_neg_isempty() {
    no_reuse(
        indoc! {r#"
            module M

            let isEmpty xs =
              match xs with
              | [] -> true
              | _ :: _ -> false
        "#},
        "isEmpty",
    );
}

#[test]
fn reuse_neg_singleton() {
    no_reuse(
        indoc! {r#"
            module M

            let singleton x = x :: []
        "#},
        "singleton",
    );
}

#[test]
fn reuse_neg_pairup() {
    no_reuse(
        indoc! {r#"
            module M

            let pairUp n = n :: n :: []
        "#},
        "pairUp",
    );
}

#[test]
fn reuse_neg_mkb() {
    no_reuse(
        indoc! {r#"
            module M

            type T = | A Int | B Int Int

            let mkB n = B n n
        "#},
        "mkB",
    );
}

#[test]
fn reuse_neg_id() {
    no_reuse(
        indoc! {r#"
            module M

            let id x = x
        "#},
        "id",
    );
}

#[test]
fn reuse_neg_addone() {
    no_reuse(
        indoc! {r#"
            module M

            let addOne x = x + 1
        "#},
        "addOne",
    );
}

#[test]
fn reuse_neg_sumt() {
    no_reuse(
        indoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            let sumT t =
              match t with
              | Leaf n -> n
              | Node l r -> sumT l + sumT r
        "#},
        "sumT",
    );
}

#[test]
fn list_more_takewhile() {
    sound(
        indoc! {r#"
            module M

            let takeWhile p xs =
              match xs with
              | [] -> []
              | x :: r -> if p x then x :: takeWhile p r else []
        "#},
        "takeWhile",
    );
}

#[test]
fn list_more_dropwhile() {
    sound(
        indoc! {r#"
            module M

            let dropWhile p xs =
              match xs with
              | [] -> []
              | x :: r -> if p x then dropWhile p r else x :: r
        "#},
        "dropWhile",
    );
}

#[test]
fn list_more_addall() {
    sound(
        indoc! {r#"
            module M

            let addAll n xs =
              match xs with
              | [] -> []
              | x :: r -> (x + n) :: addAll n r
        "#},
        "addAll",
    );
}

#[test]
fn list_more_scaleall() {
    sound(
        indoc! {r#"
            module M

            let scaleAll k xs =
              match xs with
              | [] -> []
              | x :: r -> (x * k) :: scaleAll k r
        "#},
        "scaleAll",
    );
}

#[test]
fn list_more_sumsquares() {
    sound(
        indoc! {r#"
            module M

            let sumSquares xs =
              match xs with
              | [] -> 0
              | x :: r -> (x * x) + sumSquares r
        "#},
        "sumSquares",
    );
}

#[test]
fn list_more_dot() {
    sound(
        indoc! {r#"
            module M

            let dot xs ys =
              match xs with
              | [] -> 0
              | x :: rx ->
                match ys with
                | [] -> 0
                | y :: ry -> (x * y) + dot rx ry
        "#},
        "dot",
    );
}

#[test]
fn list_more_range() {
    sound(
        indoc! {r#"
            module M

            let range lo hi =
              if lo > hi then [] else lo :: range (lo + 1) hi
        "#},
        "range",
    );
}

#[test]
fn list_more_prependall() {
    sound(
        indoc! {r#"
            module M

            let prependAll sep xs =
              match xs with
              | [] -> []
              | x :: r -> sep :: x :: prependAll sep r
        "#},
        "prependAll",
    );
}

#[test]
fn list_more_countdown() {
    sound(
        indoc! {r#"
            module M

            let countDown n =
              if n <= 0 then [] else n :: countDown (n - 1)
        "#},
        "countDown",
    );
}

#[test]
fn list_more_nth() {
    sound(
        indoc! {r#"
            module M

            let nth n xs =
              match xs with
              | [] -> 0
              | x :: r -> if n <= 0 then x else nth (n - 1) r
        "#},
        "nth",
    );
}

#[test]
fn list_more_splitfirst() {
    sound(
        indoc! {r#"
            module M

            let splitFirst xs =
              match xs with
              | [] -> (0, [])
              | x :: r -> (x, r)
        "#},
        "splitFirst",
    );
}

#[test]
fn list_more_pairs() {
    sound(
        indoc! {r#"
            module M

            let pairs xs =
              match xs with
              | [] -> []
              | x :: r ->
                match r with
                | [] -> []
                | y :: rr -> (x, y) :: pairs rr
        "#},
        "pairs",
    );
}

#[test]
fn list_more_interleave() {
    sound(
        indoc! {r#"
            module M

            let interleave xs ys =
              match xs with
              | [] -> ys
              | x :: r -> x :: interleave ys r
        "#},
        "interleave",
    );
}

#[test]
fn list_more_sumwith() {
    sound(
        indoc! {r#"
            module M

            let sumWith f xs =
              match xs with
              | [] -> 0
              | x :: r -> f x + sumWith f r
        "#},
        "sumWith",
    );
}

#[test]
fn ho_more_curry() {
    sound(
        indoc! {r#"
            module M

            let curry f a b =
              f (a, b)
        "#},
        "curry",
    );
}

#[test]
fn ho_more_uncurry() {
    sound(
        indoc! {r#"
            module M

            let uncurry f p =
              match p with
              | (a, b) -> f a b
        "#},
        "uncurry",
    );
}

#[test]
fn ho_more_applyn() {
    sound(
        indoc! {r#"
            module M

            let applyN n f x =
              if n <= 0 then x else applyN (n - 1) f (f x)
        "#},
        "applyN",
    );
}

#[test]
fn ho_more_iterate() {
    sound(
        indoc! {r#"
            module M

            let iterate n f x =
              if n <= 0 then [] else x :: iterate (n - 1) f (f x)
        "#},
        "iterate",
    );
}

#[test]
fn ho_more_pipe() {
    sound(
        indoc! {r#"
            module M

            let pipe x f g =
              g (f x)
        "#},
        "pipe",
    );
}

#[test]
fn ho_more_both() {
    sound(
        indoc! {r#"
            module M

            let both f g x =
              (f x, g x)
        "#},
        "both",
    );
}

#[test]
fn ho_more_usetwice() {
    sound(
        indoc! {r#"
            module M

            let useTwice f x =
              f x + f x
        "#},
        "useTwice",
    );
}

#[test]
fn ho_more_composepipe() {
    sound(
        indoc! {r#"
            module M

            let composePipe f g = f >> g
        "#},
        "composePipe",
    );
}

#[test]
fn ho_more_pipeinto() {
    sound(
        indoc! {r#"
            module M

            let pipeInto n = n |> Int.toString
        "#},
        "pipeInto",
    );
}

#[test]
fn peano_toint() {
    sound(
        indoc! {r#"
            module M

            type Nat = | Z | S Nat

            let toInt n =
              match n with
              | Z -> 0
              | S m -> 1 + toInt m
        "#},
        "toInt",
    );
}

#[test]
fn peano_plus() {
    sound(
        indoc! {r#"
            module M

            type Nat = | Z | S Nat

            let plus a b =
              match a with
              | Z -> b
              | S m -> S (plus m b)
        "#},
        "plus",
    );
}

#[test]
fn peano_iszero() {
    sound(
        indoc! {r#"
            module M

            type Nat = | Z | S Nat

            let isZero n =
              match n with
              | Z -> true
              | S _ -> false
        "#},
        "isZero",
    );
}

#[test]
fn peano_pred() {
    sound(
        indoc! {r#"
            module M

            type Nat = | Z | S Nat

            let pred n =
              match n with
              | Z -> Z
              | S m -> m
        "#},
        "pred",
    );
}

#[test]
fn peano_double() {
    sound(
        indoc! {r#"
            module M

            type Nat = | Z | S Nat

            let double n =
              match n with
              | Z -> Z
              | S m -> S (S (double m))
        "#},
        "double",
    );
}

#[test]
fn record_more_mk3() {
    sound(
        indoc! {r#"
            module M

            type V3 = { x : Int, y : Int, z : Int }

            let mk3 a = { x = a, y = a + 1, z = a + 2 }
        "#},
        "mk3",
    );
}

#[test]
fn record_more_shiftz() {
    sound(
        indoc! {r#"
            module M

            type V3 = { x : Int, y : Int, z : Int }

            let shiftZ v = { v with z = v.z + 1 }
        "#},
        "shiftZ",
    );
}

#[test]
fn record_more_bumpall() {
    sound(
        indoc! {r#"
            module M

            type V3 = { x : Int, y : Int, z : Int }

            let bumpAll v = { v with x = v.x + 1, y = v.y + 1, z = v.z + 1 }
        "#},
        "bumpAll",
    );
}

#[test]
fn record_more_total() {
    sound(
        indoc! {r#"
            module M

            public type V3 = { x : Int, y : Int, z : Int }

            public total : V3 -> Int
            let total v = (v.x + v.y) + v.z
        "#},
        "total",
    );
}

#[test]
fn record_more_startx() {
    sound(
        indoc! {r#"
            module M

            type Point = { x : Int, y : Int }
            type Seg = { a : Point, b : Point }

            let startX s = s.a.x
        "#},
        "startX",
    );
}

#[test]
fn record_more_dx() {
    sound(
        indoc! {r#"
            module M

            type Point = { x : Int, y : Int }
            type Seg = { a : Point, b : Point }

            let dx s = s.b.x - s.a.x
        "#},
        "dx",
    );
}

#[test]
fn record_more_tick() {
    sound(
        indoc! {r#"
            module M

            type Counter = { n : Int, step : Int }

            let tick c = { c with n = c.n + c.step }
        "#},
        "tick",
    );
}

#[test]
fn record_more_swapxy() {
    sound(
        indoc! {r#"
            module M

            public swapXY : { x : Int, y : Int | 'r } -> { x : Int, y : Int | 'r }
            let swapXY rec = { rec with x = rec.y, y = rec.x }
        "#},
        "swapXY",
    );
}

#[test]
fn adt_more_json() {
    sound(
        indoc! {r#"
            module M

            type Json =
              | JNull
              | JBool Bool
              | JNum Int
              | JArr (List Json)

            let size j =
              match j with
              | JNull -> 1
              | JBool _ -> 1
              | JNum _ -> 1
              | JArr xs -> 1 + sizeArr xs

            let sizeArr xs =
              match xs with
              | [] -> 0
              | x :: r -> size x + sizeArr r
        "#},
        "size",
    );
}

#[test]
fn adt_more_rose() {
    sound(
        indoc! {r#"
            module M

            type Rose = | Rose Int (List Rose)

            let sumR t =
              match t with
              | Rose n kids -> n + sumForest kids

            let sumForest ts =
              match ts with
              | [] -> 0
              | t :: r -> sumR t + sumForest r
        "#},
        "sumR",
    );
}

#[test]
fn adt_more_cmd() {
    sound(
        indoc! {r#"
            module M

            type Cmd =
              | Up Int
              | Down Int
              | Reset

            let apply pos c =
              match c with
              | Up n -> pos + n
              | Down n -> pos - n
              | Reset -> 0
        "#},
        "apply",
    );
}

#[test]
fn adt_more_token() {
    sound(
        indoc! {r#"
            module M

            type Token =
              | Num Int
              | Plus
              | Minus
              | Times

            let prec t =
              match t with
              | Num _ -> 0
              | Plus -> 1
              | Minus -> 1
              | Times -> 2
        "#},
        "prec",
    );
}

#[test]
fn adt_more_result3() {
    sound(
        indoc! {r#"
            module M

            type Result3 =
              | Ok3 Int
              | Warn Int Int
              | Err

            let value r =
              match r with
              | Ok3 x -> x
              | Warn x _ -> x
              | Err -> 0
        "#},
        "value",
    );
}

#[test]
fn pat_ignore2() {
    sound(
        indoc! {r#"
            module M

            let ignore2 x y = 0
        "#},
        "ignore2",
    );
}

#[test]
fn pat_ignore3() {
    sound(
        indoc! {r#"
            module M

            let ignore3 a b c = 0
        "#},
        "ignore3",
    );
}

#[test]
fn pat_heador() {
    sound(
        indoc! {r#"
            module M

            let headOr d xs =
              match xs with
              | [] -> d
              | x :: _ -> x
        "#},
        "headOr",
    );
}

#[test]
fn pat_secondor() {
    sound(
        indoc! {r#"
            module M

            let secondOr d xs =
              match xs with
              | [] -> d
              | _ :: rest ->
                match rest with
                | [] -> d
                | y :: _ -> y
        "#},
        "secondOr",
    );
}

#[test]
fn pat_firstfield() {
    sound(
        indoc! {r#"
            module M

            type T = | A Int | B Int Int

            let firstField t =
              match t with
              | A x -> x
              | B x _ -> x
        "#},
        "firstField",
    );
}

#[test]
fn pat_classify() {
    sound(
        indoc! {r#"
            module M

            let classify xs =
              match xs with
              | [] -> 0
              | _ :: [] -> 1
              | _ :: _ :: _ -> 2
        "#},
        "classify",
    );
}

#[test]
fn arith_more_poly() {
    sound(
        indoc! {r#"
            module M

            let poly x =
              let x2 = x * x
              let x3 = x2 * x
              ((x3 + (2 * x2)) + (3 * x)) + 4
        "#},
        "poly",
    );
}

#[test]
fn arith_more_hypotsq() {
    sound(
        indoc! {r#"
            module M

            let hypotSq a b =
              (a * a) + (b * b)
        "#},
        "hypotSq",
    );
}

#[test]
fn arith_more_lerp() {
    sound(
        indoc! {r#"
            module M

            let lerp a b t =
              a + (((b - a) * t) / 100)
        "#},
        "lerp",
    );
}

#[test]
fn arith_more_f() {
    sound(
        indoc! {r#"
            module M

            let f x y =
              let s = x + y
              let d = x - y
              let p = x * y
              (s + d) + p
        "#},
        "f",
    );
}

#[test]
fn arith_more_g() {
    sound(
        indoc! {r#"
            module M

            let g a b c d =
              let ab = a * b
              let cd = c * d
              ab + cd
        "#},
        "g",
    );
}

#[test]
fn arith_more_mixed() {
    sound(
        indoc! {r#"
            module M

            let mixed x =
              let a = x + 1
              let b = a * 2
              let c = b - x
              let d = c % 7
              ((a + b) + c) + d
        "#},
        "mixed",
    );
}

#[test]
fn arith_more_reuseheavy() {
    sound(
        indoc! {r#"
            module M

            let reuseHeavy x =
              let a = x + 1
              (((a + a) + a) + a) + a
        "#},
        "reuseHeavy",
    );
}

#[test]
fn std_list_length() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.length xs
        "#},
        "f",
    );
}

#[test]
fn std_list_isempty() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.isEmpty xs
        "#},
        "f",
    );
}

#[test]
fn std_list_sum() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.sum xs
        "#},
        "f",
    );
}

#[test]
fn std_list_reverse() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.reverse xs
        "#},
        "f",
    );
}

#[test]
fn std_list_append() {
    sound(
        indoc! {r#"
            module M

            let f xs ys = List.append xs ys
        "#},
        "f",
    );
}

#[test]
fn std_list_map() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.map (fun x -> x + 1) xs
        "#},
        "f",
    );
}

#[test]
fn std_list_filter() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.filter (fun x -> x > 0) xs
        "#},
        "f",
    );
}

#[test]
fn std_list_foldl() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.foldl (fun a x -> a + x) 0 xs
        "#},
        "f",
    );
}

#[test]
fn std_list_foldr() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.foldr (fun x a -> x + a) 0 xs
        "#},
        "f",
    );
}

#[test]
fn std_list_all() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.all (fun x -> x > 0) xs
        "#},
        "f",
    );
}

#[test]
fn std_list_any() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.any (fun x -> x > 0) xs
        "#},
        "f",
    );
}

#[test]
fn std_list_member() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.member 3 xs
        "#},
        "f",
    );
}

#[test]
fn std_list_take() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.take 2 xs
        "#},
        "f",
    );
}

#[test]
fn std_list_drop() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.drop 2 xs
        "#},
        "f",
    );
}

#[test]
fn std_list_range() {
    sound(
        indoc! {r#"
            module M

            let f = List.range 1 10
        "#},
        "f",
    );
}

#[test]
fn std_list_zip() {
    sound(
        indoc! {r#"
            module M

            let f xs ys = List.zip xs ys
        "#},
        "f",
    );
}

#[test]
fn std_list_find() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.find (fun x -> x > 0) xs
        "#},
        "f",
    );
}

#[test]
fn std_list_takewhile() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.takeWhile (fun x -> x < 10) xs
        "#},
        "f",
    );
}

#[test]
fn std_list_dropwhile() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.dropWhile (fun x -> x < 10) xs
        "#},
        "f",
    );
}

#[test]
fn std_list_partition() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.partition (fun x -> x > 0) xs
        "#},
        "f",
    );
}

#[test]
fn std_list_sort() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.sort xs
        "#},
        "f",
    );
}

#[test]
fn std_list_concat() {
    sound(
        indoc! {r#"
            module M

            let f xss = List.concat xss
        "#},
        "f",
    );
}

#[test]
fn std_list_concatmap() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.concatMap (fun x -> x :: x :: []) xs
        "#},
        "f",
    );
}

#[test]
fn std_list_head() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.head xs
        "#},
        "f",
    );
}

#[test]
fn std_list_tail() {
    sound(
        indoc! {r#"
            module M

            let f xs = List.tail xs
        "#},
        "f",
    );
}

#[test]
fn std_list_pipeline() {
    sound(
        indoc! {r#"
            module M

            let pipeline xs =
              xs
              |> List.filter (fun x -> x > 0)
              |> List.map (fun x -> x * 2)
              |> List.sum
        "#},
        "pipeline",
    );
}

#[test]
fn std_optres_map() {
    sound(
        indoc! {r#"
            module M

            let f o = Option.map (fun x -> x + 1) o
        "#},
        "f",
    );
}

#[test]
fn std_optres_withdefault() {
    sound(
        indoc! {r#"
            module M

            let f o = Option.withDefault 0 o
        "#},
        "f",
    );
}

#[test]
fn std_optres_issome() {
    sound(
        indoc! {r#"
            module M

            let f o = Option.isSome o
        "#},
        "f",
    );
}

#[test]
fn std_optres_isnone() {
    sound(
        indoc! {r#"
            module M

            let f o = Option.isNone o
        "#},
        "f",
    );
}

#[test]
fn std_optres_andthen() {
    sound(
        indoc! {r#"
            module M

            let f o = Option.andThen (fun x -> Some (x + 1)) o
        "#},
        "f",
    );
}

#[test]
fn std_optres_some1() {
    sound(
        indoc! {r#"
            module M

            let some1 = Some 1
        "#},
        "some1",
    );
}

#[test]
fn std_optres_none1() {
    sound(
        indoc! {r#"
            module M

            let none1 = None
        "#},
        "none1",
    );
}

#[test]
fn std_optres_wrap() {
    sound(
        indoc! {r#"
            module M

            let wrap x = Some x
        "#},
        "wrap",
    );
}

#[test]
fn std_optres_safediv() {
    sound(
        indoc! {r#"
            module M

            let safeDiv a b =
              if b = 0 then None else Some (a / b)
        "#},
        "safeDiv",
    );
}

#[test]
fn std_optres_resmap() {
    sound(
        indoc! {r#"
            module M

            let f r = Result.map (fun x -> x + 1) r
        "#},
        "f",
    );
}

#[test]
fn std_optres_isok() {
    sound(
        indoc! {r#"
            module M

            let f r = Result.isOk r
        "#},
        "f",
    );
}

#[test]
fn std_optres_iserr() {
    sound(
        indoc! {r#"
            module M

            let f r = Result.isErr r
        "#},
        "f",
    );
}

#[test]
fn std_optres_okn() {
    sound(
        indoc! {r#"
            module M

            let okN n = Ok n
        "#},
        "okN",
    );
}

#[test]
fn std_optres_errs() {
    sound(
        indoc! {r#"
            module M

            let errS s = Err s
        "#},
        "errS",
    );
}

#[test]
fn std_optres_checkpos() {
    sound(
        indoc! {r#"
            module M

            let checkPos n =
              if n > 0 then Ok n else Err "not positive"
        "#},
        "checkPos",
    );
}

#[test]
fn std_sif_inttostring() {
    sound(
        indoc! {r#"
            module M

            let f n = Int.toString n
        "#},
        "f",
    );
}

#[test]
fn std_sif_inttofloat() {
    sound(
        indoc! {r#"
            module M

            let f n = Int.toFloat n
        "#},
        "f",
    );
}

#[test]
fn std_sif_strlen() {
    sound(
        indoc! {r#"
            module M

            let f s = String.length s
        "#},
        "f",
    );
}

#[test]
fn std_sif_toupper() {
    sound(
        indoc! {r#"
            module M

            let f s = String.toUpper s
        "#},
        "f",
    );
}

#[test]
fn std_sif_tolower() {
    sound(
        indoc! {r#"
            module M

            let f s = String.toLower s
        "#},
        "f",
    );
}

#[test]
fn std_sif_trim() {
    sound(
        indoc! {r#"
            module M

            let f s = String.trim s
        "#},
        "f",
    );
}

#[test]
fn std_sif_contains() {
    sound(
        indoc! {r#"
            module M

            let f a b = String.contains a b
        "#},
        "f",
    );
}

#[test]
fn std_sif_join() {
    sound(
        indoc! {r#"
            module M

            let f sep parts = String.join sep parts
        "#},
        "f",
    );
}

#[test]
fn std_sif_split() {
    sound(
        indoc! {r#"
            module M

            let f sep s = String.split sep s
        "#},
        "f",
    );
}

#[test]
fn std_sif_floattostring() {
    sound(
        indoc! {r#"
            module M

            let f x = Float.toString x
        "#},
        "f",
    );
}

#[test]
fn std_sif_floattoint() {
    sound(
        indoc! {r#"
            module M

            let f x = Float.toInt x
        "#},
        "f",
    );
}

#[test]
fn std_sif_sqrt() {
    sound(
        indoc! {r#"
            module M

            let f x = Float.sqrt x
        "#},
        "f",
    );
}

#[test]
fn std_sif_area() {
    sound(
        indoc! {r#"
            module M

            let area r = (Float.pi * r) * r
        "#},
        "area",
    );
}

// --- Chars: immediates, so dup/drop are no-ops; the oracle confirms balance ---

#[test]
fn char_returned_is_sound() {
    sound(
        indoc! {r#"
            module M

            let f x = 'a'
        "#},
        "f",
    );
}

#[test]
fn char_identity_is_sound() {
    sound(
        indoc! {r#"
            module M

            public f : Char -> Char
            let f c = c
        "#},
        "f",
    );
}

#[test]
fn char_match_is_sound() {
    sound(
        indoc! {r#"
            module M

            let classify c =
              match c with
              | 'a' -> 1
              | 'b' -> 2
              | _ -> 0
        "#},
        "classify",
    );
}

#[test]
fn char_list_is_sound() {
    // The list cells are boxed (and reference-counted); the Char elements are
    // immediates that are never duplicated or dropped.
    no_reuse(
        indoc! {r#"
            module M

            let xs = ['a', 'b', 'c']
        "#},
        "xs",
    );
}

#[test]
fn char_tuple_is_sound() {
    sound(
        indoc! {r#"
            module M

            let p = ('a', 'b')
        "#},
        "p",
    );
}

#[test]
fn char_to_string_is_sound() {
    // Consumes an immediate Char and produces a reference-counted String.
    sound(
        indoc! {r#"
            module M

            let f c = Char.toString c
        "#},
        "f",
    );
}

#[test]
fn char_from_code_is_sound() {
    sound(
        indoc! {r#"
            module M

            let f n = Char.fromCode n
        "#},
        "f",
    );
}

#[test]
fn std_sif_describe() {
    sound(
        indoc! {r#"
            module M

            let describe n =
              "value: " ++ Int.toString n
        "#},
        "describe",
    );
}

#[test]
fn std_sif_shoutupper() {
    sound(
        indoc! {r#"
            module M

            let shoutUpper s =
              String.toUpper s ++ "!"
        "#},
        "shoutUpper",
    );
}

#[test]
fn std_dictset_insert() {
    sound(
        indoc! {r#"
            module M

            let f d k v = Dict.insert k v d
        "#},
        "f",
    );
}

#[test]
fn std_dictset_get() {
    sound(
        indoc! {r#"
            module M

            let f d k = Dict.get k d
        "#},
        "f",
    );
}

#[test]
fn std_dictset_member() {
    sound(
        indoc! {r#"
            module M

            let f d k = Dict.member k d
        "#},
        "f",
    );
}

#[test]
fn std_dictset_size() {
    sound(
        indoc! {r#"
            module M

            let f d = Dict.size d
        "#},
        "f",
    );
}

#[test]
fn std_dictset_tolist() {
    sound(
        indoc! {r#"
            module M

            let f d = Dict.toList d
        "#},
        "f",
    );
}

#[test]
fn std_dictset_setinsert() {
    sound(
        indoc! {r#"
            module M

            let f s x = Set.insert x s
        "#},
        "f",
    );
}

#[test]
fn std_dictset_setmember() {
    sound(
        indoc! {r#"
            module M

            let f s x = Set.member x s
        "#},
        "f",
    );
}

#[test]
fn std_dictset_setsize() {
    sound(
        indoc! {r#"
            module M

            let f s = Set.size s
        "#},
        "f",
    );
}

#[test]
fn std_dictset_settolist() {
    sound(
        indoc! {r#"
            module M

            let f s = Set.toList s
        "#},
        "f",
    );
}

#[test]
fn std_dictset_addpair() {
    sound(
        indoc! {r#"
            module M

            let addPair d =
              d
              |> Dict.insert 1 10
              |> Dict.insert 2 20
        "#},
        "addPair",
    );
}

#[test]
fn mutual_iseven() {
    sound(
        indoc! {r#"
            module M

            let isEven n =
              if n = 0 then true else isOdd (n - 1)

            let isOdd n =
              if n = 0 then false else isEven (n - 1)
        "#},
        "isEven",
    );
}

#[test]
fn mutual_isodd() {
    sound(
        indoc! {r#"
            module M

            let isOdd n =
              if n = 0 then false else isEven (n - 1)

            let isEven n =
              if n = 0 then true else isOdd (n - 1)
        "#},
        "isOdd",
    );
}

#[test]
fn mutual_ping() {
    sound(
        indoc! {r#"
            module M

            let ping n acc =
              if n <= 0 then acc else pong (n - 1) (acc + 1)

            let pong n acc =
              if n <= 0 then acc else ping (n - 1) (acc + 2)
        "#},
        "ping",
    );
}

#[test]
fn mutual_evens() {
    sound(
        indoc! {r#"
            module M

            let evens xs =
              match xs with
              | [] -> []
              | x :: r -> x :: odds r

            let odds xs =
              match xs with
              | [] -> []
              | _ :: r -> evens r
        "#},
        "evens",
    );
}

#[test]
fn acc_sum() {
    sound(
        indoc! {r#"
            module M

            let sumAcc acc xs =
              match xs with
              | [] -> acc
              | x :: r -> sumAcc (acc + x) r
        "#},
        "sumAcc",
    );
}

#[test]
fn acc_len() {
    sound(
        indoc! {r#"
            module M

            let lenAcc acc xs =
              match xs with
              | [] -> acc
              | _ :: r -> lenAcc (acc + 1) r
        "#},
        "lenAcc",
    );
}

#[test]
fn acc_rev() {
    sound(
        indoc! {r#"
            module M

            let revAcc acc xs =
              match xs with
              | [] -> acc
              | x :: r -> revAcc (x :: acc) r
        "#},
        "revAcc",
    );
}

#[test]
fn acc_max() {
    sound(
        indoc! {r#"
            module M

            let maxAcc best xs =
              match xs with
              | [] -> best
              | x :: r -> maxAcc (if x > best then x else best) r
        "#},
        "maxAcc",
    );
}

#[test]
fn acc_fact() {
    sound(
        indoc! {r#"
            module M

            let factAcc acc n =
              if n <= 1 then acc else factAcc (acc * n) (n - 1)
        "#},
        "factAcc",
    );
}

#[test]
fn acc_count() {
    sound(
        indoc! {r#"
            module M

            let countAcc acc p xs =
              match xs with
              | [] -> acc
              | x :: r -> countAcc (if p x then acc + 1 else acc) p r
        "#},
        "countAcc",
    );
}

#[test]
fn ho_self_compose() {
    sound(
        indoc! {r#"
            module M

            let twice f = f >> f
        "#},
        "twice",
    );
}

#[test]
fn ho_nested() {
    sound(
        indoc! {r#"
            module M

            let nested f g x = f (g (g x))
        "#},
        "nested",
    );
}

#[test]
fn upd_shift() {
    updates_in_place(
        indoc! {r#"
            module M

            type P = { x : Int, y : Int }

            let shift p = { p with x = p.x + 1 }
        "#},
        "shift",
    );
}

#[test]
fn upd_scale() {
    updates_in_place(
        indoc! {r#"
            module M

            type P = { x : Int, y : Int }

            let scale k p = { p with x = p.x * k, y = p.y * k }
        "#},
        "scale",
    );
}

#[test]
fn upd_bump_row() {
    updates_in_place(
        indoc! {r#"
            module M

            public bump : { n : Int | 'r } -> { n : Int | 'r }
            let bump rec = { rec with n = rec.n + 1 }
        "#},
        "bump",
    );
}

#[test]
fn borrow_len() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let len xs =
                  match xs with
                  | [] -> 0
                  | _ :: r -> 1 + len r
            "#},
            "len",
        ),
        vec![true],
    );
}

#[test]
fn borrow_sum() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let sum xs =
                  match xs with
                  | [] -> 0
                  | x :: r -> x + sum r
            "#},
            "sum",
        ),
        vec![true],
    );
}

#[test]
fn borrow_isempty() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let isEmpty xs =
                  match xs with
                  | [] -> true
                  | _ :: _ -> false
            "#},
            "isEmpty",
        ),
        vec![true],
    );
}

#[test]
fn borrow_sum_acc_owns_the_list() {
    // The accumulator fold is *tail*-recursive, so the list parameter flows into a
    // tail self-call and is owned (unlike non-tail `sum`, which borrows). Owning it
    // keeps the call in tail position so it can be flattened into a loop, and frees
    // the input cell-by-cell as it is consumed.
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let sumAcc acc xs =
                  match xs with
                  | [] -> acc
                  | x :: r -> sumAcc (acc + x) r
            "#},
            "sumAcc",
        ),
        vec![false, false],
    );
}

#[test]
fn borrow_all_pos_owns_the_list() {
    // A tail-recursive predicate also owns its list (its self-call is the
    // then-branch tail), where the otherwise-identical non-recursive `isEmpty`
    // borrows.
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let allPos xs =
                  match xs with
                  | [] -> true
                  | x :: r -> if x > 0 then allPos r else false
            "#},
            "allPos",
        ),
        vec![false],
    );
}

#[test]
fn borrow_find_owns_both_args() {
    // The predicate and the list both flow into the else-branch tail self-call, so
    // both are owned.
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let find p xs =
                  match xs with
                  | [] -> 0
                  | x :: r -> if p x then x else find p r
            "#},
            "find",
        ),
        vec![false, false],
    );
}

#[test]
fn borrow_inc() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let inc xs =
                  match xs with
                  | [] -> []
                  | x :: r -> (x + 1) :: inc r
            "#},
            "inc",
        ),
        vec![false],
    );
}

#[test]
fn borrow_dbl() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let dbl xs =
                  match xs with
                  | [] -> []
                  | x :: r -> (x * 2) :: dbl r
            "#},
            "dbl",
        ),
        vec![false],
    );
}

#[test]
fn borrow_map() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let map f xs =
                  match xs with
                  | [] -> []
                  | x :: r -> f x :: map f r
            "#},
            "map",
        ),
        vec![false, false],
    );
}

#[test]
fn borrow_first() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let first xs =
                  match xs with
                  | [] -> 0
                  | x :: _ -> x
            "#},
            "first",
        ),
        vec![false],
    );
}

#[test]
fn borrow_id() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let id x = x
            "#},
            "id",
        ),
        vec![false],
    );
}

#[test]
fn borrow_k() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let k x y = x
            "#},
            "k",
        ),
        vec![false, true],
    );
}

#[test]
fn borrow_snd() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let snd a b = b
            "#},
            "snd",
        ),
        vec![true, false],
    );
}

#[test]
fn borrow_konst() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let konst x = 5
            "#},
            "konst",
        ),
        vec![true],
    );
}

#[test]
fn borrow_drop2() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let drop2 x y = 0
            "#},
            "drop2",
        ),
        vec![true, true],
    );
}

#[test]
fn borrow_addone() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let addOne x = x + 1
            "#},
            "addOne",
        ),
        vec![false],
    );
}

#[test]
fn borrow_usetwice() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let useTwice x = x + x
            "#},
            "useTwice",
        ),
        vec![false],
    );
}

#[test]
fn borrow_addxy() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let addXY x y = x + y
            "#},
            "addXY",
        ),
        vec![false, false],
    );
}

#[test]
fn borrow_apply() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let apply f x = f x
            "#},
            "apply",
        ),
        vec![false, false],
    );
}

#[test]
fn borrow_twice() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let twice f x = f (f x)
            "#},
            "twice",
        ),
        vec![false, false],
    );
}

#[test]
fn borrow_compose() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let compose f g x = f (g x)
            "#},
            "compose",
        ),
        vec![false, false, false],
    );
}

#[test]
fn borrow_depth() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                type T = | A Int | B Int Int

                let depth t =
                  match t with
                  | A _ -> 1
                  | B _ _ -> 2
            "#},
            "depth",
        ),
        vec![true],
    );
}

#[test]
fn borrow_swap() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                type T = | A Int | B Int Int

                let swap t =
                  match t with
                  | A x -> A x
                  | B x y -> B y x
            "#},
            "swap",
        ),
        vec![false],
    );
}

#[test]
fn borrow_sel() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                type T = | A Int | B Int Int

                let sel t =
                  match t with
                  | A x -> x
                  | B x y -> x
            "#},
            "sel",
        ),
        vec![false],
    );
}

#[test]
fn borrow_mk() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                type T = | A Int | B Int Int

                let mk a = B a a
            "#},
            "mk",
        ),
        vec![false],
    );
}

#[test]
fn borrow_getx_closed() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                public type P = { x : Int, y : Int }

                public getX : P -> Int
                let getX p = p.x
            "#},
            "getX",
        ),
        vec![true],
    );
}

#[test]
fn borrow_shift_closed() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                public type P = { x : Int, y : Int }

                public shift : P -> P
                let shift p = { p with x = p.x + 1 }
            "#},
            "shift",
        ),
        vec![false],
    );
}

#[test]
fn borrow_gety_row() {
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                public getY : { y : Int | 'r } -> Int
                let getY r = r.y
            "#},
            "getY",
        ),
        vec![false, false],
    );
}

// ===========================================================================
// Inter-procedural borrowing: a parameter only *forwarded* to another function's
// borrowing parameter is itself borrowed. Acyclic forwarding resolves as an
// ordinary query dependency; mutual recursion resolves through the salsa fixpoint.
// ===========================================================================

#[test]
fn borrow_forward() {
    // `len` borrows its list; `f` only forwards `xs` to `len`, so `f` borrows it
    // too (the headline inter-procedural case).
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let len xs =
                  match xs with
                  | [] -> 0
                  | _ :: r -> 1 + len r

                let f xs = len xs
            "#},
            "f",
        ),
        vec![true],
    );
}

#[test]
fn borrow_forward_chain() {
    // Borrowing composes along a forwarding chain `f -> g -> len`.
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let len xs =
                  match xs with
                  | [] -> 0
                  | _ :: r -> 1 + len r

                let g xs = len xs

                let f xs = g xs
            "#},
            "f",
        ),
        vec![true],
    );
}

#[test]
fn borrow_forward_consume() {
    // Forwarding to a function that *owns* its parameter (a rebuilder) leaves the
    // forwarder owning it too — borrowing only follows borrowing callees.
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let inc xs =
                  match xs with
                  | [] -> []
                  | x :: r -> (x + 1) :: inc r

                let f xs = inc xs
            "#},
            "f",
        ),
        vec![false],
    );
}

#[test]
fn borrow_forward_through_over_application() {
    // An over-application forwards its leading arguments to the callee's saturated
    // prefix, which follows the callee's borrow signature: `chooseByLen` borrows its
    // list (it forwards it to the borrowing `len` and returns a top-level function),
    // and `f` forwards `xs` into that borrowing prefix (then applies a surplus
    // argument), so `f` borrows `xs` too.
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let add1 x = x + 1

                let add10 x = x + 10

                let len xs =
                  match xs with
                  | [] -> 0
                  | _ :: r -> 1 + len r

                let chooseByLen xs = if len xs > 3 then add10 else add1

                let f xs = chooseByLen xs 5
            "#},
            "f",
        ),
        vec![true],
    );
}

#[test]
fn borrow_forward_to_std() {
    // Forwarding to a borrowing function in *another module* (the standard
    // library `List.isEmpty`, a pure inspector that never recurses) borrows the
    // parameter too. (A tail-recursive inspector like `List.length` threads its
    // list through a loop and so owns it — see `borrow_loop_threaded_owns`.)
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let f xs = List.isEmpty xs
            "#},
            "f",
        ),
        vec![true],
    );
}

#[test]
fn borrow_loop_threaded_owns() {
    // A self-tail-recursive function compiles to a loop that threads its list
    // parameter (rebinding it to the tail each iteration), so it *owns* the list
    // even though it only inspects elements — unlike the non-tail inspector form,
    // which borrows. This is why `List.length` (a tail-recursive accumulator, for
    // constant stack) does not borrow its argument.
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let count acc xs =
                  match xs with
                  | [] -> acc
                  | _ :: rest -> count (acc + 1) rest
            "#},
            "count",
        ),
        vec![false, false],
    );
}

#[test]
fn borrow_forward_partial_is_owned() {
    // A *partial* application is not a saturated direct call, so the callee's
    // borrowing is not exploited and the forwarded parameter stays owned.
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let g a b = 0

                let f x = g x
            "#},
            "f",
        ),
        vec![false],
    );
}

#[test]
fn borrow_forward_mixed_escape_is_owned() {
    // A parameter both forwarded to a borrowing function and stored (escaping into
    // a tuple) is owned — any escape wins over the borrow.
    assert_eq!(
        crate::tests::borrow_sig(
            indoc! {r#"
                module M

                let len xs =
                  match xs with
                  | [] -> 0
                  | _ :: r -> 1 + len r

                let f xs = (len xs, xs)
            "#},
            "f",
        ),
        vec![false],
    );
}

#[test]
fn borrow_mutual_recursion() {
    // Mutually-recursive inspectors that each forward the tail to the other form a
    // borrow cycle; the salsa fixpoint converges with both borrowing their list.
    for name in ["isEven", "isOdd"] {
        assert_eq!(
            crate::tests::borrow_sig(
                indoc! {r#"
                    module M

                    let isEven xs =
                      match xs with
                      | [] -> true
                      | _ :: r -> isOdd r

                    let isOdd xs =
                      match xs with
                      | [] -> false
                      | _ :: r -> isEven r
                "#},
                name,
            ),
            vec![true],
            "{name} should borrow its list",
        );
    }
}

#[test]
fn borrow_mutual_recursion_is_sound() {
    // The reference-counted output of a borrow cycle stays sound on both members.
    let src = indoc! {r#"
        module M

        let isEven xs =
          match xs with
          | [] -> true
          | _ :: r -> isOdd r

        let isOdd xs =
          match xs with
          | [] -> false
          | _ :: r -> isEven r
    "#};
    sound(src, "isEven");
    sound(src, "isOdd");
}

// ===========================================================================
// Tail-call flattening: a self-tail-recursive function becomes a loop. A
// constructor-wrapped recursion additionally threads a destination hole; a plain
// tail recursion is a hole-free loop; a non-tail or multiply-recursive function is
// left as ordinary recursion. Every transformed function must stay sound.
// ===========================================================================

/// Asserts `name` is sound and was flattened into a loop with a destination hole
/// (the constructor-wrapped, "modulo cons" case).
#[track_caller]
fn trmc_spine(src: &str, name: &str) {
    sound(src, name);
    let out = rc_checked(src, name);
    assert!(out.contains("(join "), "expected a loop for `{name}`:\n{out}");
    assert!(out.contains("holestart"), "expected a hole for `{name}`:\n{out}");
    assert!(out.contains("holefill"), "expected a hole fill for `{name}`:\n{out}");
    assert!(out.contains("holeclose"), "expected a hole close for `{name}`:\n{out}");
    assert!(out.contains("(recur"), "expected a back-edge for `{name}`:\n{out}");
}

/// Asserts `name` is sound and was flattened into a plain (hole-free) loop.
#[track_caller]
fn trmc_plain(src: &str, name: &str) {
    sound(src, name);
    let out = rc_checked(src, name);
    assert!(out.contains("(join "), "expected a loop for `{name}`:\n{out}");
    assert!(out.contains("(recur"), "expected a back-edge for `{name}`:\n{out}");
    assert!(!out.contains("holestart"), "unexpected hole for `{name}`:\n{out}");
}

/// Asserts `name` is sound and was left as ordinary recursion (no loop).
#[track_caller]
fn no_trmc(src: &str, name: &str) {
    sound(src, name);
    let out = rc_checked(src, name);
    assert!(!out.contains("(join "), "unexpected loop for `{name}`:\n{out}");
    assert!(!out.contains("(recur"), "unexpected back-edge for `{name}`:\n{out}");
}

#[test]
fn trmc_inc() {
    trmc_spine(
        indoc! {r#"
            module M

            let inc xs =
              match xs with
              | [] -> []
              | x :: r -> (x + 1) :: inc r
        "#},
        "inc",
    );
}

#[test]
fn trmc_map() {
    trmc_spine(
        indoc! {r#"
            module M

            let map f xs =
              match xs with
              | [] -> []
              | x :: r -> f x :: map f r
        "#},
        "map",
    );
}

#[test]
fn trmc_filter() {
    // The cons arm both extends the spine (then) and recurses without extending
    // (else); the hole threads through both.
    trmc_spine(
        indoc! {r#"
            module M

            let keep xs =
              match xs with
              | [] -> []
              | x :: r -> if x > 0 then x :: keep r else keep r
        "#},
        "keep",
    );
}

#[test]
fn trmc_non_last_recursive_field() {
    // The recursive call is the *first* field; the later field (`x + 1`) is pure
    // and total, so it is hoisted ahead of the back-edge and the function is
    // flattened with the hole at field 0.
    trmc_spine(
        indoc! {r#"
            module M

            type Snoc = | Empty | Snoc Snoc Int

            let bump xs =
              match xs with
              | Empty -> Empty
              | Snoc rest x -> Snoc (bump rest) (x + 1)
        "#},
        "bump",
    );
}

#[test]
fn trmc_reverse_is_a_plain_loop() {
    trmc_plain(
        indoc! {r#"
            module M

            let rev acc xs =
              match xs with
              | [] -> acc
              | x :: r -> rev (x :: acc) r
        "#},
        "rev",
    );
}

#[test]
fn trmc_sum_acc_is_a_plain_loop() {
    trmc_plain(
        indoc! {r#"
            module M

            let sumAcc acc xs =
              match xs with
              | [] -> acc
              | x :: r -> sumAcc (acc + x) r
        "#},
        "sumAcc",
    );
}

#[test]
fn trmc_find_is_a_plain_loop() {
    trmc_plain(
        indoc! {r#"
            module M

            let find p xs =
              match xs with
              | [] -> 0
              | x :: r -> if p x then x else find p r
        "#},
        "find",
    );
}

#[test]
fn no_trmc_non_tail_sum() {
    // `x + sum r` is not in tail position (the recursion feeds `+`), so the
    // function is left as ordinary recursion.
    no_trmc(
        indoc! {r#"
            module M

            let sum xs =
              match xs with
              | [] -> 0
              | x :: r -> x + sum r
        "#},
        "sum",
    );
}

#[test]
fn no_trmc_two_recursions_in_one_constructor() {
    // `Node (incT l) (incT r)` has two self-calls in one constructor: not
    // tail-modulo-cons.
    no_trmc(
        indoc! {r#"
            module M

            type Tree = | Leaf Int | Node Tree Tree

            let incT t =
              match t with
              | Leaf n -> Leaf (n + 1)
              | Node l r -> Node (incT l) (incT r)
        "#},
        "incT",
    );
}

#[test]
fn no_trmc_reorder_unsafe_later_field() {
    // The recursive call is not last, and the later field divides by a value (it
    // may abort), so hoisting it ahead of the recursion is unsafe: bail.
    no_trmc(
        indoc! {r#"
            module M

            type Snoc = | Empty | Snoc Snoc Int

            let bump xs =
              match xs with
              | Empty -> Empty
              | Snoc rest x -> Snoc (bump rest) (100 / x)
        "#},
        "bump",
    );
}

#[test]
fn no_trmc_non_recursive() {
    no_trmc(
        indoc! {r#"
            module M

            let isEmpty xs =
              match xs with
              | [] -> true
              | _ :: _ -> false
        "#},
        "isEmpty",
    );
}

#[test]
fn trmc_inc_exact_shape() {
    // The full transformed body: a hole-carrying loop. The reuse token (`%10`) is
    // preserved on the recycled cell, which is built with a placeholder recursive
    // field (`()`) and linked via `holefill` *before* the back-edge — so a unique
    // list still rebuilds in place and the recursion runs in constant stack. The
    // base and the unreachable exhaustive fallthrough both close the hole, keeping
    // every path's reference state consistent.
    let out = rc_checked(
        indoc! {r#"
            module M

            let inc xs =
              match xs with
              | [] -> []
              | x :: r -> (x + 1) :: inc r
        "#},
        "inc",
    );
    assert_eq!(
        out,
        "fn0(%0) = (holestart %11; (join [%0, %11] (let %3 = %0; (let %4 = (tag %3); \
         (let %5 = (= %4 0); (if %5 (drop %3; (holeclose %11 (data 0))) \
         (let %6 = (tag %3); (let %7 = (= %6 1); (if %7 \
         (let %1 = (field 0 %3); (let %2 = (field 1 %3); (reset %10 = %3; \
         (let %8 = (+ %1 1); (let %12 = (holefill %11 1 (data@%10 1 %8 ())); \
          (recur %2 %12)))))) (drop %3; (holeclose %11 <error>)))))))))))\n"
    );
}

// ---------------------------------------------------------------------------
// Tail-call flattening of row-polymorphic functions. A function carrying leading
// offset-evidence parameters calls itself curried (partially applied to its
// evidence, then to the real arguments); that pair is normalized to a saturated
// self-call before reference counting, so it flattens exactly like a monomorphic
// one, with the evidence riding through the loop as a loop-carried parameter.
// ===========================================================================

#[test]
fn trmc_rowpoly_plain() {
    // A row-polymorphic accumulator fold (`r.n` forces an open record, so `sumNs`
    // carries one offset-evidence parameter). The tail self-call is plain, so it
    // becomes a hole-free loop; the evidence rides unchanged through the back-edge.
    trmc_plain(
        indoc! {r#"
            module M

            sumNs : Int -> List ({ n : Int | 'r }) -> Int
            let sumNs acc rs =
              match rs with
              | [] -> acc
              | r :: rest -> sumNs (acc + r.n) rest
        "#},
        "sumNs",
    );
}

#[test]
fn trmc_rowpoly_multi_evidence() {
    // Two open-record fields (`r.x`, `r.y`) ⇒ two offset-evidence parameters. Both
    // ride through the loop, reassigned to themselves on every back-edge.
    trmc_plain(
        indoc! {r#"
            module M

            addBoth : Int -> List ({ x : Int, y : Int | 'r }) -> Int
            let addBoth acc rs =
              match rs with
              | [] -> acc
              | r :: rest -> addBoth (acc + r.x + r.y) rest
        "#},
        "addBoth",
    );
}

#[test]
fn trmc_rowpoly_spine() {
    // A row-polymorphic modulo-cons rebuild: the recursion sits under `::`, so it
    // becomes a spine-building loop. The element field `r.x` is read through offset
    // evidence that the loop carries.
    trmc_spine(
        indoc! {r#"
            module M

            firsts : List ({ x : Int | 'r }) -> List Int
            let firsts rs =
              match rs with
              | [] -> []
              | r :: rest -> r.x :: firsts rest
        "#},
        "firsts",
    );
}

#[test]
fn trmc_rowpoly_record_update_spine() {
    // A modulo-cons rebuild whose element is a row-polymorphic `{ r with … }`
    // update (using the carried evidence both to read and to replace the field).
    trmc_spine(
        indoc! {r#"
            module M

            bumpNs : List ({ n : Int | 'r }) -> List ({ n : Int | 'r })
            let bumpNs rs =
              match rs with
              | [] -> []
              | r :: rest -> { r with n = r.n + 1 } :: bumpNs rest
        "#},
        "bumpNs",
    );
}

#[test]
fn trmc_rowpoly_filter() {
    // A row-polymorphic filter: the kept branch extends the spine and the dropped
    // branch is a plain tail call — both flatten, threading the hole and evidence.
    trmc_spine(
        indoc! {r#"
            module M

            keepPos : List ({ n : Int | 'r }) -> List ({ n : Int | 'r })
            let keepPos rs =
              match rs with
              | [] -> []
              | r :: rest -> if r.n > 0 then r :: keepPos rest else keepPos rest
        "#},
        "keepPos",
    );
}

#[test]
fn no_trmc_rowpoly_non_tail() {
    // `r.n + sumP rest` is not in tail position (the recursion feeds `+`), so the
    // row-polymorphic function is left as ordinary recursion — the curried
    // self-call is still normalized to a saturated direct call, but no loop forms.
    no_trmc(
        indoc! {r#"
            module M

            sumP : List ({ n : Int | 'r }) -> Int
            let sumP rs =
              match rs with
              | [] -> 0
              | r :: rest -> r.n + sumP rest
        "#},
        "sumP",
    );
}

#[test]
fn trmc_rowpoly_plain_exact_shape() {
    // The full transformed body of a row-polymorphic accumulator fold: a hole-free
    // loop whose first loop-carried parameter is the offset evidence (`%4`),
    // reassigned to itself on the back-edge (`recur %4 …`). The evidence is read by
    // the field projection (`field 0+%4 %2`) and consumed once per path — by the
    // back-edge on the recursive path, by a `drop` on the base/fallthrough paths —
    // so no duplicate is needed and every path's reference state stays consistent.
    let out = rc_checked(
        indoc! {r#"
            module M

            sumNs : Int -> List ({ n : Int | 'r }) -> Int
            let sumNs acc rs =
              match rs with
              | [] -> acc
              | r :: rest -> sumNs (acc + r.n) rest
        "#},
        "sumNs",
    );
    assert_eq!(
        out,
        "fn0(%4, %0, %1) = (join [%4, %0, %1] (let %5 = %1; (let %6 = (tag %5); \
         (let %7 = (= %6 0); (if %7 (drop %4; (drop %5; %0)) (let %8 = (tag %5); \
         (let %9 = (= %8 1); (if %9 (let %2 = (field 0 %5); (let %3 = (field 1 %5); \
         (drop %5; (let %10 = (field 0+%4 %2); (drop %2; (let %11 = (+ %0 %10); \
         (recur %4 %11 %3))))))) (drop %0; (drop %4; (drop %5; <error>)))))))))))\n"
    );
}

// ---------------------------------------------------------------------------
// Tail-call flattening of nested-constructor wrapping: a recursion several
// constructors deep (`K1 a (K2 b (f x))`, `x :: x :: f r`) becomes a loop that
// links a chain of cells into the spine, one hole fill per cell.
// ===========================================================================

#[test]
fn trmc_nested_two_deep() {
    // The recursion sits under two `::`, so each iteration links two cons cells.
    trmc_spine(
        indoc! {r#"
            module M

            let stutter xs =
              match xs with
              | [] -> []
              | x :: rest -> x :: x :: stutter rest
        "#},
        "stutter",
    );
}

#[test]
fn trmc_nested_three_deep() {
    // Three constructors deep: a chain of three hole fills per iteration.
    trmc_spine(
        indoc! {r#"
            module M

            let triple xs =
              match xs with
              | [] -> []
              | x :: rest -> x :: x :: x :: triple rest
        "#},
        "triple",
    );
}

#[test]
fn trmc_nested_custom_adt() {
    // A two-deep nest over a user ADT, with pure later arguments (`a + 1`,
    // `a + 2`) hoisted ahead of the back-edge.
    trmc_spine(
        indoc! {r#"
            module M

            type Nums = | End | Num Int Nums

            let bump xs =
              match xs with
              | End -> End
              | Num a rest -> Num (a + 1) (Num (a + 2) (bump rest))
        "#},
        "bump",
    );
}

#[test]
fn trmc_nested_filter() {
    // A filter whose kept branch nests two constructors and whose dropped branch
    // is a plain tail call: both flatten, threading the hole.
    trmc_spine(
        indoc! {r#"
            module M

            let keepDup xs =
              match xs with
              | [] -> []
              | x :: rest -> if x > 0 then x :: x :: keepDup rest else keepDup rest
        "#},
        "keepDup",
    );
}

#[test]
fn trmc_nested_rowpoly() {
    // Row-polymorphic (the offset evidence for `r.x`) *and* nested: the curried
    // self-call is normalized to a saturated one before reference counting, then
    // the chain of cons cells flattens with the evidence carried through the loop.
    trmc_spine(
        indoc! {r#"
            module M

            firsts2 : List ({ x : Int | 'r }) -> List Int
            let firsts2 rs =
              match rs with
              | [] -> []
              | r :: rest -> r.x :: r.x :: firsts2 rest
        "#},
        "firsts2",
    );
}

#[test]
fn no_trmc_nested_reorder_unsafe() {
    // The recursion is the non-last field of the inner constructor, and the later
    // field `100 / x` may abort, so hoisting it ahead of the recursion is unsafe:
    // the whole function stays ordinary recursion.
    no_trmc(
        indoc! {r#"
            module M

            type Snoc = | Empty | Snoc Snoc Int

            let bad xs =
              match xs with
              | Empty -> Empty
              | Snoc rest x -> Snoc (Snoc (bad rest) (100 / x)) (x + 1)
        "#},
        "bad",
    );
}

#[test]
fn trmc_nested_two_deep_exact_shape() {
    // The full transformed body of a two-deep nest (`x :: x :: stutter rest`): a
    // hole-carrying loop that, each iteration, builds the two cons cells in their
    // original (reference-count) order — the inner one (`%12`, freshly allocated,
    // dropping in its `dup` of the shared head `%1`) before the outer (`%13`,
    // recycling the matched cell via the reuse token `%10`) — then links them into
    // the spine outermost-first (`%14` stores the outer at the loop hole, `%15`
    // stores the inner into the outer's recursive field) before the back-edge.
    let out = rc_checked(
        indoc! {r#"
            module M

            let stutter xs =
              match xs with
              | [] -> []
              | x :: rest -> x :: x :: stutter rest
        "#},
        "stutter",
    );
    assert_eq!(
        out,
        "fn0(%0) = (holestart %11; (join [%0, %11] (let %3 = %0; (let %4 = (tag %3); \
         (let %5 = (= %4 0); (if %5 (drop %3; (holeclose %11 (data 0))) (let %6 = (tag %3); \
         (let %7 = (= %6 1); (if %7 (let %1 = (field 0 %3); (let %2 = (field 1 %3); \
         (reset %10 = %3; (let %12 = (dup %1; (data 1 %1 ())); (let %13 = (data@%10 1 %1 ()); \
         (let %14 = (holefill %11 1 %13); (let %15 = (holefill %14 1 %12); \
         (recur %2 %15)))))))) (drop %3; (holeclose %11 <error>)))))))))))\n"
    );
}

// ---------------------------------------------------------------------------
// Purity/totality analysis (`is_pure_total`) and the reorder-safety it unlocks:
// a later constructor argument hoisted ahead of the back-edge may call a pure,
// total (non-recursive, effect-free, abort-free) function. This analysis is the
// correctness guard — the reference-count oracle does not check effect ordering.
// ===========================================================================

use crate::tests::pure_total;

#[test]
fn pure_total_arithmetic() {
    assert!(pure_total("module M\n\nlet f x = x + 1\n", "f"));
}

#[test]
fn pure_total_constructor() {
    assert!(pure_total("module M\n\nlet f x = Some x\n", "f"));
}

#[test]
fn pure_total_division_by_literal() {
    assert!(pure_total("module M\n\nlet f x = x / 2\n", "f"));
}

#[test]
fn not_pure_total_division_by_variable() {
    assert!(!pure_total("module M\n\nlet f x y = x / y\n", "f"));
}

#[test]
fn not_pure_total_self_recursive() {
    // Termination of a recursive function is undecidable, so it is conservatively
    // not total.
    assert!(!pure_total(
        indoc! {r#"
            module M

            let countDown n = if n <= 0 then 0 else countDown (n - 1)
        "#},
        "countDown",
    ));
}

#[test]
fn not_pure_total_mutually_recursive() {
    // A mutual-recursion cycle resolves to not-total.
    let src = indoc! {r#"
        module M

        let isEven n = if n <= 0 then true else isOdd (n - 1)
        let isOdd n = if n <= 0 then false else isEven (n - 1)
    "#};
    assert!(!pure_total(src, "isEven"));
    assert!(!pure_total(src, "isOdd"));
}

#[test]
fn pure_total_calls_pure_function() {
    // Calling a non-recursive pure function is pure and total.
    assert!(pure_total(
        indoc! {r#"
            module M

            let g x = x + 1
            let f x = g x + g x
        "#},
        "f",
    ));
}

#[test]
fn not_pure_total_calls_recursive_function() {
    assert!(!pure_total(
        indoc! {r#"
            module M

            let len xs =
              match xs with
              | [] -> 0
              | _ :: rest -> 1 + len rest

            let f xs = len xs
        "#},
        "f",
    ));
}

#[test]
fn not_pure_total_capability_effect() {
    // A console write is an observable effect.
    assert!(!pure_total(
        indoc! {r#"
            module M

            public shout : Runtime -> Unit
            let shout rt = rt.console.writeLine "hi"
        "#},
        "shout",
    ));
}

#[test]
fn trmc_reorder_pure_call() {
    // The recursion is the first field; the later field calls a non-recursive pure
    // function, which the analysis now admits, so the function flattens.
    trmc_spine(
        indoc! {r#"
            module M

            type Snoc = | Empty | Snoc Snoc Int

            let twice x = x + x

            let bump xs =
              match xs with
              | Empty -> Empty
              | Snoc rest x -> Snoc (bump rest) (twice x)
        "#},
        "bump",
    );
}

#[test]
fn trmc_reorder_division_by_literal() {
    // The later field divides by a non-zero literal, which cannot abort, so the
    // function flattens.
    trmc_spine(
        indoc! {r#"
            module M

            type Snoc = | Empty | Snoc Snoc Int

            let bump xs =
              match xs with
              | Empty -> Empty
              | Snoc rest x -> Snoc (bump rest) (x / 2)
        "#},
        "bump",
    );
}

#[test]
fn no_trmc_reorder_recursive_call() {
    // The later field calls a recursive function (not provably total), so hoisting
    // it ahead of the recursion is unsafe: the function stays ordinary recursion.
    no_trmc(
        indoc! {r#"
            module M

            type Snoc = | Empty | Snoc Snoc Int

            let countDown n = if n <= 0 then 0 else countDown (n - 1)

            let bump xs =
              match xs with
              | Empty -> Empty
              | Snoc rest x -> Snoc (bump rest) (countDown x)
        "#},
        "bump",
    );
}

// ---------------------------------------------------------------------------
// Mutual-recursion flattening: a plain-tail-recursive group is detected, combined
// into one tag-dispatched self-recursive loop, and each member becomes a wrapper.
// Only intra-module, plain-tail, monomorphic, lambda-free groups qualify.
// ===========================================================================

use crate::tests::{mutual_combined, mutual_member_groups};

#[test]
fn mutual_pair_is_a_group() {
    let groups = mutual_member_groups(indoc! {r#"
        module M

        let isEven n = if n <= 0 then true else isOdd (n - 1)
        let isOdd n = if n <= 0 then false else isEven (n - 1)
    "#});
    assert_eq!(groups, vec![vec!["isEven".to_owned(), "isOdd".to_owned()]]);
}

#[test]
fn mutual_three_cycle_is_a_group() {
    let groups = mutual_member_groups(indoc! {r#"
        module M

        let a n = if n <= 0 then 0 else b (n - 1)
        let b n = if n <= 0 then 1 else c (n - 1)
        let c n = if n <= 0 then 2 else a (n - 1)
    "#});
    assert_eq!(groups, vec![vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]]);
}

#[test]
fn mutual_combined_is_sound_and_flattened() {
    let (sound, flattened, wrappers_sound) = mutual_combined(
        indoc! {r#"
            module M

            let isEven n = if n <= 0 then true else isOdd (n - 1)
            let isOdd n = if n <= 0 then false else isEven (n - 1)
        "#},
        "isEven",
    );
    assert!(sound, "combined function is reference-count sound");
    assert!(flattened, "combined function flattens to a loop");
    assert!(wrappers_sound, "member wrappers are reference-count sound");
}

#[test]
fn no_mutual_non_tail() {
    // `1 + g n` is not a tail call (it feeds `+`), so there is no plain-tail cycle.
    let groups = mutual_member_groups(indoc! {r#"
        module M

        let f n = if n <= 0 then 0 else 1 + g (n - 1)
        let g n = if n <= 0 then 0 else 1 + f (n - 1)
    "#});
    assert!(groups.is_empty(), "non-tail mutual recursion is not a group: {groups:?}");
}

#[test]
fn no_mutual_modulo_cons() {
    // The sibling call is wrapped in a constructor (`x :: g r`), the deferred
    // mutual-modulo-cons case — not a plain-tail cycle.
    let groups = mutual_member_groups(indoc! {r#"
        module M

        let f xs =
          match xs with
          | [] -> []
          | x :: r -> x :: g r

        let g xs =
          match xs with
          | [] -> []
          | x :: r -> x :: f r
    "#});
    assert!(groups.is_empty(), "modulo-cons mutual recursion is not a group: {groups:?}");
}

#[test]
fn no_mutual_self_recursion_only() {
    // A single self-recursive function is the ordinary per-definition loop, not a
    // mutual group.
    let groups = mutual_member_groups(indoc! {r#"
        module M

        let countDown n = if n <= 0 then 0 else countDown (n - 1)
    "#});
    assert!(groups.is_empty(), "self-recursion is not a mutual group: {groups:?}");
}

#[test]
fn no_mutual_with_nested_lambda() {
    // A member containing a nested lambda is excluded (the combined function would
    // need to carry the lifted lambda — deferred).
    let groups = mutual_member_groups(indoc! {r#"
        module M

        let f n = if n <= 0 then (fun x -> x) 0 else g (n - 1)
        let g n = if n <= 0 then 1 else f (n - 1)
    "#});
    assert!(groups.is_empty(), "a member with a lambda is not flattened: {groups:?}");
}

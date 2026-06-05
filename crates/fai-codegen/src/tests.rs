//! End-to-end JIT tests: compile small programs and run them, asserting their
//! console output and a clean (leak-free) exit.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use fai_core::ir::LoweredDef;
use fai_db::{Db, FaiDatabase};
use fai_rc::rc;
use fai_resolve::{DefId, module_defs, module_name};
use fai_runtime as rt;
use fai_span::SourceId;
use fai_syntax::Symbol;

use crate::jit_run;

// The runtime's console sink and live-object counter are process-global.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lowers every definition (user modules + prelude) and runs the entry file's
/// `main` through the JIT, returning `(exit_code, captured_output)`.
pub(crate) fn run(src: &str) -> (i32, String) {
    let mut db = FaiDatabase::new();
    fai_types::prelude::load_prelude(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let user = db.source_file(id).unwrap();

    let mut defs: Vec<LoweredDef> = Vec::new();
    let mut arity: HashMap<DefId, usize> = HashMap::new();
    let mut labels: HashMap<SourceId, String> = HashMap::new();
    for file in db.all_source_files() {
        let label =
            module_name(&db, file).map_or_else(|| "M".to_owned(), |m| m.0.as_str().to_owned());
        labels.insert(file.source(&db), label);
        for d in &module_defs(&db, file).defs {
            let lowered = rc(&db, file, d.name);
            let def = DefId::new(file.source(&db), d.name);
            arity.insert(def, lowered.entry().params.len());
            defs.push((*lowered).clone());
        }
    }

    let namer =
        |d: DefId| format!("fai_{}_{}", labels.get(&d.file).cloned().unwrap_or_default(), d.name);
    let arity_of = |d: DefId| arity.get(&d).copied().unwrap_or(1);
    let entry = DefId::new(user.source(&db), Symbol::intern("main"));

    let _g = lock();
    rt::capture_start();
    let code = jit_run(&defs, entry, &namer, &arity_of);
    let out = rt::capture_take();
    (code, out)
}

fn main_printing(expr: &str) -> String {
    format!(
        "module M\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime ({expr})\n"
    )
}

#[test]
fn hello_world() {
    let src = main_printing("\"Hello, Fai!\"");
    let (code, out) = run(&src);
    assert_eq!(code, 0, "clean exit");
    assert_eq!(out, "Hello, Fai!\n");
}

#[test]
fn arithmetic() {
    let (code, out) = run(&main_printing("intToString (1 + 2 * 3)"));
    assert_eq!(code, 0);
    assert_eq!(out, "7\n");
}

#[test]
fn string_concat() {
    let (code, out) = run(&main_printing("\"a\" ++ \"b\" ++ \"c\""));
    assert_eq!(code, 0);
    assert_eq!(out, "abc\n");
}

#[test]
fn conditional() {
    let (code, out) = run(&main_printing("if 1 < 2 then \"yes\" else \"no\""));
    assert_eq!(code, 0);
    assert_eq!(out, "yes\n");
}

#[test]
fn cross_definition_call() {
    let src = "module M\n\nlet double x = x + x\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (intToString (double 21))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn saturated_curried_call() {
    let src = "module M\n\nlet add x y = x + y\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (intToString (add 40 2))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn partial_application_via_zero_arity_binding() {
    // `inc = add 1` is a zero-arity value (a partial application); applying it
    // exercises over-application and forcing.
    let src = "module M\n\nlet add x y = x + y\n\nlet inc = add 1\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (intToString (inc 41))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn higher_order_with_closure_capture() {
    let src = "module M\n\nlet apply f x = f x\n\nlet adder n = fun m -> n + m\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (intToString (apply (adder 40) 2))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn equality_on_strings() {
    let (code, out) = run(&main_printing("if \"a\" = \"a\" then \"eq\" else \"ne\""));
    assert_eq!(code, 0);
    assert_eq!(out, "eq\n");
}

#[test]
fn boxed_overflow_integer_round_trips() {
    // 2^62 overflows the immediate range and must box, print, and free cleanly.
    let (code, out) = run(&main_printing("intToString (4611686018427387904 + 0)"));
    assert_eq!(code, 0);
    assert_eq!(out, "4611686018427387904\n");
}

#[test]
fn short_circuit_and_or() {
    let (code, out) = run(&main_printing("if (1 < 2) && (3 < 4) then \"both\" else \"no\""));
    assert_eq!(code, 0);
    assert_eq!(out, "both\n");
    let (code, out) = run(&main_printing("if (5 < 2) || (3 < 4) then \"some\" else \"no\""));
    assert_eq!(code, 0);
    assert_eq!(out, "some\n");
}

#[test]
fn unary_negation() {
    let (code, out) = run(&main_printing("intToString (0 - (-5))"));
    assert_eq!(code, 0);
    assert_eq!(out, "5\n");
}

#[test]
fn nested_conditionals() {
    let src = "module M\n\nlet sign n = if n < 0 then \"neg\" else if n = 0 then \"zero\" else \"pos\"\n\npublic main : Runtime -> Unit\nlet main r = Console.writeLine r (sign (0 - 3))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "neg\n");
}

#[test]
fn let_block_in_body() {
    let src = "module M\n\nlet compute n =\n  let doubled = n + n\n  let plus1 = doubled + 1\n  plus1 * 2\n\npublic main : Runtime -> Unit\nlet main r = Console.writeLine r (intToString (compute 10))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn inequality_on_strings() {
    let (code, out) = run(&main_printing("if \"a\" <> \"b\" then \"diff\" else \"same\""));
    assert_eq!(code, 0);
    assert_eq!(out, "diff\n");
}

// ── M4: data types, pattern matching, lists, Float ────────────────────────────

#[test]
fn adt_constructor_and_match() {
    let src = "module M\n\n\
        type Shape =\n  | Circle Int\n  | Rect Int Int\n\n\
        public area : Shape -> Int\n\
        let area s =\n  match s with\n  | Circle r -> 3 * r * r\n  | Rect w h -> w * h\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (intToString (area (Rect 3 4)))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0, "clean exit");
    assert_eq!(out, "12\n");
}

#[test]
fn nullary_constructor_match() {
    let src = "module M\n\n\
        public describe : Option Int -> String\n\
        let describe opt =\n  match opt with\n  | None -> \"none\"\n  | Some n -> intToString n\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (describe None)\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "none\n");
}

#[test]
fn list_map_and_fold() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let xs = [1, 2, 3, 4]\n  \
          let ys = map (fun x -> x * x) xs\n  \
          Console.writeLine r (intToString (foldl (fun acc x -> acc + x) 0 ys))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "30\n");
}

#[test]
fn cons_and_recursive_length() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (intToString (length (1 :: 2 :: 3 :: [])))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "3\n");
}

#[test]
fn list_pattern_match() {
    let src = "module M\n\n\
        public firstOr : Int -> List Int -> Int\n\
        let firstOr d xs =\n  match xs with\n  | [] -> d\n  | x :: _ -> x\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (intToString (firstOr 0 [7, 8, 9]))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "7\n");
}

#[test]
fn option_combinators_from_prelude() {
    let (code, out) =
        run(&main_printing("intToString (withDefault 0 (mapOption (fun x -> x + 1) (Some 41)))"));
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn float_arithmetic_and_to_string() {
    let (code, out) = run(&main_printing("floatToString (1.5 + 2.5)"));
    assert_eq!(code, 0);
    assert_eq!(out, "4.0\n");
}

#[test]
fn float_conversions_and_sqrt() {
    let (code, out) = run(&main_printing("floatToString (sqrt (intToFloat 16))"));
    assert_eq!(code, 0);
    assert_eq!(out, "4.0\n");
}

#[test]
fn structural_equality_on_data() {
    let (code, out) = run(&main_printing("if [1, 2, 3] = [1, 2, 3] then \"eq\" else \"ne\""));
    assert_eq!(code, 0);
    assert_eq!(out, "eq\n");
}

#[test]
fn tuple_construction_and_destructuring() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let pair = (40, 2)\n  \
          let (a, b) = pair\n  \
          Console.writeLine r (intToString (a + b))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn nested_match_and_or_patterns() {
    let src = "module M\n\n\
        public classify : Int -> String\n\
        let classify n =\n  match n with\n  | 0 | 1 -> \"small\"\n  | _ -> \"big\"\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (classify 1)\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "small\n");
}

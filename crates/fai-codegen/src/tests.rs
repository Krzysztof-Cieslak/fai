//! End-to-end JIT tests: compile small programs and run them, asserting their
//! console output and a clean (leak-free) exit.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use fai_core::ir::LoweredDef;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_rc::rc;
use fai_resolve::{DefId, module_name};
use fai_runtime as rt;
use fai_span::SourceId;
use fai_syntax::Symbol;

use crate::jit_run;

// The runtime's console sink and live-object counter are process-global.
static LOCK: Mutex<()> = Mutex::new(());

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Lowers the definitions reachable from the entry file's `main` and runs it
/// through the JIT, returning `(exit_code, captured_output)`.
///
/// Only the *reachable* closure is compiled: starting at `main`, we follow each
/// definition's referenced globals transitively. The prelude is a large module,
/// so compiling all of it on every call would dominate the test cost even though
/// most programs touch only a handful of its functions.
pub(crate) fn run(src: &str) -> (i32, String) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let user = db.source_file(id).unwrap();

    // Resolve every source id to its file and module label up front — this is
    // cheap (no lowering or codegen) and lets us map a referenced global's
    // `DefId` back to the `SourceFile` whose definition we must lower.
    let mut files: HashMap<SourceId, SourceFile> = HashMap::new();
    let mut labels: HashMap<SourceId, String> = HashMap::new();
    for file in db.all_source_files() {
        let label =
            module_name(&db, file).map_or_else(|| "M".to_owned(), |m| m.0.as_str().to_owned());
        files.insert(file.source(&db), file);
        labels.insert(file.source(&db), label);
    }

    let entry = DefId::new(user.source(&db), Symbol::intern("main"));

    // Lower only the definitions transitively reachable from `main`.
    let mut defs: Vec<LoweredDef> = Vec::new();
    let mut arity: HashMap<DefId, usize> = HashMap::new();
    let mut seen: HashSet<DefId> = HashSet::new();
    let mut worklist = vec![entry];
    while let Some(def) = worklist.pop() {
        if !seen.insert(def) {
            continue;
        }
        let Some(&file) = files.get(&def.file) else { continue };
        let lowered = rc(&db, file, def.name);
        arity.insert(def, lowered.entry().params.len());
        worklist.extend(lowered.referenced_globals());
        defs.push((*lowered).clone());
    }

    let namer =
        |d: DefId| format!("fai_{}_{}", labels.get(&d.file).cloned().unwrap_or_default(), d.name);
    let arity_of = |d: DefId| arity.get(&d).copied().unwrap_or(1);

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
    let (code, out) = run(&main_printing("Int.toString (1 + 2 * 3)"));
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
    let src = "module M\n\nlet double x = x + x\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (Int.toString (double 21))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn saturated_curried_call() {
    let src = "module M\n\nlet add x y = x + y\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (Int.toString (add 40 2))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn partial_application_via_zero_arity_binding() {
    // `inc = add 1` is a zero-arity value (a partial application); applying it
    // exercises over-application and forcing.
    let src = "module M\n\nlet add x y = x + y\n\nlet inc = add 1\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (Int.toString (inc 41))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn higher_order_with_closure_capture() {
    let src = "module M\n\nlet apply f x = f x\n\nlet adder n = fun m -> n + m\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime (Int.toString (apply (adder 40) 2))\n";
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
    let (code, out) = run(&main_printing("Int.toString (4611686018427387904 + 0)"));
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
    let (code, out) = run(&main_printing("Int.toString (0 - (-5))"));
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
    let src = "module M\n\nlet compute n =\n  let doubled = n + n\n  let plus1 = doubled + 1\n  plus1 * 2\n\npublic main : Runtime -> Unit\nlet main r = Console.writeLine r (Int.toString (compute 10))\n";
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
        let main r = Console.writeLine r (Int.toString (area (Rect 3 4)))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0, "clean exit");
    assert_eq!(out, "12\n");
}

#[test]
fn nullary_constructor_match() {
    let src = "module M\n\n\
        public describe : Option Int -> String\n\
        let describe opt =\n  match opt with\n  | None -> \"none\"\n  | Some n -> Int.toString n\n\n\
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
          let ys = List.map (fun x -> x * x) xs\n  \
          Console.writeLine r (Int.toString (List.foldl (fun acc x -> acc + x) 0 ys))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "30\n");
}

#[test]
fn cons_and_recursive_length() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (Int.toString (List.length (1 :: 2 :: 3 :: [])))\n";
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
        let main r = Console.writeLine r (Int.toString (firstOr 0 [7, 8, 9]))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "7\n");
}

#[test]
fn option_combinators_from_prelude() {
    let (code, out) = run(&main_printing(
        "Int.toString (Option.withDefault 0 (Option.map (fun x -> x + 1) (Some 41)))",
    ));
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn float_arithmetic_and_to_string() {
    let (code, out) = run(&main_printing("Float.toString (1.5 + 2.5)"));
    assert_eq!(code, 0);
    assert_eq!(out, "4.0\n");
}

#[test]
fn float_conversions_and_sqrt() {
    let (code, out) = run(&main_printing("Float.toString (Float.sqrt (Int.toFloat 16))"));
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
          Console.writeLine r (Int.toString (a + b))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn dict_runs() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let d = Dict.insert 1 10 (Dict.insert 3 30 (Dict.insert 2 20 Dict.empty))\n  \
          Console.writeLine r (Int.toString (Option.withDefault 0 (Dict.get 2 d) + Dict.size d))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "23\n");
}

#[test]
fn string_ops_run() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (String.join \"-\" (List.map String.toUpper (String.split \" \" \"hi there world\")))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "HI-THERE-WORLD\n");
}

#[test]
fn sort_runs() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let xs = List.sort [3, 1, 2]\n  \
          Console.writeLine r (Int.toString (List.foldl (fun acc x -> acc * 10 + x) 0 xs))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "123\n");
}

#[test]
fn record_literal_and_field_access() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let p = { x = 1, y = 2 }\n  \
          Console.writeLine r (Int.toString (p.x + p.y))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0, "clean exit");
    assert_eq!(out, "3\n");
}

#[test]
fn record_update_runs() {
    let src = "module M\n\n\
        type P = { a : Int, b : Int }\n\n\
        public shift : P -> P\n\
        let shift p = { p with a = p.a + 10 }\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let q = shift { a = 1, b = 2 }\n  \
          Console.writeLine r (Int.toString (q.a + q.b))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "13\n");
}

#[test]
fn record_pattern_and_punning() {
    let src = "module M\n\n\
        type Point = { x : Int, y : Int }\n\n\
        public describe : Point -> Int\n\
        let describe pt =\n  match pt with\n  | { x = 0, y } -> y\n  | { x, y } -> x + y\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (Int.toString (describe { x = 0, y = 5 }))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "5\n");
}

#[test]
fn record_destructuring_let() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let p = { a = 40, b = 2 }\n  \
          let { a, b } = p\n  \
          Console.writeLine r (Int.toString (a + b))\n";
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

#[test]
fn float_comparison_runs() {
    let (code, out) = run(&main_printing("if 2.0 < 3.0 then \"lt\" else \"ge\""));
    assert_eq!(code, 0);
    assert_eq!(out, "lt\n");
}

#[test]
fn float_sort_orders_ascending() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let xs = List.sort [3.0, 1.0, 2.0]\n  \
          Console.writeLine r (String.join \" \" (List.map Float.toString xs))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "1.0 2.0 3.0\n");
}

#[test]
fn three_way_compare_runs() {
    let (code, out) = run(&main_printing(
        "Int.toString (compare 3 2) ++ Int.toString (compare 2 2) ++ Int.toString (compare 1 5)",
    ));
    assert_eq!(code, 0);
    assert_eq!(out, "10-1\n");
}

#[test]
fn structural_ordering_sorts_constructors_by_declaration_order() {
    // Declaration order is the ordering: Low < Mid < High.
    let src = "module M\n\n\
        type Rank =\n  | Low\n  | Mid\n  | High\n\n\
        public name : Rank -> String\n\
        let name x =\n  match x with\n  | Low -> \"L\"\n  | Mid -> \"M\"\n  | High -> \"H\"\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (String.join \"\" (List.map name (List.sort [High, Low, Mid, Low])))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "LLMH\n");
}

#[test]
fn structural_equality_on_records() {
    let (code, out) =
        run(&main_printing("if { x = 1, y = 2 } = { x = 1, y = 2 } then \"eq\" else \"ne\""));
    assert_eq!(code, 0);
    assert_eq!(out, "eq\n");
    let (code, out) =
        run(&main_printing("if { x = 1, y = 2 } = { x = 1, y = 9 } then \"eq\" else \"ne\""));
    assert_eq!(code, 0);
    assert_eq!(out, "ne\n");
}

#[test]
fn recursive_tree_fold() {
    let src = "module M\n\n\
        type Tree =\n  | Leaf\n  | Node Tree Int Tree\n\n\
        public sumTree : Tree -> Int\n\
        let sumTree t =\n  match t with\n  | Leaf -> 0\n  | Node l x rt -> sumTree l + x + sumTree rt\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (Int.toString (sumTree (Node (Node Leaf 1 Leaf) 2 (Node Leaf 3 Leaf))))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "6\n");
}

#[test]
fn result_pattern_match() {
    let src = "module M\n\n\
        public describe : Result Int String -> String\n\
        let describe res =\n  match res with\n  | Ok n -> Int.toString n\n  | Err e -> e\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (describe (Ok 5) ++ describe (Err \"boom\"))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "5boom\n");
}

#[test]
fn set_dedups_elements() {
    let src = "module M\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let s = Set.insert 3 (Set.insert 1 (Set.insert 3 Set.empty))\n  \
          Console.writeLine r (Int.toString (Set.size s))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "2\n");
}

#[test]
fn nested_record_field_access() {
    let src = "module M\n\n\
        type Vec = { x : Int, y : Int }\n\n\
        type Seg = { dest : Vec, src : Vec }\n\n\
        public main : Runtime -> Unit\n\
        let main r =\n  \
          let s = { src = { x = 1, y = 2 }, dest = { x = 3, y = 4 } }\n  \
          Console.writeLine r (Int.toString (s.src.x + s.dest.y))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "5\n");
}

#[test]
fn nested_constructor_patterns() {
    let src = "module M\n\n\
        public unwrap : Option (Option Int) -> Int\n\
        let unwrap oo =\n  match oo with\n  | Some (Some n) -> n\n  | Some None -> 0\n  | None -> 0\n\n\
        public main : Runtime -> Unit\n\
        let main r = Console.writeLine r (Int.toString (unwrap (Some (Some 7))))\n";
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "7\n");
}

#[test]
fn string_trim_and_lowercase() {
    let (code, out) = run(&main_printing("toLower (trim \"  Hello WORLD  \")"));
    assert_eq!(code, 0);
    assert_eq!(out, "hello world\n");
}

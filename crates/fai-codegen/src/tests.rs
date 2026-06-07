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
use indoc::{formatdoc, indoc};

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
    // The entry trampoline forces and applies the standard library's `Runtime`
    // value binding; it is not referenced from `main`, so seed it explicitly.
    let runtime = DefId::new(
        fai_resolve::prelude_module_file(&db).expect("prelude module").source(&db),
        Symbol::intern("defaultRuntime"),
    );

    // Lower only the definitions transitively reachable from `main`.
    let mut defs: Vec<LoweredDef> = Vec::new();
    let mut arity: HashMap<DefId, usize> = HashMap::new();
    let mut seen: HashSet<DefId> = HashSet::new();
    let mut worklist = vec![entry, runtime];
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
    let code = jit_run(&defs, entry, runtime, &namer, &arity_of);
    let out = rt::capture_take();
    (code, out)
}

fn main_printing(expr: &str) -> String {
    formatdoc! {r#"
        module M

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine ({expr})
    "#}
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
    let src = indoc! {r#"
        module M

        let double x = x + x

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (double 21))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn saturated_curried_call() {
    let src = indoc! {r#"
        module M

        let add x y = x + y

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (add 40 2))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn partial_application_via_zero_arity_binding() {
    // `inc = add 1` is a zero-arity value (a partial application); applying it
    // exercises over-application and forcing.
    let src = indoc! {r#"
        module M

        let add x y = x + y

        let inc = add 1

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (inc 41))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn higher_order_with_closure_capture() {
    let src = indoc! {r#"
        module M

        let apply f x = f x

        let adder n = fun m -> n + m

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (apply (adder 40) 2))
    "#};
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
    let src = indoc! {r#"
        module M

        let sign n = if n < 0 then "neg" else if n = 0 then "zero" else "pos"

        public main : Runtime -> Unit
        let main r = r.console.writeLine (sign (0 - 3))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "neg\n");
}

#[test]
fn let_block_in_body() {
    let src = indoc! {r#"
        module M

        let compute n =
          let doubled = n + n
          let plus1 = doubled + 1
          plus1 * 2

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (compute 10))
    "#};
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
    let src = indoc! {r#"
        module M

        type Shape =
          | Circle Int
          | Rect Int Int

        public area : Shape -> Int
        let area s =
          match s with
          | Circle r -> 3 * r * r
          | Rect w h -> w * h

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (area (Rect 3 4)))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0, "clean exit");
    assert_eq!(out, "12\n");
}

#[test]
fn nullary_constructor_match() {
    let src = indoc! {r#"
        module M

        public describe : Option Int -> String
        let describe opt =
          match opt with
          | None -> "none"
          | Some n -> Int.toString n

        public main : Runtime -> Unit
        let main r = r.console.writeLine (describe None)
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "none\n");
}

#[test]
fn list_map_and_fold() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let xs = [1, 2, 3, 4]
          let ys = List.map (fun x -> x * x) xs
          r.console.writeLine (Int.toString (List.foldl (fun acc x -> acc + x) 0 ys))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "30\n");
}

#[test]
fn cons_and_recursive_length() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (List.length (1 :: 2 :: 3 :: [])))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "3\n");
}

#[test]
fn list_pattern_match() {
    let src = indoc! {r#"
        module M

        public firstOr : Int -> List Int -> Int
        let firstOr d xs =
          match xs with
          | [] -> d
          | x :: _ -> x

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (firstOr 0 [7, 8, 9]))
    "#};
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
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let pair = (40, 2)
          let (a, b) = pair
          r.console.writeLine (Int.toString (a + b))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn dict_runs() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let d = Dict.insert 1 10 (Dict.insert 3 30 (Dict.insert 2 20 Dict.empty))
          r.console.writeLine (Int.toString (Option.withDefault 0 (Dict.get 2 d) + Dict.size d))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "23\n");
}

#[test]
fn string_ops_run() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r = r.console.writeLine (String.join "-" (List.map String.toUpper (String.split " " "hi there world")))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "HI-THERE-WORLD\n");
}

#[test]
fn sort_runs() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let xs = List.sort [3, 1, 2]
          r.console.writeLine (Int.toString (List.foldl (fun acc x -> acc * 10 + x) 0 xs))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "123\n");
}

#[test]
fn record_literal_and_field_access() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let p = { x = 1, y = 2 }
          r.console.writeLine (Int.toString (p.x + p.y))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0, "clean exit");
    assert_eq!(out, "3\n");
}

#[test]
fn record_update_runs() {
    let src = indoc! {r#"
        module M

        type P = { a : Int, b : Int }

        public shift : P -> P
        let shift p = { p with a = p.a + 10 }

        public main : Runtime -> Unit
        let main r =
          let q = shift { a = 1, b = 2 }
          r.console.writeLine (Int.toString (q.a + q.b))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "13\n");
}

#[test]
fn record_pattern_and_punning() {
    let src = indoc! {r#"
        module M

        type Point = { x : Int, y : Int }

        public describe : Point -> Int
        let describe pt =
          match pt with
          | { x = 0, y } -> y
          | { x, y } -> x + y

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (describe { x = 0, y = 5 }))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "5\n");
}

#[test]
fn record_destructuring_let() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let p = { a = 40, b = 2 }
          let { a, b } = p
          r.console.writeLine (Int.toString (a + b))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn nested_match_and_or_patterns() {
    let src = indoc! {r#"
        module M

        public classify : Int -> String
        let classify n =
          match n with
          | 0 | 1 -> "small"
          | _ -> "big"

        public main : Runtime -> Unit
        let main r = r.console.writeLine (classify 1)
    "#};
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
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let xs = List.sort [3.0, 1.0, 2.0]
          r.console.writeLine (String.join " " (List.map Float.toString xs))
    "#};
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
    let src = indoc! {r#"
        module M

        type Rank =
          | Low
          | Mid
          | High

        public name : Rank -> String
        let name x =
          match x with
          | Low -> "L"
          | Mid -> "M"
          | High -> "H"

        public main : Runtime -> Unit
        let main r = r.console.writeLine (String.join "" (List.map name (List.sort [High, Low, Mid, Low])))
    "#};
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
    let src = indoc! {r#"
        module M

        type Tree =
          | Leaf
          | Node Tree Int Tree

        public sumTree : Tree -> Int
        let sumTree t =
          match t with
          | Leaf -> 0
          | Node l x rt -> sumTree l + x + sumTree rt

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (sumTree (Node (Node Leaf 1 Leaf) 2 (Node Leaf 3 Leaf))))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "6\n");
}

#[test]
fn result_pattern_match() {
    let src = indoc! {r#"
        module M

        public describe : Result Int String -> String
        let describe res =
          match res with
          | Ok n -> Int.toString n
          | Err e -> e

        public main : Runtime -> Unit
        let main r = r.console.writeLine (describe (Ok 5) ++ describe (Err "boom"))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "5boom\n");
}

#[test]
fn set_dedups_elements() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let s = Set.insert 3 (Set.insert 1 (Set.insert 3 Set.empty))
          r.console.writeLine (Int.toString (Set.size s))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "2\n");
}

#[test]
fn nested_record_field_access() {
    let src = indoc! {r#"
        module M

        type Vec = { x : Int, y : Int }

        type Seg = { dest : Vec, src : Vec }

        public main : Runtime -> Unit
        let main r =
          let s = { src = { x = 1, y = 2 }, dest = { x = 3, y = 4 } }
          r.console.writeLine (Int.toString (s.src.x + s.dest.y))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "5\n");
}

#[test]
fn nested_constructor_patterns() {
    let src = indoc! {r#"
        module M

        public unwrap : Option (Option Int) -> Int
        let unwrap oo =
          match oo with
          | Some (Some n) -> n
          | Some None -> 0
          | None -> 0

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (unwrap (Some (Some 7))))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "7\n");
}

#[test]
fn string_trim_and_lowercase() {
    let (code, out) = run(&main_printing("String.toLower (String.trim \"  Hello WORLD  \")"));
    assert_eq!(code, 0);
    assert_eq!(out, "hello world\n");
}

#[test]
fn random_capability_in_range() {
    // `nextInt 1` is deterministically `0` (the range `[0, 1)`).
    let (code, out) = run(&main_printing("Int.toString (runtime.random.nextInt 1)"));
    assert_eq!(code, 0);
    assert_eq!(out, "0\n");
}

#[test]
fn clock_capability_reads_positive_time() {
    let (code, out) = run(&main_printing("if runtime.clock.now () > 0 then \"ok\" else \"no\""));
    assert_eq!(code, 0);
    assert_eq!(out, "ok\n");
}

#[test]
fn shared_partial_application_is_applied_safely() {
    // A partial application bound to a parameter is dup'd at its use; applying it
    // must respect the refcount (it must not free storage another reference
    // holds).
    let src = indoc! {r#"
        module M

        let add a b = a + b

        let applyIt g = g 10

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (applyIt (add 5)))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "15\n");
}

#[test]
fn row_polymorphic_field_access_runs() {
    // A least-authority signature: `pick` accepts any record with an `a` field.
    let src = indoc! {r#"
        module M

        pick : { a : Int | 'r } -> Int
        let pick rec = rec.a

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (pick { a = 7, b = 9 }))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "7\n");
}

#[test]
fn row_polymorphic_offset_differs_per_call_site() {
    // `c` sits at slot 2 in `{a,b,c}` but slot 1 in `{a,c,z}`: the same function
    // reads it via runtime evidence, not a baked-in slot.
    let src = indoc! {r#"
        module M

        sumAC : { a : Int, c : Int | 'r } -> Int
        let sumAC rec = rec.a + rec.c

        public main : Runtime -> Unit
        let main r =
          r.console.writeLine (Int.toString (sumAC { a = 1, b = 2, c = 3 } + sumAC { a = 10, c = 20, z = 9 }))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "34\n"); // (1+3) + (10+20)
}

#[test]
fn row_polymorphic_evidence_threads_through_calls() {
    // `greet` forwards its record to `emit`; the offset evidence threads through.
    let src = indoc! {r#"
        module M

        emit : { console : Console | 'r } -> String -> Unit
        let emit env msg = env.console.writeLine msg

        greet : { console : Console | 'r } -> Unit
        let greet env = emit env "hi"

        public main : Runtime -> Unit
        let main r = greet r
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "hi\n");
}

#[test]
fn row_polymorphic_function_passed_first_class() {
    // `getA` (row-polymorphic) is passed as a value; its evidence is baked in.
    let src = indoc! {r#"
        module M

        getA : { a : Int | 'r } -> Int
        let getA rec = rec.a

        applyRec : ({ a : Int, b : Int } -> Int) -> Int
        let applyRec f = f { a = 5, b = 7 }

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (applyRec getA))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "5\n");
}

#[test]
fn row_polymorphic_record_update_runs() {
    let src = indoc! {r#"
        module M

        bump : { score : Int | 'r } -> { score : Int | 'r }
        let bump rec = { rec with score = rec.score + 100 }

        public main : Runtime -> Unit
        let main r =
          let bumped = bump { name = "x", score = 5 }
          r.console.writeLine (Int.toString bumped.score)
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "105\n");
}

#[test]
fn file_system_write_then_read_runs() {
    let path = std::env::temp_dir().join("fai-codegen-fs-roundtrip.txt");
    let path = path.to_str().unwrap().replace('\\', "/");
    let src = formatdoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          match r.fs.writeFile "{path}" "round-trip" with
          | Err e -> r.console.writeLine e
          | Ok u ->
            match r.fs.readFile "{path}" with
            | Err e -> r.console.writeLine e
            | Ok c -> r.console.writeLine c
    "#};
    let (code, out) = run(&src);
    assert_eq!(code, 0);
    assert_eq!(out, "round-trip\n");
}

#[test]
fn env_get_unset_variable_runs() {
    // A variable that is certainly unset yields `None`, exercising the `Env`
    // capability's `Option`-wrapping deterministically.
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          match r.env.get "FAI_DEFINITELY_UNSET_PROBE_XYZ" with
          | Some v -> r.console.writeLine v
          | None -> r.console.writeLine "unset"
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "unset\n");
}

#[test]
fn runtime_threaded_through_signatured_helper() {
    // A helper that receives the full `Runtime` can project a capability, given a
    // signature (the receiver's type must be known for method access).
    let src = indoc! {r#"
        module M

        emit : Runtime -> String -> Unit
        let emit runtime msg = runtime.console.writeLine msg

        public main : Runtime -> Unit
        let main runtime = emit runtime "hi"
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "hi\n");
}

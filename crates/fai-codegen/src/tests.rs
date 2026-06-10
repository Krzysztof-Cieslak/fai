//! End-to-end JIT tests: compile small programs and run them, asserting their
//! console output and a clean (leak-free) exit.
//!
//! The leak-free exit and the allocation-count assertions rely on the runtime's
//! counters, which are compiled in only under `debug_assertions` — so these tests
//! are meaningful only in a debug build (the default for `cargo test`).

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

/// The native calling-convention shape of `def` (which parameters/result are
/// unboxed floats), derived from its signature — the test-harness analogue of the
/// driver's `abi_of`. `nparams` is the runtime arity (source + evidence).
fn abi_of_def(db: &FaiDatabase, def: DefId, nparams: usize) -> fai_core::ir::FnAbi {
    match fai_types::declared_or_inferred_scheme(db, def) {
        Some(scheme) => {
            let source = nparams.saturating_sub(fai_types::evidence_count(&scheme));
            fai_core::ir::FnAbi::from_scheme(&scheme, source)
        }
        None => fai_core::ir::FnAbi::default(),
    }
}

/// Lowers the definitions reachable from the entry file's `main` and runs it
/// through the JIT, returning `(exit_code, captured_output)`.
///
/// Only the *reachable* closure is compiled: starting at `main`, we follow each
/// definition's referenced globals transitively. The prelude is a large module,
/// so compiling all of it on every call would dominate the test cost even though
/// most programs touch only a handful of its functions.
pub(crate) fn run(src: &str) -> (i32, String) {
    let (code, out, _allocs) = run_counted(src);
    (code, out)
}

/// As [`run`], but also returns the number of heap allocations performed during
/// execution (for reuse measurement).
pub(crate) fn run_counted(src: &str) -> (i32, String, i64) {
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
    let mut abi: HashMap<DefId, fai_core::ir::FnAbi> = HashMap::new();
    let mut seen: HashSet<DefId> = HashSet::new();
    let mut worklist = vec![entry, runtime];
    while let Some(def) = worklist.pop() {
        if !seen.insert(def) {
            continue;
        }
        let Some(&file) = files.get(&def.file) else { continue };
        let lowered = rc(&db, file, def.name);
        let nparams = lowered.entry().params.len();
        arity.insert(def, nparams);
        abi.insert(def, abi_of_def(&db, def, nparams));
        worklist.extend(lowered.referenced_globals());
        defs.push((*lowered).clone());
    }

    let namer =
        |d: DefId| format!("fai_{}_{}", labels.get(&d.file).cloned().unwrap_or_default(), d.name);
    let arity_of = |d: DefId| arity.get(&d).copied().unwrap_or(1);
    let signature_of = |d: DefId| abi.get(&d).cloned().unwrap_or_default();

    let _g = lock();
    rt::capture_start();
    rt::reset_allocations();
    let code = jit_run(&defs, entry, runtime, &namer, &arity_of, &signature_of);
    let allocs = rt::allocations();
    let out = rt::capture_take();
    (code, out, allocs)
}

/// Lowers `def_name` from `src` (plus, for direct-call arity, the globals it
/// references) and returns the Cranelift IR text of its compiled functions
/// (entry first). For inspecting the emitted code — e.g. that a known data
/// cell's drop is inlined (a reference-count branch, hence a `brif`) rather than
/// dispatched to the runtime (a plain `fai_drop` call, no branch).
fn function_ir(src: &str, def_name: &str) -> Vec<String> {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let user = db.source_file(id).unwrap();

    let mut files: HashMap<SourceId, SourceFile> = HashMap::new();
    let mut labels: HashMap<SourceId, String> = HashMap::new();
    for file in db.all_source_files() {
        let label =
            module_name(&db, file).map_or_else(|| "M".to_owned(), |m| m.0.as_str().to_owned());
        files.insert(file.source(&db), file);
        labels.insert(file.source(&db), label);
    }

    let target = DefId::new(user.source(&db), Symbol::intern(def_name));
    let mut arity: HashMap<DefId, usize> = HashMap::new();
    let mut abi: HashMap<DefId, fai_core::ir::FnAbi> = HashMap::new();
    let mut seen: HashSet<DefId> = HashSet::new();
    let mut worklist = vec![target];
    let mut lowered = None;
    while let Some(def) = worklist.pop() {
        if !seen.insert(def) {
            continue;
        }
        let Some(&file) = files.get(&def.file) else { continue };
        let l = rc(&db, file, def.name);
        let nparams = l.entry().params.len();
        arity.insert(def, nparams);
        abi.insert(def, abi_of_def(&db, def, nparams));
        worklist.extend(l.referenced_globals());
        if def == target {
            lowered = Some((*l).clone());
        }
    }
    let lowered = lowered.expect("target definition lowered");

    let namer =
        |d: DefId| format!("fai_{}_{}", labels.get(&d.file).cloned().unwrap_or_default(), d.name);
    let arity_of = |d: DefId| arity.get(&d).copied().unwrap_or(1);
    let signature_of = |d: DefId| abi.get(&d).cloned().unwrap_or_default();
    crate::aot::function_ir_text(&lowered, &namer, &arity_of, &signature_of)
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
fn three_argument_direct_call() {
    // Several register arguments at a saturated direct call.
    let src = indoc! {r#"
        module M

        let add3 a b c = a + b + c

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (add3 10 20 12))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "42\n");
}

#[test]
fn float_direct_call_uses_registers_and_is_leak_free() {
    // A non-row-poly float function passed/returns scalar `Float` in f64 registers.
    // `powf`'s self-call is non-tail (an operand of `*.`), so it is a genuine
    // direct call exercising the register float ABI on both the argument and the
    // result; `main` direct-calls it. A clean exit also asserts no leak.
    let src = indoc! {r#"
        module M

        powf : Float -> Int -> Float
        let powf x n = if n <= 0 then 1.0 else x * powf x (n - 1)

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Float.toString (powf 2.0 10))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0, "clean exit (no leak)");
    assert_eq!(out, "1024.0\n");
}

#[test]
fn function_used_both_directly_and_first_class() {
    // The same definition is reached two ways: a saturated direct call (register
    // ABI) and as a first-class value applied through `apply_n` (the bridging
    // wrapper). Both must agree and stay leak-free.
    let src = indoc! {r#"
        module M

        let inc x = x + 1

        let apply f x = f x

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (inc (apply inc 40)))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0, "clean exit (no leak)");
    assert_eq!(out, "42\n");
}

#[test]
fn aliased_function_is_a_direct_call() {
    // `let g = add` aliases a top-level function; `g 40 2` is copy-propagated to a
    // direct call to `add`. Correct value and a leak-free exit.
    let src = indoc! {r#"
        module M

        let add x y = x + y

        public main : Runtime -> Unit
        let main runtime =
          let g = add
          runtime.console.writeLine (Int.toString (g 40 2))
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "42\n"));
}

#[test]
fn aliased_function_used_directly_and_as_a_value() {
    // The alias is reached both as a direct call (`g 41`) and as a first-class
    // value (`apply g 40` passes its closure); both resolve to `inc`.
    let src = indoc! {r#"
        module M

        let inc x = x + 1

        let apply f x = f x

        public main : Runtime -> Unit
        let main runtime =
          let g = inc
          runtime.console.writeLine (Int.toString (g (apply g 40)))
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "42\n"));
}

#[test]
fn over_application_of_a_closure_returning_function() {
    // `constAdd 40 2` over-applies `constAdd` (arity 1, returns a closure): the
    // saturated prefix `constAdd 40` is a direct call, and the surplus `2` is
    // applied to its result through `apply_n`.
    let src = indoc! {r#"
        module M

        let constAdd x = fun y -> x + y

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (constAdd 40 2))
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "42\n"));
}

#[test]
fn over_application_of_a_borrowing_function_is_leak_free() {
    // The RC-critical case: `chooseByLen` *borrows* its list (it forwards it to the
    // borrowing `len` and returns a top-level function, capturing nothing), so
    // over-applying it lends the list for the saturated prefix. The owner (`main`'s
    // `nums`) must drop it after the call — a clean exit asserts the borrow lending
    // at the widened over-application boundary balances (no leak, no double free).
    let src = indoc! {r#"
        module M

        let add1 x = x + 1

        let add10 x = x + 10

        let len xs =
          match xs with
          | [] -> 0
          | _ :: r -> 1 + len r

        let chooseByLen xs = if len xs > 3 then add10 else add1

        public main : Runtime -> Unit
        let main runtime =
          let nums = [1, 2, 3, 4, 5]
          runtime.console.writeLine (Int.toString (chooseByLen nums 5))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0, "clean exit (no leak)");
    assert_eq!(out, "15\n");
}

#[test]
fn aliased_row_polymorphic_function_runs() {
    // A row-polymorphic function aliased by a `let` is *not* copy-propagated to a
    // direct call (it lowers to a partial application, kept on the `apply_n` path);
    // it must still run correctly.
    let src = indoc! {r#"
        module M

        let getX r = r.x

        public main : Runtime -> Unit
        let main runtime =
          let g = getX
          runtime.console.writeLine (Int.toString (g { x = 5, y = 9 }))
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "5\n"));
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

/// Unboxed monomorphic floats: a tail-recursive float-accumulator loop allocates
/// a constant number of heap cells regardless of its iteration count. A
/// regression that re-boxed per-operation floats would make the count scale with
/// the iterations and fail this gate.
#[test]
fn unboxed_float_loop_allocates_independently_of_iterations() {
    let program = |n: i64| {
        formatdoc! {r#"
            module M

            sumFrom : Float -> Int -> Int -> Float
            let sumFrom acc i n =
              if i >= n then acc else sumFrom (acc + Int.toFloat i) (i + 1) n

            public main : Runtime -> Unit
            let main runtime = runtime.console.writeLine (Float.toString (sumFrom 0.0 0 {n}))
        "#}
    };
    let (code, _out, few) = run_counted(&program(10));
    assert_eq!(code, 0, "clean exit");
    let (_, _, many) = run_counted(&program(100_000));
    assert_eq!(
        few, many,
        "an unboxed float loop must allocate a constant number of cells (got {few} vs {many})"
    );
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
fn char_equality() {
    let (code, out) = run(&main_printing("if 'a' = 'a' then \"eq\" else \"ne\""));
    assert_eq!(code, 0);
    assert_eq!(out, "eq\n");
}

#[test]
fn char_ordering() {
    let (code, out) = run(&main_printing("if 'a' < 'b' then \"lt\" else \"ge\""));
    assert_eq!(code, 0);
    assert_eq!(out, "lt\n");
}

#[test]
fn char_pattern_match() {
    let src = indoc! {r#"
        module M

        let classify c =
          match c with
          | 'a' -> "first"
          | 'z' -> "last"
          | _ -> "other"

        public main : Runtime -> Unit
        let main r = r.console.writeLine (classify 'z')
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "last\n");
}

#[test]
fn char_unicode_escape_literal() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r = r.console.writeLine (if '\u{1F600}' = '\u{1F600}' then "eq" else "ne")
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "eq\n");
}

#[test]
fn char_to_string_ascii() {
    let (code, out) = run(&main_printing("Char.toString 'A'"));
    assert_eq!(code, 0);
    assert_eq!(out, "A\n");
}

#[test]
fn char_to_string_multibyte() {
    let (code, out) = run(&main_printing("Char.toString '\\u{1F600}'"));
    assert_eq!(code, 0);
    assert_eq!(out, "\u{1F600}\n");
}

#[test]
fn char_to_code_renders_int() {
    let (code, out) = run(&main_printing("Int.toString (Char.toCode 'A')"));
    assert_eq!(code, 0);
    assert_eq!(out, "65\n");
}

#[test]
fn char_from_code_valid_round_trips() {
    let (code, out) =
        run(&main_printing("Char.toString (Option.withDefault 'z' (Char.fromCode 66))"));
    assert_eq!(code, 0);
    assert_eq!(out, "B\n");
}

#[test]
fn char_from_code_surrogate_is_none() {
    // 0xD800 is a surrogate, so `fromCode` is `None` and the default is used.
    let (code, out) =
        run(&main_printing("Char.toString (Option.withDefault 'z' (Char.fromCode 55296))"));
    assert_eq!(code, 0);
    assert_eq!(out, "z\n");
}

#[test]
fn chars_sort_by_code_point() {
    let expr = "String.join \"\" (List.map Char.toString (List.sort ['c', 'a', 'b']))";
    let (code, out) = run(&main_printing(expr));
    assert_eq!(code, 0);
    assert_eq!(out, "abc\n");
}

#[test]
fn char_as_dict_key() {
    // A Char key exercises the BST's structural comparison at runtime.
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let d = Dict.insert 'b' "two" (Dict.insert 'a' "one" Dict.empty)
          r.console.writeLine (Option.withDefault "?" (Dict.get 'b' d))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "two\n");
}

#[test]
fn char_tuple_destructuring_runs() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let (a, b) = ('x', 'y')
          r.console.writeLine (Char.toString a ++ Char.toString b)
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "xy\n");
}

#[test]
fn multi_arm_char_match_with_escape_runs() {
    let src = indoc! {r#"
        module M

        let name c =
          match c with
          | 'a' -> "alpha"
          | '\n' -> "newline"
          | ' ' -> "space"
          | _ -> "other"

        public main : Runtime -> Unit
        let main r =
          r.console.writeLine (name '\n' ++ "," ++ name ' ' ++ "," ++ name 'a' ++ "," ++ name 'q')
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "newline,space,alpha,other\n");
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
fn float_list_fold_returns_correct_value() {
    // A self-tail-recursive fold with a `Float` accumulator reading `Float`
    // elements of a list compiles to a loop whose result is an unboxed `f64`.
    // Regression: the loop's exit representation was taken from the loop node's
    // static type, which a desugared `match` records as `Error`, so the `f64`
    // result was mistaken for a boxed word and unboxed (a wild dereference).
    let src = indoc! {r#"
        module M

        sumF : Float -> List Float -> Float
        let sumF acc xs =
          match xs with
          | [] -> acc
          | x :: rest -> sumF (acc + x) rest

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Float.toString (sumF 0.0 [1.0, 2.0, 3.0]))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "6.0\n");
}

#[test]
fn mapped_float_list_fold_returns_correct_value() {
    // The same float-accumulator loop over a list produced by `List.map` of a
    // `Float`-returning function (the original symptom: building and folding a
    // `List Float`).
    let src = indoc! {r#"
        module M

        sumF : Float -> List Float -> Float
        let sumF acc xs =
          match xs with
          | [] -> acc
          | x :: rest -> sumF (acc + x) rest

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Float.toString (sumF 0.0 (List.map Int.toFloat (List.range 1 4))))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "6.0\n");
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
fn row_polymorphic_access_of_last_sorting_field() {
    // `z` sorts after `a`/`b`, so its slot is the maximum — a non-zero evidence.
    let src = indoc! {r#"
        module M

        get : { z : Int | 'r } -> Int
        let get rec = rec.z

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (get { a = 1, b = 2, z = 99 }))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "99\n");
}

#[test]
fn row_polymorphic_outer_with_monomorphic_inner_record() {
    // `rec.p` is a row-polymorphic projection (evidence); the inner `{ x, y }` is
    // closed, so `.x`/`.y` are constant offsets.
    let src = indoc! {r#"
        module M

        getInner : { p : { x : Int, y : Int } | 'r } -> Int
        let getInner rec = rec.p.x + rec.p.y

        public main : Runtime -> Unit
        let main r =
          r.console.writeLine (Int.toString (getInner { tag = 0, p = { x = 3, y = 4 } }))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "7\n");
}

#[test]
fn interface_value_in_a_record_field_dispatches() {
    let src = indoc! {r#"
        module M

        interface Greeter =
          greet : String -> String

        public main : Runtime -> Unit
        let main r =
          let g = { Greeter with greet n = n ++ "!" }
          let rec = { count = 1, greeter = g }
          r.console.writeLine (rec.greeter.greet "hi")
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "hi!\n");
}

#[test]
fn row_polymorphic_update_preserves_the_other_fields() {
    let src = indoc! {r#"
        module M

        bump : { n : Int | 'r } -> { n : Int | 'r }
        let bump rec = { rec with n = rec.n + 1 }

        public main : Runtime -> Unit
        let main r =
          let rec = { a = 1, b = 2, c = 3, n = 10, z = 99 }
          let rec2 = bump rec
          r.console.writeLine (Int.toString (rec2.a + rec2.b + rec2.c + rec2.n + rec2.z))
    "#};
    let (code, out) = run(src);
    assert_eq!(code, 0);
    assert_eq!(out, "116\n"); // 1 + 2 + 3 + 11 + 99
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

// --- Reuse (in-place recycling of a unique list) ---------------------------

/// Shared `build`/`inc`/`sum` definitions plus a `use : List Int -> Int`
/// consumer, for measuring the allocations a `map`-like rebuild performs over a
/// unique vs a shared list. `use_body` references its parameter `xs`.
fn reuse_program(use_body: &str) -> String {
    formatdoc! {r#"
        module M

        let build n = if n <= 0 then [] else n :: build (n - 1)

        let inc xs =
          match xs with
          | [] -> []
          | x :: rest -> (x + 1) :: inc rest

        let sum xs =
          match xs with
          | [] -> 0
          | x :: rest -> x + sum rest

        let use xs = {use_body}

        public main : Runtime -> Unit
        let main rt = rt.console.writeLine (Int.toString (use (build 50)))
    "#}
}

#[test]
fn record_update_is_in_place_for_a_unique_record() {
    // `bumpN` rebuilds the (closed-typed, so allocation-free to thread) record `k`
    // times; each `{ rec with … }` owns its record uniquely, so it overwrites the
    // field in place — the allocation count is independent of `k`.
    let prog = |k: i32| {
        formatdoc! {r#"
            module M

            type R = {{ a : Int, n : Int }}

            bumpN : Int -> R -> R
            let bumpN k rec =
              if k <= 0 then rec else bumpN (k - 1) {{ rec with n = rec.n + 1 }}

            getN : R -> Int
            let getN rec = rec.n

            public main : Runtime -> Unit
            let main rt = rt.console.writeLine (Int.toString (getN (bumpN {k} {{ a = 0, n = 0 }})))
        "#}
    };
    let (code_a, out_a, allocs_a) = run_counted(&prog(50));
    let (code_b, out_b, allocs_b) = run_counted(&prog(100));
    assert_eq!((code_a, out_a.trim()), (0, "50"));
    assert_eq!((code_b, out_b.trim()), (0, "100"));
    assert_eq!(
        allocs_a, allocs_b,
        "in-place update allocates the same regardless of update count \
         (50→{allocs_a}, 100→{allocs_b})"
    );
}

#[test]
fn record_update_copies_a_shared_record() {
    // When the record is read again after the update, it is shared, so the update
    // must copy it — one extra allocation versus the unique case.
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
    // Unique: `rec` flows only into `bump`, updated in place.
    let (code_u, out_u, allocs_u) = run_counted(&prog("getN (bump rec)"));
    assert_eq!((code_u, out_u.trim()), (0, "11"));
    // Shared: `rec` is also read directly, so `bump` copies it.
    let (code_s, out_s, allocs_s) = run_counted(&prog("getN (bump rec) + getN rec"));
    assert_eq!((code_s, out_s.trim()), (0, "21")); // 11 + 10
    assert_eq!(
        allocs_s - allocs_u,
        1,
        "the shared update copies the record once (unique={allocs_u}, shared={allocs_s})"
    );
}

#[test]
fn reuse_recycles_a_unique_list_but_copies_a_shared_one() {
    // Unique: the list flows straight into `inc`, which recycles each cons cell
    // in place — no fresh cons cells are allocated by `inc`.
    let unique = reuse_program("sum (inc xs)");
    let (code_u, out_u, allocs_u) = run_counted(&unique);
    assert_eq!(code_u, 0, "unique program exits cleanly (no leak)");
    assert_eq!(out_u.trim(), "1325"); // sum (2..=51)

    // Shared: `xs` is read again by `sum xs`, so it is not unique when `inc`
    // runs; `inc` must allocate fresh cons cells (the rc==1 guard falls back to a
    // copy), one per element.
    let shared = reuse_program("sum (inc xs) + sum xs");
    let (code_s, out_s, allocs_s) = run_counted(&shared);
    assert_eq!(code_s, 0, "shared program exits cleanly (no leak)");
    assert_eq!(out_s.trim(), "2600"); // 1325 + sum (1..=50)

    // Everything else (building the list, the runtime, the result string) is the
    // same; the difference is exactly the 50 cons cells `inc` had to allocate in
    // the shared case but recycled in place in the unique case.
    assert_eq!(
        allocs_s - allocs_u,
        50,
        "shared map allocates 50 cons cells the unique map recycles \
         (unique={allocs_u}, shared={allocs_s})"
    );
}

// --- Drop specialization: inlined drops of monomorphic data cells ----------

#[test]
fn drop_of_a_monomorphic_record_is_inlined() {
    // `p` is a closed-record let-local dropped (unused) at its last point; its
    // release is inlined — a reference-count decrement and a branch on zero — so
    // the function carries a `brif`. The body has no `if`, so the only possible
    // source of a branch is the specialized drop.
    let src = indoc! {r#"
        module M

        type R = { a : Int, b : Int }

        mk : Int -> R
        let mk n = { a = n, b = n }

        f : Int -> Int
        let f n =
          let p = mk n
          n
    "#};
    let ir = function_ir(src, "f").join("\n");
    assert!(ir.contains("brif"), "the inlined record drop branches on the refcount:\n{ir}");
}

#[test]
fn drop_of_a_tuple_is_inlined() {
    let src = indoc! {r#"
        module M

        pair : Int -> Int * Int
        let pair n = (n, n)

        f : Int -> Int
        let f n =
          let p = pair n
          n
    "#};
    let ir = function_ir(src, "f").join("\n");
    assert!(ir.contains("brif"), "the inlined tuple drop branches on the refcount:\n{ir}");
}

#[test]
fn drop_of_a_list_is_inlined_with_a_runtime_dead_path() {
    // A `List` is a known data type, so its drop is inlined: a tag-check and an
    // in-place reference-count decrement (each a `brif`), releasing the cell's
    // children through the runtime (`fai_drop_dead`) only on the dead path. The
    // body has no `if`, so any `brif` comes from the inlined drop.
    let src = indoc! {r#"
        module M

        g : Int -> Int
        let g n =
          let xs = [n]
          n
    "#};
    let ir = function_ir(src, "g").join("\n");
    assert!(ir.contains("brif"), "the inlined list drop branches on the tag and refcount:\n{ir}");
}

#[test]
fn drop_of_a_string_leaf_is_inlined() {
    // A `String` is a boxed leaf (no reference-counted children), so its drop is
    // inlined: an in-place decrement and a free on the dead path (a `brif`), no
    // descriptor scan. The body has no `if`, so any `brif` is the inlined drop.
    let src = indoc! {r#"
        module M

        f : Int -> Int
        let f n =
          let s = Int.toString n
          n
    "#};
    let ir = function_ir(src, "f").join("\n");
    assert!(ir.contains("brif"), "the inlined leaf drop branches on the refcount:\n{ir}");
}

#[test]
fn dup_of_an_always_boxed_value_omits_the_tag_check() {
    // `s` is used twice, so it is duplicated; a `String` is always boxed, so the
    // increment is unconditional — no tag-check branch. The body has no `if` and
    // builds a tuple (a runtime call, no branch), so the absence of any `brif`
    // confirms the guard was elided.
    let src = indoc! {r#"
        module M

        g : String -> String * String
        let g s = (s, s)
    "#};
    let ir = function_ir(src, "g").join("\n");
    assert!(!ir.contains("brif"), "an always-boxed dup needs no tag-check branch:\n{ir}");
}

#[test]
fn dup_of_an_int_is_tag_checked() {
    // `n` is used twice, so it is duplicated; an `Int` may be an immediate, so the
    // increment is guarded by a tag-check (`brif`). The body has no `if`, so that
    // branch is the inlined dup's guard.
    let src = indoc! {r#"
        module M

        g : Int -> Int * Int
        let g n = (n, n)
    "#};
    let ir = function_ir(src, "g").join("\n");
    assert!(ir.contains("brif"), "an Int dup is guarded by a tag-check:\n{ir}");
}

// --- Drop specialization: behavioral leak/correctness matrix ----------------
// Each program drops a monomorphic data cell through the inlined path; a clean
// (code 0) exit is the runtime's end-of-run leak check, so it proves the cell —
// and its reference-counted children — were released exactly once.

#[test]
fn inlined_drop_frees_a_records_boxed_child() {
    // The record owns a `String`; dropping the record must free it (no leak).
    let src = indoc! {r#"
        module M

        type R = { name : String, n : Int }

        make : String -> R
        let make s = { name = s, n = 5 }

        public main : Runtime -> Unit
        let main r =
          let rec = make "hello"
          r.console.writeLine (Int.toString rec.n)
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "5\n"));
}

#[test]
fn inlined_drop_of_a_tuple_with_a_boxed_element() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let t = ("hi", 5)
          let (s, n) = t
          r.console.writeLine (s ++ Int.toString n)
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "hi5\n"));
}

#[test]
fn inlined_drop_of_an_all_immediate_record() {
    // No boxed fields: the inlined drop is a bare decrement-and-free, no child
    // drops at all.
    let src = indoc! {r#"
        module M

        type Flags = { a : Bool, b : Bool }

        mkFlags : Bool -> Flags
        let mkFlags x = { a = x, b = x }

        public main : Runtime -> Unit
        let main r =
          let f = mkFlags true
          r.console.writeLine (if f.a then "yes" else "no")
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "yes\n"));
}

#[test]
fn inlined_drop_of_a_nested_record_releases_the_inner_cell() {
    // The outer drop is inlined; the inner record field is released through the
    // runtime drop (no inline recursion), which in turn frees its String.
    let src = indoc! {r#"
        module M

        type Inner = { s : String }
        type Outer = { inner : Inner, k : Int }

        public main : Runtime -> Unit
        let main r =
          let o = { inner = { s = "deep" }, k = 1 }
          r.console.writeLine o.inner.s
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "deep\n"));
}

#[test]
fn inlined_drop_in_tail_position_of_a_loop() {
    // A tail-recursive loop builds and discards a record each iteration; the drop
    // sits in tail position (before the back-edge), exercising the tail emitter.
    let src = indoc! {r#"
        module M

        type R = { a : Int, b : Int }

        sumR : Int -> Int -> Int
        let sumR n acc =
          if n <= 0 then acc
          else
            let p = { a = n, b = n }
            sumR (n - 1) (acc + p.a)

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (sumR 5 0))
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "15\n"));
}

#[test]
fn inlined_drop_of_a_shared_record_decrements_without_freeing() {
    // `p` is aliased, so it is shared (rc > 1) when the first drop runs: that
    // inlined drop must only decrement; the last reference's drop frees it.
    let src = indoc! {r#"
        module M

        type R = { a : Int, b : Int }

        public main : Runtime -> Unit
        let main r =
          let p = { a = 10, b = 20 }
          let q = p
          r.console.writeLine (Int.toString (p.a + q.b))
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "30\n"));
}

// --- Inlined drop of variable-shape data (List/ADT) and boxed leaves ---------
// Each program discards a value through the inlined data/leaf drop path; a clean
// (code 0) exit is the runtime's end-of-run leak check, proving the cell — and
// its reference-counted children — were released exactly once.

#[test]
fn inlined_drop_of_a_list_frees_cells_and_elements() {
    // The list (boxed cons cells) owns boxed `String` elements; dropping it must
    // free every cell and every element.
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let xs = ["a", "b", "c"]
          r.console.writeLine "ok"
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "ok\n"));
}

#[test]
fn inlined_drop_of_an_adt_value() {
    // A boxed `Some` cell owning a boxed child, dropped unused.
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let x = Some "wrapped"
          r.console.writeLine "ok"
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "ok\n"));
}

#[test]
fn inlined_drop_of_a_nullary_constructor_is_a_no_op() {
    // `None` is an immediate (a nullary constructor), so the tag-checked data drop
    // must take the immediate branch and do nothing — no spurious free.
    let src = indoc! {r#"
        module M

        none : Option String
        let none = None

        public main : Runtime -> Unit
        let main r =
          let x = none
          r.console.writeLine "ok"
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "ok\n"));
}

#[test]
fn unused_float_local_is_unboxed_and_leak_free() {
    // An unused scalar `Float` local is an unboxed `f64` (no allocation, no
    // reference count): it exits cleanly and adds zero allocations over a baseline
    // without it.
    let with_float = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let x = 1.5
          r.console.writeLine "ok"
    "#};
    let baseline = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          r.console.writeLine "ok"
    "#};
    let (code, out, allocs) = run_counted(with_float);
    assert_eq!((code, out.as_str()), (0, "ok\n"));
    let (_, _, base_allocs) = run_counted(baseline);
    assert_eq!(allocs, base_allocs, "an unboxed float local allocates nothing");
}

#[test]
fn first_class_float_param_function_is_leak_free() {
    // A float-parameter function that only inspects its argument is borrow-eligible
    // AND used first-class (applied via `apply_n` through its closure wrapper). The
    // wrapper must unbox the boxed float argument and release its box exactly once
    // (float-slot handling supersedes the borrow drop) — no leak, no double free.
    let src = indoc! {r#"
        module M

        isPositive : Float -> Bool
        let isPositive x = x > 0.0

        public main : Runtime -> Unit
        let main runtime =
          let check = isPositive
          runtime.console.writeLine (if check 1.5 then "yes" else "no")
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "yes\n"));
}

#[test]
fn inlined_drop_of_a_string_leaf() {
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let s = Int.toString 42
          r.console.writeLine "ok"
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "ok\n"));
}

#[test]
fn inlined_drop_of_a_recursive_adt_parameter() {
    // A binary-trees-shaped traversal: `count` drops its `Tree` *parameter* at the
    // match's last use. Parameter types reach codegen through the `var_tys`
    // pre-pass, so the node drop is the inlined data path (not a runtime fallback);
    // a clean exit proves every allocated node was freed.
    let src = indoc! {r#"
        module M

        type Tree =
          | Leaf
          | Node Tree Tree

        build : Int -> Tree
        let build n =
          if n <= 0 then Leaf else Node (build (n - 1)) (build (n - 1))

        count : Tree -> Int
        let count t =
          match t with
          | Leaf -> 0
          | Node l r -> 1 + count l + count r

        public main : Runtime -> Unit
        let main r = r.console.writeLine (Int.toString (count (build 5)))
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "31\n"));
}

#[test]
fn inlined_drop_of_a_shared_list_does_not_double_free() {
    // The list is aliased, so it is shared when the first drop runs: the inlined
    // data drop must only decrement; the last reference frees it (and its cells).
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let xs = ["x", "y"]
          let ys = xs
          r.console.writeLine (Int.toString (List.length xs + List.length ys))
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "4\n"));
}

#[test]
fn inlined_drop_of_a_deep_list_is_stack_safe() {
    // A long list dropped at once: the inlined decrement reaches zero and hands
    // the dead cell to `fai_drop_dead`, which drains the spine iteratively — so a
    // structure far deeper than the native stack still releases without overflow.
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit
        let main r =
          let xs = List.range 0 200000
          r.console.writeLine "ok"
    "#};
    let (code, out) = run(src);
    assert_eq!((code, out.as_str()), (0, "ok\n"));
}

// --- Inline integer primitives: emitted IR shape ----------------------------
// The hot integer/boolean primitives compile to inline machine code with an
// immediate fast path and a runtime-call fallback. `function_ir` shows the
// pre-optimization IR we emit. In that form a runtime callee is a numbered
// external reference (`fn0`), not its symbol name, so these assert the structural
// shape: the inline machine op, the immediate guard branch (`brif`), and the lone
// fallback `call` (the fast path makes no call).

#[test]
fn integer_add_is_inlined_with_a_runtime_fallback() {
    let src = indoc! {r#"
        module M

        f : Int -> Int -> Int
        let f x y = x + y
    "#};
    let ir = function_ir(src, "f").join("\n");
    assert!(ir.contains("iadd"), "inline native add:\n{ir}");
    assert!(ir.contains("sadd_overflow"), "inline 63-bit fit check:\n{ir}");
    assert!(ir.contains("brif"), "immediate-operand guard branch:\n{ir}");
    assert!(ir.contains("call fn0"), "runtime fallback call retained:\n{ir}");
}

#[test]
fn integer_comparison_is_inlined_with_a_runtime_fallback() {
    let src = indoc! {r#"
        module M

        g : Int -> Int -> Bool
        let g x y = x < y
    "#};
    let ir = function_ir(src, "g").join("\n");
    assert!(ir.contains("icmp"), "inline native comparison:\n{ir}");
    assert!(ir.contains("brif"), "immediate-operand guard branch:\n{ir}");
    assert!(ir.contains("call fn0"), "runtime fallback call retained:\n{ir}");
}

#[test]
fn integer_equality_is_inlined_with_a_runtime_fallback() {
    let src = indoc! {r#"
        module M

        e : Int -> Int -> Bool
        let e x y = x = y
    "#};
    let ir = function_ir(src, "e").join("\n");
    assert!(ir.contains("icmp"), "inline native equality:\n{ir}");
    assert!(ir.contains("brif"), "immediate-operand guard branch:\n{ir}");
    assert!(ir.contains("call fn0"), "runtime fallback call retained:\n{ir}");
}

#[test]
fn char_equality_is_inlined_without_a_guard_or_fallback() {
    // Char is unconditionally immediate, so equality is a bare `icmp eq` with no
    // guard branch and no runtime call. The body has no `if`, so the absence of
    // any `brif` confirms the bare inline path.
    let src = indoc! {r#"
        module M

        ceq : Char -> Char -> Bool
        let ceq a b = a = b
    "#};
    let ir = function_ir(src, "ceq").join("\n");
    assert!(ir.contains("icmp"), "inline native equality:\n{ir}");
    assert!(!ir.contains("brif"), "no guard for an always-immediate type:\n{ir}");
    // Match the call *instruction* (`call fn0`), not a bare "call": the Windows
    // calling-convention name (`windows_fastcall`) in the function signature
    // contains the substring "call".
    assert!(!ir.contains("call fn0"), "no runtime fallback for an always-immediate type:\n{ir}");
}

#[test]
fn integer_division_stays_an_out_of_line_call() {
    // Division guards against zero in the runtime, so it is not inlined: a plain
    // call, with no immediate guard branch and no inline fit check.
    let src = indoc! {r#"
        module M

        d : Int -> Int -> Int
        let d x y = x / y
    "#};
    let ir = function_ir(src, "d").join("\n");
    assert!(ir.contains("call fn0"), "division is a runtime call:\n{ir}");
    assert!(!ir.contains("brif"), "no immediate guard for the non-inlined call:\n{ir}");
    assert!(!ir.contains("sadd_overflow"), "no inline fit check for division:\n{ir}");
}

// --- Register calling convention: emitted IR shape --------------------------
// A saturated direct call to a known top-level function passes its arguments in
// registers (the call's operands are the values themselves), skipping the
// argument-array spill that the uniform `apply_n` path still uses. `function_ir`
// shows the pre-optimization IR, where `stack_store` is the spill instruction.

#[test]
fn direct_call_passes_arguments_in_registers_without_a_spill() {
    // `run a b = add a b` is a saturated direct call to `add`: the value arguments
    // are passed in registers, so no argument array is spilled (`stack_store`).
    let src = indoc! {r#"
        module M

        let add x y = x + y

        let run a b = add a b
    "#};
    let ir = function_ir(src, "run").join("\n");
    assert!(ir.contains("call fn0"), "the callee is called directly:\n{ir}");
    assert!(!ir.contains("stack_store"), "a direct call spills no argument array:\n{ir}");
}

#[test]
fn first_class_application_still_spills_an_argument_array() {
    // An application of a function-typed parameter routes through `fai_apply_n`,
    // which marshals a uniform argument array — so the spill (`stack_store`) is
    // retained for the first-class path, the counterpart to the direct path above.
    let src = indoc! {r#"
        module M

        let run f x = f x x
    "#};
    let ir = function_ir(src, "run").join("\n");
    assert!(ir.contains("stack_store"), "a first-class application spills its args:\n{ir}");
}

// --- Inline integer primitives: immediate/boxed boundary behavior -----------
// One case each across the 63-bit immediate boundary, exercising the fast path,
// the overflow fallback, and the boxed-operand fallback. A clean (code 0) exit
// also asserts no leak.

/// Runs `main` printing `Int.toString (expr)` and returns `(exit, output)`.
fn int_out(expr: &str) -> (i32, String) {
    run(&main_printing(&format!("Int.toString ({expr})")))
}

#[test]
fn add_at_the_max_immediate_stays_immediate() {
    // 2^62 - 1 is the largest immediate; adding 0 stays in range (fast path).
    let (code, out) = int_out("4611686018427387903 + 0");
    assert_eq!((code, out.as_str()), (0, "4611686018427387903\n"));
}

#[test]
fn add_overflowing_the_immediate_boxes_via_the_fallback() {
    // 2^62 - 1 + 1 = 2^62 no longer fits the immediate: the fast path's fit check
    // fails and the runtime fallback boxes the result.
    let (code, out) = int_out("4611686018427387903 + 1");
    assert_eq!((code, out.as_str()), (0, "4611686018427387904\n"));
}

#[test]
fn add_of_two_maxima_boxes_without_i64_overflow() {
    // 2^62 - 1 doubled is 2^63 - 2: fits i64 but not the 63-bit immediate, so the
    // fit check still routes it to the boxing fallback.
    let (code, out) = int_out("4611686018427387903 + 4611686018427387903");
    assert_eq!((code, out.as_str()), (0, "9223372036854775806\n"));
}

#[test]
fn subtraction_reaches_the_min_immediate_on_the_fast_path() {
    // -(2^62 - 1) - 1 = -2^62, the smallest immediate, all on the fast path.
    let (code, out) = int_out("(0 - 4611686018427387903) - 1");
    assert_eq!((code, out.as_str()), (0, "-4611686018427387904\n"));
}

#[test]
fn multiplication_wraps_like_the_runtime() {
    // Both operands are immediates, but the product overflows i64; the wrapped
    // result must match the runtime's `wrapping_mul` (then box).
    let expected = 3_037_000_500_i64.wrapping_mul(3_037_000_500);
    let (code, out) = int_out("3037000500 * 3037000500");
    assert_eq!((code, out.as_str()), (0, format!("{expected}\n").as_str()));
}

#[test]
fn logical_shift_right_of_negative_one_boxes_via_the_fallback() {
    // shiftRightLogical (-1) 1 = 2^63 - 1, which overflows the immediate: the fit
    // check fails and the runtime boxes it.
    let (code, out) = int_out("Int.shiftRightLogical (0 - 1) 1");
    assert_eq!((code, out.as_str()), (0, "9223372036854775807\n"));
}

#[test]
fn bitwise_and_of_a_boxed_operand_uses_the_fallback() {
    // 2^62 is boxed; `and` with an immediate falls back to the runtime, which
    // unboxes, masks (clearing the low bit), and re-boxes nothing (0 is immediate).
    let (code, out) = int_out("Int.and 4611686018427387904 1");
    assert_eq!((code, out.as_str()), (0, "0\n"));
}

#[test]
fn bitwise_xor_on_the_fast_path() {
    let (code, out) = int_out("Int.xor 6 3");
    assert_eq!((code, out.as_str()), (0, "5\n"));
}

#[test]
fn complement_on_the_fast_path() {
    let (code, out) = int_out("Int.complement 0");
    assert_eq!((code, out.as_str()), (0, "-1\n"));
}

#[test]
fn equality_of_two_boxed_integers_uses_the_fallback() {
    // Both operands are boxed (2^62); equality falls back to `fai_equal`, which
    // compares the unboxed values.
    let (code, out) =
        run(&main_printing("if 4611686018427387904 = 4611686018427387904 then \"eq\" else \"ne\""));
    assert_eq!((code, out.as_str()), (0, "eq\n"));
}

#[test]
fn comparison_across_the_immediate_boundary_uses_the_fallback() {
    // Both operands are boxed; `<` falls back to `fai_int_lt` on the unboxed values.
    let (code, out) =
        run(&main_printing("if 4611686018427387904 < 4611686018427387905 then \"lt\" else \"ge\""));
    assert_eq!((code, out.as_str()), (0, "lt\n"));
}

#[test]
fn boolean_not_and_inequality_are_inlined() {
    let (code, out) = run(&main_printing("if not (true <> true) then \"y\" else \"n\""));
    assert_eq!((code, out.as_str()), (0, "y\n"));
}

#[test]
fn boolean_equality_on_the_fast_path() {
    let (code, out) = run(&main_printing("if true = true then \"y\" else \"n\""));
    assert_eq!((code, out.as_str()), (0, "y\n"));
}

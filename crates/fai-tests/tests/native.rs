//! End-to-end native tests: compile a program with `fai build` (in process) and
//! run the produced executable, asserting its output and a clean, leak-free exit
//! (the runtime aborts with a nonzero code if any object leaks).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use indoc::{formatdoc, indoc};

/// Builds `src` (as `Main.fai`) into a native binary and runs it, returning its
/// `(stdout, exit_code)`.
fn build_and_run(src: &str) -> (String, Option<i32>) {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Main.fai"), src).unwrap();
    let exe = dir.join("prog");

    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = fai_cli::run(
        [
            "fai",
            "build",
            "--no-daemon",
            "-C",
            dir.to_str().unwrap(),
            "Main.fai",
            "--out",
            exe.to_str().unwrap(),
        ],
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "build failed: {}", String::from_utf8_lossy(&err));

    // `fai build` appends the platform executable extension (`.exe` on Windows).
    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = Command::new(&produced).output().unwrap();
    (String::from_utf8_lossy(&run.stdout).into_owned(), run.status.code())
}

fn unique_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fai-native-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Builds a multi-file program (entry is `Main.fai`) and runs it.
fn build_and_run_files(files: &[(&str, &str)]) -> (String, Option<i32>) {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    for (name, src) in files {
        std::fs::write(dir.join(name), src).unwrap();
    }
    let exe = dir.join("prog");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = fai_cli::run(
        [
            "fai",
            "build",
            "--no-daemon",
            "-C",
            dir.to_str().unwrap(),
            "Main.fai",
            "--out",
            exe.to_str().unwrap(),
        ],
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "build failed: {}", String::from_utf8_lossy(&err));
    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = Command::new(&produced).output().unwrap();
    (String::from_utf8_lossy(&run.stdout).into_owned(), run.status.code())
}

#[test]
fn nested_module_values_types_and_ctors_run() {
    let src = indoc! {r#"
        module Main

        module Math =
          let square x = x * x

          public type Shape =
            | Circle Int
            | Rect Int Int

          public area : Shape -> Int
          let area s =
            match s with
            | Circle r -> square r
            | Rect w h -> w * h

        public main : Runtime -> Unit
        let main runtime =
          runtime.console.writeLine (Int.toString (Math.area (Math.Circle 3)))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "9\n");
    assert_eq!(code, Some(0));
}

#[test]
fn cross_file_nested_qualified_access_runs() {
    let lib = indoc! {r#"
        module Lib

        module Geo =
          public double : Int -> Int
          let double x = x + x
    "#};
    let main = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime =
          runtime.console.writeLine (Int.toString (Lib.Geo.double 21))
    "#};
    let (out, code) = build_and_run_files(&[("Lib.fai", lib), ("Main.fai", main)]);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
fn cross_module_forwarder_borrows_and_runs() {
    // `Lib.sumList` borrows its list; `Main.forward` only forwards `xs` to it, so
    // inter-procedural inference borrows `xs` too. `main` lends the same list to
    // two `forward` calls and releases it once — a leak-free (exit 0) run proves
    // the cross-module borrowing is reference-count sound.
    let lib = indoc! {r#"
        module Lib

        public sumList : List Int -> Int
        let sumList xs =
          match xs with
          | [] -> 0
          | x :: r -> x + sumList r
    "#};
    let main = indoc! {r#"
        module Main

        forward : List Int -> Int
        let forward xs = Lib.sumList xs

        public main : Runtime -> Unit
        let main runtime =
          let xs = [1, 2, 3, 4, 5]
          runtime.console.writeLine (Int.toString (forward xs + forward xs))
    "#};
    let (out, code) = build_and_run_files(&[("Lib.fai", lib), ("Main.fai", main)]);
    assert_eq!(out, "30\n");
    assert_eq!(code, Some(0));
}

#[test]
fn inlined_record_drop_links_and_runs() {
    // A let-bound closed record owning a `String` is dropped through the inlined
    // release path, which calls the `fai_free` runtime export directly. Building
    // natively proves that symbol resolves against the runtime archive at link
    // time; the leak-free (exit 0) run proves the inlined drop frees the cell and
    // its String child.
    let src = indoc! {r#"
        module Main

        type R = { name : String, n : Int }

        make : String -> R
        let make s = { name = s, n = 7 }

        public main : Runtime -> Unit
        let main runtime =
          let rec = make "boxed"
          runtime.console.writeLine (Int.toString rec.n)
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "7\n");
    assert_eq!(code, Some(0));
}

#[test]
fn cross_module_mutual_recursion_borrow_cycle_runs() {
    // `Ev.isEven` and `Od.isOdd` are mutually recursive *across files*, each only
    // forwarding the tail to the other — a borrow cycle that spans modules,
    // resolved by the salsa borrow fixpoint (both borrow their list). A leak-free
    // run confirms the cross-module cycle is reference-count sound end-to-end.
    let ev = indoc! {r#"
        module Ev

        public isEven : List Int -> Bool
        let isEven xs =
          match xs with
          | [] -> true
          | _ :: r -> Od.isOdd r
    "#};
    let od = indoc! {r#"
        module Od

        public isOdd : List Int -> Bool
        let isOdd xs =
          match xs with
          | [] -> false
          | _ :: r -> Ev.isEven r
    "#};
    let main = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime =
          let xs = [1, 2, 3, 4]
          runtime.console.writeLine (if Ev.isEven xs then "even" else "odd")
    "#};
    let (out, code) = build_and_run_files(&[("Ev.fai", ev), ("Od.fai", od), ("Main.fai", main)]);
    assert_eq!(out, "even\n");
    assert_eq!(code, Some(0));
}

#[test]
fn as_pattern_binds_and_runs() {
    // The as-pattern aliases the whole matched (cons) value; both the alias and
    // the bound tail reference it, so this also exercises reference counting.
    let src = indoc! {r#"
        module Main

        sizeIfNonEmpty : List Int -> Int
        let sizeIfNonEmpty xs =
          match xs with
          | first :: rest as whole -> List.length whole
          | [] -> 0

        public main : Runtime -> Unit
        let main runtime =
          runtime.console.writeLine (Int.toString (sizeIfNonEmpty [10, 20, 30]))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "3\n");
    assert_eq!(code, Some(0));
}

fn print_main(expr: &str) -> String {
    formatdoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine ({expr})
    "#}
}

#[test]
fn arithmetic() {
    let (out, code) = build_and_run(&print_main("Int.toString (1 + 2 * 3)"));
    assert_eq!(out, "7\n");
    assert_eq!(code, Some(0));
}

#[test]
fn string_concatenation() {
    let (out, code) = build_and_run(&print_main("\"a\" ++ \"b\" ++ \"c\""));
    assert_eq!(out, "abc\n");
    assert_eq!(code, Some(0));
}

#[test]
fn conditional() {
    let (out, code) = build_and_run(&print_main("if 2 < 1 then \"t\" else \"f\""));
    assert_eq!(out, "f\n");
    assert_eq!(code, Some(0));
}

#[test]
fn cross_definition_call() {
    let src = indoc! {r#"
        module Main

        let double x = x + x

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (double 21))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
// A named top-level function used as a *value*: `add 40` is a partial application
// passed to `apply`, which applies it. This exercises the static-closure code
// pointer and the runtime partial-application path on every target, AOT included.
fn higher_order_and_partial_application() {
    let src = indoc! {r#"
        module Main

        let add x y = x + y

        let apply f x = f x

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (apply (add 40) 2))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
// A function returns a function (a closure), which is then bound and applied.
fn returns_a_function() {
    let src = indoc! {r#"
        module Main

        let makeAdder x = fun y -> x + y

        public main : Runtime -> Unit
        let main runtime =
          let add40 = makeAdder 40
          runtime.console.writeLine (Int.toString (add40 2))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
// Over-application: `makeAdder` has arity one but is applied to two arguments in a
// single expression, so the runtime applies the surplus to the returned function.
fn over_application_of_returned_function() {
    let src = indoc! {r#"
        module Main

        let makeAdder x = fun y -> x + y

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (makeAdder 40 2))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
// A zero-arity top-level binding holding a partial application: referencing `inc`
// forces its static closure (applying it to no arguments), yielding the `add 1`
// partial application, which is then applied.
fn forced_zero_arity_value_binding() {
    let src = indoc! {r#"
        module Main

        let add x y = x + y

        let inc = add 1

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (inc 41))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
fn hello_sample_builds_and_runs() {
    let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/Hello.fai");
    let src = std::fs::read_to_string(sample).unwrap();
    let (out, code) = build_and_run(&src);
    assert_eq!(out, "Hello, Fai!\n");
    assert_eq!(code, Some(0));
}

#[test]
fn user_defined_operator_runs() {
    let src = indoc! {r#"
        module Main

        let (+++) a b = a * b + 1

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString (2 +++ 3))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "7\n"); // 2 * 3 + 1
    assert_eq!(code, Some(0));
}

#[test]
fn interface_instance_dispatch_runs() {
    let src = indoc! {r#"
        module Main

        interface Greeter =
          greet : String -> String

        let exclaimer = { Greeter with greet name = name ++ "!" }

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (exclaimer.greet "hi")
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "hi!\n");
    assert_eq!(code, Some(0));
}

#[test]
fn interface_instance_captures_state() {
    // The method closure captures the surrounding `n`.
    let src = indoc! {r#"
        module Main

        interface Counter =
          next : Unit -> Int

        let always n = { Counter with next u = n }

        public main : Runtime -> Unit
        let main runtime = runtime.console.writeLine (Int.toString ((always 42).next ()))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
fn builtin_operator_as_value_runs() {
    // `(+)` passed first-class to a fold.
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime =
          runtime.console.writeLine (Int.toString (List.foldl (+) 0 [1, 2, 3, 4]))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "10\n");
    assert_eq!(code, Some(0));
}

#[test]
fn derived_capability_with_least_authority_runs() {
    // The milestone's acceptance scenario: build a derived capability via an
    // interface instance (a prefixing `Console`), pass a runtime carrying only
    // that capability to a least-authority function, and run.
    let src = indoc! {r#"
        module Main

        prefixed : String -> Console -> Console
        let prefixed tag inner = { Console with writeLine s = inner.writeLine (tag ++ s) }

        announce : { console : Console | _ } -> String -> Unit
        let announce env msg = env.console.writeLine msg

        public main : Runtime -> Unit
        let main runtime =
          let logger = prefixed "[log] " runtime.console
          announce { console = logger } "hello"
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "[log] hello\n");
    assert_eq!(code, Some(0));
}

#[test]
fn row_polymorphic_least_authority_runs() {
    // A function that requests only the `console` capability, given the full
    // `Runtime`. The native build resolves the field offset via runtime evidence.
    let src = indoc! {r#"
        module Main

        announce : { console : Console | 'r } -> String -> Unit
        let announce env msg = env.console.writeLine msg

        public main : Runtime -> Unit
        let main runtime = announce runtime "least authority"
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "least authority\n");
    assert_eq!(code, Some(0));
}

#[test]
fn row_polymorphic_record_update_runs() {
    let src = indoc! {r#"
        module Main

        bump : { score : Int | 'r } -> { score : Int | 'r }
        let bump rec = { rec with score = rec.score + 100 }

        public main : Runtime -> Unit
        let main runtime =
          let bumped = bump { name = "x", score = 5 }
          runtime.console.writeLine (Int.toString bumped.score)
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "105\n");
    assert_eq!(code, Some(0));
}

#[test]
fn all_capabilities_compose_in_one_program() {
    // Console, clock, random, file system, and environment used together. The
    // output is deterministic: `nextInt 1` is `0` and the clock reads positive.
    // The path comes from the host temp directory so it is valid on every OS.
    let path = std::env::temp_dir().join("fai-native-allcaps.txt");
    let path = path.to_str().unwrap().replace('\\', "/");
    let src = formatdoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime =
          let t = runtime.clock.now ()
          let n = runtime.random.nextInt 1
          match runtime.fs.writeFile "{path}" "x" with
          | Err e -> runtime.console.writeLine e
          | Ok u ->
            match runtime.env.get "FAI_DEFINITELY_UNSET_E2E" with
            | Some v -> runtime.console.writeLine v
            | None -> runtime.console.writeLine (if t > 0 then Int.toString n else "no-clock")
    "#};
    let (out, code) = build_and_run(&src);
    assert_eq!(out, "0\n");
    assert_eq!(code, Some(0));
}

#[test]
fn file_system_capability_round_trips() {
    let path = std::env::temp_dir().join("fai-native-fs-roundtrip.txt");
    let path = path.to_str().unwrap().replace('\\', "/");
    let src = formatdoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime =
          match runtime.fs.writeFile "{path}" "persisted" with
          | Err e -> runtime.console.writeLine e
          | Ok u ->
            match runtime.fs.readFile "{path}" with
            | Err e -> runtime.console.writeLine e
            | Ok contents -> runtime.console.writeLine contents
    "#};
    let (out, code) = build_and_run(&src);
    assert_eq!(out, "persisted\n");
    assert_eq!(code, Some(0));
}

#[test]
fn random_capability_runs() {
    // `nextInt 1` is always `0` (the half-open range `[0, 1)`), so the output is
    // deterministic even though the source is the host's random capability.
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime =
          runtime.console.writeLine (Int.toString (runtime.random.nextInt 1))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "0\n");
    assert_eq!(code, Some(0));
}

#[test]
fn clock_capability_runs() {
    // The clock reads positive epoch milliseconds.
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit
        let main runtime =
          runtime.console.writeLine (if runtime.clock.now () > 0 then "ok" else "no")
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "ok\n");
    assert_eq!(code, Some(0));
}

#[test]
fn user_supplied_console_instance_runs() {
    // Capabilities are ordinary interfaces: a program can build its own `Console`
    // instance and dispatch through it. This one discards its argument, so it
    // prints nothing (and the host runtime goes unused).
    let src = indoc! {r#"
        module Main

        silent : Console
        let silent = { Console with writeLine s = () }

        public main : Runtime -> Unit
        let main runtime = silent.writeLine "ignored"
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "");
    assert_eq!(code, Some(0));
}

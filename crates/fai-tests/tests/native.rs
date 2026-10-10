//! End-to-end native tests: compile a program with `fai build` (in process) and
//! run the produced executable, asserting its output and a clean, leak-free exit
//! (the runtime aborts with a nonzero code if any object leaks).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use indoc::{formatdoc, indoc};

#[test]
fn peeled_scalar_recursion_preserves_full_width_integer_results() {
    let source = "module Main\nlet f n = if n <= 1 then 1 else n * f (n - 1) + f (n - 2)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (f 30))\n";
    let mut previous = 1i64;
    let mut expected = 1i64;
    for n in 2i64..=30 {
        let next = n.wrapping_mul(expected).wrapping_add(previous);
        previous = expected;
        expected = next;
    }
    assert_eq!(build_and_run(source), (format!("{expected}\n"), Some(0)));
}

#[test]
fn peeled_scalar_recursion_preserves_float_results() {
    let source = "module Main\nlet f n x = if n <= 0 then x else x + f (n - 1) (x + 0.5)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Float.toString (f 6 1.0))\n";
    assert_eq!(build_and_run(source), ("17.5\n".into(), Some(0)));
}

#[test]
fn peeled_scalar_recursion_preserves_effect_order() {
    let source = "module Main\nlet f n =\n  let _ = stdConsole.writeLine (Int.toString n)\n  if n <= 1 then n else f (n - 1) + f (n - 2)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (f 3))\n";
    assert_eq!(build_and_run(source), ("3\n2\n1\n0\n1\n2\n".into(), Some(0)));
}

#[test]
fn wide_float_return_crosses_the_native_first_class_wrapper() {
    let library = "module Wide\npublic type V = { x : Float, y : Float, z : Float }\npublic shift : V -> V\nlet shift v = { x = v.x + 1.0, y = v.y + 2.0, z = v.z + 3.0 }\n";
    let main = "module Main\nlet apply n f x = if n = 0 then f x else apply (n - 1) f x\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let v = apply 1 Wide.shift { x = 1.0, y = 2.0, z = 3.0 }\n  r.console.writeLine (Int.toString (Float.toInt (v.x + v.y + v.z)))\n";
    assert_eq!(
        build_and_run_files(&[("Wide.fai", library), ("Main.fai", main)]),
        ("12\n".into(), Some(0))
    );
}

#[test]
fn eight_float_native_return_accepts_spilled_arguments() {
    let library = "module Wide\npublic type V = { a : Float, b : Float, c : Float, d : Float, e : Float, f : Float, g : Float, h : Float }\npublic add : V -> V -> V\nlet add x y = { a = x.a + y.a, b = x.b + y.b, c = x.c + y.c, d = x.d + y.d, e = x.e + y.e, f = x.f + y.f, g = x.g + y.g, h = x.h + y.h }\n";
    let main = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let a = { a = 1.0, b = 2.0, c = 3.0, d = 4.0, e = 5.0, f = 6.0, g = 7.0, h = 8.0 }\n  let b = Wide.add a a\n  let c = Wide.add b a\n  r.console.writeLine (Int.toString (Float.toInt (c.a + c.b + c.c + c.d + c.e + c.f + c.g + c.h)))\n";
    assert_eq!(
        build_and_run_files(&[("Wide.fai", library), ("Main.fai", main)]),
        ("108\n".into(), Some(0))
    );
}

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
    assert_eq!(
        code,
        0,
        "build failed: {}{}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );

    // `fai build` appends the platform executable extension (`.exe` on Windows).
    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = Command::new(&produced).output().unwrap();
    (String::from_utf8_lossy(&run.stdout).into_owned(), run.status.code())
}

/// Builds `src` and runs it, returning `(stdout, stderr, exit_code)`. For a test
/// that asserts a runtime abort (a located fault), which writes to stderr and exits
/// non-zero — with no exit code on a Unix signal abort (`None`).
fn build_and_run_captured(src: &str) -> (String, String, Option<i32>) {
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
    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = Command::new(&produced).output().unwrap();
    (
        String::from_utf8_lossy(&run.stdout).into_owned(),
        String::from_utf8_lossy(&run.stderr).into_owned(),
        run.status.code(),
    )
}

fn unique_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "fai-native-e2e-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

#[test]
fn niche_construction_preserves_uniform_integer_aliases() {
    let library = "module Lib\npublic same : { tag : Int | _ } -> Int -> Bool\nlet same r x = r.tag = 0 && Option.withDefault 0 (Some x) = x\n";
    let main = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (if Lib.same { tag = 0 } 0 && Lib.same { tag = 0 } 9223372036854775807 then \"ok\" else \"failed\")\n";
    assert_eq!(
        build_and_run_files(&[("Lib.fai", library), ("Main.fai", main)]),
        ("ok\n".into(), Some(0))
    );
}

#[test]
fn niche_construction_preserves_native_string_aliases() {
    let source = "module Main\nkeep : String -> Bool\nlet keep x = Option.withDefault \"\" (Some x) = x\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (if keep \"hello\" then \"ok\" else \"failed\")\n";
    assert_eq!(build_and_run(source), ("ok\n".into(), Some(0)));
}

#[test]
fn mixed_self_tail_calls_run_natively() {
    let source = "module Main\nlet ack m n = if m = 0 then n + 1 else if n = 0 then ack (m - 1) 1 else ack (m - 1) (ack m (n - 1))\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (ack 3 5))\n";
    assert_eq!(build_and_run(source), ("253\n".into(), Some(0)));
}

#[test]
fn addition_tail_recursion_runs_deeply_with_wrapping_results() {
    let source = "module Main\nlet sum n = if n <= 0 then 9223372036854775807 else n + sum (n - 1)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (sum 1000000))\n";
    let expected = i64::MAX.wrapping_add(1_000_000 * 1_000_001 / 2);
    assert_eq!(build_and_run(source), (format!("{expected}\n"), Some(0)));
}

#[test]
fn addition_tail_recursion_keeps_effect_order() {
    let source = "module Main\nlet sum n =\n  let _ = stdConsole.writeLine (Int.toString n)\n  if n <= 0 then 0 else n + sum (n - 1)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (sum 3))\n";
    assert_eq!(build_and_run(source), ("3\n2\n1\n0\n6\n".into(), Some(0)));
}

#[test]
fn addition_tail_recursion_preserves_row_evidence() {
    let source = "module Main\nlet sum xs = match xs with | [] -> 0 | x :: rest -> x.value + sum rest\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (sum [{ value = 10, extra = true }, { value = 20, extra = false }]))\n";
    assert_eq!(build_and_run(source), ("30\n".into(), Some(0)));
}

#[test]
fn mixed_row_self_calls_keep_the_uniform_float_abi() {
    let source = "module Main\nlet f r n x = if n <= 0 then r.base + x else if n % 2 = 0 then f r (n - 1) (x + 0.5) else 1.0 + f r (n - 1) (x + 0.5)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Float.toString (f { base = 2.25, extra = \"alive\" } 4 0.0))\n";
    assert_eq!(build_and_run(source), ("6.25\n".into(), Some(0)));
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

        public main : Runtime -> Unit / { Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (Math.area (Math.Circle 3)))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "9\n");
    assert_eq!(code, Some(0));
}

/// Compiles `c_src` to an object with the C compiler (`$CC`, default `cc`),
/// returning its path. Skips (returns `None`) if no C compiler is available.
fn compile_c_object(dir: &Path, name: &str, c_src: &str) -> Option<PathBuf> {
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let c_path = dir.join(format!("{name}.c"));
    let o_path = dir.join(format!("{name}.o"));
    std::fs::write(&c_path, c_src).unwrap();
    let status = Command::new(&cc).arg("-c").arg(&c_path).arg("-o").arg(&o_path).status().ok()?;
    status.success().then_some(o_path)
}

#[test]
fn user_foreign_links_and_runs_against_a_native_object() {
    // A user `foreign` function: its native object is declared in `fai.toml` and
    // linked into the executable, and its scalar/string arguments and results are
    // marshalled across the plain native ABI.
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let c_src = r#"
#include <stdint.h>
#include <string.h>
int64_t fai_test_triple(int64_t x) { return x * 3; }
double  fai_test_scale(double x) { return x * 1.5; }
// Returns a static C string; writes its length through `out_len`.
const char* fai_test_shout(const char* p, int64_t len, int64_t* out_len) {
    static char buf[256];
    int64_t n = len < 250 ? len : 250;
    for (int64_t i = 0; i < n; i++) buf[i] = (p[i] >= 'a' && p[i] <= 'z') ? p[i] - 32 : p[i];
    buf[n] = '!';
    *out_len = n + 1;
    return buf;
}
"#;
    let Some(_obj) = compile_c_object(&dir, "nativelib", c_src) else {
        eprintln!("skipping: no C compiler available");
        return;
    };
    std::fs::write(dir.join("fai.toml"), "[native]\nobjects = [\"nativelib.o\"]\n").unwrap();
    let src = indoc! {r#"
        module Main

        foreign "fai_test_triple" triple : Int -> Int / { Console }

        foreign "fai_test_scale" scale : Float -> Float / { Console }

        foreign "fai_test_shout" shout : String -> String / { Console }

        public main : Runtime -> Unit / { Console }
        let main rt =
          let u = rt.console.writeLine (Int.toString (triple 14))
          let v = rt.console.writeLine (Float.toString (scale 2.0))
          rt.console.writeLine (shout "fai")
    "#};
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
    let produced = exe.with_extension(std::env::consts::EXE_EXTENSION);
    let run = Command::new(&produced).output().unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert_eq!(stdout, "42\n3.0\nFAI!\n", "got: {stdout}");
    assert_eq!(run.status.code(), Some(0));
}

#[cfg(unix)]
#[test]
fn native_object_initializers_survive_section_elimination() {
    let dir = unique_dir();
    std::fs::create_dir_all(&dir).unwrap();
    let c_source = r#"
#include <stdint.h>
static int64_t initialized;
__attribute__((constructor)) static void initialize(void) { initialized = 42; }
int64_t fai_initialized_value(int64_t add) { return initialized + add; }
"#;
    let Some(_) = compile_c_object(&dir, "initializer", c_source) else {
        eprintln!("skipping: no C compiler available");
        return;
    };
    std::fs::write(dir.join("fai.toml"), "[native]\nobjects = [\"initializer.o\"]\n").unwrap();
    std::fs::write(dir.join("Main.fai"), "module Main\nforeign \"fai_initialized_value\" initialized : Int -> Int / { Console }\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (initialized 0))\n").unwrap();
    let executable = dir.join("program");
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
            executable.to_str().unwrap(),
        ],
        &mut out,
        &mut err,
    );
    assert_eq!(code, 0, "{}{}", String::from_utf8_lossy(&out), String::from_utf8_lossy(&err));
    let output = Command::new(executable).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"42\n");
}

#[test]
fn tls_plaintext_reports_bounded_write_progress() {
    let (out, code) = build_and_run(indoc! {r#"
        module Main
        public main : Runtime -> Unit / { Console, Tls }
        let main runtime =
          match runtime.tls.client "localhost" with
          | Err e -> runtime.console.writeLine e
          | Ok session ->
            let bytes = Bytes.fromString (String.joinArray "" (Array.repeat 65536 "abcd"))
            match runtime.tls.writePlaintext session bytes with
            | Err e -> runtime.console.writeLine e
            | Ok accepted ->
              match runtime.tls.writePlaintext session bytes with
              | Err e -> runtime.console.writeLine e
              | Ok blocked -> runtime.console.writeLine (if accepted > 0 && accepted < Bytes.length bytes && blocked = 0 then "partial then blocked" else "wrong progress")
    "#});
    assert_eq!(code, Some(0));
    assert_eq!(out, "partial then blocked\n");
}

#[test]
fn float_order_and_sign_bits_are_preserved_by_native_code() {
    let source = indoc! {r#"
        module Main
        public main : Runtime -> Unit / { Console }
        let main runtime =
          let negativeZero = -0.0
          let nan = Float.fromBits 0x7ff8000000001234
          let valid = negativeZero < 0.0 && Float.toBits negativeZero = 0x8000000000000000 && nan >= nan && nan <= nan && Float.toBits (-nan) = 0xfff8000000001234
          runtime.console.writeLine (if valid then "ok" else "wrong float semantics")
    "#};
    let (out, code) = build_and_run(source);
    assert_eq!(code, Some(0));
    assert_eq!(out, "ok\n");
}

#[test]
fn user_runtime_builder_extends_the_capability_bundle() {
    // The entry file defines its own `runtime` builder, so `main` receives an
    // extended bundle: the standard console (a public default) plus a user-defined
    // `Banner` capability (effect-parameterized, backed by that console). The
    // backend prefers the entry-file `runtime` over `defaultRuntime`.
    let src = indoc! {r#"
        module Main

        public interface Banner 'e =
          banner : String -> Unit / 'e

        public bannerOverConsole : Console -> Banner { Console }
        let bannerOverConsole console =
          { Banner with banner title = console.writeLine ("=== " ++ title ++ " ===") }

        public type AppRuntime = { banner : Banner { Console }, console : Console }

        public runtime : AppRuntime
        let runtime = { banner = bannerOverConsole stdConsole, console = stdConsole }

        public main : AppRuntime -> Unit / { Console }
        let main app =
          let u = app.banner.banner "Welcome"
          app.console.writeLine "Body"
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "=== Welcome ===\nBody\n");
    assert_eq!(code, Some(0));
}

#[test]
fn char_operations_run_natively() {
    // Exercises char literals, intrinsics, pattern matching, and a multibyte
    // scalar value through the AOT build path; a clean exit also proves the
    // reference counting stays balanced (the runtime aborts nonzero on a leak).
    let src = indoc! {r#"
        module Main

        let vowel c =
          match c with
          | 'a' | 'e' | 'i' | 'o' | 'u' -> "yes"
          | _ -> "no"

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let parts = [Char.toString 'A', Int.toString (Char.toCode 'A'), Char.toString '\u{1F600}', vowel 'e', vowel 'z']
          runtime.console.writeLine (String.join "|" parts)
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "A|65|\u{1F600}|yes|no\n");
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

        public main : Runtime -> Unit / { Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (Lib.Geo.double 21))
    "#};
    let (out, code) = build_and_run_files(&[("Lib.fai", lib), ("Main.fai", main)]);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
fn cross_module_opaque_types_build_and_run() {
    // An opaque union and an opaque record are built and read only in `Lib`;
    // `Main` holds and forwards their values across the module boundary by name.
    // A leak-free (exit 0) run proves the uniform boxed representation carries an
    // opaque value across files with no special codegen.
    let lib = indoc! {r#"
        module Lib

        public opaque type Counter =
          | Counter Int

        public opaque type Stats = { hits : Int, misses : Int }

        public zero : Counter
        let zero = Counter 0

        public bump : Counter -> Counter
        let bump c =
          match c with
          | Counter n -> Counter (n + 1)

        public value : Counter -> Int
        let value c =
          match c with
          | Counter n -> n

        public stats : Int -> Int -> Stats
        let stats h m = { hits = h, misses = m }

        public total : Stats -> Int
        let total s = s.hits + s.misses
    "#};
    let main = indoc! {r#"
        module Main

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let c = Lib.bump (Lib.bump Lib.zero)
          let s = Lib.stats 3 4
          runtime.console.writeLine (Int.toString (Lib.value c + Lib.total s))
    "#};
    let (out, code) = build_and_run_files(&[("Lib.fai", lib), ("Main.fai", main)]);
    assert_eq!(out, "9\n");
    assert_eq!(code, Some(0));
}

#[test]
fn exhaustive_record_patterns_build_and_run() {
    let src = indoc! {r#"
        module Main
        choose : { flag : Bool, extra : Int } -> Int
        let choose r =
          match r with
          | { flag = true | _ } -> 19
          | { flag = false | _ } -> 23
        public main : Runtime -> Unit / { Console }
        let main runtime =
          let a = choose { extra = 0, flag = true }
          let b = choose { flag = false, extra = 100 }
          runtime.console.writeLine (Int.toString (a + b))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!((out.as_str(), code), ("42\n", Some(0)));
}

#[track_caller]
fn assert_array_capacity_aborts(capacity: &str) {
    let src = formatdoc! {r#"
        module Main
        allocate : Int -> Int
        let allocate n = Array.length (Array.withCapacity n)
        public main : Runtime -> Unit / {{ Console }}
        let main runtime = runtime.console.writeLine (Int.toString (allocate {capacity}))
    "#};
    let (out, err, code) = build_and_run_captured(&src);
    assert_ne!(code, Some(0));
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("allocation size exceeds supported range"), "{err}");
}

#[test]
fn oversized_array_capacity_wrapping_to_zero_aborts() {
    assert_array_capacity_aborts("2305843009213693948");
}

#[test]
fn oversized_array_capacity_wrapping_below_header_aborts() {
    assert_array_capacity_aborts("2305843009213693949");
}

#[test]
fn oversized_array_capacity_exceeding_layout_limit_aborts() {
    assert_array_capacity_aborts("1152921504606846972");
}

#[test]
fn array_capacity_negative_boxed_argument_is_leak_free() {
    let src = indoc! {r#"
        module Main
        let allocate make = Array.length (make (0 - 9223372036854775807 - 1))
        public main : Runtime -> Unit / { Console }
        let main runtime =
          runtime.console.writeLine (Int.toString (allocate Array.withCapacity))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!((out.as_str(), code), ("0\n", Some(0)));
}

#[test]
fn runtime_integer_results_merge_as_values_not_tagged_words() {
    let source = indoc! {r#"
        module Main
        public main : Runtime -> Unit / { Console }
        let main runtime =
          let bytes = Bytes.fromString "a"
          let value = if Bytes.isEmpty bytes then 0 else Bytes.unsafeGet 0 bytes
          runtime.console.writeLine (Int.toString value)
    "#};
    let (out, code) = build_and_run(source);
    assert_eq!((out.as_str(), code), ("97\n", Some(0)));
}

#[track_caller]
fn assert_native_bounds_trap(source: &str) {
    let (out, err, code) = build_and_run_captured(source);
    assert_ne!(code, Some(0));
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("array index out of bounds"), "{err}");
}

#[test]
fn wrapping_addition_keeps_the_native_bounds_check() {
    assert_native_bounds_trap(include_str!("fixtures/bounds/WrappingAdd.fai"));
}

#[test]
fn wrapping_subtraction_keeps_the_native_bounds_check() {
    assert_native_bounds_trap(include_str!("fixtures/bounds/WrappingSub.fai"));
}

#[test]
fn generated_callers_keep_the_native_bounds_check() {
    assert_native_bounds_trap(include_str!("fixtures/bounds/FusedCaller.fai"));
}

#[test]
fn a_checked_array_does_not_remove_another_arrays_native_check() {
    assert_native_bounds_trap(include_str!("fixtures/bounds/CheckedThenOther.fai"));
}

#[test]
fn unique_constructor_tags_run_natively() {
    assert_eq!(build_and_run(include_str!("fixtures/UniqueTags.fai")), ("39\n".into(), Some(0)));
}

#[test]
fn borrowed_uniform_fields_run_natively() {
    assert_eq!(build_and_run(include_str!("fixtures/BorrowedFields.fai")), ("6\n".into(), Some(0)));
}

#[test]
fn spread_self_recursion_runs_in_constant_native_stack() {
    assert_eq!(
        build_and_run(include_str!("fixtures/SpreadLoop.fai")),
        ("1000000\n".into(), Some(0))
    );
}

#[test]
fn canonical_callback_links_and_runs_natively() {
    assert_eq!(
        build_and_run(include_str!("fixtures/CanonicalCallback.fai")),
        ("9223372036854775805\n".into(), Some(0))
    );
}

#[test]
fn numeric_array_loop_versions_preserve_shared_values() {
    assert_eq!(
        build_and_run(include_str!("fixtures/NumericArrayLoop.fai")),
        ("yes\n".into(), Some(0))
    );
}

#[test]
fn const_preserves_native_effect_order() {
    let source = indoc! {r#"
        module Main
        public main : Runtime -> Unit / { Console }
        let main r = const (r.console.writeLine "first") (r.console.writeLine "second")
    "#};
    let (output, code) = build_and_run(source);
    assert_eq!((output.as_str(), code), ("first\nsecond\n", Some(0)));
}

#[test]
fn fused_search_keeps_strict_producer_traps() {
    let source = indoc! {r#"
        module Main
        public main : Runtime -> Unit / { Console }
        let main r =
          let found = List.any (fun x -> x = 1) (List.map (fun n -> 1 / n) [1, 0])
          r.console.writeLine (if found then "found" else "missing")
    "#};
    let (output, error, code) = build_and_run_captured(source);
    assert_ne!(code, Some(0));
    assert!(output.is_empty());
    assert!(error.contains("division by zero"), "{error}");
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

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let xs = [1, 2, 3, 4]
          runtime.console.writeLine (if Ev.isEven xs then "even" else "odd")
    "#};
    let (out, code) = build_and_run_files(&[("Ev.fai", ev), ("Od.fai", od), ("Main.fai", main)]);
    assert_eq!(out, "even\n");
    assert_eq!(code, Some(0));
}

#[test]
fn intra_module_mutual_recursion_flattens_and_runs() {
    // `isEven`/`isOdd` are an intra-module plain-tail mutual group, so they compile
    // to one shared loop. A 500000-deep call runs in constant stack and exits
    // cleanly; ordinary mutual recursion would overflow the stack.
    let src = indoc! {r#"
        module Main

        isEven : Int -> Bool
        let isEven n = if n <= 0 then true else isOdd (n - 1)

        isOdd : Int -> Bool
        let isOdd n = if n <= 0 then false else isEven (n - 1)

        public main : Runtime -> Unit / { Console }
        let main rt = rt.console.writeLine (if isEven 500000 then "even" else "odd")
    "#};
    let (out, code) = build_and_run(src);
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

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / {{ Console }}
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
fn string_slicing_preserves_unicode_boundaries() {
    let source = print_main(
        "String.join \"|\" [String.take 5 \"hello🙂\", String.drop 5 \"hello🙂\", String.substring 1 1 \"a🙂b\"]",
    );
    assert_eq!(build_and_run(&source), ("hello|🙂|🙂\n".into(), Some(0)));
}

#[track_caller]
fn generic_choice_output(main: &str, expected: &str) {
    let choose = "module Choice\npublic choose : Bool -> 'a -> 'a -> 'a\nlet choose c a b = if c then a else b\n";
    assert_eq!(
        build_and_run_files(&[("Choice.fai", choose), ("Main.fai", main)]),
        (expected.into(), Some(0)),
    );
}

#[test]
fn generic_drop_releases_full_width_integer_boxes() {
    generic_choice_output(
        "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (Choice.choose false 9223372036854775807 (-9223372036854775808)))\n",
        "-9223372036854775808\n",
    );
}

#[test]
fn generic_drop_releases_float_boxes() {
    generic_choice_output(
        "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Float.toString (Choice.choose true 1.25 2.5))\n",
        "1.25\n",
    );
}

#[test]
fn generic_drop_preserves_retained_string_aliases() {
    generic_choice_output(
        "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let original = Int.toString 123\n  let chosen = Choice.choose false original (original ++ \"!\")\n  let _ = r.console.writeLine original\n  r.console.writeLine chosen\n",
        "123\n123!\n",
    );
}

#[test]
fn generic_drop_releases_aggregate_children() {
    generic_choice_output(
        "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let chosen = Choice.choose false { n = 1, text = Int.toString 123 } { n = 2, text = Int.toString 456 }\n  r.console.writeLine chosen.text\n",
        "456\n",
    );
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

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
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

// The pure date/time pipeline end to end (calendar conversion, custom-pattern
// formatting, instant rendering) — deterministic and leak-free through AOT.
#[test]
fn datetime_library_builds_and_runs() {
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let d = Option.withDefault (LocalDate.fromEpochDay 0) (LocalDate.of 2020 6 15)
          runtime.console.writeLine (LocalDate.toString d ++ " " ++ DateTimeFormat.formatDate "EEEE" d ++ " " ++ Instant.toString (Instant.fromUnixTimeSeconds 90))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "2020-06-15 Monday 1970-01-01T00:01:30Z\n");
    assert_eq!(code, Some(0));
}

// The clock capability and the local-offset primitive: `Instant.now` and
// `OffsetDateTime.now` (which also reads `Clock.localOffset`) must link and run.
// The assertion is clock-relative (after the epoch, year past 2020), so it is
// stable on any real system without hard-coding a moment.
#[test]
fn clock_now_builds_and_runs() {
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit / { Clock, Console }
        let main runtime =
          let odt = OffsetDateTime.now runtime
          let after = Instant.isAfter (Instant.now runtime) Instant.epoch
          runtime.console.writeLine (if after && (OffsetDateTime.year odt >= 2020) then "ok" else "bad")
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "ok\n");
    assert_eq!(code, Some(0));
}

#[track_caller]
fn period_round_trip(start: &str, finish: &str) {
    let source = format!(
        "module Main\npublic main : Runtime -> Unit / {{ Console }}\nlet main r =\n  let start = Option.withDefault (LocalDate.fromEpochDay 0) (LocalDate.parse \"{start}\")\n  let finish = Option.withDefault (LocalDate.fromEpochDay 0) (LocalDate.parse \"{finish}\")\n  r.console.writeLine (LocalDate.toString (Period.addToDate (Period.between start finish) start))\n"
    );
    assert_eq!(build_and_run(&source), (format!("{finish}\n"), Some(0)));
}

#[test]
fn backwards_periods_round_trip_month_end_in_native_code() {
    period_round_trip("2021-03-31", "2021-02-28");
}

#[test]
fn year_then_month_periods_round_trip_leap_day_in_native_code() {
    period_round_trip("2020-02-29", "2021-03-29");
}

#[test]
fn custom_datetime_parsing_checks_fields_in_native_code() {
    let source = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let valid = DateTimeFormat.parseTime \"hh:mm tt\" \"99:00 PM\" = None && DateTimeFormat.parseDate \"EEE yyyy-MM-dd\" \"Tue 2020-06-15\" = None && DateTimeFormat.parseTime \"hh:mm tt\" \"12:00 PM\" = Some LocalTime.noon\n  r.console.writeLine (if valid then \"strict\" else \"failed\")\n";
    assert_eq!(build_and_run(source), ("strict\n".into(), Some(0)));
}

#[test]
fn large_time_arithmetic_preserves_native_day_carries() {
    let source = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let date = LocalDateTime.plusHours 3000000 LocalDateTime.epoch\n  let elapsed = Duration.multiply 1000000 (Duration.ofHours 4)\n  let valid = LocalDate.toEpochDay (LocalDateTime.date date) = 125000 && LocalTime.nanoOfDay (LocalDateTime.time date) = 0 && Duration.days elapsed = 166666 && Duration.nanosecondOfDay elapsed = 57600000000000\n  r.console.writeLine (if valid then \"ok\" else \"failed\")\n";
    assert_eq!(build_and_run(source), ("ok\n".into(), Some(0)));
}

#[test]
fn user_defined_operator_runs() {
    let src = indoc! {r#"
        module Main

        let (+++) a b = a * b + 1

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
        let main runtime = runtime.console.writeLine (Int.toString ((always 42).next ()))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "42\n");
    assert_eq!(code, Some(0));
}

#[test]
fn effect_parameterized_interface_runs() {
    // An effect-parameterized `Logger 'e`: the console-backed instance has type
    // `Logger { Console }`, and dispatching `.log` forwards the effect. The
    // effect argument is erased, so this compiles to an ordinary dictionary.
    let src = indoc! {r#"
        module Main

        interface Logger 'e =
          log : String -> Unit / 'e

        consoleLogger : Console -> Logger { Console }
        let consoleLogger c = { Logger with log msg = c.writeLine msg }

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let logger = consoleLogger runtime.console
          logger.log "logged via Logger"
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "logged via Logger\n");
    assert_eq!(code, Some(0));
}

#[test]
fn deep_effect_subsumption_runs() {
    // `apply0` expects a maker whose returned function may use the console. Passing
    // a maker that returns a *pure* function type-checks only by deep subsumption
    // (the nested arrow's `{} ⊆ { Console }`); effects are erased, so it runs.
    let src = indoc! {r#"
        module Main

        public apply0 : (Unit -> (Unit -> Unit / { Console })) -> Unit / { Console }
        let apply0 make =
          let f = make ()
          f ()

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let c = runtime.console
          let a = apply0 (fun u -> fun v -> ())
          c.writeLine "ok"
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "ok\n");
    assert_eq!(code, Some(0));
}

#[test]
fn builtin_operator_as_value_runs() {
    // `(+)` passed first-class to a fold.
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit / { Console }
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
        let prefixed tag inner =
          { Console with writeLine s = inner.writeLine (tag ++ s), write s = inner.write (tag ++ s), writeError s = inner.writeError (tag ++ s), readLine = inner.readLine }

        announce : { console : Console | _ } -> String -> Unit / { Console }
        let announce env msg = env.console.writeLine msg

        public main : Runtime -> Unit / { Console }
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

        announce : { console : Console | 'r } -> String -> Unit / { Console }
        let announce env msg = env.console.writeLine msg

        public main : Runtime -> Unit / { Console }
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

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let bumped = bump { name = "x", score = 5 }
          runtime.console.writeLine (Int.toString bumped.score)
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "105\n");
    assert_eq!(code, Some(0));
}

#[test]
fn bytes_buffer_round_trips_natively() {
    // The built-in `Bytes` type through the AOT path: build from a list and from a
    // string, measure length, concatenate, and decode back to a `String`. Exercises
    // the runtime `fai_bytes_*` functions and the `KIND_BYTES` descriptor linked
    // into the executable.
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let hi = Bytes.fromList [72, 105]
          let _ = runtime.console.writeLine (Int.toString (Bytes.length hi))
          let greeting = Bytes.concat (Bytes.fromString "Hello, ") (Bytes.fromString "Bytes!")
          let _ =
            match Bytes.toString greeting with
            | Some s -> runtime.console.writeLine s
            | None -> runtime.console.writeLine "invalid"
          match Bytes.toString (Bytes.fromList [255]) with
          | Some s -> runtime.console.writeLine s
          | None -> runtime.console.writeLine "not-utf8"
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "2\nHello, Bytes!\nnot-utf8\n");
    assert_eq!(code, Some(0));
}

#[test]
fn row_polymorphic_field_projected_inside_a_closure_runs() {
    // A lambda that projects a field through a row variable must capture the
    // enclosing function's offset-evidence local, not merely the record. Regression:
    // free-variable collection skipped the evidence local buried in the field
    // descriptor, so the lifted lambda read a stale slot (a wrong offset) — reading
    // a plain `Int` field crashed, and dispatching a captured capability jumped to
    // garbage. Cover both: a value field and a dispatched capability.
    let src = indoc! {r#"
        module Main

        pickB : { a : Int, b : Int | 'r } -> Int
        let pickB rec =
          let get = (fun u -> rec.b)
          get 0

        shout : { console : Console | 'r } -> String -> Unit / { Console }
        let shout env msg =
          let say = (fun s -> env.console.writeLine s)
          say msg

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let _ = runtime.console.writeLine (Int.toString (pickB { a = 10, b = 20, c = 30 }))
          shout runtime "captured"
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "20\ncaptured\n");
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

        public main : Runtime -> Unit / {{ Clock, Console, Env, FileSystem, Random }}
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

        public main : Runtime -> Unit / {{ Console, FileSystem }}
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

        public main : Runtime -> Unit / { Console, Random }
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

        public main : Runtime -> Unit / { Clock, Console }
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
        let silent =
          { Console with writeLine s = (), write s = (), writeError s = (), readLine u = Ok None }

        public main : Runtime -> Unit / { Console }
        let main runtime = silent.writeLine "ignored"
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(out, "");
    assert_eq!(code, Some(0));
}

#[test]
fn progressive_file_write_then_read_round_trips() {
    // The new FileSystem handle operations: write a file in two chunks through a
    // Writer, flush+close it, then read it back in small chunks through a Reader.
    // The clean (leak-free) exit also confirms the native handles are released by
    // reference counting when their cells die (no explicit Reader close needed).
    let path = std::env::temp_dir().join(format!("fai-stream-rt-{}.txt", std::process::id()));
    let src = formatdoc! {r#"
        module Main

        writeAll : {{ fs : FileSystem | _ }} -> String -> Result Unit String / {{ FileSystem }}
        let writeAll env path =
          match env.fs.openWrite path with
          | Err e -> Err e
          | Ok w ->
            match env.fs.writeChunk w (Bytes.fromString "hello\n") with
            | Err e -> Err e
            | Ok u1 ->
              match env.fs.writeChunk w (Bytes.fromString "streamed\n") with
              | Err e -> Err e
              | Ok u2 -> env.fs.closeWriter w

        readAll : {{ fs : FileSystem | _ }} -> Reader -> String -> Result String String / {{ FileSystem }}
        let readAll env r acc =
          match env.fs.readChunk r 4 with
          | Err e -> Err e
          | Ok chunk ->
            if Bytes.length chunk = 0 then Ok acc
            else
              match Bytes.toString chunk with
              | None -> Err "invalid utf-8"
              | Some s -> readAll env r (acc ++ s)

        public main : Runtime -> Unit / {{ Console, FileSystem }}
        let main runtime =
          let path = "{path}"
          match writeAll runtime path with
          | Err e -> runtime.console.writeLine ("write failed: " ++ e)
          | Ok u ->
            match runtime.fs.openRead path with
            | Err e -> runtime.console.writeLine ("open failed: " ++ e)
            | Ok r ->
              match readAll runtime r "" with
              | Err e -> runtime.console.writeLine ("read failed: " ++ e)
              | Ok contents -> runtime.console.write contents
    "#, path = path.to_str().unwrap().replace('\\', "/")};
    let (out, code) = build_and_run(&src);
    assert_eq!(code, Some(0), "clean (leak-free) exit");
    assert_eq!(out, "hello\nstreamed\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn stream_pipeline_builds_and_runs() {
    // The pure Stream library end to end: a lazy pipeline (range |> filter |> map)
    // consumed by a fold, exercising the effect-carrying ADT through the whole
    // native pipeline. Sum of (2*x) for even x in [0,10) = 2*(0+2+4+6+8) = 40.
    let src = indoc! {r#"
        module Main

        public run : Int -> Result Int String
        let run n =
          Stream.fold (fun acc x -> acc + x) 0
            (Stream.map (fun x -> x * 2)
              (Stream.filter (fun x -> x % 2 = 0) (Stream.range 0 n)))

        public main : Runtime -> Unit / { Console }
        let main rt =
          match run 10 with
          | Ok total -> rt.console.writeLine (Int.toString total)
          | Err e -> rt.console.writeLine e
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(code, Some(0), "clean (leak-free) exit");
    assert_eq!(out, "40\n");
}

#[test]
fn stream_lines_from_file_builds_and_runs() {
    // A Stream consuming a real file handle: read a file's bytes in a stream and
    // count them, then a list round-trip. Validates that an effectful Stream
    // (carrying { FileSystem }) consumes and parks correctly.
    let path = std::env::temp_dir().join(format!("fai-stream-lines-{}.txt", std::process::id()));
    std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();
    let src = formatdoc! {r#"
        module Main

        // Read the whole file as one chunk stream and concatenate, counting bytes.
        bytesOf : {{ fs : FileSystem | _ }} -> Reader -> Stream Bytes {{ FileSystem }}
        let bytesOf env r =
          Stream.unfold
            (fun reader ->
              match env.fs.readChunk reader 8 with
              | Err e -> None
              | Ok chunk -> if Bytes.length chunk = 0 then None else Some (chunk, reader))
            r

        public main : Runtime -> Unit / {{ Console, FileSystem }}
        let main runtime =
          match runtime.fs.openRead "{path}" with
          | Err e -> runtime.console.writeLine ("open failed: " ++ e)
          | Ok r ->
            match Stream.fold (fun acc b -> acc + Bytes.length b) 0 (bytesOf runtime r) with
            | Err e -> runtime.console.writeLine ("read failed: " ++ e)
            | Ok total -> runtime.console.writeLine (Int.toString total)
    "#, path = path.to_str().unwrap().replace('\\', "/")};
    let (out, code) = build_and_run(&src);
    assert_eq!(code, Some(0), "clean (leak-free) exit");
    assert_eq!(out, "17\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn file_lines_grep_pipeline_builds_and_runs() {
    // The headline path: progressively read a file's UTF-8 lines (decoding across
    // chunk boundaries), filter, and write matches to stdout — all lazy, constant
    // memory, with the handle closed by reference counting. A multi-byte character
    // exercises the cross-chunk UTF-8 decode.
    let path = std::env::temp_dir().join(format!("fai-stream-grep-{}.txt", std::process::id()));
    std::fs::write(&path, "alpha\nbeta\u{00e9}\ngamma\ndelta\n").unwrap();
    let src = formatdoc! {r#"
        module Main

        public main : Runtime -> Unit / {{ Console, FileSystem }}
        let main runtime =
          let matches =
            Stream.fileLines runtime "{path}"
              |> Stream.filter (fun line -> String.contains line "e")
              |> Stream.toStdoutLines runtime
          match matches with
          | Ok done1 -> ()
          | Err message -> runtime.console.writeError message
    "#, path = path.to_str().unwrap().replace('\\', "/")};
    let (out, code) = build_and_run(&src);
    assert_eq!(code, Some(0), "clean (leak-free) exit");
    assert_eq!(out, "beta\u{00e9}\ndelta\n");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn deforested_pipeline_builds_and_runs_via_aot() {
    // A fused pipeline compiles through the cached AOT object path (object_code),
    // whose call into the synthesized loop must be marshalled with the loop's real
    // register ABI. Sum of doubling [0, 1000) = 999000.
    let src = indoc! {r#"
        module Main

        public run : Int -> Int
        let run n = Array.sum (Array.map (fun x -> x * 2) (Array.range 0 n))

        public main : Runtime -> Unit / { Console }
        let main rt = rt.console.writeLine (Int.toString (run 1000))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(code, Some(0), "clean (leak-free) exit");
    assert_eq!(out.trim(), "999000");
}

#[test]
fn deforested_list_fold_builds_and_runs_via_aot() {
    let src = indoc! {r#"
        module Main

        public run : Int -> Int
        let run n = List.foldl (fun acc x -> acc + x) 0 (List.range 0 n)

        public main : Runtime -> Unit / { Console }
        let main rt = rt.console.writeLine (Int.toString (run 1000))
    "#};
    let (out, code) = build_and_run(src);
    assert_eq!(code, Some(0), "clean (leak-free) exit");
    assert_eq!(out.trim(), "499500");
}

#[test]
fn out_of_bounds_unsafe_get_aborts_with_a_located_message() {
    // The inlined `unsafeGet` keeps an inline bounds check: an out-of-bounds index
    // aborts with the located "array index out of bounds" message (the runtime's
    // checked behavior, "aborts like /"), rather than reading past the buffer. The
    // process aborts, so it does not exit cleanly (no exit code on a Unix signal
    // abort), and the message reaches stderr.
    let src = indoc! {r#"
        module Main

        public main : Runtime -> Unit / { Console }
        let main runtime =
          let xs = Array.range 0 3
          runtime.console.writeLine (Int.toString (Array.unsafeGet 5 xs))
    "#};
    let (_out, err, code) = build_and_run_captured(src);
    assert_ne!(code, Some(0), "an out-of-bounds unsafeGet must not exit cleanly (stderr: {err})");
    assert!(err.contains("array index out of bounds"), "located out-of-bounds message: {err}");
}

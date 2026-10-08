//! Retained and partially-applied native functions across the uniform ABI.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const C_SOURCE: &str = r#"
#include <stdint.h>
#ifdef _WIN32
#define API __declspec(dllexport)
#else
#define API
#endif
static int64_t stored = 17;
API int64_t fai_ffi_increment(int64_t x) { return x + 1; }
API int64_t fai_ffi_add(int64_t a, int64_t b) { return a + b; }
API double fai_ffi_scale(double x) { return x * 2.0; }
API int64_t fai_ffi_not(int64_t x) { return !x; }
API void fai_ffi_store(int64_t x) { stored = x; }
API int64_t fai_ffi_load(void) { return stored; }
API void fai_ffi_noop(void) {}
API const char* fai_ffi_echo(const char* p, int64_t len, int64_t* out_len) {
  *out_len = len;
  return p;
}
API const char* fai_ffi_suffix(int64_t skip, const char* p, int64_t len, int64_t* out_len) {
  *out_len = len - skip;
  return p + skip;
}
"#;

const DECLARATIONS: &str = r#"module Main
foreign "fai_ffi_increment" increment : Int -> Int / { Console }
foreign "fai_ffi_add" add : Int -> Int -> Int / { Console }
foreign "fai_ffi_scale" scale : Float -> Float / { Console }
foreign "fai_ffi_not" invert : Bool -> Bool / { Console }
foreign "fai_ffi_echo" echo : String -> String / { Console }
foreign "fai_ffi_suffix" suffix : Int -> String -> String / { Console }
foreign "fai_ffi_store" store : Int -> Unit / { Console }
foreign "fai_ffi_load" load : Unit -> Int / { Console }
foreign "fai_ffi_noop" noop : Unit -> Unit / { Console }
type Unary = Int -> Int / { Console }
foreign "fai_ffi_increment" aliased : Unary
"#;

struct Workspace(PathBuf);

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn compile_fixture(dir: &Path, native: bool) -> String {
    let source = dir.join("fixture.c");
    std::fs::write(&source, C_SOURCE).unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| if cfg!(windows) { "cl" } else { "cc" }.into());
    let mut command = Command::new(cc);
    command.current_dir(dir);
    let output = if native {
        dir.join(if cfg!(windows) { "fixture.obj" } else { "fixture.o" })
    } else {
        dir.join(if cfg!(windows) {
            "fixture.dll"
        } else if cfg!(target_os = "macos") {
            "libfixture.dylib"
        } else {
            "libfixture.so"
        })
    };
    if cfg!(windows) {
        command.arg("/nologo");
        if native {
            command.arg("/c").arg(&source).arg(format!("/Fo{}", output.display()));
        } else {
            command.arg("/LD").arg(&source).arg("/link").arg(format!("/OUT:{}", output.display()));
        }
    } else {
        command.arg(if native {
            "-c"
        } else if cfg!(target_os = "macos") {
            "-dynamiclib"
        } else {
            "-shared"
        });
        command.arg("-fPIC").arg(&source).arg("-o").arg(&output);
    }
    let result = command.output().expect("C compiler required for FFI coverage");
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    if native {
        format!("[native]\nobjects = [\"{}\"]\n", output.file_name().unwrap().to_str().unwrap())
    } else {
        "[native]\nlibrary-dirs = [\".\"]\nlibraries = [\"fixture\"]\n".into()
    }
}

#[track_caller]
fn run_case(case: &str, native: bool) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "fai-foreign-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let _workspace = Workspace(dir.clone());
    std::fs::write(dir.join("fai.toml"), compile_fixture(&dir, native)).unwrap();
    std::fs::write(
        dir.join("Helper.fai"),
        "module Helper\npublic apply : ('a -> 'b / 'e) -> 'a -> 'b / 'e\nlet apply f x = f x\n",
    )
    .unwrap();
    let (body, expected) = match case {
        "int" => ("Int.toString (Helper.apply increment 41)", "42\n"),
        "partial" => ("Int.toString (Helper.apply (add 7) 35)", "42\n"),
        "float" => ("Float.toString (Helper.apply scale 1.25)", "2.5\n"),
        "bool" => ("if Helper.apply invert true then \"wrong\" else \"false\"", "false\n"),
        "string" => {
            ("Helper.apply echo (String.join \"\" [\"café\", \"\\0\", \"😀\"])", "café\0😀\n")
        }
        "string-partial" => {
            ("Helper.apply (suffix 2) (String.join \"\" [\"xx\", \"hello\"])", "hello\n")
        }
        "string-direct" => ("echo (String.join \"\" [\"owned\", \" buffer\"])", "owned buffer\n"),
        "unit-result" => {
            ("let _ = Helper.apply store 42\nInt.toString (Helper.apply load ())", "42\n")
        }
        "unit-argument" => ("Int.toString (Helper.apply load ())", "17\n"),
        "unit" => ("if Helper.apply noop () = () then \"unit\" else \"wrong\"", "unit\n"),
        "alias" => ("Int.toString (Helper.apply aliased 41)", "42\n"),
        _ => panic!("unknown FFI case"),
    };
    let body = body.lines().map(|line| format!("  {line}\n")).collect::<String>();
    let source = format!(
        "{DECLARATIONS}\nlet result u =\n{body}\npublic main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (result ())\n"
    );
    std::fs::write(dir.join("Main.fai"), source).unwrap();
    let mut command = if native {
        let exe = dir.join(format!("program{}", std::env::consts::EXE_SUFFIX));
        let output = Command::new(env!("CARGO_BIN_EXE_fai"))
            .args(["build", "--no-daemon", "-C"])
            .arg(&dir)
            .arg("Main.fai")
            .arg("--out")
            .arg(&exe)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Command::new(exe)
    } else {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fai"));
        command
            .args(["run", "--no-daemon", "-C"])
            .arg(&dir)
            .arg("Main.fai")
            .env("FAI_RUN_TIMEOUT_MS", "10000");
        command
    };
    let output = command.stdin(Stdio::null()).output().unwrap();
    assert!(
        output.status.success(),
        "{case}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, expected.as_bytes(), "{case}");
}

#[test]
fn jit_first_class_integer() {
    run_case("int", false);
}
#[test]
fn native_first_class_integer() {
    run_case("int", true);
}
#[test]
fn jit_partial_integer() {
    run_case("partial", false);
}
#[test]
fn native_partial_integer() {
    run_case("partial", true);
}
#[test]
fn jit_first_class_float() {
    run_case("float", false);
}
#[test]
fn native_first_class_float() {
    run_case("float", true);
}
#[test]
fn jit_first_class_boolean() {
    run_case("bool", false);
}
#[test]
fn native_first_class_boolean() {
    run_case("bool", true);
}
#[test]
fn jit_borrowed_string_result() {
    run_case("string", false);
}
#[test]
fn native_borrowed_string_result() {
    run_case("string", true);
}
#[test]
fn jit_partial_string_result() {
    run_case("string-partial", false);
}
#[test]
fn native_partial_string_result() {
    run_case("string-partial", true);
}
#[test]
fn jit_direct_borrowed_string_result() {
    run_case("string-direct", false);
}
#[test]
fn native_direct_borrowed_string_result() {
    run_case("string-direct", true);
}
#[test]
fn jit_unit_result() {
    run_case("unit-result", false);
}
#[test]
fn native_unit_result() {
    run_case("unit-result", true);
}
#[test]
fn jit_unit_argument() {
    run_case("unit-argument", false);
}
#[test]
fn native_unit_argument() {
    run_case("unit-argument", true);
}
#[test]
fn jit_unit_to_unit() {
    run_case("unit", false);
}
#[test]
fn native_unit_to_unit() {
    run_case("unit", true);
}
#[test]
fn jit_aliased_signature() {
    run_case("alias", false);
}
#[test]
fn native_aliased_signature() {
    run_case("alias", true);
}

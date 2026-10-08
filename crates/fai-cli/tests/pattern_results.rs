//! Pattern continuations retain scalar result types through helper inlining.

use std::process::Command;

#[track_caller]
fn float_result(tag: &str, definition: &str, input: &str, native: bool) {
    let dir = std::env::temp_dir().join(format!("fai-pattern-result-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = format!(
        "module Main\n{definition}\npublic main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (Float.toString (read ({input})))\n"
    );
    std::fs::write(dir.join("Main.fai"), source).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_fai"));
    command.args(["--no-daemon", "-C"]).arg(&dir);
    let output = if native {
        let exe = dir.join(format!("program{}", std::env::consts::EXE_SUFFIX));
        let build = command.args(["build", "Main.fai", "--out"]).arg(&exe).output().unwrap();
        assert!(
            build.status.success(),
            "{}{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
        Command::new(exe).output().unwrap()
    } else {
        command.args(["run", "Main.fai"]).output().unwrap()
    };
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"3.5\n");
    std::fs::remove_dir_all(dir).unwrap();
}

const CONSTRUCTOR: &str = "type Wrapped = | Wrapped Float\nlet read wrapped =\n  match wrapped with\n  | Wrapped value -> value";

#[test]
fn jit_constructor_match_returns_an_unboxed_float() {
    float_result("constructor-jit", CONSTRUCTOR, "Wrapped 3.5", false);
}

#[test]
fn native_constructor_match_returns_an_unboxed_float() {
    float_result("constructor-native", CONSTRUCTOR, "Wrapped 3.5", true);
}

#[test]
fn nested_list_tuple_pattern_returns_an_unboxed_float() {
    float_result(
        "list",
        "let read values =\n  match values with\n  | [(value, _)] -> value\n  | _ -> 0.0",
        "[(3.5, 1)]",
        false,
    );
}

#[test]
fn record_pattern_returns_an_unboxed_float() {
    float_result(
        "record",
        "let read record =\n  match record with\n  | { value = value } -> value",
        "{ value = 3.5 }",
        false,
    );
}

#[test]
fn literal_pattern_returns_an_unboxed_float() {
    float_result(
        "literal",
        "let read tag =\n  match tag with\n  | 0 -> 3.5\n  | _ -> 4.5",
        "0",
        false,
    );
}

#[test]
fn as_pattern_keeps_a_scalar_scrutinee_unboxed() {
    float_result(
        "as-pattern",
        "let read x =\n  match x with\n  | (_ as value) -> value",
        "3.5",
        false,
    );
}

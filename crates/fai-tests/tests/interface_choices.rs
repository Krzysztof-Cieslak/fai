//! Finite interface choices preserve strict evaluation, captures and fallback.

use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use fai_db::{Db, FaiDatabase};
use fai_syntax::Symbol;
use wait_timeout::ChildExt;

static LOCK: Mutex<()> = Mutex::new(());

const EFFECTS: &str = "module M\ninterface Scorer 'e = score : Int -> Int / 'e\ninvoke : Runtime -> Int -> Int -> Int / { Console }\nlet invoke r bias x =\n  let _ = r.console.writeLine \"method-called\"\n  bias + x\nbranch : Runtime -> Int -> Int / { Console }\nlet branch r value =\n  let _ = r.console.writeLine (\"branch\" ++ Int.toString value)\n  value\nargument : Runtime -> Int / { Console }\nlet argument r =\n  let _ = r.console.writeLine \"argument-called\"\n  7\nmake : Runtime -> Bool -> Scorer { Console } / { Console }\nlet make r positive =\n  let _ = r.console.writeLine \"receiver\"\n  if positive then\n    let bias = branch r 10\n    { Scorer with score x = invoke r bias x }\n  else\n    let bias = branch r 20\n    { Scorer with score x = invoke r bias x }\n";

fn database(source: &str) -> (FaiDatabase, fai_db::SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source(
        "Generic.fai".into(),
        "module Generic\npublic keep : 'a -> 'a\nlet keep x = x\n".into(),
    );
    let id = db.add_source("M.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    let diagnostics = fai_tests::check_source_diagnostics(&db, file);
    assert!(
        !diagnostics.iter().any(|d| d.severity == fai_diagnostics::Severity::Error),
        "{diagnostics:?}"
    );
    (db, file)
}

#[track_caller]
fn run(source: &str, expected: &str, native: bool) -> i64 {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (db, file) = database(source);
    if native {
        let dir = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(dir.path().join("choices")).unwrap();
        let built = fai_driver::build_native(&db, file, &path);
        assert!(built.ok, "{:?}", built.diagnostics);
        let out = Command::new(built.artifact.unwrap()).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected);
        0
    } else {
        fai_runtime::capture_start();
        fai_runtime::reset_allocations();
        let outcome = fai_driver::jit_run_program(&db, file);
        let allocations = fai_runtime::allocations();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(fai_runtime::capture_take().trim(), expected);
        allocations
    }
}

const ORDER: &str = "\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString ((make r true).score (argument r)))\n";

#[test]
fn receiver_and_branch_effects_precede_arguments_and_method() {
    run(
        &(EFFECTS.to_owned() + ORDER),
        "receiver\nbranch10\nargument-called\nmethod-called\n17",
        false,
    );
}

#[test]
fn native_receiver_and_argument_order_is_identical() {
    run(
        &(EFFECTS.to_owned() + ORDER),
        "receiver\nbranch10\nargument-called\nmethod-called\n17",
        true,
    );
}

#[test]
fn bound_choice_stays_before_intervening_effects() {
    let main = "\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let receiver = make r false\n  let _ = r.console.writeLine \"between\"\n  let alias = receiver\n  r.console.writeLine (Int.toString (alias.score (argument r)))\n";
    run(
        &(EFFECTS.to_owned() + main),
        "receiver\nbranch20\nbetween\nargument-called\nmethod-called\n27",
        false,
    );
}

#[test]
fn a_global_receiver_is_forced_once_per_read() {
    let source = "module M\ninterface S = score : Int -> Int\nlet instance =\n  let _ = stdConsole.writeLine \"init\"\n  { S with score x = x + 1 }\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (instance.score 1 + instance.score 2))\n";
    run(source, "init\ninit\n5", false);
}

#[test]
fn a_bound_global_receiver_is_forced_only_once() {
    let source = "module M\ninterface S = score : Int -> Int\nlet instance =\n  let _ = stdConsole.writeLine \"init\"\n  { S with score x = x + 1 }\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let d = instance\n  r.console.writeLine (Int.toString (d.score 1 + d.score 2))\n";
    run(source, "init\n5", false);
}

#[test]
fn escaped_methods_keep_their_branch_local_captures() {
    let source = "module M\ninterface S = score : Int -> Int\nlet pick flag bias = if flag then { S with score x = bias + x } else { S with score x = bias - x }\npublic make : Bool -> Int -> (Int -> Int)\nlet make flag bias = (pick flag bias).score\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let f = make true 40\n  let g = make false 5\n  r.console.writeLine (Int.toString (f 2 + g 3))\n";
    run(source, "44", false);
}

#[test]
fn qualified_interfaces_select_their_own_layout_slot() {
    let source = "module M\nmodule One =\n  interface Service = score : Int -> Int\nmodule Two =\n  interface Service =\n    adjust : Int -> Int\n    score : Int -> Int\nlet compute x =\n  let a = { One.Service with score y = y + 1 }\n  let b = { Two.Service with adjust y = 1000, score y = y * 2 }\n  a.score x + b.score x\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (compute 3))\n";
    run(source, "10", false);
}

#[test]
fn exhaustive_match_choices_keep_their_methods() {
    let source = "module M\ninterface S = score : Int -> Int\nlet pick flag =\n  match flag with\n  | true -> { S with score x = x + 1 }\n  | false -> { S with score x = x * 2 }\nlet compute flag = (pick flag).score 10\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (compute true + compute false))\n";
    run(source, "31", false);
}

#[test]
fn generic_and_escaping_dictionaries_keep_their_fallback() {
    let source = "module M\ninterface Box 'a = get : Unit -> 'a\nlet make x = { Box with get u = x }\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let a = Generic.keep (make 42)\n  let b = Generic.keep (make \"value\")\n  r.console.writeLine (b.get () ++ Int.toString (a.get ()))\n";
    run(source, "value42", false);
}

#[test]
fn captured_float_methods_keep_scalar_representation() {
    let source = "module M\ninterface F = apply : Float -> Float\nlet make flag bias = if flag then { F with apply x = x + bias } else { F with apply x = x * bias }\nlet compute flag x = (make flag 1.5).apply x\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Float.toString (compute true 2.0 + compute false 2.0))\n";
    run(source, "6.5", false);
}

#[test]
fn niche_arguments_keep_their_logical_value() {
    let source = "module M\ninterface F = apply : Option Int -> Int\nlet make flag =\n  if flag then { F with apply value = Option.withDefault 7 value }\n  else { F with apply value = Option.withDefault 8 value }\nlet compute flag value = (make flag).apply value\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (compute true (Some 0x7fffffffffffffff) + compute false None))\n";
    run(source, "-9223372036854775801", false);
}

#[test]
fn multi_argument_methods_evaluate_arguments_once_in_order() {
    let extra = "\ninterface PairScorer 'e = score : Int -> Int -> Int / 'e\nfactory : Runtime -> PairScorer { Console }\nlet factory r = { PairScorer with score x y = invoke r x y }\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString ((factory r).score (argument r) (branch r 2)))\n";
    run(&(EFFECTS.to_owned() + extra), "argument-called\nbranch2\nmethod-called\n9", false);
}

#[test]
fn escaped_float_method_captures_keep_their_representation() {
    let source = "module M\ninterface F = apply : Float -> Float\nlet choose flag bias = if flag then { F with apply x = x + bias } else { F with apply x = x * bias }\npublic make : Bool -> Float -> (Float -> Float)\nlet make flag bias = (choose flag bias).apply\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let f = make true 1.5\n  let g = make false 2.0\n  r.console.writeLine (Float.toString (f 2.0 + g 3.0))\n";
    run(source, "9.5", false);
}

#[test]
fn duplicated_argument_scopes_freshen_nested_captures() {
    let source = "module M\ninterface S = score : (Int -> Int) -> Int\nlet choose flag = if flag then { S with score f = f 1 } else { S with score f = f 2 }\nlet compute flag = (choose flag).score ((fun amount -> (fun x -> x + amount)) 5)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (compute true + compute false))\n";
    run(source, "13", false);
}

#[test]
fn captured_shared_aliases_add_no_per_iteration_cells() {
    let source = "module M\ninterface S = score : Int -> Int\nlet loop i n acc =\n  if i >= n then acc else\n    let d = if i % 2 = 0 then { S with score x = x + i } else { S with score x = x - i }\n    let alias = d\n    loop (i + 1) n (acc + d.score i + alias.score 1)\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString (loop 0 8 0))\n";
    let small = run(source, "28", false);
    let large = run(&source.replace("loop 0 8 0", "loop 0 80 0"), "3160", false);
    assert_eq!(small, large, "captures and aliases do not allocate per iteration");
}

#[test]
fn method_and_condition_edits_match_clean_objects() {
    let source = "module M\ninterface S = score : Int -> Int\nlet pick x = if x < 0 then { S with score y = y + 1 } else { S with score y = y * 2 }\npublic compute : Int -> Int\nlet compute x = (pick x).score x\n";
    let method = source.replace("y + 1", "y + 3");
    let condition = method.replace("x < 0", "x <= 0");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", source)], &[("M.fai", &method)], &[("M.fai", &condition)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let name = Symbol::intern("compute");
            (
                fai_core::pretty_def(&fai_core::simplified(db, file, name)),
                (*fai_driver::object_code(db, file, name, false)).clone(),
            )
        },
    );
}

#[track_caller]
fn trap(case: &str, present: &str, absent: &str) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "trap_worker", "--nocapture"])
        .env("FAI_CHOICE_TRAP", case)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let finished = child.wait_timeout(Duration::from_secs(30)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(finished && !output.status.success());
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(out.contains(present) && !out.contains(absent), "{out}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("division by zero"));
}

#[test]
fn receiver_trap_prevents_argument_evaluation() {
    trap("receiver", "receiver", "argument-called");
}
#[test]
fn argument_trap_prevents_method_evaluation() {
    trap("argument", "argument-called", "method-called");
}

#[test]
fn trap_worker() {
    let Ok(case) = std::env::var("FAI_CHOICE_TRAP") else { return };
    let extra = if case == "receiver" {
        "bad : Runtime -> Scorer { Console } / { Console }\nlet bad r =\n  let _ = r.console.writeLine \"receiver\"\n  let _ = 1 / 0\n  { Scorer with score x = invoke r 0 x }\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString ((bad r).score (argument r)))\n"
    } else {
        "bad : Runtime -> Int / { Console }\nlet bad r =\n  let _ = r.console.writeLine \"argument-called\"\n  1 / 0\npublic main : Runtime -> Unit / { Console }\nlet main r = r.console.writeLine (Int.toString ((make r true).score (bad r)))\n"
    };
    let (db, file) = database(&(EFFECTS.to_owned() + extra));
    let _ = fai_driver::jit_run_program(&db, file);
}

mod proptests {
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
        #[test]
        fn finite_captured_choices_match_arithmetic(flag in any::<bool>(), seed in -1000i64..1000, add in -9i64..10, scale in -9i64..10) {
            let source = format!("module M\ninterface S = score : Int -> Int\npublic compute : Bool -> Int -> Int\nlet compute flag seed =\n  let d = if flag then {{ S with score x = x + seed + ({add}) }} else {{ S with score x = x * ({scale}) - seed }}\n  let alias = d\n  alias.score 7\npublic main : Runtime -> Unit / {{ Console }}\nlet main r = r.console.writeLine (Int.toString (compute {flag} ({seed})))\n");
            let expected = if flag { 7 + seed + add } else { 7 * scale - seed };
            super::run(&source, &expected.to_string(), false);
        }
    }
}

//! Instrumented oracle copies verify source evaluation work outside timed kernels.

#[track_caller]
fn ocaml_calls(module: &str) {
    let source = format!(
        "let evaluator_calls = ref 0\n{}\nlet () = Printf.eprintf \"calls:%d\\n\" !evaluator_calls\n",
        include_str!("../ocaml/baseline.ml")
            .replace("let eval_chain i =", "let eval_chain i =\n    incr evaluator_calls;")
    );
    let Some(binary) = fai_tests::ocaml::build(&format!("evaluator-{module}"), &source) else {
        return;
    };
    let run = std::process::Command::new(binary).args([module, "5000"]).output().unwrap();
    assert!(run.status.success());
    assert_eq!(String::from_utf8_lossy(&run.stderr), "calls:10000\n");
    assert_eq!(
        String::from_utf8_lossy(&run.stdout).trim().parse::<i64>().unwrap(),
        fai_tests::algorithms::option_eval(5000)
    );
}

#[test]
fn ocaml_option_evaluator_is_eager() {
    ocaml_calls("OptionEval");
}
#[test]
fn ocaml_int_evaluator_is_eager() {
    ocaml_calls("IntEval");
}

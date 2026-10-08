//! Contract execution effects are checked even when capability values are hidden.

use fai_db::{Db, Diag, FaiDatabase};
use fai_diagnostics::Diagnostic;

const NOISY: &str = "public noisy : Int -> Int / { Console }\nlet noisy n =\n  let ignored = stdConsole.writeLine \"effect-executed\"\n  n\n";

fn diagnostics(extra: &str) -> (String, Vec<Diagnostic>) {
    let source = format!("module Main\n{NOISY}{extra}\n");
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source.clone());
    let file = db.source_file(id).unwrap();
    let errors = crate::check_file::accumulated::<Diag>(&db, file)
        .into_iter()
        .filter(|d| d.0.primary.source() == id)
        .map(|d| d.0.clone())
        .collect();
    (source, errors)
}

#[test]
fn honest_effectful_helper_is_rejected_at_its_application() {
    let (source, errors) = diagnostics("example: noisy 1 = 1");
    let error = errors.iter().find(|d| d.code.as_str() == "FAI6004").expect("impure contract");
    assert_eq!(
        &source[error.primary.start().raw() as usize..error.primary.end().raw() as usize],
        "noisy 1"
    );
    assert!(error.message.contains("Console"));
}

#[test]
fn an_invoked_returned_closure_carries_its_effect() {
    let (_, errors) = diagnostics("let make u = fun n -> noisy n\nexample: (make ()) 1 = 1");
    assert!(errors.iter().any(|d| d.code.as_str() == "FAI6004"), "{errors:?}");
}

#[test]
fn consuming_an_effectful_stream_is_rejected() {
    let (_, errors) = diagnostics(
        "let source = Stream.map noisy (Stream.fromList [1])\nexample: Stream.toList source = Ok [1]",
    );
    assert!(errors.iter().any(|d| d.code.as_str() == "FAI6004"), "{errors:?}");
}

#[test]
fn unknown_forwarded_execution_effect_is_not_assumed_pure() {
    let (_, errors) = diagnostics("forall f: f 1 = 1");
    assert!(errors.iter().any(|d| d.code.as_str() == "FAI6004"), "{errors:?}");
}

#[test]
fn pure_higher_order_application_closes_its_residual_effect() {
    let (_, errors) = diagnostics("example: List.map (fun x -> x + 1) [1, 2] = [2, 3]");
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn holding_an_unused_effectful_function_is_not_executing_it() {
    let (_, errors) = diagnostics("example: const true noisy");
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn an_unused_lambda_does_not_steal_the_executed_calls_location() {
    let (source, errors) = diagnostics("example: (const true (fun n -> noisy n)) && noisy 2 = 2");
    let error = errors.iter().find(|d| d.code.as_str() == "FAI6004").expect("impure contract");
    assert_eq!(
        &source[error.primary.start().raw() as usize..error.primary.end().raw() as usize],
        "noisy 2"
    );
}

#[test]
fn a_cross_module_helper_cannot_hide_its_declared_effect() {
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    db.add_source("Library.fai".into(), format!("module Library\n{NOISY}"));
    let id = db.add_source("Main.fai".into(), "module Main\nexample: Library.noisy 1 = 1\n".into());
    let errors = crate::check_file::accumulated::<Diag>(&db, db.source_file(id).unwrap());
    assert!(errors.iter().any(|d| d.0.code.as_str() == "FAI6004"), "{errors:?}");
}

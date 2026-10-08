//! Signature promises preserve latent effects and quantified effect identities.

use fai_db::{Db, Diag, FaiDatabase};

fn diagnostics(body: &str) -> (String, Vec<fai_diagnostics::Diagnostic>) {
    let source = format!("module M\n// π: signature boundary\n{body}\n");
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.clone());
    let file = db.source_file(id).unwrap();
    let diagnostics = crate::check_file::accumulated::<Diag>(&db, file)
        .into_iter()
        .filter(|d| d.0.primary.source() == id)
        .map(|d| d.0.clone())
        .collect();
    (source, diagnostics)
}

#[track_caller]
fn rejected(body: &str) -> fai_diagnostics::Diagnostic {
    let (source, diagnostics) = diagnostics(body);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.code, crate::SIGNATURE_MISMATCH);
    assert_eq!(diagnostic.primary.range().start().raw() as usize, source.rfind("let f ").unwrap());
    assert_eq!(diagnostic.primary.range().end().raw() as usize, source.trim_end().len());
    assert!(
        diagnostic.message.starts_with("the body of `f` does not match its declared type `"),
        "{diagnostic:?}"
    );
    diagnostic.clone()
}

#[track_caller]
fn accepted(body: &str) {
    let (_, diagnostics) = diagnostics(body);
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn returned_lambda_cannot_claim_purity() {
    let diagnostic =
        rejected("public f : Console -> (String -> Unit)\nlet f c = fun s -> c.writeLine s");
    assert_eq!(
        diagnostic.message,
        "the body of `f` does not match its declared type `Console -> String -> ()`"
    );
}

#[test]
fn quantified_callback_effect_cannot_be_erased() {
    rejected("public f : (Unit -> Unit / 'e) -> Unit\nlet f callback = callback ()");
}

#[test]
fn tuple_result_cannot_hide_a_callback_effect() {
    rejected(
        "public f : Console -> (Unit -> Unit) * Int\nlet f c = ((fun u -> c.writeLine \"x\"), 1)",
    );
}

#[test]
fn record_result_cannot_hide_a_callback_effect() {
    rejected(
        "public f : Console -> { log : String -> Unit }\nlet f c = { log = fun s -> c.writeLine s }",
    );
}

#[test]
fn option_result_cannot_hide_a_callback_effect() {
    rejected(
        "public f : Console -> Option (String -> Unit)\nlet f c = Some (fun s -> c.writeLine s)",
    );
}

#[test]
fn effect_parameterized_data_cannot_claim_purity() {
    rejected(
        "public type Thunk 'e = | Thunk (Unit -> Unit / 'e)\npublic f : Console -> Thunk {}\nlet f c = Thunk (fun u -> c.writeLine \"x\")",
    );
}

#[test]
fn effect_parameterized_interface_cannot_claim_purity() {
    rejected(
        "public interface Logger 'e = log : String -> Unit / 'e\npublic f : Console -> Logger {}\nlet f c = { Logger with log s = c.writeLine s }",
    );
}

#[test]
fn distinct_quantified_effects_cannot_collapse() {
    rejected(
        "public f : (Unit -> Unit / 'e) -> (Unit -> Unit / 'f) -> Unit / 'e\nlet f a b = const (a ()) (b ())",
    );
}

#[test]
fn correctly_declared_returned_effect_is_accepted() {
    accepted(
        "public f : Console -> (String -> Unit / { Console })\nlet f c = fun s -> c.writeLine s",
    );
}

#[test]
fn correctly_forwarded_quantified_effect_is_accepted() {
    accepted("public f : (Unit -> Unit / 'e) -> Unit / 'e\nlet f callback = callback ()");
}

#[test]
fn safe_latent_effect_widening_is_accepted() {
    accepted("public f : Unit -> (Unit -> Unit / { Console })\nlet f u = fun v -> ()");
}

#[test]
fn returned_callback_parameters_are_contravariant() {
    rejected(
        "public runPure : (Unit -> Unit) -> Unit\nlet runPure callback = callback ()\npublic f : Unit -> ((Unit -> Unit / { Console }) -> Unit)\nlet f u = runPure",
    );
}

#[test]
fn pure_callback_can_have_an_arbitrary_effect_upper_bound() {
    accepted("public f : Unit -> (Unit -> Unit / 'e)\nlet f u = fun v -> ()");
}

#[test]
fn consuming_a_pure_argument_keeps_the_callers_abstract_effect() {
    accepted(
        "public f : ((Unit -> Unit / 'e) -> Unit / 'e) -> Unit / 'e\nlet f call = call (fun u -> ())",
    );
}

#[test]
fn partial_application_keeps_effects_for_future_arguments() {
    accepted(
        "public both : (Unit -> Unit / 'e) -> (Unit -> Unit / 'e) -> Unit / 'e\nlet both a b = const (a ()) (b ())\npublic f : Console -> Unit / { Console }\nlet f c =\n  let partial = both (fun u -> ())\n  partial (fun u -> c.writeLine \"x\")",
    );
}

#[test]
fn a_concrete_call_residual_does_not_specialize_an_unrelated_effect() {
    accepted(
        "public tap : (Unit -> Unit / 'e) -> (Unit -> Unit / 'e) / 'e\nlet tap action =\n  let _ = action ()\n  action\npublic f : (Unit -> Unit / 'e) -> Console -> (Unit -> Unit / { Console }) / { Console | 'e }\nlet f before c =\n  let _ = before ()\n  tap (fun u -> c.writeLine \"x\")",
    );
}

#[test]
fn an_already_reported_outer_mismatch_does_not_suppress_nested_effects() {
    let (_, diagnostics) = diagnostics(
        "public f : Console -> (Unit -> Unit) / { Clock }\nlet f c = fun u -> c.writeLine \"x\"",
    );
    let mut codes: Vec<_> = diagnostics.iter().map(|d| d.code.as_str()).collect();
    codes.sort();
    assert_eq!(codes, ["FAI3004", "FAI5001"]);
}

#[test]
fn direct_effect_mismatch_is_reported_once() {
    let (_, diagnostics) = diagnostics("public f : Console -> Unit\nlet f c = c.writeLine \"x\"");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].code, crate::EFFECT_MISMATCH);
    assert_eq!(
        diagnostics[0].message,
        "the declared effect of `f` is `{}`, but its body uses `{ Console }`"
    );
}

#[test]
fn signature_effect_edits_match_clean_inference_and_keep_comment_cutoff() {
    let valid = "module M\npublic f : Console -> (String -> Unit / { Console })\nlet f c = fun s -> c.writeLine s\n";
    let mut db = FaiDatabase::new();
    crate::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), valid.into());
    let file = db.source_file(id).unwrap();
    let name = fai_syntax::Symbol::intern("f");
    let client_id = db.add_source("Client.fai".into(), "module Client\nlet g c = M.f c\n".into());
    let client = db.source_file(client_id).unwrap();
    let client_name = fai_syntax::Symbol::intern("g");
    let ty = crate::def_type(&db, client, client_name);
    assert!(crate::check_file::accumulated::<Diag>(&db, file).is_empty());
    db.enable_event_log();
    db.add_source("M.fai".into(), format!("{valid}// comment\n"));
    assert_eq!(ty, crate::def_type(&db, client, client_name));
    assert!(!db.take_events().iter().any(|e| e.contains("infer_scc_query")));
    let invalid = valid.replace(" / { Console }", "");
    db.add_source("M.fai".into(), invalid.clone());
    let _ = crate::def_type(&db, file, name);
    let incremental: Vec<_> = crate::check_file::accumulated::<Diag>(&db, file)
        .iter()
        .map(|d| (d.0.code, d.0.message.clone(), d.0.primary.range()))
        .collect();
    let mut clean = FaiDatabase::new();
    crate::std_lib::load_std(&mut clean);
    let id = clean.add_source("M.fai".into(), invalid);
    let expected: Vec<_> =
        crate::check_file::accumulated::<Diag>(&clean, clean.source_file(id).unwrap())
            .iter()
            .map(|d| (d.0.code, d.0.message.clone(), d.0.primary.range()))
            .collect();
    assert!(!incremental.is_empty());
    assert_eq!(incremental, expected);
}

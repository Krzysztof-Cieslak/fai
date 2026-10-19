//! Parsed headers retain ordinary value semantics across native storage forms.

use fai_db::{Db, Setter};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

const HELPERS: &str = r#"
parsedHeaders : String -> Result Headers String
let parsedHeaders text =
  match parseRequestHeadMeta (Bytes.fromString text) text with
  | Err error -> Err error
  | Ok (method, url, headers, version, metadata) -> Ok headers
"#;

#[track_caller]
fn check(expression: &str, native: bool) {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let file = db
        .all_source_files()
        .into_iter()
        .find(|file| file.path(&db).ends_with("/Http.fai"))
        .unwrap();
    let body = expression.lines().map(|line| format!("  {line}\n")).collect::<String>();
    let text = format!(
        "{}\n{HELPERS}\nheaderCase : Unit -> Bool\nlet headerCase unit =\n{body}\npublic main : Runtime -> Unit / {{ Console }}\nlet main runtime = runtime.console.writeLine (if headerCase () then \"ok\" else \"failed\")\n",
        file.text(&db)
    );
    file.set_text(&mut db).to(text);
    if native {
        let directory = tempfile::tempdir().unwrap();
        let output = camino::Utf8PathBuf::from_path_buf(directory.path().join("headers")).unwrap();
        let built = fai_driver::build_native(&db, file, &output);
        assert!(built.ok, "{:?}", built.diagnostics);
        let run = std::process::Command::new(built.artifact.unwrap()).output().unwrap();
        assert!(run.status.success(), "{}", String::from_utf8_lossy(&run.stderr));
        assert_eq!(run.stdout, b"ok\n");
    } else {
        fai_runtime::capture_start();
        let run = fai_driver::jit_run_program(&db, file);
        let output = fai_runtime::capture_take();
        assert_eq!(run.exit_code, 0, "{output}\n{:?}", run.diagnostics);
        assert_eq!(output, "ok\n");
    }
}

const VALUE_SEMANTICS: &str = r#"let fields = [("X", "first"), ("x", "é😀"), ("Empty", "")]
let original = Headers.fromList fields
match (parsedHeaders "GET /first HTTP/1.1\r\nX:first\r\nx: é😀\r\nEmpty:\r\n", parsedHeaders "GET /other HTTP/1.0\r\nX:  first \t\r\nx:\té😀 \r\nEmpty: \t\r\n") with
| (Ok first, Ok second) -> first = original && first = second && compare first second = 0 && Headers.toList first = fields && HashSet.size (HashSet.fromList [first, second, original]) = 1 && HashDict.get (first, 42) (HashDict.singleton (original, 42) "found") = Some "found"
| _ -> false"#;

#[test]
fn parsed_and_constructed_headers_share_equality_ordering_and_hashes() {
    check(VALUE_SEMANTICS, false);
}

#[test]
fn native_parsed_and_constructed_headers_share_value_semantics() {
    check(VALUE_SEMANTICS, true);
}

#[test]
fn structural_ordering_preserves_case_order_duplicates_and_prefixes() {
    check(
        r#"let a = Headers.fromList [("Z", "v")]
let b = Headers.fromList [("a", "v")]
let c = Headers.fromList [("a", "v"), ("a", "v")]
match parsedHeaders "GET / HTTP/1.1\r\na:v\r\n" with
| Err _ -> false
| Ok parsed -> compare a parsed < 0 && compare parsed c < 0 && parsed = b && a <> parsed && Headers.get "A" parsed = Some "v" && Headers.empty < parsed"#,
        false,
    );
}

#[test]
fn updates_and_materialization_leave_retained_header_aliases_intact() {
    check(
        r#"match parsedHeaders "GET / HTTP/1.1\r\nX:first\r\nx:second\r\nY:é\r\n" with
| Err _ -> false
| Ok original ->
  let updated = Headers.set "X" "changed" original
  let removed = Headers.remove "x" original
  let added = Headers.add "X" "third" original
  Headers.toList original = [("X", "first"), ("x", "second"), ("Y", "é")] && Headers.toList updated = [("Y", "é"), ("X", "changed")] && Headers.toList removed = [("Y", "é")] && Headers.getAll "x" added = ["first", "second", "third"] && original = Headers.fromList (Headers.toList original)"#,
        false,
    );
}

#[test]
fn first_class_and_partial_header_readers_keep_uniform_value_boundaries() {
    check(
        r#"match parsedHeaders "GET / HTTP/1.1\r\nX:value\r\n" with
| Err _ -> false
| Ok headers -> List.map (Headers.get "x") [headers, Headers.empty] = [Some "value", None] && List.map Headers.toList [headers] = [[("X", "value")]]"#,
        false,
    );
}

#[test]
fn constructed_unicode_names_keep_existing_lowercase_matching() {
    check(
        r#"let headers = Headers.fromList [("K", "kelvin"), ("K", "ascii"), ("ΟΣ", "sigma")]
Headers.getAll "k" headers = ["kelvin", "ascii"] && Headers.get "ος" headers = Some "sigma" && Headers.toList headers = [("K", "kelvin"), ("K", "ascii"), ("ΟΣ", "sigma")]"#,
        false,
    );
}

#[test]
fn invalid_late_fields_keep_validation_ahead_of_framing_interpretation() {
    check(
        r#"parsedHeaders "GET / HTTP/1.1\r\nContent-Length:bad\r\nBad Name:value\r\n" = Err "invalid header field""#,
        false,
    );
}

#[test]
fn many_parsed_fields_keep_constant_stack_and_full_value_equality() {
    check(
        r#"let fields = List.map (fun _ -> ("X", "é😀")) (List.range 0 5000)
let text = "GET / HTTP/1.1\r\n" ++ String.join "" (List.map (fun _ -> "X:é😀\r\n") fields)
match parsedHeaders text with
| Err _ -> false
| Ok parsed -> parsed = Headers.fromList fields && Headers.toList parsed = fields && List.length (Headers.getAll "x" parsed) = 5000"#,
        false,
    );
}

#[test]
fn header_reader_edits_match_clean_compilation() {
    let initial =
        "module Lookup\npublic find : Headers -> Option String\nlet find h = Headers.get \"X\" h\n";
    let edited = initial.replace("\"X\"", "\"Y\"");
    let consumer =
        "module Consumer\npublic find : Headers -> Option String\nlet find h = Lookup.find h\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[
            &[("Lookup.fai", initial), ("Consumer.fai", consumer)],
            &[("Lookup.fai", &edited), ("Consumer.fai", consumer)],
            &[("Lookup.fai", initial), ("Consumer.fai", consumer)],
        ],
        |db, ids| {
            ids.iter()
                .map(|id| {
                    let file = db.source_file(*id).unwrap();
                    let name = fai_syntax::Symbol::intern("find");
                    (
                        fai_types::body_types(db, file, name),
                        fai_rc::borrow_signature(db, file, name).0,
                        fai_core::pretty_def(&fai_rc::rc(db, file, name)),
                    )
                })
                .collect::<Vec<_>>()
        },
    );
}

#[test]
fn header_lookup_body_edits_keep_the_callers_cached_types() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let initial =
        "module Lookup\npublic find : Headers -> Option String\nlet find h = Headers.get \"X\" h\n";
    db.add_source("Lookup.fai".into(), initial.into());
    let id = db.add_source(
        "Consumer.fai".into(),
        "module Consumer\npublic find : Headers -> Option String\nlet find h = Lookup.find h\n"
            .into(),
    );
    let file = db.source_file(id).unwrap();
    let name = fai_syntax::Symbol::intern("find");
    let before = fai_types::body_types(&db, file, name);
    db.enable_event_log();
    db.add_source("Lookup.fai".into(), initial.replace("\"X\"", "\"Y\""));
    let after = fai_types::body_types(&db, file, name);
    assert_eq!(before, after);
    assert!(!db.take_events().iter().any(|event| event.contains("infer_scc_query")));
}

#[test]
fn selective_reads_borrow_headers_and_bulk_enumeration_can_consume_them() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "Readers.fai".into(),
        "module Readers\nlet find h = Headers.get \"x\" h\nlet all h = Headers.toList h\n".into(),
    );
    let file = db.source_file(id).unwrap();
    assert_eq!(fai_rc::borrow_signature(&db, file, fai_syntax::Symbol::intern("find")).0, [true]);
    assert_eq!(fai_rc::borrow_signature(&db, file, fai_syntax::Symbol::intern("all")).0, [false]);
}

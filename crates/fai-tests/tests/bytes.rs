//! General byte readers across ownership, generic calls, workers and native code.
use std::sync::Mutex;

use fai_db::{Db, FaiDatabase, SourceFile};
use fai_syntax::Symbol;

static RUN: Mutex<()> = Mutex::new(());

const READERS: &str = r#"module Main
public sum : Bytes -> Int
let sum bytes = scan bytes 0 0
let scan bytes i total =
  if i >= Bytes.length bytes then total
  else scan bytes (i + 1) (total + Bytes.unsafeGet i bytes)
"#;

const CALLBACKS: &str = "module Callbacks\npublic call : ('a -> 'b / 'e) -> 'a -> 'b / 'e\nlet call f value = f value\npublic getAt : Int -> Bytes -> Option Int\nlet getAt i bytes = Bytes.get i bytes\n";

const CONCURRENT_MAIN: &str = r#"
public main : Runtime -> Unit / { Concurrency, Console }
let main runtime =
  let bytes = Bytes.fromList [0, 127, 128, 255]
  let (left, right) = Async.parallel2 runtime.concurrency (fun _ -> sum bytes) (fun _ -> sum bytes)
  runtime.console.writeLine (Int.toString (left + right + Bytes.length bytes))
"#;

fn source(text: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Callbacks.fai".into(), CALLBACKS.into());
    let id = db.add_source("Main.fai".into(), text.into());
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn program(body: &str) -> String {
    let body = body.lines().map(|line| format!("  {line}\n")).collect::<String>();
    format!("{READERS}\npublic main : Runtime -> Unit / {{ Console }}\nlet main runtime =\n{body}")
}

#[track_caller]
fn output(text: &str, expected: &str) {
    let _guard = RUN.lock().unwrap();
    let (db, file) = source(text);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, file);
    let captured = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{result:?}");
    assert_eq!(captured.trim(), expected);
}

#[test]
fn scanning_all_byte_values_keeps_them_unsigned() {
    output(
        &program(
            "runtime.console.writeLine (Int.toString (sum (Bytes.fromList (List.range 0 256))))",
        ),
        "32640",
    );
}

#[test]
fn an_empty_buffer_scan_never_indexes_it() {
    output(&program("runtime.console.writeLine (Int.toString (sum Bytes.empty))"), "0");
}

#[test]
fn safe_indexing_handles_full_width_boundaries() {
    output(
        &program(
            r#"let bytes = Bytes.fromList [0, 127, 128, 255]
let indices = [0 - 9223372036854775807 - 1, 0 - 1, 0, 3, 4, 9223372036854775807]
let values = List.map (fun i -> Callbacks.getAt i bytes) indices
runtime.console.writeLine (if values = [None, None, Some 0, Some 255, None, None] then "ok" else "wrong")"#,
        ),
        "ok",
    );
}

#[test]
fn length_loads_observe_new_values_after_unique_growth() {
    output(
        &format!(
            "{READERS}\nlet grow n bytes = if n <= 0 then Bytes.length bytes else grow (n - 1) (Bytes.concat bytes (Bytes.fromList [255]))\npublic main : Runtime -> Unit / {{ Console }}\nlet main runtime =\n  let bytes = Bytes.fromList [128]\n  let initial = Bytes.length bytes\n  runtime.console.writeLine (Int.toString (initial + grow 32 bytes))\n"
        ),
        "34",
    );
}

#[test]
fn byte_fields_keep_their_owner_during_reads_and_later_updates() {
    output(
        &format!(
            r#"{READERS}
type Packet = | Packet Bytes
let inspect packet =
  match packet with
  | Packet bytes ->
    let first = Bytes.unsafeGet 0 bytes
    let changed = Bytes.concat bytes (Bytes.fromList [42])
    (first, Bytes.length changed)
public main : Runtime -> Unit / {{ Console }}
let main runtime =
  let packet = Packet (Bytes.fromList [255, 128])
  let (first, length) = inspect packet
  let preserved = match packet with | Packet bytes -> Bytes.toList bytes = [255, 128]
  runtime.console.writeLine (if first = 255 && length = 3 && preserved then "ok" else "wrong")
"#
        ),
        "ok",
    );
}

#[test]
fn borrowed_reads_preserve_aliases_and_later_concatenation() {
    output(
        &program(
            r#"let bytes = Bytes.fromList [0, 128, 255]
let total = sum bytes
let appended = Bytes.concat bytes (Bytes.fromList [42])
let valid = total = 383 && Bytes.toList bytes = [0, 128, 255] && Bytes.toList appended = [0, 128, 255, 42]
runtime.console.writeLine (if valid then "ok" else "wrong")"#,
        ),
        "ok",
    );
}

#[test]
fn partially_applied_readers_cross_a_generic_call_boundary() {
    output(
        &program(
            r#"let bytes = Bytes.fromList [0, 255]
let value = Callbacks.call (Bytes.unsafeGet 1) bytes
let length = Callbacks.call Bytes.length bytes
runtime.console.writeLine (Int.toString (value + length))"#,
        ),
        "257",
    );
}

#[test]
fn utf8_readers_preserve_buffers_for_conversion_and_reuse() {
    output(
        &program(
            r#"let text = Bytes.fromString "é😀"
let invalid = Bytes.fromList [240, 159]
let valid = Callbacks.call Bytes.isValidUtf8 text && not (Callbacks.call Bytes.isValidUtf8 invalid) && Bytes.toString text = Some "é😀" && Bytes.toString invalid = None
runtime.console.writeLine (if valid then "ok" else "wrong")"#,
        ),
        "ok",
    );
}

#[test]
fn conversions_preserve_string_literals_views_and_binary_aliases() {
    output(
        &program(
            r#"let original = "é😀abcdefghijklmnopqrstuvxyz0123456789"
let bytes = Callbacks.call Bytes.fromString original
let restored = Callbacks.call Bytes.toString bytes
let view = String.take 34 original
let viewedBytes = Bytes.fromString view
let good = restored = Some original && Bytes.toString viewedBytes = Some view && String.length original = 37 && Bytes.toList (Bytes.fromList [255]) = [255]
runtime.console.writeLine (if good then "ok" else "wrong")"#,
        ),
        "ok",
    );
}

#[test]
fn compiled_round_trip_transfers_unique_text_storage() {
    let _guard = RUN.lock().unwrap();
    let (db, file) = source(
        "module Main\npublic roundTrip : String -> String\nlet roundTrip text =\n  match Bytes.toString (Bytes.fromString text) with\n  | Some restored -> restored\n  | None -> \"\"\npublic main : Runtime -> Unit\nlet main runtime = ()\n",
    );
    let mut program = fai_driver::jit_compile(&db, file).unwrap();
    let f = program.function(Symbol::intern("roundTrip")).unwrap();
    let base = (fai_runtime::live_count(), fai_runtime::live_bytes());
    let original = fai_runtime::make_str("unique é😀");
    let result = fai_runtime::apply(f, &[original]);
    assert_eq!(result, original, "the compiled path must retain unique ownership");
    assert_eq!(fai_runtime::read_string(result), "unique é😀".as_bytes());
    fai_runtime::fai_drop(result);
    assert_eq!((fai_runtime::live_count(), fai_runtime::live_bytes()), base);
}

#[test]
fn row_polymorphic_readers_keep_the_uniform_argument_abi() {
    output(
        &format!(
            "{READERS}\nlet at record index = Bytes.unsafeGet index record.bytes\npublic main : Runtime -> Unit / {{ Console }}\nlet main runtime = runtime.console.writeLine (Int.toString (at {{ bytes = Bytes.fromList [128, 255], extra = 7 }} 1))\n"
        ),
        "255",
    );
}

#[test]
fn reader_operands_keep_their_source_effect_order() {
    output(
        &program(
            r#"let index = fun _ ->
  let _ = runtime.console.writeLine "index"
  0
let buffer = fun _ ->
  let _ = runtime.console.writeLine "buffer"
  Bytes.fromList [255]
runtime.console.writeLine (Int.toString (Bytes.unsafeGet (index ()) (buffer ())))"#,
        ),
        "index\nbuffer\n255",
    );
}

#[test]
fn shared_byte_scans_are_safe_across_tasks() {
    output(&format!("{READERS}{CONCURRENT_MAIN}"), "1024");
}

#[test]
fn worker_transport_retains_byte_reader_borrowing() {
    let (db, file) = source(&format!("{READERS}{CONCURRENT_MAIN}"));
    let bundle = fai_driver::build_run_bundle(&db, file).bundle.unwrap();
    let scan =
        bundle.defs.iter().find(|def| def.id.module == "Main" && def.id.name == "scan").unwrap();
    assert_eq!(scan.entry_borrowed, [true, false, false]);
    let bundle = fai_driver::bundle_from_slice(&serde_json::to_vec(&bundle).unwrap()).unwrap();
    let _guard = RUN.lock().unwrap();
    fai_runtime::capture_start();
    let exit = fai_driver::jit_run_bundle(&bundle);
    let captured = fai_runtime::capture_take();
    assert_eq!((exit, captured.as_str()), (0, "1024\n"));
}

#[test]
fn native_shared_readers_match_jit_results() {
    let (db, file) = source(&format!("{READERS}{CONCURRENT_MAIN}"));
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().join("bytes")).unwrap();
    let build = fai_driver::build_native(&db, file, &path);
    assert!(build.ok, "{:?}", build.diagnostics);
    let result = std::process::Command::new(build.artifact.unwrap()).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(result.stdout, b"1024\n");
}

#[test]
fn byte_reader_edits_match_clean_incremental_results() {
    let initial =
        "module Reader\npublic read : Bytes -> Int\nlet read bytes = Bytes.unsafeGet 0 bytes\n";
    let edited = initial.replace("unsafeGet 0", "unsafeGet 1");
    let consuming =
        initial.replace("Bytes.unsafeGet 0 bytes", "Bytes.length (Bytes.concat bytes bytes)");
    let proxy = "module Proxy\npublic read : Bytes -> Int\nlet read bytes = Reader.read bytes\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[
            &[("Reader.fai", initial), ("Proxy.fai", proxy)],
            &[("Reader.fai", &edited), ("Proxy.fai", proxy)],
            &[("Reader.fai", &consuming), ("Proxy.fai", proxy)],
            &[("Reader.fai", initial), ("Proxy.fai", proxy)],
        ],
        |db, ids| {
            ids.iter()
                .map(|id| {
                    let file = db.source_file(*id).unwrap();
                    let name = Symbol::intern("read");
                    (
                        fai_rc::borrow_signature(db, file, name).0,
                        fai_core::pretty_def(&fai_rc::rc(db, file, name)),
                    )
                })
                .collect::<Vec<_>>()
        },
    );
}

#[test]
fn a_borrow_preserving_callee_edit_cuts_off_at_the_caller() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "Reader.fai".into(),
        "module Reader\npublic read : Bytes -> Int\nlet read bytes = Bytes.unsafeGet 0 bytes\n"
            .into(),
    );
    let proxy_id = db.add_source(
        "Proxy.fai".into(),
        "module Proxy\npublic read : Bytes -> Int\nlet read bytes = Reader.read bytes\n".into(),
    );
    let file = db.source_file(proxy_id).unwrap();
    let name = Symbol::intern("read");
    let before = fai_rc::rc(&db, file, name);
    assert_eq!(fai_rc::borrow_signature(&db, file, name).0, [true]);
    db.add_source(
        "Reader.fai".into(),
        "module Reader\npublic read : Bytes -> Int\nlet read bytes = Bytes.unsafeGet 1 bytes\n"
            .into(),
    );
    assert_eq!(db.source_file(id).unwrap().source(&db), id);
    let after = fai_rc::rc(&db, file, name);
    assert!(std::sync::Arc::ptr_eq(&before, &after));
}

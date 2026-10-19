//! Untrusted HTTP metadata cannot introduce extra wire lines or multipart headers.

use fai_db::{Db, Setter};
use std::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::new(());

#[track_caller]
fn check(expression: &str, effectful: bool) {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = fai_db::FaiDatabase::new();
    let ids = fai_types::std_lib::load_std(&mut db);
    let file = ids
        .into_iter()
        .filter_map(|id| db.source_file(id))
        .find(|file| file.path(&db).ends_with("/Http.fai"))
        .unwrap();
    let effect = if effectful { " / { Net, Tls }" } else { "" };
    let main_effect = if effectful { "Console, Net, Tls" } else { "Console" };
    let body = expression.lines().map(|line| format!("  {line}\n")).collect::<String>();
    let source = format!(
        "{}\nserializationCase : Unit -> Bool{effect}\nlet serializationCase u =\n{body}\npublic main : Runtime -> Unit / {{ {main_effect} }}\nlet main r = r.console.writeLine (if serializationCase () then \"ok\" else \"failed\")\n",
        file.text(&db)
    );
    file.set_text(&mut db).to(source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, file);
    let out = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(out, "ok\n");
}

#[track_caller]
fn rejected_header(name: &str, value: &str) {
    check(
        &format!(
            "let t = {{ Transport with close u = (), recv n = Ok Bytes.empty, send b = Err \"transport touched\" }}\nsendMessage false t \"GET / HTTP/1.1\\r\\n\" (Headers.fromList [({name:?}, {value:?})]) (Stream.failed \"body forced\") = Err \"invalid outgoing HTTP start line or header\""
        ),
        true,
    );
}

#[test]
fn header_name_cannot_inject_a_second_field() {
    rejected_header("Good\r\nInjected", "value");
}

#[test]
fn header_value_cannot_inject_a_second_field() {
    rejected_header("Good", "value\r\nInjected: yes");
}

#[test]
fn lone_carriage_return_is_rejected() {
    rejected_header("Good", "value\rsuffix");
}

#[test]
fn lone_line_feed_is_rejected() {
    rejected_header("Good", "value\nsuffix");
}

#[test]
fn empty_header_names_are_rejected() {
    rejected_header("", "value");
}

#[test]
fn header_value_nul_is_rejected() {
    rejected_header("Good", "value\0suffix");
}

#[test]
fn custom_method_is_validated_before_the_transport_or_body() {
    check(
        "let t = { Transport with close u = (), recv n = Ok Bytes.empty, send b = Err \"transport touched\" }\nmatch Url.parse \"http://example.test/\" with\n| Err e -> false\n| Ok url -> sendRequest false t { method = Custom \"GET /injected HTTP/1.1\\r\\nX:\", url = url, headers = Headers.empty, body = Stream.failed \"body forced\" } = Err \"invalid outgoing HTTP method or request target\"",
        true,
    );
}

#[test]
fn request_target_cannot_contain_raw_line_breaks() {
    check(
        "let t = { Transport with close u = (), recv n = Ok Bytes.empty, send b = Err \"transport touched\" }\nmatch Url.parse \"http://example.test/path\\r\\nInjected:yes\" with\n| Err e -> true\n| Ok url -> sendRequest false t { method = GET, url = url, headers = Headers.empty, body = Stream.failed \"body forced\" } = Err \"invalid outgoing HTTP method or request target\"",
        true,
    );
}

#[test]
fn request_target_cannot_add_a_request_line_token() {
    check(
        "not (validRequestTarget \"/path HTTP/1.1\") && validRequestTarget \"/path%20with%20spaces\"",
        false,
    );
}

#[test]
fn response_reason_is_validated_even_for_a_bodyless_response() {
    check(
        "let t = { Transport with close u = (), recv n = Ok Bytes.empty, send b = Err \"transport touched\" }\nsendResponse HEAD t { status = 200, reason = \"OK\\r\\nInjected: yes\", headers = Headers.empty, body = Stream.failed \"body forced\" } = Err \"invalid outgoing HTTP status line or header\"",
        true,
    );
}

#[test]
fn valid_duplicate_headers_and_tabbed_unicode_values_remain_valid() {
    check(
        "let headers = Headers.fromList [(\"Set-Cookie\", \"a=1\"), (\"Set-Cookie\", \"b=2\"), (\"X-Note\", \"café\\t世界\")]\nvalidHead \"HTTP/1.1 200 OK\\r\\n\" headers && Bytes.toString (renderHead \"HTTP/1.1 200 OK\\r\\n\" headers) = Some \"HTTP/1.1 200 OK\\r\\nSet-Cookie: a=1\\r\\nSet-Cookie: b=2\\r\\nX-Note: café\\t世界\\r\\n\\r\\n\"",
        false,
    );
}

#[test]
fn sized_heads_preserve_order_and_replace_every_length_field() {
    check(
        r#"let headers = Headers.fromList [("Content-Length", "7"), ("Set-Cookie", "a=1"), ("content-length", "007"), ("Set-Cookie", "b=2"), ("X-Note", "café\t世界")]
renderSizedHead "HTTP/1.1 200 OK\r\n" headers 42 = renderHead "HTTP/1.1 200 OK\r\n" (Headers.set "Content-Length" "42" headers)"#,
        false,
    );
}

#[test]
fn invalid_authored_length_is_rejected_before_collecting_the_body() {
    check(
        r#"let transport = { Transport with close u = (), recv n = Ok Bytes.empty, send b = Err "transport touched" }
sendMessage false transport "HTTP/1.1 200 OK\r\n" (Headers.fromList [("Content-Length", "-1")]) (Stream.failed "body forced") = Err "invalid Content-Length""#,
        true,
    );
}

#[test]
fn sized_message_uses_utf8_byte_length() {
    check(
        r#"let expected = Bytes.fromString "HTTP/1.1 200 OK\r\nX: kept\r\nContent-Length: 6\r\n\r\né😀"
let transport = { Transport with close u = (), recv n = Ok Bytes.empty, send bytes = if bytes = expected then Ok () else Err "unexpected wire data" }
sendMessage false transport "HTTP/1.1 200 OK\r\n" (Headers.fromList [("X", "kept")]) (stringBody "é😀") = Ok ()"#,
        true,
    );
}

#[test]
fn buffered_binary_chunks_are_joined_once_in_wire_order() {
    check(
        r#"let expected = Bytes.concat (Bytes.fromString "HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n") (Bytes.fromList [0, 255, 128, 1])
let transport = { Transport with close u = (), recv n = Ok Bytes.empty, send bytes = if bytes = expected then Ok () else Err "unexpected wire data" }
sendMessage false transport "HTTP/1.1 200 OK\r\n" Headers.empty (Stream.fromList [Bytes.fromList [0, 255], Bytes.empty, Bytes.fromList [128, 1]]) = Ok ()"#,
        true,
    );
}

#[test]
fn body_size_overflow_is_checked_before_arithmetic() {
    check(
        "bodySize [Bytes.fromList [1]] 9223372036854775807 = Err \"HTTP body is too large\"",
        false,
    );
}

#[test]
fn sized_head_field_rendering_keeps_constant_stack() {
    check(
        r#"let headers = Headers.fromList (List.map (fun _ -> ("X", "v")) (List.range 0 4000))
Bytes.length (renderSizedHead "HTTP/1.1 200 OK\r\n" headers 0) = 24038"#,
        false,
    );
}

#[track_caller]
fn scoped_response_case(body: &str) {
    check(
        &format!(
            "let transport = {{ Transport with close u = (), recv n = Ok Bytes.empty, send b = Err \"transport touched\" }}\nlet sender = bufferedChunkSender transport\n{body}"
        ),
        true,
    );
}

#[track_caller]
fn scoped_response_bytes(expected: &str, body: &str) {
    check(
        &format!(
            "let expected = Bytes.fromString {expected:?}\nlet transport = {{ Transport with close u = (), recv n = Ok Bytes.empty, send b = if b = expected then Ok () else Err \"unexpected wire data\" }}\nlet sender = bufferedChunkSender transport\n{body}"
        ),
        true,
    );
}

#[test]
fn scoped_response_preserves_duplicates_and_replaces_close_and_length_in_order() {
    scoped_response_bytes(
        "HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nX-Note: café\t世界\r\nSet-Cookie: b=2\r\nConnection: close\r\nContent-Length: 6\r\n\r\né😀",
        r#"let headers = Headers.fromList [("Content-Length", "002"), ("Set-Cookie", "a=1"), ("Connection", "keep-alive"), ("X-Note", "café\t世界"), ("content-length", "2"), ("connection", "CLOSE"), ("Set-Cookie", "b=2")]
sendScopedResponse sender false GET transport { status = 200, reason = "OK", headers = headers, body = stringBody "é😀" } = Ok true"#,
    );
}

#[test]
fn scoped_response_rejects_conflicting_lengths_before_body_evaluation() {
    scoped_response_case(
        r#"sendScopedResponse sender false GET transport { status = 200, reason = "OK", headers = Headers.fromList [("Content-Length", "2"), ("content-length", "3")], body = Stream.failed "body forced" } = Err "conflicting Content-Length fields""#,
    );
}

#[test]
fn scoped_response_rejects_invalid_fields_before_framing_errors() {
    scoped_response_case(
        r#"sendScopedResponse sender false GET transport { status = 200, reason = "OK", headers = Headers.fromList [("Content-Length", "2"), ("Transfer-Encoding", "chunked"), ("X", "bad\nvalue")], body = Stream.failed "body forced" } = Err "invalid outgoing HTTP status line or header""#,
    );
}

#[test]
fn scoped_response_validates_fields_before_rejecting_an_upgrade() {
    scoped_response_case(
        r#"sendScopedResponse sender false GET transport { status = 101, reason = "Switching", headers = Headers.fromList [("Bad Name", "value")], body = Stream.failed "body forced" } = Err "invalid outgoing HTTP status line or header""#,
    );
}

#[test]
fn scoped_head_removes_transfer_coding_before_interpreting_framing() {
    scoped_response_bytes(
        "HTTP/1.1 200 OK\r\nContent-Length: 0007\r\n\r\n",
        r#"sendScopedResponse sender false HEAD transport { status = 200, reason = "OK", headers = Headers.fromList [("Transfer-Encoding", "unsupported"), ("Content-Length", "0007")], body = Stream.failed "body forced" } = Ok false"#,
    );
}

#[test]
fn scoped_no_content_removes_invalid_length_without_forcing_the_body() {
    scoped_response_bytes(
        "HTTP/1.1 204 No Content\r\nX: kept\r\n\r\n",
        r#"sendScopedResponse sender false GET transport { status = 204, reason = "No Content", headers = Headers.fromList [("Content-Length", "bad"), ("Transfer-Encoding", "chunked"), ("X", "kept")], body = Stream.failed "body forced" } = Ok false"#,
    );
}

#[test]
fn scoped_not_modified_preserves_authored_length_without_forcing_the_body() {
    scoped_response_bytes(
        "HTTP/1.1 304 Not Modified\r\nContent-Length: 0007\r\n\r\n",
        r#"sendScopedResponse sender false GET transport { status = 304, reason = "Not Modified", headers = Headers.fromList [("Content-Length", "0007")], body = Stream.failed "body forced" } = Ok false"#,
    );
}

#[test]
fn scoped_chunked_send_failure_precedes_body_failure() {
    scoped_response_case(
        r#"sendScopedResponse sender false GET transport { status = 200, reason = "OK", headers = Headers.fromList [("Transfer-Encoding", "chunked")], body = Stream.failed "body forced" } = Err "transport touched""#,
    );
}

#[test]
fn scoped_buffered_body_failure_precedes_sending() {
    scoped_response_case(
        r#"sendScopedResponse sender false GET transport { status = 200, reason = "OK", headers = Headers.empty, body = Stream.failed "body forced" } = Err "body forced""#,
    );
}

#[test]
fn scoped_successful_connect_is_rejected_before_sending() {
    scoped_response_case(
        r#"sendScopedResponse sender false (Custom "CONNECT") transport { status = 200, reason = "OK", headers = Headers.empty, body = Stream.failed "body forced" } = Err "HTTP protocol upgrades and CONNECT tunnels are not supported""#,
    );
}

#[test]
fn scoped_unsuccessful_connect_remains_an_ordinary_response() {
    scoped_response_bytes(
        "HTTP/1.1 404 Missing\r\nContent-Length: 1\r\n\r\nx",
        r#"sendScopedResponse sender false (Custom "CONNECT") transport { status = 404, reason = "Missing", headers = Headers.empty, body = stringBody "x" } = Ok false"#,
    );
}

#[test]
fn scoped_metadata_keeps_repeated_values_in_wire_order() {
    check(
        r#"match analyzeScopedFields [("Connection", "keep-alive"), ("Content-Length", "02"), ("CONNECTION", "close"), ("content-length", "2"), ("Transfer-Encoding", "gzip"), ("transfer-encoding", "chunked")] [] [] [] [] with
| Err _ -> false
| Ok head -> head.connections = ["keep-alive", "close"] && head.lengths = ["02", "2"] && head.transfers = ["gzip", "chunked"]"#,
        false,
    );
}

#[test]
fn scoped_large_head_preserves_the_input_alias_and_constant_stack() {
    check(
        r#"let headers = Headers.fromList (List.map (fun _ -> ("X", "v")) (List.range 0 5000))
match analyzeScopedFields (Headers.toList headers) [] [] [] [] with
| Err _ -> false
| Ok head -> Bytes.length (renderScopedHead "HTTP/1.1 200 OK\r\n" head.fields false true false (Some 0)) = 30038 && List.length (Headers.toList headers) = 5000"#,
        false,
    );
}

mod proptests {
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(16))]
        #[test]
        fn scoped_classification_matches_header_operations(
            fields in prop::collection::vec((0u8..4, "[A-Za-z][A-Za-z0-9-]{0,10}", "[A-Za-z0-9 \t]{0,16}", any::<bool>()), 0..16),
            close in any::<bool>(),
            mode in 0u8..4,
        ) {
            let literal = fields.iter().map(|(kind, name, value, unicode)| {
                let name = match kind {
                    1 => "CoNnEcTiOn",
                    2 => "Content-Length",
                    3 => "TRANSFER-ENCODING",
                    _ => name.as_str(),
                };
                let value = if *unicode { format!("{value}é世界") } else { value.clone() };
                format!("({name:?}, {value:?})")
            }).collect::<Vec<_>>().join(", ");
            let remove_length = mode == 0 || mode == 3;
            let remove_transfer = mode >= 2;
            let length = if mode == 0 { "Some 42" } else { "None" };
            let render = if mode == 0 {
                "renderSizedHead start expected 42"
            } else {
                "renderHead start expected"
            };
            super::check(&format!(r#"let headers = Headers.fromList [{literal}]
let start = "HTTP/1.1 200 OK\r\n"
let expected = if {remove_transfer} then Headers.remove "Transfer-Encoding" headers else headers
let expected = if {remove_length} then Headers.remove "Content-Length" expected else expected
let expected = if {close} then Headers.set "Connection" "close" expected else expected
match analyzeScopedFields (Headers.toList headers) [] [] [] [] with
| Err _ -> false
| Ok head -> head.connections = Headers.getAll "Connection" headers && head.lengths = Headers.getAll "Content-Length" headers && head.transfers = Headers.getAll "Transfer-Encoding" headers && renderScopedHead start head.fields {close} {remove_length} {remove_transfer} ({length}) = {render}"#), false);
        }
    }
}

#[track_caller]
fn rejected_part(part: &str) {
    check(
        &format!(
            "Result.map bodyText (multipartBody \"B\" [{part}]) = Err \"invalid multipart boundary or metadata\""
        ),
        false,
    );
}

#[test]
fn multipart_name_cannot_inject_headers() {
    rejected_part("field \"name\\r\\nInjected: yes\" \"value\"");
}

#[test]
fn multipart_filename_cannot_inject_headers() {
    rejected_part("filePart \"file\" \"file\\r\\nInjected: yes\" \"text/plain\" Bytes.empty");
}

#[test]
fn multipart_content_type_cannot_inject_headers() {
    rejected_part("filePart \"file\" \"file.txt\" \"text/plain\\r\\nInjected: yes\" Bytes.empty");
}

#[test]
fn multipart_content_type_requires_a_media_type() {
    rejected_part("filePart \"file\" \"file.txt\" \"not-a-media-type\" Bytes.empty");
}

#[test]
fn multipart_boundary_cannot_inject_headers() {
    check(
        "Result.map bodyText (multipartBody \"B\\r\\nInjected: yes\" []) = Err \"invalid multipart boundary or metadata\"",
        false,
    );
}

#[test]
fn multipart_quotes_and_backslashes_are_escaped() {
    check(
        r#"dispositionLine (filePart "na\"me" "a\"b\\c.txt" "text/plain" Bytes.empty) = "Content-Disposition: form-data; name=\"na\\\"me\"; filename=\"a\\\"b\\\\c.txt\"\r\n""#,
        false,
    );
}

#[test]
fn a_trailing_backslash_cannot_escape_the_parameter_delimiter() {
    check(r#"quoteParameter "end\\" = "end\\\\""#, false);
}

#[test]
fn multipart_unicode_parameters_are_preserved() {
    check(
        r#"dispositionLine (filePart "naïve" "résumé🌍.txt" "text/plain" Bytes.empty) = "Content-Disposition: form-data; name=\"naïve\"; filename=\"résumé🌍.txt\"\r\n""#,
        false,
    );
}

#[test]
fn multipart_boundaries_with_spaces_are_quoted() {
    check(
        "multipartContentType \"a b\" = \"multipart/form-data; boundary=\\\"a b\\\"\" && Result.map bodyText (multipartBody \"a b\" []) = Ok (Ok \"--a b--\\r\\n\")",
        false,
    );
}

#[test]
fn multipart_boundary_length_is_bounded() {
    let boundary = "x".repeat(71);
    check(
        &format!(
            "Result.map bodyText (multipartBody {boundary:?} []) = Err \"invalid multipart boundary or metadata\""
        ),
        false,
    );
}

#[test]
fn multipart_accepts_a_maximum_width_boundary() {
    let boundary = "x".repeat(70);
    check(
        &format!(
            "Result.map bodyText (multipartBody {boundary:?} []) = Ok (Ok \"--{boundary}--\\r\\n\")"
        ),
        false,
    );
}

#[test]
fn multipart_rejects_an_empty_boundary() {
    check("Result.isErr (multipartBody \"\" [])", false);
}

#[test]
fn multipart_rejects_trailing_boundary_space() {
    check("Result.isErr (multipartBody \"boundary \" [])", false);
}

#[test]
fn invalid_request_headers_write_no_bytes_to_a_real_peer() {
    use std::io::{Read, Write};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    let peer = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        // This startup wait includes cold JIT compilation of the client.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        let mut connection = loop {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if finished.load(Ordering::Acquire) {
                        return 0;
                    }
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        connection.set_nonblocking(false).unwrap();
        connection.set_read_timeout(Some(std::time::Duration::from_secs(15))).unwrap();
        let mut bytes = [0; 4096];
        let n = connection.read(&mut bytes).unwrap();
        if n > 0 {
            connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
        }
        n
    });
    let source = format!(
        r#"module Main
public main : Runtime -> Unit / {{ Console, Net, Tls }}
let main r =
  match Url.parse "http://127.0.0.1:{port}/" with
  | Err e -> r.console.writeLine e
  | Ok url ->
    let req = {{ method = Http.GET, url = url, headers = Headers.fromList [("X-Note", "ok\r\nInjected: yes")], body = Http.emptyBody }}
    match Http.requestOnce r None req with
    | Err e -> r.console.writeLine "rejected"
    | Ok response -> r.console.writeLine "sent"
"#
    );
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    done.store(true, Ordering::Release);
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(peer.join().unwrap(), 0);
    assert_eq!(output, "rejected\n");
}

//! Strict HTTP framing and segmentation-independent chunk decoding.

use std::sync::Mutex;

use fai_db::{Db, Setter};

static SERIAL: Mutex<()> = Mutex::new(());

const MOCK: &str = r#"
testRead : Runtime -> Channel Bytes -> Result Bytes String / { Concurrency }
let testRead env channel =
  match env.concurrency.recv channel with
  | None -> Ok Bytes.empty
  | Some bytes -> Ok bytes

testTransport : Runtime -> Channel Bytes -> Transport { Concurrency }
let testTransport env channel = { Transport with close u = env.concurrency.close channel, recv max = testRead env channel, send bytes = Ok () }

testLoad : Runtime -> Channel Bytes -> List Bytes -> Unit / { Concurrency }
let testLoad env channel chunks =
  match chunks with
  | [] -> env.concurrency.close channel
  | chunk :: rest ->
    let _ = env.concurrency.send channel chunk
    testLoad env channel rest

testChannel : Runtime -> List Bytes -> Channel Bytes / { Concurrency }
let testChannel env chunks =
  let channel = env.concurrency.channel (List.length chunks + 1)
  let _ = testLoad env channel chunks
  channel

testStrictRead : Runtime -> Channel Bytes -> Result Bytes String / { Concurrency }
let testStrictRead env channel =
  match env.concurrency.recv channel with
  | None -> Err "unexpected body read"
  | Some bytes -> Ok bytes

testStrictTransport : Runtime -> Channel Bytes -> Transport { Concurrency }
let testStrictTransport env channel = { Transport with close u = env.concurrency.close channel, recv max = testStrictRead env channel, send bytes = Ok () }
"#;

#[track_caller]
fn empty_response(method: &str, wire: &str) {
    check(
        &format!(
            "let channel = testChannel env {}\nmatch parseResponse {method} (testStrictTransport env channel) with\n| Err e -> false\n| Ok response -> bodyBytes response.body = Ok Bytes.empty",
            chunks(&[wire])
        ),
        true,
    );
}

#[test]
fn head_ignores_content_length_metadata() {
    empty_response("HEAD", "HTTP/1.1 200 OK\r\nContent-Length: 900\r\n\r\n");
}

#[test]
fn custom_head_has_the_same_body_semantics() {
    empty_response("(Custom \"HEAD\")", "HTTP/1.1 200 OK\r\nContent-Length: 900\r\n\r\n");
}

#[test]
fn no_content_does_not_wait_for_connection_eof() {
    empty_response("GET", "HTTP/1.1 204 No Content\r\n\r\n");
}

#[test]
fn not_modified_ignores_content_length_metadata() {
    empty_response("GET", "HTTP/1.1 304 Not Modified\r\nContent-Length: 900\r\n\r\n");
}

#[test]
fn informational_responses_are_skipped_across_transport_chunks() {
    check(
        &format!(
            "let channel = testChannel env {}\nmatch parseResponse GET (testStrictTransport env channel) with\n| Err e -> false\n| Ok response -> response.status = 200 && bodyText response.body = Ok \"ok\"",
            chunks(&[
                "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\n",
                "Link: </asset>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
            ])
        ),
        true,
    );
}

#[test]
fn switching_protocols_is_explicitly_unsupported() {
    check(
        "let t = { Transport with close u = (), recv n = Err \"unexpected read\", send b = Ok () }\nreadHead t (Bytes.fromString \"HTTP/1.1 101 Switching Protocols\\r\\n\\r\\n\") = Err \"HTTP protocol upgrades are not supported\"",
        false,
    );
}

#[test]
fn informational_response_count_is_bounded() {
    let wire = "HTTP/1.1 100 Continue\r\n\r\n".repeat(33);
    check(
        &format!(
            "let t = {{ Transport with close u = (), recv n = Err \"unexpected read\", send b = Ok () }}\nreadHead t (Bytes.fromString {wire:?}) = Err \"too many informational HTTP responses\""
        ),
        false,
    );
}

#[test]
fn successful_connect_tunnels_are_explicitly_unsupported() {
    check(
        "responseFraming (Custom \"CONNECT\") 200 Headers.empty = Err \"CONNECT tunnels are not supported\"",
        false,
    );
}

#[test]
fn http_10_requires_explicit_keep_alive_for_pool_reuse() {
    check(
        "not (isReusable \"HTTP/1.0\" Headers.empty NoBody) && isReusable \"HTTP/1.0\" (Headers.fromList [(\"Connection\", \"keep-alive\")]) NoBody",
        false,
    );
}

#[test]
fn unsupported_http_versions_are_rejected() {
    check(
        "match parseStatusLine \"HTTP/2 200 OK\" with\n| Err e -> true\n| Ok head -> false",
        false,
    );
}

#[track_caller]
fn header_retry_hint(buffered: &str, expected: bool) {
    check(
        &format!(
            "let t = {{ Transport with close u = (), recv n = Ok Bytes.empty, send b = Ok () }}\nmatch readHeadAfter t (Bytes.fromString {buffered:?}) 0 with\n| Ok head -> false\n| Err (retry, message) -> retry = {}",
            if expected { "true" } else { "false" }
        ),
        false,
    );
}

#[test]
fn only_an_empty_response_eof_can_hint_at_a_stale_connection() {
    header_retry_hint("", true);
}

#[test]
fn a_partial_response_head_is_never_a_stale_connection_hint() {
    header_retry_hint("HTTP/1.1 200 OK\r\nContent-", false);
}

#[test]
fn a_malformed_response_head_is_not_replayable() {
    header_retry_hint("invalid\r\n\r\n", false);
}

#[test]
fn an_interim_response_prevents_a_later_eof_from_requesting_replay() {
    header_retry_hint("HTTP/1.1 100 Continue\r\n\r\n", false);
}

#[test]
fn cancellation_never_hints_at_replay() {
    check(
        "let t = { Transport with close u = (), recv n = Err \"operation cancelled\", send b = Ok () }\nreadHeadAfter t Bytes.empty 0 = Err (false, \"operation cancelled\")",
        false,
    );
}

#[track_caller]
fn check(body: &str, concurrent: bool) {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = fai_db::FaiDatabase::new();
    let ids = fai_types::std_lib::load_std(&mut db);
    let file = ids
        .into_iter()
        .filter_map(|id| db.source_file(id))
        .find(|file| file.path(&db).ends_with("/Http.fai"))
        .unwrap();
    let case_effect = if concurrent { " / { Concurrency }" } else { "" };
    let main_effect = if concurrent { "Concurrency, Console" } else { "Console" };
    let body = body.lines().map(|line| format!("  {line}\n")).collect::<String>();
    let source = format!(
        "{}\n{MOCK}\ntestCase : Runtime -> Bool{case_effect}\nlet testCase env =\n{body}\npublic main : Runtime -> Unit / {{ {main_effect} }}\nlet main env = env.console.writeLine (if testCase env then \"ok\" else \"failed\")\n",
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
fn invalid_headers(fields: &[(&str, &str)]) {
    let fields = fields
        .iter()
        .map(|(name, value)| format!("({name:?}, {value:?})"))
        .collect::<Vec<_>>()
        .join(", ");
    check(
        &format!(
            "match framingOf (Headers.fromList [{fields}]) NoBody with\n| Err e -> true\n| Ok framing -> false"
        ),
        false,
    );
}

fn chunks(parts: &[&str]) -> String {
    format!(
        "[{}]",
        parts
            .iter()
            .map(|part| format!("Bytes.fromString {part:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[track_caller]
fn invalid_chunks(wire: &str) {
    check(
        &format!(
            "let channel = testChannel env {}\nmatch bodyBytes (chunkedBody (testTransport env channel) Bytes.empty) with\n| Err e -> true\n| Ok bytes -> false",
            chunks(&[wire])
        ),
        true,
    );
}

#[test]
fn invalid_decimal_length_is_rejected() {
    invalid_headers(&[("Content-Length", "nope")]);
}
#[test]
fn negative_length_is_rejected() {
    invalid_headers(&[("Content-Length", "-1")]);
}
#[test]
fn signed_positive_length_is_rejected() {
    invalid_headers(&[("Content-Length", "+1")]);
}
#[test]
fn overflowing_length_is_rejected() {
    invalid_headers(&[("Content-Length", "9223372036854775808")]);
}
#[test]
fn conflicting_repeated_lengths_are_rejected() {
    invalid_headers(&[("Content-Length", "1"), ("content-length", "2")]);
}
#[test]
fn conflicting_comma_lengths_are_rejected() {
    invalid_headers(&[("Content-Length", "1, 2")]);
}
#[test]
fn empty_comma_length_is_rejected() {
    invalid_headers(&[("Content-Length", "1,")]);
}
#[test]
fn transfer_coding_and_length_are_ambiguous() {
    invalid_headers(&[("Transfer-Encoding", "chunked"), ("Content-Length", "0")]);
}
#[test]
fn chunked_substring_is_not_a_transfer_coding() {
    invalid_headers(&[("Transfer-Encoding", "not-chunked")]);
}
#[test]
fn repeated_chunked_coding_is_rejected() {
    invalid_headers(&[("Transfer-Encoding", "chunked, chunked")]);
}
#[test]
fn unsupported_transfer_coding_is_rejected() {
    invalid_headers(&[("Transfer-Encoding", "gzip, chunked")]);
}

#[test]
fn non_ascii_whitespace_is_not_part_of_a_decimal_length() {
    invalid_headers(&[("Content-Length", "\u{a0}1")]);
}

#[test]
fn transfer_coding_case_and_optional_whitespace_are_accepted() {
    check(
        "framingOf (Headers.fromList [(\"Transfer-Encoding\", \" CHUNKED \")]) NoBody = Ok Chunked",
        false,
    );
}

#[test]
fn identical_repeated_lengths_are_accepted() {
    check(
        "framingOf (Headers.fromList [(\"Content-Length\", \"5, 005\"), (\"content-length\", \" 5 \")]) NoBody = Ok (SizedBody 5)",
        false,
    );
}

#[test]
fn maximum_content_length_is_accepted_without_wrapping() {
    check(
        "framingOf (Headers.fromList [(\"Content-Length\", \"9223372036854775807\")]) NoBody = Ok (SizedBody 9223372036854775807)",
        false,
    );
}

#[test]
fn maximum_chunk_size_is_accepted_without_wrapping() {
    check("parseChunkSize (Bytes.fromString \"7fffffffffffffff\") = Ok 9223372036854775807", false);
}

#[test]
fn empty_content_length_is_rejected() {
    invalid_headers(&[("Content-Length", "")]);
}

#[test]
fn empty_chunk_size_is_rejected() {
    invalid_chunks("\r\n");
}

#[test]
fn truncated_trailers_never_invoke_connection_release() {
    check(
        &format!(
            "let channel = testChannel env {}\nlet released = env.concurrency.channel 1\nlet finish rest =\n  let _ = env.concurrency.send released 1\n  Stream.empty\nlet result = bodyBytes (chunkedWith finish (testTransport env channel) Bytes.empty)\nlet _ = env.concurrency.close released\nlet marker = env.concurrency.recv released\nResult.isErr result && marker = None",
            chunks(&["0\r\n"])
        ),
        true,
    );
}

#[test]
fn invalid_hex_chunk_size_is_rejected() {
    invalid_chunks("g\r\n\r\n");
}
#[test]
fn trailing_non_hex_chunk_digits_are_rejected() {
    invalid_chunks("1g\r\na\r\n0\r\n\r\n");
}
#[test]
fn overflowing_chunk_size_is_rejected() {
    invalid_chunks("8000000000000000\r\n\r\n");
}
#[test]
fn chunk_data_requires_crlf() {
    invalid_chunks("1\r\naXY0\r\n\r\n");
}
#[test]
fn zero_chunk_requires_a_trailer_terminator() {
    invalid_chunks("0\r\n");
}
#[test]
fn trailer_fields_require_a_final_empty_line() {
    invalid_chunks("0\r\nX-Note: ok\r\n");
}
#[test]
fn trailers_cannot_redefine_framing() {
    invalid_chunks("0\r\nContent-Length: 10\r\n\r\n");
}

#[test]
fn chunk_extensions_and_their_optional_whitespace_are_accepted() {
    check(
        &format!(
            "let channel = testChannel env {}\nbodyBytes (chunkedBody (testTransport env channel) Bytes.empty) = Ok (Bytes.fromString \"a\")",
            chunks(&["1 \t; ignored=value\r\na\r\n0\r\n\r\n"])
        ),
        true,
    );
}

#[test]
fn total_trailer_metadata_is_bounded() {
    invalid_chunks(&format!("0\r\n{}\r\n", format!("X: {}\r\n", "a".repeat(7990)).repeat(9)));
}

#[test]
fn every_chunk_delimiter_can_be_split_between_reads() {
    let wire = "2\r\nab\r\n0\r\nX-Note: yes\r\n\r\n";
    let parts: Vec<_> =
        wire.as_bytes().iter().map(|byte| String::from_utf8(vec![*byte]).unwrap()).collect();
    let parts: Vec<_> = parts.iter().map(String::as_str).collect();
    check(
        &format!(
            "let channel = testChannel env {}\nlet decoded = bodyBytes (chunkedBody (testTransport env channel) Bytes.empty)\nlet remaining = env.concurrency.recv channel\ndecoded = Ok (Bytes.fromString \"ab\") && remaining = None",
            chunks(&parts)
        ),
        true,
    );
}

#[test]
fn completion_receives_buffered_bytes_after_trailers() {
    check(
        &format!(
            "let channel = testChannel env {}\nlet finish = fun rest -> Stream.cons rest Stream.empty\nbodyBytes (chunkedWith finish (testTransport env channel) Bytes.empty) = Ok (Bytes.fromString \"aNEXT\")",
            chunks(&["1\r\na\r\n0\r\nX: y\r\n\r\nNEXT"])
        ),
        true,
    );
}

#[test]
fn chunk_size_lines_are_bounded() {
    invalid_chunks(&format!("0;{}\r\n\r\n", "x".repeat(8192)));
}

#[test]
fn large_chunks_are_yielded_in_bounded_pieces() {
    let wire = format!("c350\r\n{}\r\n0\r\n\r\n", "x".repeat(50_000));
    check(
        &format!(
            "let channel = testChannel env {}\nmatch Stream.toList (chunkedBody (testTransport env channel) Bytes.empty) with\n| Err e -> false\n| Ok pieces -> List.all (fun b -> Bytes.length b <= 16384) pieces && List.sum (List.map Bytes.length pieces) = 50000",
            chunks(&[&wire])
        ),
        true,
    );
}

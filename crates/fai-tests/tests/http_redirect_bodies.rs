//! Method-preserving redirects need proof of an empty or replayable request body.

use fai_db::Db;
use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

static SERIAL: Mutex<()> = Mutex::new(());

fn read_request(stream: &mut std::net::TcpStream) -> String {
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut bytes = Vec::new();
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&bytes[..end]);
    let length = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < end + length {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    String::from_utf8(bytes).unwrap()
}

#[track_caller]
fn check(
    method: Option<&str>,
    statuses: Vec<u16>,
    expected_requests: usize,
    expected_output: &str,
) {
    let _guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    let peer = std::thread::spawn(move || {
        // This startup wait includes cold JIT compilation of the client.
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut requests = Vec::new();
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    requests.push(read_request(&mut stream));
                    let status = statuses.get(requests.len() - 1).copied().unwrap_or(200);
                    stream.write_all(format!("HTTP/1.1 {status} Response\r\nLocation: /next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if finished.load(Ordering::Acquire) {
                        break;
                    }
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
        requests
    });
    let call = method.map_or_else(|| format!("Http.get r \"http://127.0.0.1:{port}/\""), |method| format!("Http.request r {{ method = Http.{method}, url = url, headers = Headers.empty, body = Stream.defer (payload r) }}"));
    let source = format!(
        "module Main\npayload : Runtime -> Unit -> Stream Bytes {{ Console }} / {{ Console }}\nlet payload r u =\n  let wrote = r.console.writeLine \"body\"\n  Http.stringBody \"payload\"\npublic main : Runtime -> Unit / {{ Console, Net, Tls }}\nlet main r =\n  match Url.parse \"http://127.0.0.1:{port}/\" with\n  | Err e -> r.console.writeLine e\n  | Ok url ->\n    match {call} with\n    | Err e -> r.console.writeLine e\n    | Ok response -> r.console.writeLine (Int.toString response.status)\n"
    );
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("Main.fai".into(), source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    done.store(true, Ordering::Release);
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    let requests = peer.join().unwrap();
    assert_eq!(requests.len(), expected_requests, "{requests:?}");
    assert_eq!(output, expected_output);
    if method.is_some() {
        assert!(requests[0].ends_with("payload"));
    }
    assert!(requests.iter().skip(1).all(|request| request.ends_with("\r\n\r\n")));
}

#[test]
fn options_body_is_not_discarded_by_a_307() {
    check(Some("OPTIONS"), vec![307], 1, "body\n307\n");
}
#[test]
fn general_get_body_is_not_discarded_by_a_308() {
    check(Some("GET"), vec![308], 1, "body\n308\n");
}
#[test]
fn known_empty_get_follows_307() {
    check(None, vec![307], 2, "200\n");
}
#[test]
fn known_empty_get_follows_308() {
    check(None, vec![308], 2, "200\n");
}
#[test]
fn rewritten_303_body_remains_known_empty_on_later_redirects() {
    check(Some("POST"), vec![303, 307], 3, "body\n200\n");
}

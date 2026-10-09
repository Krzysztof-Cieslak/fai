//! Ordered address fallback with deterministic loopback candidates and cleanup.

use super::*;
use wait_timeout::ChildExt;

#[track_caller]
fn candidate_case(case: &str) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "reactor::connect_tests::candidate_worker", "--nocapture"])
        .env("FAI_CANDIDATE_CASE", case)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let finished = child.wait_timeout(Duration::from_secs(20)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        finished && output.status.success(),
        "{case}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_refused_first_address_falls_back_without_leaking_registration() {
    candidate_case("fallback");
}

#[test]
fn a_failed_ipv6_candidate_falls_back_to_ipv4() {
    candidate_case("dual-stack");
}

#[test]
fn all_failed_candidates_leave_no_registrations() {
    candidate_case("all-fail");
}

#[test]
fn registration_failure_advances_to_the_next_candidate() {
    candidate_case("register-fail");
}

#[test]
fn cancellation_during_connect_stops_fallback_and_releases_registration() {
    candidate_case("cancelled");
}

#[test]
fn an_empty_candidate_list_has_a_deterministic_error() {
    assert_eq!(
        connect_candidates::<()>(Vec::new(), |_| panic!("no candidate to connect")),
        Err("no address for host".to_owned())
    );
}

#[test]
fn immediate_failures_preserve_resolver_order_and_the_last_error() {
    let first = "127.0.0.1:1".parse::<SocketAddr>().unwrap();
    let second = "127.0.0.1:2".parse::<SocketAddr>().unwrap();
    let mut attempts = Vec::new();
    let result = connect_candidates::<()>(vec![first, second], |addr| {
        attempts.push(addr);
        Err(format!("failed {addr}"))
    });
    assert_eq!(attempts, vec![first, second]);
    assert_eq!(result, Err(format!("failed {second}")));
}

#[test]
fn numeric_ipv4_bypasses_the_blocking_resolver() {
    assert_eq!(
        resolve_addrs("127.0.0.1".to_owned(), 80),
        Ok(vec!["127.0.0.1:80".parse().unwrap()])
    );
}

#[test]
fn numeric_ipv6_bypasses_the_blocking_resolver() {
    assert_eq!(resolve_addrs("::1".to_owned(), 443), Ok(vec!["[::1]:443".parse().unwrap()]));
}

#[test]
fn candidate_worker() {
    let Ok(case) = std::env::var("FAI_CANDIDATE_CASE") else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let live = listener.local_addr().unwrap();
    let refused = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
    let baseline = reactor().sources.lock().unwrap().len();
    if case == "cancelled" {
        let (started, ready) = std::sync::mpsc::channel();
        let task = scheduler::spawn(Box::new(move || {
            let gate = scheduler::channel(1);
            let mut attempts = 0;
            let result = connect_candidates(vec![live, live], |addr| {
                attempts += 1;
                connect_address_with(addr, |stream| {
                    let source = register(stream)?;
                    started.send(()).unwrap();
                    scheduler::chan_recv(&gate);
                    Ok(source)
                })
            });
            assert!(matches!(result, Err(ref error) if error == scheduler::CANCELLED_MESSAGE));
            assert_eq!(attempts, 1);
            1
        }));
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        scheduler::cancel_handle(&task);
        scheduler::block_on(Box::new(move || scheduler::await_handle(&task)));
        assert_eq!(reactor().sources.lock().unwrap().len(), baseline);
        return;
    }
    scheduler::block_on(Box::new(move || {
        match case.as_str() {
            "fallback" | "dual-stack" => {
                let first = if case == "dual-stack" { "[::1]:0".parse().unwrap() } else { refused };
                let candidates = resolve_socket_addrs(&[first, live][..]).unwrap();
                assert_eq!(candidates, vec![first, live]);
                let connection = connect_candidates(candidates, connect_address)
                    .unwrap_or_else(|error| panic!("second endpoint is listening: {error}"));
                assert_eq!(reactor().sources.lock().unwrap().len(), baseline + 1);
                listener.set_nonblocking(true).unwrap();
                let _accepted = listener.accept().unwrap();
                drop(connection);
            }
            "all-fail" => {
                assert!(connect_candidates(vec![refused; 16], connect_address).is_err());
            }
            "register-fail" => {
                let mut attempts = 0;
                let connection = connect_candidates(vec![live, live], |addr| {
                    attempts += 1;
                    if attempts == 1 {
                        connect_address_with(addr, |_| Err(io::Error::other("registration failed")))
                    } else {
                        connect_address(addr)
                    }
                })
                .unwrap_or_else(|error| panic!("fallback after registration failure: {error}"));
                assert_eq!(attempts, 2);
                assert_eq!(reactor().sources.lock().unwrap().len(), baseline + 1);
                drop(connection);
            }
            _ => panic!("unknown candidate case"),
        }
        1
    }));
    assert_eq!(reactor().sources.lock().unwrap().len(), baseline);
}

//! Real-child checks use an isolated copy of this test executable.

use super::*;
use std::io::{Read, Write};

fn options() -> Options {
    Options {
        buffer: 4096,
        chunk: 1024,
        grace: Duration::ZERO,
        timeout: Some(Duration::from_secs(10)),
        drain: Some(Duration::from_secs(1)),
        poll: Duration::from_millis(5),
    }
}
fn fixture(mode: &str) -> Spec {
    Spec {
        executable: std::env::current_exe().unwrap().to_str().unwrap().into(),
        arguments: vec![
            "--exact".into(),
            "process::tests::child_fixture".into(),
            "--nocapture".into(),
        ],
        directory: String::new(),
        inherit: true,
        edits: vec![("FAI_PROCESS_FIXTURE".into(), Some(mode.into()))],
        stdin: 0,
        stdout: 0,
        stderr: 0,
    }
}

#[test]
fn host_capture_read_is_nonblocking_with_an_open_writer() {
    let (mut reader, writer) = host_output().unwrap();
    let mut bytes = [0; 4];
    assert_eq!(reader.try_read(&mut bytes).unwrap_err().kind(), io::ErrorKind::WouldBlock);
    drop(writer);
}

#[test]
fn host_capture_delivers_bytes_and_eof() {
    let (mut reader, mut writer) = host_output().unwrap();
    writer.write_all(b"abc").unwrap();
    drop(writer);
    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut bytes = [0; 8];
        match reader.try_read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&bytes[..n]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "pipe did not reach EOF");
                reader.wait(Duration::from_millis(10)).unwrap();
            }
            Err(error) => panic!("capture read failed: {error}"),
        }
    }
    assert_eq!(received, b"abc");
}

#[test]
fn child_fixture() {
    let Ok(mode) = std::env::var("FAI_PROCESS_FIXTURE") else { return };
    match mode.as_str() {
        "exchange" => {
            let stderr =
                std::thread::spawn(|| std::io::stderr().write_all(&vec![b'e'; 131072]).unwrap());
            std::io::stdout().write_all(&vec![b'o'; 131072]).unwrap();
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input).unwrap();
            assert_eq!(input, vec![b'i'; 131072]);
            stderr.join().unwrap();
        }
        "hold" => loop {
            std::thread::park();
        },
        "exit" => std::process::exit(37),
        "exitwide" => std::process::exit(-1),
        "echo" => {
            std::io::copy(&mut std::io::stdin(), &mut std::io::stdout()).unwrap();
        }
        _ => panic!("unknown fixture mode"),
    }
    std::process::exit(0);
}

#[test]
fn exchanges_more_than_pipe_capacity_in_both_directions() {
    let session = Session::start(vec![fixture("exchange")], options()).unwrap();
    let writer = session.clone();
    let writer = std::thread::spawn(move || {
        let bytes = vec![b'i'; 131072];
        let mut offset = 0;
        while offset < bytes.len() {
            offset += writer.write(&bytes[offset..]).unwrap();
        }
        writer.end_input().unwrap();
    });
    let mut out = Vec::new();
    let mut err = Vec::new();
    while let Some(chunk) = session.next().unwrap() {
        if chunk.stderr {
            err.extend(chunk.bytes);
        } else {
            out.extend(chunk.bytes);
        }
    }
    writer.join().unwrap();
    session.close();
    assert_eq!(err, vec![b'e'; 131072]);
    assert!(out.windows(131072).any(|s| s == vec![b'o'; 131072]));
    assert_eq!(lock(&session.shared.state).exits, [Some(Exit::Code(0))]);
}

#[test]
fn preserves_nonzero_exit_status() {
    let session = Session::start(vec![fixture("exit")], options()).unwrap();
    while session.next().unwrap().is_some() {}
    session.close();
    let state = lock(&session.shared.state);
    assert_eq!(state.exits, [Some(Exit::Code(37))]);
    assert!(state.cleanup.is_empty(), "cleanup failures: {:?}", state.cleanup);
    assert!(state.failure.is_none(), "process failure: {:?}", state.failure);
}

#[test]
fn close_interrupts_a_pending_output_read_and_expires_aliases() {
    let session = Session::start(vec![fixture("hold")], options()).unwrap();
    let reader = session.clone();
    let waiting = std::thread::spawn(move || while reader.next().is_ok_and(|v| v.is_some()) {});
    session.close();
    waiting.join().unwrap();
    assert_eq!(session.next().unwrap_err().kind, 8);
    assert!(lock(&session.shared.state).exits[0].is_some());
}

#[test]
fn deadline_stops_an_idle_child() {
    let mut config = options();
    config.timeout = Some(Duration::from_millis(100));
    let session = Session::start(vec![fixture("hold")], config).unwrap();
    let failure = loop {
        match session.next() {
            Err(e) => break e,
            Ok(Some(_)) => {}
            Ok(None) => panic!("missing deadline"),
        }
    };
    assert_eq!(failure.kind, 6);
    session.close();
}

#[test]
fn partial_pipeline_startup_reaps_started_children() {
    let mut missing = fixture("hold");
    missing.executable = "/fai-does-not-exist/program".into();
    let result = Session::start(vec![missing, fixture("hold")], options());
    let failure = match result {
        Err(failure) => failure,
        Ok(_) => panic!("missing executable succeeded"),
    };
    assert_eq!(failure.exits.len(), 2);
    assert!(failure.exits[0].is_none() && failure.exits[1].is_some());
}

#[test]
fn native_pipeline_preserves_each_stage_status() {
    let session = Session::start(vec![fixture("echo"), fixture("echo")], options()).unwrap();
    session.write(b"pipeline-payload\n").unwrap();
    session.end_input().unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = session.next().unwrap() {
        if !chunk.stderr {
            out.extend(chunk.bytes);
        }
    }
    session.close();
    assert!(String::from_utf8(out).unwrap().contains("pipeline-payload"));
    assert_eq!(lock(&session.shared.state).exits, [Some(Exit::Code(0)), Some(Exit::Code(0))]);
}

#[test]
fn invalid_command_is_rejected_before_start() {
    let mut command = fixture("exit");
    command.arguments.push("bad\0argument".into());
    assert!(Session::start(vec![command], options()).is_err());
}

#[test]
fn discarded_stdio_completes_without_output_buffers() {
    let mut command = fixture("echo");
    command.stdin = 2;
    command.stdout = 2;
    command.stderr = 2;
    let session = Session::start(vec![command], options()).unwrap();
    assert!(session.next().unwrap().is_none());
    session.close();
    assert_eq!(lock(&session.shared.state).exits, [Some(Exit::Code(0))]);
}

#[test]
fn conflicting_pipeline_stdio_is_rejected_before_launch() {
    let mut command = fixture("echo");
    command.stdout = 2;
    assert!(Session::start(vec![command, fixture("echo")], options()).is_err());
}

#[test]
fn final_drop_joins_and_releases_supervision() {
    let session = Session::start(vec![fixture("hold")], options()).unwrap();
    let shared = Arc::downgrade(&session.shared);
    drop(session);
    assert!(shared.upgrade().is_none());
}

#[test]
fn close_interrupts_an_input_writer_with_a_full_pipe() {
    let mut config = options();
    config.chunk = 1048576;
    config.buffer = 1048576;
    let session = Session::start(vec![fixture("hold")], config).unwrap();
    let writer = session.clone();
    let writing = std::thread::spawn(move || writer.write(&vec![1; 1048576]));
    let deadline = Instant::now() + Duration::from_secs(5);
    while lock(&session.shared.state).input.is_none() {
        assert!(Instant::now() < deadline, "writer did not enqueue input");
        std::thread::yield_now();
    }
    session.close();
    assert!(writing.join().unwrap().is_err());
}

#[test]
fn cancelled_task_does_not_start_a_process() {
    use crate::scheduler;
    scheduler::block_on(Box::new(|| {
        let gate = scheduler::channel(1);
        let task = scheduler::spawn(Box::new(move || {
            // The channel registers its waiter atomically with cancellation;
            // a separate flag check followed by a bare park can lose the wake.
            assert!(scheduler::chan_recv(&gate).is_none());
            assert!(scheduler::is_cancelled());
            match Session::start(vec![fixture("hold")], options()) {
                Err(failure) => assert_eq!(failure.kind, 5),
                Ok(_) => panic!("cancelled task started a process"),
            }
            crate::FAI_UNIT
        }));
        scheduler::cancel_handle(&task);
        scheduler::await_handle(&task);
        crate::FAI_UNIT
    }));
}

#[test]
fn repeated_scopes_release_every_supervisor() {
    for _ in 0..20 {
        let session = Session::start(vec![fixture("exit")], options()).unwrap();
        let shared = Arc::downgrade(&session.shared);
        while session.next().unwrap().is_some() {}
        session.close();
        session.close();
        drop(session);
        assert!(shared.upgrade().is_none());
    }
}

#[test]
#[cfg(windows)]
fn full_width_windows_exit_codes_are_not_signed_or_truncated() {
    let session = Session::start(vec![fixture("exitwide")], options()).unwrap();
    while session.next().unwrap().is_some() {}
    session.close();
    assert_eq!(lock(&session.shared.state).exits, [Some(Exit::Code(4_294_967_295))]);
}

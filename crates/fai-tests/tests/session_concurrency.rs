//! Concurrent reads over a workspace [`Session`] via [`Session::snapshot`] — the
//! primitive the daemon builds on to serve read requests in parallel. Drives real
//! snapshots on real threads and asserts: a snapshot reads identically to the
//! session it came from; many snapshots check concurrently and correctly; and a
//! read on a snapshot survives a concurrent edit (salsa cancels it; the reader
//! catches the cancellation and retries), without deadlock or torn results.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use camino::Utf8PathBuf;
use fai_driver::{DirtyFile, Session, catch_cancellation, check};
use indoc::indoc;

const CLEAN: &str = indoc! {r#"
    module M

    let x = 1
"#};
const TYPE_ERROR: &str = indoc! {r#"
    module M

    public f : Int -> Bool
    let f x = x + 1
"#};

fn workspace() -> Utf8PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).unwrap().join(format!(
        "fai-session-conc-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Utf8PathBuf, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

/// Whether `session` (or a snapshot of it) currently type-checks cleanly.
fn checks_ok(session: &Session) -> bool {
    check(session.db(), &session.select_files(None)).ok
}

#[test]
fn a_snapshot_reads_identically_to_its_session() {
    let dir = workspace();
    write(&dir, "M.fai", CLEAN);
    let session = Session::open(dir).unwrap();

    let snapshot = session.snapshot();
    // The snapshot sees the same files and the same check result as the original.
    assert_eq!(snapshot.user_files().len(), session.user_files().len());
    assert_eq!(checks_ok(&snapshot), checks_ok(&session));
    assert!(checks_ok(&snapshot));
}

#[test]
fn many_snapshots_check_concurrently_and_agree() {
    let dir = workspace();
    write(&dir, "M.fai", CLEAN);
    let session = Session::open(dir).unwrap();

    // Take N independent snapshots up front, then check them all at once: each
    // runs on its own database handle (sharing salsa storage), so this exercises
    // concurrent reads on cloned handles — the core of daemon read concurrency.
    const N: usize = 8;
    let snapshots: Vec<Session> = (0..N).map(|_| session.snapshot()).collect();
    let barrier = Arc::new(Barrier::new(N));

    let results: Vec<bool> = std::thread::scope(|scope| {
        let handles: Vec<_> = snapshots
            .into_iter()
            .map(|snapshot| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait(); // maximize overlap
                    checks_ok(&snapshot)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    assert_eq!(results.len(), N);
    assert!(results.iter().all(|&ok| ok), "every concurrent snapshot check must agree (clean)");
}

#[test]
fn reads_survive_concurrent_edits_without_deadlock() {
    // Mirror the daemon's discipline: the authoritative session is mutated under
    // a lock (the "writer"); readers briefly lock to clone a snapshot, then run
    // the check off-lock under `catch_cancellation`, retrying if a concurrent edit
    // cancels them. The test asserts liveness (all threads finish, no deadlock),
    // safety (no reader panics, every result is internally consistent), and that
    // the session reflects the final edit after the storm.
    let dir = workspace();
    write(&dir, "M.fai", CLEAN);
    let session = Arc::new(Mutex::new(Session::open(dir.clone()).unwrap()));

    const READERS: usize = 4;
    const READS_EACH: usize = 25;
    const EDITS: usize = 40;

    std::thread::scope(|scope| {
        // Readers: lock → snapshot → unlock → check (off-lock, cancellable).
        for _ in 0..READERS {
            let session = Arc::clone(&session);
            scope.spawn(move || {
                let mut done = 0;
                while done < READS_EACH {
                    let snapshot = lock(&session).snapshot();
                    // `check` runs off-lock; a concurrent edit cancels it (None).
                    match catch_cancellation(std::panic::AssertUnwindSafe(|| {
                        let files = snapshot.select_files(None);
                        check(snapshot.db(), &files)
                    })) {
                        Some(result) => {
                            // Internally consistent: ok iff there is no error.
                            let has_error = result
                                .diagnostics
                                .iter()
                                .any(|d| d.severity == fai_diagnostics::Severity::Error);
                            assert_eq!(result.ok, !has_error);
                            done += 1;
                        }
                        None => { /* cancelled by an edit; retry */ }
                    }
                }
            });
        }

        // Writer: toggle the file between clean and type-erroring content. The
        // mutation bumps the salsa revision, cancelling in-flight reader checks.
        let writer_session = Arc::clone(&session);
        scope.spawn(move || {
            let session = writer_session;
            for i in 0..EDITS {
                let content = if i % 2 == 0 { TYPE_ERROR } else { CLEAN };
                lock(&session)
                    .apply_dirty(&[DirtyFile {
                        path: "M.fai".to_owned(),
                        hash: None,
                        content: Some(content.to_owned()),
                    }])
                    .unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
            // Leave a known final state for the post-storm assertion.
            lock(&session)
                .apply_dirty(&[DirtyFile {
                    path: "M.fai".to_owned(),
                    hash: None,
                    content: Some(CLEAN.to_owned()),
                }])
                .unwrap();
        });
    });

    // The scope joined every thread (no deadlock), and the final edit is visible.
    assert!(checks_ok(&lock(&session)), "the session must reflect the final clean edit");
}

#[test]
fn a_pending_edit_cancels_an_inflight_snapshot_read() {
    // Directly exercise salsa's cancel-on-write, deterministically. The reader
    // holds one live snapshot and runs checks in a loop; the writer mutates the
    // authoritative session, which sets salsa's cancellation flag and then blocks
    // until the snapshot drops. While the writer is blocked the flag stays set, so
    // the reader's next check is guaranteed to observe a cancellation (`None`); it
    // then breaks, dropping the snapshot, which lets the writer complete. No
    // timeouts: termination is guaranteed by the protocol, not by timing.
    let dir = workspace();
    write(&dir, "M.fai", CLEAN);
    let session = Arc::new(Mutex::new(Session::open(dir).unwrap()));
    let ready = Arc::new(Barrier::new(2));

    let observed_cancellation = std::thread::scope(|scope| {
        let reader = {
            let session = Arc::clone(&session);
            let ready = Arc::clone(&ready);
            scope.spawn(move || {
                let snapshot = lock(&session).snapshot();
                ready.wait(); // tell the writer a snapshot is live
                // Loop until a check is cancelled by the pending edit. The writer
                // sets the flag right after the barrier and waits for us, so the
                // flag is observed within a bounded number of iterations.
                loop {
                    let cancelled = catch_cancellation(std::panic::AssertUnwindSafe(|| {
                        let files = snapshot.select_files(None);
                        let _ = check(snapshot.db(), &files);
                    }))
                    .is_none();
                    if cancelled {
                        break true;
                    }
                }
                // `snapshot` drops here, releasing the writer waiting on us.
            })
        };

        ready.wait(); // the reader holds a snapshot
        // Mutate the authoritative session: salsa sets the cancellation flag, then
        // waits for the reader's snapshot to drop. The reader observes the flag and
        // drops, so this returns.
        lock(&session)
            .apply_dirty(&[DirtyFile {
                path: "M.fai".to_owned(),
                hash: None,
                content: Some(TYPE_ERROR.to_owned()),
            }])
            .unwrap();

        reader.join().unwrap()
    });

    assert!(observed_cancellation, "an edit must cancel the in-flight snapshot read");
    assert!(!checks_ok(&lock(&session)), "the cancelling edit (a type error) is now visible");
}

/// Locks the shared session, recovering from a poisoned lock (a reader assertion
/// failure should surface as that thread's panic, not as lock poisoning noise).
fn lock(session: &Mutex<Session>) -> std::sync::MutexGuard<'_, Session> {
    session.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

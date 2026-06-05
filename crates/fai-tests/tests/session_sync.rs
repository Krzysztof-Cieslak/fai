//! Incremental file-state sync on the workspace [`Session`] — the bookkeeping the
//! daemon relies on to keep its warm database current. Drives real files on disk
//! and asserts add / edit / delete / touch and the dirty-set fast path are
//! reflected (or correctly ignored) by `select_files` and `check`.

use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use fai_driver::{DirtyFile, Session, check};

const CLEAN: &str = "module Bad\n\nlet x = 1\n";
const TYPE_ERROR: &str = "module Bad\n\npublic f : Int -> Bool\nlet f x = x + 1\n";

fn workspace() -> Utf8PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = Utf8PathBuf::from_path_buf(std::env::temp_dir()).unwrap().join(format!(
        "fai-session-sync-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Utf8PathBuf, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

/// Whether the whole workspace currently type-checks.
fn checks_ok(session: &Session) -> bool {
    check(session.db(), &session.select_files(None)).ok
}

#[test]
fn new_file_is_picked_up() {
    let dir = workspace();
    write(&dir, "A.fai", "module A\n\nlet a = 1\n");
    let mut session = Session::open(dir.clone()).unwrap();
    assert_eq!(session.user_files().len(), 1);

    write(&dir, "B.fai", "module B\n\nlet b = 2\n");
    session.sync_from_disk().unwrap();
    assert_eq!(session.user_files().len(), 2);
    assert!(checks_ok(&session));
}

#[test]
fn edit_is_reflected() {
    let dir = workspace();
    write(&dir, "Bad.fai", CLEAN);
    let mut session = Session::open(dir.clone()).unwrap();
    assert!(checks_ok(&session));

    write(&dir, "Bad.fai", TYPE_ERROR);
    session.sync_from_disk().unwrap();
    assert!(!checks_ok(&session), "the type error introduced on disk must be seen");
}

#[test]
fn delete_is_dropped_from_selection() {
    let dir = workspace();
    write(&dir, "A.fai", "module A\n\nlet a = 1\n");
    write(&dir, "B.fai", "module B\n\nlet b = 2\n");
    let mut session = Session::open(dir.clone()).unwrap();
    assert_eq!(session.user_files().len(), 2);

    std::fs::remove_file(dir.join("B.fai")).unwrap();
    session.sync_from_disk().unwrap();
    let paths: Vec<String> =
        session.user_files().iter().map(|f| f.path(session.db()).to_owned()).collect();
    assert_eq!(paths, vec!["A.fai".to_owned()], "the deleted file must leave the live set");
}

#[test]
fn rewriting_identical_content_is_harmless() {
    let dir = workspace();
    write(&dir, "Bad.fai", CLEAN);
    let mut session = Session::open(dir.clone()).unwrap();
    assert!(checks_ok(&session));

    // Rewrite byte-identical content (a `touch`-like change): still clean.
    write(&dir, "Bad.fai", CLEAN);
    session.sync_from_disk().unwrap();
    assert!(checks_ok(&session));
    assert_eq!(session.user_files().len(), 1);
}

#[test]
fn dirty_set_inline_content_overrides_the_input() {
    let dir = workspace();
    write(&dir, "Bad.fai", CLEAN);
    let mut session = Session::open(dir.clone()).unwrap();
    assert!(checks_ok(&session));

    // The client declares the file changed and supplies the new content directly.
    session
        .apply_dirty(&[DirtyFile {
            path: "Bad.fai".to_owned(),
            hash: None,
            content: Some(TYPE_ERROR.to_owned()),
        }])
        .unwrap();
    assert!(!checks_ok(&session), "inline dirty content must update the database");
}

#[test]
fn dirty_set_without_content_rereads_disk() {
    let dir = workspace();
    write(&dir, "Bad.fai", CLEAN);
    let mut session = Session::open(dir.clone()).unwrap();
    assert!(checks_ok(&session));

    // Disk changed; the client points at the path without inline content.
    write(&dir, "Bad.fai", TYPE_ERROR);
    session
        .apply_dirty(&[DirtyFile { path: "Bad.fai".to_owned(), hash: None, content: None }])
        .unwrap();
    assert!(!checks_ok(&session), "a content-less dirty entry must re-read disk");
}

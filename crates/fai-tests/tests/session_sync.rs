//! Incremental file-state sync on the workspace [`Session`] — the bookkeeping the
//! daemon relies on to keep its warm database current. Drives real files on disk
//! and asserts add / edit / delete / touch and the dirty-set fast path are
//! reflected (or correctly ignored) by `select_files` and `check`.

use std::sync::atomic::{AtomicU64, Ordering};

use camino::Utf8PathBuf;
use fai_driver::{DirtyFile, Session, check};
use indoc::indoc;

const CLEAN: &str = indoc! {r#"
    module Bad

    let x = 1
"#};
const TYPE_ERROR: &str = indoc! {r#"
    module Bad

    public f : Int -> Bool
    let f x = x + 1
"#};

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

const CLIENT: &str = "module A\npublic answer : Int\nlet answer = B.value\n";
const PROVIDER: &str = "module B\npublic value : Int\nlet value = 7\n";

fn check_snapshot(session: &Session) -> (bool, Vec<String>) {
    let result = check(session.db(), &session.select_files(None));
    let mut diagnostics: Vec<_> = result
        .diagnostics
        .iter()
        .map(|diagnostic| {
            let file = session.db().source_file(diagnostic.primary.source()).unwrap();
            format!(
                "{}:{}:{:?}:{}",
                file.path(session.db()),
                diagnostic.code.as_str(),
                diagnostic.primary.range(),
                diagnostic.message
            )
        })
        .collect();
    diagnostics.sort();
    (result.ok, diagnostics)
}

#[track_caller]
fn assert_matches_clean(session: &Session) {
    let clean = Session::open(session.root().to_owned()).unwrap();
    assert_eq!(check_snapshot(session), check_snapshot(&clean));
}

#[test]
fn adding_a_missing_dependency_clears_cached_errors() {
    let dir = workspace();
    write(&dir, "A.fai", CLIENT);
    let mut session = Session::open(dir.clone()).unwrap();
    assert!(!checks_ok(&session));
    write(&dir, "B.fai", PROVIDER);
    session.sync_from_disk().unwrap();
    assert!(checks_ok(&session));
    assert_matches_clean(&session);
}

#[test]
fn deleting_a_referenced_module_invalidates_resolution() {
    let dir = workspace();
    write(&dir, "A.fai", CLIENT);
    write(&dir, "B.fai", PROVIDER);
    let mut session = Session::open(dir.clone()).unwrap();
    assert!(checks_ok(&session));
    std::fs::remove_file(dir.join("B.fai")).unwrap();
    session.sync_from_disk().unwrap();
    assert!(!checks_ok(&session));
    assert_matches_clean(&session);
}

#[test]
fn renaming_a_module_does_not_leave_a_phantom_duplicate() {
    let dir = workspace();
    write(&dir, "A.fai", CLIENT);
    write(&dir, "B.fai", PROVIDER);
    let mut session = Session::open(dir.clone()).unwrap();
    assert!(checks_ok(&session));
    std::fs::rename(dir.join("B.fai"), dir.join("Renamed.fai")).unwrap();
    session.sync_from_disk().unwrap();
    let module = fai_resolve::module_file(
        session.db(),
        fai_resolve::ModuleName(fai_syntax::Symbol::intern("B")),
    )
    .unwrap();
    assert_eq!(module.path(session.db()), "Renamed.fai");
    assert!(checks_ok(&session));
    assert_matches_clean(&session);
}

#[test]
fn duplicate_module_membership_changes_match_clean_checks() {
    let dir = workspace();
    write(&dir, "A.fai", CLIENT);
    write(&dir, "B.fai", PROVIDER);
    let mut session = Session::open(dir.clone()).unwrap();
    assert!(checks_ok(&session));
    write(&dir, "Duplicate.fai", PROVIDER);
    session.sync_from_disk().unwrap();
    assert!(!checks_ok(&session));
    assert_matches_clean(&session);
    std::fs::remove_file(dir.join("Duplicate.fai")).unwrap();
    session.sync_from_disk().unwrap();
    assert!(checks_ok(&session));
    assert_matches_clean(&session);
}

#[test]
fn deleted_module_can_be_readded_with_its_stable_id() {
    let dir = workspace();
    write(&dir, "A.fai", CLIENT);
    write(&dir, "B.fai", PROVIDER);
    let mut session = Session::open(dir.clone()).unwrap();
    assert!(checks_ok(&session));
    let name = fai_resolve::ModuleName(fai_syntax::Symbol::intern("B"));
    let original = fai_resolve::module_file(session.db(), name).unwrap().source(session.db());
    std::fs::remove_file(dir.join("B.fai")).unwrap();
    session.sync_from_disk().unwrap();
    assert!(!checks_ok(&session));
    write(&dir, "B.fai", PROVIDER);
    session.sync_from_disk().unwrap();
    assert!(checks_ok(&session));
    assert_eq!(
        fai_resolve::module_file(session.db(), name).unwrap().source(session.db()),
        original
    );
    assert_matches_clean(&session);
}

#[test]
fn new_file_is_picked_up() {
    let dir = workspace();
    write(
        &dir,
        "A.fai",
        indoc! {r#"
            module A

            let a = 1
        "#},
    );
    let mut session = Session::open(dir.clone()).unwrap();
    assert_eq!(session.user_files().len(), 1);

    write(
        &dir,
        "B.fai",
        indoc! {r#"
            module B

            let b = 2
        "#},
    );
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
    write(
        &dir,
        "A.fai",
        indoc! {r#"
            module A

            let a = 1
        "#},
    );
    write(
        &dir,
        "B.fai",
        indoc! {r#"
            module B

            let b = 2
        "#},
    );
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

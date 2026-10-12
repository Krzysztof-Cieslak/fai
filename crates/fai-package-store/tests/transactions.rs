//! Compare-and-replace metadata updates retain independent user edits after interruption.

use camino::Utf8PathBuf;
use fai_package_store::{FileEdit, MetadataTransaction, atomic_write};

fn directory() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_owned()).unwrap();
    (temp, root)
}

#[test]
fn transaction_publishes_manifest_and_lock_together() {
    let (_temp, root) = directory();
    std::fs::write(root.join("fai.toml"), "before").unwrap();
    MetadataTransaction::new(
        vec![
            FileEdit::new("fai.toml".into(), Some("before"), "after".into()),
            FileEdit::new("fai.lock".into(), None, "lock".into()),
        ],
        Vec::new(),
    )
    .commit(&root)
    .unwrap();
    assert_eq!(std::fs::read_to_string(root.join("fai.toml")).unwrap(), "after");
    assert_eq!(std::fs::read_to_string(root.join("fai.lock")).unwrap(), "lock");
    assert!(!root.join(".fai/transaction.json").exists());
}

#[test]
fn concurrent_manifest_edits_prevent_every_prepared_write() {
    let (_temp, root) = directory();
    std::fs::write(root.join("fai.toml"), "user edit").unwrap();
    let result = MetadataTransaction::new(
        vec![
            FileEdit::new("fai.lock".into(), None, "lock".into()),
            FileEdit::new("fai.toml".into(), Some("before"), "after".into()),
        ],
        Vec::new(),
    )
    .commit(&root);
    assert_eq!(result.unwrap_err().code, fai_package::INPUTS_CHANGED);
    assert!(!root.join("fai.lock").exists());
    assert_eq!(std::fs::read_to_string(root.join("fai.toml")).unwrap(), "user edit");
}

#[test]
fn recovery_finishes_a_partially_applied_transaction_once() {
    let (_temp, root) = directory();
    std::fs::write(root.join("fai.toml"), "after").unwrap();
    let transaction = MetadataTransaction::new(
        vec![
            FileEdit::new("fai.toml".into(), Some("before"), "after".into()),
            FileEdit::new("fai.lock".into(), None, "lock".into()),
        ],
        Vec::new(),
    );
    atomic_write(&root.join(".fai/transaction.json"), &serde_json::to_vec(&transaction).unwrap())
        .unwrap();
    MetadataTransaction::recover(&root).unwrap();
    MetadataTransaction::recover(&root).unwrap();
    assert_eq!(std::fs::read_to_string(root.join("fai.lock")).unwrap(), "lock");
}

#[test]
fn recovery_preserves_a_conflicting_user_edit() {
    let (_temp, root) = directory();
    std::fs::write(root.join("fai.toml"), "independent edit").unwrap();
    let transaction = MetadataTransaction::new(
        vec![FileEdit::new("fai.toml".into(), Some("before"), "after".into())],
        Vec::new(),
    );
    atomic_write(&root.join(".fai/transaction.json"), &serde_json::to_vec(&transaction).unwrap())
        .unwrap();
    assert_eq!(MetadataTransaction::recover(&root).unwrap_err().code, fai_package::INPUTS_CHANGED);
    assert_eq!(std::fs::read_to_string(root.join("fai.toml")).unwrap(), "independent edit");
}

#[test]
fn transaction_destinations_cannot_escape_the_project() {
    let (_temp, root) = directory();
    let result = MetadataTransaction::new(
        vec![FileEdit::new("../fai.toml".into(), None, "after".into())],
        Vec::new(),
    )
    .commit(&root);
    assert!(result.is_err());
}

#[test]
fn competing_transactions_have_one_successful_compare_and_replace() {
    let (_temp, root) = directory();
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            MetadataTransaction::new(
                vec![FileEdit::new("fai.lock".into(), None, "first".into())],
                Vec::new(),
            )
            .commit(&root)
        });
        let second = scope.spawn(|| {
            barrier.wait();
            MetadataTransaction::new(
                vec![FileEdit::new("fai.lock".into(), None, "second".into())],
                Vec::new(),
            )
            .commit(&root)
        });
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_ne!(results.0.is_ok(), results.1.is_ok());
}

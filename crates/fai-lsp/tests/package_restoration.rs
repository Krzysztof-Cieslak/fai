//! A held metadata lock cannot block the editor's request loop during restoration.

use lsp_server::{Connection, Message, Notification, Request};
use serde_json::json;
use std::time::Duration;

fn response(connection: &Connection, id: i32) -> lsp_server::Response {
    loop {
        if let Message::Response(response) =
            connection.receiver.recv_timeout(Duration::from_secs(30)).unwrap()
            && response.id == id.into()
        {
            return response;
        }
    }
}

#[test]
fn editor_requests_are_served_while_dependency_publication_waits() {
    let temporary = tempfile::tempdir().unwrap();
    let root = camino::Utf8PathBuf::from_path_buf(temporary.path().to_owned()).unwrap();
    std::fs::create_dir(root.join(".fai")).unwrap();
    std::fs::write(
        root.join("fai.toml"),
        "manifest-version = 1\n[package]\nname = 'app'\nversion = '1.0.0'\n",
    )
    .unwrap();
    let held = std::fs::File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(root.join(".fai/metadata.lock"))
        .unwrap();
    held.lock().unwrap();
    let (server, client) = Connection::memory();
    let worker = std::thread::spawn(move || fai_lsp::serve(&server, root).unwrap());
    client
        .sender
        .send(Message::Request(Request::new(
            1.into(),
            "initialize".into(),
            json!({"capabilities": {}}),
        )))
        .unwrap();
    assert!(response(&client, 1).error.is_none());
    client
        .sender
        .send(Message::Notification(Notification::new("initialized".into(), json!({}))))
        .unwrap();
    client
        .sender
        .send(Message::Request(Request::new(
            2.into(),
            "workspace/symbol".into(),
            json!({"query": ""}),
        )))
        .unwrap();
    assert!(response(&client, 2).error.is_none());
    drop(held);
    loop {
        if let Message::Notification(note) =
            client.receiver.recv_timeout(Duration::from_secs(30)).unwrap()
            && note.method == "$/fai/projectStatus"
            && note.params["state"] == "ready"
        {
            break;
        }
    }
    client
        .sender
        .send(Message::Request(Request::new(3.into(), "shutdown".into(), serde_json::Value::Null)))
        .unwrap();
    assert!(response(&client, 3).error.is_none());
    client
        .sender
        .send(Message::Notification(Notification::new("exit".into(), serde_json::Value::Null)))
        .unwrap();
    worker.join().unwrap();
}

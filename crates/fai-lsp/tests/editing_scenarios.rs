//! Real-world editing-fidelity scenarios: incremental sync (insert/delete/
//! replace/multi-edit/multi-line), didSave/didClose, dependent diagnostics across
//! several open files, and the negotiated position encoding.

mod harness;

use harness::{Harness, pos, position_of, position_of_utf8, range};
use serde_json::json;

/// The formatted (canonical) text of the current buffer — a robust way to read
/// back what an incremental edit produced.
fn buffer(h: &mut Harness, uri: &str) -> String {
    let edits = h.formatting(uri);
    edits
        .as_array()
        .unwrap()
        .first()
        .map_or(String::new(), |e| e["newText"].as_str().unwrap_or_default().to_owned())
}

#[test]
fn incremental_insert_adds_text() {
    let src = "module M\n\npublic n : Int\nlet n = 1\n";
    let (mut h, uri) = Harness::open_main("ed-insert", src);
    // Insert ` + 1` just after the `1` on line 3.
    h.did_change_range(&uri, range(pos(3, 9), pos(3, 9)), " + 1");
    assert!(h.diagnostics(&uri).is_empty());
    assert!(buffer(&mut h, &uri).contains("let n = 1 + 1"), "{}", buffer(&mut h, &uri));
    h.shutdown();
}

#[test]
fn incremental_delete_removes_text() {
    let src = "module M\n\npublic n : Int\nlet n = 1 + 1\n";
    let (mut h, uri) = Harness::open_main("ed-delete", src);
    // Delete ` + 1` (line 3, cols 9..13).
    h.did_change_range(&uri, range(pos(3, 9), pos(3, 13)), "");
    assert!(buffer(&mut h, &uri).contains("let n = 1\n"), "{}", buffer(&mut h, &uri));
    h.shutdown();
}

#[test]
fn incremental_replace_swaps_text() {
    let src = "module M\n\npublic n : Int\nlet n = 1\n";
    let (mut h, uri) = Harness::open_main("ed-replace", src);
    // Replace `1` (line 3, cols 8..9) with `42`.
    h.did_change_range(&uri, range(pos(3, 8), pos(3, 9)), "42");
    assert!(buffer(&mut h, &uri).contains("let n = 42"), "{}", buffer(&mut h, &uri));
    h.shutdown();
}

#[test]
fn multiple_changes_in_one_notification_apply_in_order() {
    let src = "module M\n\npublic a : Int\nlet a = 1\n\npublic b : Int\nlet b = 2\n";
    let (mut h, uri) = Harness::open_main("ed-multi", src);
    h.notify(
        "textDocument/didChange",
        json!({
            "textDocument": { "uri": uri, "version": 2 },
            "contentChanges": [
                { "range": range(pos(3, 8), pos(3, 9)), "text": "10" },
                { "range": range(pos(6, 8), pos(6, 9)), "text": "20" }
            ]
        }),
    );
    let text = buffer(&mut h, &uri);
    assert!(text.contains("let a = 10") && text.contains("let b = 20"), "{text}");
    h.shutdown();
}

#[test]
fn multi_line_incremental_replace() {
    let src = "module M\n\npublic f : Int -> Int\nlet f x =\n  let y = x\n  y\n";
    let (mut h, uri) = Harness::open_main("ed-multiline", src);
    // Replace the two-line body (lines 4..6) with a one-line body.
    h.did_change_range(&uri, range(pos(4, 0), pos(6, 0)), "  x\n");
    assert!(h.diagnostics(&uri).is_empty(), "{:?}", h.diagnostics(&uri));
    assert!(!buffer(&mut h, &uri).contains("let y"), "{}", buffer(&mut h, &uri));
    h.shutdown();
}

#[test]
fn an_empty_change_list_is_a_no_op() {
    let src = "module M\n\npublic n : Int\nlet n = 1\n";
    let (mut h, uri) = Harness::open_main("ed-noop", src);
    h.notify(
        "textDocument/didChange",
        json!({ "textDocument": { "uri": uri, "version": 2 }, "contentChanges": [] }),
    );
    assert!(h.diagnostics(&uri).is_empty(), "still clean and not crashed");
    h.shutdown();
}

#[test]
fn incremental_edit_then_hover_reflects_the_new_text() {
    let src = "module M\n\npublic n : Int\nlet n = 1\n";
    let (mut h, uri) = Harness::open_main("ed-hover", src);
    // Append a new binding `let m = n` at the end of the buffer.
    h.did_change_range(&uri, range(pos(4, 0), pos(4, 0)), "\npublic m : Int\nlet m = n\n");
    let text = h.hover_text(&uri, pos(6, 8)).expect("hover on the new `n` use");
    assert!(text.contains("Int"), "{text}");
    h.shutdown();
}

#[test]
fn did_save_keeps_diagnostics_current() {
    let broken = "module M\n\npublic inc : Int -> Bool\nlet inc x = x + 1\n";
    let (mut h, uri) = Harness::open_main("ed-save", broken);
    h.did_save(&uri);
    assert!(!h.diagnostics(&uri).is_empty(), "save re-checks the buffer");
    h.shutdown();
}

#[test]
fn did_close_clears_a_files_diagnostics() {
    let broken = "module M\n\npublic inc : Int -> Bool\nlet inc x = x + 1\n";
    let mut h = Harness::start("ed-close", &[("Main.fai", broken)]);
    let uri = h.did_open("Main.fai", broken);
    assert!(!h.diagnostics(&uri).is_empty(), "broken while open");
    h.did_close(&uri);
    assert!(h.diagnostics(&uri).is_empty(), "closing clears the published diagnostics");
    h.shutdown();
}

#[test]
fn reopening_a_closed_file_works() {
    let src = "module M\n\npublic inc : Int -> Int\nlet inc x = x + 1\n\npublic two : Int\nlet two = inc 1\n";
    let mut h = Harness::start("ed-reopen", &[("Main.fai", src)]);
    let uri = h.did_open("Main.fai", src);
    h.did_close(&uri);
    let uri = h.did_open("Main.fai", src);
    let text = h.hover_text(&uri, position_of(src, "inc 1")).expect("hover after reopen");
    assert!(text.contains("inc : Int -> Int"), "{text}");
    h.shutdown();
}

#[test]
fn editing_a_dependency_refreshes_every_open_dependent() {
    // A is used by B; C is independent. Breaking A must refresh B (now broken)
    // and re-publish C (still clean).
    let a = "module A\n\npublic n : Int\nlet n = 0\n";
    let b = "module B\n\npublic m : Int\nlet m = A.n + 1\n";
    let c = "module C\n\npublic k : Int\nlet k = 7\n";
    let mut h = Harness::start("ed-chain", &[("A.fai", a), ("B.fai", b), ("C.fai", c)]);
    let uri_a = h.did_open("A.fai", a);
    let uri_b = h.did_open("B.fai", b);
    let uri_c = h.did_open("C.fai", c);
    assert!(h.diagnostics(&uri_b).is_empty() && h.diagnostics(&uri_c).is_empty());
    // Retype A.n to Bool.
    h.did_change(&uri_a, "module A\n\npublic n : Bool\nlet n = true\n");
    assert!(!h.diagnostics(&uri_b).is_empty(), "B breaks");
    assert!(h.diagnostics(&uri_c).is_empty(), "C is unaffected but still refreshed");
    h.shutdown();
}

#[test]
fn closing_a_dependency_reverts_it_for_open_dependents() {
    let disk_a = "module A\n\npublic n : Int\nlet n = 0\n";
    let disk_b = "module B\n\npublic m : Int\nlet m = A.n + 1\n";
    let mut h = Harness::start("ed-close-dep", &[("A.fai", disk_a), ("B.fai", disk_b)]);
    let uri_a = h.did_open("A.fai", disk_a);
    let uri_b = h.did_open("B.fai", disk_b);
    // Unsaved edit to A breaks B.
    h.did_change(&uri_a, "module A\n\npublic n : Bool\nlet n = true\n");
    assert!(!h.diagnostics(&uri_b).is_empty(), "B sees A's unsaved edit");
    // Closing A reverts it to the on-disk `Int`, so B is valid again.
    h.did_close(&uri_a);
    assert!(h.diagnostics(&uri_b).is_empty(), "closing A restores B");
    h.shutdown();
}

#[test]
fn utf8_position_encoding_locates_tokens_after_a_multibyte_char() {
    // With UTF-8 negotiated, positions are byte columns. A token after a
    // multibyte string literal is found only if the encoding is honored.
    let src = "module M\n\npublic pair : String * Int\nlet pair = (\"té\", 1)\n";
    let (mut h, _init) = Harness::start_with_caps(
        "ed-utf8",
        &[("Main.fai", src)],
        json!({ "general": { "positionEncodings": ["utf-8"] } }),
    );
    let uri = h.did_open("Main.fai", src);
    // The `1` sits after `"té"`, whose `é` is one UTF-16 unit but two UTF-8 bytes.
    let text = h.hover_text(&uri, position_of_utf8(src, "1)")).expect("hover under utf-8");
    assert!(text.contains("Int"), "{text}");
    h.shutdown();
}

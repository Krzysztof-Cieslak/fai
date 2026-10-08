//! Requests always receive a terminal JSON-RPC response, including bad input.

mod harness;
use harness::Harness;
use lsp_server::{ErrorCode, RequestId};
use serde_json::{Value, json};

#[track_caller]
fn invalid_params(method: &str, params: Value) {
    let mut harness = Harness::start("invalid-params", &[]);
    let id = RequestId::from("bad-请求".to_owned());
    let response = harness.raw_request(id.clone(), method, params);
    assert_eq!(response.id, id);
    assert!(response.result.is_none());
    let error = response.error.expect("invalid parameters must be reported");
    assert_eq!(error.code, ErrorCode::InvalidParams as i32);
    assert!(error.message.contains(method), "{}", error.message);
    assert!(harness.request("workspace/symbol", json!({"query": "__absent__"})).is_array());
    harness.shutdown();
}

#[test]
fn missing_hover_parameters_are_reported() {
    invalid_params("textDocument/hover", Value::Null);
}
#[test]
fn malformed_definition_parameters_are_reported() {
    invalid_params("textDocument/definition", json!({}));
}
#[test]
fn malformed_formatting_parameters_are_reported() {
    invalid_params("textDocument/formatting", json!({}));
}
#[test]
fn malformed_range_formatting_parameters_are_reported() {
    invalid_params("textDocument/rangeFormatting", json!({}));
}
#[test]
fn malformed_on_type_formatting_parameters_are_reported() {
    invalid_params("textDocument/onTypeFormatting", json!({}));
}
#[test]
fn malformed_document_symbol_parameters_are_reported() {
    invalid_params("textDocument/documentSymbol", json!({}));
}
#[test]
fn malformed_workspace_symbol_parameters_are_reported() {
    invalid_params("workspace/symbol", json!({}));
}
#[test]
fn malformed_reference_parameters_are_reported() {
    invalid_params("textDocument/references", json!({}));
}
#[test]
fn malformed_prepare_rename_parameters_are_reported() {
    invalid_params("textDocument/prepareRename", json!({}));
}
#[test]
fn malformed_rename_parameters_are_reported() {
    invalid_params("textDocument/rename", json!({}));
}
#[test]
fn malformed_completion_parameters_are_reported() {
    invalid_params("textDocument/completion", json!({}));
}
#[test]
fn malformed_completion_resolution_parameters_are_reported() {
    invalid_params("completionItem/resolve", json!({}));
}
#[test]
fn malformed_signature_help_parameters_are_reported() {
    invalid_params("textDocument/signatureHelp", json!({}));
}
#[test]
fn malformed_code_action_parameters_are_reported() {
    invalid_params("textDocument/codeAction", json!({}));
}
#[test]
fn malformed_inlay_hint_parameters_are_reported() {
    invalid_params("textDocument/inlayHint", json!({}));
}
#[test]
fn malformed_semantic_token_parameters_are_reported() {
    invalid_params("textDocument/semanticTokens/full", json!({}));
}

#[test]
fn negative_position_is_an_invalid_parameter() {
    invalid_params(
        "textDocument/hover",
        json!({"textDocument": {"uri": "file:///Main.fai"}, "position": {"line": -1, "character": 0}}),
    );
}

#[test]
fn malformed_shutdown_leaves_the_session_usable() {
    invalid_params("shutdown", json!({"unexpected": true}));
}

#[test]
fn unknown_method_returns_method_not_found_with_its_numeric_id() {
    let mut harness = Harness::start("unknown-request", &[]);
    let response = harness.raw_request((-17).into(), "unknown/request", json!({}));
    assert_eq!(response.id, (-17).into());
    assert!(response.result.is_none());
    let error = response.error.expect("unknown methods must be reported");
    assert_eq!(error.code, ErrorCode::MethodNotFound as i32);
    assert_eq!(error.message, "unsupported method: unknown/request");
    assert!(harness.request("workspace/symbol", json!({"query": "__absent__"})).is_array());
    harness.shutdown();
}

#[test]
fn unknown_and_malformed_notifications_leave_the_session_usable() {
    let mut harness = Harness::start("unknown-notification", &[]);
    harness.notify("unknown/notification", json!({}));
    harness.notify("textDocument/didOpen", Value::Null);
    assert!(harness.request("workspace/symbol", json!({"query": "__absent__"})).is_array());
    harness.shutdown();
}

//! Golden file for `tools/list`: any change to names, schemas or annotations is deliberate and reviewed.

mod support;

use oomtop_core::provider::StaticProvider;
use oomtop_core::Snapshot;
use oomtop_mcp::{tool_definitions, Server};
use serde_json::json;

#[test]
fn tool_definitions_golden() {
    insta::assert_json_snapshot!("tools_with_actions", tool_definitions(true));
    let ro = tool_definitions(false);
    assert_eq!(ro.len(), 8);
    assert!(ro.iter().all(|t| t["annotations"]["readOnlyHint"] == true));
    let rw = tool_definitions(true);
    let reclaim = rw.iter().find(|t| t["name"] == "reclaim").unwrap();
    assert_eq!(reclaim["annotations"]["destructiveHint"], true);
    assert_eq!(reclaim["annotations"]["readOnlyHint"], false);
}

#[test]
fn can_fit_without_measurements_reports_an_error_answer() {
    // No memory data at all: the answer is "cannot measure" (exit code 1), never a guess.
    let mut opts = support::options(false, None);
    opts.provider = Box::new(StaticProvider::new(Snapshot::default()));
    let mut sv = Server::new(opts);
    sv.handle(
        &json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    );
    let r = sv
        .handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"can_fit","arguments":{"size":"1G"}}}))
        .unwrap();
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["exit_code"], 1, "{sc}");
    assert!(sc["error"].is_string());
    assert!(sc["summary"].as_str().unwrap().starts_with("Cannot answer"));
    support::assert_conforms(&tool_definitions(false), "can_fit", &r["result"]);
}

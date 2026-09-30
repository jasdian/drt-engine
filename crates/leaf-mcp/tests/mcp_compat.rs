//! Acceptance criterion 4: leaf plugins are REAL MCP stdio servers — proven
//! by driving leaf-wrap with a stock rmcp client (no leaf-* client code).

mod common;

use common::{leaf_wrap_bin, write_jq_manifest};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::child_process::{ConfigureCommandExt, TokioChildProcess};

#[tokio::test]
async fn stock_rmcp_client_lists_and_calls_the_wrapped_tool() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = write_jq_manifest(dir.path());

    let cmd = tokio::process::Command::new(leaf_wrap_bin()).configure(|c| {
        c.args(["--manifest", manifest_path.to_str().unwrap()]);
    });
    let running =
        ().serve(TokioChildProcess::new(cmd).unwrap())
            .await
            .expect("stock client initializes against leaf-wrap");

    let tools = running.peer().list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert!(
        names.contains(&"jq"),
        "tool list must contain jq, got {names:?}"
    );
    assert!(
        names.contains(&"__leaf_manifest"),
        "tool list must contain the determinism declaration tool, got {names:?}"
    );
    let jq_tool = tools.iter().find(|t| t.name == "jq").unwrap();
    assert!(
        jq_tool
            .description
            .as_deref()
            .unwrap_or_default()
            .contains("deterministic: true"),
        "human-readable determinism note missing"
    );

    // Call the wrapped tool like any MCP client would.
    let args = serde_json::json!({"expr": ".a", "input": "{\"a\": 1}"});
    let result = running
        .peer()
        .call_tool(
            CallToolRequestParams::new("jq").with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap();
    assert_ne!(result.is_error, Some(true));
    let text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();
    assert_eq!(text, "1\n");

    // And read the machine-readable determinism declaration.
    let manifest = running
        .peer()
        .call_tool(CallToolRequestParams::new("__leaf_manifest"))
        .await
        .unwrap();
    let text: String = manifest
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["determinism"], "deterministic");
    assert!(v.get("env").is_none(), "env must be redacted");

    running.cancel().await.unwrap();
}

#[tokio::test]
async fn tool_error_surfaces_as_tool_error_not_protocol_failure() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = write_jq_manifest(dir.path());
    let cmd = tokio::process::Command::new(leaf_wrap_bin()).configure(|c| {
        c.args(["--manifest", manifest_path.to_str().unwrap()]);
    });
    let running = ().serve(TokioChildProcess::new(cmd).unwrap()).await.unwrap();

    // Invalid jq program: the tool fails, the protocol must not.
    let args = serde_json::json!({"expr": "((", "input": "{}"});
    let result = running
        .peer()
        .call_tool(
            CallToolRequestParams::new("jq").with_arguments(args.as_object().cloned().unwrap()),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    running.cancel().await.unwrap();
}

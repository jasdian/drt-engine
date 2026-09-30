//! Acceptance criterion 7: a non-allowlisted binary is refused — at the
//! leaf-wrap server (won't serve it) and at the client transport (won't
//! spawn it).

mod common;

use std::collections::BTreeMap;

use common::{find_bin, leaf_wrap_bin, write_jq_manifest};
use leaf_mcp::StdioMcpTransport;
use leaf_traits::{DeterminismClass, LeafSpec, Transport, TransportError};

#[tokio::test]
async fn leaf_wrap_refuses_manifest_whose_binary_is_not_allowlisted() {
    let dir = tempfile::tempdir().unwrap();
    let cat = find_bin("cat");
    let jq = find_bin("jq");
    // Binary is `cat`, allowlist only holds `jq`.
    let manifest = serde_json::json!({
        "name": "cat",
        "description": "not allowed",
        "determinism": "deterministic",
        "binary": cat,
        "allowlist": [jq],
        "args_template": [],
        "timeout_ms": 1000,
        "workdir": dir.path().join("jail"),
        "env": {},
        "input_schema": {"type": "object", "properties": {}}
    });
    let path = dir.path().join("cat.json");
    std::fs::write(&path, manifest.to_string()).unwrap();

    let out = tokio::process::Command::new(leaf_wrap_bin())
        .args(["--manifest", path.to_str().unwrap()])
        .output()
        .await
        .unwrap();
    assert!(
        !out.status.success(),
        "leaf-wrap must refuse a non-allowlisted binary"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("allowlist"),
        "refusal must name the allowlist, got: {stderr}"
    );
}

#[tokio::test]
async fn client_refuses_non_allowlisted_server_binary() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = write_jq_manifest(dir.path());
    let spec = LeafSpec {
        id: "jq".into(),
        command: leaf_wrap_bin(),
        args: vec!["--manifest".into(), manifest_path.to_str().unwrap().into()],
        env: BTreeMap::new(),
        cwd: None,
        call_timeout_ms: 10_000,
        expected_determinism: DeterminismClass::Deterministic,
    };
    // Empty runtime allowlist: nothing may be spawned.
    let transport = StdioMcpTransport {
        server_allowlist: vec![],
    };
    let err = transport.connect(&spec).await.unwrap_err();
    assert!(matches!(err, TransportError::NotAllowlisted(_)));
}

#[tokio::test]
async fn client_detects_determinism_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = write_jq_manifest(dir.path());
    let spec = LeafSpec {
        id: "jq".into(),
        command: leaf_wrap_bin(),
        args: vec!["--manifest".into(), manifest_path.to_str().unwrap().into()],
        env: BTreeMap::new(),
        cwd: None,
        call_timeout_ms: 10_000,
        // The jq manifest declares Deterministic; the spec demands ReplayOnly.
        expected_determinism: DeterminismClass::ReplayOnly,
    };
    let transport = StdioMcpTransport {
        server_allowlist: vec![leaf_wrap_bin()],
    };
    let err = transport.connect(&spec).await.unwrap_err();
    assert!(matches!(
        err,
        TransportError::DeterminismMismatch {
            declared: DeterminismClass::Deterministic,
            expected: DeterminismClass::ReplayOnly
        }
    ));
}

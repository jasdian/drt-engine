//! Acceptance criterion 2: a journaled NETWORK leaf replays from its journal
//! WITHOUT network access.
//!
//! We record a run driving a real `curl` leaf (declared `replay_only`) against
//! a stable URL, then replay it with a transport whose server allowlist is
//! EMPTY — so any attempt to spawn/connect the leaf server would error and the
//! replay would diverge. An Identical replay therefore proves the network leaf
//! output came from the journal, not the network.

mod common;

use std::collections::BTreeMap;

use common::{find_bin, leaf_wrap_bin};
use leaf_core::{
    ArgsTemplate, Branch, Executor, FixedClock, LeafNode, Node, ReplayReport, RouteMode, RunConfig,
    RunOutcome, ScriptedRouter, Tree, replay,
};
use leaf_mcp::StdioMcpTransport;
use leaf_traits::{DeterminismClass, FullLog, LeafSpec, ToonCodec};

fn write_curl_manifest(dir: &std::path::Path) -> std::path::PathBuf {
    let curl = find_bin("curl");
    let manifest = serde_json::json!({
        "name": "fetch",
        "description": "HTTP GET a stable URL.",
        "determinism": "replay_only",
        "binary": curl,
        "allowlist": [curl],
        "args_template": ["-s", "--max-time", "20", "{url}"],
        "timeout_ms": 25000,
        "workdir": dir.join("jail"),
        "env": {},
        "input_schema": {
            "type": "object",
            "properties": { "url": {"type": "string"} },
            "required": ["url"],
            "additionalProperties": false
        }
    });
    let path = dir.join("curl.json");
    std::fs::write(&path, manifest.to_string()).unwrap();
    path
}

fn curl_tree() -> Tree {
    Tree {
        name: "curl-fetch".into(),
        version: "1".into(),
        root: Branch {
            name: "root".into(),
            goal: "fetch the stable URL, then finish".into(),
            route: RouteMode::Llm,
            candidates: vec![
                Node::Leaf(LeafNode {
                    name: "fetch".into(),
                    description: "GET https://example.com".into(),
                    leaf: "curl".into(),
                    tool: "fetch".into(),
                    args: ArgsTemplate(serde_json::json!({"url": "https://example.com"})),
                }),
                Node::Done {
                    name: "done".into(),
                    description: "finish".into(),
                    terminal: false,
                },
            ],
        },
    }
}

fn curl_leaves(manifest: &std::path::Path) -> BTreeMap<String, LeafSpec> {
    let mut m = BTreeMap::new();
    m.insert(
        "curl".to_string(),
        LeafSpec {
            id: "curl".into(),
            command: leaf_wrap_bin(),
            args: vec!["--manifest".into(), manifest.to_str().unwrap().into()],
            env: BTreeMap::new(),
            cwd: None,
            call_timeout_ms: 25_000,
            expected_determinism: DeterminismClass::ReplayOnly,
        },
    );
    m
}

#[tokio::test]
#[ignore = "hits the network once (https://example.com) to record; run with --ignored"]
async fn network_leaf_replays_without_network() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = write_curl_manifest(dir.path());
    let tree = curl_tree();
    let leaves = curl_leaves(&manifest);

    // ---- record (real network) ----
    let recording_transport = StdioMcpTransport {
        server_allowlist: vec![leaf_wrap_bin()],
    };
    let clock = FixedClock::new(0);
    let cfg = RunConfig {
        seed: 1,
        max_steps: 8,
        max_llm_calls: 4,
        input: "fetch it".into(),
        journal_path: dir.path().join("recorded.jsonl"),
    };
    let mut exec = Executor {
        transport: &recording_transport,
        router: ScriptedRouter::new([0usize, 1]),
        codec: &ToonCodec,
        propagation: &FullLog,
        clock: &clock,
        leaves: &leaves,
    };
    let result = exec.run(&tree, &cfg).await.unwrap();
    assert_eq!(result.outcome, RunOutcome::Done);
    assert!(
        result
            .last_output
            .as_deref()
            .unwrap_or_default()
            .contains("Example Domain"),
        "expected the example.com body"
    );

    // ---- replay with a transport that CANNOT connect anything ----
    // Empty allowlist ⇒ StdioMcpTransport::connect errors on any spec. Because
    // the leaf is replay_only, ReplayTransport must serve it from the journal
    // and never call connect — proven by an Identical replay.
    let no_network = StdioMcpTransport {
        server_allowlist: vec![],
    };
    let report = replay(
        &cfg.journal_path,
        &dir.path().join("replayed.jsonl"),
        &tree,
        &no_network,
        &ToonCodec,
        &FullLog,
        &leaves,
    )
    .await
    .unwrap();
    match report {
        ReplayReport::Identical { .. } => {}
        ReplayReport::Diverged(d) => {
            panic!("network leaf failed to replay from journal without network: {d}")
        }
    }
}

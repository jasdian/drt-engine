//! The flagship integration test (acceptance criterion 1): a real jq leaf
//! behind leaf-wrap, driven by the executor over MCP stdio — record a run,
//! replay it (jq is Deterministic, so replay genuinely re-executes it and
//! byte-compares), then tamper with the journal and watch replay catch it.

mod common;

use std::collections::BTreeMap;

use common::{leaf_wrap_bin, write_jq_manifest};
use leaf_core::{
    ArgsTemplate, Branch, Executor, FixedClock, JournalEntry, JournalReader, JournalWriter,
    LeafNode, Node, ReplayReport, RouteMode, RunConfig, RunOutcome, ScriptedRouter, Tree, replay,
};
use leaf_mcp::StdioMcpTransport;
use leaf_traits::{DeterminismClass, FullLog, LeafSpec, ToonCodec};

fn jq_tree() -> Tree {
    Tree {
        name: "jq-extract".into(),
        version: "1".into(),
        root: Branch {
            name: "root".into(),
            goal: "extract field .a from the input JSON, then finish".into(),
            route: RouteMode::Llm,
            candidates: vec![
                Node::Leaf(LeafNode {
                    name: "jq_extract".into(),
                    description: "run jq '.a' over the run input".into(),
                    leaf: "jq".into(),
                    tool: "jq".into(),
                    args: ArgsTemplate(serde_json::json!({
                        "expr": ".a",
                        "input": "{input}"
                    })),
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

fn jq_leaves(manifest_path: &std::path::Path) -> BTreeMap<String, LeafSpec> {
    let mut m = BTreeMap::new();
    m.insert(
        "jq".to_string(),
        LeafSpec {
            id: "jq".into(),
            command: leaf_wrap_bin(),
            args: vec!["--manifest".into(), manifest_path.to_str().unwrap().into()],
            env: BTreeMap::new(),
            cwd: None,
            call_timeout_ms: 10_000,
            expected_determinism: DeterminismClass::Deterministic,
        },
    );
    m
}

#[tokio::test]
async fn record_replay_tamper_through_real_mcp_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = write_jq_manifest(dir.path());
    let tree = jq_tree();
    let leaves = jq_leaves(&manifest_path);
    let transport = StdioMcpTransport {
        server_allowlist: vec![leaf_wrap_bin()],
    };
    let clock = FixedClock::new(1_000);

    // ---- record ----
    let cfg = RunConfig {
        seed: 42,
        max_steps: 8,
        max_llm_calls: 4,
        input: "{\"a\": 41}".into(),
        journal_path: dir.path().join("recorded.jsonl"),
    };
    let mut exec = Executor {
        transport: &transport,
        router: ScriptedRouter::new([0usize, 1]),
        codec: &ToonCodec,
        propagation: &FullLog,
        clock: &clock,
        leaves: &leaves,
    };
    let result = exec.run(&tree, &cfg).await.unwrap();
    assert_eq!(result.outcome, RunOutcome::Done);
    assert_eq!(result.last_output.as_deref(), Some("41\n"));

    // ---- replay: jq is Deterministic, so it is re-executed live ----
    let report = replay(
        &cfg.journal_path,
        &dir.path().join("replayed.jsonl"),
        &tree,
        &transport,
        &ToonCodec,
        &FullLog,
        &leaves,
    )
    .await
    .unwrap();
    match report {
        ReplayReport::Identical { entries } => assert_eq!(entries, 5),
        ReplayReport::Diverged(d) => panic!("replay must be identical, diverged: {d}"),
    }

    // ---- tamper: flip the recorded jq output ----
    let mut entries = JournalReader::read_all(&cfg.journal_path).unwrap();
    let mut tampered_index = None;
    for (i, e) in entries.iter_mut().enumerate() {
        if let JournalEntry::LeafCall { output, .. } = e {
            *output = "999\n".into();
            tampered_index = Some(i);
            break;
        }
    }
    let tampered_index = tampered_index.expect("journal has a leaf call");
    let tampered_path = dir.path().join("tampered.jsonl");
    let mut w = JournalWriter::create(&tampered_path).unwrap();
    for e in &entries {
        w.append(e).unwrap();
    }
    drop(w);

    let report = replay(
        &tampered_path,
        &dir.path().join("replayed-tampered.jsonl"),
        &tree,
        &transport,
        &ToonCodec,
        &FullLog,
        &leaves,
    )
    .await
    .unwrap();
    match report {
        ReplayReport::Diverged(d) => {
            assert_eq!(d.entry_index, tampered_index);
            assert_eq!(d.field, "leaf_call.output");
            assert!(d.expected.contains("999"));
            assert!(d.actual.contains("41"));
        }
        ReplayReport::Identical { .. } => panic!("tampered journal must diverge"),
    }
}

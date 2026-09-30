//! In-crate fakes for executor/replay tests: no subprocesses, no LLM.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use leaf_traits::{DeterminismClass, LeafHandle, LeafOutput, LeafSpec, Transport, TransportError};

use crate::router::{LlmCallRecord, RouteDecision, RouteRequest, Router, RouterError};
use crate::tree::{ArgsTemplate, Branch, LeafNode, Node, RouteMode, Tree};

/// Fake transport with two behaviors keyed by leaf id:
/// - "echo" (Deterministic): output is a pure function of args.
/// - "volatile" (ReplayOnly): output embeds a live call counter — different
///   on every call, standing in for a network tool.
pub struct FakeTransport {
    volatile_counter: Arc<AtomicU64>,
}

impl FakeTransport {
    pub fn deterministic() -> Self {
        Self {
            volatile_counter: Arc::new(AtomicU64::new(0)),
        }
    }
    /// Number of live volatile calls made through this transport.
    pub fn volatile_calls(&self) -> u64 {
        self.volatile_counter.load(Ordering::SeqCst)
    }
}

pub struct FakeHandle {
    leaf_id: String,
    class: DeterminismClass,
    counter: Arc<AtomicU64>,
}

impl LeafHandle for FakeHandle {
    fn determinism(&self) -> DeterminismClass {
        self.class
    }

    async fn call(
        &mut self,
        _tool: &str,
        args: serde_json::Value,
        _timeout: Duration,
    ) -> Result<LeafOutput, TransportError> {
        let text = args
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("?")
            .to_string();
        let out = match self.leaf_id.as_str() {
            "echo" => format!("echo: {text}"),
            "volatile" => {
                let n = self.counter.fetch_add(1, Ordering::SeqCst);
                format!("volatile#{n}: {text}")
            }
            // For the {source} idempotency test: "fetch" (a non-jq source) emits
            // a fixed raw string; "xform" (tool jq) wraps its `input` arg — so a
            // correct {source} keeps re-runs idempotent (single wrap) while a
            // broken one double-wraps.
            "fetch" => "RAW".to_string(),
            "xform" => {
                let input = args.get("input").and_then(|t| t.as_str()).unwrap_or("");
                format!("x({input})")
            }
            other => {
                return Err(TransportError::Protocol(format!(
                    "unknown fake leaf {other}"
                )));
            }
        };
        Ok(LeafOutput {
            text: out,
            is_error: false,
            duration_ms: 5,
        })
    }
}

impl Transport for FakeTransport {
    type Handle = FakeHandle;

    async fn connect(&self, spec: &LeafSpec) -> Result<FakeHandle, TransportError> {
        Ok(FakeHandle {
            leaf_id: spec.id.clone(),
            class: spec.expected_determinism,
            counter: self.volatile_counter.clone(),
        })
    }
}

pub fn leaf_registry() -> BTreeMap<String, LeafSpec> {
    let mut m = BTreeMap::new();
    m.insert(
        "echo".to_string(),
        LeafSpec {
            id: "echo".into(),
            command: "/fake/echo".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            call_timeout_ms: 1_000,
            expected_determinism: DeterminismClass::Deterministic,
        },
    );
    m.insert(
        "volatile".to_string(),
        LeafSpec {
            id: "volatile".into(),
            command: "/fake/volatile".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            call_timeout_ms: 1_000,
            expected_determinism: DeterminismClass::ReplayOnly,
        },
    );
    m.insert(
        "fetch".to_string(),
        LeafSpec {
            id: "fetch".into(),
            command: "/fake/fetch".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            call_timeout_ms: 1_000,
            expected_determinism: DeterminismClass::ReplayOnly,
        },
    );
    m.insert(
        "xform".to_string(),
        LeafSpec {
            id: "xform".into(),
            command: "/fake/xform".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            call_timeout_ms: 1_000,
            expected_determinism: DeterminismClass::Deterministic,
        },
    );
    m
}

/// root -> [echo leaf, Done]
pub fn echo_tree() -> Tree {
    Tree {
        name: "echo-test".into(),
        version: "1".into(),
        root: Branch {
            name: "root".into(),
            route: RouteMode::Llm,
            goal: "echo the input, then finish".into(),
            candidates: vec![
                Node::Leaf(LeafNode {
                    name: "echo".into(),
                    description: "echo the run input".into(),
                    leaf: "echo".into(),
                    tool: "echo".into(),
                    args: ArgsTemplate(serde_json::json!({"text": "{input}"})),
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

/// root -> [volatile leaf, Done] — the "network tool" shape.
pub fn volatile_tree() -> Tree {
    Tree {
        name: "volatile-test".into(),
        version: "1".into(),
        root: Branch {
            name: "root".into(),
            route: RouteMode::Llm,
            goal: "poll the volatile source twice, then finish".into(),
            candidates: vec![
                Node::Leaf(LeafNode {
                    name: "volatile".into(),
                    description: "read the volatile source".into(),
                    leaf: "volatile".into(),
                    tool: "read".into(),
                    args: ArgsTemplate(serde_json::json!({"text": "{input}"})),
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

/// root -> [fetch (tool "api"), xform (tool "jq", input={source}), Done].
///
/// The transform reads `{source}` — the most recent NON-jq output — so
/// re-picking `xform` a second time must still read the raw fetch ("RAW"),
/// producing "x(RAW)" both times. A broken `{source}` (falling back to the
/// last output) would double-wrap to "x(x(RAW))" on the second pick. This
/// pins the idempotency contract that the bench trees rely on.
pub fn source_idempotency_tree() -> Tree {
    Tree {
        name: "source-test".into(),
        version: "1".into(),
        root: Branch {
            name: "root".into(),
            route: RouteMode::Llm,
            goal: "fetch raw data, then reduce it — re-runnable".into(),
            candidates: vec![
                Node::Leaf(LeafNode {
                    name: "fetch".into(),
                    description: "fetch the raw source".into(),
                    leaf: "fetch".into(),
                    tool: "api".into(),
                    args: ArgsTemplate(serde_json::json!({"text": "{input}"})),
                }),
                Node::Leaf(LeafNode {
                    name: "xform".into(),
                    description: "reduce the fetched source".into(),
                    leaf: "xform".into(),
                    tool: "jq".into(),
                    args: ArgsTemplate(serde_json::json!({"input": "{source}"})),
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

/// root (LLM) -> [ capability (SEQUENCE: [echo leaf, terminal Done]) ].
///
/// A forced pipeline: the LLM picks the capability ONCE, then the capability
/// runs deterministically (echo) and its terminal Done ends the whole run.
/// Exactly one `BranchDecision` is journaled — the sequence steps make none.
pub fn forced_pipeline_tree() -> Tree {
    Tree {
        name: "forced-pipeline".into(),
        version: "1".into(),
        root: Branch {
            name: "root".into(),
            goal: "pick the capability".into(),
            route: RouteMode::Llm,
            candidates: vec![Node::Branch(Branch {
                name: "capability".into(),
                goal: "run echo then finish".into(),
                route: RouteMode::Sequence,
                candidates: vec![
                    Node::Leaf(LeafNode {
                        name: "echo".into(),
                        description: "echo the run input".into(),
                        leaf: "echo".into(),
                        tool: "echo".into(),
                        args: ArgsTemplate(serde_json::json!({"text": "{input}"})),
                    }),
                    Node::Done {
                        name: "done".into(),
                        description: "finish the run".into(),
                        terminal: true,
                    },
                ],
            })],
        },
    }
}

/// root (LLM) -> [ hybrid (SEQUENCE: [fetch echo, classify (LLM: [path_a, path_b],
/// each a terminal SEQUENCE)]) ].
///
/// A hybrid capability: one forced fetch step, then a GENUINE mid-pipeline LLM
/// decision (`classify`), then a terminal deterministic arm. TWO
/// `BranchDecision`s are journaled (root + classify); the sequence steps make
/// none.
pub fn hybrid_tree() -> Tree {
    fn arm(name: &str) -> Node {
        Node::Branch(Branch {
            name: name.into(),
            goal: format!("{name} arm"),
            route: RouteMode::Sequence,
            candidates: vec![
                Node::Leaf(LeafNode {
                    name: format!("{name}_echo"),
                    description: "echo".into(),
                    leaf: "echo".into(),
                    tool: "echo".into(),
                    args: ArgsTemplate(serde_json::json!({"text": "{input}"})),
                }),
                Node::Done {
                    name: "done".into(),
                    description: "finish".into(),
                    terminal: true,
                },
            ],
        })
    }
    Tree {
        name: "hybrid".into(),
        version: "1".into(),
        root: Branch {
            name: "root".into(),
            goal: "pick the capability".into(),
            route: RouteMode::Llm,
            candidates: vec![Node::Branch(Branch {
                name: "hybrid".into(),
                goal: "fetch then classify".into(),
                route: RouteMode::Sequence,
                candidates: vec![
                    Node::Leaf(LeafNode {
                        name: "fetch".into(),
                        description: "echo the run input".into(),
                        leaf: "echo".into(),
                        tool: "echo".into(),
                        args: ArgsTemplate(serde_json::json!({"text": "{input}"})),
                    }),
                    Node::Branch(Branch {
                        name: "classify".into(),
                        goal: "choose path A or B based on the fetched data".into(),
                        route: RouteMode::Llm,
                        candidates: vec![arm("path_a"), arm("path_b")],
                    }),
                ],
            })],
        },
    }
}

/// Scripted picks that also fabricate deterministic LlmCallRecords — used to
/// test LlmCall journaling and replay pairing without any LLM.
pub struct ScriptedLlmRouter {
    picks: VecDeque<usize>,
}

impl ScriptedLlmRouter {
    pub fn new(picks: impl IntoIterator<Item = usize>) -> Self {
        Self {
            picks: picks.into_iter().collect(),
        }
    }
}

impl Router for ScriptedLlmRouter {
    async fn route(&mut self, req: RouteRequest<'_>) -> Result<RouteDecision, RouterError> {
        let chosen = self
            .picks
            .pop_front()
            .ok_or(RouterError::ReplayExhausted(req.decision_index))?;
        Ok(RouteDecision {
            chosen,
            exploration: None,
            llm: Some(LlmCallRecord {
                model: "fake-model".into(),
                request_digest: req.prompt_digest.to_string(),
                response_text: format!("c{chosen}"),
                stop_reason: "end_turn".into(),
                input_tokens: 100 + req.decision_index as u64,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 3,
                cost_microusd: 345,
                api_calls: 1,
                parse_fallback: false,
            }),
        })
    }
}

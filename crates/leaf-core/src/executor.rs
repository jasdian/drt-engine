//! The run loop: ties Tree, Router, Transport, Propagation, Codec, Clock and
//! Journal together.
//!
//! A branch is a loop head. An `Llm` branch asks the router to pick a
//! candidate; a `Sequence` branch walks its candidates deterministically by a
//! cursor with no router call. A Leaf pick runs the tool and returns to the
//! branch; a Branch pick descends; a Done pick pops (or, if `terminal`, ends
//! the whole run). `max_steps` caps TOTAL loop iterations (Llm routing
//! decisions AND deterministic sequence steps); `max_llm_calls` caps billed
//! calls (only `Llm` branch picks bill).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Duration;

use leaf_traits::{
    Codec, HistoryItem, LeafHandle, LeafSpec, Propagation, Transport, TransportError,
};

use crate::clock::Clock;
use crate::digest::sha256_hex;
use crate::journal::{FORMAT_VERSION, JournalEntry, JournalError, JournalWriter, RunOutcome};
use crate::router::{RouteRequest, Router, RouterError};
use crate::tree::{Branch, Node, RouteMode, Tree, TreeError, derive_seed};

pub use crate::journal::RunOutcome as Outcome;

/// A branch on the execution stack plus, for a `Sequence` branch, the cursor
/// into its candidates (an `Llm` branch ignores the cursor — the router
/// re-decides each visit).
struct Frame<'a> {
    branch: &'a Branch,
    cursor: usize,
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub seed: u64,
    /// Cap on total loop iterations — Llm routing decisions AND deterministic
    /// sequence-walk steps (not just routing decisions).
    pub max_steps: u32,
    /// Cap on billed LLM calls (exploration-skipped decisions don't count).
    pub max_llm_calls: u32,
    pub input: String,
    pub journal_path: PathBuf,
}

#[derive(Debug)]
pub struct RunResult {
    pub outcome: RunOutcome,
    pub steps: u32,
    pub llm_calls: u32,
    pub total_cost_microusd: u64,
    pub journal_path: PathBuf,
    /// Text of the last leaf output (the conventional "answer" surface when
    /// the pipeline formats the final answer deterministically).
    pub last_output: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    Router(#[from] RouterError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("unknown leaf id in tree: {0}")]
    UnknownLeaf(String),
    #[error("router chose candidate {chosen} of {available}")]
    InvalidChoice { chosen: usize, available: usize },
    #[error(transparent)]
    Tree(#[from] TreeError),
}

pub struct Executor<'a, T: Transport, R: Router> {
    pub transport: &'a T,
    pub router: R,
    pub codec: &'a dyn Codec,
    pub propagation: &'a dyn Propagation,
    pub clock: &'a dyn Clock,
    /// Leaf registry. BTreeMap for stable iteration order.
    pub leaves: &'a BTreeMap<String, LeafSpec>,
}

impl<T: Transport, R: Router> Executor<'_, T, R> {
    pub async fn run(&mut self, tree: &Tree, cfg: &RunConfig) -> Result<RunResult, ExecError> {
        // Reject a float in any leaf's args template before touching the
        // journal — the no-floats rule is a journal invariant.
        tree.validate()?;
        let mut journal = JournalWriter::create(&cfg.journal_path)?;
        journal.append(&JournalEntry::RunStarted {
            run_id: format!("{:016x}", derive_seed(cfg.seed, u32::MAX)),
            seed: cfg.seed,
            tree_digest: tree.digest(),
            input: cfg.input.clone(),
            codec: self.codec.name().to_string(),
            propagation: self.propagation.name().to_string(),
            format_version: FORMAT_VERSION,
            ts_ms: self.clock.now_unix_ms(),
        })?;

        let mut history = vec![HistoryItem::RunInput {
            text: cfg.input.clone(),
        }];
        // Branch stack, root first. Each frame carries a cursor used only by
        // `Sequence` branches (deterministic in-order walk).
        let mut stack: Vec<Frame> = vec![Frame {
            branch: &tree.root,
            cursor: 0,
        }];
        let mut handles: HashMap<String, T::Handle> = HashMap::new();
        let mut step: u32 = 0;
        let mut llm_calls: u32 = 0;
        let mut total_cost: u64 = 0;
        let mut total_in: u64 = 0;
        let mut total_out: u64 = 0;
        let mut last_output: Option<String> = None;

        let outcome = loop {
            let Some(frame) = stack.last() else {
                break RunOutcome::Done;
            };
            // Copy the branch ref out — it borrows the tree, not the stack, so a
            // `Sequence` branch can bump its own cursor via `stack.last_mut()`.
            let branch: &Branch = frame.branch;
            if step >= cfg.max_steps {
                break RunOutcome::MaxSteps;
            }

            let chosen: usize;
            let explored: bool;

            match branch.route {
                RouteMode::Sequence => {
                    // Deterministic in-order walk: no router, no billed LLM call,
                    // no BranchDecision (reproduced from the tree on replay). The
                    // "hybrid tree" lever — forced pipeline steps cost 0 LLM calls.
                    let cursor = {
                        let f = stack.last_mut().expect("stack nonempty at loop head");
                        let c = f.cursor;
                        f.cursor += 1;
                        c
                    };
                    if cursor >= branch.candidates.len() {
                        // Ran past the end without a Done — pop defensively.
                        stack.pop();
                        step += 1;
                        continue;
                    }
                    chosen = cursor;
                    explored = false;
                }
                RouteMode::Llm => {
                    // Checked here (only Llm branches bill a call) so a run that
                    // completes on its last allowed call is Done, not MaxLlmCalls,
                    // and a deterministic sequence is never blocked by the cap.
                    if llm_calls >= cfg.max_llm_calls {
                        break RunOutcome::MaxLlmCalls;
                    }
                    let rendered = self.propagation.render(&history, self.codec);
                    let candidates: Vec<_> = branch
                        .candidates
                        .iter()
                        .enumerate()
                        .map(|(i, n)| n.candidate_info(i))
                        .collect();
                    let menu = self.codec.render_candidates(&candidates);
                    let prompt_digest =
                        sha256_hex(format!("{}\0{}\0{}", branch.goal, rendered, menu).as_bytes());

                    let decision = match self
                        .router
                        .route(RouteRequest {
                            goal: &branch.goal,
                            candidates: &candidates,
                            rendered_context: &rendered,
                            rendered_menu: &menu,
                            prompt_digest: &prompt_digest,
                            decision_index: step,
                        })
                        .await
                    {
                        Ok(d) => d,
                        Err(e) => {
                            let msg = e.to_string();
                            finish(
                                &mut journal,
                                self.clock,
                                step,
                                RunOutcome::Error { message: msg },
                                total_cost,
                                total_in,
                                total_out,
                            )?;
                            return Err(e.into());
                        }
                    };

                    if let Some(llm) = &decision.llm {
                        total_cost += llm.cost_microusd;
                        total_in += llm.input_tokens
                            + llm.cache_creation_input_tokens
                            + llm.cache_read_input_tokens;
                        total_out += llm.output_tokens;
                        // Count billed API calls, not decisions — a router re-ask
                        // makes two calls under one decision.
                        llm_calls += llm.api_calls;
                        journal.append(&JournalEntry::LlmCall {
                            step,
                            rec: llm.clone(),
                        })?;
                    }

                    if decision.chosen >= branch.candidates.len() {
                        let err = ExecError::InvalidChoice {
                            chosen: decision.chosen,
                            available: branch.candidates.len(),
                        };
                        finish(
                            &mut journal,
                            self.clock,
                            step,
                            RunOutcome::Error {
                                message: err.to_string(),
                            },
                            total_cost,
                            total_in,
                            total_out,
                        )?;
                        return Err(err);
                    }

                    journal.append(&JournalEntry::BranchDecision {
                        step,
                        branch: branch.name.clone(),
                        prompt_digest,
                        candidates: candidates.iter().map(|c| c.name.clone()).collect(),
                        chosen: decision.chosen,
                        exploration: decision.exploration.clone(),
                    })?;

                    chosen = decision.chosen;
                    explored = decision
                        .exploration
                        .as_ref()
                        .map(|e| e.explored)
                        .unwrap_or(false);
                }
            }

            let node = &branch.candidates[chosen];
            history.push(HistoryItem::Decision {
                branch: branch.name.clone(),
                chosen: node.name().to_string(),
                explored,
            });

            match node {
                Node::Done { terminal, .. } => {
                    if *terminal {
                        // End the WHOLE run here (a hybrid capability's finish),
                        // instead of popping to the root for another rubber-stamp
                        // routing decision. Next loop head sees an empty stack.
                        stack.clear();
                    } else {
                        stack.pop();
                    }
                }
                Node::Branch(child) => {
                    stack.push(Frame {
                        branch: child,
                        cursor: 0,
                    });
                }
                Node::Leaf(leaf_node) => {
                    let spec = match self.leaves.get(&leaf_node.leaf) {
                        Some(s) => s,
                        None => {
                            // Journal RunFinished{Error} like every other fatal
                            // path, so the recording stays replayable.
                            let err = ExecError::UnknownLeaf(leaf_node.leaf.clone());
                            finish(
                                &mut journal,
                                self.clock,
                                step,
                                RunOutcome::Error {
                                    message: err.to_string(),
                                },
                                total_cost,
                                total_in,
                                total_out,
                            )?;
                            return Err(err);
                        }
                    };

                    if !handles.contains_key(&leaf_node.leaf) {
                        match self.transport.connect(spec).await {
                            Ok(h) => {
                                handles.insert(leaf_node.leaf.clone(), h);
                            }
                            Err(e) => {
                                finish(
                                    &mut journal,
                                    self.clock,
                                    step,
                                    RunOutcome::Error {
                                        message: e.to_string(),
                                    },
                                    total_cost,
                                    total_in,
                                    total_out,
                                )?;
                                return Err(e.into());
                            }
                        }
                    }
                    let handle = handles.get_mut(&leaf_node.leaf).expect("just inserted");

                    let last = history
                        .iter()
                        .rev()
                        .find_map(|h| match h {
                            HistoryItem::LeafOutput { output, .. } => Some(output.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    // `{source}` = most recent RAW data-source output (a leaf
                    // whose tool is not the `jq` transform), so a re-picked
                    // transform re-reads the original fetch rather than its own
                    // reduced output → re-running is idempotent, no clobber.
                    let source = history
                        .iter()
                        .rev()
                        .find_map(|h| match h {
                            HistoryItem::LeafOutput { output, tool, .. } if tool != "jq" => {
                                Some(output.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_default();
                    let call_seed = leaf_node
                        .args
                        .uses_seed()
                        .then(|| derive_seed(cfg.seed, step));
                    let args = leaf_node.args.build(
                        &cfg.input,
                        &last,
                        &source,
                        call_seed.unwrap_or_default(),
                    );

                    let out = match handle
                        .call(
                            &leaf_node.tool,
                            args.clone(),
                            Duration::from_millis(spec.call_timeout_ms),
                        )
                        .await
                    {
                        Ok(o) => o,
                        Err(e) => {
                            finish(
                                &mut journal,
                                self.clock,
                                step,
                                RunOutcome::Error {
                                    message: e.to_string(),
                                },
                                total_cost,
                                total_in,
                                total_out,
                            )?;
                            return Err(e.into());
                        }
                    };

                    journal.append(&JournalEntry::LeafCall {
                        step,
                        leaf: leaf_node.leaf.clone(),
                        tool: leaf_node.tool.clone(),
                        input: args,
                        seed: call_seed,
                        determinism: spec.expected_determinism,
                        output: out.text.clone(),
                        is_error: out.is_error,
                        duration_ms: out.duration_ms,
                    })?;
                    history.push(HistoryItem::LeafOutput {
                        leaf: leaf_node.leaf.clone(),
                        tool: leaf_node.tool.clone(),
                        output: out.text.clone(),
                        is_error: out.is_error,
                    });
                    last_output = Some(out.text);
                }
            }

            step += 1;
        };

        finish(
            &mut journal,
            self.clock,
            step,
            outcome.clone(),
            total_cost,
            total_in,
            total_out,
        )?;
        Ok(RunResult {
            outcome,
            steps: step,
            llm_calls,
            total_cost_microusd: total_cost,
            journal_path: cfg.journal_path.clone(),
            last_output,
        })
    }
}

fn finish(
    journal: &mut JournalWriter,
    clock: &dyn Clock,
    steps: u32,
    outcome: RunOutcome,
    total_cost_microusd: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
) -> Result<(), JournalError> {
    journal.append(&JournalEntry::RunFinished {
        steps,
        outcome,
        total_cost_microusd,
        total_input_tokens,
        total_output_tokens,
        ts_ms: clock.now_unix_ms(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::router::ScriptedRouter;
    use crate::testutil::{FakeTransport, echo_tree, leaf_registry};
    use leaf_traits::DeterminismClass;

    fn cfg(dir: &tempfile::TempDir, seed: u64) -> RunConfig {
        RunConfig {
            seed,
            max_steps: 16,
            max_llm_calls: 12,
            input: "hello world".into(),
            journal_path: dir.path().join("run.jsonl"),
        }
    }

    #[tokio::test]
    async fn executor_journals_expected_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let tree = echo_tree();
        let leaves = leaf_registry();
        let transport = FakeTransport::deterministic();
        let clock = FixedClock::new(1_000);
        let mut exec = Executor {
            transport: &transport,
            router: ScriptedRouter::new([0usize, 1]),
            codec: &leaf_traits::ToonCodec,
            propagation: &leaf_traits::FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let cfg = cfg(&dir, 42);
        let result = exec.run(&tree, &cfg).await.unwrap();
        assert_eq!(result.outcome, RunOutcome::Done);
        assert_eq!(result.steps, 2);
        assert_eq!(result.last_output.as_deref(), Some("echo: hello world"));

        let entries = crate::journal::JournalReader::read_all(&cfg.journal_path).unwrap();
        let kinds: Vec<&'static str> = entries
            .iter()
            .map(|e| match e {
                JournalEntry::RunStarted { .. } => "start",
                JournalEntry::LlmCall { .. } => "llm",
                JournalEntry::BranchDecision { .. } => "decision",
                JournalEntry::LeafCall { .. } => "leaf",
                JournalEntry::RunFinished { .. } => "finish",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["start", "decision", "leaf", "decision", "finish"]
        );

        match &entries[0] {
            JournalEntry::RunStarted {
                tree_digest,
                codec,
                propagation,
                ..
            } => {
                assert_eq!(tree_digest, &tree.digest());
                assert_eq!(codec, "toon.v1");
                assert_eq!(propagation, "full");
            }
            other => panic!("unexpected first entry {other:?}"),
        }
        match &entries[2] {
            JournalEntry::LeafCall {
                input,
                output,
                determinism,
                ..
            } => {
                assert_eq!(input["text"], "hello world");
                assert_eq!(output, "echo: hello world");
                assert_eq!(*determinism, DeterminismClass::Deterministic);
            }
            other => panic!("unexpected leaf entry {other:?}"),
        }
    }

    #[tokio::test]
    async fn source_placeholder_is_idempotent_across_reruns() {
        // fetch (tool "api") -> xform (tool "jq", input={source}) -> xform -> done.
        // `{source}` must resolve to the raw fetch ("RAW") on BOTH xform picks,
        // so the answer is "x(RAW)", never the double-wrapped "x(x(RAW))".
        use crate::testutil::source_idempotency_tree;
        let dir = tempfile::tempdir().unwrap();
        let tree = source_idempotency_tree();
        let leaves = leaf_registry();
        let transport = FakeTransport::deterministic();
        let clock = FixedClock::new(0);
        let mut exec = Executor {
            transport: &transport,
            router: ScriptedRouter::new([0usize, 1, 1, 2]),
            codec: &leaf_traits::ToonCodec,
            propagation: &leaf_traits::FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let c = cfg(&dir, 7);
        let result = exec.run(&tree, &c).await.unwrap();
        assert_eq!(result.outcome, RunOutcome::Done);
        assert_eq!(
            result.last_output.as_deref(),
            Some("x(RAW)"),
            "re-picking the transform must re-read the raw source, not its own output"
        );
    }

    fn kind(e: &JournalEntry) -> &'static str {
        match e {
            JournalEntry::RunStarted { .. } => "start",
            JournalEntry::LlmCall { .. } => "llm",
            JournalEntry::BranchDecision { .. } => "decision",
            JournalEntry::LeafCall { .. } => "leaf",
            JournalEntry::RunFinished { .. } => "finish",
        }
    }

    #[tokio::test]
    async fn sequence_branch_makes_no_routing_decision() {
        // Forced pipeline: root LLM picks the capability ONCE; the capability is
        // a Sequence that runs echo then a terminal Done. Exactly one
        // BranchDecision is journaled (the sequence steps make none), and the
        // terminal Done ends the run without popping back to root for a second
        // decision — proven by giving the ScriptedRouter only ONE pick.
        let dir = tempfile::tempdir().unwrap();
        let tree = crate::testutil::forced_pipeline_tree();
        let leaves = leaf_registry();
        let transport = FakeTransport::deterministic();
        let clock = FixedClock::new(0);
        let mut exec = Executor {
            transport: &transport,
            router: ScriptedRouter::new([0usize]),
            codec: &leaf_traits::ToonCodec,
            propagation: &leaf_traits::FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let cfg = cfg(&dir, 42);
        let result = exec.run(&tree, &cfg).await.unwrap();
        assert_eq!(result.outcome, RunOutcome::Done);
        assert_eq!(result.last_output.as_deref(), Some("echo: hello world"));

        let entries = crate::journal::JournalReader::read_all(&cfg.journal_path).unwrap();
        let kinds: Vec<&str> = entries.iter().map(|e| kind(e)).collect();
        assert_eq!(kinds, vec!["start", "decision", "leaf", "finish"]);
        let decisions = entries
            .iter()
            .filter(|e| matches!(e, JournalEntry::BranchDecision { .. }))
            .count();
        assert_eq!(
            decisions, 1,
            "only the root LLM branch decides; the deterministic sequence makes none"
        );
    }

    #[tokio::test]
    async fn hybrid_capability_decides_twice() {
        // root LLM picks the hybrid capability; the capability fetches (forced),
        // then a GENUINE mid-pipeline LLM decision (classify) picks an arm, which
        // runs deterministically to a terminal Done. TWO BranchDecisions (root +
        // classify), two leaves.
        let dir = tempfile::tempdir().unwrap();
        let tree = crate::testutil::hybrid_tree();
        let leaves = leaf_registry();
        let transport = FakeTransport::deterministic();
        let clock = FixedClock::new(0);
        let mut exec = Executor {
            transport: &transport,
            router: ScriptedRouter::new([0usize, 1]),
            codec: &leaf_traits::ToonCodec,
            propagation: &leaf_traits::FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let cfg = cfg(&dir, 42);
        let result = exec.run(&tree, &cfg).await.unwrap();
        assert_eq!(result.outcome, RunOutcome::Done);
        let entries = crate::journal::JournalReader::read_all(&cfg.journal_path).unwrap();
        let kinds: Vec<&str> = entries.iter().map(|e| kind(e)).collect();
        assert_eq!(
            kinds,
            vec!["start", "decision", "leaf", "decision", "leaf", "finish"]
        );
    }

    #[tokio::test]
    async fn max_steps_guard_fires() {
        let dir = tempfile::tempdir().unwrap();
        let tree = echo_tree();
        let leaves = leaf_registry();
        let transport = FakeTransport::deterministic();
        let clock = FixedClock::new(0);
        let mut exec = Executor {
            transport: &transport,
            // Always pick the leaf, never Done: loops until the guard.
            router: ScriptedRouter::new(std::iter::repeat_n(0usize, 100)),
            codec: &leaf_traits::ToonCodec,
            propagation: &leaf_traits::FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let mut c = cfg(&dir, 1);
        c.max_steps = 3;
        let result = exec.run(&tree, &c).await.unwrap();
        assert_eq!(result.outcome, RunOutcome::MaxSteps);
        assert_eq!(result.steps, 3);
    }

    #[tokio::test]
    async fn invalid_choice_is_fatal_and_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let tree = echo_tree();
        let leaves = leaf_registry();
        let transport = FakeTransport::deterministic();
        let clock = FixedClock::new(0);
        let mut exec = Executor {
            transport: &transport,
            router: ScriptedRouter::new([99usize]),
            codec: &leaf_traits::ToonCodec,
            propagation: &leaf_traits::FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let c = cfg(&dir, 1);
        let err = exec.run(&tree, &c).await.unwrap_err();
        assert!(matches!(err, ExecError::InvalidChoice { chosen: 99, .. }));
        let entries = crate::journal::JournalReader::read_all(&c.journal_path).unwrap();
        assert!(matches!(
            entries.last(),
            Some(JournalEntry::RunFinished {
                outcome: RunOutcome::Error { .. },
                ..
            })
        ));
    }
}

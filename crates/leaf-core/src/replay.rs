//! Replay: re-run the executor from a recorded journal and structurally
//! compare the produced entry stream against the recorded one.
//!
//! Substitutions:
//! - `ReplayRouter` serves recorded routing decisions and LLM records —
//!   vendor nondeterminism is quarantined by construction, no LLM is called.
//! - `ReplayTransport` re-executes `Deterministic` leaves live (the comparator
//!   then byte-asserts their outputs) and serves `ReplayOnly` leaf outputs
//!   straight from the journal, touching neither network nor subprocess.
//!
//! ALL divergence detection flows through the comparator: if context
//! rendering drifts, the replayed BranchDecision carries a different
//! prompt_digest; if args construction drifts, the replayed LeafCall carries a
//! different input; if a deterministic tool changed, its output differs. The
//! first divergent entry is reported structurally.
//!
//! Exempt (informational) fields: `ts_ms`, `duration_ms`.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use leaf_traits::{
    Codec, DeterminismClass, LeafHandle, LeafOutput, LeafSpec, Propagation, Transport,
    TransportError,
};
use serde::Serialize;

use crate::clock::FixedClock;
use crate::executor::{Executor, RunConfig};
use crate::journal::{JournalEntry, JournalError, JournalReader, RunOutcome};
use crate::router::{
    ExplorationRecord, LlmCallRecord, RouteDecision, RouteRequest, Router, RouterError,
};
use crate::tree::Tree;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Divergence {
    /// First divergent journal line (0-based).
    pub entry_index: usize,
    /// Dotted path, e.g. "leaf_call.output".
    pub field: String,
    /// Recorded value (truncated to 500 chars for display).
    pub expected: String,
    /// Replayed value (same truncation).
    pub actual: String,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "entry #{}: field {}\n  expected: {}\n  actual:   {}",
            self.entry_index, self.field, self.expected, self.actual
        )
    }
}

#[derive(Debug)]
pub enum ReplayReport {
    Identical { entries: usize },
    Diverged(Divergence),
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("bad recorded journal: {0}")]
    BadJournal(String),
}

struct RecordedDecision {
    chosen: usize,
    exploration: Option<ExplorationRecord>,
}

/// Serves recorded decisions; never calls an LLM.
pub struct ReplayRouter {
    queue: VecDeque<(Option<LlmCallRecord>, RecordedDecision)>,
}

impl Router for ReplayRouter {
    async fn route(&mut self, req: RouteRequest<'_>) -> Result<RouteDecision, RouterError> {
        let (llm, dec) = self
            .queue
            .pop_front()
            .ok_or(RouterError::ReplayExhausted(req.decision_index))?;
        // For an EXPLORED decision, re-derive the chosen index from the
        // seeded roll rather than trusting the recorded `chosen` — a tampered
        // `chosen` then diverges here instead of silently replaying.
        let chosen = match &dec.exploration {
            Some(e) if e.explored && !req.candidates.is_empty() => {
                (e.pick.unwrap_or(0) % req.candidates.len() as u64) as usize
            }
            _ => dec.chosen,
        };
        Ok(RouteDecision {
            chosen,
            exploration: dec.exploration,
            llm,
        })
    }
}

#[derive(Clone)]
pub struct ServedOutput {
    output: String,
    is_error: bool,
    duration_ms: u64,
}

/// Wraps a real transport; `ReplayOnly` leaves never touch it.
pub struct ReplayTransport<'a, T: Transport> {
    inner: &'a T,
    replay_only_queue: Arc<Mutex<VecDeque<ServedOutput>>>,
}

pub enum ReplayHandle<H> {
    Real(H),
    Journal {
        queue: Arc<Mutex<VecDeque<ServedOutput>>>,
    },
}

impl<H: LeafHandle> LeafHandle for ReplayHandle<H> {
    fn determinism(&self) -> DeterminismClass {
        match self {
            ReplayHandle::Real(h) => h.determinism(),
            ReplayHandle::Journal { .. } => DeterminismClass::ReplayOnly,
        }
    }

    async fn call(
        &mut self,
        tool: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<LeafOutput, TransportError> {
        match self {
            ReplayHandle::Real(h) => h.call(tool, args, timeout).await,
            ReplayHandle::Journal { queue } => {
                let served = queue.lock().expect("replay queue poisoned").pop_front();
                match served {
                    Some(s) => Ok(LeafOutput {
                        text: s.output,
                        is_error: s.is_error,
                        duration_ms: s.duration_ms,
                    }),
                    None => Err(TransportError::Protocol(
                        "replay: recorded replay-only outputs exhausted".into(),
                    )),
                }
            }
        }
    }
}

impl<T: Transport> Transport for ReplayTransport<'_, T> {
    type Handle = ReplayHandle<T::Handle>;

    async fn connect(&self, spec: &LeafSpec) -> Result<Self::Handle, TransportError> {
        match spec.expected_determinism {
            DeterminismClass::Deterministic => {
                Ok(ReplayHandle::Real(self.inner.connect(spec).await?))
            }
            DeterminismClass::ReplayOnly => Ok(ReplayHandle::Journal {
                queue: self.replay_only_queue.clone(),
            }),
        }
    }
}

/// Re-run `recorded_path` against `tree`, writing the replayed journal to
/// `replay_journal_path` (must not exist), and report the first divergence.
pub async fn replay<T: Transport>(
    recorded_path: &Path,
    replay_journal_path: &Path,
    tree: &Tree,
    transport: &T,
    codec: &dyn Codec,
    propagation: &dyn Propagation,
    leaves: &BTreeMap<String, LeafSpec>,
) -> Result<ReplayReport, ReplayError> {
    let recorded = JournalReader::read_all(recorded_path)?;

    let Some(JournalEntry::RunStarted { seed, input, .. }) = recorded.first() else {
        return Err(ReplayError::BadJournal(
            "first entry is not run_started".into(),
        ));
    };
    let (seed, input) = (*seed, input.clone());

    // Recover the effective caps so guard-terminated runs replay identically.
    let (recorded_steps, recorded_outcome) = recorded
        .iter()
        .find_map(|e| match e {
            JournalEntry::RunFinished { steps, outcome, .. } => Some((*steps, outcome.clone())),
            _ => None,
        })
        .ok_or_else(|| ReplayError::BadJournal("no run_finished entry".into()))?;
    let llm_call_count = recorded
        .iter()
        .filter(|e| matches!(e, JournalEntry::LlmCall { .. }))
        .count() as u32;
    let max_steps = match recorded_outcome {
        RunOutcome::MaxSteps => recorded_steps,
        _ => recorded_steps.saturating_add(1),
    };
    let max_llm_calls = match recorded_outcome {
        RunOutcome::MaxLlmCalls => llm_call_count,
        _ => llm_call_count.saturating_add(1),
    };

    // Pair each BranchDecision with its immediately-preceding LlmCall.
    let mut queue = VecDeque::new();
    let mut pending_llm: Option<LlmCallRecord> = None;
    let mut replay_only = VecDeque::new();
    for entry in &recorded {
        match entry {
            JournalEntry::LlmCall { rec, .. } => pending_llm = Some(rec.clone()),
            JournalEntry::BranchDecision {
                chosen,
                exploration,
                ..
            } => {
                queue.push_back((
                    pending_llm.take(),
                    RecordedDecision {
                        chosen: *chosen,
                        exploration: exploration.clone(),
                    },
                ));
            }
            JournalEntry::LeafCall {
                determinism: DeterminismClass::ReplayOnly,
                output,
                is_error,
                duration_ms,
                ..
            } => replay_only.push_back(ServedOutput {
                output: output.clone(),
                is_error: *is_error,
                duration_ms: *duration_ms,
            }),
            _ => {}
        }
    }

    let replay_transport = ReplayTransport {
        inner: transport,
        replay_only_queue: Arc::new(Mutex::new(replay_only)),
    };
    let clock = FixedClock::new(0);
    let mut executor = Executor {
        transport: &replay_transport,
        router: ReplayRouter { queue },
        codec,
        propagation,
        clock: &clock,
        leaves,
    };
    let cfg = RunConfig {
        seed,
        max_steps,
        max_llm_calls,
        input,
        journal_path: replay_journal_path.to_path_buf(),
    };
    // A journal error means the replay never actually began (e.g. the
    // replay journal path already exists, so create_new fails before any
    // step runs) — that must NOT be swallowed, or the comparator would read
    // a stale pre-existing file and rubber-stamp it Identical. Router /
    // transport / choice errors DO leave a partially-written fresh journal
    // and are reported structurally by the comparator, so those stay
    // swallowed.
    if let Err(crate::executor::ExecError::Journal(e)) = executor.run(tree, &cfg).await {
        return Err(ReplayError::Journal(e));
    }

    let replayed = JournalReader::read_all(replay_journal_path)?;
    Ok(compare(&recorded, &replayed))
}

/// Structural comparison with the exemption list applied.
pub fn compare(recorded: &[JournalEntry], replayed: &[JournalEntry]) -> ReplayReport {
    for (i, (rec, rep)) in recorded.iter().zip(replayed.iter()).enumerate() {
        let rv = strip_exempt(serde_json::to_value(rec).expect("entry serializes"));
        let pv = strip_exempt(serde_json::to_value(rep).expect("entry serializes"));
        let tag = rv
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("entry")
            .to_string();
        if let Some((path, expected, actual)) = first_diff(&rv, &pv, String::new()) {
            return ReplayReport::Diverged(Divergence {
                entry_index: i,
                field: format!("{tag}.{path}"),
                expected,
                actual,
            });
        }
    }
    if recorded.len() != replayed.len() {
        return ReplayReport::Diverged(Divergence {
            entry_index: recorded.len().min(replayed.len()),
            field: "journal.length".into(),
            expected: recorded.len().to_string(),
            actual: replayed.len().to_string(),
        });
    }
    ReplayReport::Identical {
        entries: recorded.len(),
    }
}

fn strip_exempt(mut v: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = v.as_object_mut() {
        obj.remove("ts_ms");
        obj.remove("duration_ms");
    }
    v
}

fn truncate_display(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.chars().count() > 500 {
        let cut: String = s.chars().take(500).collect();
        format!("{cut}…")
    } else {
        s
    }
}

/// First differing field between two values, walking objects in sorted key
/// order (serde_json maps are BTreeMaps, so iteration is already sorted).
fn first_diff(
    expected: &serde_json::Value,
    actual: &serde_json::Value,
    path: String,
) -> Option<(String, String, String)> {
    use serde_json::Value;
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            let keys: std::collections::BTreeSet<&String> = e.keys().chain(a.keys()).collect();
            for k in keys {
                let sub = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                match (e.get(k), a.get(k)) {
                    (Some(ev), Some(av)) => {
                        if let Some(d) = first_diff(ev, av, sub) {
                            return Some(d);
                        }
                    }
                    (Some(ev), None) => {
                        return Some((sub, truncate_display(ev), "<missing>".into()));
                    }
                    (None, Some(av)) => {
                        return Some((sub, "<missing>".into(), truncate_display(av)));
                    }
                    (None, None) => unreachable!(),
                }
            }
            None
        }
        (Value::Array(e), Value::Array(a)) => {
            for (i, (ev, av)) in e.iter().zip(a.iter()).enumerate() {
                if let Some(d) = first_diff(ev, av, format!("{path}[{i}]")) {
                    return Some(d);
                }
            }
            if e.len() != a.len() {
                return Some((
                    format!("{path}.len"),
                    e.len().to_string(),
                    a.len().to_string(),
                ));
            }
            None
        }
        (e, a) => {
            if e == a {
                None
            } else {
                Some((path, truncate_display(e), truncate_display(a)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::journal::JournalWriter;
    use crate::router::ScriptedRouter;
    use crate::testutil::{
        FakeTransport, ScriptedLlmRouter, echo_tree, hybrid_tree, leaf_registry, volatile_tree,
    };
    use leaf_traits::{FullLog, ToonCodec};

    async fn record_echo(
        dir: &tempfile::TempDir,
        transport: &FakeTransport,
    ) -> (Tree, BTreeMap<String, LeafSpec>, std::path::PathBuf) {
        let tree = echo_tree();
        let leaves = leaf_registry();
        let clock = FixedClock::new(1_000);
        let mut exec = Executor {
            transport,
            router: ScriptedRouter::new([0usize, 1]),
            codec: &ToonCodec,
            propagation: &FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let cfg = RunConfig {
            seed: 42,
            max_steps: 16,
            max_llm_calls: 12,
            input: "hello world".into(),
            journal_path: dir.path().join("recorded.jsonl"),
        };
        exec.run(&tree, &cfg).await.unwrap();
        (tree, leaves, cfg.journal_path)
    }

    #[tokio::test]
    async fn replay_of_deterministic_run_is_identical() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport::deterministic();
        let (tree, leaves, recorded) = record_echo(&dir, &transport).await;
        let report = replay(
            &recorded,
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
            ReplayReport::Diverged(d) => panic!("unexpected divergence: {d}"),
        }
    }

    #[tokio::test]
    async fn replay_reproduces_deterministic_sequence_picks() {
        // Hybrid tree: root LLM -> [ hybrid SEQUENCE: fetch, classify LLM -> arm ].
        // The Sequence picks emit NO BranchDecision, so replay must REPRODUCE
        // them from the tree (not serve them from the journal) while the two LLM
        // decisions (root + classify) replay from the journal. An Identical
        // replay proves the sequence cursor is reconstructed exactly.
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport::deterministic();
        let tree = hybrid_tree();
        let leaves = leaf_registry();
        let clock = FixedClock::new(0);
        let mut exec = Executor {
            transport: &transport,
            router: ScriptedRouter::new([0usize, 1]),
            codec: &ToonCodec,
            propagation: &FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let cfg = RunConfig {
            seed: 42,
            max_steps: 16,
            max_llm_calls: 12,
            input: "hello world".into(),
            journal_path: dir.path().join("recorded.jsonl"),
        };
        exec.run(&tree, &cfg).await.unwrap();

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
            // start, decision(root), leaf(fetch), decision(classify), leaf(arm), finish
            ReplayReport::Identical { entries } => assert_eq!(entries, 6),
            ReplayReport::Diverged(d) => panic!("hybrid replay diverged: {d}"),
        }
    }

    #[tokio::test]
    async fn replay_serves_replay_only_leaves_from_journal_without_live_calls() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport::deterministic();
        let tree = volatile_tree();
        let leaves = leaf_registry();
        let clock = FixedClock::new(0);
        let mut exec = Executor {
            transport: &transport,
            router: ScriptedRouter::new([0usize, 0, 1]),
            codec: &ToonCodec,
            propagation: &FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let cfg = RunConfig {
            seed: 7,
            max_steps: 16,
            max_llm_calls: 12,
            input: "net".into(),
            journal_path: dir.path().join("recorded.jsonl"),
        };
        exec.run(&tree, &cfg).await.unwrap();
        let live_calls_after_record = transport.volatile_calls();
        assert_eq!(live_calls_after_record, 2);

        // The volatile leaf returns a different value on every live call, so
        // an Identical replay proves the outputs came from the journal…
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
        assert!(
            matches!(report, ReplayReport::Identical { .. }),
            "replay-only outputs must be served from the journal"
        );
        // …and the counter proves no live call happened at all.
        assert_eq!(transport.volatile_calls(), live_calls_after_record);
    }

    #[tokio::test]
    async fn tampered_output_diverges_at_exact_entry_and_field() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport::deterministic();
        let (tree, leaves, recorded) = record_echo(&dir, &transport).await;

        // Tamper: flip one byte of the recorded leaf output.
        let mut entries = JournalReader::read_all(&recorded).unwrap();
        let mut tampered_index = None;
        for (i, e) in entries.iter_mut().enumerate() {
            if let JournalEntry::LeafCall { output, .. } = e {
                output.replace_range(0..1, "X");
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
            ReplayReport::Diverged(d) => {
                assert_eq!(d.entry_index, tampered_index);
                assert_eq!(d.field, "leaf_call.output");
                assert!(d.expected.contains("Xcho:"));
                assert!(d.actual.contains("echo:"));
            }
            ReplayReport::Identical { .. } => panic!("tampering must diverge"),
        }
    }

    #[tokio::test]
    async fn replay_reproduces_llm_records_and_totals() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport::deterministic();
        let tree = echo_tree();
        let leaves = leaf_registry();
        let clock = FixedClock::new(0);
        let mut exec = Executor {
            transport: &transport,
            router: ScriptedLlmRouter::new([0usize, 1]),
            codec: &ToonCodec,
            propagation: &FullLog,
            clock: &clock,
            leaves: &leaves,
        };
        let cfg = RunConfig {
            seed: 9,
            max_steps: 16,
            max_llm_calls: 12,
            input: "task".into(),
            journal_path: dir.path().join("recorded.jsonl"),
        };
        let result = exec.run(&tree, &cfg).await.unwrap();
        assert_eq!(result.llm_calls, 2);
        assert!(result.total_cost_microusd > 0);

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
        assert!(matches!(report, ReplayReport::Identical { .. }));
    }

    #[tokio::test]
    async fn second_replay_to_same_path_errors_not_rubber_stamps() {
        // Regression for the critical: a pre-existing replay journal must make
        // replay() return Err (create_new fails), never read the stale file
        // and report Identical.
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport::deterministic();
        let (tree, leaves, recorded) = record_echo(&dir, &transport).await;
        let replay_path = dir.path().join("replayed.jsonl");
        let first = replay(
            &recorded,
            &replay_path,
            &tree,
            &transport,
            &ToonCodec,
            &FullLog,
            &leaves,
        )
        .await
        .unwrap();
        assert!(matches!(first, ReplayReport::Identical { .. }));
        // Same replay path again → must Err, not silently pass on the stale file.
        let second = replay(
            &recorded,
            &replay_path,
            &tree,
            &transport,
            &ToonCodec,
            &FullLog,
            &leaves,
        )
        .await;
        assert!(
            matches!(second, Err(ReplayError::Journal(_))),
            "second replay to an existing path must error, got {second:?}"
        );
    }

    #[tokio::test]
    async fn wrong_codec_on_replay_diverges_on_run_started() {
        let dir = tempfile::tempdir().unwrap();
        let transport = FakeTransport::deterministic();
        let (tree, leaves, recorded) = record_echo(&dir, &transport).await;
        let report = replay(
            &recorded,
            &dir.path().join("replayed.jsonl"),
            &tree,
            &transport,
            &leaf_traits::JsonCodec, // recorded with ToonCodec
            &FullLog,
            &leaves,
        )
        .await
        .unwrap();
        match report {
            ReplayReport::Diverged(d) => {
                assert_eq!(d.entry_index, 0);
                assert_eq!(d.field, "run_started.codec");
            }
            ReplayReport::Identical { .. } => panic!("codec mismatch must diverge"),
        }
    }
}

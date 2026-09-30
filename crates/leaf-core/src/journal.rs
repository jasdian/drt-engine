//! Append-only JSONL journal.
//!
//! Discipline: one compact-JSON entry per line, flushed after every entry so
//! a crashed run leaves a valid prefix; NO floating-point fields anywhere
//! (money in micro-USD u64, epsilon in ppm u32) so float-formatting
//! instability cannot exist; outputs stored verbatim as JSON strings (JSON
//! string escaping round-trips any UTF-8 exactly — no base64 needed).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use leaf_traits::DeterminismClass;
use serde::{Deserialize, Serialize};

use crate::router::{ExplorationRecord, LlmCallRecord};

/// Journal schema version. Bump on any breaking shape change.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Done,
    MaxSteps,
    MaxLlmCalls,
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalEntry {
    RunStarted {
        /// hex of derive_seed(seed, u32::MAX) — deterministic, not a uuid,
        /// so replay produces the same id.
        run_id: String,
        seed: u64,
        /// sha256 of the canonical JSON of the tree; asserted on replay.
        tree_digest: String,
        input: String,
        /// Codec::name().
        codec: String,
        /// Propagation::name().
        propagation: String,
        format_version: u32,
        /// Informational only — replay-exempt.
        ts_ms: u64,
    },
    /// Emitted immediately before its BranchDecision.
    LlmCall {
        step: u32,
        #[serde(flatten)]
        rec: LlmCallRecord,
    },
    BranchDecision {
        step: u32,
        branch: String,
        /// sha256(goal \0 rendered_context \0 rendered_menu) — the real
        /// determinism check on Codec+Propagation+history.
        prompt_digest: String,
        /// Candidate names in menu order.
        candidates: Vec<String>,
        chosen: usize,
        exploration: Option<ExplorationRecord>,
    },
    LeafCall {
        step: u32,
        leaf: String,
        tool: String,
        /// Fully substituted args.
        input: serde_json::Value,
        /// Per-call derived seed iff the template used {seed}.
        seed: Option<u64>,
        determinism: DeterminismClass,
        /// Verbatim output text.
        output: String,
        is_error: bool,
        /// Informational only — replay-exempt.
        duration_ms: u64,
    },
    RunFinished {
        steps: u32,
        outcome: RunOutcome,
        total_cost_microusd: u64,
        total_input_tokens: u64,
        total_output_tokens: u64,
        /// Informational only — replay-exempt.
        ts_ms: u64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal io: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal line {line}: {source}")]
    Parse {
        line: usize,
        source: serde_json::Error,
    },
    #[error("journal serialize: {0}")]
    Serialize(serde_json::Error),
}

pub struct JournalWriter {
    out: BufWriter<File>,
}

impl JournalWriter {
    /// Create a NEW journal. Refuses to overwrite an existing file —
    /// journals are append-only artifacts.
    pub fn create(path: &Path) -> Result<Self, JournalError> {
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            out: BufWriter::new(file),
        })
    }

    /// Append one entry and flush, so a crash leaves a valid prefix.
    pub fn append(&mut self, entry: &JournalEntry) -> Result<(), JournalError> {
        let line = serde_json::to_string(entry).map_err(JournalError::Serialize)?;
        self.out.write_all(line.as_bytes())?;
        self.out.write_all(b"\n")?;
        self.out.flush()?;
        Ok(())
    }
}

pub struct JournalReader;

impl JournalReader {
    pub fn read_all(path: &Path) -> Result<Vec<JournalEntry>, JournalError> {
        let file = File::open(path)?;
        let mut entries = Vec::new();
        for (idx, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry = serde_json::from_str(&line).map_err(|source| JournalError::Parse {
                line: idx + 1,
                source,
            })?;
            entries.push(entry);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entries() -> Vec<JournalEntry> {
        vec![
            JournalEntry::RunStarted {
                run_id: "abc".into(),
                seed: 42,
                tree_digest: "d".repeat(64),
                input: "task".into(),
                codec: "toon.v1".into(),
                propagation: "full".into(),
                format_version: FORMAT_VERSION,
                ts_ms: 1000,
            },
            JournalEntry::LlmCall {
                step: 0,
                rec: LlmCallRecord {
                    model: "claude-sonnet-4-6".into(),
                    request_digest: "r".repeat(64),
                    response_text: "c1".into(),
                    stop_reason: "end_turn".into(),
                    input_tokens: 100,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                    output_tokens: 3,
                    cost_microusd: 345,
                    api_calls: 1,
                    parse_fallback: false,
                },
            },
            JournalEntry::BranchDecision {
                step: 0,
                branch: "root".into(),
                prompt_digest: "p".repeat(64),
                candidates: vec!["jq".into(), "done".into()],
                chosen: 0,
                exploration: Some(ExplorationRecord {
                    epsilon_ppm: 100_000,
                    roll_ppm: 999_999,
                    explored: false,
                    pick: None,
                }),
            },
            JournalEntry::LeafCall {
                step: 0,
                leaf: "jq".into(),
                tool: "jq".into(),
                input: serde_json::json!({"expr": ".a", "input": "{\"a\":1}"}),
                seed: Some(7),
                determinism: DeterminismClass::Deterministic,
                output: "1\n".into(),
                is_error: false,
                duration_ms: 12,
            },
            JournalEntry::RunFinished {
                steps: 2,
                outcome: RunOutcome::Done,
                total_cost_microusd: 345,
                total_input_tokens: 100,
                total_output_tokens: 3,
                ts_ms: 2000,
            },
        ]
    }

    #[test]
    fn write_read_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let entries = sample_entries();
        {
            let mut w = JournalWriter::create(&path).unwrap();
            for e in &entries {
                w.append(e).unwrap();
            }
        }
        let back = JournalReader::read_all(&path).unwrap();
        assert_eq!(entries, back);
    }

    #[test]
    fn create_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        std::fs::write(&path, "existing").unwrap();
        assert!(matches!(
            JournalWriter::create(&path),
            Err(JournalError::Io(_))
        ));
    }

    #[test]
    fn parse_error_reports_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run.jsonl");
        let good = serde_json::to_string(&sample_entries()[0]).unwrap();
        std::fs::write(&path, format!("{good}\nnot json\n")).unwrap();
        match JournalReader::read_all(&path) {
            Err(JournalError::Parse { line, .. }) => assert_eq!(line, 2),
            other => panic!("expected parse error, got {other:?}"),
        }
    }

    #[test]
    fn no_float_tokens_in_serialized_entries() {
        // Guard the house rule: journal lines must never contain JSON floats.
        for e in sample_entries() {
            let line = serde_json::to_string(&e).unwrap();
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            fn assert_no_floats(v: &serde_json::Value) {
                match v {
                    serde_json::Value::Number(n) => assert!(n.is_u64() || n.is_i64()),
                    serde_json::Value::Array(a) => a.iter().for_each(assert_no_floats),
                    serde_json::Value::Object(o) => o.values().for_each(assert_no_floats),
                    _ => {}
                }
            }
            assert_no_floats(&v);
        }
    }
}

//! Per-run artifact layout + the completion gate that makes `run` resumable.

use std::path::PathBuf;

use anyhow::Result;
use leaf_core::sha256_hex;
use serde::{Deserialize, Serialize};

use crate::checker::GradeResult;
use crate::tasks::TaskManifest;

#[derive(Debug, Serialize, Deserialize)]
pub struct RunMeta {
    pub task: String,
    pub condition: String,
    pub rep: u32,
    pub model: String,
    pub seed: u64,
    /// sha256(task prompt \0 condition \0 model \0 tree_digest) — a run is
    /// "complete" iff its grade exists AND this hash matches the current config.
    pub config_hash: String,
    pub started_at_ms: u64,
    pub outcome: String,
    pub llm_calls: u32,
    pub cost_microusd: u64,
    pub cassette_misses: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UsageRecord {
    pub cost_microusd: u64,
    pub llm_calls: u32,
    /// Fresh (non-cached) input tokens.
    pub input_tokens: u64,
    /// Cache-write and cache-read input tokens, kept separate so the report
    /// can re-price at a different table without re-reading the journal.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

pub fn config_hash(task: &TaskManifest, condition: &str, model: &str) -> String {
    sha256_hex(
        format!(
            "{}\0{}\0{}\0{}",
            task.prompt,
            condition,
            model,
            task.tree.digest()
        )
        .as_bytes(),
    )
}

pub struct RunPaths {
    pub dir: PathBuf,
    pub meta: PathBuf,
    pub journal: PathBuf,
    pub answer: PathBuf,
    pub grade: PathBuf,
    pub usage: PathBuf,
}

impl RunPaths {
    pub fn at(runs_root: &std::path::Path, condition: &str, task: &str, rep: u32) -> Self {
        let dir = runs_root.join(condition).join(task).join(rep.to_string());
        Self {
            meta: dir.join("meta.json"),
            journal: dir.join("journal.jsonl"),
            answer: dir.join("answer.txt"),
            grade: dir.join("grade.json"),
            usage: dir.join("usage.json"),
            dir,
        }
    }

    /// A run is complete iff grade.json exists and meta's config_hash matches.
    pub fn is_complete(&self, expected_hash: &str) -> bool {
        if !self.grade.exists() {
            return false;
        }
        let Ok(text) = std::fs::read_to_string(&self.meta) else {
            return false;
        };
        let Ok(meta) = serde_json::from_str::<RunMeta>(&text) else {
            return false;
        };
        if meta.config_hash != expected_hash {
            return false;
        }
        // A cell is complete only if the run reached a VALID terminal state.
        // Infrastructure faults (an HTTP Error, a Timeout, a runtime Error such
        // as a dropped transport) are not benchmark results — a resumed `run`
        // re-executes them once the fault is fixed. It never re-rolls a validly
        // completed run (Done / MaxSteps / MaxLlmCalls), even a graded fail, so
        // this cannot be used to fish for passes.
        let o = meta.outcome.as_str();
        o.starts_with("Done") || o.starts_with("MaxSteps") || o.starts_with("MaxLlmCalls")
    }

    pub fn write_all(
        &self,
        meta: &RunMeta,
        answer: &str,
        grade: &GradeResult,
        usage: &UsageRecord,
    ) -> Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(&self.meta, serde_json::to_string_pretty(meta)?)?;
        std::fs::write(&self.answer, answer)?;
        std::fs::write(&self.grade, serde_json::to_string_pretty(grade)?)?;
        std::fs::write(&self.usage, serde_json::to_string_pretty(usage)?)?;
        Ok(())
    }

    pub fn load_grade(&self) -> Option<GradeResult> {
        serde_json::from_str(&std::fs::read_to_string(&self.grade).ok()?).ok()
    }
    pub fn load_meta(&self) -> Option<RunMeta> {
        serde_json::from_str(&std::fs::read_to_string(&self.meta).ok()?).ok()
    }
    pub fn load_answer(&self) -> Option<String> {
        std::fs::read_to_string(&self.answer).ok()
    }
}

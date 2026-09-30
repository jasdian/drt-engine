//! bench/bench.toml + the bench directory layout.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct BenchConfig {
    pub router_model: String,
    /// Hard global cap, integer micro-USD.
    pub budget_cap_microusd: u64,
    /// Per-bucket hard sub-caps, micro-USD.
    #[serde(default)]
    pub buckets: BTreeMap<String, u64>,
    pub run: RunConfigSection,
    /// Optional re-pricing table: model → per-MTok micro-USD prices. When a row
    /// matches `router_model`, `report` recomputes each run's cost from the
    /// journaled token split instead of the recorded cost — so a future token
    /// price is a one-line edit here + `leaf-bench report`, no re-run.
    #[serde(default)]
    pub prices: BTreeMap<String, PricesToml>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
pub struct PricesToml {
    /// micro-USD per million tokens.
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

impl From<PricesToml> for leaf_llm::Prices {
    fn from(p: PricesToml) -> Self {
        leaf_llm::Prices {
            input: p.input,
            cache_read: p.cache_read,
            cache_write: p.cache_write,
            output: p.output,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RunConfigSection {
    pub repeats: u32,
    /// Advisory default; the per-task `timeout_secs` is what the runner enforces.
    #[allow(dead_code)]
    pub timeout_secs: u64,
    /// Advisory default; the per-task `max_llm_calls` is what the runner enforces.
    #[allow(dead_code)]
    pub max_llm_calls: u32,
    /// Exploration epsilon for benchmark runs, ppm. 0 = off for headline
    /// numbers (exploration remains seeded+journaled where enabled; the
    /// acceptance tests cover it separately). Recorded in results validity notes.
    pub epsilon_ppm: u32,
    /// Base seed; run seed = base + rep index (deterministic, journaled).
    pub seed_base: u64,
    /// Pre-registered shrink order if the ledger projects a budget overrun
    /// (advisory — documented in bench.toml, applied by the operator).
    #[serde(default)]
    #[allow(dead_code)]
    pub task_shrink_order: Vec<String>,
}

/// Everything is anchored on the repo root (the directory holding bench/).
pub struct BenchPaths {
    pub root: PathBuf,
    pub bench: PathBuf,
    pub tasks: PathBuf,
    pub fixtures: PathBuf,
    pub cassette: PathBuf,
    /// Separate cassette for the live-world control diagnostic (record-on-miss
    /// against real GitHub) — kept isolated so the primary frozen cassette that
    /// every other condition replays stays byte-identical.
    pub cassette_live: PathBuf,
    pub ground_truth: PathBuf,
    /// Same-time-window ground truth captured live for fair grading of the
    /// live-world control (the frozen GT drifts for volatile tasks).
    pub ground_truth_live: PathBuf,
    pub artifacts: PathBuf,
    pub manifests: PathBuf,
    pub jail: PathBuf,
    pub budget_file: PathBuf,
    pub docs_experiments: PathBuf,
}

impl BenchPaths {
    pub fn at(root: PathBuf) -> Self {
        let bench = root.join("bench");
        Self {
            tasks: bench.join("tasks"),
            fixtures: bench.join("fixtures"),
            cassette: bench.join("cassette"),
            cassette_live: bench.join("cassette-live-control"),
            ground_truth: bench.join("ground-truth"),
            ground_truth_live: bench.join("ground-truth-live"),
            artifacts: bench.join("artifacts"),
            manifests: bench.join("artifacts").join("manifests"),
            jail: bench.join("artifacts").join("jail"),
            budget_file: bench.join("budget.jsonl"),
            docs_experiments: root.join("docs").join("experiments"),
            bench,
            root,
        }
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            &self.tasks,
            &self.fixtures,
            &self.cassette,
            &self.cassette_live,
            &self.ground_truth,
            &self.ground_truth_live,
            &self.artifacts,
            &self.manifests,
            &self.jail,
            &self.docs_experiments,
        ] {
            std::fs::create_dir_all(d).with_context(|| format!("mkdir {}", d.display()))?;
        }
        Ok(())
    }
}

impl BenchConfig {
    pub fn load(paths: &BenchPaths) -> Result<Self> {
        let path = paths.bench.join("bench.toml");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

/// A resolved LLM credential: which provider, the key, and where it came from.
pub struct ResolvedKey {
    pub provider: leaf_llm::Provider,
    pub key: String,
    pub source: String,
}

/// Read `<VAR>` from the process env (wins) or the gitignored `.env` file.
fn read_var(paths: &BenchPaths, var: &str) -> Option<(String, String)> {
    if let Ok(k) = std::env::var(var)
        && !k.trim().is_empty()
    {
        return Some((k, "process environment".into()));
    }
    let text = std::fs::read_to_string(paths.root.join(".env")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix(&format!("{var}="))
            && let v = v.trim().trim_matches('"')
            && !v.is_empty()
            && !v.ends_with("...")
        {
            return Some((v.to_string(), ".env file".into()));
        }
    }
    None
}

/// Resolve which LLM to use. An OpenAI key selects OpenAI; otherwise Anthropic.
/// (The `sk-ant-oat…` OAuth token is accepted — the Anthropic client uses
/// Bearer auth for it — but note it may lack Messages-API access.)
pub fn resolve_key(paths: &BenchPaths) -> Option<ResolvedKey> {
    if let Some((key, source)) = read_var(paths, "OPENAI_API_KEY") {
        return Some(ResolvedKey {
            provider: leaf_llm::Provider::OpenAi,
            key,
            source,
        });
    }
    if let Some((key, source)) = read_var(paths, "ANTHROPIC_API_KEY") {
        return Some(ResolvedKey {
            provider: leaf_llm::Provider::Anthropic,
            key,
            source,
        });
    }
    None
}

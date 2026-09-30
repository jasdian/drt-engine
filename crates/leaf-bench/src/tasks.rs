//! Task manifests: bench/tasks/<id>.toml — the runtime↔bench contract.

use std::path::Path;

use anyhow::{Context, Result, bail};
use leaf_core::Tree;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct RawTask {
    id: String,
    category: String,
    prompt: String,
    max_llm_calls: u32,
    timeout_secs: u64,
    leaves: Vec<String>,
    reference_route: Vec<usize>,
    /// Inline tree, OR `tree_file` (a shared general tree relative to the task file).
    #[serde(default)]
    tree_json: Option<String>,
    #[serde(default)]
    tree_file: Option<String>,
    checker: RawChecker,
    /// AXI-style loose "KNOWN FACTS" band for the LLM-judge grader (`judge-grade`).
    #[serde(default)]
    grading_hint: Option<String>,
    /// Structured entity params (owner, name, number, term, tag) exposed to the
    /// tree as `{input.field}` — parameterizes a general tree per task instance.
    #[serde(default)]
    input: std::collections::BTreeMap<String, String>,
    /// The correct root capability index — an INDEPENDENT routing label (not the
    /// tree's own output) used to score routing accuracy.
    #[serde(default)]
    expected_capability: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RawChecker {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    must: Vec<String>,
    #[serde(default)]
    forbid: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum CheckerSpec {
    /// Every string/number scalar VALUE in the ground-truth JSON object must
    /// appear (case-insensitive substring / decimal rendering) in the answer.
    /// Bools are skipped (recorded as skipped assertions).
    ContainsValues,
    Regex {
        must: Vec<String>,
        forbid: Vec<String>,
    },
}

#[derive(Debug)]
pub struct TaskManifest {
    pub id: String,
    pub category: String,
    pub prompt: String,
    pub max_llm_calls: u32,
    pub timeout_secs: u64,
    pub leaves: Vec<String>,
    pub reference_route: Vec<usize>,
    pub tree: Tree,
    pub checker: CheckerSpec,
    pub grading_hint: Option<String>,
    pub input: std::collections::BTreeMap<String, String>,
    pub expected_capability: Option<usize>,
}

impl TaskManifest {
    /// The run input for the tree runtime: TERSE `key: value` lines (the codec
    /// style, NOT JSON) combining the NL `prompt` (so the router can route on
    /// it) with the structured entity params (so leaves parameterize via
    /// `{input.field}`). The free-driving control receives only `prompt`.
    pub fn run_input(&self) -> String {
        let mut s = format!("prompt: {}", self.prompt);
        for (k, v) in &self.input {
            s.push_str(&format!("\n{k}: {v}"));
        }
        s
    }
}

impl TaskManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let raw: RawTask =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let tree_src = match (&raw.tree_file, &raw.tree_json) {
            (Some(file), _) => {
                let tree_path = path.parent().unwrap_or(Path::new(".")).join(file);
                std::fs::read_to_string(&tree_path)
                    .with_context(|| format!("reading tree_file {}", tree_path.display()))?
            }
            (None, Some(inline)) => inline.clone(),
            (None, None) => bail!("task {}: needs tree_json or tree_file", raw.id),
        };
        let tree: Tree = serde_json::from_str(&tree_src)
            .with_context(|| format!("parsing tree of {}", raw.id))?;
        let checker = match raw.checker.kind.as_str() {
            "contains_values" => CheckerSpec::ContainsValues,
            "regex" => CheckerSpec::Regex {
                must: raw.checker.must,
                forbid: raw.checker.forbid,
            },
            other => bail!("task {}: unknown checker type '{other}'", raw.id),
        };
        if raw.reference_route.is_empty() {
            bail!("task {}: empty reference_route", raw.id);
        }
        Ok(Self {
            id: raw.id,
            category: raw.category,
            prompt: raw.prompt,
            max_llm_calls: raw.max_llm_calls,
            timeout_secs: raw.timeout_secs,
            leaves: raw.leaves,
            reference_route: raw.reference_route,
            tree,
            checker,
            grading_hint: raw.grading_hint,
            input: raw.input,
            expected_capability: raw.expected_capability,
        })
    }

    pub fn load_all(tasks_dir: &Path) -> Result<Vec<Self>> {
        let mut tasks = Vec::new();
        let mut paths: Vec<_> = std::fs::read_dir(tasks_dir)
            .with_context(|| format!("reading {}", tasks_dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        paths.sort();
        for p in paths {
            tasks.push(Self::load(&p)?);
        }
        if tasks.is_empty() {
            bail!("no task manifests in {}", tasks_dir.display());
        }
        Ok(tasks)
    }
}

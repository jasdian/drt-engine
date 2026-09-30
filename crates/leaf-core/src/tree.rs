//! Trees are static Rust structs with serde derives: that makes them
//! digestable (`tree_digest`) and JSON-loadable later, without a config
//! parser now.

use leaf_traits::CandidateInfo;
use serde::{Deserialize, Serialize};

use crate::digest::sha256_hex;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tree {
    pub name: String,
    pub version: String,
    pub root: Branch,
}

impl Tree {
    /// sha256 of the canonical JSON of the tree. serde_json's default map is
    /// a BTreeMap (sorted keys) and struct fields serialize in declaration
    /// order, so this is byte-stable.
    pub fn digest(&self) -> String {
        sha256_hex(&serde_json::to_vec(self).expect("tree serializes"))
    }

    /// Structural validation run before a tree executes: currently the
    /// no-floats rule on every leaf's args template.
    pub fn validate(&self) -> Result<(), TreeError> {
        fn walk(node: &Node) -> Result<(), TreeError> {
            match node {
                Node::Leaf(l) => l.args.validate_no_floats(),
                Node::Branch(b) => b.candidates.iter().try_for_each(walk),
                Node::Done { .. } => Ok(()),
            }
        }
        self.root.candidates.iter().try_for_each(walk)
    }
}

/// How a branch chooses among its candidates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMode {
    /// The Router (LLM) picks among candidates — a genuine decision. Only these
    /// picks cost an LLM call and are journaled as a `BranchDecision`.
    #[default]
    Llm,
    /// Deterministic in-order walk: the executor advances a cursor through the
    /// candidates with NO router call, NO LLM call, and NO `BranchDecision`
    /// (it is reproduced from the tree on replay). This is the "hybrid tree"
    /// lever — a forced pipeline (fetch→reduce→done) and the deterministic arms
    /// of a hybrid capability run the LLM zero times; the LLM is spent only on
    /// genuine choices (an `Llm` branch).
    Sequence,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum Node {
    Branch(Branch),
    Leaf(LeafNode),
    /// Explicit terminal candidate: picking it ends the enclosing branch. When
    /// `terminal` is set, it ends the WHOLE run (used by a hybrid capability's
    /// finish so the run stops there instead of popping back to the root for
    /// another rubber-stamp routing decision).
    Done {
        name: String,
        description: String,
        #[serde(default)]
        terminal: bool,
    },
}

impl Node {
    pub fn name(&self) -> &str {
        match self {
            Node::Branch(b) => &b.name,
            Node::Leaf(l) => &l.name,
            Node::Done { name, .. } => name,
        }
    }
    pub fn description(&self) -> &str {
        match self {
            Node::Branch(b) => &b.goal,
            Node::Leaf(l) => &l.description,
            Node::Done { description, .. } => description,
        }
    }
    pub fn candidate_info(&self, index: usize) -> CandidateInfo {
        CandidateInfo {
            id: format!("c{index}"),
            name: self.name().to_string(),
            description: self.description().to_string(),
        }
    }
}

/// A branch is a loop head: the router picks among candidates; a Leaf pick
/// runs the tool and control returns to the same branch; a Branch pick
/// descends; a Done pick terminates the branch.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    /// The routing question put to the Router (only used when `route == Llm`).
    pub goal: String,
    /// Whether the LLM routes this branch (`Llm`, the default) or the executor
    /// walks it deterministically (`Sequence`). `#[serde(default)]` keeps old
    /// JSON (and every Llm branch) terse.
    #[serde(default)]
    pub route: RouteMode,
    pub candidates: Vec<Node>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LeafNode {
    pub name: String,
    /// What the Router sees in the menu.
    pub description: String,
    /// LeafSpec id in the runtime registry.
    pub leaf: String,
    /// MCP tool name on that leaf.
    pub tool: String,
    /// Deterministic input construction — the LLM routes, it never authors
    /// arguments.
    pub args: ArgsTemplate,
}

/// A JSON value whose string leaves may contain the placeholders `{input}`
/// (the run input), `{last_output}` (most recent leaf output text, "" if
/// none) and `{seed}` (per-call derived seed, decimal u64). Substitution is
/// plain string replacement; the fully substituted args are journaled, so
/// replay re-derives and asserts them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ArgsTemplate(pub serde_json::Value);

#[derive(Debug, thiserror::Error)]
pub enum TreeError {
    #[error("args template contains a float in {0} — journal fields must be integer-only")]
    FloatInArgsTemplate(String),
}

impl ArgsTemplate {
    /// Enforce the no-floats journal rule at the one place a float can enter:
    /// `build()` only ever produces strings, so a float-free template
    /// guarantees a float-free journaled `LeafCall.input`.
    pub fn validate_no_floats(&self) -> Result<(), TreeError> {
        fn walk(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::Number(n) => n.is_u64() || n.is_i64(),
                serde_json::Value::Array(a) => a.iter().all(walk),
                serde_json::Value::Object(o) => o.values().all(walk),
                _ => true,
            }
        }
        if walk(&self.0) {
            Ok(())
        } else {
            Err(TreeError::FloatInArgsTemplate(self.0.to_string()))
        }
    }

    pub fn uses_seed(&self) -> bool {
        fn walk(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::String(s) => s.contains("{seed}"),
                serde_json::Value::Array(a) => a.iter().any(walk),
                serde_json::Value::Object(o) => o.values().any(walk),
                _ => false,
            }
        }
        walk(&self.0)
    }

    /// Placeholders: `{input}` (run input), `{last_output}` (most recent leaf
    /// output text), `{source}` (most recent RAW data-source output — the most
    /// recent leaf whose tool is not a transform; the executor computes it), and
    /// `{seed}` (per-call derived seed). `{source}` lets a transform re-read the
    /// original fetch on a re-pick instead of its own reduced output, making
    /// re-running idempotent.
    pub fn build(
        &self,
        input: &str,
        last_output: &str,
        source: &str,
        seed: u64,
    ) -> serde_json::Value {
        // `{input.field}`: the run input is TERSE `key: value` lines (the codec
        // encoding, NOT JSON), so a general (non-hardcoded) tree parameterizes
        // its leaf args from the task's structured input. `{input}` (the whole
        // raw input) still works — it never matches `{input.` (different literal).
        let fields: Vec<(String, String)> = input
            .lines()
            .filter_map(|line| {
                let (k, v) = line.split_once(": ")?;
                let k = k.trim();
                (!k.is_empty()).then(|| (format!("{{input.{k}}}"), v.to_string()))
            })
            .collect();
        fn walk(
            v: &serde_json::Value,
            input: &str,
            last_output: &str,
            source: &str,
            seed: &str,
            fields: &[(String, String)],
        ) -> serde_json::Value {
            match v {
                serde_json::Value::String(s) => {
                    let mut s = s.clone();
                    for (ph, val) in fields {
                        s = s.replace(ph, val);
                    }
                    serde_json::Value::String(
                        s.replace("{input}", input)
                            .replace("{last_output}", last_output)
                            .replace("{source}", source)
                            .replace("{seed}", seed),
                    )
                }
                serde_json::Value::Array(a) => serde_json::Value::Array(
                    a.iter()
                        .map(|x| walk(x, input, last_output, source, seed, fields))
                        .collect(),
                ),
                serde_json::Value::Object(o) => serde_json::Value::Object(
                    o.iter()
                        .map(|(k, x)| (k.clone(), walk(x, input, last_output, source, seed, fields)))
                        .collect(),
                ),
                other => other.clone(),
            }
        }
        walk(&self.0, input, last_output, source, &seed.to_string(), &fields)
    }
}

/// Per-call seed derivation: documented, frozen, golden-tested.
/// `u64::from_le_bytes(sha256(run_seed_le || step_le)[..8])`.
pub fn derive_seed(run_seed: u64, step: u32) -> u64 {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(run_seed.to_le_bytes());
    hasher.update(step.to_le_bytes());
    let out = hasher.finalize();
    u64::from_le_bytes(out[..8].try_into().expect("sha256 is 32 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_template_substitutes_everywhere() {
        let t = ArgsTemplate(serde_json::json!({
            "expr": ".title",
            "input": "{input}",
            "nested": {"prev": "prefix {last_output} suffix"},
            "src": "from {source}",
            "list": ["{seed}", 42, true],
        }));
        assert!(t.uses_seed());
        let built = t.build("THE-INPUT", "PREV", "RAW-FETCH", 7);
        assert_eq!(built["input"], "THE-INPUT");
        assert_eq!(built["nested"]["prev"], "prefix PREV suffix");
        assert_eq!(built["src"], "from RAW-FETCH");
        assert_eq!(built["list"][0], "7");
        assert_eq!(built["list"][1], 42);
    }

    #[test]
    fn args_template_without_seed() {
        let t = ArgsTemplate(serde_json::json!({"x": "{input}"}));
        assert!(!t.uses_seed());
    }

    #[test]
    fn args_template_substitutes_input_fields() {
        // A general (non-hardcoded) tree parameterizes leaf args from the run
        // input's TERSE `key: value` lines via `{input.field}` (NOT JSON);
        // `{input}` (whole) still works.
        let t = ArgsTemplate(serde_json::json!({
            "endpoint": "repos/{input.owner}/{input.name}/pulls/{input.number}",
            "whole": "{input}",
        }));
        let input = "owner: openclaw\nname: openclaw\nnumber: 50782\nprompt: view it";
        let built = t.build(input, "", "", 0);
        assert_eq!(built["endpoint"], "repos/openclaw/openclaw/pulls/50782");
        assert_eq!(built["whole"], input);
        // A missing field leaves the placeholder untouched (no panic).
        let t2 = ArgsTemplate(serde_json::json!({"x": "{input.absent}"}));
        assert_eq!(t2.build(input, "", "", 0)["x"], "{input.absent}");
        // A prompt value containing ": " keeps everything after the first colon.
        let t3 = ArgsTemplate(serde_json::json!({"p": "{input.prompt}"}));
        assert_eq!(
            t3.build("prompt: report: how many?", "", "", 0)["p"],
            "report: how many?"
        );
    }

    #[test]
    fn args_template_rejects_floats() {
        assert!(
            ArgsTemplate(serde_json::json!({"weight": 0.1}))
                .validate_no_floats()
                .is_err()
        );
        assert!(
            ArgsTemplate(serde_json::json!({"n": 42, "s": "ok"}))
                .validate_no_floats()
                .is_ok()
        );
        assert!(
            ArgsTemplate(serde_json::json!({"nested": [1, {"x": 1.5}]}))
                .validate_no_floats()
                .is_err()
        );
    }

    #[test]
    fn derive_seed_is_deterministic_and_spread() {
        assert_eq!(derive_seed(42, 0), derive_seed(42, 0));
        assert_ne!(derive_seed(42, 0), derive_seed(42, 1));
        assert_ne!(derive_seed(42, 0), derive_seed(43, 0));
    }

    #[test]
    fn derive_seed_golden() {
        // Frozen forever: changing the derivation breaks replay of old
        // journals. Values computed independently via
        // `printf <le bytes> | sha256sum` + first-8-bytes-as-LE-u64.
        assert_eq!(derive_seed(42, 0), 0x91491766afd15e19);
        assert_eq!(derive_seed(0, 0), 0xb43207b5f07bec15);
    }

    #[test]
    fn tree_digest_stable() {
        let tree = Tree {
            name: "t".into(),
            version: "1".into(),
            root: Branch {
                name: "root".into(),
                goal: "g".into(),
                route: RouteMode::Llm,
                candidates: vec![Node::Done {
                    name: "done".into(),
                    description: "d".into(),
                    terminal: false,
                }],
            },
        };
        assert_eq!(tree.digest(), tree.digest());
        let mut tree2 = tree.clone();
        tree2.version = "2".into();
        assert_ne!(tree.digest(), tree2.digest());
    }
}

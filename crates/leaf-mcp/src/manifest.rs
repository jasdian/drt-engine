//! The leaf-wrap manifest: how one CLI binary becomes a sandboxed MCP leaf
//! server with a determinism declaration. JSON — serde_json is already a dep
//! and "no custom DSL" is honored either way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use leaf_traits::DeterminismClass;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WrapManifest {
    /// Tool name advertised over MCP.
    pub name: String,
    pub description: String,
    /// The determinism contract this leaf declares. Network tools MUST
    /// declare `replay_only` (see docs/experiments/02-determinism.md).
    pub determinism: DeterminismClass,
    /// The wrapped binary. Must appear in `allowlist`.
    pub binary: PathBuf,
    /// Canonical binary paths this manifest may execute.
    pub allowlist: Vec<PathBuf>,
    /// argv template; `{key}` placeholders resolve from tool args. Each
    /// element becomes a single argv entry — never a shell string.
    pub args_template: Vec<String>,
    /// Tool-arg key whose value is written to the child's stdin, if any.
    #[serde(default)]
    pub stdin_from: Option<String>,
    pub timeout_ms: u64,
    /// Working-dir jail for the wrapped binary (created at startup).
    pub workdir: PathBuf,
    /// Explicit env allowlist on top of env_clear.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// JSON Schema for the tool's arguments (hand-rolled checks, no jsonschema dep).
    pub input_schema: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest io: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest parse: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("manifest invalid: {0}")]
    Invalid(String),
}

impl WrapManifest {
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path)?;
        let m: WrapManifest = serde_json::from_str(&text)?;
        m.validate()?;
        Ok(m)
    }

    /// Structural validation (no filesystem access — startup does that part).
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.name.is_empty() {
            return Err(ManifestError::Invalid("empty tool name".into()));
        }
        if !self.allowlist.contains(&self.binary) {
            return Err(ManifestError::Invalid(format!(
                "binary {} is not in its own allowlist",
                self.binary.display()
            )));
        }
        if self.input_schema.get("type").and_then(|t| t.as_str()) != Some("object") {
            return Err(ManifestError::Invalid(
                "input_schema must be an object schema".into(),
            ));
        }
        Ok(())
    }

    /// The manifest as exposed by the reserved `__leaf_manifest` tool: env
    /// values are redacted — they may hold tokens.
    pub fn redacted(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).expect("manifest serializes");
        if let Some(obj) = v.as_object_mut() {
            obj.remove("env");
        }
        v
    }

    /// Names of required args per the schema.
    pub fn required_args(&self) -> Vec<&str> {
        self.input_schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default()
    }

    /// Declared property names.
    pub fn property_names(&self) -> Vec<&str> {
        self.input_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|o| o.keys().map(|s| s.as_str()).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn jq_manifest_json(binary: &str, workdir: &str) -> String {
        serde_json::json!({
            "name": "jq",
            "description": "Run a jq filter over a JSON document supplied on stdin.",
            "determinism": "deterministic",
            "binary": binary,
            "allowlist": [binary],
            "args_template": ["-c", "{expr}"],
            "stdin_from": "input",
            "timeout_ms": 10000,
            "workdir": workdir,
            "env": {},
            "input_schema": {
                "type": "object",
                "properties": {
                    "expr": {"type": "string"},
                    "input": {"type": "string"}
                },
                "required": ["expr", "input"],
                "additionalProperties": false
            }
        })
        .to_string()
    }

    #[test]
    fn parses_and_validates() {
        let m: WrapManifest =
            serde_json::from_str(&jq_manifest_json("/usr/bin/jq", "/tmp/jail")).unwrap();
        m.validate().unwrap();
        assert_eq!(m.name, "jq");
        assert_eq!(m.determinism, DeterminismClass::Deterministic);
        assert_eq!(m.required_args(), vec!["expr", "input"]);
    }

    #[test]
    fn rejects_binary_outside_own_allowlist() {
        let mut m: WrapManifest =
            serde_json::from_str(&jq_manifest_json("/usr/bin/jq", "/tmp/jail")).unwrap();
        m.binary = "/usr/bin/curl".into();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn redacted_drops_env() {
        let mut m: WrapManifest =
            serde_json::from_str(&jq_manifest_json("/usr/bin/jq", "/tmp/jail")).unwrap();
        m.env.insert("TOKEN".into(), "secret".into());
        let red = m.redacted();
        assert!(red.get("env").is_none());
        assert!(!red.to_string().contains("secret"));
    }
}

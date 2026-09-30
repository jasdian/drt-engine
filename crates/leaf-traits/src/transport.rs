//! The Transport seam, stated honestly.
//!
//! rmcp owns the MCP wire. This trait exists so that (a) leaf-core never
//! depends on rmcp, (b) replay can substitute a journal-backed fake, and
//! (c) the demoted transport experiments (UDP/QUIC — disproven: one LLM call
//! dwarfs a TCP handshake by ~4 orders of magnitude) remain a seam rather
//! than code. That is its whole justification.
//!
//! The single production impl is `leaf_mcp::StdioMcpTransport`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::types::{DeterminismClass, LeafOutput};

/// How to spawn and talk to one leaf server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeafSpec {
    /// Registry key, e.g. "jq".
    pub id: String,
    /// Leaf server binary (e.g. the leaf-wrap binary). Must be allowlisted.
    pub command: PathBuf,
    /// e.g. ["--manifest", "manifests/jq.json"].
    pub args: Vec<String>,
    /// Explicit env allowlist ON TOP of env_clear — the child sees nothing else.
    pub env: BTreeMap<String, String>,
    /// Working-dir jail for the *server* process.
    pub cwd: Option<PathBuf>,
    pub call_timeout_ms: u64,
    /// Cross-checked against the leaf's own declaration at connect time.
    pub expected_determinism: DeterminismClass,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("binary not allowlisted: {0}")]
    NotAllowlisted(PathBuf),
    #[error("spawn failed: {0}")]
    Spawn(String),
    #[error("call timed out after {0} ms")]
    Timeout(u64),
    #[error("determinism mismatch: leaf declares {declared:?}, spec expects {expected:?}")]
    DeterminismMismatch {
        declared: DeterminismClass,
        expected: DeterminismClass,
    },
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// A connected leaf server.
pub trait LeafHandle: Send {
    /// As declared by the connected leaf itself.
    fn determinism(&self) -> DeterminismClass;
    fn call(
        &mut self,
        tool: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> impl Future<Output = Result<LeafOutput, TransportError>> + Send;
}

pub trait Transport: Send + Sync {
    type Handle: LeafHandle;
    fn connect(
        &self,
        spec: &LeafSpec,
    ) -> impl Future<Output = Result<Self::Handle, TransportError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaf_spec_round_trips_with_sorted_env() {
        let mut env = BTreeMap::new();
        env.insert("Z".to_string(), "26".to_string());
        env.insert("A".to_string(), "1".to_string());
        let spec = LeafSpec {
            id: "jq".into(),
            command: "/usr/bin/leaf-wrap".into(),
            args: vec!["--manifest".into(), "jq.json".into()],
            env,
            cwd: Some("/tmp/jail".into()),
            call_timeout_ms: 10_000,
            expected_determinism: DeterminismClass::Deterministic,
        };
        let json = serde_json::to_string(&spec).unwrap();
        // BTreeMap => sorted keys => stable bytes for digesting.
        assert!(json.find("\"A\"").unwrap() < json.find("\"Z\"").unwrap());
        let back: LeafSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }
}

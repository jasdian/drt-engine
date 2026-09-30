//! Shared vocabulary types. These shapes are journaled, so they must stay
//! serde-stable; no floats anywhere (see leaf-core journal discipline).

use serde::{Deserialize, Serialize};

/// The determinism contract a leaf declares and replay enforces.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeterminismClass {
    /// Pure/local tool: replay re-executes the call and asserts byte-identical output.
    Deterministic,
    /// Network/side-effecting tool: replay serves the output from the journal.
    ReplayOnly,
}

/// One item of run history — the input to Propagation/Codec rendering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HistoryItem {
    RunInput {
        text: String,
    },
    Decision {
        branch: String,
        chosen: String,
        explored: bool,
    },
    LeafOutput {
        leaf: String,
        tool: String,
        output: String,
        is_error: bool,
    },
}

/// What a Router sees about one candidate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateInfo {
    /// Stable menu id: "c0".."cN".
    pub id: String,
    /// Node name.
    pub name: String,
    /// Routing hint authored in the tree (trusted — never leaf output).
    pub description: String,
}

/// Result of one leaf tool call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LeafOutput {
    /// Verbatim tool output text (UTF-8; lossy-converted once at the leaf boundary).
    pub text: String,
    pub is_error: bool,
    /// Informational only — never asserted in replay, never rendered into prompts.
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determinism_class_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&DeterminismClass::Deterministic).unwrap(),
            "\"deterministic\""
        );
        assert_eq!(
            serde_json::to_string(&DeterminismClass::ReplayOnly).unwrap(),
            "\"replay_only\""
        );
        let back: DeterminismClass = serde_json::from_str("\"replay_only\"").unwrap();
        assert_eq!(back, DeterminismClass::ReplayOnly);
    }

    #[test]
    fn history_item_round_trip() {
        let items = vec![
            HistoryItem::RunInput {
                text: "do the thing".into(),
            },
            HistoryItem::Decision {
                branch: "root".into(),
                chosen: "jq".into(),
                explored: true,
            },
            HistoryItem::LeafOutput {
                leaf: "jq".into(),
                tool: "jq".into(),
                output: "{\"a\":1}\n".into(),
                is_error: false,
            },
        ];
        let json = serde_json::to_string(&items).unwrap();
        let back: Vec<HistoryItem> = serde_json::from_str(&json).unwrap();
        assert_eq!(items, back);
        assert!(json.contains("\"kind\":\"leaf_output\""));
    }
}

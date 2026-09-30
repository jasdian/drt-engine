//! Provider-neutral tool-loop vocabulary.
//!
//! The control loop (and any agentic loop) is written against these types so
//! the same code drives either Anthropic or OpenAI — each client translates
//! them to/from its own wire format. Keeps the runtime honest to its
//! "LLM is a swappable router" premise.

use serde::{Deserialize, Serialize};

/// A tool the model may call (name + JSON-Schema parameters).
#[derive(Clone, Debug)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A tool call the model emitted.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// A tool result the harness feeds back.
#[derive(Clone, Debug)]
pub struct ToolResult {
    pub id: String,
    pub output: String,
    pub is_error: bool,
}

/// One turn of a neutral tool-using conversation.
#[derive(Clone, Debug)]
pub enum ChatTurn {
    User(String),
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    ToolResults(Vec<ToolResult>),
}

/// Result of one model turn in a tool loop.
#[derive(Clone, Debug)]
pub struct ToolTurn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: String,
    pub usage: crate::anthropic::Usage,
}

//! Anthropic-backed routing layer: the only nondeterministic component of the
//! runtime, quarantined behind `leaf_core::Router` and journaled verbatim.

pub mod anthropic;
pub mod chat;
pub mod client;
pub mod explore;
pub mod openai;
pub mod pricing;
pub mod prompt;
pub mod router;

pub use anthropic::{AnthropicClient, Usage};
pub use chat::{ChatTurn, ToolCall, ToolResult, ToolSchema, ToolTurn};
pub use client::{LlmClient, Provider};
pub use explore::{EpsilonGreedy, verify_exploration_stream};
pub use openai::OpenAiClient;
pub use pricing::{
    GPT_4O_MINI, GPT_5_4_MINI_ESTIMATE, HAIKU_4_5, PRICE_TABLE, Prices, SONNET_4_6, cost_microusd,
};
pub use router::LlmRouter;

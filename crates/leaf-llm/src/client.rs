//! Provider-neutral client: the runtime treats the LLM as a swappable router,
//! so it holds an `LlmClient` that dispatches to either Anthropic or OpenAI.
//! Both back-ends produce the SAME neutral response types.

use leaf_core::RouterError;

use crate::anthropic::{AnthropicClient, MessagesResponse};
use crate::chat::{ChatTurn, ToolSchema, ToolTurn};
use crate::openai::OpenAiClient;
use crate::pricing::{self, Prices, SONNET_4_6};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    Anthropic,
    OpenAi,
}

pub enum LlmClient {
    Anthropic(AnthropicClient),
    OpenAi(OpenAiClient),
}

impl LlmClient {
    pub fn new(provider: Provider, api_key: String, model: String) -> Self {
        match provider {
            Provider::Anthropic => LlmClient::Anthropic(AnthropicClient::new(api_key, model)),
            Provider::OpenAi => LlmClient::OpenAi(OpenAiClient::new(api_key, model)),
        }
    }

    /// Test seam: point at a mock server.
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        let url = base_url.into();
        match self {
            LlmClient::Anthropic(c) => LlmClient::Anthropic(c.with_base_url(url)),
            LlmClient::OpenAi(c) => LlmClient::OpenAi(c.with_base_url(url)),
        }
    }

    pub fn model(&self) -> &str {
        match self {
            LlmClient::Anthropic(c) => &c.model,
            LlmClient::OpenAi(c) => &c.model,
        }
    }

    /// The pinned price table for this client's model; falls back to
    /// sonnet-4-6 with a warning for an unknown model.
    pub fn prices(&self) -> Prices {
        pricing::lookup(self.model()).unwrap_or_else(|| {
            tracing::warn!(
                model = self.model(),
                "no pinned prices; defaulting to sonnet-4-6"
            );
            SONNET_4_6
        })
    }

    pub async fn messages(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        stop_sequences: &[&str],
    ) -> Result<(MessagesResponse, Vec<u8>), RouterError> {
        match self {
            LlmClient::Anthropic(c) => c.messages(system, user, max_tokens, stop_sequences).await,
            LlmClient::OpenAi(c) => c.messages(system, user, max_tokens, stop_sequences).await,
        }
    }

    pub async fn count_tokens(&self, system: &str, user: &str) -> Result<u64, RouterError> {
        match self {
            LlmClient::Anthropic(c) => c.count_tokens(system, user).await,
            LlmClient::OpenAi(c) => c.count_tokens(system, user).await,
        }
    }

    pub async fn tool_turn(
        &self,
        system: &str,
        history: &[ChatTurn],
        tools: &[ToolSchema],
        max_tokens: u32,
    ) -> Result<ToolTurn, RouterError> {
        match self {
            LlmClient::Anthropic(c) => c.tool_turn(system, history, tools, max_tokens).await,
            LlmClient::OpenAi(c) => c.tool_turn(system, history, tools, max_tokens).await,
        }
    }
}

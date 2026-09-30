//! Raw-HTTP OpenAI Chat Completions client, producing the SAME neutral
//! response types as the Anthropic client so the runtime is provider-agnostic.
//!
//! Notes pinned from probing the live API (gpt-4o-mini, gpt-5.4-mini):
//! - use `max_completion_tokens` (not `max_tokens`);
//! - gpt-5* models REJECT `stop` and `temperature` — we send neither
//!   (the router's answer is a short id that parses fine without a stop);
//! - usage carries `prompt_tokens`, `completion_tokens`, and
//!   `prompt_tokens_details.cached_tokens`; reasoning tokens are already
//!   folded into `completion_tokens`.
//! - OpenAI has no free count-tokens endpoint, so `count_tokens` makes a
//!   minimal (1-completion-token) call and reads the exact `prompt_tokens`.

use leaf_core::RouterError;
use serde::Deserialize;

use crate::anthropic::{ContentBlock, MessagesResponse, Usage};
use crate::chat::{ChatTurn, ToolCall, ToolSchema, ToolTurn};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";

pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub model: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: OpenAiUsage,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
    #[serde(default)]
    finish_reason: String,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
}

#[derive(Deserialize)]
struct RawToolCall {
    id: String,
    function: RawFunction,
}

#[derive(Deserialize)]
struct RawFunction {
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize, Default)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: PromptDetails,
}

#[derive(Deserialize, Default)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: u64,
}

impl OpenAiUsage {
    /// Map OpenAI usage to the neutral `Usage`. OpenAI reports cached input in
    /// `cached_tokens`; there is no cache-write premium, so cache_creation = 0.
    fn to_neutral(&self) -> Usage {
        Usage {
            input_tokens: self
                .prompt_tokens
                .saturating_sub(self.prompt_tokens_details.cached_tokens),
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: self.prompt_tokens_details.cached_tokens,
            output_tokens: self.completion_tokens,
        }
    }
}

impl OpenAiClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
            model,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Reasoning models (gpt-5*, o1/o3/o4) bill hidden reasoning tokens against
    /// `max_completion_tokens`, so a tiny cap (the router's frozen 8) is
    /// exhausted by reasoning before the visible `cN` id is emitted → a hard
    /// 400. Detect them so we can raise the cap and turn reasoning off.
    fn is_reasoning_model(&self) -> bool {
        let m = &self.model;
        m.starts_with("gpt-5") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
    }

    /// Effective token cap for a routing/short call: give reasoning models real
    /// headroom (the visible answer stays a few tokens regardless).
    fn effective_max(&self, requested: u32) -> u32 {
        if self.is_reasoning_model() {
            requested.max(512)
        } else {
            requested
        }
    }

    async fn post(&self, body: Vec<u8>) -> Result<(u16, String), RouterError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        for attempt in 0..4u8 {
            let sent = self
                .http
                .post(&url)
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await;
            match sent {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    // Read Retry-After (429 TPM) before consuming the body.
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok());
                    let text = resp
                        .text()
                        .await
                        .map_err(|e| RouterError::Http(format!("reading body: {e}")))?;
                    if (status == 429 || status >= 500) && attempt < 3 {
                        let secs = retry_after.unwrap_or(1u64 << attempt); // 1,2,4s
                        tracing::warn!(status, secs, "openai backoff");
                        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
                        continue;
                    }
                    return Ok((status, text));
                }
                Err(e) if attempt < 3 => {
                    tracing::warn!(error = %e, "openai connect error, retrying");
                    tokio::time::sleep(std::time::Duration::from_secs(1u64 << attempt)).await;
                }
                Err(e) => return Err(RouterError::Http(format!("connect: {e}"))),
            }
        }
        unreachable!("loop returns on the final attempt")
    }

    /// Single text answer (router). Returns the neutral `MessagesResponse` plus
    /// the exact request-body bytes for the journal digest.
    pub async fn messages(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        _stop_sequences: &[&str], // gpt-5* rejects `stop`; ignored on purpose
    ) -> Result<(MessagesResponse, Vec<u8>), RouterError> {
        let mut req = serde_json::json!({
            "model": self.model,
            "max_completion_tokens": self.effective_max(max_tokens),
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        });
        if self.is_reasoning_model() {
            // Turn reasoning OFF for the routing call — a next-step id needs no
            // reasoning, and reasoning tokens bill at output rates. gpt-5.4-mini
            // rejects 'minimal'; its enum is none|low|medium|high|xhigh.
            req["reasoning_effort"] = serde_json::json!("none");
        }
        let body = serde_json::to_vec(&req).map_err(|e| RouterError::Http(e.to_string()))?;
        let (status, text) = self.post(body.clone()).await?;
        if !(200..300).contains(&status) {
            let head: String = text.chars().take(400).collect();
            return Err(RouterError::Http(format!("openai {status}: {head}")));
        }
        let parsed: ChatResponse =
            serde_json::from_str(&text).map_err(|e| RouterError::Http(format!("parse: {e}")))?;
        let choice = parsed.choices.into_iter().next();
        let (content, finish) = choice
            .map(|c| (c.message.content.unwrap_or_default(), c.finish_reason))
            .unwrap_or_default();
        Ok((
            MessagesResponse {
                content: vec![ContentBlock {
                    kind: "text".into(),
                    text: content,
                }],
                stop_reason: Some(finish),
                usage: parsed.usage.to_neutral(),
                model: self.model.clone(),
            },
            body,
        ))
    }

    /// Count tokens LOCALLY with tiktoken (o200k_base — the encoding gpt-4o and
    /// gpt-5 families use). OpenAI has no free count endpoint, and a minimal
    /// live call fails on reasoning models (reasoning tokens exhaust a tiny
    /// budget), so this is both cheaper and robust. It counts the raw
    /// system+user text; per-message chat overhead (~a few constant tokens) is
    /// omitted, which is irrelevant to the encoding experiment's relative
    /// comparison.
    pub async fn count_tokens(&self, system: &str, user: &str) -> Result<u64, RouterError> {
        static BPE: std::sync::OnceLock<tiktoken_rs::CoreBPE> = std::sync::OnceLock::new();
        let bpe = BPE
            .get_or_init(|| tiktoken_rs::o200k_base().expect("o200k_base tokenizer is embedded"));
        let text = if system.is_empty() {
            user.to_string()
        } else {
            format!("{system}\n{user}")
        };
        Ok(bpe.encode_with_special_tokens(&text).len() as u64)
    }

    /// One tool-loop turn from the neutral conversation.
    pub async fn tool_turn(
        &self,
        system: &str,
        history: &[ChatTurn],
        tools: &[ToolSchema],
        max_tokens: u32,
    ) -> Result<ToolTurn, RouterError> {
        let mut messages = vec![serde_json::json!({"role": "system", "content": system})];
        for turn in history {
            match turn {
                ChatTurn::User(text) => {
                    messages.push(serde_json::json!({"role": "user", "content": text}));
                }
                ChatTurn::Assistant { text, tool_calls } => {
                    let calls: Vec<serde_json::Value> = tool_calls
                        .iter()
                        .map(|tc| {
                            serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {"name": tc.name, "arguments": tc.input.to_string()},
                            })
                        })
                        .collect();
                    let mut m = serde_json::Map::new();
                    m.insert("role".into(), "assistant".into());
                    m.insert(
                        "content".into(),
                        if text.is_empty() {
                            serde_json::Value::Null
                        } else {
                            text.clone().into()
                        },
                    );
                    if !calls.is_empty() {
                        m.insert("tool_calls".into(), calls.into());
                    }
                    messages.push(serde_json::Value::Object(m));
                }
                ChatTurn::ToolResults(results) => {
                    for r in results {
                        messages.push(serde_json::json!({
                            "role": "tool",
                            "tool_call_id": r.id,
                            "content": r.output,
                        }));
                    }
                }
            }
        }
        let tool_defs: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();
        let mut req = serde_json::json!({
            "model": self.model,
            "max_completion_tokens": self.effective_max(max_tokens),
            "messages": messages,
            "tools": tool_defs,
        });
        if self.is_reasoning_model() {
            req["reasoning_effort"] = serde_json::json!("none");
        }
        let body = serde_json::to_vec(&req).map_err(|e| RouterError::Http(e.to_string()))?;
        let (status, text) = self.post(body).await?;
        if !(200..300).contains(&status) {
            let head: String = text.chars().take(400).collect();
            return Err(RouterError::Http(format!("openai tool {status}: {head}")));
        }
        let parsed: ChatResponse =
            serde_json::from_str(&text).map_err(|e| RouterError::Http(format!("parse: {e}")))?;
        let usage = parsed.usage.to_neutral();
        let Some(choice) = parsed.choices.into_iter().next() else {
            return Ok(ToolTurn {
                text: String::new(),
                tool_calls: vec![],
                stop_reason: "stop".into(),
                usage,
            });
        };
        let tool_calls = choice
            .message
            .tool_calls
            .into_iter()
            .map(|c| ToolCall {
                id: c.id,
                name: c.function.name,
                input: serde_json::from_str(&c.function.arguments).unwrap_or(serde_json::json!({})),
            })
            .collect();
        Ok(ToolTurn {
            text: choice.message.content.unwrap_or_default(),
            tool_calls,
            stop_reason: choice.finish_reason,
            usage,
        })
    }
}

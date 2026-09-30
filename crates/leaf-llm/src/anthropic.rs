//! Raw-HTTP Anthropic Messages client (Rust has no official SDK).
//!
//! POST /v1/messages and /v1/messages/count_tokens with `x-api-key` +
//! `anthropic-version: 2023-06-01`. No `thinking` field (omission = no
//! thinking on claude-sonnet-4-6 — right for a cheap router call), no
//! sampling params (responses are journaled, never trusted to reproduce).
//!
//! Retry policy: one retry after a fixed 2 s sleep on 429/5xx/connect
//! errors; retries happen BEFORE anything is journaled, so they are
//! replay-invisible. The API key is never journaled and never logged.

use leaf_core::RouterError;
use serde::{Deserialize, Serialize};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Return a clone of `messages` with a `cache_control: ephemeral` breakpoint on
/// the last content block of the last message (converting a string content into
/// a single text block so the marker has somewhere to attach).
fn cache_last_content_block(messages: &serde_json::Value) -> serde_json::Value {
    let mut messages = messages.clone();
    let Some(last) = messages.as_array_mut().and_then(|a| a.last_mut()) else {
        return messages;
    };
    let Some(obj) = last.as_object_mut() else {
        return messages;
    };
    let ephemeral = serde_json::json!({"type": "ephemeral"});
    match obj.get_mut("content") {
        Some(serde_json::Value::String(s)) => {
            let text = std::mem::take(s);
            obj.insert(
                "content".into(),
                serde_json::json!([{"type": "text", "text": text, "cache_control": ephemeral}]),
            );
        }
        Some(serde_json::Value::Array(blocks)) => {
            if let Some(block) = blocks.last_mut()
                && let Some(b) = block.as_object_mut()
            {
                b.insert("cache_control".into(), ephemeral);
            }
        }
        _ => {}
    }
    messages
}

pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    pub model: String,
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<&'a [&'a str]>,
    system: &'a str,
    messages: [Msg<'a>; 1],
}

#[derive(Serialize)]
struct Msg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize, Debug)]
pub struct MessagesResponse {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<String>,
    pub usage: Usage,
    pub model: String,
}

#[derive(Deserialize, Debug)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Deserialize, Debug, Default, Clone)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

impl MessagesResponse {
    /// Verbatim text of the first text content block ("" on refusals/empty).
    pub fn first_text(&self) -> &str {
        self.content
            .iter()
            .find(|c| c.kind == "text")
            .map(|c| c.text.as_str())
            .unwrap_or("")
    }
}

#[derive(Deserialize)]
struct CountTokensResponse {
    input_tokens: u64,
}

impl AnthropicClient {
    pub fn new(api_key: String, model: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key,
            model,
        }
    }

    /// Test seam (mock server) and the replay no-network poison pill.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    async fn post_with_retry(
        &self,
        path: &str,
        body: Vec<u8>,
    ) -> Result<(u16, String), RouterError> {
        let url = format!("{}{}", self.base_url, path);
        // OAuth access tokens (`sk-ant-oat…`, e.g. from `ant auth
        // print-credentials`) authenticate via `Authorization: Bearer` plus
        // the oauth beta header; standard API keys (`sk-ant-api…`) use
        // `x-api-key`. Sending an OAuth token on `x-api-key` returns 401.
        let is_oauth = self.api_key.starts_with("sk-ant-oat");
        for attempt in 0..2u8 {
            let mut builder = self
                .http
                .post(&url)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json");
            builder = if is_oauth {
                builder
                    .header("authorization", format!("Bearer {}", self.api_key))
                    .header("anthropic-beta", "oauth-2025-04-20")
            } else {
                builder.header("x-api-key", &self.api_key)
            };
            let sent = builder.body(body.clone()).send().await;
            match sent {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let text = resp
                        .text()
                        .await
                        .map_err(|e| RouterError::Http(format!("reading response body: {e}")))?;
                    let retryable = status == 429 || status >= 500;
                    if retryable && attempt == 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        continue;
                    }
                    return Ok((status, text));
                }
                Err(e) if attempt == 0 => {
                    tracing::warn!(error = %e, "anthropic connect error, retrying once");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(e) => return Err(RouterError::Http(format!("connect: {e}"))),
            }
        }
        unreachable!("loop returns on second attempt")
    }

    /// Returns (parsed response, exact request-body bytes sent) — the caller
    /// digests the bytes for the journal's request_digest.
    pub async fn messages(
        &self,
        system: &str,
        user: &str,
        max_tokens: u32,
        stop_sequences: &[&str],
    ) -> Result<(MessagesResponse, Vec<u8>), RouterError> {
        let req = MessagesRequest {
            model: &self.model,
            max_tokens,
            stop_sequences: if stop_sequences.is_empty() {
                None
            } else {
                Some(stop_sequences)
            },
            system,
            messages: [Msg {
                role: "user",
                content: user,
            }],
        };
        let body =
            serde_json::to_vec(&req).map_err(|e| RouterError::Http(format!("serialize: {e}")))?;
        let (status, text) = self.post_with_retry("/v1/messages", body.clone()).await?;
        if !(200..300).contains(&status) {
            let head: String = text.chars().take(400).collect();
            return Err(RouterError::Http(format!("/v1/messages {status}: {head}")));
        }
        let parsed: MessagesResponse = serde_json::from_str(&text)
            .map_err(|e| RouterError::Http(format!("parse response: {e}")))?;
        Ok((parsed, body))
    }

    /// One tool-loop turn from the neutral conversation (control loop).
    pub async fn tool_turn(
        &self,
        system: &str,
        history: &[crate::chat::ChatTurn],
        tools: &[crate::chat::ToolSchema],
        max_tokens: u32,
    ) -> Result<crate::chat::ToolTurn, RouterError> {
        use crate::chat::{ChatTurn, ToolCall, ToolTurn};

        // Translate neutral turns to Anthropic content blocks.
        let mut messages: Vec<serde_json::Value> = Vec::new();
        for turn in history {
            match turn {
                ChatTurn::User(text) => {
                    messages.push(serde_json::json!({"role": "user", "content": text}));
                }
                ChatTurn::Assistant { text, tool_calls } => {
                    let mut blocks: Vec<serde_json::Value> = Vec::new();
                    if !text.is_empty() {
                        blocks.push(serde_json::json!({"type": "text", "text": text}));
                    }
                    for tc in tool_calls {
                        blocks.push(serde_json::json!({
                            "type": "tool_use", "id": tc.id, "name": tc.name, "input": tc.input,
                        }));
                    }
                    messages.push(serde_json::json!({"role": "assistant", "content": blocks}));
                }
                ChatTurn::ToolResults(results) => {
                    let blocks: Vec<serde_json::Value> = results
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "type": "tool_result",
                                "tool_use_id": r.id,
                                "content": r.output,
                                "is_error": r.is_error,
                            })
                        })
                        .collect();
                    messages.push(serde_json::json!({"role": "user", "content": blocks}));
                }
            }
        }
        let mut tool_defs: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        // Prompt caching: the full transcript is resent each turn, so without a
        // cache breakpoint every turn re-bills the whole history at full input
        // price. Mark the last tool schema (caches tools+system) and the last
        // message's last block (caches the growing history prefix).
        if let Some(last) = tool_defs.last_mut()
            && let Some(o) = last.as_object_mut()
        {
            o.insert(
                "cache_control".into(),
                serde_json::json!({"type": "ephemeral"}),
            );
        }
        let messages = cache_last_content_block(&serde_json::Value::Array(messages));

        let req = serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": messages,
            "tools": tool_defs,
        });
        let body =
            serde_json::to_vec(&req).map_err(|e| RouterError::Http(format!("serialize: {e}")))?;
        let (status, text) = self.post_with_retry("/v1/messages", body).await?;
        if !(200..300).contains(&status) {
            let head: String = text.chars().take(400).collect();
            return Err(RouterError::Http(format!(
                "/v1/messages(tools) {status}: {head}"
            )));
        }
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            content: Vec<serde_json::Value>,
            stop_reason: Option<String>,
            usage: Usage,
        }
        let raw: Raw = serde_json::from_str(&text)
            .map_err(|e| RouterError::Http(format!("parse tool response: {e}")))?;
        let mut out_text = String::new();
        let mut tool_calls = Vec::new();
        for block in &raw.content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    out_text.push_str(block.get("text").and_then(|t| t.as_str()).unwrap_or(""))
                }
                Some("tool_use") => {
                    if let (Some(id), Some(name)) = (
                        block.get("id").and_then(|v| v.as_str()),
                        block.get("name").and_then(|v| v.as_str()),
                    ) {
                        tool_calls.push(ToolCall {
                            id: id.to_string(),
                            name: name.to_string(),
                            input: block.get("input").cloned().unwrap_or(serde_json::json!({})),
                        });
                    }
                }
                _ => {}
            }
        }
        Ok(ToolTurn {
            text: out_text,
            tool_calls,
            stop_reason: raw.stop_reason.unwrap_or_default(),
            usage: raw.usage,
        })
    }

    /// Free token counting (never tiktoken — model-specific counts).
    pub async fn count_tokens(&self, system: &str, user: &str) -> Result<u64, RouterError> {
        let req = serde_json::json!({
            "model": self.model,
            "system": system,
            "messages": [{"role": "user", "content": user}],
        });
        let body =
            serde_json::to_vec(&req).map_err(|e| RouterError::Http(format!("serialize: {e}")))?;
        let (status, text) = self
            .post_with_retry("/v1/messages/count_tokens", body)
            .await?;
        if !(200..300).contains(&status) {
            let head: String = text.chars().take(400).collect();
            return Err(RouterError::Http(format!(
                "/v1/messages/count_tokens {status}: {head}"
            )));
        }
        let parsed: CountTokensResponse = serde_json::from_str(&text)
            .map_err(|e| RouterError::Http(format!("parse response: {e}")))?;
        Ok(parsed.input_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serialization_is_byte_stable() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-6",
            max_tokens: 8,
            stop_sequences: Some(&["\n"]),
            system: "sys",
            messages: [Msg {
                role: "user",
                content: "hello",
            }],
        };
        let a = serde_json::to_vec(&req).unwrap();
        let b = serde_json::to_vec(&req).unwrap();
        assert_eq!(a, b);
        let golden = r#"{"model":"claude-sonnet-4-6","max_tokens":8,"stop_sequences":["\n"],"system":"sys","messages":[{"role":"user","content":"hello"}]}"#;
        assert_eq!(String::from_utf8(a).unwrap(), golden);
    }

    #[test]
    fn response_parses_with_missing_optional_usage_fields() {
        let json = r#"{
            "content": [{"type": "text", "text": "c1"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 3},
            "model": "claude-sonnet-4-6"
        }"#;
        let r: MessagesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(r.first_text(), "c1");
        assert_eq!(r.usage.cache_read_input_tokens, 0);
    }

    #[test]
    fn refusal_shaped_response_yields_empty_text() {
        let json = r#"{
            "content": [],
            "stop_reason": "refusal",
            "usage": {"input_tokens": 10, "output_tokens": 0},
            "model": "claude-sonnet-4-6"
        }"#;
        let r: MessagesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(r.first_text(), "");
        assert_eq!(r.stop_reason.as_deref(), Some("refusal"));
    }
}

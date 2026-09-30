//! `LlmRouter`: the single nondeterministic component, quarantined behind
//! `leaf_core::Router` and journaled verbatim.
//!
//! Flow per decision: seeded epsilon roll FIRST — an explored decision skips
//! the LLM entirely (no API call, no LlmCall entry). Otherwise one Messages
//! call; on an unparseable answer, one re-ask with a terser reminder; on a
//! second failure, a DETERMINISTIC fallback to candidate 0 journaled with
//! `parse_fallback: true` (never a random pick — fallback must be
//! replay-stable and visible to the bench).

use leaf_core::sha256_hex;
use leaf_core::{RouteDecision, RouteRequest, Router, RouterError};

use crate::client::LlmClient;
use crate::explore::EpsilonGreedy;
use crate::pricing::{Prices, cost_microusd};
use crate::prompt;

pub struct LlmRouter {
    pub client: LlmClient,
    pub explore: EpsilonGreedy,
    pub prices: Prices,
}

impl LlmRouter {
    pub fn new(client: LlmClient, explore: EpsilonGreedy, prices: Prices) -> Self {
        Self {
            client,
            explore,
            prices,
        }
    }

    /// Token count of the routing prompt for `req` — the bench's
    /// decomposition-ceiling experiment surface (count_tokens is free).
    pub async fn count_route_tokens(&self, req: &RouteRequest<'_>) -> Result<u64, RouterError> {
        let user = prompt::build_user(req.goal, req.rendered_context, req.rendered_menu);
        self.client.count_tokens(prompt::PROMPT_V1, &user).await
    }
}

impl Router for LlmRouter {
    async fn route(&mut self, req: RouteRequest<'_>) -> Result<RouteDecision, RouterError> {
        let n = req.candidates.len();
        if n == 0 {
            return Err(RouterError::NoCandidates);
        }

        let exploration = self.explore.decide();
        if exploration.explored {
            let chosen = (exploration.pick.expect("explored carries a pick") % n as u64) as usize;
            return Ok(RouteDecision {
                chosen,
                exploration: Some(exploration),
                llm: None,
            });
        }

        let user = prompt::build_user(req.goal, req.rendered_context, req.rendered_menu);

        // Attempt 1 + at most one re-ask; usage accumulates into ONE record
        // so cost accounting sees every billed token.
        let mut total = leaf_core::LlmCallRecord {
            model: self.client.model().to_string(),
            request_digest: String::new(),
            response_text: String::new(),
            stop_reason: String::new(),
            input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 0,
            cost_microusd: 0,
            api_calls: 0,
            parse_fallback: false,
        };
        let mut chosen: Option<usize> = None;
        for attempt in 0..2u8 {
            let user_msg = if attempt == 0 {
                user.clone()
            } else {
                format!("{user}{}", prompt::REASK_SUFFIX)
            };
            let (resp, body) = match self
                .client
                .messages(
                    prompt::PROMPT_V1,
                    &user_msg,
                    prompt::MAX_ANSWER_TOKENS,
                    prompt::STOP_SEQUENCES,
                )
                .await
            {
                Ok(v) => v,
                // Attempt 0 failing bills nothing → propagate. A re-ask (attempt
                // 1) failing must NOT drop attempt-0's already-billed usage:
                // break to the deterministic fallback so the spend is journaled.
                Err(e) if attempt == 0 => return Err(e),
                Err(e) => {
                    tracing::warn!(error = %e, "router re-ask failed; falling back with billed usage");
                    break;
                }
            };
            total.api_calls = attempt as u32 + 1;
            total.request_digest = sha256_hex(&body);
            total.response_text = resp.first_text().to_string();
            total.stop_reason = resp.stop_reason.clone().unwrap_or_default();
            total.input_tokens += resp.usage.input_tokens;
            total.cache_creation_input_tokens += resp.usage.cache_creation_input_tokens;
            total.cache_read_input_tokens += resp.usage.cache_read_input_tokens;
            total.output_tokens += resp.usage.output_tokens;

            if let Some(idx) = prompt::parse_answer(resp.first_text(), n) {
                chosen = Some(idx);
                break;
            }
            tracing::warn!(
                attempt,
                answer = %resp.first_text(),
                "router answer unparseable"
            );
        }
        total.cost_microusd = cost_microusd(
            &self.prices,
            total.input_tokens,
            total.cache_creation_input_tokens,
            total.cache_read_input_tokens,
            total.output_tokens,
        );

        let chosen = match chosen {
            Some(idx) => idx,
            None => {
                total.parse_fallback = true;
                0
            }
        };
        Ok(RouteDecision {
            chosen,
            exploration: Some(exploration),
            llm: Some(total),
        })
    }
}

//! The Router trait boundary: leaf-llm plugs in here, so leaf-core never
//! depends on reqwest or any LLM vendor code.

use std::collections::VecDeque;

use leaf_traits::CandidateInfo;
use serde::{Deserialize, Serialize};

/// Everything a router sees for one branch decision.
pub struct RouteRequest<'a> {
    pub goal: &'a str,
    pub candidates: &'a [CandidateInfo],
    /// Propagation::render output — the executor computes it.
    pub rendered_context: &'a str,
    /// Codec::render_candidates output — same bytes the prompt_digest covers.
    pub rendered_menu: &'a str,
    /// sha256 over goal/context/menu, computed by the executor. Routers that
    /// replay a journal assert against it; live routers may ignore it.
    pub prompt_digest: &'a str,
    /// 0-based count of routing decisions this run.
    pub decision_index: u32,
}

/// Everything the executor needs to journal the decision.
pub struct RouteDecision {
    /// Index into `candidates`.
    pub chosen: usize,
    /// None for routers that don't explore (scripted, replay).
    pub exploration: Option<ExplorationRecord>,
    /// None when exploration skipped the LLM (or no LLM router).
    pub llm: Option<LlmCallRecord>,
}

/// Seeded epsilon-greedy bookkeeping. Integer-only (ppm) — floats are banned
/// from journal types.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplorationRecord {
    /// Epsilon in parts-per-million (e.g. 100_000 = 10%).
    pub epsilon_ppm: u32,
    /// `rng.next_u64() % 1_000_000`.
    pub roll_ppm: u32,
    /// `roll_ppm < epsilon_ppm`.
    pub explored: bool,
    /// Raw `next_u64` used for the uniform pick, iff explored.
    pub pick: Option<u64>,
}

/// Verbatim record of one LLM call — the only nondeterministic component,
/// quarantined here so replay can serve it from the journal.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmCallRecord {
    pub model: String,
    /// sha256_hex of the exact request-body bytes sent.
    pub request_digest: String,
    /// Verbatim text of the response's first content block.
    pub response_text: String,
    pub stop_reason: String,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    /// Integer micro-USD at the pinned price table.
    pub cost_microusd: u64,
    /// Number of billed `/v1/messages` calls folded into this record (a
    /// router re-ask makes 2). The cap and call-count must count API calls,
    /// not decisions. Defaults to 1 for journals written before this field.
    #[serde(default = "one")]
    pub api_calls: u32,
    /// True if the answer was unparseable and the deterministic fallback fired.
    pub parse_fallback: bool,
}

fn one() -> u32 {
    1
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("llm http error: {0}")]
    Http(String),
    #[error("no candidates offered")]
    NoCandidates,
    #[error("replay journal exhausted at decision {0}")]
    ReplayExhausted(u32),
}

pub trait Router: Send {
    fn route(
        &mut self,
        req: RouteRequest<'_>,
    ) -> impl Future<Output = Result<RouteDecision, RouterError>> + Send;
}

/// Test/bench utility: a router with pre-baked picks and no LLM. Not a
/// production impl — it exists so the executor and the MCP layer can be
/// tested with zero LLM.
pub struct ScriptedRouter {
    picks: VecDeque<usize>,
}

impl ScriptedRouter {
    pub fn new(picks: impl IntoIterator<Item = usize>) -> Self {
        Self {
            picks: picks.into_iter().collect(),
        }
    }
}

impl Router for ScriptedRouter {
    async fn route(&mut self, req: RouteRequest<'_>) -> Result<RouteDecision, RouterError> {
        if req.candidates.is_empty() {
            return Err(RouterError::NoCandidates);
        }
        let chosen = self
            .picks
            .pop_front()
            .ok_or(RouterError::ReplayExhausted(req.decision_index))?;
        Ok(RouteDecision {
            chosen,
            exploration: None,
            llm: None,
        })
    }
}

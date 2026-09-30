//! Pinned price table (2026-07), integer micro-USD end to end.
//!
//! Prices are stored as micro-USD per MILLION tokens (i.e. dollars × 1e6),
//! so fractional per-token rates (cache reads at $0.30/MTok) stay integral.
//! Costs floor at the micro-USD — deterministic, and the error is < 1e-6 USD
//! per usage field.
//!
//! The formula matches AXI's bench-github accounting exactly:
//! `input×in + cache_write×(in×1.25) + cache_read×cr + output×out`.

/// Micro-USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prices {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
}

pub const SONNET_4_6: Prices = Prices {
    input: 3_000_000,       // $3.00 / MTok
    cache_read: 300_000,    // $0.30 / MTok
    cache_write: 3_750_000, // $3.75 / MTok (1.25 × input)
    output: 15_000_000,     // $15.00 / MTok
};

pub const HAIKU_4_5: Prices = Prices {
    input: 1_000_000,       // $1.00 / MTok
    cache_read: 100_000,    // $0.10 / MTok
    cache_write: 1_250_000, // $1.25 / MTok
    output: 5_000_000,      // $5.00 / MTok
};

/// OpenAI gpt-4o-mini — prices KNOWN and pinned exactly, so the budget cap is
/// accurate. OpenAI has no cache-write premium; cached input is billed at the
/// cache_read rate and cache_creation is always 0.
pub const GPT_4O_MINI: Prices = Prices {
    input: 150_000,       // $0.15 / MTok
    cache_read: 75_000,   // $0.075 / MTok
    cache_write: 150_000, // unused (no write premium)
    output: 600_000,      // $0.60 / MTok
};

/// OpenAI gpt-5.4-mini — AXI's judge model, more comparable, but its exact
/// price is not published to us, so this is a deliberately CONSERVATIVE
/// (over-)estimate: the budget cap then triggers early rather than
/// overspending real money. Token counts are exact regardless; re-price via
/// the `[prices]` table in bench.toml once the real numbers are known.
pub const GPT_5_4_MINI_ESTIMATE: Prices = Prices {
    input: 300_000,       // ~$0.30 / MTok (est.)
    cache_read: 30_000,   // ~$0.03 / MTok (est.)
    cache_write: 300_000, // unused
    output: 2_500_000,    // ~$2.50 / MTok (est.) — reasoning tokens bill as output
};

pub const PRICE_TABLE: &[(&str, Prices)] = &[
    ("claude-sonnet-4-6", SONNET_4_6),
    ("claude-haiku-4-5", HAIKU_4_5),
    ("gpt-4o-mini", GPT_4O_MINI),
    ("gpt-5.4-mini", GPT_5_4_MINI_ESTIMATE),
];

pub fn lookup(model: &str) -> Option<Prices> {
    PRICE_TABLE
        .iter()
        .find(|(m, _)| *m == model)
        .map(|(_, p)| *p)
}

/// Integer cost in micro-USD (floored per component).
pub fn cost_microusd(
    p: &Prices,
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
) -> u64 {
    let per =
        |tokens: u64, price: u64| -> u64 { ((tokens as u128 * price as u128) / 1_000_000) as u64 };
    per(input_tokens, p.input)
        + per(cache_creation_input_tokens, p.cache_write)
        + per(cache_read_input_tokens, p.cache_read)
        + per(output_tokens, p.output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_mtok_of_input_costs_three_dollars() {
        assert_eq!(cost_microusd(&SONNET_4_6, 1_000_000, 0, 0, 0), 3_000_000);
    }

    #[test]
    fn axi_formula_components() {
        // 100k in + 50k cache-write + 200k cache-read + 10k out on sonnet:
        // 0.30 + 0.1875 + 0.06 + 0.15 = $0.6975 = 697_500 µUSD
        assert_eq!(
            cost_microusd(&SONNET_4_6, 100_000, 50_000, 200_000, 10_000),
            300_000 + 187_500 + 60_000 + 150_000
        );
    }

    #[test]
    fn small_counts_floor_deterministically() {
        // 1 token of cache-read on sonnet = 0.3 µUSD → floors to 0.
        assert_eq!(cost_microusd(&SONNET_4_6, 0, 0, 1, 0), 0);
        // 4 tokens = 1.2 µUSD → floors to 1.
        assert_eq!(cost_microusd(&SONNET_4_6, 0, 0, 4, 0), 1);
    }

    #[test]
    fn lookup_finds_pinned_models() {
        assert_eq!(lookup("claude-sonnet-4-6"), Some(SONNET_4_6));
        assert_eq!(lookup("claude-haiku-4-5"), Some(HAIKU_4_5));
        assert_eq!(lookup("gpt-x"), None);
    }
}

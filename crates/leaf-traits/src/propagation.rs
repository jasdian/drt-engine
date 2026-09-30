//! Context-propagation policies: how run history flows into the next router
//! prompt. This is the surviving form of the "Markov" idea — three policies
//! behind a trait, compared as an A/B/C experiment, not a belief.
//!
//! Every policy is deterministic: same items + same codec => byte-identical
//! output. `DecayedSummary` is deterministic *truncation*, not LLM
//! summarization — LLM summarization would put nondeterminism below the
//! router, violating the core contract.

use crate::codec::Codec;
use crate::types::HistoryItem;

pub trait Propagation: Send + Sync {
    /// Stable identifier recorded in the journal ("full", "last_k", "decayed").
    fn name(&self) -> &'static str;
    /// Deterministic: same items + same codec => byte-identical string.
    fn render(&self, items: &[HistoryItem], codec: &dyn Codec) -> String;
}

/// The whole history, verbatim.
pub struct FullLog;

impl Propagation for FullLog {
    fn name(&self) -> &'static str {
        "full"
    }
    fn render(&self, items: &[HistoryItem], codec: &dyn Codec) -> String {
        codec.render_history(items)
    }
}

/// The run input (always kept — the router must never lose the task) plus the
/// last `k` other items.
pub struct LastK {
    pub k: usize,
}

impl Propagation for LastK {
    fn name(&self) -> &'static str {
        "last_k"
    }
    fn render(&self, items: &[HistoryItem], codec: &dyn Codec) -> String {
        let (head, rest): (Vec<&HistoryItem>, Vec<&HistoryItem>) = items
            .iter()
            .partition(|i| matches!(i, HistoryItem::RunInput { .. }));
        let start = rest.len().saturating_sub(self.k);
        let mut kept: Vec<HistoryItem> = head.into_iter().cloned().collect();
        let dropped = start;
        if dropped > 0 {
            // A fixed marker so the model knows context was elided; contains
            // only a count derived from history length — deterministic.
            kept.push(HistoryItem::Decision {
                branch: "(context)".into(),
                chosen: format!("[{dropped} earlier items elided]"),
                explored: false,
            });
        }
        kept.extend(rest[start..].iter().map(|i| (*i).clone()));
        codec.render_history(&kept)
    }
}

/// Exponentially decaying per-item character budget: the newest item gets
/// `base_budget_chars`, each step older multiplies by `decay_num/decay_den`,
/// floored at `min_chars`. Integer ratios only — floats are banned from
/// anything replay-sensitive.
pub struct DecayedSummary {
    pub base_budget_chars: usize,
    pub decay_num: u32,
    pub decay_den: u32,
    pub min_chars: usize,
}

impl DecayedSummary {
    /// Budget for an item `age` steps older than the newest.
    fn budget(&self, age: usize) -> usize {
        let mut b = self.base_budget_chars as u128;
        for _ in 0..age {
            b = b * self.decay_num as u128 / self.decay_den as u128;
        }
        (b as usize).max(self.min_chars)
    }
}

/// Truncate to at most `budget` chars on a char boundary, appending a fixed
/// marker when anything was cut.
fn truncate_chars(s: &str, budget: usize) -> String {
    match s.char_indices().nth(budget) {
        None => s.to_string(),
        Some((byte_idx, _)) => {
            let mut t = s[..byte_idx].to_string();
            t.push_str("…[truncated]");
            t
        }
    }
}

impl Propagation for DecayedSummary {
    fn name(&self) -> &'static str {
        "decayed"
    }
    fn render(&self, items: &[HistoryItem], codec: &dyn Codec) -> String {
        let newest = items.len().saturating_sub(1);
        let mut out = String::new();
        for (idx, item) in items.iter().enumerate() {
            let rendered = codec.render_history(std::slice::from_ref(item));
            // The run input is the task — never decayed.
            let piece = if matches!(item, HistoryItem::RunInput { .. }) {
                rendered
            } else {
                truncate_chars(&rendered, self.budget(newest - idx))
            };
            out.push_str(&piece);
            if !piece.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::ToonCodec;

    fn history(n: usize) -> Vec<HistoryItem> {
        let mut items = vec![HistoryItem::RunInput {
            text: "task".into(),
        }];
        for i in 0..n {
            items.push(HistoryItem::LeafOutput {
                leaf: "jq".into(),
                tool: "jq".into(),
                output: format!("output number {i} {}", "x".repeat(100)),
                is_error: false,
            });
        }
        items
    }

    #[test]
    fn full_log_is_codec_verbatim() {
        let items = history(3);
        assert_eq!(
            FullLog.render(&items, &ToonCodec),
            ToonCodec.render_history(&items)
        );
    }

    #[test]
    fn last_k_keeps_run_input_and_tail() {
        let items = history(5);
        let out = LastK { k: 2 }.render(&items, &ToonCodec);
        assert!(out.contains("input: task"));
        assert!(out.contains("output number 4"));
        assert!(out.contains("output number 3"));
        assert!(!out.contains("output number 2"));
        assert!(out.contains("[3 earlier items elided]"));
    }

    #[test]
    fn last_k_window_edges() {
        let items = history(2);
        // k larger than history: everything kept, no elision marker
        let out = LastK { k: 10 }.render(&items, &ToonCodec);
        assert!(!out.contains("elided"));
        assert!(out.contains("output number 0"));
        // k = 0: only run input + marker
        let out = LastK { k: 0 }.render(&items, &ToonCodec);
        assert!(out.contains("input: task"));
        assert!(!out.contains("output number"));
        assert!(out.contains("[2 earlier items elided]"));
    }

    #[test]
    fn decayed_budgets_shrink_with_age() {
        let d = DecayedSummary {
            base_budget_chars: 400,
            decay_num: 1,
            decay_den: 2,
            min_chars: 20,
        };
        assert_eq!(d.budget(0), 400);
        assert_eq!(d.budget(1), 200);
        assert_eq!(d.budget(2), 100);
        assert_eq!(d.budget(10), 20); // floored
    }

    #[test]
    fn decayed_truncates_old_keeps_new_and_run_input() {
        let items = history(4);
        let d = DecayedSummary {
            base_budget_chars: 500,
            decay_num: 1,
            decay_den: 4,
            min_chars: 10,
        };
        let out = d.render(&items, &ToonCodec);
        assert!(out.contains("input: task")); // run input never decayed
        assert!(out.contains("output number 3")); // newest intact
        assert!(out.contains("…[truncated]")); // oldest cut
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "żółć-żółć-żółć";
        let t = truncate_chars(s, 5);
        assert!(t.starts_with("żółć-"));
        assert!(t.ends_with("…[truncated]"));
    }

    #[test]
    fn policies_are_deterministic() {
        let items = history(6);
        let policies: Vec<Box<dyn Propagation>> = vec![
            Box::new(FullLog),
            Box::new(LastK { k: 3 }),
            Box::new(DecayedSummary {
                base_budget_chars: 100,
                decay_num: 2,
                decay_den: 3,
                min_chars: 8,
            }),
        ];
        for p in &policies {
            assert_eq!(p.render(&items, &ToonCodec), p.render(&items, &ToonCodec));
        }
    }
}

//! Seeded epsilon-greedy exploration.
//!
//! RNG choice (deliberate, see docs/design.md): `rand_chacha::ChaCha8Rng`
//! via `rand_core` only — no `rand` facade. The ChaCha stream is defined by
//! the ChaCha specification, so the same seed produces an identical byte
//! stream across platforms and releases (unlike `StdRng`, which disclaims
//! cross-version stability), and with no facade there is no `thread_rng` in
//! the dependency graph — ambient entropy is structurally impossible.
//!
//! ## Draw discipline (FROZEN — golden-tested; changing it breaks replay)
//! - draw #1: `roll_ppm = next_u64() % 1_000_000`; `explored = roll_ppm < epsilon_ppm`
//! - draw #2 (only if explored): `pick = next_u64()`; chosen index = `pick % n`
//!
//! The roll happens BEFORE the LLM call; an explored decision skips the LLM
//! entirely (no API call, no LlmCall journal entry).

use leaf_core::{ExplorationRecord, JournalEntry};
use rand_chacha::ChaCha8Rng;
use rand_core::{RngCore, SeedableRng};

pub struct EpsilonGreedy {
    rng: ChaCha8Rng,
    pub epsilon_ppm: u32,
}

impl EpsilonGreedy {
    pub fn new(seed: u64, epsilon_ppm: u32) -> Self {
        Self {
            rng: ChaCha8Rng::seed_from_u64(seed),
            epsilon_ppm,
        }
    }

    pub fn decide(&mut self) -> ExplorationRecord {
        let roll_ppm = (self.rng.next_u64() % 1_000_000) as u32;
        let explored = roll_ppm < self.epsilon_ppm;
        let pick = if explored {
            Some(self.rng.next_u64())
        } else {
            None
        };
        ExplorationRecord {
            epsilon_ppm: self.epsilon_ppm,
            roll_ppm,
            explored,
            pick,
        }
    }
}

/// Independently re-derive the exploration stream from `seed` and assert
/// every journaled `ExplorationRecord` matches (acceptance criterion 3).
/// Returns the number of records verified.
pub fn verify_exploration_stream(seed: u64, entries: &[JournalEntry]) -> Result<u32, String> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let mut verified = 0u32;
    for entry in entries {
        let JournalEntry::BranchDecision {
            step,
            candidates,
            chosen,
            exploration,
            ..
        } = entry
        else {
            continue;
        };
        let Some(rec) = exploration else {
            return Err(format!("decision at step {step} has no exploration record"));
        };
        let roll_ppm = (rng.next_u64() % 1_000_000) as u32;
        if roll_ppm != rec.roll_ppm {
            return Err(format!(
                "step {step}: roll_ppm {} recorded, {roll_ppm} derived from seed {seed}",
                rec.roll_ppm
            ));
        }
        let explored = roll_ppm < rec.epsilon_ppm;
        if explored != rec.explored {
            return Err(format!(
                "step {step}: explored flag {} inconsistent with roll {roll_ppm} < epsilon {}",
                rec.explored, rec.epsilon_ppm
            ));
        }
        if explored {
            let pick = rng.next_u64();
            if Some(pick) != rec.pick {
                return Err(format!(
                    "step {step}: pick {:?} recorded, {pick} derived",
                    rec.pick
                ));
            }
            // The frozen explore mapping is chosen = pick % n; a tampered
            // `chosen` on an explored decision diverges here.
            let n = candidates.len();
            if n == 0 {
                return Err(format!(
                    "step {step}: explored decision with empty candidates"
                ));
            }
            let derived = (pick % n as u64) as usize;
            if *chosen != derived {
                return Err(format!(
                    "step {step}: chosen {chosen} != pick % {n} = {derived} (frozen explore mapping violated)"
                ));
            }
        } else if rec.pick.is_some() {
            return Err(format!("step {step}: unexplored decision carries a pick"));
        }
        verified += 1;
    }
    Ok(verified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = EpsilonGreedy::new(42, 100_000);
        let mut b = EpsilonGreedy::new(42, 100_000);
        for _ in 0..50 {
            assert_eq!(a.decide(), b.decide());
        }
    }

    #[test]
    fn golden_stream_seed_42() {
        // FROZEN: pins ChaCha8Rng::seed_from_u64 + the draw discipline.
        // If this test ever fails after a dependency bump, old journals can
        // no longer be verified — that is a breaking change, not a test fix.
        let mut e = EpsilonGreedy::new(42, 100_000);
        let first: Vec<u32> = (0..6).map(|_| e.decide().roll_ppm).collect();
        assert_eq!(first, GOLDEN_ROLLS_SEED_42_EPS_10PCT);
    }

    // Bootstrap-pinned (rand_chacha 0.9, 2026-07-10), then frozen.
    const GOLDEN_ROLLS_SEED_42_EPS_10PCT: [u32; 6] =
        [200_737, 322_312, 131_020, 374_866, 461_744, 231_838];

    #[test]
    fn epsilon_zero_never_explores_epsilon_million_always_explores() {
        let mut never = EpsilonGreedy::new(7, 0);
        let mut always = EpsilonGreedy::new(7, 1_000_000);
        for _ in 0..20 {
            let n = never.decide();
            assert!(!n.explored);
            assert!(n.pick.is_none());
            let a = always.decide();
            assert!(a.explored);
            assert!(a.pick.is_some());
        }
    }

    fn decision(step: u32, rec: ExplorationRecord) -> JournalEntry {
        // chosen follows the frozen explore mapping for explored decisions.
        let chosen = match rec.pick {
            Some(p) if rec.explored => (p % 2) as usize,
            _ => 0,
        };
        JournalEntry::BranchDecision {
            step,
            branch: "root".into(),
            prompt_digest: "d".repeat(64),
            candidates: vec!["a".into(), "b".into()],
            chosen,
            exploration: Some(rec),
        }
    }

    #[test]
    fn verify_stream_green_and_red() {
        let mut e = EpsilonGreedy::new(1234, 500_000);
        let entries: Vec<JournalEntry> = (0..10).map(|i| decision(i, e.decide())).collect();
        assert_eq!(verify_exploration_stream(1234, &entries), Ok(10));

        // Wrong seed must be detected.
        assert!(verify_exploration_stream(1235, &entries).is_err());

        // A tampered roll must be detected.
        let mut tampered = entries.clone();
        if let JournalEntry::BranchDecision {
            exploration: Some(rec),
            ..
        } = &mut tampered[3]
        {
            rec.roll_ppm ^= 1;
        }
        let err = verify_exploration_stream(1234, &tampered).unwrap_err();
        assert!(err.contains("step 3"), "got: {err}");
    }
}

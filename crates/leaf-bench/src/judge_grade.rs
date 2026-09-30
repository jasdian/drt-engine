//! AXI-FAITHFUL grading (`leaf-bench judge-grade`).
//!
//! The primary benchmark grades with exact-substring `contains_values` against
//! the tree's own jq output — far stricter than AXI, which grades with a binary
//! LLM judge fed a per-task `grading_hint` (a RANGE/lower-bound, e.g. "most
//! checks passed, a few failures", "at least N≥100"). Under AXI's own grading a
//! correct-but-differently-counted control answer (31 vs 32 checks, 205 vs 1650
//! "in the hundreds") PASSES. This command re-grades the recorded answers the
//! AXI way, to show how much of the tree↔control success gap is our strict
//! grader vs a genuine capability gap. It is a DIAGNOSTIC — it never overrides
//! the primary deterministic grade. Prompt shape mirrors AXI's grader.ts.
//! Each cell is judged by MAJORITY-OF-3 independent votes (see JUDGE_VOTES).

use std::collections::BTreeMap;

use anyhow::Result;
use leaf_llm::{LlmClient, Provider, cost_microusd};

use crate::artifacts::RunPaths;
use crate::budget::Budget;
use crate::config::{BenchConfig, BenchPaths};
use crate::tasks::TaskManifest;

const BUCKET: &str = "judge";
/// Votes per graded cell: each answer is judged independently this many times
/// and the majority verdict stands. A single-vote judge inherited the vendor's
/// sampling jitter (±2 grades observed between identical re-runs); majority
/// voting damps it. Split votes are counted and reported as the noise metric.
const JUDGE_VOTES: u32 = 3;
/// (artifacts subdir, condition) pairs to re-grade AXI-style. The fair
/// comparison is LIVE: the tree (`runs-live/full`) and the fused free-driving
/// control (`runs-live/control`), both most affected by strict exact-substring
/// grading against a drifting live world; plus the Claude Code control
/// (`runs/control_cc`). Output is keyed by condition name.
const JUDGE_TARGETS: &[(&str, &str)] = &[
    ("runs-live", "full"),
    ("runs-live", "control"),
    ("runs", "control_cc"),
];

const JUDGE_SYSTEM: &str = "You are a benchmark grader evaluating whether an AI agent completed a \
task about a GitHub repository. You are given the task, the KNOWN FACTS (an acceptable band, not a \
single pinned value), and the agent's answer. Reply with exactly one word: PASS or FAIL.";

fn cap(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        s.chars().take(n).collect::<String>() + "…[truncated]"
    } else {
        s.to_string()
    }
}

pub async fn run(
    paths: &BenchPaths,
    cfg: &BenchConfig,
    provider: Provider,
    api_key: &str,
) -> Result<()> {
    let tasks = TaskManifest::load_all(&paths.tasks)?;
    let client = LlmClient::new(provider, api_key.to_string(), cfg.router_model.clone());
    let prices = client.prices();
    let mut budget = Budget::load(
        paths.budget_file.clone(),
        cfg.budget_cap_microusd,
        cfg.buckets.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    )?;

    // condition -> task -> (pass, n)
    let mut grid: BTreeMap<String, BTreeMap<String, (u32, u32)>> = BTreeMap::new();
    let mut judged_cells = 0u32;
    let mut split_cells = 0u32;

    'outer: for (source, condition) in JUDGE_TARGETS {
        let runs_root = paths.artifacts.join(source);
        for task in &tasks {
            let hint = task.grading_hint.clone().unwrap_or_default();
            for rep in 0..cfg.run.repeats {
                let rp = RunPaths::at(&runs_root, condition, &task.id, rep);
                let Some(answer) = rp.load_answer() else {
                    continue;
                };
                let user = format!(
                    "TASK: {}\n\nKNOWN FACTS (acceptable band): {}\n\nAGENT ANSWER:\n{}\n\nRules: PASS if the answer is correct and complete WITHIN the KNOWN FACTS band (a count anywhere in the stated range is correct; do not require an exact pinned number). FAIL if the agent hallucinated, misinterpreted, or gave a partial/empty answer. For error-recovery tasks, PASS if it correctly identified and reported the error.\n\nVerdict (PASS or FAIL):",
                    task.prompt,
                    hint,
                    cap(&answer, 3000),
                );
                // Majority-of-3: identical prompt each vote — the averaging is
                // over vendor sampling nondeterminism, nothing else. A budget
                // stop mid-cell discards the partial cell (never a 1-vote grade).
                let mut pass_votes = 0u32;
                for _vote in 0..JUDGE_VOTES {
                    let projected = budget.projection(BUCKET);
                    let id = match budget.reserve(BUCKET, projected) {
                        Ok(id) => id,
                        Err(e) => {
                            println!("judge-grade: budget stop — {e}");
                            break 'outer;
                        }
                    };
                    let (resp, _body) = client.messages(JUDGE_SYSTEM, &user, 4, &["\n"]).await?;
                    let cost = cost_microusd(
                        &prices,
                        resp.usage.input_tokens,
                        resp.usage.cache_creation_input_tokens,
                        resp.usage.cache_read_input_tokens,
                        resp.usage.output_tokens,
                    );
                    budget.settle(id, BUCKET, cost)?;
                    if resp.first_text().trim().to_uppercase().starts_with("PASS") {
                        pass_votes += 1;
                    }
                }
                let pass = pass_votes * 2 > JUDGE_VOTES;
                if pass_votes != 0 && pass_votes != JUDGE_VOTES {
                    split_cells += 1;
                }
                judged_cells += 1;
                let e = grid
                    .entry(condition.to_string())
                    .or_default()
                    .entry(task.id.clone())
                    .or_insert((0, 0));
                e.1 += 1;
                if pass {
                    e.0 += 1;
                }
            }
        }
    }

    // Persist for the report (bench/axi-grade.json) and print a compact grid.
    let json = serde_json::to_string_pretty(&grid)?;
    std::fs::write(paths.bench.join("axi-grade.json"), json)?;
    println!("\n== AXI-faithful grading (LLM judge + range hints) ==");
    for (_source, condition) in JUDGE_TARGETS {
        let Some(per) = grid.get(*condition) else {
            continue;
        };
        let (mut p, mut n) = (0u32, 0u32);
        for (tp, tn) in per.values() {
            p += tp;
            n += tn;
        }
        let pct = if n > 0 {
            100.0 * p as f64 / n as f64
        } else {
            0.0
        };
        println!("  {condition:14} {p}/{n} ({pct:.0}%)");
    }
    println!(
        "wrote bench/axi-grade.json (majority-of-{JUDGE_VOTES}: {split_cells}/{judged_cells} cells had split votes; judge spend ${:.4})",
        budget.spent_bucket(BUCKET) as f64 / 1e6
    );
    Ok(())
}

//! LLM-judge AUDIT (never the grader): re-scores every deterministic FAIL and
//! a deterministic sample of PASSes with a binary judge, and reports the
//! checker-vs-judge agreement rate. The judge never overrides the checker in
//! headline numbers — this only surfaces where the deterministic checker and
//! an LLM disagree, so a reader can trust the grade.

use anyhow::Result;
use leaf_core::sha256_hex;
use leaf_llm::{LlmClient, Provider, cost_microusd};

use crate::artifacts::RunPaths;
use crate::budget::Budget;
use crate::config::{BenchConfig, BenchPaths};
use crate::runner::CONDITIONS;
use crate::tasks::TaskManifest;

const BUCKET: &str = "judge";
/// Audit ~10% of passes deterministically (hash of the run path).
const PASS_SAMPLE_PPM: u64 = 100_000;

const JUDGE_SYSTEM: &str = "You are a strict grader. Given a task, the KNOWN correct facts, and an \
agent's answer, decide if the answer is correct and complete. Reply with exactly one word: PASS or \
FAIL.";

pub async fn run(
    paths: &BenchPaths,
    cfg: &BenchConfig,
    provider: Provider,
    api_key: &str,
) -> Result<()> {
    let tasks = TaskManifest::load_all(&paths.tasks)?;
    let client = LlmClient::new(provider, api_key.to_string(), cfg.router_model.clone());
    let prices = client.prices();
    let runs_root = paths.artifacts.join("runs");
    let mut budget = Budget::load(
        paths.budget_file.clone(),
        cfg.budget_cap_microusd,
        cfg.buckets.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    )?;

    let mut agree = 0u32;
    let mut disagree = 0u32;
    let mut audited = 0u32;
    let mut disagreements = Vec::new();

    for condition in CONDITIONS {
        for task in &tasks {
            let gt = std::fs::read_to_string(paths.ground_truth.join(format!("{}.json", task.id)))
                .unwrap_or_default();
            for rep in 0..cfg.run.repeats {
                let rp = RunPaths::at(&runs_root, condition, &task.id, rep);
                let (Some(grade), Some(answer)) = (rp.load_grade(), rp.load_answer()) else {
                    continue;
                };
                // Audit every FAIL; sample 10% of PASSes deterministically.
                let sample = {
                    let h = sha256_hex(rp.dir.to_string_lossy().as_bytes());
                    let n = u64::from_str_radix(&h[..8], 16).unwrap_or(0);
                    (n % 1_000_000) < PASS_SAMPLE_PPM
                };
                if grade.pass && !sample {
                    continue;
                }

                let projected = budget.projection(BUCKET);
                let id = match budget.reserve(BUCKET, projected) {
                    Ok(id) => id,
                    Err(e) => {
                        println!("judge-audit: budget stop — {e}");
                        write_report(paths, agree, disagree, audited, &disagreements)?;
                        return Ok(());
                    }
                };
                let user = format!(
                    "TASK: {}\n\nKNOWN CORRECT FACTS (JSON):\n{}\n\nAGENT ANSWER:\n{}\n\nVerdict (PASS or FAIL):",
                    task.prompt, gt, answer
                );
                let (resp, body) = client.messages(JUDGE_SYSTEM, &user, 4, &["\n"]).await?;
                let _ = body;
                let cost = cost_microusd(
                    &prices,
                    resp.usage.input_tokens,
                    resp.usage.cache_creation_input_tokens,
                    resp.usage.cache_read_input_tokens,
                    resp.usage.output_tokens,
                );
                budget.settle(id, BUCKET, cost)?;

                let judge_pass = resp.first_text().trim().to_uppercase().starts_with("PASS");
                audited += 1;
                if judge_pass == grade.pass {
                    agree += 1;
                } else {
                    disagree += 1;
                    disagreements.push(format!(
                        "{condition}/{}/{rep}: checker={} judge={}",
                        task.id,
                        if grade.pass { "PASS" } else { "FAIL" },
                        if judge_pass { "PASS" } else { "FAIL" },
                    ));
                }
            }
        }
    }

    write_report(paths, agree, disagree, audited, &disagreements)?;
    println!(
        "judge audit: {audited} audited, {agree} agree, {disagree} disagree (spent ${:.4})",
        budget.spent_bucket(BUCKET) as f64 / 1e6
    );
    Ok(())
}

fn write_report(
    paths: &BenchPaths,
    agree: u32,
    disagree: u32,
    audited: u32,
    disagreements: &[String],
) -> Result<()> {
    let mut md = String::new();
    md.push_str("# Judge audit\n\n");
    md.push_str("The deterministic checker is the grader; this binary LLM-judge audit re-scores every checker FAIL plus a deterministic 10% sample of PASSes, to surface disagreements. The judge never overrides the checker in headline numbers.\n\n");
    md.push_str(&format!(
        "- audited: {audited}\n- agree: {agree}\n- disagree: {disagree}\n- agreement rate: {}\n\n",
        if audited > 0 {
            format!("{:.0}%", 100.0 * agree as f64 / audited as f64)
        } else {
            "n/a".into()
        }
    ));
    if !disagreements.is_empty() {
        md.push_str("## Disagreements (for human review)\n\n");
        for d in disagreements {
            md.push_str(&format!("- {d}\n"));
        }
    }
    std::fs::write(paths.bench.join("judge-audit.md"), md)?;
    Ok(())
}

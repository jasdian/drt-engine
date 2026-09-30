//! The benchmark runner: ground-truth capture, the tree conditions
//! (full/last_k/decayed via LlmRouter over the cassette), and the no-runtime
//! control (a plain agentic loop free-driving the same leaves).
//!
//! Budget: each run reserves a projected cost BEFORE it starts and settles
//! the actual after — per-run granularity, so the $5 global cap can be
//! overshot by at most one run's cost (~$0.04). Simpler than per-call
//! bracketing and honest to within a run; recorded in the results notes.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use leaf_core::{Executor, FixedClock, JournalReader, RunConfig, RunOutcome, ScriptedRouter};
use leaf_llm::{EpsilonGreedy, LlmClient, LlmRouter, Provider};
use leaf_traits::{DecayedSummary, FullLog, LastK, LeafSpec, Propagation, ToonCodec, Transport};

use crate::artifacts::{RunMeta, RunPaths, UsageRecord, config_hash};
use crate::budget::Budget;
use crate::cassette::{CassetteStore, CassetteTransport};
use crate::checker::grade;
use crate::config::{BenchConfig, BenchPaths};
use crate::control;
use crate::tasks::TaskManifest;

pub const CONDITIONS: &[&str] = &["last_k", "decayed", "full", "control"];

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn propagation_for(condition: &str) -> Box<dyn Propagation> {
    match condition {
        "full" => Box::new(FullLog),
        "last_k" => Box::new(LastK { k: 4 }),
        "decayed" => Box::new(DecayedSummary {
            base_budget_chars: 600,
            decay_num: 1,
            decay_den: 2,
            min_chars: 40,
        }),
        other => panic!("no propagation for condition {other}"),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Ground truth: run each task's reference route with a ScriptedRouter (zero
/// LLM) over the cassette in record mode, snapshotting the final answer.
pub async fn ground_truth<T: Transport>(
    paths: &BenchPaths,
    tasks: &[TaskManifest],
    base_transport: &T,
    leaves: &BTreeMap<String, LeafSpec>,
) -> Result<()> {
    let store = CassetteStore::at(paths.cassette.clone());
    for task in tasks {
        // Record mode: seeding may hit live gh/curl and append to the cassette.
        let cassette = CassetteTransport::recording(base_transport, store.clone());
        let clock = FixedClock::new(0);
        let journal_path = paths
            .artifacts
            .join(format!("groundtruth-{}.jsonl", task.id));
        let _ = std::fs::remove_file(&journal_path);
        let mut exec = Executor {
            transport: &cassette,
            router: ScriptedRouter::new(task.reference_route.clone()),
            codec: &ToonCodec,
            propagation: &FullLog,
            clock: &clock,
            leaves,
        };
        let cfg = RunConfig {
            seed: 1,
            max_steps: 32,
            max_llm_calls: 1,
            input: task.run_input(),
            journal_path: journal_path.clone(),
        };
        let result = exec
            .run(&task.tree, &cfg)
            .await
            .with_context(|| format!("ground-truth reference route for {} failed", task.id))?;
        if result.outcome != RunOutcome::Done {
            bail!(
                "task {} reference route did not finish: {:?}",
                task.id,
                result.outcome
            );
        }
        let answer = result.last_output.unwrap_or_default();
        let snapshot: serde_json::Value = serde_json::from_str(answer.trim())
            .unwrap_or_else(|_| serde_json::Value::String(answer.clone()));
        std::fs::write(
            paths.ground_truth.join(format!("{}.json", task.id)),
            serde_json::to_string_pretty(&snapshot)?,
        )?;
        println!(
            "ground-truth {}: {} (cassette misses: {})",
            task.id,
            truncate(&answer, 80),
            cassette.misses.load(Ordering::SeqCst)
        );
    }
    println!("ground truth + cassette written under bench/ground-truth and bench/cassette");
    Ok(())
}

/// Run the full matrix (or a filtered subset), skipping complete runs.
#[allow(clippy::too_many_arguments)]
pub async fn run_matrix<T: Transport>(
    paths: &BenchPaths,
    cfg: &BenchConfig,
    tasks: &[TaskManifest],
    base_transport: &T,
    leaves: &BTreeMap<String, LeafSpec>,
    provider: Provider,
    api_key: &str,
    condition_filter: Option<&str>,
    task_filter: Option<&str>,
    live: bool,
) -> Result<()> {
    // --live: real GitHub, record-on-miss to the live cassette, graded against a
    // SAME-TIME live ground truth, written to a separate runs-live/ dir so the
    // frozen (fail-closed) runs are untouched. This is the apples-to-apples world:
    // same model + tools + live data for BOTH the tree and the free-driving control.
    let store = if live {
        CassetteStore::at(paths.cassette_live.clone())
    } else {
        CassetteStore::at(paths.cassette.clone())
    };
    if live {
        crate::live_control::capture_live_gt(paths, tasks, base_transport, leaves, &store).await?;
    }
    let runs_root = paths.artifacts.join(if live { "runs-live" } else { "runs" });
    let gt_dir = if live {
        &paths.ground_truth_live
    } else {
        &paths.ground_truth
    };
    let mut budget = Budget::load(
        paths.budget_file.clone(),
        cfg.budget_cap_microusd,
        cfg.buckets.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    )?;

    for condition in CONDITIONS {
        if let Some(f) = condition_filter
            && *condition != f
        {
            continue;
        }
        // A frozen control is fail-closed against the cassette (its
        // self-authored args miss the recording) — a replay artifact, not a
        // baseline. The report ignores it; don't burn billed calls on it.
        if !live && *condition == "control" {
            if condition_filter.is_some() {
                println!(
                    "skipping frozen control — fail-closed by construction; run it with --live"
                );
            }
            continue;
        }
        for task in tasks {
            if let Some(f) = task_filter
                && task.id != f
            {
                continue;
            }
            let gt = serde_json::from_str::<serde_json::Value>(
                &std::fs::read_to_string(gt_dir.join(format!("{}.json", task.id)))
                    .unwrap_or_default(),
            )
            .ok();
            let hash = config_hash(task, condition, &cfg.router_model);
            for rep in 0..cfg.run.repeats {
                let rp = RunPaths::at(&runs_root, condition, &task.id, rep);
                if rp.is_complete(&hash) {
                    continue;
                }
                let projected = budget.projection(condition);
                let reservation = match budget.reserve(condition, projected) {
                    Ok(id) => id,
                    Err(e) => {
                        println!("STOP: {e}");
                        println!("remaining cells left for a future resumed `run`.");
                        return Ok(());
                    }
                };

                let seed = cfg.run.seed_base + rep as u64;
                // Frozen: replay (fail-closed) so every condition shares the
                // recorded world. Live: record-on-miss against real GitHub.
                let cassette = if live {
                    CassetteTransport::recording(base_transport, store.clone())
                } else {
                    CassetteTransport::replaying(base_transport, store.clone())
                };

                // Clear stale artifacts from a prior (different-config) attempt
                // so a fresh run can't restamp a half-corrupt cell.
                for p in [&rp.meta, &rp.answer, &rp.grade, &rp.usage] {
                    let _ = std::fs::remove_file(p);
                }

                let work = async {
                    if *condition == "control" {
                        run_control(cfg, task, &cassette, leaves, provider, api_key, seed, &rp)
                            .await
                    } else {
                        run_tree(
                            cfg, task, &cassette, leaves, provider, api_key, condition, seed, &rp,
                        )
                        .await
                    }
                };
                // Whole-run wall-clock ceiling (matches AXI's 5-min/run).
                let outcome =
                    tokio::time::timeout(std::time::Duration::from_secs(task.timeout_secs), work)
                        .await;

                // On error/timeout, settle at the journaled partial spend (the
                // journal is flushed per entry, so it survives a dropped run),
                // floored at the reservation so we never under-count.
                let (actual, outcome_label) = match outcome {
                    Ok(Ok(cost)) => (cost, None),
                    Ok(Err(e)) => {
                        eprintln!("run {condition}/{}/{rep} errored: {e:#}", task.id);
                        (
                            rp.usage_record().cost_microusd.max(projected),
                            Some("Error"),
                        )
                    }
                    Err(_) => {
                        eprintln!(
                            "run {condition}/{}/{rep} timed out after {}s",
                            task.id, task.timeout_secs
                        );
                        (
                            rp.usage_record().cost_microusd.max(projected),
                            Some("Timeout"),
                        )
                    }
                };
                budget.settle(reservation, condition, actual)?;

                // Grade whatever answer landed. A run that errored/timed out
                // before writing meta still counts as a graded failure (correct
                // n, cost included) rather than silently vanishing.
                let answer = rp.load_answer().unwrap_or_default();
                // Live CONTROL cells drift: their self-authored queries hit
                // GitHub at cell time, minutes after the invocation GT. Grade
                // them against a per-cell GT captured seconds after the answer
                // (raw transport — the cassette would serve the stale first
                // snapshot). Tree cells need none of this: their fetches replay
                // from the same cassette world the invocation GT recorded.
                let cell_gt = if live && *condition == "control" {
                    match crate::live_control::capture_live_gt_one(
                        task,
                        base_transport,
                        leaves,
                        rp.dir.join("gt-journal.jsonl"),
                    )
                    .await
                    {
                        Ok(v) => {
                            std::fs::write(
                                rp.dir.join("gt.json"),
                                serde_json::to_string_pretty(&v)?,
                            )?;
                            Some(v)
                        }
                        Err(e) => {
                            println!(
                                "[--] per-cell GT capture failed for {}/{rep} ({e:#}) — grading against invocation GT",
                                task.id
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                let g = grade(&task.checker, cell_gt.as_ref().or(gt.as_ref()), &answer);
                let usage = rp.usage_record();
                let mut meta = rp.load_meta().unwrap_or_else(|| RunMeta {
                    task: task.id.clone(),
                    condition: condition.to_string(),
                    rep,
                    model: cfg.router_model.clone(),
                    seed,
                    config_hash: String::new(),
                    started_at_ms: now_ms(),
                    outcome: outcome_label.unwrap_or("Error").to_string(),
                    llm_calls: usage.llm_calls,
                    cost_microusd: usage.cost_microusd,
                    cassette_misses: cassette.misses.load(Ordering::SeqCst),
                });
                meta.config_hash = hash.clone();
                meta.rep = rep;
                if let Some(label) = outcome_label {
                    meta.outcome = label.to_string();
                }
                rp.write_all(&meta, &answer, &g, &usage)?;
                println!(
                    "{condition}/{}/{rep}: {} (${:.4} spent total)",
                    task.id,
                    if g.pass { "PASS" } else { "fail" },
                    budget.spent_total() as f64 / 1e6
                );
            }
        }
    }
    println!(
        "matrix done. total spend ${:.4}",
        budget.spent_total() as f64 / 1e6
    );
    Ok(())
}


impl RunPaths {
    pub(crate) fn usage_record(&self) -> UsageRecord {
        let entries = JournalReader::read_all(&self.journal).unwrap_or_default();
        let mut u = UsageRecord {
            cost_microusd: 0,
            llm_calls: 0,
            input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 0,
        };
        for e in &entries {
            if let leaf_core::JournalEntry::LlmCall { rec, .. } = e {
                u.cost_microusd += rec.cost_microusd;
                // Count billed API calls, not decisions (a re-ask is 2).
                u.llm_calls += rec.api_calls;
                u.input_tokens += rec.input_tokens;
                u.cache_creation_input_tokens += rec.cache_creation_input_tokens;
                u.cache_read_input_tokens += rec.cache_read_input_tokens;
                u.output_tokens += rec.output_tokens;
            }
        }
        u
    }
}

/// A tree condition: LlmRouter over the given propagation policy.
#[allow(clippy::too_many_arguments)]
async fn run_tree<T: Transport>(
    cfg: &BenchConfig,
    task: &TaskManifest,
    cassette: &CassetteTransport<'_, T>,
    leaves: &BTreeMap<String, LeafSpec>,
    provider: Provider,
    api_key: &str,
    condition: &str,
    seed: u64,
    rp: &RunPaths,
) -> Result<u64> {
    std::fs::create_dir_all(&rp.dir)?;
    let _ = std::fs::remove_file(&rp.journal);
    let client = LlmClient::new(provider, api_key.to_string(), cfg.router_model.clone());
    let prices = client.prices();
    let router = LlmRouter::new(
        client,
        EpsilonGreedy::new(seed, cfg.run.epsilon_ppm),
        prices,
    );
    let propagation = propagation_for(condition);
    let clock = FixedClock::new(now_ms());
    let mut exec = Executor {
        transport: cassette,
        router,
        codec: &ToonCodec,
        propagation: propagation.as_ref(),
        clock: &clock,
        leaves,
    };
    let run_cfg = RunConfig {
        seed,
        max_steps: 32,
        max_llm_calls: task.max_llm_calls,
        input: task.run_input(),
        journal_path: rp.journal.clone(),
    };
    let result = exec.run(&task.tree, &run_cfg).await?;
    let answer = result.last_output.clone().unwrap_or_default();
    std::fs::write(&rp.answer, &answer)?;
    write_meta(
        rp,
        task,
        condition,
        &cfg.router_model,
        seed,
        &format!("{:?}", result.outcome),
        result.llm_calls,
        result.total_cost_microusd,
        cassette.misses.load(Ordering::SeqCst),
    )?;
    Ok(result.total_cost_microusd)
}

/// The no-runtime control: a plain agentic loop free-driving the SAME leaves.
#[allow(clippy::too_many_arguments)]
async fn run_control<T: Transport>(
    cfg: &BenchConfig,
    task: &TaskManifest,
    cassette: &CassetteTransport<'_, T>,
    leaves: &BTreeMap<String, LeafSpec>,
    provider: Provider,
    api_key: &str,
    seed: u64,
    rp: &RunPaths,
) -> Result<u64> {
    std::fs::create_dir_all(&rp.dir)?;
    let manifests_dir = rp
        .dir
        .ancestors()
        .find(|p| p.join("manifests").is_dir())
        .map(|p| p.join("manifests"))
        .context("locating manifests dir")?;
    let client = LlmClient::new(provider, api_key.to_string(), cfg.router_model.clone());
    let outcome =
        control::run(task, cassette, leaves, &client, &manifests_dir, &rp.journal).await?;
    std::fs::write(&rp.answer, &outcome.answer)?;
    write_meta(
        rp,
        task,
        "control",
        &cfg.router_model,
        seed,
        &outcome.stop,
        outcome.llm_calls,
        outcome.cost_microusd,
        cassette.misses.load(Ordering::SeqCst),
    )?;
    Ok(outcome.cost_microusd)
}

#[allow(clippy::too_many_arguments)]
fn write_meta(
    rp: &RunPaths,
    task: &TaskManifest,
    condition: &str,
    model: &str,
    seed: u64,
    outcome: &str,
    llm_calls: u32,
    cost_microusd: u64,
    cassette_misses: u64,
) -> Result<()> {
    let meta = RunMeta {
        task: task.id.clone(),
        condition: condition.to_string(),
        rep: 0,
        model: model.to_string(),
        seed,
        config_hash: String::new(),
        started_at_ms: now_ms(),
        outcome: outcome.to_string(),
        llm_calls,
        cost_microusd,
        cassette_misses,
    };
    std::fs::write(&rp.meta, serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

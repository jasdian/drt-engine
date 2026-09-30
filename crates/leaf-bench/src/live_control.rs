//! The live-world control diagnostic (opt-in via `leaf-bench live-control`).
//!
//! Same free-driving control loop as the primary `control` condition, but its
//! network leaves hit REAL GitHub (record-on-miss into a SEPARATE cassette)
//! instead of failing closed against the frozen recording. The primary
//! benchmark measured that the fail-closed control scores 31% because 24/24 of
//! its failures never obtained data (its self-authored args, e.g.
//! `issues?...per_page=100&sort=created`, don't byte-match the frozen
//! recording). This diagnostic removes that confound to measure the control's
//! ORCHESTRATION once it can actually acquire data.
//!
//! Grading is the honest hard part: the frozen GT drifts for volatile tasks
//! (open-issue lists, "last 10 merged" windows), so a live control graded vs
//! stale frozen GT would fail on drift, not orchestration. We therefore capture
//! a SAME-TIME-WINDOW live ground truth (the reference route run live, right
//! before the control) and grade against THAT (fair), while also cross-checking
//! against the frozen GT and reporting the delta so drift is visible, not hidden.
//! It is a point-in-time measurement (live data is not replayable across days);
//! the record-on-miss cassette makes a single session's runs reproducible.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, bail};
use leaf_core::{Executor, FixedClock, RunConfig, RunOutcome, ScriptedRouter};
use leaf_llm::{LlmClient, Provider};
use leaf_traits::{FullLog, LeafSpec, ToonCodec, Transport};

use crate::artifacts::{RunMeta, RunPaths, config_hash};
use crate::budget::Budget;
use crate::cassette::{CassetteStore, CassetteTransport};
use crate::checker::grade;
use crate::config::{BenchConfig, BenchPaths};
use crate::control;
use crate::tasks::TaskManifest;

pub const CONDITION: &str = "control_live";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load_gt(dir: &std::path::Path, task_id: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{task_id}.json"))).ok()?).ok()
}

fn truncate80(s: &str) -> String {
    if s.chars().count() > 80 {
        format!("{}…", s.chars().take(80).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Capture one task's live ground truth by running its reference route (zero
/// LLM) on the given transport, journaling to `journal_path`. Pass a recording
/// cassette transport for the shared invocation snapshot; pass the RAW
/// transport for a per-cell capture — a cassette would serve the stale first
/// snapshot back (replay-first) and defeat the point.
pub(crate) async fn capture_live_gt_one<T: Transport>(
    task: &TaskManifest,
    transport: &T,
    leaves: &BTreeMap<String, LeafSpec>,
    journal_path: std::path::PathBuf,
) -> Result<serde_json::Value> {
    let clock = FixedClock::new(0);
    let _ = std::fs::remove_file(&journal_path);
    let mut exec = Executor {
        transport,
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
        journal_path,
    };
    let result = exec
        .run(&task.tree, &cfg)
        .await
        .with_context(|| format!("live ground-truth reference route for {} failed", task.id))?;
    if result.outcome != RunOutcome::Done {
        bail!(
            "live ground-truth for {} did not finish: {:?}",
            task.id,
            result.outcome
        );
    }
    let answer = result.last_output.unwrap_or_default();
    Ok(serde_json::from_str(answer.trim())
        .unwrap_or_else(|_| serde_json::Value::String(answer.clone())))
}

/// Phase 1: capture a same-time live ground truth by running each task's
/// reference route (zero LLM) live, recording into the SEPARATE live cassette.
pub(crate) async fn capture_live_gt<T: Transport>(
    paths: &BenchPaths,
    tasks: &[TaskManifest],
    base_transport: &T,
    leaves: &BTreeMap<String, LeafSpec>,
    live_store: &CassetteStore,
) -> Result<()> {
    println!("== capturing same-time live ground truth (reference routes, live) ==");
    for task in tasks {
        let cassette = CassetteTransport::recording(base_transport, live_store.clone());
        let snapshot = capture_live_gt_one(
            task,
            &cassette,
            leaves,
            paths.artifacts.join(format!("livegt-{}.jsonl", task.id)),
        )
        .await?;
        std::fs::write(
            paths.ground_truth_live.join(format!("{}.json", task.id)),
            serde_json::to_string_pretty(&snapshot)?,
        )?;
        println!("  live-gt {}: {}", task.id, truncate80(&snapshot.to_string()));
    }
    Ok(())
}

/// The live-world control diagnostic: capture same-time live GT, then run the
/// control loop live (record-on-miss, separate cassette), grading vs live GT
/// (fair) and cross-checking vs frozen GT (drift delta).
#[allow(clippy::too_many_arguments)]
pub async fn run<T: Transport>(
    paths: &BenchPaths,
    cfg: &BenchConfig,
    tasks: &[TaskManifest],
    base_transport: &T,
    leaves: &BTreeMap<String, LeafSpec>,
    provider: Provider,
    api_key: &str,
) -> Result<()> {
    let live_store = CassetteStore::at(paths.cassette_live.clone());
    capture_live_gt(paths, tasks, base_transport, leaves, &live_store).await?;

    let runs_root = paths.artifacts.join("runs");
    let mut budget = Budget::load(
        paths.budget_file.clone(),
        cfg.budget_cap_microusd,
        cfg.buckets.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    )?;

    // task -> (live_pass, frozen_pass, n)
    let mut per_task: BTreeMap<String, (u32, u32, u32)> = BTreeMap::new();

    for task in tasks {
        let live_gt = load_gt(&paths.ground_truth_live, &task.id);
        let frozen_gt = load_gt(&paths.ground_truth, &task.id);
        let hash = config_hash(task, CONDITION, &cfg.router_model);
        for rep in 0..cfg.run.repeats {
            let rp = RunPaths::at(&runs_root, CONDITION, &task.id, rep);
            let frozen_grade_path = rp.dir.join("grade-frozen.json");

            let (g_live_pass, g_frozen_pass) = if rp.is_complete(&hash) {
                let live = rp.load_grade().map(|g| g.pass).unwrap_or(false);
                let frozen = std::fs::read_to_string(&frozen_grade_path)
                    .ok()
                    .and_then(|t| serde_json::from_str::<crate::checker::GradeResult>(&t).ok())
                    .map(|g| g.pass)
                    .unwrap_or(false);
                (live, frozen)
            } else {
                let projected = budget.projection(CONDITION);
                let reservation = match budget.reserve(CONDITION, projected) {
                    Ok(id) => id,
                    Err(e) => {
                        println!("STOP: {e}");
                        break;
                    }
                };
                for p in [&rp.meta, &rp.answer, &rp.grade, &rp.usage, &frozen_grade_path] {
                    let _ = std::fs::remove_file(p);
                }
                std::fs::create_dir_all(&rp.dir)?;
                let cassette = CassetteTransport::recording(base_transport, live_store.clone());
                let client = LlmClient::new(provider, api_key.to_string(), cfg.router_model.clone());
                let outcome =
                    control::run(task, &cassette, leaves, &client, &paths.manifests, &rp.journal)
                        .await;
                let (answer, label) = match outcome {
                    Ok(o) => (o.answer, "Done"),
                    Err(e) => {
                        eprintln!("control_live {}/{rep} errored: {e:#}", task.id);
                        (String::new(), "Error")
                    }
                };
                let usage = rp.usage_record();
                let actual = usage.cost_microusd.max(projected);
                budget.settle(reservation, CONDITION, actual)?;

                let g_live = grade(&task.checker, live_gt.as_ref(), &answer);
                let g_frozen = grade(&task.checker, frozen_gt.as_ref(), &answer);
                let meta = RunMeta {
                    task: task.id.clone(),
                    condition: CONDITION.to_string(),
                    rep,
                    model: cfg.router_model.clone(),
                    seed: 0,
                    config_hash: hash.clone(),
                    started_at_ms: now_ms(),
                    outcome: label.to_string(),
                    llm_calls: usage.llm_calls,
                    cost_microusd: usage.cost_microusd,
                    cassette_misses: cassette.misses.load(Ordering::SeqCst),
                };
                rp.write_all(&meta, &answer, &g_live, &usage)?;
                std::fs::write(&frozen_grade_path, serde_json::to_string_pretty(&g_frozen)?)?;
                println!(
                    "control_live/{}/{rep}: live={} frozen={} (${:.4} spent total)",
                    task.id,
                    if g_live.pass { "PASS" } else { "fail" },
                    if g_frozen.pass { "PASS" } else { "fail" },
                    budget.spent_total() as f64 / 1e6
                );
                (g_live.pass, g_frozen.pass)
            };

            let e = per_task.entry(task.id.clone()).or_insert((0, 0, 0));
            e.2 += 1;
            if g_live_pass {
                e.0 += 1;
            }
            if g_frozen_pass {
                e.1 += 1;
            }
        }
    }

    let (mut live_p, mut frozen_p, mut n) = (0u32, 0u32, 0u32);
    println!("\n== live-world control (fair = same-time live GT; frozen = drift cross-check) ==");
    for (task, (lp, fp, tn)) in &per_task {
        println!("  {task}: live {lp}/{tn}  frozen {fp}/{tn}");
        live_p += lp;
        frozen_p += fp;
        n += tn;
    }
    if n > 0 {
        println!(
            "OVERALL live-world control: live-GT {}/{} ({:.0}%)  frozen-GT {}/{} ({:.0}%)  total spend ${:.4}",
            live_p,
            n,
            live_p as f64 / n as f64 * 100.0,
            frozen_p,
            n,
            frozen_p as f64 / n as f64 * 100.0,
            budget.spent_total() as f64 / 1e6
        );
    }
    Ok(())
}

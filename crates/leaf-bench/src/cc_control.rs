//! The Claude Code control (`leaf-bench cc-control`): AXI's actual baseline.
//!
//! AXI's `cli`/`axi` conditions ran on `claude -p` (Claude Code headless) + a
//! generic tool. Our earlier control — a bare ~150-line raw-API loop against a
//! FAIL-CLOSED cassette — was an unfair, crippled baseline. This runs the real
//! thing: `claude -p` (Sonnet) with generic `Bash` + a live, authenticated
//! `gh`, against REAL GitHub, one process per task. It is the fair free-driving
//! baseline for the routing runtime.
//!
//! Grading matches the tree: a same-time live ground truth (captured live right
//! before the runs) for fairness, cross-checked against the frozen GT. Cost is
//! the Anthropic subscription cost claude reports (`total_cost_usd`), summed and
//! bounded — SEPARATE from the OpenAI budget the tree runs on. Point-in-time.

use std::collections::BTreeMap;
use std::process::Stdio;

use anyhow::{Context, Result};
use leaf_traits::{LeafSpec, Transport};
use tokio::io::AsyncReadExt;

use crate::artifacts::{RunMeta, RunPaths};
use crate::checker::grade;
use crate::config::{BenchConfig, BenchPaths};
use crate::tasks::TaskManifest;

pub const CONDITION: &str = "control_cc";
/// Safety bound on Anthropic spend for this diagnostic (USD).
const CC_COST_CAP_USD: f64 = 4.0;
/// Repeats for the Claude Code control (each run is a full agentic session at
/// ~$0.05–0.20). Resumable: already-graded cells are skipped, so bumping this
/// only spends on the NEW reps.
const CC_REPS: u32 = 5;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load_gt(dir: &std::path::Path, task_id: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{task_id}.json"))).ok()?).ok()
}

/// One `claude -p` agentic run over the task prompt. Returns (answer, cost_usd).
async fn run_claude(prompt: &str, model: &str) -> Result<(String, f64, bool)> {
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("-p")
        .arg(prompt)
        .arg("--model")
        .arg(model)
        .arg("--allowedTools")
        .arg("Bash")
        .arg("--dangerously-skip-permissions")
        .arg("--output-format")
        .arg("json")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().context("spawn `claude` (is the CLI on PATH?)")?;
    let mut out = String::new();
    child
        .stdout
        .take()
        .context("claude stdout")?
        .read_to_string(&mut out)
        .await
        .context("read claude stdout")?;
    let status = child.wait().await.context("wait claude")?;
    let v: serde_json::Value = serde_json::from_str(out.trim())
        .with_context(|| format!("parse claude json (status {status}): {}", &out.chars().take(200).collect::<String>()))?;
    let answer = v.get("result").and_then(|r| r.as_str()).unwrap_or("").to_string();
    let cost = v.get("total_cost_usd").and_then(|c| c.as_f64()).unwrap_or(0.0);
    let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
    Ok((answer, cost, is_error))
}

pub async fn run<T: Transport>(
    paths: &BenchPaths,
    _cfg: &BenchConfig,
    tasks: &[TaskManifest],
    base_transport: &T,
    leaves: &BTreeMap<String, LeafSpec>,
) -> Result<()> {
    // Sonnet 5 (AXI parity). The `sonnet` alias resolves to the latest Sonnet.
    let model = "sonnet";
    let live_store = crate::cassette::CassetteStore::at(paths.cassette_live.clone());
    crate::live_control::capture_live_gt(paths, tasks, base_transport, leaves, &live_store).await?;

    let runs_root = paths.artifacts.join("runs");
    // task -> (live_pass, frozen_pass, n)
    let mut per_task: BTreeMap<String, (u32, u32, u32)> = BTreeMap::new();
    let mut total_cost = 0.0f64;

    // Rep-major (rep 0 for every task, then rep 1, …): when the spend cap
    // stops the run early it truncates DEPTH (fewer reps per task), never
    // BREADTH — a task-major sweep left later tasks with zero cells.
    'outer: for rep in 0..CC_REPS {
        for task in tasks {
            let live_gt = load_gt(&paths.ground_truth_live, &task.id);
            let frozen_gt = load_gt(&paths.ground_truth, &task.id);
            if total_cost >= CC_COST_CAP_USD {
                println!("cc-control: STOP — Anthropic spend ${total_cost:.2} hit the ${CC_COST_CAP_USD:.2} cap");
                break 'outer;
            }
            let rp = RunPaths::at(&runs_root, CONDITION, &task.id, rep);
            // Resume: an already-graded cell is kept (don't re-pay Anthropic).
            if rp.grade.exists() {
                let live = rp.load_grade().map(|g| g.pass).unwrap_or(false);
                let frozen = std::fs::read_to_string(rp.dir.join("grade-frozen.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<crate::checker::GradeResult>(&t).ok())
                    .map(|g| g.pass)
                    .unwrap_or(false);
                let e = per_task.entry(task.id.clone()).or_insert((0, 0, 0));
                e.2 += 1;
                if live {
                    e.0 += 1;
                }
                if frozen {
                    e.1 += 1;
                }
                continue;
            }
            std::fs::create_dir_all(&rp.dir)?;
            let (answer, cost, is_error) = match run_claude(&task.prompt, model).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("cc-control {}/{rep} errored: {e:#}", task.id);
                    (String::new(), 0.0, true)
                }
            };
            total_cost += cost;
            // cc runs always hit live GitHub — grade against a per-cell GT
            // captured seconds after the answer (raw transport, no cassette),
            // falling back to the invocation snapshot if the capture fails.
            let cell_gt = match crate::live_control::capture_live_gt_one(
                task,
                base_transport,
                leaves,
                rp.dir.join("gt-journal.jsonl"),
            )
            .await
            {
                Ok(v) => {
                    std::fs::write(rp.dir.join("gt.json"), serde_json::to_string_pretty(&v)?)?;
                    Some(v)
                }
                Err(e) => {
                    println!(
                        "[--] per-cell GT capture failed for {}/{rep} ({e:#}) — grading against invocation GT",
                        task.id
                    );
                    None
                }
            };
            let g_live = grade(&task.checker, cell_gt.as_ref().or(live_gt.as_ref()), &answer);
            let g_frozen = grade(&task.checker, frozen_gt.as_ref(), &answer);
            let meta = RunMeta {
                task: task.id.clone(),
                condition: CONDITION.to_string(),
                rep,
                model: model.to_string(),
                seed: 0,
                config_hash: String::new(),
                started_at_ms: now_ms(),
                outcome: if is_error { "Error" } else { "Done" }.to_string(),
                llm_calls: 0,
                cost_microusd: (cost * 1e6) as u64,
                cassette_misses: 0,
            };
            let usage = rp.usage_record();
            rp.write_all(&meta, &answer, &g_live, &usage)?;
            std::fs::write(
                rp.dir.join("grade-frozen.json"),
                serde_json::to_string_pretty(&g_frozen)?,
            )?;
            println!(
                "cc-control/{}/{rep}: live={} frozen={} (${cost:.4}; ${total_cost:.2} cc total)",
                task.id,
                if g_live.pass { "PASS" } else { "fail" },
                if g_frozen.pass { "PASS" } else { "fail" },
            );
            let e = per_task.entry(task.id.clone()).or_insert((0, 0, 0));
            e.2 += 1;
            if g_live.pass {
                e.0 += 1;
            }
            if g_frozen.pass {
                e.1 += 1;
            }
        }
    }

    let (mut lp, mut fp, mut n) = (0u32, 0u32, 0u32);
    println!("\n== Claude Code control (claude -p, {model}, Bash+gh, LIVE) ==");
    for (task, (l, f, tn)) in &per_task {
        println!("  {task}: live {l}/{tn}  frozen {f}/{tn}");
        lp += l;
        fp += f;
        n += tn;
    }
    if n > 0 {
        println!(
            "OVERALL cc-control: live-GT {lp}/{n} ({:.0}%)  frozen-GT {fp}/{n} ({:.0}%)  Anthropic spend ${total_cost:.2}",
            lp as f64 / n as f64 * 100.0,
            fp as f64 / n as f64 * 100.0,
        );
    }
    Ok(())
}

//! Phase-1 experiment 03: decomposition-ceiling test.
//!
//! Is $0.050/task even beatable in principle? For a floor task (view_pr,
//! ~zero routing) and a ceiling task (merged_pr_ci_audit, most data/steps),
//! measure the theoretical-best cost: `count_tokens` (free) on the ACTUAL
//! router prompts the runtime would send, plus a small assumed output, priced
//! at the pinned table.
//!
//! Pre-registered kill rule: if hand-optimal ≥ $0.050 on EITHER task, the
//! economic headline is dead — recorded verbatim, then we continue (the
//! project's value becomes the replay/audit capability).

use anyhow::{Context, Result};
use leaf_llm::{LlmClient, Prices, cost_microusd};
use leaf_traits::{CandidateInfo, Codec, FullLog, HistoryItem, Propagation, ToonCodec};

use crate::config::{BenchConfig, BenchPaths};
use crate::tasks::TaskManifest;

/// Assumed output tokens per routing decision — a candidate id ("c0") is a
/// few tokens; we budget 8 (the router's max_tokens) to be conservative.
const ASSUMED_OUTPUT_TOKENS: u64 = 8;

struct TaskCeiling {
    id: String,
    routing_calls: usize,
    prompt_tokens: u64,
    cost_microusd: u64,
}

pub async fn run(paths: &BenchPaths, _cfg: &BenchConfig, client: &LlmClient) -> Result<()> {
    let prices = client.prices();
    let targets = ["view_pr", "merged_pr_ci_audit"];
    let mut results = Vec::new();

    for id in targets {
        let manifest = TaskManifest::load(&paths.tasks.join(format!("{id}.toml")))
            .with_context(|| format!("loading task {id}"))?;
        let ceiling = measure(client, &prices, &manifest).await?;
        results.push(ceiling);
    }

    let headline_alive = results.iter().all(|r| r.cost_microusd < 50_000);
    write_report(paths, &results, headline_alive)?;
    for r in &results {
        println!(
            "{}: {} routing call(s), {} prompt tokens → hand-optimal ${:.4}",
            r.id,
            r.routing_calls,
            r.prompt_tokens,
            r.cost_microusd as f64 / 1e6
        );
    }
    if headline_alive {
        println!("headline ALIVE: every hand-optimal < $0.050");
    } else {
        println!("headline DEAD: a hand-optimal ≥ $0.050 — see report for honest reframing");
    }
    println!("report: docs/experiments/03-decomposition-ceiling.md");
    Ok(())
}

/// Reconstruct the router prompts the runtime WOULD send while walking the
/// task's reference route, and count their tokens. Each routing decision at a
/// branch counts as one call (a leaf pick and the final done pick).
async fn measure(
    client: &LlmClient,
    prices: &Prices,
    manifest: &TaskManifest,
) -> Result<TaskCeiling> {
    let codec = ToonCodec;
    let prop = FullLog;
    let branch = &manifest.tree.root;

    let candidates: Vec<CandidateInfo> = branch
        .candidates
        .iter()
        .enumerate()
        .map(|(i, n)| n.candidate_info(i))
        .collect();
    let menu = codec.render_candidates(&candidates);

    // Simulate history growth: each routing decision sees the run input plus
    // the outputs of prior leaves. We approximate leaf outputs with a modest
    // fixed stand-in so this is measurable offline; the real benchmark uses
    // actual outputs. Count tokens at each decision on the reference route.
    let mut history = vec![HistoryItem::RunInput {
        text: manifest.prompt.clone(),
    }];
    let mut prompt_tokens = 0u64;
    let mut routing_calls = 0usize;

    for &pick in &manifest.reference_route {
        let rendered = prop.render(&history, &codec);
        let user = leaf_llm::prompt::build_user(&branch.goal, &rendered, &menu);
        prompt_tokens += client
            .count_tokens(leaf_llm::prompt::PROMPT_V1, &user)
            .await
            .context("count_tokens on route prompt")?;
        routing_calls += 1;

        // Advance history as the executor would.
        if let Some(node) = branch.candidates.get(pick) {
            use leaf_core::Node;
            match node {
                Node::Done { name, .. } => {
                    history.push(HistoryItem::Decision {
                        branch: branch.name.clone(),
                        chosen: name.clone(),
                        explored: false,
                    });
                }
                Node::Leaf(l) => {
                    history.push(HistoryItem::Decision {
                        branch: branch.name.clone(),
                        chosen: l.name.clone(),
                        explored: false,
                    });
                    // Stand-in leaf output (offline estimate).
                    history.push(HistoryItem::LeafOutput {
                        leaf: l.leaf.clone(),
                        tool: l.tool.clone(),
                        output: "{\"stub\": \"offline ceiling estimate\"}".into(),
                        is_error: false,
                    });
                }
                Node::Branch(_) => {}
            }
        }
    }

    let output_tokens = ASSUMED_OUTPUT_TOKENS * routing_calls as u64;
    let cost = cost_microusd(prices, prompt_tokens, 0, 0, output_tokens);
    Ok(TaskCeiling {
        id: manifest.id.clone(),
        routing_calls,
        prompt_tokens,
        cost_microusd: cost,
    })
}

fn write_report(paths: &BenchPaths, results: &[TaskCeiling], alive: bool) -> Result<()> {
    let mut md = String::new();
    md.push_str("# Experiment 03: decomposition-ceiling test\n\n");
    md.push_str("**Question** — is $0.050/completed-task beatable *in principle* by a routing-only-LLM + deterministic-leaf decomposition?\n\n");
    md.push_str("**Prediction (pre-registered)** — the hand-optimal decomposition of `view_pr` (floor: ~1 routing call) is well under $0.050, and even `merged_pr_ci_audit` (ceiling: the most data-heavy AXI task) stays under it. Kill rule: if EITHER hand-optimal is ≥ $0.050, the economic headline is dead — recorded, and the project's value becomes the replay/audit capability, honestly framed.\n\n");
    md.push_str("**Method** — walk each task's reference route, reconstruct the exact router prompt at each decision (frozen PROMPT_V1 + TOON context + candidate menu), count its tokens via the router model's tokenizer (Anthropic `count_tokens`, or tiktoken `o200k_base` for OpenAI), add an assumed 8 output tokens per decision (the router's `max_tokens`), price at the configured model's pinned table (see bench.toml / pricing.rs). Leaf outputs are offline stand-ins here; the live benchmark measures the real thing. Deterministic-leaf work is $0 (no LLM).\n\n");
    md.push_str("## Results\n\n");
    md.push_str("| task | routing calls | prompt tokens | assumed output tokens | hand-optimal cost |\n|---|---|---|---|---|\n");
    for r in results {
        md.push_str(&format!(
            "| {} | {} | {} | {} | ${:.4} |\n",
            r.id,
            r.routing_calls,
            r.prompt_tokens,
            ASSUMED_OUTPUT_TOKENS * r.routing_calls as u64,
            r.cost_microusd as f64 / 1e6
        ));
    }
    md.push_str("\n## Decision\n\n");
    if alive {
        md.push_str("**Headline ALIVE.** Every hand-optimal decomposition is under $0.050/task, so a routing-only runtime *can* in principle beat the AXI headline. Whether it does in practice is the Phase-4 benchmark's job (`docs/results.md`); this experiment only establishes the ceiling is not self-defeating.\n");
    } else {
        md.push_str("**Headline DEAD.** At least one hand-optimal decomposition is ≥ $0.050/task, so the economic headline cannot be beaten even in the best case. Per the pre-registered rule, we record this verbatim and continue: the project's durable value is the seeded replay/audit capability, and the benchmark is reframed around reliability and LLM-calls/task, not the dollar figure.\n");
    }
    md.push_str("\nNote: this measures the runtime's OWN prompt shape (routing-only). A no-runtime control that free-drives tools pays for tool schemas + full-transcript resends on top — measured separately in `docs/results.md`.\n\n");
    md.push_str("## Reproduce\n\n```\nleaf-bench exp ceiling\n```\n");
    std::fs::write(
        paths.docs_experiments.join("03-decomposition-ceiling.md"),
        md,
    )?;
    Ok(())
}

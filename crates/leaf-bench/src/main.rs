//! leaf-bench: the harness + falsification experiments.
//!
//! Subcommands: preflight | ground-truth | exp <name> | run | live-control |
//! cc-control | judge-audit | judge-grade | report | budget | lint | regrade.

mod artifacts;
mod budget;
mod cassette;
mod cc_control;
mod checker;
mod config;
mod control;
mod exp;
mod judge;
mod judge_grade;
mod leaves;
mod lint;
mod live_control;
mod regrade;
mod report;
mod runner;
mod tasks;

use anyhow::{Context, Result, bail};
use config::{BenchConfig, BenchPaths, ResolvedKey, resolve_key};
use leaf_llm::LlmClient;
use leaf_mcp::StdioMcpTransport;
use tasks::TaskManifest;

fn repo_root() -> Result<std::path::PathBuf> {
    // Walk up from the current dir looking for bench/bench.toml.
    let mut dir = std::env::current_dir()?;
    loop {
        if dir.join("bench").join("bench.toml").is_file() {
            return Ok(dir);
        }
        if !dir.pop() {
            bail!("could not find bench/bench.toml above the current directory");
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // A CLI that agents (and humans) pipe into `head` must not panic on a
    // closed pipe — restore SIGPIPE's default (terminate quietly), which Rust
    // overrides to ignore at startup.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    let paths = BenchPaths::at(repo_root()?);
    paths.ensure_dirs()?;

    match cmd {
        "preflight" => preflight(&paths).await,
        "ground-truth" => ground_truth(&paths).await,
        "exp" => {
            let name = args.get(1).map(|s| s.as_str()).unwrap_or("");
            experiment(&paths, name).await
        }
        "run" => {
            let condition = flag(&args, "--condition");
            let task = flag(&args, "--task");
            let live = args.iter().any(|a| a == "--live");
            run_matrix(&paths, condition.as_deref(), task.as_deref(), live).await
        }
        "live-control" => {
            println!(
                "note: live-control is the ARCHIVED fine-grained-leaves diagnostic — the fair \
                 live comparison is the fused control in `run --live`; this bucket is ~spent by design"
            );
            live_control_cmd(&paths).await
        }
        "cc-control" => {
            let cfg = BenchConfig::load(&paths)?;
            let tasks = TaskManifest::load_all(&paths.tasks)?;
            for t in &tasks {
                if !paths.ground_truth.join(format!("{}.json", t.id)).exists() {
                    bail!("no frozen ground truth for {} — run `ground-truth` first", t.id);
                }
            }
            let (transport, specs) = build_transport_and_leaves(&paths, &tasks)?;
            cc_control::run(&paths, &cfg, &tasks, &transport, &specs).await
        }
        "judge-audit" => judge_audit(&paths).await,
        "judge-grade" => {
            let cfg = BenchConfig::load(&paths)?;
            let ResolvedKey { provider, key, .. } =
                resolve_key(&paths).context("`judge-grade` needs an API key")?;
            judge_grade::run(&paths, &cfg, provider, &key).await
        }
        "report" => {
            let cfg = BenchConfig::load(&paths)?;
            report::generate(&paths, &cfg)
        }
        "budget" => show_budget(&paths),
        "lint" => lint::run(&paths, &args),
        "regrade" => regrade::run(&paths, &args),
        _ => {
            println!(
                "leaf-bench <preflight | ground-truth | exp <encoding|determinism|ceiling> | run [--condition C] [--task T] | live-control | cc-control | judge-audit | judge-grade | report | budget | lint [--task T] [--strict] [--install-hook] | regrade [--task T] [--dry-run]>"
            );
            Ok(())
        }
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Which leaf ids every task references — the registry the runtime needs.
fn needed_leaves(tasks: &[TaskManifest]) -> Vec<String> {
    let mut ids: Vec<String> = tasks.iter().flat_map(|t| t.leaves.clone()).collect();
    ids.sort();
    ids.dedup();
    ids
}

async fn preflight(paths: &BenchPaths) -> Result<()> {
    println!("== leaf-bench preflight ==");
    let mut ok = true;

    match resolve_key(paths) {
        Some(r) => println!("[ok] {:?} key present (from {})", r.provider, r.source),
        None => {
            println!(
                "[--] no OPENAI_API_KEY / ANTHROPIC_API_KEY (copy .env.example → .env). Live phases will fail."
            );
        }
    }

    let tasks = TaskManifest::load_all(&paths.tasks)
        .context("loading task manifests (bench/tasks/*.toml)")?;
    println!("[ok] {} task manifests parse", tasks.len());

    let ids = needed_leaves(&tasks);
    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    let missing = leaves::missing_binaries(&id_refs);
    if missing.is_empty() {
        println!(
            "[ok] all required CLIs on PATH: {:?}",
            leaves::required_binaries(&id_refs)
        );
    } else {
        println!("[!!] missing CLIs: {missing:?}");
        ok = false;
    }

    match leaves::leaf_wrap_bin() {
        Ok(p) => println!("[ok] leaf-wrap at {}", p.display()),
        Err(e) => {
            println!("[!!] {e}");
            ok = false;
        }
    }

    // gh auth (only if any gh_ leaf is needed).
    if id_refs.iter().any(|i| i.starts_with("gh_")) {
        let gh = leaves::find_bin("gh");
        match gh {
            Some(gh) => {
                let status = std::process::Command::new(gh)
                    .args(["auth", "status"])
                    .output();
                match status {
                    Ok(o) if o.status.success() => println!("[ok] gh authenticated"),
                    _ => {
                        println!("[!!] gh not authenticated — run `gh auth login`");
                        ok = false;
                    }
                }
            }
            None => {
                println!("[!!] gh not on PATH");
                ok = false;
            }
        }
    }

    // count_tokens ping if a key exists (free on Anthropic; a ~1-token call on OpenAI).
    if let Some(r) = resolve_key(paths) {
        let cfg = BenchConfig::load(paths)?;
        let client = LlmClient::new(r.provider, r.key, cfg.router_model.clone());
        match client.count_tokens("", "ping").await {
            Ok(n) => println!(
                "[ok] LLM reachable, model {} ({n} tokens for 'ping')",
                cfg.router_model
            ),
            Err(e) => {
                println!("[!!] LLM call failed: {e}");
                ok = false;
            }
        }
    }

    if ok {
        println!("preflight OK");
        Ok(())
    } else {
        bail!("preflight found problems (see above)")
    }
}

fn build_transport_and_leaves(
    paths: &BenchPaths,
    tasks: &[TaskManifest],
) -> Result<(
    StdioMcpTransport,
    std::collections::BTreeMap<String, leaf_traits::LeafSpec>,
)> {
    let ids = needed_leaves(tasks);
    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    let generated = leaves::generate(paths, &id_refs)?;
    let transport = StdioMcpTransport {
        server_allowlist: generated.server_allowlist,
    };
    Ok((transport, generated.specs))
}

async fn ground_truth(paths: &BenchPaths) -> Result<()> {
    let tasks = TaskManifest::load_all(&paths.tasks)?;
    let (transport, specs) = build_transport_and_leaves(paths, &tasks)?;
    runner::ground_truth(paths, &tasks, &transport, &specs).await
}

async fn experiment(paths: &BenchPaths, name: &str) -> Result<()> {
    match name {
        "determinism" => exp::determinism::run(paths).await,
        "encoding" => {
            let cfg = BenchConfig::load(paths)?;
            let r = resolve_key(paths).context("encoding experiment needs an API key")?;
            let client = LlmClient::new(r.provider, r.key, cfg.router_model.clone());
            exp::encoding::run(paths, &cfg, &client).await
        }
        "ceiling" => {
            let cfg = BenchConfig::load(paths)?;
            let r = resolve_key(paths).context("ceiling experiment needs an API key")?;
            let client = LlmClient::new(r.provider, r.key, cfg.router_model.clone());
            exp::ceiling::run(paths, &cfg, &client).await
        }
        other => bail!("unknown experiment '{other}' (encoding | determinism | ceiling)"),
    }
}

async fn live_control_cmd(paths: &BenchPaths) -> Result<()> {
    let cfg = BenchConfig::load(paths)?;
    let ResolvedKey {
        provider,
        key,
        source,
    } = resolve_key(paths).context("`live-control` needs an API key")?;
    println!(
        "using {provider:?} key from {source}, model {} — LIVE-WORLD control (real GitHub, record-on-miss to a separate cassette)",
        cfg.router_model
    );
    let tasks = TaskManifest::load_all(&paths.tasks)?;
    // Frozen ground truth must exist for the drift cross-check.
    for t in &tasks {
        if !paths.ground_truth.join(format!("{}.json", t.id)).exists() {
            bail!(
                "no frozen ground truth for {} — run `leaf-bench ground-truth` first",
                t.id
            );
        }
    }
    let (transport, specs) = build_transport_and_leaves(paths, &tasks)?;
    live_control::run(paths, &cfg, &tasks, &transport, &specs, provider, &key).await
}

async fn run_matrix(
    paths: &BenchPaths,
    condition: Option<&str>,
    task: Option<&str>,
    live: bool,
) -> Result<()> {
    let cfg = BenchConfig::load(paths)?;
    let ResolvedKey {
        provider,
        key,
        source,
    } = resolve_key(paths).context("`run` needs an API key")?;
    println!(
        "using {provider:?} key from {source}, model {}",
        cfg.router_model
    );
    let tasks = TaskManifest::load_all(&paths.tasks)?;
    // Ground truth must exist for grading.
    for t in &tasks {
        if !paths.ground_truth.join(format!("{}.json", t.id)).exists() {
            bail!(
                "no ground truth for {} — run `leaf-bench ground-truth` first",
                t.id
            );
        }
    }
    let (transport, specs) = build_transport_and_leaves(paths, &tasks)?;
    runner::run_matrix(
        paths, &cfg, &tasks, &transport, &specs, provider, &key, condition, task, live,
    )
    .await
}

async fn judge_audit(paths: &BenchPaths) -> Result<()> {
    let cfg = BenchConfig::load(paths)?;
    let r = resolve_key(paths).context("judge-audit needs an API key")?;
    judge::run(paths, &cfg, r.provider, &r.key).await
}

fn show_budget(paths: &BenchPaths) -> Result<()> {
    let cfg = BenchConfig::load(paths)?;
    let budget = budget::Budget::load(
        paths.budget_file.clone(),
        cfg.budget_cap_microusd,
        cfg.buckets.iter().map(|(k, v)| (k.clone(), *v)).collect(),
    )?;
    println!(
        "total spend: ${:.4} / ${:.2} cap",
        budget.spent_total() as f64 / 1e6,
        cfg.budget_cap_microusd as f64 / 1e6
    );
    for (bucket, spent) in budget.spent_by_bucket() {
        let cap = cfg
            .buckets
            .get(&bucket)
            .map(|c| format!("${:.2}", *c as f64 / 1e6));
        println!(
            "  {bucket}: ${:.4}{}",
            spent as f64 / 1e6,
            cap.map(|c| format!(" / {c} cap")).unwrap_or_default()
        );
    }
    Ok(())
}

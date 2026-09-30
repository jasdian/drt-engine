//! Phase-1 experiment 02: determinism-of-world probe. Zero LLM, $0.
//!
//! Runs each probe 10× with a pinned environment (LC_ALL=C, TZ=UTC) and
//! counts byte-identical stdouts (sha256). The result calibrates which
//! determinism class each leaf may DECLARE: `pure` leaves may claim
//! `deterministic` (replay re-executes + byte-asserts), everything else is
//! `replay_only` (journaled replay mandatory; grading must never assert on
//! live values).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use leaf_core::sha256_hex;

use crate::config::BenchPaths;
use crate::leaves::find_bin;

struct Probe {
    name: &'static str,
    binary: &'static str,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    expectation: &'static str,
}

pub async fn run(paths: &BenchPaths) -> Result<()> {
    // Self-sufficient fixture so the probe doesn't depend on task authoring.
    let probe_fixture = paths.fixtures.join("probe.json");
    if !probe_fixture.exists() {
        std::fs::write(&probe_fixture, r#"[{"a":1},{"a":2},{"a":3}]"#)?;
    }

    let pinned = || {
        let mut env = BTreeMap::new();
        env.insert("LC_ALL".to_string(), "C".to_string());
        env.insert("TZ".to_string(), "UTC".to_string());
        env
    };
    let gh_pinned = || {
        let mut env = pinned();
        if let Ok(home) = std::env::var("HOME") {
            env.insert("HOME".to_string(), home);
        }
        env.insert("GH_NO_UPDATE_NOTIFIER".to_string(), "1".to_string());
        env
    };

    let probes = vec![
        Probe {
            name: "jq (pure transform)",
            binary: "jq",
            args: vec![
                "-c".into(),
                "length".into(),
                probe_fixture.to_string_lossy().into_owned(),
            ],
            env: pinned(),
            expectation: "10/10 identical (pure)",
        },
        Probe {
            name: "sha256sum (filesystem, fixture)",
            binary: "sha256sum",
            args: vec![probe_fixture.to_string_lossy().into_owned()],
            env: pinned(),
            expectation: "10/10 identical (fs_stable)",
        },
        Probe {
            name: "ls -1 (filesystem, directory)",
            binary: "ls",
            args: vec!["-1".into(), paths.fixtures.to_string_lossy().into_owned()],
            env: pinned(),
            expectation: "10/10 within a session; not across sessions",
        },
        Probe {
            name: "curl example.com (network, stable page)",
            binary: "curl",
            args: vec![
                "-s".into(),
                "--max-time".into(),
                "25".into(),
                "https://example.com".into(),
            ],
            env: pinned(),
            expectation: "body likely stable; still network class",
        },
        Probe {
            name: "gh api repos/openclaw/openclaw (network, live repo)",
            binary: "gh",
            args: vec!["api".into(), "repos/openclaw/openclaw".into()],
            env: gh_pinned(),
            expectation: "<10/10 (updated_at, counters drift)",
        },
    ];

    let mut rows = Vec::new();
    for probe in &probes {
        let Some(bin) = find_bin(probe.binary) else {
            rows.push((
                probe.name,
                0usize,
                0usize,
                "SKIPPED: binary not on PATH".to_string(),
            ));
            continue;
        };
        let mut digests = Vec::new();
        let mut failures = 0usize;
        for _ in 0..10 {
            let out = tokio::process::Command::new(&bin)
                .args(&probe.args)
                .env_clear()
                .envs(&probe.env)
                .output()
                .await
                .with_context(|| format!("running probe {}", probe.name))?;
            if out.status.success() {
                digests.push(sha256_hex(&out.stdout));
            } else {
                failures += 1;
            }
        }
        let distinct = {
            let mut d = digests.clone();
            d.sort();
            d.dedup();
            d.len()
        };
        let identical = if digests.is_empty() {
            0
        } else {
            digests.iter().filter(|d| **d == digests[0]).count()
        };
        let note = if failures > 0 {
            format!("{failures}/10 runs failed (network/auth?)")
        } else {
            format!("{distinct} distinct output(s)")
        };
        rows.push((probe.name, identical, digests.len(), note));
    }

    write_report(paths, &probes, &rows)?;
    for (name, identical, total, note) in &rows {
        println!("{name}: {identical}/{total} byte-identical — {note}");
    }
    println!("report: docs/experiments/02-determinism.md");
    Ok(())
}

fn write_report(
    paths: &BenchPaths,
    probes: &[Probe],
    rows: &[(&str, usize, usize, String)],
) -> Result<()> {
    let mut md = String::new();
    md.push_str("# Experiment 02: determinism-of-world probe\n\n");
    md.push_str("**Question** — which CLI tools may honestly declare `deterministic: true` (replay re-executes and byte-asserts) vs `replay_only` (journaled replay mandatory)?\n\n");
    md.push_str("**Prediction (pre-registered)** — pure/local tools 10/10 byte-identical; network tools not; therefore classes {pure, fs_stable, network}, with network ⇒ journal+replay mandatory.\n\n");
    md.push_str("**Method** — each probe run 10× with a scrubbed, pinned env (`LC_ALL=C`, `TZ=UTC`; gh additionally gets HOME for auth), stdout hashed with SHA-256, identical-to-first counted. Zero LLM calls; spend $0.00.\n\n");
    md.push_str("## Results\n\n");
    md.push_str("| probe | byte-identical | expectation | note |\n|---|---|---|---|\n");
    for (probe, (name, identical, total, note)) in probes.iter().zip(rows) {
        md.push_str(&format!(
            "| {name} | {identical}/{total} | {} | {note} |\n",
            probe.expectation
        ));
    }
    md.push_str("\n## Decision\n\n");
    md.push_str("- `jq`, `sha256sum` (fixture-pinned): may declare `deterministic` — replay re-executes them live and byte-asserts.\n");
    md.push_str("- `ls`: deterministic only while the directory is unchanged — treat as `fs_stable`; leaves over mutable directories must declare `replay_only`.\n");
    md.push_str("- `curl`, `gh`: `replay_only`, regardless of how stable a particular endpoint looks today. Grading must assert only against snapshots, never live values.\n");
    md.push_str("\nThese classes are encoded in the leaf manifests (`bench/artifacts/manifests/*.json`).\n\n");
    md.push_str("## Reproduce\n\n```\ncargo run -p leaf-bench -- exp determinism\n```\n");
    std::fs::create_dir_all(&paths.docs_experiments)?;
    std::fs::write(paths.docs_experiments.join("02-determinism.md"), md)?;
    Ok(())
}

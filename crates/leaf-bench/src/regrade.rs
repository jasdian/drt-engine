//! `leaf-bench regrade` — re-run the CURRENT deterministic checkers over the
//! STORED answers and rewrite grade files. Zero LLM, zero network, zero spend.
//!
//! Why it exists: grades are computed at run time and frozen into grade.json,
//! so a checker change (contains_values → regex, a new forbid pattern) used to
//! strand every already-paid answer — twice we re-paid Anthropic to re-answer
//! questions whose answers were already on disk. Regrading is a pure function
//! of (checker, ground truth, answer); this makes it re-runnable.
//!
//! Honesty note: cells are regraded against the CURRENT ground-truth
//! snapshots. Frozen GT is cassette-pinned (stable); LIVE GT drifts, so a
//! contains_values live cell can flip because the world moved, not because a
//! checker changed. Every flip is printed per cell — audit the summary before
//! committing, and use git to revert if a flip is drift, not intent.

use std::path::Path;

use anyhow::{Context, Result};

use crate::checker::{GradeResult, grade};
use crate::config::BenchPaths;
use crate::tasks::TaskManifest;

const USAGE: &str = "usage: leaf-bench regrade [--task <id>] [--dry-run]";

#[derive(Debug, Default)]
pub(crate) struct Stats {
    pub cells: u32,
    pub rewritten: u32,
    pub flips_to_pass: u32,
    pub flips_to_fail: u32,
}

pub fn run(paths: &BenchPaths, args: &[String]) -> Result<()> {
    let mut task_filter: Option<String> = None;
    let mut dry_run = false;
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--task" => match it.next() {
                Some(v) => task_filter = Some(v.clone()),
                None => {
                    eprintln!("error: --task requires a value\n{USAGE}");
                    std::process::exit(2);
                }
            },
            "--dry-run" => dry_run = true,
            other => {
                eprintln!("error: unknown flag '{other}' — valid flags: --task <id>, --dry-run\n{USAGE}");
                std::process::exit(2);
            }
        }
    }

    let tasks = TaskManifest::load_all(&paths.tasks)?;
    if let Some(id) = &task_filter {
        if !tasks.iter().any(|t| &t.id == id) {
            let known: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
            eprintln!("error: unknown task '{id}' — known tasks: {}\n{USAGE}", known.join(", "));
            std::process::exit(2);
        }
    }

    println!("== leaf-bench regrade{} ==", if dry_run { " (dry run)" } else { "" });
    let mut total = Stats::default();
    for (runs_root, live) in [
        (paths.artifacts.join("runs"), false),
        (paths.artifacts.join("runs-live"), true),
    ] {
        let stats = regrade_root(
            &runs_root,
            live,
            &tasks,
            &paths.ground_truth,
            &paths.ground_truth_live,
            task_filter.as_deref(),
            dry_run,
        )?;
        total.cells += stats.cells;
        total.rewritten += stats.rewritten;
        total.flips_to_pass += stats.flips_to_pass;
        total.flips_to_fail += stats.flips_to_fail;
    }
    println!(
        "regrade{}: {} cells checked, {} grade files {}, {} flipped fail→pass, {} flipped pass→fail",
        if dry_run { " (dry run)" } else { "" },
        total.cells,
        total.rewritten,
        if dry_run { "would change" } else { "rewritten" },
        total.flips_to_pass,
        total.flips_to_fail,
    );
    if total.rewritten > 0 && !dry_run {
        println!("grades changed — re-run `leaf-bench report` (and `judge-grade` if answers were also refreshed)");
    }
    Ok(())
}

/// Regrade every cell under one runs root. `runs/` grades grade.json against
/// FROZEN ground truth — except `control_cc`, whose grade.json is a live grade
/// (cc also carries grade-frozen.json, regraded against frozen GT). `runs-live/`
/// grades grade.json against LIVE ground truth.
fn regrade_root(
    runs_root: &Path,
    live: bool,
    tasks: &[TaskManifest],
    gt_dir: &Path,
    gt_live_dir: &Path,
    task_filter: Option<&str>,
    dry_run: bool,
) -> Result<Stats> {
    let mut stats = Stats::default();
    let Ok(conditions) = std::fs::read_dir(runs_root) else {
        return Ok(stats); // no runs of this kind yet
    };
    for cond in conditions.filter_map(|e| e.ok()).filter(|e| e.path().is_dir()) {
        let condition = cond.file_name().to_string_lossy().into_owned();
        // Legacy live-experiment cells were graded against a same-time live GT
        // that no longer exists — neither current GT is their baseline. Leave
        // their history alone.
        if condition == "control_live" {
            println!("[--] {condition}: legacy live-experiment artifacts — same-time GT not reconstructible, skipped");
            continue;
        }
        let cc = condition == "control_cc";
        for task_dir in std::fs::read_dir(cond.path())?.filter_map(|e| e.ok()) {
            let task_id = task_dir.file_name().to_string_lossy().into_owned();
            if let Some(f) = task_filter
                && task_id != f
            {
                continue;
            }
            let Some(task) = tasks.iter().find(|t| t.id == task_id) else {
                println!("[--] {condition}/{task_id}: no such task manifest — stale artifacts, skipped");
                continue;
            };
            let gt_frozen = load_gt(gt_dir, &task_id);
            let gt_live = load_gt(gt_live_dir, &task_id);
            for rep_dir in std::fs::read_dir(task_dir.path())?.filter_map(|e| e.ok()) {
                let dir = rep_dir.path();
                let Ok(answer) = std::fs::read_to_string(dir.join("answer.txt")) else {
                    continue;
                };
                stats.cells += 1;
                // grade.json: live grade for runs-live and for cc; frozen
                // otherwise. A cell that captured its own per-cell GT snapshot
                // (gt.json, written seconds after the answer) is regraded
                // against exactly that — the reconstructible same-time baseline.
                let cell_gt: Option<serde_json::Value> = std::fs::read_to_string(dir.join("gt.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok());
                let gt_main = if live || cc {
                    cell_gt.as_ref().or(gt_live.as_ref())
                } else {
                    gt_frozen.as_ref()
                };
                regrade_file(
                    &dir.join("grade.json"),
                    &grade(&task.checker, gt_main, &answer),
                    &format!("{condition}/{task_id}/{}", rep_dir.file_name().to_string_lossy()),
                    dry_run,
                    &mut stats,
                )?;
                // cc's second surface: the frozen-GT grade.
                let gf = dir.join("grade-frozen.json");
                if cc && gf.exists() {
                    regrade_file(
                        &gf,
                        &grade(&task.checker, gt_frozen.as_ref(), &answer),
                        &format!("{condition}/{task_id}/{} (frozen)", rep_dir.file_name().to_string_lossy()),
                        dry_run,
                        &mut stats,
                    )?;
                }
            }
        }
    }
    Ok(stats)
}

fn load_gt(dir: &Path, task_id: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(dir.join(format!("{task_id}.json"))).ok()?).ok()
}

/// Write the recomputed grade if it differs from the stored one; report flips.
fn regrade_file(
    path: &Path,
    fresh: &GradeResult,
    label: &str,
    dry_run: bool,
    stats: &mut Stats,
) -> Result<()> {
    let old: Option<GradeResult> = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let fresh_json = serde_json::to_string_pretty(fresh)?;
    let unchanged = std::fs::read_to_string(path).map(|t| t == fresh_json).unwrap_or(false);
    if unchanged {
        return Ok(());
    }
    let old_pass = old.map(|g| g.pass);
    if old_pass != Some(fresh.pass) {
        match fresh.pass {
            true => stats.flips_to_pass += 1,
            false => stats.flips_to_fail += 1,
        }
        println!(
            "[!!] {label}: {} → {}",
            match old_pass {
                Some(true) => "PASS",
                Some(false) => "fail",
                None => "(missing)",
            },
            if fresh.pass { "PASS" } else { "fail" },
        );
    }
    stats.rewritten += 1;
    if !dry_run {
        std::fs::write(path, fresh_json).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::CheckerSpec;

    fn manifest(id: &str, checker: CheckerSpec) -> TaskManifest {
        TaskManifest {
            id: id.into(),
            category: "c".into(),
            prompt: "p".into(),
            max_llm_calls: 12,
            timeout_secs: 300,
            leaves: vec![],
            reference_route: vec![0],
            tree: serde_json::from_str(
                r#"{"name":"t","version":"1","root":{"name":"root","goal":"g","route":"llm","candidates":[
                    {"node":"done","name":"f","description":"d","terminal":true}]}}"#,
            )
            .unwrap(),
            checker,
            grading_hint: None,
            input: Default::default(),
            expected_capability: Some(0),
        }
    }

    fn cell(root: &Path, condition: &str, task: &str, rep: u32, answer: &str, old_pass: bool) {
        let dir = root.join(condition).join(task).join(rep.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("answer.txt"), answer).unwrap();
        std::fs::write(
            dir.join("grade.json"),
            serde_json::json!({"pass": old_pass, "checker": "regex", "assertions": []}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn regrade_flips_with_current_checker_and_respects_dry_run() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("runs");
        let gt = tmp.path().join("gt");
        let gt_live = tmp.path().join("gt-live");
        std::fs::create_dir_all(&gt).unwrap();
        std::fs::create_dir_all(&gt_live).unwrap();

        // Old grade says fail, but under the CURRENT regex checker the stored
        // answer passes — the view_pr "Status: Merged" scenario.
        let tasks = vec![manifest(
            "view",
            CheckerSpec::Regex { must: vec![r"(?i)\bmerged\b".into()], forbid: vec![r"(?i)not\s+merged".into()] },
        )];
        cell(&runs, "full", "view", 0, "Status: Merged (2026-03-20)", false);
        // and a cell whose grade is already correct — must not be rewritten
        cell(&runs, "full", "view", 1, "it was not merged", false);

        // dry run: reports the flip, changes nothing on disk
        let s = regrade_root(&runs, false, &tasks, &gt, &gt_live, None, true).unwrap();
        assert_eq!((s.cells, s.flips_to_pass, s.flips_to_fail), (2, 1, 0), "{s:?}");
        let still_old: GradeResult = serde_json::from_str(
            &std::fs::read_to_string(runs.join("full/view/0/grade.json")).unwrap(),
        )
        .unwrap();
        assert!(!still_old.pass);

        // real run: rewrites the flipped cell
        let s = regrade_root(&runs, false, &tasks, &gt, &gt_live, None, false).unwrap();
        assert_eq!(s.flips_to_pass, 1, "{s:?}");
        let fresh: GradeResult = serde_json::from_str(
            &std::fs::read_to_string(runs.join("full/view/0/grade.json")).unwrap(),
        )
        .unwrap();
        assert!(fresh.pass);

        // idempotent: second run rewrites nothing
        let s = regrade_root(&runs, false, &tasks, &gt, &gt_live, None, false).unwrap();
        assert_eq!(s.rewritten, 0, "{s:?}");
    }

    #[test]
    fn regrade_contains_values_uses_frozen_gt_for_frozen_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("runs");
        let gt = tmp.path().join("gt");
        let gt_live = tmp.path().join("gt-live");
        std::fs::create_dir_all(&gt).unwrap();
        std::fs::create_dir_all(&gt_live).unwrap();
        std::fs::write(gt.join("count.json"), r#"{"n": 7}"#).unwrap();
        std::fs::write(gt_live.join("count.json"), r#"{"n": 9}"#).unwrap();

        let tasks = vec![manifest("count", CheckerSpec::ContainsValues)];
        cell(&runs, "full", "count", 0, "the count is 7", false);

        let s = regrade_root(&runs, false, &tasks, &gt, &gt_live, None, false).unwrap();
        // frozen GT (7) matches the answer — flips to pass; live GT (9) unused
        assert_eq!(s.flips_to_pass, 1, "{s:?}");
    }

    #[test]
    fn regrade_prefers_cell_local_gt() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("runs-live");
        let gt = tmp.path().join("gt");
        let gt_live = tmp.path().join("gt-live");
        std::fs::create_dir_all(&gt).unwrap();
        std::fs::create_dir_all(&gt_live).unwrap();
        // shared live GT says 9; the cell's own same-time snapshot says 7
        std::fs::write(gt_live.join("count.json"), r#"{"n": 9}"#).unwrap();
        let tasks = vec![manifest("count", CheckerSpec::ContainsValues)];
        cell(&runs, "control", "count", 0, "the count is 7", false);
        std::fs::write(runs.join("control/count/0/gt.json"), r#"{"n": 7}"#).unwrap();

        let s = regrade_root(&runs, true, &tasks, &gt, &gt_live, None, false).unwrap();
        // graded against the cell-local 7 -> pass (shared 9 would fail it)
        assert_eq!(s.flips_to_pass, 1, "{s:?}");
    }

    #[test]
    fn regrade_skips_legacy_control_live() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("runs");
        let gt = tmp.path().join("gt");
        std::fs::create_dir_all(&gt).unwrap();
        let tasks = vec![manifest(
            "view",
            CheckerSpec::Regex { must: vec![r"(?i)\bmerged\b".into()], forbid: vec![] },
        )];
        // would flip under the current checker — but control_live is history
        cell(&runs, "control_live", "view", 0, "Status: Merged", false);
        let s = regrade_root(&runs, false, &tasks, &gt, &gt, None, false).unwrap();
        assert_eq!((s.cells, s.rewritten), (0, 0), "{s:?}");
    }

    #[test]
    fn regrade_skips_unknown_task_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("runs");
        let gt = tmp.path().join("gt");
        std::fs::create_dir_all(&gt).unwrap();
        cell(&runs, "full", "ghost", 0, "answer", true);
        let tasks = vec![manifest("real", CheckerSpec::ContainsValues)];
        let s = regrade_root(&runs, false, &tasks, &gt, &gt, None, false).unwrap();
        assert_eq!(s.cells, 0, "{s:?}");
    }
}

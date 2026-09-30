//! `leaf-bench lint` — the mechanized static pass of the drt-no-mistakes gate.
//!
//! Offline, zero LLM, zero network: checks the decidable slice of the
//! invariants in docs/extending.md (skill §1–§10) over bench/tasks/, every
//! referenced tree, the leaf-def registry, ground-truth snapshots, and the
//! committed example manifests — before a run spends anything.
//!
//! Exit codes: 0 = no error findings (warnings allowed unless --strict),
//! 1 = error findings, 2 = usage error.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use leaf_core::{Branch, LeafNode, Node, RouteMode, Tree};
use leaf_mcp::WrapManifest;
use leaf_traits::DeterminismClass;

use crate::config::BenchPaths;
use crate::leaves::{self, LeafDef};
use crate::tasks::{CheckerSpec, TaskManifest};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Severity {
    /// Informational — printed, never counted, never promoted by --strict.
    Info,
    Warning,
    Error,
}

#[derive(Debug)]
pub(crate) struct Finding {
    pub check: &'static str,
    pub section: &'static str,
    pub severity: Severity,
    pub target: String,
    pub message: String,
}

impl Finding {
    fn new(
        check: &'static str,
        section: &'static str,
        severity: Severity,
        target: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Finding { check, section, severity, target: target.into(), message: message.into() }
    }
}

/// Check ids in report order, with the skill section and the zero-state line.
const CATALOG: &[(&str, &str, &str)] = &[
    ("task-load", "gate step 4", "all task manifests load"),
    ("floats", "skill §2", "trees float-free"),
    ("dash-dash", "skill §3", "every task-derived slot guarded (stdin / prefix / --)"),
    ("source-reduce", "skill §5", "all reduces read {source}"),
    ("terminal-done", "skill §7", "every sequence ends in done{terminal:true} or a nested branch"),
    ("gt-bool", "skill §6", "ground truth boolean-free"),
    ("gt-volatile", "skill §6", "no volatile timestamp keys in ground truth"),
    ("gt-joined-list", "skill §6", "no joined-list strings in ground truth"),
    ("gt-error-surface", "skill §6", "no raw error surfaces as ground truth"),
    ("goal-dupes", "skill §8", "goals non-empty, unique among siblings, selection-hint phrased"),
    ("routing-label", "skill §9", "every task labels a valid root capability"),
    ("route-shape", "skill §9", "every reference_route replays; task leaves cover the tree"),
    ("read-only", "skill §10", "no mutating flags or GraphQL mutations"),
    ("determinism-class", "skill §4", "determinism classes consistent with binaries"),
    ("gt-coverage", "gate step 5", "every task has a frozen ground truth"),
    ("manifest-hygiene", "skill §3/§4", "example manifests valid; generated-manifests dir gitignored"),
];

#[derive(Debug)]
struct Flags {
    task: Option<String>,
    strict: bool,
    install_hook: bool,
}

const USAGE: &str = "usage: leaf-bench lint [--task <id>] [--strict] [--install-hook]";

fn parse_flags(args: &[String]) -> std::result::Result<Flags, String> {
    let mut flags = Flags { task: None, strict: false, install_hook: false };
    let mut it = args.iter().skip(1); // args[0] == "lint"
    while let Some(a) = it.next() {
        match a.as_str() {
            "--task" => match it.next() {
                Some(v) => flags.task = Some(v.clone()),
                None => return Err("--task requires a value".into()),
            },
            "--strict" => flags.strict = true,
            "--install-hook" => flags.install_hook = true,
            other => {
                return Err(format!(
                    "unknown flag '{other}' — valid flags: --task <id>, --strict, --install-hook"
                ));
            }
        }
    }
    if flags.install_hook && (flags.task.is_some() || flags.strict) {
        return Err("--install-hook cannot be combined with other flags".into());
    }
    Ok(flags)
}

pub fn run(paths: &BenchPaths, args: &[String]) -> Result<()> {
    let flags = match parse_flags(args) {
        Ok(f) => f,
        Err(msg) => {
            eprintln!("error: {msg}\n{USAGE}");
            std::process::exit(2);
        }
    };

    if flags.install_hook {
        install_hook(&paths.root)?;
        return Ok(());
    }

    println!("== leaf-bench lint ==");
    let defs = leaves::defs();
    let (mut tasks, mut findings) = load_tasks(&paths.tasks, &paths.root);

    if let Some(id) = &flags.task {
        // A target that exists but fails to parse must surface its diagnostic
        // (exit 1), not a misleading "unknown task" (exit 2); and unrelated
        // broken files must not fail a scoped run.
        let target_load_failed =
            findings.iter().any(|f| f.check == "task-load" && &f.target == id);
        if !tasks.iter().any(|t| &t.id == id) && !target_load_failed {
            let known: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
            eprintln!("error: unknown task '{id}' — known tasks: {}\n{USAGE}", known.join(", "));
            std::process::exit(2);
        }
        tasks.retain(|t| &t.id == id);
        findings.retain(|f| f.check != "task-load" || &f.target == id);
    }

    let gt_files = lint_all(&tasks, &defs, paths, &mut findings);

    // Report per check in catalog order: a zero-state [ok] line or the findings.
    findings.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.check.cmp(b.check)).then(a.target.cmp(&b.target)));
    let mut errors = 0usize;
    let mut warnings = 0usize;
    for (check, section, zero_state) in CATALOG {
        let hits: Vec<&Finding> = findings.iter().filter(|f| &f.check == check).collect();
        if hits.is_empty() {
            println!("[ok] {check} — {zero_state} ({section})");
            continue;
        }
        for f in hits {
            // --strict promotes warnings to errors at emit time.
            let effective = match f.severity {
                Severity::Warning if flags.strict => Severity::Error,
                s => s,
            };
            let mark = match effective {
                Severity::Error => "[!!]",
                Severity::Warning | Severity::Info => "[--]",
            };
            match effective {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                Severity::Info => {}
            }
            println!("{mark} {} {}: {} ({})", f.check, f.target, f.message, f.section);
        }
    }

    let counts = format!(
        "{} tasks / {} leaf defs / {} ground-truth files",
        tasks.len(),
        defs.len(),
        gt_files
    );
    if errors == 0 {
        println!("lint OK — 0 errors, {warnings} warnings across {counts}");
        Ok(())
    } else {
        println!("lint: found {errors} errors, {warnings} warnings across {counts}");
        bail!("lint found {errors} error finding(s) (see above)")
    }
}

/// A task's tree_file must stay inside the repository — a path that resolves
/// outside lints green locally but no clone will have it.
fn check_tree_file_contained(task_path: &Path, root: &Path, stem: &str, out: &mut Vec<Finding>) {
    let Ok(text) = std::fs::read_to_string(task_path) else { return };
    let Ok(v) = text.parse::<toml::Value>() else { return };
    let Some(tf) = v.get("tree_file").and_then(|x| x.as_str()) else { return };
    let resolved = task_path.parent().unwrap_or(Path::new(".")).join(tf);
    if let (Ok(r), Ok(rt)) = (resolved.canonicalize(), root.canonicalize()) {
        if !r.starts_with(&rt) {
            out.push(Finding::new(
                "task-load",
                "gate step 4",
                Severity::Error,
                stem.to_string(),
                format!("tree_file '{tf}' escapes the repository — a fresh clone would not have it"),
            ));
        }
    }
}

/// Load every bench/tasks/*.toml, turning each failure into a finding instead
/// of aborting on the first bad file (which is what load_all would do).
fn load_tasks(tasks_dir: &Path, root: &Path) -> (Vec<TaskManifest>, Vec<Finding>) {
    let mut findings = Vec::new();
    let mut paths: Vec<std::path::PathBuf> = match std::fs::read_dir(tasks_dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect(),
        Err(e) => {
            findings.push(Finding::new(
                "task-load",
                "gate step 4",
                Severity::Error,
                tasks_dir.display().to_string(),
                format!("cannot read tasks dir: {e}"),
            ));
            return (Vec::new(), findings);
        }
    };
    paths.sort();
    if paths.is_empty() {
        // ensure_dirs() pre-creates the dir, so "empty" must not read as clean.
        findings.push(Finding::new(
            "task-load",
            "gate step 4",
            Severity::Error,
            tasks_dir.display().to_string(),
            "no task manifests found",
        ));
    }
    let mut tasks = Vec::new();
    for p in paths {
        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        match TaskManifest::load(&p) {
            Ok(t) => {
                check_tree_file_contained(&p, root, &stem, &mut findings);
                tasks.push(t);
            }
            Err(e) => findings.push(Finding::new(
                "task-load",
                "gate step 4",
                Severity::Error,
                stem,
                format!("{e:#}"),
            )),
        }
    }
    (tasks, findings)
}

/// All checks over the loaded tasks. Tree-scoped checks run once per distinct
/// tree (all committed tasks share general-tree.json). Returns the number of
/// ground-truth files scanned.
fn lint_all(
    tasks: &[TaskManifest],
    defs: &[LeafDef],
    paths: &BenchPaths,
    findings: &mut Vec<Finding>,
) -> usize {
    // Dedup trees by serialized form; remember which tasks share each.
    let mut trees: BTreeMap<String, (&Tree, Vec<&str>)> = BTreeMap::new();
    for t in tasks {
        let key = serde_json::to_string(&t.tree).expect("tree serializes");
        trees.entry(key).or_insert_with(|| (&t.tree, Vec::new())).1.push(&t.id);
    }
    for (tree, used_by) in trees.values() {
        let label = format!("tree '{}' (tasks: {})", tree.name, used_by.join(", "));
        check_floats(&label, tree, findings);
        check_dash_dash(&label, tree, defs, findings);
        check_source_reduce(&label, tree, defs, findings);
        check_terminal_done(&label, tree, findings);
        check_goals(&label, tree, findings);
        check_root_name(&label, tree, findings);
        check_tree_read_only(&label, tree, defs, findings);
    }

    check_defs_read_only(defs, findings);
    check_determinism_class(defs, findings);

    let mut gt_files = 0usize;
    for t in tasks {
        check_routing_label(t, findings);
        check_route_shape(
            &t.id,
            &t.tree,
            &t.reference_route,
            t.expected_capability,
            &t.leaves,
            t.max_llm_calls,
            defs,
            findings,
        );
        let gt_path = paths.ground_truth.join(format!("{}.json", t.id));
        if !gt_path.exists() {
            findings.push(Finding::new(
                "gt-coverage",
                "gate step 5",
                Severity::Warning,
                t.id.clone(),
                "no frozen ground truth — run `leaf-bench ground-truth`",
            ));
            continue;
        }
        match std::fs::read_to_string(&gt_path)
            .map_err(anyhow::Error::from)
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).map_err(Into::into))
        {
            Ok(gt) => {
                gt_files += 1;
                let graded_by_values = matches!(t.checker, CheckerSpec::ContainsValues);
                check_gt(&t.id, &gt, graded_by_values, findings);
            }
            Err(e) => findings.push(Finding::new(
                "gt-bool",
                "skill §6",
                Severity::Error,
                t.id.clone(),
                format!("ground truth unreadable: {e:#}"),
            )),
        }
    }

    check_manifest_hygiene(&paths.root, findings);
    gt_files
}

// ---------------------------------------------------------------- tree walks

/// Pre-order (path, branch) pairs, root included as "root".
fn branches<'a>(root: &'a Branch) -> Vec<(String, &'a Branch)> {
    fn walk<'a>(b: &'a Branch, path: String, out: &mut Vec<(String, &'a Branch)>) {
        out.push((path.clone(), b));
        for c in &b.candidates {
            if let Node::Branch(child) = c {
                walk(child, format!("{path}/{}", child.name), out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "root".to_string(), &mut out);
    out
}

fn leaf_nodes<'a>(root: &'a Branch) -> Vec<(String, &'a LeafNode)> {
    let mut out = Vec::new();
    for (path, b) in branches(root) {
        for c in &b.candidates {
            if let Node::Leaf(l) = c {
                out.push((format!("{path}/{}", l.name), l));
            }
        }
    }
    out
}

fn any_string_contains(v: &serde_json::Value, needles: &[&str]) -> bool {
    match v {
        serde_json::Value::String(s) => needles.iter().any(|n| s.contains(n)),
        serde_json::Value::Array(a) => a.iter().any(|v| any_string_contains(v, needles)),
        serde_json::Value::Object(o) => o.values().any(|v| any_string_contains(v, needles)),
        _ => false,
    }
}

// -------------------------------------------------------------------- checks

/// skill §2 — author-time float check: task parse never calls Tree::validate
/// (only Executor::run does), so this is new coverage, per-leaf for precision.
fn check_floats(label: &str, tree: &Tree, out: &mut Vec<Finding>) {
    for (path, l) in leaf_nodes(&tree.root) {
        if let Err(e) = l.args.validate_no_floats() {
            out.push(Finding::new(
                "floats",
                "skill §2",
                Severity::Error,
                format!("{label} {path}"),
                format!("{e} — use integer units (micro-USD u64, ppm u32)"),
            ));
        }
    }
}

/// skill §3 — an argv element that BEGINS with a placeholder receiving
/// task-derived data must be preceded by a literal `--`. Safe routes: stdin
/// (`stdin_from`), prefix-embedding (`query={query}`), or no task-derived
/// data in the slot at all (trusted authored programs like jq's {expr}).
fn check_dash_dash(label: &str, tree: &Tree, defs: &[LeafDef], out: &mut Vec<Finding>) {
    for (path, l) in leaf_nodes(&tree.root) {
        let Some(def) = defs.iter().find(|d| d.id == l.leaf) else {
            continue; // unknown leaf ids are route-shape's finding
        };
        let Some(args) = l.args.0.as_object() else { continue };
        for (key, value) in args {
            let task_derived = any_string_contains(value, &["{input.", "{input}"])
                || (def.binary != "jq"
                    && any_string_contains(value, &["{source}", "{last_output}"]));
            if !task_derived {
                continue;
            }
            if def.stdin_from == Some(key.as_str()) {
                continue; // stdin can never be parsed as a flag
            }
            let placeholder = format!("{{{key}}}");
            // Every occurrence of the placeholder must be guarded — a second,
            // bare slot after a prefix-embedded one is still injectable.
            let positions: Vec<usize> = def
                .args_template
                .iter()
                .enumerate()
                .filter(|(_, e)| e.contains(&placeholder))
                .map(|(i, _)| i)
                .collect();
            if positions.is_empty() {
                out.push(Finding::new(
                    "dash-dash",
                    "skill §3",
                    Severity::Error,
                    format!("{label} {path}"),
                    format!(
                        "arg '{key}' carries task-derived data but leaf def '{}' has no argv slot or stdin route for it",
                        def.id
                    ),
                ));
                continue;
            }
            for pos in positions {
                if !def.args_template[pos].starts_with(&placeholder) {
                    continue; // prefix-embedded ("query={query}") — cannot become a flag
                }
                if !def.args_template[..pos].iter().any(|e| *e == "--") {
                    out.push(Finding::new(
                        "dash-dash",
                        "skill §3",
                        Severity::Error,
                        format!("{label} {path}"),
                        format!(
                            "task-derived arg '{key}' fills a leading argv slot in leaf def '{}' with no `--` before it — a value starting with '-' would parse as a flag (fix: add \"--\", or route via stdin_from / a literal prefix)",
                            def.id
                        ),
                    ));
                }
            }
        }
    }
}

/// skill §5 — reduces must read {source} so a re-picked reduce is idempotent.
/// Keyed on the def, not the literal arg name "input": the rule applies to
/// the document a jq reduce consumes (whatever its stdin arg is called), and
/// never to a non-jq leaf that happens to have an arg named "input".
fn check_source_reduce(label: &str, tree: &Tree, defs: &[LeafDef], out: &mut Vec<Finding>) {
    for (path, l) in leaf_nodes(&tree.root) {
        let Some(def) = defs.iter().find(|d| d.id == l.leaf) else { continue };
        if def.binary != "jq" {
            continue;
        }
        let Some(stdin_key) = def.stdin_from else { continue };
        if let Some(input) = l.args.0.get(stdin_key) {
            if input.as_str() != Some("{source}") {
                out.push(Finding::new(
                    "source-reduce",
                    "skill §5",
                    Severity::Error,
                    format!("{label} {path}"),
                    format!("reduce arg '{stdin_key}' is {input} — must be \"{{source}}\""),
                ));
            }
        }
    }
}

/// skill §7 — the done/terminal rules apply to sequence branches only: in an
/// llm branch a done{terminal:false} is a legitimate escape hatch (the root's
/// trailing "nothing fits" done is one), so llm branches are exempt from all
/// but the empty-candidates rule — an empty branch of EITHER route is a dead
/// end no other check (or the scripted route walk) would ever visit.
fn check_terminal_done(label: &str, tree: &Tree, out: &mut Vec<Finding>) {
    for (path, b) in branches(&tree.root) {
        if b.candidates.is_empty() {
            out.push(Finding::new(
                "terminal-done",
                "skill §7",
                Severity::Error,
                format!("{label} {path}"),
                "branch has no candidates — a dead end the router could still descend into",
            ));
            continue;
        }
        if b.route != RouteMode::Sequence {
            continue;
        }
        for c in &b.candidates {
            if let Node::Done { name, terminal: false, .. } = c {
                out.push(Finding::new(
                    "terminal-done",
                    "skill §7",
                    Severity::Error,
                    format!("{label} {path}/{name}"),
                    "done without terminal:true inside a sequence — pops to the parent and wastes a billed routing call",
                ));
            }
        }
        match b.candidates.last() {
            Some(Node::Done { terminal: true, .. }) | Some(Node::Branch(_)) => {}
            Some(Node::Done { terminal: false, .. }) => {} // already reported above
            Some(Node::Leaf(l)) => out.push(Finding::new(
                "terminal-done",
                "skill §7",
                Severity::Error,
                format!("{label} {path}/{}", l.name),
                "sequence does not end in done{terminal:true} (or a nested branch)",
            )),
            None => unreachable!("empty candidates handled above"),
        }
    }
}

/// skill §8 — goals are the routing surface: non-empty everywhere, unique
/// among sibling branches, and root capabilities phrased as selection hints.
fn check_goals(label: &str, tree: &Tree, out: &mut Vec<Finding>) {
    for (path, b) in branches(&tree.root) {
        if b.goal.trim().is_empty() {
            out.push(Finding::new(
                "goal-dupes",
                "skill §8",
                Severity::Error,
                format!("{label} {path}"),
                "branch goal is empty — the router routes on this text",
            ));
        }
        // The routing menu of an llm branch is every candidate's description
        // (a child branch's description IS its goal) — dedupe across all of
        // them, not just branch children. Sequences never route: skip.
        if b.route == RouteMode::Llm {
            let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
            for c in &b.candidates {
                let desc = c.description().trim();
                if desc.is_empty() {
                    continue; // empty branch goals are reported by the walk above
                }
                if let Some(prev) = seen.insert(desc, c.name()) {
                    out.push(Finding::new(
                        "goal-dupes",
                        "skill §8",
                        Severity::Error,
                        format!("{label} {path}/{}", c.name()),
                        format!("menu description duplicates sibling '{prev}' — the router cannot tell them apart"),
                    ));
                }
            }
        }
    }
    for c in &tree.root.candidates {
        if let Node::Branch(child) = c {
            if !child.goal.trim_start().starts_with("Use when") {
                out.push(Finding::new(
                    "goal-dupes",
                    "skill §8",
                    Severity::Warning,
                    format!("{label} root/{}", child.name),
                    "root capability goal does not start with \"Use when\" — the selection-hint convention that carries routing accuracy",
                ));
            }
        }
    }
}

/// skill §9 — routing-accuracy scoring matches the FIRST BranchDecision whose
/// branch is literally named "root" (report.rs); a renamed root would make
/// every task on the tree silently unscored.
fn check_root_name(label: &str, tree: &Tree, out: &mut Vec<Finding>) {
    if tree.root.name != "root" {
        out.push(Finding::new(
            "routing-label",
            "skill §9",
            Severity::Warning,
            label.to_string(),
            format!(
                "root branch is named '{}' — routing-accuracy scoring matches the literal name 'root' and would silently skip every task on this tree",
                tree.root.name
            ),
        ));
    }
}

/// skill §9 — a missing label is silently skipped by routing-accuracy scoring
/// (report.rs); the gate upgrades that to an error.
fn check_routing_label(task: &TaskManifest, out: &mut Vec<Finding>) {
    match task.expected_capability {
        None => out.push(Finding::new(
            "routing-label",
            "skill §9",
            Severity::Error,
            task.id.clone(),
            "missing expected_capability — the task would be silently skipped by routing-accuracy scoring",
        )),
        Some(i) if i >= task.tree.root.candidates.len() => out.push(Finding::new(
            "routing-label",
            "skill §9",
            Severity::Error,
            task.id.clone(),
            format!(
                "expected_capability {i} is out of range (root has {} candidates)",
                task.tree.root.candidates.len()
            ),
        )),
        Some(i) => {
            if !matches!(task.tree.root.candidates[i], Node::Branch(_)) {
                out.push(Finding::new(
                    "routing-label",
                    "skill §9",
                    Severity::Warning,
                    task.id.clone(),
                    format!("expected_capability {i} points at a non-branch root candidate"),
                ));
            }
        }
    }
}

/// skill §9 + gate step 5 — reference_route must replay through the tree
/// (entries are consumed ONLY at llm branches; sequences walk free), the
/// label must agree with the route's root pick, and the task/registry must
/// cover every leaf id the tree references.
#[allow(clippy::too_many_arguments)]
fn check_route_shape(
    task_id: &str,
    tree: &Tree,
    reference_route: &[usize],
    expected_capability: Option<usize>,
    task_leaves: &[String],
    max_llm_calls: u32,
    defs: &[LeafDef],
    out: &mut Vec<Finding>,
) {
    let def_ids: BTreeSet<&str> = defs.iter().map(|d| d.id).collect();
    let tree_leaf_ids: BTreeSet<&str> =
        leaf_nodes(&tree.root).iter().map(|(_, l)| l.leaf.as_str()).collect();

    let mut unknown_reported: BTreeSet<&str> = BTreeSet::new();
    for (path, l) in leaf_nodes(&tree.root) {
        match defs.iter().find(|d| d.id == l.leaf) {
            None => {
                if unknown_reported.insert(l.leaf.as_str()) {
                    out.push(Finding::new(
                        "route-shape",
                        "skill §9",
                        Severity::Error,
                        task_id.to_string(),
                        format!("tree references unknown leaf id '{}' (not in the registry)", l.leaf),
                    ));
                }
            }
            // The executor calls `handle.call(&leaf_node.tool, …)` and the
            // wrapped server serves exactly one tool named def.tool_name — a
            // mismatch lints clean everywhere else but kills the first call.
            Some(def) if l.tool != def.tool_name => out.push(Finding::new(
                "route-shape",
                "skill §9",
                Severity::Error,
                task_id.to_string(),
                format!(
                    "{path}: tool '{}' does not match leaf def '{}' (its tool is '{}') — every call to this leaf would fail with 'unknown tool'",
                    l.tool, def.id, def.tool_name
                ),
            )),
            Some(_) => {}
        }
    }
    for id in task_leaves {
        if !def_ids.contains(id.as_str()) {
            out.push(Finding::new(
                "route-shape",
                "skill §9",
                Severity::Error,
                task_id.to_string(),
                format!("task declares unknown leaf id '{id}' (not in the registry)"),
            ));
        }
    }
    let declared: BTreeSet<&str> = task_leaves.iter().map(|s| s.as_str()).collect();
    let missing: Vec<&&str> = tree_leaf_ids.iter().filter(|id| !declared.contains(**id)).collect();
    if !missing.is_empty() {
        out.push(Finding::new(
            "route-shape",
            "skill §9",
            Severity::Error,
            task_id.to_string(),
            format!(
                "task.leaves does not cover tree leaf id(s): {}",
                missing.iter().map(|s| **s).collect::<Vec<_>>().join(", ")
            ),
        ));
    }

    if tree.root.route == RouteMode::Llm {
        if let (Some(e), Some(first)) = (expected_capability, reference_route.first()) {
            if *first != e {
                out.push(Finding::new(
                    "route-shape",
                    "skill §9",
                    Severity::Error,
                    task_id.to_string(),
                    format!(
                        "reference_route[0] = {first} disagrees with expected_capability = {e} — the scripted ground truth would exercise a different capability than the label claims"
                    ),
                ));
            }
        }
    }

    // Every reference_route entry is one billed llm decision — a route longer
    // than the task's cap would end MaxLlmCalls at runtime.
    if reference_route.len() as u32 > max_llm_calls {
        out.push(Finding::new(
            "route-shape",
            "skill §9",
            Severity::Error,
            task_id.to_string(),
            format!(
                "reference_route needs {} llm decisions but max_llm_calls = {max_llm_calls} — the run would end MaxLlmCalls before finishing",
                reference_route.len()
            ),
        ));
    }

    // Walk simulation — a transliteration of the executor's match arms:
    // llm branches pop one reference_route entry per route() call, sequence
    // branches cursor-walk for free, done{terminal:true} ends the run,
    // done{terminal:false} pops to the parent. Every walk iteration is one
    // executor step (decisions, leaf runs, and defensive pops all count), and
    // the runners hardcode max_steps = 32 — so a route that needs more steps
    // lints red here instead of burning live calls before MaxSteps.
    const MAX_STEPS: usize = 32;
    let mut stack: Vec<(&Branch, usize)> = vec![(&tree.root, 0)];
    let mut idx = 0usize;
    let mut iterations = 0usize;
    while let Some(frame) = stack.last_mut() {
        iterations += 1;
        if iterations > MAX_STEPS {
            out.push(Finding::new(
                "route-shape",
                "skill §9",
                Severity::Error,
                task_id.to_string(),
                format!(
                    "route needs more than the executor's max_steps ({MAX_STEPS}) — the run would end MaxSteps before finishing (runner.rs hardcodes the cap)"
                ),
            ));
            return;
        }
        let (branch, node) = match frame.0.route {
            RouteMode::Llm => {
                let branch = frame.0;
                if idx >= reference_route.len() {
                    out.push(Finding::new(
                        "route-shape",
                        "skill §9",
                        Severity::Error,
                        task_id.to_string(),
                        format!(
                            "reference_route exhausted at llm branch '{}' — it needs one entry per genuine decision",
                            branch.name
                        ),
                    ));
                    return;
                }
                let pick = reference_route[idx];
                idx += 1;
                let Some(node) = branch.candidates.get(pick) else {
                    out.push(Finding::new(
                        "route-shape",
                        "skill §9",
                        Severity::Error,
                        task_id.to_string(),
                        format!(
                            "reference_route entry {pick} is out of range at llm branch '{}' ({} candidates)",
                            branch.name,
                            branch.candidates.len()
                        ),
                    ));
                    return;
                };
                (branch, Some(node))
            }
            RouteMode::Sequence => {
                let cursor = frame.1;
                frame.1 += 1;
                let branch = frame.0;
                match branch.candidates.get(cursor) {
                    Some(node) => (branch, Some(node)),
                    None => (branch, None), // ran past the end — defensive pop
                }
            }
        };
        let _ = branch;
        match node {
            None => {
                stack.pop();
            }
            Some(Node::Leaf(_)) => {} // leaf runs; control returns to the same branch
            Some(Node::Branch(b)) => stack.push((b, 0)),
            Some(Node::Done { terminal: true, .. }) => break,
            Some(Node::Done { terminal: false, .. }) => {
                stack.pop();
            }
        }
    }
    if idx != reference_route.len() {
        out.push(Finding::new(
            "route-shape",
            "skill §9",
            Severity::Error,
            task_id.to_string(),
            format!(
                "{} unused reference_route entr(y/ies) — entries are consumed only at llm decision points",
                reference_route.len() - idx
            ),
        ));
    }
}

/// skill §10 (leaf-def side) — mutating flags in argv templates, including
/// the implicit-POST ones (gh api -f/-F/--field/--input force POST on REST
/// endpoints; curl -F/--json are POST shorthands; wget --post-*).
fn check_defs_read_only(defs: &[LeafDef], out: &mut Vec<Finding>) {
    for def in defs {
        // `gh api graphql -f query=…` is the read path — GraphQL rides POST
        // by design, and writes are caught by the tree-side mutation check.
        let is_graphql = def.args_template.iter().any(|e| *e == "graphql");
        for e in &def.args_template {
            let gh_field_flag = *e == "-f"
                || *e == "-F"
                || *e == "--field"
                || e.starts_with("--field=")
                || *e == "--raw-field"
                || e.starts_with("--raw-field=")
                || *e == "--input"
                || e.starts_with("--input=");
            let mutating = *e == "-d"
                || *e == "-T"
                || e.starts_with("-X")
                || *e == "--request"
                || e.starts_with("--request=")
                || *e == "--method"
                || e.starts_with("--method=")
                || e.starts_with("--data")
                || e.starts_with("--upload-file")
                || e.starts_with("--post-data")
                || e.starts_with("--post-file")
                || *e == "--json"
                || e.starts_with("--json=")
                || *e == "--form"
                || e.starts_with("--form=")
                || (!is_graphql && gh_field_flag);
            if mutating {
                out.push(Finding::new(
                    "read-only",
                    "skill §10",
                    Severity::Error,
                    def.id.to_string(),
                    format!("argv template element '{e}' is a mutating flag — benchmark leaves must stay read-only"),
                ));
            }
        }
    }
}

/// skill §10 (tree side) — GraphQL mutations, scoped to graphql-tool query
/// slots so the word "mutation" in a prompt or jq expr never flags.
fn check_tree_read_only(label: &str, tree: &Tree, defs: &[LeafDef], out: &mut Vec<Finding>) {
    let mutation = regex::Regex::new(r"^\s*mutation\b").expect("static regex");
    for (path, l) in leaf_nodes(&tree.root) {
        let is_graphql = defs.iter().any(|d| d.id == l.leaf && d.tool_name == "graphql");
        if !is_graphql {
            continue;
        }
        if let Some(q) = l.args.0.get("query").and_then(|v| v.as_str()) {
            if mutation.is_match(q) {
                out.push(Finding::new(
                    "read-only",
                    "skill §10",
                    Severity::Error,
                    format!("{label} {path}"),
                    "GraphQL query is a mutation — benchmark leaves must stay read-only",
                ));
            }
        }
    }
}

/// skill §4 — the connect-time cross-check is tautological here (manifest and
/// spec read the same LeafDef field), so this binary-name heuristic is the
/// only static coverage; the real catch stays gate step 8 (frozen replay).
fn check_determinism_class(defs: &[LeafDef], out: &mut Vec<Finding>) {
    const NETWORK: &[&str] = &["gh", "curl", "wget"];
    const PURE: &[&str] = &["jq", "sha256sum"];
    for def in defs {
        if !NETWORK.contains(&def.binary) && !PURE.contains(&def.binary) {
            out.push(Finding::new(
                "determinism-class",
                "skill §4",
                Severity::Info,
                def.id.to_string(),
                format!(
                    "binary '{}' has no static determinism classification — frozen replay (gate step 8) is the only honesty check for it",
                    def.binary
                ),
            ));
        }
        if NETWORK.contains(&def.binary) && def.determinism != DeterminismClass::ReplayOnly {
            out.push(Finding::new(
                "determinism-class",
                "skill §4",
                Severity::Error,
                def.id.to_string(),
                format!(
                    "binary '{}' is a network tool but is not declared ReplayOnly — replay would re-execute and diverge",
                    def.binary
                ),
            ));
        }
        if PURE.contains(&def.binary) && def.determinism == DeterminismClass::ReplayOnly {
            out.push(Finding::new(
                "determinism-class",
                "skill §4",
                Severity::Warning,
                def.id.to_string(),
                format!(
                    "binary '{}' is a pure function of its inputs but is declared ReplayOnly — replay loses the byte-assert",
                    def.binary
                ),
            ));
        }
    }
}

/// skill §6 — booleans, volatile timestamp keys, joined-list strings, and raw
/// error surfaces in a ground-truth snapshot. The last two only matter when
/// the checker is contains_values (GT values ARE the assertions then); a
/// regex-checked task's GT is informational, so they are skipped.
/// GT is not always an object (nonexistent_repo.json is a bare string).
fn check_gt(task_id: &str, gt: &serde_json::Value, graded_by_values: bool, out: &mut Vec<Finding>) {
    if graded_by_values {
        if let Some(s) = gt.as_str() {
            if regex::Regex::new(r"^exit \d+:").expect("static regex").is_match(s) {
                out.push(Finding::new(
                    "gt-error-surface",
                    "skill §6",
                    Severity::Warning,
                    task_id.to_string(),
                    "ground truth is a raw tool error surface — only the tree can reproduce it verbatim; use a regex checker so any arm's honest error report grades",
                ));
            }
        }
    }
    fn joined_list(task_id: &str, v: &serde_json::Value, path: &str, out: &mut Vec<Finding>) {
        match v {
            serde_json::Value::String(s) => {
                // A joined LIST is 3+ comma-separated slug-like segments
                // (no spaces inside any segment) — natural prose with commas
                // (an issue title) has multi-word segments and must not flag.
                // 6+ segments is a list regardless of segment shape: prose
                // rarely chains that many commas, joined titles do.
                let segs: Vec<&str> = s.split(", ").collect();
                if (segs.len() >= 3 && segs.iter().all(|seg| !seg.trim().contains(' ')))
                    || segs.len() >= 6
                {
                    out.push(Finding::new(
                        "gt-joined-list",
                        "skill §6",
                        Severity::Warning,
                        task_id.to_string(),
                        format!(
                            "joined-list string at {path} — a format/order trap for cross-arm grading; emit per-item scalars (item1, item2, …) in the terminal reduce"
                        ),
                    ));
                }
            }
            serde_json::Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    joined_list(task_id, v, &format!("{path}[{i}]"), out);
                }
            }
            serde_json::Value::Object(o) => {
                for (k, v) in o {
                    joined_list(task_id, v, &format!("{path}/{k}"), out);
                }
            }
            _ => {}
        }
    }
    if graded_by_values {
        joined_list(task_id, gt, "$", out);
    }
    fn walk(task_id: &str, v: &serde_json::Value, path: &str, out: &mut Vec<Finding>) {
        match v {
            serde_json::Value::Bool(_) => out.push(Finding::new(
                "gt-bool",
                "skill §6",
                Severity::Error,
                task_id.to_string(),
                format!(
                    "boolean at {path} — ungradeable by contains_values; emit strings (e.g. \"yes\"/\"no\") in the terminal reduce"
                ),
            )),
            serde_json::Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    walk(task_id, v, &format!("{path}[{i}]"), out);
                }
            }
            serde_json::Value::Object(o) => {
                for (k, v) in o {
                    let normalized: String =
                        k.chars().filter(|c| *c != '_').collect::<String>().to_lowercase();
                    if normalized == "updatedat" || normalized == "pushedat" {
                        out.push(Finding::new(
                            "gt-volatile",
                            "skill §6",
                            Severity::Warning,
                            task_id.to_string(),
                            format!(
                                "volatile timestamp key '{k}' at {path} — the snapshot will drift from the live world (the UPDATED_AT lesson)"
                            ),
                        ));
                    }
                    walk(task_id, v, &format!("{path}/{k}"), out);
                }
            }
            _ => {}
        }
    }
    walk(task_id, gt, "$", out);
}

/// Committed example manifests must satisfy the WrapManifest contract, and the
/// token-bearing generated-manifests dir must stay gitignored.
fn check_manifest_hygiene(root: &Path, out: &mut Vec<Finding>) {
    let examples = root.join("examples").join("manifests");
    if examples.is_dir() {
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&examples)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "json"))
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        for p in paths {
            if let Err(e) = WrapManifest::load(&p) {
                let rel = p.strip_prefix(root).unwrap_or(&p);
                out.push(Finding::new(
                    "manifest-hygiene",
                    "skill §3/§4",
                    Severity::Error,
                    rel.display().to_string(),
                    format!("{e}"),
                ));
            }
        }
    }
    let gitignore = std::fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
    // Accept any pattern that covers the dir, not only the exact line — a
    // broader "bench/artifacts/" ignore is just as safe.
    const COVERING: &[&str] = &[
        "bench/artifacts/manifests/",
        "bench/artifacts/manifests",
        "bench/artifacts/",
        "bench/artifacts",
    ];
    if !gitignore.lines().any(|l| COVERING.contains(&l.trim())) {
        out.push(Finding::new(
            "manifest-hygiene",
            "skill §3/§4",
            Severity::Error,
            ".gitignore".to_string(),
            "missing 'bench/artifacts/manifests/' — generated manifests embed the gh token and must never be committed",
        ));
    }
}

// ---------------------------------------------------------------------- hook

const HOOK_MARKER: &str = "# leaf-bench lint pre-push hook v1";

/// Where git will actually look for hooks: `git rev-parse --git-path hooks`
/// resolves core.hooksPath and worktree layouts; fall back to .git/hooks when
/// git is unavailable or the dir is not a repository (tests use a tempdir).
fn hooks_dir(root: &Path) -> std::path::PathBuf {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", "hooks"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| {
            let p = std::path::PathBuf::from(s);
            if p.is_absolute() { p } else { root.join(p) }
        })
        .unwrap_or_else(|| root.join(".git").join("hooks"))
}

/// `lint --install-hook`: wire the gate into the repo's pre-push hook.
/// Idempotent; refuses to clobber a hook it did not write.
fn install_hook(root: &Path) -> Result<()> {
    let hooks = hooks_dir(root);
    if !hooks.is_dir() {
        bail!(
            "{} is not a directory — not a normal git checkout (bare repo, or a core.hooksPath that does not exist?)",
            hooks.display()
        );
    }
    let hook_path = hooks.join("pre-push");
    if hook_path.exists() {
        let existing = std::fs::read_to_string(&hook_path).unwrap_or_default();
        if !existing.contains(HOOK_MARKER) {
            bail!(
                "refusing to overwrite existing pre-push hook without the leaf-bench marker — remove {} manually first",
                hook_path.display()
            );
        }
    }
    // ${CARGO_TARGET_DIR:-target} keeps the hook working when cargo's output
    // dir is relocated via the env var (a build.target-dir in .cargo/config
    // is not resolvable from sh — set CARGO_TARGET_DIR to match if you use it).
    let body = format!(
        "#!/bin/sh\n{HOOK_MARKER} (installed by `leaf-bench lint --install-hook`)\nset -e\ncd \"$(git rev-parse --show-toplevel)\"\nBIN=\"${{CARGO_TARGET_DIR:-target}}/release/leaf-bench\"\nif [ ! -x \"$BIN\" ] || [ -n \"$(find crates -name '*.rs' -newer \"$BIN\" -print -quit 2>/dev/null)\" ]; then\n  cargo build --release -p leaf-bench >&2\nfi\nexec \"$BIN\" lint\n"
    );
    std::fs::write(&hook_path, body).with_context(|| format!("writing {}", hook_path.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755))?;
    }
    println!("[ok] installed {}", hook_path.display());
    Ok(())
}

// --------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed tree, hermetically: compile-time embed, cwd-independent.
    /// Tree- and def-level checks re-verify the real tree on every test run.
    const COMMITTED_TREE: &str = include_str!("../../../bench/general-tree.json");

    fn tree(json: &str) -> Tree {
        serde_json::from_str(json).expect("test tree parses")
    }

    fn committed_tree() -> Tree {
        tree(COMMITTED_TREE)
    }

    fn def(
        id: &'static str,
        binary: &'static str,
        template: &[&'static str],
        stdin_from: Option<&'static str>,
        determinism: DeterminismClass,
    ) -> LeafDef {
        LeafDef {
            id,
            tool_name: id,
            description: "test def",
            binary,
            args_template: template.to_vec(),
            stdin_from,
            determinism,
            properties: serde_json::json!({}),
            required: vec![],
            needs_gh_env: false,
            timeout_ms: 1_000,
        }
    }

    /// One llm root with a single sequence capability holding the given leaf
    /// args — the smallest tree the checks accept.
    fn one_leaf_tree(leaf_id: &str, args: serde_json::Value) -> Tree {
        tree(&format!(
            r#"{{"name":"t","version":"1","root":{{"name":"root","goal":"route","route":"llm","candidates":[
                {{"node":"branch","name":"cap","goal":"Use when testing.","route":"sequence","candidates":[
                    {{"node":"leaf","name":"step","description":"d","leaf":"{leaf_id}","tool":"{leaf_id}","args":{args}}},
                    {{"node":"done","name":"finish","description":"d","terminal":true}}
                ]}}
            ]}}}}"#
        ))
    }

    fn findings_for<'a>(out: &'a [Finding], check: &str) -> Vec<&'a Finding> {
        out.iter().filter(|f| f.check == check).collect()
    }

    // ---- floats (§2)

    #[test]
    fn floats_flagged_and_committed_tree_clean() {
        let t = one_leaf_tree("jq", serde_json::json!({"expr": ".", "n": 2.5}));
        let mut out = Vec::new();
        check_floats("t", &t, &mut out);
        assert_eq!(findings_for(&out, "floats").len(), 1, "{out:?}");

        let mut out = Vec::new();
        check_floats("t", &committed_tree(), &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    // ---- dash-dash (§3)

    #[test]
    fn dash_dash_flags_unguarded_leading_slot() {
        let defs = vec![def("f", "gh", &["{endpoint}"], None, DeterminismClass::ReplayOnly)];
        let t = one_leaf_tree("f", serde_json::json!({"endpoint": "repos/{input.owner}"}));
        let mut out = Vec::new();
        check_dash_dash("t", &t, &defs, &mut out);
        assert_eq!(findings_for(&out, "dash-dash").len(), 1, "{out:?}");
    }

    #[test]
    fn dash_dash_exemption_matrix() {
        // (def, args, expected finding count)
        let cases: Vec<(LeafDef, serde_json::Value, usize)> = vec![
            // stdin route
            (
                def("s", "jq", &["-c", "{expr}"], Some("input"), DeterminismClass::Deterministic),
                serde_json::json!({"expr": ".", "input": "{input}"}),
                0,
            ),
            // prefix-embedding
            (
                def("p", "gh", &["-f", "query={query}"], None, DeterminismClass::ReplayOnly),
                serde_json::json!({"query": "query {{input.owner}}"}),
                0,
            ),
            // dash-dash guard
            (
                def("g", "gh", &["api", "--", "{endpoint}"], None, DeterminismClass::ReplayOnly),
                serde_json::json!({"endpoint": "repos/{input.owner}"}),
                0,
            ),
            // trusted authored literal — no task-derived placeholder at all
            (
                def("t", "jq", &["-c", "{expr}"], Some("input"), DeterminismClass::Deterministic),
                serde_json::json!({"expr": "{title: .title}", "input": "{source}"}),
                0,
            ),
            // {source} into a NON-jq binary, unguarded leading slot -> error
            (
                def("c", "curl", &["{url}"], None, DeterminismClass::ReplayOnly),
                serde_json::json!({"url": "{source}"}),
                1,
            ),
            // arg carries task data but the def has no slot or stdin for it
            (
                def("n", "gh", &["api"], None, DeterminismClass::ReplayOnly),
                serde_json::json!({"endpoint": "repos/{input.owner}"}),
                1,
            ),
        ];
        for (d, args, expect) in cases {
            let t = one_leaf_tree(d.id, args);
            let mut out = Vec::new();
            check_dash_dash("t", &t, &[d], &mut out);
            assert_eq!(findings_for(&out, "dash-dash").len(), expect, "{out:?}");
        }
    }

    #[test]
    fn dash_dash_committed_repo_green() {
        let mut out = Vec::new();
        check_dash_dash("t", &committed_tree(), &leaves::defs(), &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn dash_dash_checks_every_placeholder_occurrence() {
        // first occurrence is prefix-embedded (safe), second is a bare
        // leading slot with no `--` — must still flag
        let d = def("multi", "gh", &["-f", "query={q}", "{q}"], None, DeterminismClass::ReplayOnly);
        let t = one_leaf_tree("multi", serde_json::json!({"q": "{input.x}"}));
        let mut out = Vec::new();
        check_dash_dash("t", &t, &[d], &mut out);
        assert_eq!(findings_for(&out, "dash-dash").len(), 1, "{out:?}");
    }

    // ---- source-reduce (§5)

    #[test]
    fn source_reduce_flags_last_output() {
        let defs = leaves::defs();
        let bad = one_leaf_tree("jq", serde_json::json!({"expr": ".", "input": "{last_output}"}));
        let mut out = Vec::new();
        check_source_reduce("t", &bad, &defs, &mut out);
        assert_eq!(findings_for(&out, "source-reduce").len(), 1, "{out:?}");

        let good = one_leaf_tree("jq", serde_json::json!({"expr": ".", "input": "{source}"}));
        let mut out = Vec::new();
        check_source_reduce("t", &good, &defs, &mut out);
        assert!(out.is_empty(), "{out:?}");

        // keyed on the def, not the literal arg name: a non-jq leaf with an
        // arg named "input" is not a reduce and must not flag
        let non_jq = one_leaf_tree("curl", serde_json::json!({"url": "x", "input": "{last_output}"}));
        let mut out = Vec::new();
        check_source_reduce("t", &non_jq, &defs, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    // ---- terminal-done (§7)

    #[test]
    fn terminal_done_matrix() {
        // (a) sequence ending in a leaf
        let ends_in_leaf = tree(
            r#"{"name":"t","version":"1","root":{"name":"root","goal":"g","route":"llm","candidates":[
                {"node":"branch","name":"cap","goal":"Use when x.","route":"sequence","candidates":[
                    {"node":"leaf","name":"l","description":"d","leaf":"jq","tool":"jq","args":{"expr":"."}}
                ]}
            ]}}"#,
        );
        let mut out = Vec::new();
        check_terminal_done("t", &ends_in_leaf, &mut out);
        assert_eq!(findings_for(&out, "terminal-done").len(), 1, "{out:?}");

        // (b) mid-sequence done{terminal:false}
        let mid_false = tree(
            r#"{"name":"t","version":"1","root":{"name":"root","goal":"g","route":"llm","candidates":[
                {"node":"branch","name":"cap","goal":"Use when x.","route":"sequence","candidates":[
                    {"node":"done","name":"oops","description":"d","terminal":false},
                    {"node":"done","name":"finish","description":"d","terminal":true}
                ]}
            ]}}"#,
        );
        let mut out = Vec::new();
        check_terminal_done("t", &mid_false, &mut out);
        assert_eq!(findings_for(&out, "terminal-done").len(), 1, "{out:?}");

        // (c) empty sequence
        let empty = tree(
            r#"{"name":"t","version":"1","root":{"name":"root","goal":"g","route":"llm","candidates":[
                {"node":"branch","name":"cap","goal":"Use when x.","route":"sequence","candidates":[]}
            ]}}"#,
        );
        let mut out = Vec::new();
        check_terminal_done("t", &empty, &mut out);
        assert_eq!(findings_for(&out, "terminal-done").len(), 1, "{out:?}");

        // (d) empty llm branch — a dead end no other check or the scripted
        // walk would visit; only a live route/exploration pick hits it
        let empty_llm = tree(
            r#"{"name":"t","version":"1","root":{"name":"root","goal":"g","route":"llm","candidates":[
                {"node":"branch","name":"escalate","goal":"Use when nothing fits.","route":"llm","candidates":[]}
            ]}}"#,
        );
        let mut out = Vec::new();
        check_terminal_done("t", &empty_llm, &mut out);
        assert_eq!(findings_for(&out, "terminal-done").len(), 1, "{out:?}");

        // (e) committed tree: hybrid allowance + root llm done{terminal:false}
        // must both stay clean.
        let mut out = Vec::new();
        check_terminal_done("t", &committed_tree(), &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    // ---- goals (§8)

    #[test]
    fn goal_matrix() {
        let dupes_and_empty = tree(
            r#"{"name":"t","version":"1","root":{"name":"root","goal":"g","route":"llm","candidates":[
                {"node":"branch","name":"a","goal":"Use when x.","route":"sequence","candidates":[
                    {"node":"done","name":"f","description":"d","terminal":true}]},
                {"node":"branch","name":"b","goal":"Use when x.","route":"sequence","candidates":[
                    {"node":"done","name":"f","description":"d","terminal":true}]},
                {"node":"branch","name":"c","goal":"  ","route":"sequence","candidates":[
                    {"node":"done","name":"f","description":"d","terminal":true}]}
            ]}}"#,
        );
        let mut out = Vec::new();
        check_goals("t", &dupes_and_empty, &mut out);
        let errors: Vec<_> =
            findings_for(&out, "goal-dupes").into_iter().filter(|f| f.severity == Severity::Error).collect();
        // one duplicate (b vs a) + one empty (c)
        assert_eq!(errors.len(), 2, "{out:?}");
        // c also gets the "Use when" warning; nested arms would not (committed
        // tree has unprefixed nested arms and must stay warning-free).
        let mut out = Vec::new();
        check_goals("t", &committed_tree(), &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn root_name_warning() {
        let renamed = tree(
            r#"{"name":"t","version":"1","root":{"name":"trunk","goal":"g","route":"llm","candidates":[
                {"node":"branch","name":"cap","goal":"Use when x.","route":"sequence","candidates":[
                    {"node":"done","name":"f","description":"d","terminal":true}]}
            ]}}"#,
        );
        let mut out = Vec::new();
        check_root_name("t", &renamed, &mut out);
        let hits = findings_for(&out, "routing-label");
        assert_eq!(hits.len(), 1, "{out:?}");
        assert_eq!(hits[0].severity, Severity::Warning);

        let mut out = Vec::new();
        check_root_name("t", &committed_tree(), &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn tree_file_escape_flagged() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("repo");
        let tasks = root.join("bench/tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        // a valid tree OUTSIDE the repo root
        std::fs::write(base.path().join("outside-tree.json"), COMMITTED_TREE).unwrap();
        std::fs::write(
            tasks.join("escapee.toml"),
            r#"id = "escapee"
category = "c"
prompt = "p"
max_llm_calls = 12
timeout_secs = 300
leaves = ["gh_api", "gh_graphql", "jq"]
tree_file = "../../../outside-tree.json"
reference_route = [0]
expected_capability = 0

[checker]
type = "contains_values"
"#,
        )
        .unwrap();
        let (tasks_loaded, findings) = load_tasks(&tasks, &root);
        assert_eq!(tasks_loaded.len(), 1);
        let hits = findings_for(&findings, "task-load");
        assert_eq!(hits.len(), 1, "{findings:?}");
        assert!(hits[0].message.contains("escapes"), "{findings:?}");
    }

    // ---- routing-label (§9)

    #[test]
    fn routing_label_matrix() {
        let t = committed_tree();
        let base = |expected: Option<usize>| TaskManifest {
            id: "x".into(),
            category: "c".into(),
            prompt: "p".into(),
            max_llm_calls: 12,
            timeout_secs: 300,
            leaves: vec!["gh_api".into(), "gh_graphql".into(), "jq".into()],
            reference_route: vec![0],
            tree: t.clone(),
            checker: crate::tasks::CheckerSpec::ContainsValues,
            grading_hint: None,
            input: Default::default(),
            expected_capability: expected,
        };
        let cases = [
            (None, Severity::Error),
            (Some(99), Severity::Error),
            (Some(9), Severity::Warning), // the trailing done — legal at runtime, suspicious as a label
        ];
        for (expected, severity) in cases {
            let mut out = Vec::new();
            check_routing_label(&base(expected), &mut out);
            let hits = findings_for(&out, "routing-label");
            assert_eq!(hits.len(), 1, "{expected:?}: {out:?}");
            assert_eq!(hits[0].severity, severity, "{expected:?}");
        }
        let mut out = Vec::new();
        check_routing_label(&base(Some(0)), &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    // ---- route-shape (§9)

    #[test]
    fn route_shape_walk_on_committed_tree() {
        let t = committed_tree();
        let defs = leaves::defs();
        let all = ["gh_api".to_string(), "gh_graphql".to_string(), "jq".to_string()];
        let shape = |route: &[usize], expected: Option<usize>, leaves_decl: &[String]| {
            let mut out = Vec::new();
            check_route_shape("x", &t, route, expected, leaves_decl, 12, &defs, &mut out);
            out
        };

        // forced pipeline + hybrid, as committed
        assert!(shape(&[0], Some(0), &all).is_empty());
        assert!(shape(&[7, 1], Some(7), &all).is_empty());
        // exhausted at the second llm decision
        assert_eq!(shape(&[7], Some(7), &all).len(), 1);
        // leftover entry
        assert_eq!(shape(&[7, 1, 0], Some(7), &all).len(), 1);
        // out-of-range pick (label agrees, so only the walk flags; the tree
        // has 10 root candidates — 9 capabilities + the trailing done)
        assert_eq!(shape(&[10], Some(10), &all).len(), 1);
        // label disagreement (route still replays, so exactly one finding)
        assert_eq!(shape(&[0], Some(1), &all).len(), 1);
        // coverage: jq missing from task.leaves
        let partial = ["gh_api".to_string(), "gh_graphql".to_string()];
        let out = shape(&[0], Some(0), &partial);
        assert!(out.iter().any(|f| f.message.contains("jq")), "{out:?}");

        // a route needing more llm decisions than max_llm_calls
        let mut out = Vec::new();
        check_route_shape("x", &t, &[7, 1], Some(7), &all, 1, &defs, &mut out);
        let hits = findings_for(&out, "route-shape");
        assert_eq!(hits.len(), 1, "{out:?}");
        assert!(hits[0].message.contains("MaxLlmCalls"), "{out:?}");
    }

    #[test]
    fn route_shape_flags_unknown_leaf_ids() {
        let t = one_leaf_tree("nope", serde_json::json!({"expr": "."}));
        let mut out = Vec::new();
        check_route_shape("x", &t, &[0], Some(0), &["nope".to_string()], 12, &leaves::defs(), &mut out);
        // unknown in tree + unknown in task.leaves
        assert_eq!(findings_for(&out, "route-shape").len(), 2, "{out:?}");
    }

    #[test]
    fn route_shape_flags_tool_mismatch() {
        // one_leaf_tree sets tool = leaf id, but gh_api's registered tool is
        // "api" — the mismatch would kill every call with 'unknown tool'
        let t = one_leaf_tree("gh_api", serde_json::json!({"endpoint": "repos/x/y"}));
        let mut out = Vec::new();
        check_route_shape("x", &t, &[0], Some(0), &["gh_api".to_string()], 12, &leaves::defs(), &mut out);
        let hits = findings_for(&out, "route-shape");
        assert_eq!(hits.len(), 1, "{out:?}");
        assert!(hits[0].message.contains("unknown tool"), "{out:?}");
    }

    #[test]
    fn route_shape_enforces_executor_max_steps() {
        // 32 leaves in one sequence: 1 root decision + 32 leaf steps > the
        // executor's max_steps (32) — must lint red, not fail live later.
        let leaves_json: String = (0..32)
            .map(|i| format!(
                r#"{{"node":"leaf","name":"l{i}","description":"d","leaf":"jq","tool":"jq","args":{{"expr":".","input":"{{source}}"}}}},"#
            ))
            .collect();
        let t = tree(&format!(
            r#"{{"name":"t","version":"1","root":{{"name":"root","goal":"g","route":"llm","candidates":[
                {{"node":"branch","name":"cap","goal":"Use when x.","route":"sequence","candidates":[
                    {leaves_json}
                    {{"node":"done","name":"finish","description":"d","terminal":true}}
                ]}}
            ]}}}}"#
        ));
        let all = ["jq".to_string()];
        let mut out = Vec::new();
        check_route_shape("x", &t, &[0], Some(0), &all, 12, &leaves::defs(), &mut out);
        let hits = findings_for(&out, "route-shape");
        assert_eq!(hits.len(), 1, "{out:?}");
        assert!(hits[0].message.contains("max_steps"), "{out:?}");
    }

    // ---- read-only (§10) + determinism-class (§4)

    #[test]
    fn read_only_flags_mutating_defs_and_graphql_mutations() {
        let bad = vec![def("post", "curl", &["--data", "{body}"], None, DeterminismClass::ReplayOnly)];
        let mut out = Vec::new();
        check_defs_read_only(&bad, &mut out);
        assert_eq!(findings_for(&out, "read-only").len(), 1, "{out:?}");

        // gh api -f forces POST on a REST endpoint (implicit mutation)…
        let rest_field =
            vec![def("gh_post", "gh", &["api", "-f", "body={b}"], None, DeterminismClass::ReplayOnly)];
        let mut out = Vec::new();
        check_defs_read_only(&rest_field, &mut out);
        assert_eq!(findings_for(&out, "read-only").len(), 1, "{out:?}");

        // …but `gh api graphql -f query=…` is the read path — exempt
        let gql = vec![def(
            "gql",
            "gh",
            &["api", "graphql", "-f", "query={q}"],
            None,
            DeterminismClass::ReplayOnly,
        )];
        let mut out = Vec::new();
        check_defs_read_only(&gql, &mut out);
        assert!(out.is_empty(), "{out:?}");

        let defs = leaves::defs();
        let t = one_leaf_tree("gh_graphql", serde_json::json!({"query": "mutation { addStar }"}));
        let mut out = Vec::new();
        check_tree_read_only("t", &t, &defs, &mut out);
        assert_eq!(findings_for(&out, "read-only").len(), 1, "{out:?}");

        // the word "mutation" in a jq expr must NOT flag (scope test)
        let t = one_leaf_tree("jq", serde_json::json!({"expr": ".mutation", "input": "{source}"}));
        let mut out = Vec::new();
        check_tree_read_only("t", &t, &defs, &mut out);
        assert!(out.is_empty(), "{out:?}");

        // committed defs + committed tree: green
        let mut out = Vec::new();
        check_defs_read_only(&defs, &mut out);
        check_tree_read_only("t", &committed_tree(), &defs, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn determinism_class_heuristic() {
        let bad = vec![
            def("net", "gh", &["api"], None, DeterminismClass::Deterministic),
            def("pure", "jq", &["-c", "{expr}"], Some("input"), DeterminismClass::ReplayOnly),
        ];
        let mut out = Vec::new();
        check_determinism_class(&bad, &mut out);
        let hits = findings_for(&out, "determinism-class");
        assert_eq!(hits.len(), 2, "{out:?}");
        assert!(hits.iter().any(|f| f.severity == Severity::Error && f.target == "net"));
        assert!(hits.iter().any(|f| f.severity == Severity::Warning && f.target == "pure"));

        let mut out = Vec::new();
        check_determinism_class(&leaves::defs(), &mut out);
        assert!(out.is_empty(), "{out:?}");

        // an unclassified binary gets an informational note, never counted
        let unknown = vec![def("docker", "docker", &["ps"], None, DeterminismClass::ReplayOnly)];
        let mut out = Vec::new();
        check_determinism_class(&unknown, &mut out);
        let hits = findings_for(&out, "determinism-class");
        assert_eq!(hits.len(), 1, "{out:?}");
        assert_eq!(hits[0].severity, Severity::Info);
    }

    // ---- ground truth (§6)

    #[test]
    fn gt_bool_and_volatile() {
        let mut out = Vec::new();
        check_gt("x", &serde_json::json!({"merged": true}), true, &mut out);
        assert_eq!(findings_for(&out, "gt-bool").len(), 1, "{out:?}");

        let mut out = Vec::new();
        check_gt("x", &serde_json::json!({"merged": "yes", "ci": "green"}), true, &mut out);
        assert!(out.is_empty(), "{out:?}");

        // nested bool carries a path
        let mut out = Vec::new();
        check_gt("x", &serde_json::json!({"a": [{"b": false}]}), true, &mut out);
        let hits = findings_for(&out, "gt-bool");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].message.contains("$/a[0]/b"), "{out:?}");

        // volatile keys: updated_at / updatedAt / pushed_at flag, created_at does not
        for key in ["updated_at", "updatedAt", "pushed_at"] {
            let mut out = Vec::new();
            check_gt("x", &serde_json::json!({key: "2026-01-01"}), true, &mut out);
            assert_eq!(findings_for(&out, "gt-volatile").len(), 1, "{key}: {out:?}");
        }
        let mut out = Vec::new();
        check_gt("x", &serde_json::json!({"created_at": "2026-01-01"}), true, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn gt_joined_list_and_error_surface() {
        // joined-list string (3+ segments) under contains_values -> warning
        let mut out = Vec::new();
        check_gt("x", &serde_json::json!({"topics": "ai, assistant, molty"}), true, &mut out);
        assert_eq!(findings_for(&out, "gt-joined-list").len(), 1, "{out:?}");

        // two segments (a title with one comma) is not a list — no finding
        let mut out = Vec::new();
        check_gt("x", &serde_json::json!({"t": "fix bugs, add tests"}), true, &mut out);
        assert!(out.is_empty(), "{out:?}");

        // natural prose with 2+ commas (multi-word segments) is not a list —
        // the issue_triage title false-positive class
        let mut out = Vec::new();
        check_gt(
            "x",
            &serde_json::json!({"title": "Settings: single page with auth state, defaults, and per-provider fields"}),
            true,
            &mut out,
        );
        assert!(out.is_empty(), "{out:?}");

        // 6+ segments is a list even with multi-word items (joined titles)
        let mut out = Vec::new();
        check_gt(
            "x",
            &serde_json::json!({"titles": "fix one thing, fix another, add a test, update docs, bump version, cut release"}),
            true,
            &mut out,
        );
        assert_eq!(findings_for(&out, "gt-joined-list").len(), 1, "{out:?}");

        // raw error surface as bare-string GT under contains_values -> warning
        let mut out = Vec::new();
        check_gt("x", &serde_json::json!("exit 1:\ngh: Not Found (HTTP 404)\n"), true, &mut out);
        assert_eq!(findings_for(&out, "gt-error-surface").len(), 1, "{out:?}");

        // a regex-checked task's GT values are not assertions — both skipped
        let mut out = Vec::new();
        check_gt("x", &serde_json::json!({"topics": "ai, assistant, molty"}), false, &mut out);
        check_gt("x", &serde_json::json!("exit 1:\ngh: Not Found (HTTP 404)\n"), false, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    // ---- manifest hygiene

    #[test]
    fn manifest_hygiene_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("examples/manifests")).unwrap();

        // invalid: binary not in its own allowlist
        std::fs::write(
            root.join("examples/manifests/bad.json"),
            serde_json::json!({
                "name": "bad", "description": "d", "determinism": "replay_only",
                "binary": "/usr/bin/x", "allowlist": ["/usr/bin/other"],
                "args_template": [], "stdin_from": null, "timeout_ms": 1000,
                "workdir": "/tmp", "env": {},
                "input_schema": {"type": "object", "properties": {}, "required": []}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();

        let mut out = Vec::new();
        check_manifest_hygiene(root, &mut out);
        let hits = findings_for(&out, "manifest-hygiene");
        assert_eq!(hits.len(), 2, "{out:?}"); // bad manifest + missing gitignore line

        std::fs::remove_file(root.join("examples/manifests/bad.json")).unwrap();
        std::fs::write(root.join(".gitignore"), "bench/artifacts/manifests/\n").unwrap();
        let mut out = Vec::new();
        check_manifest_hygiene(root, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    // ---- flags + hook

    #[test]
    fn parse_flags_matrix() {
        let ok = parse_flags(&["lint".into(), "--task".into(), "view_pr".into(), "--strict".into()])
            .unwrap();
        assert_eq!(ok.task.as_deref(), Some("view_pr"));
        assert!(ok.strict);

        let err = parse_flags(&["lint".into(), "--frobnicate".into()]).unwrap_err();
        assert!(err.contains("--task") && err.contains("--strict") && err.contains("--install-hook"));

        assert!(parse_flags(&["lint".into(), "--task".into()]).is_err());
        assert!(
            parse_flags(&["lint".into(), "--install-hook".into(), "--strict".into()]).is_err()
        );
    }

    #[test]
    fn install_hook_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // no .git/hooks -> refuse
        assert!(install_hook(root).is_err());

        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        install_hook(root).unwrap();
        let hook = root.join(".git/hooks/pre-push");
        let body = std::fs::read_to_string(&hook).unwrap();
        assert!(body.contains(HOOK_MARKER));
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777, 0o755);
        }

        // idempotent re-install
        install_hook(root).unwrap();

        // foreign hook -> refuse
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").unwrap();
        let err = install_hook(root).unwrap_err();
        assert!(err.to_string().contains("refusing"), "{err}");
    }
}

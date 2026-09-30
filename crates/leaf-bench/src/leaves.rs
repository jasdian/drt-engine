//! Leaf-manifest generation: binaries are resolved from PATH at setup time
//! (paths differ per machine — nix store, homebrew, apt), manifests land in
//! bench/artifacts/manifests/, and the returned registry maps leaf ids to
//! LeafSpecs pointing at the sibling leaf-wrap binary.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use leaf_traits::{DeterminismClass, LeafSpec};

pub fn find_bin(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Write a file with 0o600 perms (Unix — the documented target env). `.mode()`
/// only applies on creation, so set_permissions covers a pre-existing 0644.
fn write_owner_only(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// leaf-wrap sits next to the leaf-bench binary in target/<profile>/.
pub fn leaf_wrap_bin() -> Result<PathBuf> {
    let me = std::env::current_exe().context("current_exe")?;
    let dir = me.parent().context("exe has a parent")?;
    let candidate = dir.join("leaf-wrap");
    if candidate.is_file() {
        Ok(candidate)
    } else {
        bail!(
            "leaf-wrap binary not found at {} — build it with `cargo build -p leaf-mcp`",
            candidate.display()
        )
    }
}

pub struct GeneratedLeaves {
    pub specs: BTreeMap<String, LeafSpec>,
    pub server_allowlist: Vec<PathBuf>,
}

// pub(crate) so `lint` can statically check the registry (dash-dash slots,
// determinism classes, mutating flags) without going through generate().
pub(crate) struct LeafDef {
    pub(crate) id: &'static str,
    pub(crate) tool_name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) binary: &'static str,
    pub(crate) args_template: Vec<&'static str>,
    pub(crate) stdin_from: Option<&'static str>,
    pub(crate) determinism: DeterminismClass,
    pub(crate) properties: serde_json::Value,
    pub(crate) required: Vec<&'static str>,
    pub(crate) needs_gh_env: bool,
    pub(crate) timeout_ms: u64,
}

pub(crate) fn defs() -> Vec<LeafDef> {
    use DeterminismClass::*;
    vec![
        LeafDef {
            id: "jq",
            tool_name: "jq",
            description: "Run a jq filter (compact output) over a JSON document supplied on stdin.",
            binary: "jq",
            args_template: vec!["-c", "{expr}"],
            stdin_from: Some("input"),
            determinism: Deterministic,
            properties: serde_json::json!({
                "expr": {"type": "string", "description": "jq program"},
                "input": {"type": "string", "description": "JSON document for stdin"}
            }),
            required: vec!["expr", "input"],
            needs_gh_env: false,
            timeout_ms: 10_000,
        },
        LeafDef {
            id: "jq_raw",
            tool_name: "jq",
            description: "Run a jq filter with RAW output (no quotes, no trailing newline) — for values that feed another leaf's endpoint/query.",
            binary: "jq",
            args_template: vec!["-j", "{expr}"],
            stdin_from: Some("input"),
            determinism: Deterministic,
            properties: serde_json::json!({
                "expr": {"type": "string", "description": "jq program"},
                "input": {"type": "string", "description": "JSON document for stdin"}
            }),
            required: vec!["expr", "input"],
            needs_gh_env: false,
            timeout_ms: 10_000,
        },
        LeafDef {
            id: "gh_api",
            tool_name: "api",
            description: "GitHub REST via `gh api <endpoint>` (read-only benchmark usage).",
            binary: "gh",
            // `--` end-of-options: an untrusted endpoint can't be parsed as a flag.
            args_template: vec!["api", "--", "{endpoint}"],
            stdin_from: None,
            determinism: ReplayOnly,
            properties: serde_json::json!({
                "endpoint": {"type": "string", "description": "REST endpoint, may carry query params"}
            }),
            required: vec!["endpoint"],
            needs_gh_env: true,
            timeout_ms: 30_000,
        },
        LeafDef {
            id: "gh_graphql",
            tool_name: "graphql",
            description: "GitHub GraphQL via `gh api graphql -f query=<query>`.",
            binary: "gh",
            args_template: vec!["api", "graphql", "-f", "query={query}"],
            stdin_from: None,
            determinism: ReplayOnly,
            properties: serde_json::json!({
                "query": {"type": "string", "description": "GraphQL query"}
            }),
            required: vec!["query"],
            needs_gh_env: true,
            timeout_ms: 30_000,
        },
        LeafDef {
            id: "curl",
            tool_name: "fetch",
            description: "HTTP GET via `curl -s <url>`.",
            binary: "curl",
            // `--` end-of-options: an untrusted url can't be parsed as a flag.
            args_template: vec!["-s", "--max-time", "25", "--", "{url}"],
            stdin_from: None,
            determinism: ReplayOnly,
            properties: serde_json::json!({
                "url": {"type": "string"}
            }),
            required: vec!["url"],
            needs_gh_env: false,
            timeout_ms: 30_000,
        },
        LeafDef {
            id: "sha256sum",
            tool_name: "sha256sum",
            description: "SHA-256 of a file path inside the jail.",
            binary: "sha256sum",
            args_template: vec!["{path}"],
            stdin_from: None,
            determinism: Deterministic,
            properties: serde_json::json!({
                "path": {"type": "string", "description": "path relative to the jail"}
            }),
            required: vec!["path"],
            needs_gh_env: false,
            timeout_ms: 10_000,
        },
    ]
}

/// gh auth for the wrapped `gh` (children run with a scrubbed env): export
/// the token once at generation time. It lands in the manifest's `env`,
/// which the `__leaf_manifest` tool REDACTS.
fn gh_env() -> Result<BTreeMap<String, String>> {
    let out = std::process::Command::new(find_bin("gh").context("gh not on PATH")?)
        .args(["auth", "token"])
        .output()
        .context("running gh auth token")?;
    if !out.status.success() {
        bail!("gh auth token failed — run `gh auth login` first");
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let mut env = BTreeMap::new();
    env.insert("GH_TOKEN".to_string(), token);
    // gh wants a HOME for config/state even with GH_TOKEN auth.
    if let Ok(home) = std::env::var("HOME") {
        env.insert("HOME".to_string(), home);
    }
    env.insert("GH_NO_UPDATE_NOTIFIER".to_string(), "1".to_string());
    env.insert("GH_PROMPT_DISABLED".to_string(), "1".to_string());
    Ok(env)
}

pub fn generate(paths: &crate::config::BenchPaths, needed: &[&str]) -> Result<GeneratedLeaves> {
    let wrap = leaf_wrap_bin()?;
    let mut specs = BTreeMap::new();
    let gh_environment = if needed.iter().any(|n| n.starts_with("gh_")) {
        Some(gh_env()?)
    } else {
        None
    };

    for def in defs() {
        if !needed.contains(&def.id) {
            continue;
        }
        let binary = find_bin(def.binary).with_context(|| {
            format!("`{}` not on PATH — required by leaf {}", def.binary, def.id)
        })?;
        let manifest = serde_json::json!({
            "name": def.tool_name,
            "description": def.description,
            "determinism": def.determinism,
            "binary": binary,
            "allowlist": [binary],
            "args_template": def.args_template,
            "stdin_from": def.stdin_from,
            "timeout_ms": def.timeout_ms,
            "workdir": paths.jail,
            "env": if def.needs_gh_env {
                serde_json::to_value(gh_environment.as_ref().unwrap()).unwrap()
            } else {
                serde_json::json!({})
            },
            "input_schema": {
                "type": "object",
                "properties": def.properties,
                "required": def.required,
                "additionalProperties": false
            }
        });
        let manifest_path = paths.manifests.join(format!("{}.json", def.id));
        // Manifests can carry the gh token in `env` — write owner-only, and
        // fix perms on a pre-existing file from an earlier run.
        write_owner_only(
            &manifest_path,
            serde_json::to_string_pretty(&manifest)?.as_bytes(),
        )?;

        specs.insert(
            def.id.to_string(),
            LeafSpec {
                id: def.id.to_string(),
                command: wrap.clone(),
                args: vec![
                    "--manifest".to_string(),
                    manifest_path.to_string_lossy().into_owned(),
                ],
                env: BTreeMap::new(),
                cwd: None,
                call_timeout_ms: def.timeout_ms + 5_000,
                expected_determinism: def.determinism,
            },
        );
    }
    Ok(GeneratedLeaves {
        specs,
        server_allowlist: vec![wrap],
    })
}

/// Which binaries the given leaf ids need — for preflight reporting.
pub fn required_binaries(needed: &[&str]) -> Vec<&'static str> {
    let mut bins: Vec<&'static str> = defs()
        .iter()
        .filter(|d| needed.contains(&d.id))
        .map(|d| d.binary)
        .collect();
    bins.sort_unstable();
    bins.dedup();
    bins
}

/// The path check used at generation time, exposed for preflight.
pub fn missing_binaries(needed: &[&str]) -> Vec<&'static str> {
    required_binaries(needed)
        .into_iter()
        .filter(|b| find_bin(b).is_none())
        .collect()
}

//! Subprocess sandbox for wrapped CLI binaries.
//!
//! ## What this guarantees
//! - Only the allowlisted, canonicalized binary is executed (symlinks resolved).
//! - The child inherits ZERO ambient environment (`env_clear` + explicit
//!   allowlist): no HOME, no PATH, no API keys.
//! - Its working directory is the jail directory.
//! - It cannot outlive `timeout_ms` or the parent (`kill_on_drop`).
//! - Arguments are passed as argv array entries — never through a shell, so
//!   *shell* injection via tool args is structurally impossible.
//!
//! Note: argv-array passing does NOT stop OPTION injection — an untrusted
//! value that begins with `-` is still parsed as a flag by the wrapped tool.
//! The defense for that subclass is a manifest that places every untrusted
//! positional after a `--` end-of-options separator (see the `gh_api`/`curl`
//! templates in leaf-bench).
//!
//! ## What this honestly does NOT do
//! This is policy sandboxing for a COOPERATIVE binary, not containment of a
//! hostile one: no seccomp, no namespaces, no cgroups, no rlimits. The binary
//! itself can still read /etc/passwd, open sockets, or fork-bomb. For the
//! hobbyist threat model — prompt-injected *arguments* to a *trusted* tool —
//! argv-array passing plus fenced prompt rendering is the actual defense.
//! Upgrade path if that ever changes: unshare(1), systemd-run --scope, or
//! Landlock.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use crate::manifest::WrapManifest;

#[derive(Debug)]
pub struct SandboxOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("binary not allowlisted: {0}")]
    NotAllowlisted(PathBuf),
    #[error("sandbox io: {0}")]
    Io(#[from] std::io::Error),
    #[error("wrapped binary timed out after {0} ms")]
    Timeout(u64),
}

/// Canonicalize `binary` and require it to match a canonicalized allowlist
/// entry exactly.
pub fn check_allowlisted(
    binary: &std::path::Path,
    allowlist: &[PathBuf],
) -> Result<PathBuf, SandboxError> {
    let canonical = std::fs::canonicalize(binary)
        .map_err(|_| SandboxError::NotAllowlisted(binary.to_path_buf()))?;
    let ok = allowlist
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .any(|p| p == canonical);
    if ok {
        Ok(canonical)
    } else {
        Err(SandboxError::NotAllowlisted(canonical))
    }
}

pub async fn run_sandboxed(
    m: &WrapManifest,
    argv: Vec<String>,
    stdin_data: Option<&str>,
) -> Result<SandboxOutput, SandboxError> {
    check_allowlisted(&m.binary, &m.allowlist)?;
    std::fs::create_dir_all(&m.workdir)?;

    // The allowlist CHECK canonicalizes (symlinks resolved, exact match), but
    // execution uses the manifest path: multi-call binaries (coreutils,
    // busybox) dispatch on argv[0], so spawning the canonical target would
    // change the tool's personality.
    let mut child = tokio::process::Command::new(&m.binary)
        .args(&argv)
        .env_clear()
        .envs(&m.env)
        .current_dir(&m.workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            // A closed pipe (tool exited early) is the tool's business, not a
            // sandbox failure — the exit code will say what happened.
            let _ = stdin.write_all(data.as_bytes()).await;
        }
    } else {
        drop(child.stdin.take());
    }

    match tokio::time::timeout(
        Duration::from_millis(m.timeout_ms),
        child.wait_with_output(),
    )
    .await
    {
        Ok(out) => {
            let out = out?;
            Ok(SandboxOutput {
                // Leaves are text tools; binary-output tools are out of scope.
                // Non-UTF-8 output is lossy-converted exactly once, here.
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                exit_code: out.status.code().unwrap_or(-1),
            })
        }
        Err(_) => Err(SandboxError::Timeout(m.timeout_ms)), // kill_on_drop reaps it
    }
}

#[cfg(test)]
pub(crate) fn find_bin(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use leaf_traits::DeterminismClass;
    use std::collections::BTreeMap;

    fn manifest_for(
        bin: &std::path::Path,
        allow: &std::path::Path,
        dir: &std::path::Path,
    ) -> WrapManifest {
        WrapManifest {
            name: "t".into(),
            description: "d".into(),
            determinism: DeterminismClass::Deterministic,
            binary: bin.to_path_buf(),
            allowlist: vec![allow.to_path_buf()],
            args_template: vec![],
            stdin_from: None,
            timeout_ms: 5_000,
            workdir: dir.to_path_buf(),
            env: BTreeMap::new(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[tokio::test]
    async fn refuses_non_allowlisted_binary() {
        let dir = tempfile::tempdir().unwrap();
        let jq = find_bin("jq").expect("test needs jq on PATH — install jq");
        let cat = find_bin("cat").expect("test needs cat on PATH");
        // Manifest says run `cat`, but only jq is allowlisted.
        let m = manifest_for(&cat, &jq, dir.path());
        let err = run_sandboxed(&m, vec![], None).await.unwrap_err();
        assert!(matches!(err, SandboxError::NotAllowlisted(_)));
    }

    #[tokio::test]
    async fn timeout_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let sleep = find_bin("sleep").expect("test needs sleep on PATH");
        let mut m = manifest_for(&sleep, &sleep, dir.path());
        m.timeout_ms = 200;
        let start = std::time::Instant::now();
        let err = run_sandboxed(&m, vec!["30".into()], None)
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Timeout(200)));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn env_is_scrubbed_to_the_explicit_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let env_bin = find_bin("env").expect("test needs env on PATH");
        let mut m = manifest_for(&env_bin, &env_bin, dir.path());
        m.env.insert("LEAF_MARKER".into(), "present".into());
        let out = run_sandboxed(&m, vec![], None).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.stdout.contains("LEAF_MARKER=present"));
        assert!(
            !out.stdout.contains("HOME="),
            "ambient HOME leaked: {}",
            out.stdout
        );
        assert!(
            !out.stdout.contains("PATH="),
            "ambient PATH leaked: {}",
            out.stdout
        );
    }

    #[tokio::test]
    async fn stdin_and_argv_reach_the_tool() {
        let dir = tempfile::tempdir().unwrap();
        let jq = find_bin("jq").expect("test needs jq on PATH — install jq");
        let m = manifest_for(&jq, &jq, dir.path());
        let out = run_sandboxed(&m, vec!["-c".into(), ".a".into()], Some("{\"a\":1}"))
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "1\n");
    }

    #[tokio::test]
    async fn workdir_jail_is_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let pwd = find_bin("pwd").expect("test needs pwd on PATH");
        let m = manifest_for(&pwd, &pwd, dir.path());
        let out = run_sandboxed(&m, vec![], None).await.unwrap();
        let reported = std::path::Path::new(out.stdout.trim());
        assert_eq!(
            std::fs::canonicalize(reported).unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
    }
}

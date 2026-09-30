//! Shared helpers for leaf-mcp integration tests.
//!
//! Each integration-test binary compiles this module independently, so a
//! helper used by only some of them looks "dead" to the others.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Resolve a binary from PATH (returns the PATH entry, not the canonical
/// target — multi-call binaries dispatch on argv[0]).
pub fn find_bin(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(name))
                .find(|p| p.is_file())
        })
        .unwrap_or_else(|| panic!("integration tests need `{name}` on PATH — install it"))
}

pub fn leaf_wrap_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_leaf-wrap"))
}

/// Write a jq wrap-manifest into `dir`; returns the manifest path.
pub fn write_jq_manifest(dir: &Path) -> PathBuf {
    let jq = find_bin("jq");
    let manifest = serde_json::json!({
        "name": "jq",
        "description": "Run a jq filter over a JSON document supplied on stdin.",
        "determinism": "deterministic",
        "binary": jq,
        "allowlist": [jq],
        "args_template": ["-c", "{expr}"],
        "stdin_from": "input",
        "timeout_ms": 10000,
        "workdir": dir.join("jail"),
        "env": {},
        "input_schema": {
            "type": "object",
            "properties": {
                "expr": {"type": "string"},
                "input": {"type": "string"}
            },
            "required": ["expr", "input"],
            "additionalProperties": false
        }
    });
    let path = dir.join("jq.json");
    std::fs::write(&path, serde_json::to_string_pretty(&manifest).unwrap()).unwrap();
    path
}

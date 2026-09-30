//! `StdioMcpTransport`: the single production `Transport` impl — spawns a
//! leaf server as a child process (allowlisted, scrubbed env, cwd jail),
//! speaks MCP over its stdio via rmcp, and verifies the leaf's determinism
//! declaration against the spec before handing out a handle.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use leaf_traits::{DeterminismClass, LeafHandle, LeafOutput, LeafSpec, Transport, TransportError};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::child_process::{ConfigureCommandExt, TokioChildProcess};

use crate::wrap_server::MANIFEST_TOOL;

pub struct StdioMcpTransport {
    /// Absolute paths of leaf-server binaries this runtime may spawn.
    /// Checked with canonicalize + exact match BEFORE spawn.
    pub server_allowlist: Vec<PathBuf>,
}

pub struct McpLeafHandle {
    running: RunningService<RoleClient, ()>,
    determinism: DeterminismClass,
}

impl std::fmt::Debug for McpLeafHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpLeafHandle")
            .field("determinism", &self.determinism)
            .finish()
    }
}

fn concat_text_content(result: &CallToolResult) -> String {
    let mut out = String::new();
    for c in &result.content {
        if let Some(t) = c.as_text() {
            out.push_str(&t.text);
        }
    }
    out
}

impl Transport for StdioMcpTransport {
    type Handle = McpLeafHandle;

    async fn connect(&self, spec: &LeafSpec) -> Result<McpLeafHandle, TransportError> {
        let canonical = std::fs::canonicalize(&spec.command)
            .map_err(|_| TransportError::NotAllowlisted(spec.command.clone()))?;
        let allowed = self
            .server_allowlist
            .iter()
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .any(|p| p == canonical);
        if !allowed {
            return Err(TransportError::NotAllowlisted(canonical));
        }

        // Check canonical, execute as-given (argv[0] personality — see sandbox.rs).
        let cmd = tokio::process::Command::new(&spec.command).configure(|c| {
            c.args(&spec.args).env_clear().envs(&spec.env);
            if let Some(dir) = &spec.cwd {
                c.current_dir(dir);
            }
        });
        let child =
            TokioChildProcess::new(cmd).map_err(|e| TransportError::Spawn(e.to_string()))?;
        let running = ().serve(child).await.map_err(|e| TransportError::Spawn(e.to_string()))?;

        // Read the machine-readable determinism declaration and cross-check.
        let manifest_result = tokio::time::timeout(
            Duration::from_millis(spec.call_timeout_ms),
            running
                .peer()
                .call_tool(CallToolRequestParams::new(MANIFEST_TOOL)),
        )
        .await
        .map_err(|_| TransportError::Timeout(spec.call_timeout_ms))?
        .map_err(|e| TransportError::Protocol(format!("{MANIFEST_TOOL}: {e}")))?;
        let manifest_json = concat_text_content(&manifest_result);
        let declared: DeterminismClass = serde_json::from_str::<serde_json::Value>(&manifest_json)
            .ok()
            .and_then(|v| serde_json::from_value(v.get("determinism")?.clone()).ok())
            .ok_or_else(|| {
                TransportError::Protocol(format!(
                    "leaf did not return a parseable {MANIFEST_TOOL} declaration"
                ))
            })?;
        if declared != spec.expected_determinism {
            return Err(TransportError::DeterminismMismatch {
                declared,
                expected: spec.expected_determinism,
            });
        }

        Ok(McpLeafHandle {
            running,
            determinism: declared,
        })
    }
}

impl LeafHandle for McpLeafHandle {
    fn determinism(&self) -> DeterminismClass {
        self.determinism
    }

    async fn call(
        &mut self,
        tool: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<LeafOutput, TransportError> {
        let arguments = match args {
            serde_json::Value::Object(o) => Some(o),
            serde_json::Value::Null => None,
            other => {
                return Err(TransportError::Protocol(format!(
                    "tool args must be a JSON object, got: {other}"
                )));
            }
        };
        let started = Instant::now();
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Some(o) = arguments {
            params = params.with_arguments(o);
        }
        let result = tokio::time::timeout(timeout, self.running.peer().call_tool(params))
            .await
            .map_err(|_| TransportError::Timeout(timeout.as_millis() as u64))?
            .map_err(|e| TransportError::Protocol(e.to_string()))?;

        Ok(LeafOutput {
            text: concat_text_content(&result),
            is_error: result.is_error.unwrap_or(false),
            duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

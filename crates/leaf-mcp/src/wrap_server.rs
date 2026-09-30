//! `WrapServer`: a real MCP server (manual `ServerHandler`, stdio transport)
//! exposing one wrapped CLI binary as a tool, plus the reserved
//! `__leaf_manifest` tool carrying the machine-readable determinism
//! declaration (MCP has no standard server-metadata blob, so a reserved tool
//! is the cleanest verifiable surface — stock clients just see two tools).

use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer};

use crate::manifest::WrapManifest;
use crate::sandbox::run_sandboxed;

pub const MANIFEST_TOOL: &str = "__leaf_manifest";

#[derive(Clone)]
pub struct WrapServer {
    pub manifest: Arc<WrapManifest>,
}

impl WrapServer {
    pub fn new(manifest: WrapManifest) -> Self {
        Self {
            manifest: Arc::new(manifest),
        }
    }
}

/// Stringify one tool arg for argv insertion. Only scalars are allowed —
/// nested structures in argv are a manifest design smell.
fn arg_to_string(v: &serde_json::Value) -> Result<String, McpError> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        other => Err(McpError::invalid_params(
            format!("argument must be a scalar, got: {other}"),
            None,
        )),
    }
}

/// Single-pass placeholder substitution: `{key}` tokens are resolved
/// left-to-right and inserted VALUES ARE NEVER RESCANNED, so a value
/// containing "{other_key}" cannot trigger a second substitution.
fn substitute(
    template: &str,
    keys: &[&str],
    values: &std::collections::BTreeMap<&str, String>,
) -> Result<String, McpError> {
    let mut out = String::with_capacity(template.len());
    let mut cursor = 0usize;
    while cursor < template.len() {
        // Earliest placeholder at or after the cursor.
        let next = keys
            .iter()
            .filter_map(|k| {
                template[cursor..]
                    .find(&format!("{{{k}}}"))
                    .map(|off| (cursor + off, *k))
            })
            .min();
        match next {
            Some((pos, key)) => {
                out.push_str(&template[cursor..pos]);
                let val = values.get(key).ok_or_else(|| {
                    McpError::invalid_params(format!("missing required argument '{key}'"), None)
                })?;
                out.push_str(val);
                cursor = pos + key.len() + 2;
            }
            None => {
                out.push_str(&template[cursor..]);
                break;
            }
        }
    }
    Ok(out)
}

impl WrapServer {
    fn build_tools(&self) -> Vec<Tool> {
        let m = &self.manifest;
        let determinism_note = match m.determinism {
            leaf_traits::DeterminismClass::Deterministic => "deterministic: true",
            leaf_traits::DeterminismClass::ReplayOnly => {
                "deterministic: false (journaled-replay only)"
            }
        };
        let schema: Arc<JsonObject> =
            Arc::new(m.input_schema.as_object().cloned().unwrap_or_default());
        vec![
            Tool::new(
                m.name.clone(),
                format!("{} [{determinism_note}]", m.description),
                schema,
            ),
            Tool::new(
                MANIFEST_TOOL,
                "Reserved: returns this leaf's manifest (binary, allowlist, determinism \
                 declaration) as JSON. Data surface for runtime verification.",
                Arc::new(
                    serde_json::json!({
                        "type": "object", "properties": {}, "additionalProperties": false
                    })
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
                ),
            ),
        ]
    }

    async fn call_wrapped(&self, args: &JsonObject) -> Result<CallToolResult, McpError> {
        let m = &self.manifest;

        for req in m.required_args() {
            if !args.contains_key(req) {
                return Err(McpError::invalid_params(
                    format!("missing required argument '{req}'"),
                    None,
                ));
            }
        }

        let props = m.property_names();
        let mut values = std::collections::BTreeMap::new();
        for (k, v) in args {
            if !props.contains(&k.as_str()) {
                return Err(McpError::invalid_params(
                    format!("unknown argument '{k}'"),
                    None,
                ));
            }
            values.insert(k.as_str(), arg_to_string(v)?);
        }

        let mut argv = Vec::with_capacity(m.args_template.len());
        for el in &m.args_template {
            argv.push(substitute(el, &props, &values)?);
        }

        let stdin_data = match &m.stdin_from {
            Some(key) => Some(values.get(key.as_str()).cloned().ok_or_else(|| {
                McpError::invalid_params(format!("missing required argument '{key}'"), None)
            })?),
            None => None,
        };

        match run_sandboxed(m, argv, stdin_data.as_deref()).await {
            Ok(out) if out.exit_code == 0 => {
                Ok(CallToolResult::success(vec![Content::text(out.stdout)]))
            }
            Ok(out) => Ok(CallToolResult::error(vec![Content::text(format!(
                "exit {}:\n{}",
                out.exit_code, out.stderr
            ))])),
            // Sandbox refusals/timeouts are protocol-level failures, not tool
            // output the model should reason about.
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }
}

impl ServerHandler for WrapServer {
    fn get_info(&self) -> ServerInfo {
        let m = &self.manifest;
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            format!(
                "leaf-wrap server for '{}' (binary: {}, determinism: {:?}). \
                 Call the '{}' tool with schema-conformant args; call '{MANIFEST_TOOL}' \
                 for the machine-readable determinism declaration.",
                m.name,
                m.binary.display(),
                m.determinism,
                m.name,
            ),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(self.build_tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = request.name.as_ref();
        if name == MANIFEST_TOOL {
            return Ok(CallToolResult::success(vec![Content::text(
                self.manifest.redacted().to_string(),
            )]));
        }
        if name == self.manifest.name {
            let empty = JsonObject::new();
            let args = request.arguments.as_ref().unwrap_or(&empty);
            return self.call_wrapped(args).await;
        }
        Err(McpError::invalid_params(
            format!("unknown tool '{name}'"),
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_resolves_placeholders_left_to_right() {
        let mut values = std::collections::BTreeMap::new();
        values.insert("expr", ".a".to_string());
        values.insert("input", "{\"a\":1}".to_string());
        let out = substitute("run {expr} on {input}", &["expr", "input"], &values).unwrap();
        assert_eq!(out, "run .a on {\"a\":1}");
    }

    #[test]
    fn substitute_never_rescans_inserted_values() {
        // A malicious arg value containing another placeholder must land
        // verbatim, not trigger a second substitution.
        let mut values = std::collections::BTreeMap::new();
        values.insert("expr", "{input}".to_string());
        values.insert("input", "SECRET".to_string());
        let out = substitute("{expr}", &["expr", "input"], &values).unwrap();
        assert_eq!(out, "{input}");
    }

    #[test]
    fn substitute_missing_referenced_arg_errors() {
        let values = std::collections::BTreeMap::new();
        assert!(substitute("{expr}", &["expr"], &values).is_err());
    }

    #[test]
    fn substitute_leaves_unknown_braces_alone() {
        // jq programs legitimately contain braces: only declared property
        // names are placeholders.
        let mut values = std::collections::BTreeMap::new();
        values.insert("expr", "x".to_string());
        let out = substitute("{title: .title}", &["expr"], &values).unwrap();
        assert_eq!(out, "{title: .title}");
    }
}

//! MCP stdio client transport and the `leaf-wrap` sandboxed CLI-wrapper server.
//!
//! A leaf = a real MCP stdio server (verifiable with any stock MCP client)
//! plus a determinism contract: a declared `DeterminismClass` exposed via the
//! reserved `__leaf_manifest` tool, journaled inputs/outputs, and sandboxed
//! execution (binary allowlist, scrubbed env, working-dir jail, timeout).

pub mod client;
pub mod manifest;
pub mod sandbox;
pub mod wrap_server;

pub use client::{McpLeafHandle, StdioMcpTransport};
pub use manifest::WrapManifest;
pub use sandbox::{SandboxError, SandboxOutput, run_sandboxed};
pub use wrap_server::{MANIFEST_TOOL, WrapServer};

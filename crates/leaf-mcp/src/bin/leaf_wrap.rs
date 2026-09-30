//! `leaf-wrap --manifest <path>`: serve one wrapped CLI binary as a sandboxed
//! MCP stdio server.
//!
//! HARD RULE: stdout is the MCP wire. All logging goes to stderr.

use anyhow::{Context, Result, bail};
use leaf_mcp::WrapServer;
use leaf_mcp::manifest::WrapManifest;
use leaf_mcp::sandbox::check_allowlisted;
use rmcp::ServiceExt;
use rmcp::transport::io::stdio;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let manifest_path = match (args.next().as_deref(), args.next()) {
        (Some("--manifest"), Some(p)) => p,
        _ => bail!("usage: leaf-wrap --manifest <path>"),
    };

    let manifest = WrapManifest::load(std::path::Path::new(&manifest_path))
        .with_context(|| format!("loading manifest {manifest_path}"))?;

    // Startup validation: refuse to serve a manifest whose binary does not
    // canonicalize into its own allowlist. This is where the mandated
    // refusal test bites.
    check_allowlisted(&manifest.binary, &manifest.allowlist)
        .map_err(|e| anyhow::anyhow!("refusing to serve: {e} (manifest {manifest_path})"))?;
    std::fs::create_dir_all(&manifest.workdir)
        .with_context(|| format!("creating workdir {}", manifest.workdir.display()))?;

    tracing::info!(
        tool = %manifest.name,
        binary = %manifest.binary.display(),
        determinism = ?manifest.determinism,
        "leaf-wrap serving on stdio"
    );

    let service = WrapServer::new(manifest)
        .serve(stdio())
        .await
        .context("serving MCP on stdio")?;
    service.waiting().await?;
    Ok(())
}

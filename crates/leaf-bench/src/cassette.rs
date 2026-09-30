//! The cross-run cassette: replay-first, record-on-miss, keyed by the
//! canonicalized leaf invocation.
//!
//! This is the bench-layer answer to live-data drift: `ground-truth` seeds
//! the cassette once, then every benchmark condition resolves ReplayOnly
//! (network) leaves from the same recorded world — byte-identical across
//! conditions, no GitHub rate noise, no drift between grading and reality.
//! Deterministic leaves (jq) always run live. Misses (novel invocations the
//! free-driving control invents) pass through live and are appended, with
//! the miss counted per run.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use leaf_core::sha256_hex;
use leaf_traits::{DeterminismClass, LeafHandle, LeafOutput, LeafSpec, Transport, TransportError};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct CassetteEntry {
    leaf: String,
    tool: String,
    args: serde_json::Value,
    output: String,
    is_error: bool,
    recorded_at_ms: u64,
}

#[derive(Clone)]
pub struct CassetteStore {
    dir: PathBuf,
}

impl CassetteStore {
    pub fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn key(leaf: &str, tool: &str, args: &serde_json::Value) -> String {
        // serde_json maps are BTreeMaps → canonical bytes.
        let canonical = serde_json::json!({"leaf": leaf, "tool": tool, "args": args});
        sha256_hex(canonical.to_string().as_bytes())
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    fn lookup(&self, leaf: &str, tool: &str, args: &serde_json::Value) -> Option<(String, bool)> {
        let path = self.path_for(&Self::key(leaf, tool, args));
        let text = std::fs::read_to_string(path).ok()?;
        let entry: CassetteEntry = serde_json::from_str(&text).ok()?;
        Some((entry.output, entry.is_error))
    }

    fn store(
        &self,
        leaf: &str,
        tool: &str,
        args: &serde_json::Value,
        output: &str,
        is_error: bool,
    ) {
        let entry = CassetteEntry {
            leaf: leaf.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output: output.to_string(),
            is_error,
            recorded_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        let path = self.path_for(&Self::key(leaf, tool, args));
        if let Ok(json) = serde_json::to_string_pretty(&entry) {
            let _ = std::fs::write(path, json);
        }
    }
}

pub struct CassetteTransport<'a, T: Transport> {
    pub inner: &'a T,
    pub store: CassetteStore,
    /// `true` = record mode (miss passes through to the live inner transport
    /// and is appended) — used by `ground-truth` seeding. `false` = replay
    /// mode (a miss is fail-closed, returning an error output without any
    /// live call) — used by the benchmark, so EVERY condition lives strictly
    /// in the recorded world.
    pub record: bool,
    pub hits: Arc<AtomicU64>,
    pub misses: Arc<AtomicU64>,
}

impl<'a, T: Transport> CassetteTransport<'a, T> {
    /// Record mode: misses pass through live and are appended (seeding).
    pub fn recording(inner: &'a T, store: CassetteStore) -> Self {
        Self::with_mode(inner, store, true)
    }
    /// Replay mode: misses are fail-closed (benchmark).
    pub fn replaying(inner: &'a T, store: CassetteStore) -> Self {
        Self::with_mode(inner, store, false)
    }
    fn with_mode(inner: &'a T, store: CassetteStore, record: bool) -> Self {
        Self {
            inner,
            store,
            record,
            hits: Arc::new(AtomicU64::new(0)),
            misses: Arc::new(AtomicU64::new(0)),
        }
    }
}

pub enum CassetteHandle<H> {
    Passthrough(H),
    Cassetted {
        leaf_id: String,
        inner: H,
        store: CassetteStore,
        record: bool,
        hits: Arc<AtomicU64>,
        misses: Arc<AtomicU64>,
    },
}

impl<H: LeafHandle> LeafHandle for CassetteHandle<H> {
    fn determinism(&self) -> DeterminismClass {
        match self {
            CassetteHandle::Passthrough(h) => h.determinism(),
            CassetteHandle::Cassetted { inner, .. } => inner.determinism(),
        }
    }

    async fn call(
        &mut self,
        tool: &str,
        args: serde_json::Value,
        timeout: Duration,
    ) -> Result<LeafOutput, TransportError> {
        match self {
            CassetteHandle::Passthrough(h) => h.call(tool, args, timeout).await,
            CassetteHandle::Cassetted {
                leaf_id,
                inner,
                store,
                record,
                hits,
                misses,
            } => {
                if let Some((output, is_error)) = store.lookup(leaf_id, tool, &args) {
                    hits.fetch_add(1, Ordering::SeqCst);
                    return Ok(LeafOutput {
                        text: output,
                        is_error,
                        duration_ms: 0,
                    });
                }
                misses.fetch_add(1, Ordering::SeqCst);
                if *record {
                    let out = inner.call(tool, args.clone(), timeout).await?;
                    store.store(leaf_id, tool, &args, &out.text, out.is_error);
                    Ok(out)
                } else {
                    // Replay mode: a novel invocation the model invented is NOT
                    // executed live — it fails closed so the run stays strictly
                    // inside the recorded world. The model can adapt/retry just
                    // as it would against a dead endpoint.
                    Ok(LeafOutput {
                        text: "cassette miss: no recorded response for this invocation in the \
                               replayed world"
                            .into(),
                        is_error: true,
                        duration_ms: 0,
                    })
                }
            }
        }
    }
}

impl<T: Transport> Transport for CassetteTransport<'_, T> {
    type Handle = CassetteHandle<T::Handle>;

    async fn connect(&self, spec: &LeafSpec) -> Result<Self::Handle, TransportError> {
        let inner = self.inner.connect(spec).await?;
        match spec.expected_determinism {
            DeterminismClass::Deterministic => Ok(CassetteHandle::Passthrough(inner)),
            DeterminismClass::ReplayOnly => Ok(CassetteHandle::Cassetted {
                leaf_id: spec.id.clone(),
                inner,
                store: self.store.clone(),
                record: self.record,
                hits: self.hits.clone(),
                misses: self.misses.clone(),
            }),
        }
    }
}

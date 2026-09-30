//! Injected time. Game logic never calls `SystemTime::now()` directly —
//! journal timestamps flow through `Clock` so tests and replay are stable.
//! Timestamps are informational only; replay never asserts them.

use std::sync::atomic::{AtomicU64, Ordering};

pub trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Fixed (or manually stepped) clock for tests and replay.
pub struct FixedClock(pub AtomicU64);

impl FixedClock {
    pub fn new(ms: u64) -> Self {
        Self(AtomicU64::new(ms))
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_unix_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

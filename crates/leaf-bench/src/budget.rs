//! Append-only budget ledger with reserve-before-dispatch.
//!
//! Every billed API call is bracketed: `reserve(bucket, projected)` BEFORE
//! dispatch (fails if the bucket or global cap would be crossed), then
//! `settle(id, actual)` after the usage block is known. Sums count the
//! actual when present, else the reservation — so a crash between dispatch
//! and settle over-counts (conservative), never under-counts.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct LedgerEntry {
    id: u64,
    bucket: String,
    event: LedgerEvent,
    microusd: u64,
    ts_ms: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum LedgerEvent {
    Reserve,
    Actual,
}

#[derive(Debug, thiserror::Error)]
#[error("budget cap reached: {scope} would hit {would} µUSD of {cap} µUSD cap")]
pub struct BudgetExceeded {
    pub scope: String,
    pub would: u64,
    pub cap: u64,
}

pub struct Budget {
    path: PathBuf,
    entries: Vec<LedgerEntry>,
    global_cap: u64,
    bucket_caps: BTreeMap<String, u64>,
    next_id: u64,
}

impl Budget {
    pub fn load(
        path: PathBuf,
        global_cap: u64,
        bucket_caps: BTreeMap<String, u64>,
    ) -> Result<Self> {
        let mut entries: Vec<LedgerEntry> = Vec::new();
        if path.exists() {
            for (i, line) in std::fs::read_to_string(&path)?.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                entries.push(
                    serde_json::from_str(line).with_context(|| format!("ledger line {}", i + 1))?,
                );
            }
        }
        let next_id = entries.iter().map(|e| e.id + 1).max().unwrap_or(0);
        Ok(Self {
            path,
            entries,
            global_cap,
            bucket_caps,
            next_id,
        })
    }

    fn append(&mut self, entry: LedgerEntry) -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(serde_json::to_string(&entry)?.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()?;
        self.entries.push(entry);
        Ok(())
    }

    /// Effective spend per reservation id: actual if settled, else reserve.
    fn effective(&self) -> BTreeMap<u64, (String, u64)> {
        let mut m: BTreeMap<u64, (String, u64)> = BTreeMap::new();
        for e in &self.entries {
            match e.event {
                LedgerEvent::Reserve => {
                    m.entry(e.id)
                        .or_insert_with(|| (e.bucket.clone(), e.microusd));
                }
                LedgerEvent::Actual => {
                    m.insert(e.id, (e.bucket.clone(), e.microusd));
                }
            }
        }
        m
    }

    pub fn spent_total(&self) -> u64 {
        self.effective().values().map(|(_, v)| v).sum()
    }

    pub fn spent_bucket(&self, bucket: &str) -> u64 {
        self.effective()
            .values()
            .filter(|(b, _)| b == bucket)
            .map(|(_, v)| v)
            .sum()
    }

    pub fn spent_by_bucket(&self) -> BTreeMap<String, u64> {
        let mut m = BTreeMap::new();
        for (b, v) in self.effective().values() {
            *m.entry(b.clone()).or_insert(0) += v;
        }
        m
    }

    /// Reserve `projected` µUSD against `bucket`, enforcing both caps.
    pub fn reserve(&mut self, bucket: &str, projected: u64) -> Result<u64> {
        let total = self.spent_total();
        if total + projected > self.global_cap {
            return Err(BudgetExceeded {
                scope: "GLOBAL".into(),
                would: total + projected,
                cap: self.global_cap,
            }
            .into());
        }
        if let Some(cap) = self.bucket_caps.get(bucket) {
            let b = self.spent_bucket(bucket);
            if b + projected > *cap {
                return Err(BudgetExceeded {
                    scope: format!("bucket {bucket}"),
                    would: b + projected,
                    cap: *cap,
                }
                .into());
            }
        }
        let id = self.next_id;
        self.next_id += 1;
        self.append(LedgerEntry {
            id,
            bucket: bucket.to_string(),
            event: LedgerEvent::Reserve,
            microusd: projected,
            ts_ms: now_ms(),
        })?;
        Ok(id)
    }

    pub fn settle(&mut self, id: u64, bucket: &str, actual: u64) -> Result<()> {
        self.append(LedgerEntry {
            id,
            bucket: bucket.to_string(),
            event: LedgerEvent::Actual,
            microusd: actual,
            ts_ms: now_ms(),
        })
    }

    /// Projection for the next call in a bucket: max actual seen there so
    /// far, floored at a conservative default.
    pub fn projection(&self, bucket: &str) -> u64 {
        const FLOOR: u64 = 10_000; // $0.01
        self.entries
            .iter()
            .filter(|e| e.bucket == bucket && e.event == LedgerEvent::Actual)
            .map(|e| e.microusd)
            .max()
            .unwrap_or(0)
            .max(FLOOR)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(dir: &tempfile::TempDir, global: u64, buckets: &[(&str, u64)]) -> Budget {
        Budget::load(
            dir.path().join("ledger.jsonl"),
            global,
            buckets.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        )
        .unwrap()
    }

    #[test]
    fn reserve_settle_and_caps() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = budget(&dir, 100, &[("x", 50)]);
        let id = b.reserve("x", 40).unwrap();
        assert_eq!(b.spent_total(), 40);
        b.settle(id, "x", 30).unwrap();
        assert_eq!(b.spent_total(), 30);
        assert_eq!(b.spent_bucket("x"), 30);
        // Bucket cap: 30 + 25 > 50.
        assert!(b.reserve("x", 25).is_err());
        // Other bucket ok, but global cap binds: 30 + 80 > 100.
        assert!(b.reserve("y", 80).is_err());
        let id2 = b.reserve("y", 60).unwrap();
        b.settle(id2, "y", 60).unwrap();
        assert_eq!(b.spent_total(), 90);
    }

    #[test]
    fn unsettled_reservation_counts_conservatively_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        {
            let mut b = Budget::load(path.clone(), 1000, BTreeMap::new()).unwrap();
            let _id = b.reserve("x", 123).unwrap();
            // Crash before settle.
        }
        let b = Budget::load(path, 1000, BTreeMap::new()).unwrap();
        assert_eq!(b.spent_total(), 123);
    }

    #[test]
    fn projection_grows_with_actuals() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = budget(&dir, 1_000_000, &[]);
        assert_eq!(b.projection("x"), 10_000);
        let id = b.reserve("x", 10_000).unwrap();
        b.settle(id, "x", 42_000).unwrap();
        assert_eq!(b.projection("x"), 42_000);
    }
}

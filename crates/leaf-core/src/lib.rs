//! Tree/branch/leaf executor, append-only journal, and replay.
//!
//! There is no hidden state: a run is a seed, a tree, an input, and a
//! journal. Replay re-runs the executor serving LLM decisions from the
//! journal and re-executing deterministic leaves, asserting the decision
//! sequence and outputs are identical.

pub mod clock;
pub mod digest;
pub mod executor;
pub mod journal;
pub mod replay;
pub mod router;
pub mod tree;

#[cfg(test)]
pub(crate) mod testutil;

pub use clock::{Clock, FixedClock, SystemClock};
pub use digest::sha256_hex;
pub use executor::{ExecError, Executor, RunConfig, RunResult};
pub use journal::{
    FORMAT_VERSION, JournalEntry, JournalError, JournalReader, JournalWriter, RunOutcome,
};
pub use replay::{Divergence, ReplayReport, replay};
pub use router::{
    ExplorationRecord, LlmCallRecord, RouteDecision, RouteRequest, Router, RouterError,
    ScriptedRouter,
};
pub use tree::{ArgsTemplate, Branch, LeafNode, Node, RouteMode, Tree, TreeError, derive_seed};

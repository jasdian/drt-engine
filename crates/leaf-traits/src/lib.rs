//! Shared vocabulary and trait seams for the leaf runtime.
//!
//! The three traits here (`Codec`, `Propagation`, `Transport`) exist so that
//! demoted design hypotheses stay swappable *experiments* rather than beliefs
//! baked into the executor. Exactly the impls named in the design exist —
//! do not add more.

pub mod codec;
pub mod propagation;
pub mod transport;
pub mod types;

pub use codec::{Codec, JsonCodec, ToonCodec, fence_untrusted};
pub use propagation::{DecayedSummary, FullLog, LastK, Propagation};
pub use transport::{LeafHandle, LeafSpec, Transport, TransportError};
pub use types::{CandidateInfo, DeterminismClass, HistoryItem, LeafOutput};

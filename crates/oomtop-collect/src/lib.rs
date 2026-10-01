//! # oomtop-collect
//!
//! Collectors = **Source** (I/O) + **decode** (pure) (SPEC §6.1). Sources return serde-serializable
//! [`RawSample`]s; [`decode()`] turns them into a [`PartialSnapshot`] on any OS, so fixtures recorded on a
//! Mac replay in Linux CI and vice versa ([`replay`]). The [`sampler::Sampler`] runs sources on the tiered
//! cadence of SPEC §6.2. [`signal`] is the only place that sends signals (with identity re-verification).
//!
//! Invariants: sources honor their time budget (≤ 50 ms, SPEC §14), never panic on missing/changed OS
//! data (they return [`SourceError::Unavailable`] or a partial sample), and never store environments.

pub mod decode;
#[cfg(unix)]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
pub mod raw;
pub mod replay;
pub mod sampler;
pub mod signal;

pub use decode::{decode, PartialSnapshot};
pub use raw::{RawPayload, RawSample};
pub use sampler::{Cadence, Sampler, SamplerOptions, Tier};

use std::time::Duration;
use thiserror::Error;

/// Why a source could not produce a sample.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// Not available on this OS / without privileges / data missing.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Exceeded its budget (ms).
    #[error("timed out after {0} ms")]
    Timeout(u64),
    #[error("i/o: {0}")]
    Io(String),
}

/// One OS data source. I/O only — no interpretation (that is `decode`).
pub trait Source: Send {
    /// Stable name, e.g. "macos.procs"; used as `RawSample::source` and `Snapshot::source_status` key.
    fn name(&self) -> &'static str;
    /// Reads one raw sample within `budget`. Must not panic; may return a partial sample.
    fn read(&mut self, budget: Duration) -> Result<RawSample, SourceError>;
}

/// Wall clock in ms since the Unix epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

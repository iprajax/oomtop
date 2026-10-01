//! # oomtop-core
//!
//! Pure core of oomtop (SPEC §6): the data model, units, `Measured<T>`, attribution matching, headroom,
//! `can_fit`, model-load estimates, OOM forecast, throttle, `why`, redaction, query understanding, ranking,
//! headline generation and situation modes. **No I/O** — everything is a pure function over snapshots,
//! so it is fully unit-testable with fixtures and golden files.
//!
//! Module ↔ spec map:
//! | module | spec |
//! |---|---|
//! | [`model`] | SPEC §5 (FROZEN) |
//! | [`measured`], [`units`] | SPEC §5, UX §12.5 |
//! | [`headroom`] | SPEC §8.1 |
//! | [`can_fit`] | SPEC §8.2 |
//! | [`model_estimate`] | SPEC §8.2 (GGUF header parser) |
//! | [`history`], [`forecast`] | SPEC §6.2, §8.3 |
//! | [`attribution`] | SPEC §7 |
//! | [`idle`] | SPEC §7 (idle/orphan) |
//! | [`fingerprint`] | UX §4 |
//! | [`throttle`], [`why`] | SPEC §9 |
//! | [`redact`] | SPEC §13 |
//! | [`actions`] | SPEC §7, §13 |
//! | [`query`] | UX §5.2 |
//! | [`ranking`] | UX §5.4 |
//! | [`headline`] | UX §9 |
//! | [`machine`] | UX §2 (machine display name) |
//! | [`modes`] | UX §3 |
//! | [`provider`] | frontend contracts (docs/CONTRACTS.md) |

pub mod actions;
pub mod attribution;
pub mod can_fit;
pub mod fingerprint;
pub mod forecast;
pub mod headline;
pub mod headroom;
pub mod history;
pub mod idle;
pub mod machine;
pub mod measured;
pub mod model;
pub mod model_estimate;
pub mod modes;
pub mod provider;
pub mod query;
pub mod ranking;
pub mod redact;
pub mod throttle;
pub mod units;
pub mod why;

pub use model::*;

/// Crate version (also the `oomtop --version`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

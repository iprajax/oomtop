//! # oomtop-mcp
//!
//! MCP server over stdio (SPEC §12.3): newline-delimited JSON-RPC 2.0, protocol `2025-06-18`
//! (`initialize` → `notifications/initialized` → `tools/list` / `tools/call`, plus `ping`). Every tool
//! declares an `outputSchema` and returns `structuredContent` with a JSON text copy; read-only tools carry
//! `readOnlyHint`. Everything exported is redacted (SPEC §13).
//!
//! `reclaim` (`destructiveHint`) is registered **only** with `--allow-actions`. oomtop itself asks the user
//! through MCP *elicitation* (`elicitation/create`, only if the client declared the capability) with the
//! exact targets, pids and estimated gains, and signals only on `action == "accept"` with `confirm == true`.
//! Otherwise nothing is executed and the `oomtop reclaim --groups …` command is returned for the user. The
//! calling agent's own session (the MCP process's parent chain) and oomtop itself are never targets.
//!
//! The server samples **on each call** (the CLI's provider takes two samples 250 ms apart) and blocks on
//! stdin between calls, so an idle MCP server costs no CPU (SPEC §6.2, §14).

pub mod elicit;
pub mod export;
pub mod jsonrpc;
pub mod peer;
mod server;
pub mod tools;
mod transport;

use oomtop_core::actions::{Actuator, ProtectContext};
use oomtop_core::provider::SnapshotProvider;
use thiserror::Error;

pub use export::export_snapshot;
pub use peer::{ClientPeer, NoPeer, PeerError};
pub use server::Server;
pub use tools::tool_definitions;
pub use transport::{serve_io, serve_stdio, IoConfig, ELICITATION_TIMEOUT, MAX_LINE_BYTES};

/// The MCP protocol version this server implements (and prefers).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Versions accepted in `initialize` (echoed back when requested). Elicitation needs 2025-06-18.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = [PROTOCOL_VERSION, "2025-03-26", "2024-11-05"];

#[derive(Debug, Error)]
pub enum McpError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// Estimates the bytes needed to load a model file (headers only), for `can_fit({model_path})`.
pub type ModelEstimator = Box<dyn Fn(&str) -> Result<u64, String> + Send>;

pub struct McpOptions {
    pub provider: Box<dyn SnapshotProvider>,
    /// Executes a confirmed plan (`reclaim`); `None` = actions unavailable even with `allow_actions`.
    pub actuator: Option<Box<dyn Actuator>>,
    /// Registers `reclaim` (`oomtop mcp --allow-actions`).
    pub allow_actions: bool,
    /// Protection context; `ancestor_pids`/`self_pid`/`caller_group` identify the calling agent's session.
    pub protect: ProtectContext,
    /// Estimates bytes for `can_fit({model_path})` (reads only headers; injected by the CLI).
    pub model_estimator: Option<ModelEstimator>,
    /// Units of every human-readable summary (`format.memory_units`; the CLI passes the config's, so MCP
    /// and `oomtop headroom` say the same amounts). `UnitSystem::default()` = IEC ("13.0 GiB").
    pub units: oomtop_core::units::UnitSystem,
}

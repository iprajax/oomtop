//! Server → client requests (MCP elicitation) behind a small trait, so the session logic stays independent
//! of the transport and tests can script the client.

use serde_json::Value;
use std::time::Duration;

/// Why a server → client request produced no result.
#[derive(Debug, Clone, PartialEq)]
pub enum PeerError {
    /// No channel to the client (e.g. [`crate::Server::handle`] without a transport).
    Unsupported,
    /// The client did not answer in time.
    Timeout(Duration),
    /// The client closed the connection.
    Closed,
    /// The client cancelled the tool call this request was made for.
    Cancelled,
    /// The client answered with a JSON-RPC error.
    Rpc { code: i64, message: String },
    /// Transport failure.
    Io(String),
}

impl std::fmt::Display for PeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeerError::Unsupported => write!(f, "no client channel"),
            PeerError::Timeout(d) => write!(f, "no answer within {} s", d.as_secs()),
            PeerError::Closed => write!(f, "client disconnected"),
            PeerError::Cancelled => write!(f, "request cancelled by the client"),
            PeerError::Rpc { code, message } => write!(f, "client error {code}: {message}"),
            PeerError::Io(e) => write!(f, "i/o: {e}"),
        }
    }
}

/// Sends a request to the MCP client and blocks until its response.
pub trait ClientPeer {
    /// `on_behalf_of` is the id of the client request being served (a `notifications/cancelled` for it
    /// aborts the wait with [`PeerError::Cancelled`]).
    fn request(&mut self, method: &str, params: Value, on_behalf_of: &Value) -> Result<Value, PeerError>;
}

/// A peer that cannot reach the client.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPeer;

impl ClientPeer for NoPeer {
    fn request(&mut self, _method: &str, _params: Value, _on_behalf_of: &Value) -> Result<Value, PeerError> {
        Err(PeerError::Unsupported)
    }
}

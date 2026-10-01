//! Newline-delimited JSON-RPC over a byte stream (MCP stdio transport).
//!
//! A reader thread turns lines into events on a channel; the main loop blocks on that channel, so an idle
//! server uses no CPU. During a server → client request (elicitation) the loop keeps answering `ping`,
//! honours `notifications/cancelled` for the call being served and queues every other message until the
//! call completes. Nothing but JSON-RPC is ever written to the output stream.

use crate::jsonrpc::{self, classify, Incoming, INTERNAL_ERROR, INVALID_REQUEST, PARSE_ERROR};
use crate::peer::{ClientPeer, PeerError};
use crate::{McpError, McpOptions, Server};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufRead, ErrorKind, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Lines longer than this are rejected with a parse error (and skipped to the next newline).
pub const MAX_LINE_BYTES: usize = 4 << 20;
/// How long `reclaim` waits for the user to answer an elicitation.
pub const ELICITATION_TIMEOUT: Duration = Duration::from_secs(300);
/// Messages queued while a call waits for the client; beyond this, requests get "busy".
const MAX_PENDING: usize = 256;

/// Transport tunables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoConfig {
    pub elicitation_timeout: Duration,
}

impl Default for IoConfig {
    fn default() -> Self {
        IoConfig {
            elicitation_timeout: ELICITATION_TIMEOUT,
        }
    }
}

enum Event {
    Line(Vec<u8>),
    TooLong,
    Failed(String),
}

fn spawn_reader<R: BufRead + Send + 'static>(mut reader: R) -> Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("oomtop-mcp-stdin".into())
        .spawn(move || loop {
            let mut buf = Vec::new();
            let limit = MAX_LINE_BYTES as u64 + 1;
            let ev = match (&mut reader).take(limit).read_until(b'\n', &mut buf) {
                Ok(0) => break, // EOF
                Ok(_) if buf.len() > MAX_LINE_BYTES && buf.last() != Some(&b'\n') => {
                    // Skip the rest of the oversized line.
                    loop {
                        let mut rest = Vec::new();
                        match (&mut reader).take(64 * 1024).read_until(b'\n', &mut rest) {
                            Ok(0) => break,
                            Ok(_) if rest.last() == Some(&b'\n') => break,
                            Ok(_) => continue,
                            Err(_) => break,
                        }
                    }
                    Event::TooLong
                }
                Ok(_) => Event::Line(buf),
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => Event::Failed(e.to_string()),
            };
            let stop = matches!(ev, Event::Failed(_));
            if tx.send(ev).is_err() || stop {
                break;
            }
        })
        .ok();
    rx
}

/// Parses one line: `Ok(None)` for blank lines, `Err(response)` for a parse error to send back.
fn parse_line(bytes: &[u8]) -> Result<Option<Value>, Value> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(jsonrpc::err(&Value::Null, PARSE_ERROR, "invalid UTF-8"));
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    serde_json::from_str::<Value>(text)
        .map(Some)
        .map_err(|e| jsonrpc::err(&Value::Null, PARSE_ERROR, &format!("parse error: {e}")))
}

fn too_long() -> Value {
    jsonrpc::err(
        &Value::Null,
        INVALID_REQUEST,
        &format!("message longer than {MAX_LINE_BYTES} bytes"),
    )
}

struct Out<W: Write> {
    w: W,
}

impl<W: Write> Out<W> {
    fn send(&mut self, v: &Value) -> std::io::Result<()> {
        // serde_json never emits a raw newline inside a message, so one message = one line.
        serde_json::to_writer(&mut self.w, v)?;
        self.w.write_all(b"\n")?;
        self.w.flush()
    }
}

struct StdioPeer<'a, W: Write> {
    rx: &'a Receiver<Event>,
    out: &'a mut Out<W>,
    pending: &'a mut VecDeque<Value>,
    next_id: &'a mut u64,
    timeout: Duration,
}

impl<W: Write> StdioPeer<'_, W> {
    fn send(&mut self, v: &Value) -> Result<(), PeerError> {
        self.out.send(v).map_err(|e| PeerError::Io(e.to_string()))
    }
}

impl<W: Write> ClientPeer for StdioPeer<'_, W> {
    fn request(&mut self, method: &str, params: Value, on_behalf_of: &Value) -> Result<Value, PeerError> {
        *self.next_id += 1;
        let id = json!(format!("oomtop-{}", self.next_id));
        self.send(&jsonrpc::request(&id, method, params))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PeerError::Timeout(self.timeout));
            }
            let ev = match self.rx.recv_timeout(remaining) {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => return Err(PeerError::Timeout(self.timeout)),
                Err(RecvTimeoutError::Disconnected) => return Err(PeerError::Closed),
            };
            let v = match ev {
                Event::Line(b) => match parse_line(&b) {
                    Ok(Some(v)) => v,
                    Ok(None) => continue,
                    Err(resp) => {
                        self.send(&resp)?;
                        continue;
                    }
                },
                Event::TooLong => {
                    self.send(&too_long())?;
                    continue;
                }
                Event::Failed(e) => return Err(PeerError::Io(e)),
            };
            match classify(&v) {
                Incoming::Response { id: rid, result } if rid == id => {
                    return result.map_err(|e| PeerError::Rpc {
                        code: e.code,
                        message: e.message,
                    });
                }
                Incoming::Response { .. } => {} // stray response: ignore
                Incoming::Request { id: rid, method, .. } if method == "ping" => {
                    self.send(&jsonrpc::ok(&rid, json!({})))?;
                }
                Incoming::Notification { method, params } if method == "notifications/cancelled" => {
                    let rid = params.get("requestId").cloned().unwrap_or(Value::Null);
                    if &rid == on_behalf_of {
                        return Err(PeerError::Cancelled);
                    }
                    self.pending
                        .retain(|m| m.get("id") != Some(&rid) || m.get("method").is_none());
                }
                Incoming::Request { id: rid, .. } if self.pending.len() >= MAX_PENDING => {
                    self.send(&jsonrpc::err(
                        &rid,
                        INTERNAL_ERROR,
                        "server busy: waiting for the user",
                    ))?;
                }
                _ => self.pending.push_back(v),
            }
        }
    }
}

/// Serves MCP over any line stream until EOF (tests use pipes; [`serve_stdio`] uses stdin/stdout).
pub fn serve_io<R, W>(opts: McpOptions, reader: R, writer: W, cfg: &IoConfig) -> Result<(), McpError>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    let rx = spawn_reader(reader);
    let mut server = Server::new(opts);
    let mut out = Out { w: writer };
    let mut pending: VecDeque<Value> = VecDeque::new();
    let mut next_id = 0u64;
    let result = loop {
        let msg = match pending.pop_front() {
            Some(m) => m,
            None => match rx.recv() {
                Ok(Event::Line(b)) => match parse_line(&b) {
                    Ok(Some(v)) => v,
                    Ok(None) => continue,
                    Err(resp) => match out.send(&resp) {
                        Ok(()) => continue,
                        Err(e) => break Err(e),
                    },
                },
                Ok(Event::TooLong) => match out.send(&too_long()) {
                    Ok(()) => continue,
                    Err(e) => break Err(e),
                },
                Ok(Event::Failed(e)) => break Err(std::io::Error::other(e)),
                Err(_) => break Ok(()), // EOF: the client closed stdin
            },
        };
        let resp = {
            let mut peer = StdioPeer {
                rx: &rx,
                out: &mut out,
                pending: &mut pending,
                next_id: &mut next_id,
                timeout: cfg.elicitation_timeout,
            };
            server.handle_with(&msg, &mut peer)
        };
        if let Some(r) = resp {
            if let Err(e) = out.send(&r) {
                break Err(e);
            }
        }
    };
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(()), // client went away
        Err(e) => Err(McpError::Io(e)),
    }
}

/// Serves MCP over stdin/stdout until EOF.
pub fn serve_stdio(opts: McpOptions) -> Result<(), McpError> {
    let stdin = std::io::BufReader::new(std::io::stdin());
    let stdout = std::io::stdout().lock();
    serve_io(opts, stdin, stdout, &IoConfig::default())
}

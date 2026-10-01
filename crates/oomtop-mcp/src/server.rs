//! The MCP session state machine and tool handlers (transport-independent).

use crate::elicit::{self, Answer};
use crate::export::export_snapshot;
use crate::jsonrpc::{self, classify, Incoming, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND};
use crate::peer::{ClientPeer, NoPeer, PeerError};
use crate::tools::{self, tool_definitions};
use crate::{McpOptions, PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS};
use oomtop_core::actions::{is_protected_process, plan_group, ActionKind, ActionPlan, ActionTarget, Refusal};
use oomtop_core::can_fit::{
    answer_exit_code, can_fit, reason_with, reclaim_candidates, Need, ReclaimCandidate,
};
use oomtop_core::headline::{input_from, render};
use oomtop_core::headroom::{compute, is_reclaim_candidate, Headroom};
use oomtop_core::modes::signals;
use oomtop_core::redact::redact_text;
use oomtop_core::units::{format_bytes, parse_bytes, UnitSystem};
use oomtop_core::why::explain;
use oomtop_core::{Group, GroupKind, Measured, Snapshot};
use serde_json::{json, Map, Value};
use std::collections::BTreeSet;

/// Maximum ancestry depth walked when looking for the calling agent's session.
const MAX_ANCESTRY: usize = 64;
/// Re-samples after a reclaim while waiting for roots to exit (each live sample takes ≥ 250 ms).
const REMEASURE_SAMPLES: usize = 4;

#[derive(Debug, Default, Clone)]
struct Session {
    initialized: bool,
    /// `notifications/initialized` received.
    ready: bool,
    protocol: Option<String>,
    client_elicitation: bool,
    client_name: Option<String>,
}

/// Why a tool call produced no normal result.
enum ToolFailure {
    /// Tool execution error, reported with `isError: true` so the model can self-correct.
    Error(String),
    /// Protocol error (unknown tool, malformed arguments).
    Protocol(i64, String),
    /// The client cancelled the request: no response is sent.
    Cancelled,
}

impl From<String> for ToolFailure {
    fn from(s: String) -> Self {
        ToolFailure::Error(s)
    }
}

impl From<&str> for ToolFailure {
    fn from(s: &str) -> Self {
        ToolFailure::Error(s.to_string())
    }
}

type ToolResult = Result<Value, ToolFailure>;

/// An MCP server session. Feed it messages with [`Server::handle`] (no client round-trips) or
/// [`Server::handle_with`] (server → client requests such as elicitation go through `peer`).
pub struct Server {
    opts: McpOptions,
    session: Session,
}

fn tool_result(structured: Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": serde_json::to_string(&structured).unwrap_or_default() }],
        "structuredContent": structured,
        "isError": false,
    })
}

fn tool_error(msg: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

fn bytes_opt(m: &Measured<u64>) -> Option<u64> {
    m.value.filter(|_| m.quality.is_available())
}

/// Human amount in the configured units (`format.memory_units`, IEC by default) with one decimal — the
/// same rendering as the CLI (`oomtop headroom`) and the headline, so an agent that asked for `13G` reads
/// "13.0 GiB", exactly what a person running the CLI reads.
pub(crate) fn amount(u: UnitSystem, b: u64) -> String {
    format_bytes(b, u, 1)
}

/// The headline's name for a unit system.
fn units_name(u: UnitSystem) -> &'static str {
    match u {
        UnitSystem::Iec => "iec",
        UnitSystem::Si => "si",
    }
}

pub(crate) fn amount_signed(u: UnitSystem, v: i64) -> String {
    if v < 0 {
        format!("-{}", amount(u, v.unsigned_abs()))
    } else {
        amount(u, v as u64)
    }
}

/// Quotes a group id list for a shell command when needed.
pub(crate) fn reclaim_command(ids: &[String]) -> String {
    if ids.is_empty() {
        return "oomtop reclaim".into();
    }
    let joined = ids.join(",");
    let safe = joined
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-_.:,@+/".contains(&b));
    if safe {
        format!("oomtop reclaim --groups {joined}")
    } else {
        format!("oomtop reclaim --groups '{}'", joined.replace('\'', "'\\''"))
    }
}

/// Compact, privacy-safe group summary (no member list, no command lines).
pub(crate) fn group_summary(g: &Group, caller: &BTreeSet<String>) -> Value {
    json!({
        "id": g.id,
        "kind": g.kind.as_str(),
        "label": redact_text(&g.label),
        "root_pid": g.root.map(|r| r.pid),
        "process_count": if g.totals.process_count > 0 { g.totals.process_count as usize } else { g.members.len() },
        "footprint": g.totals.footprint,
        "resident": g.totals.resident,
        "gpu": g.totals.gpu,
        "swapped": g.totals.swapped,
        "cpu_pct": g.totals.cpu_pct,
        "reclaim_gain": g.reclaim_gain,
        "swap_gain": g.swap_gain,
        "idle": g.idle,
        "idle_for_s": g.idle_for_s,
        "orphan": g.orphan,
        "protected": g.protected,
        "is_self": g.is_self,
        "is_caller": caller.contains(&g.id),
        "lower_bound": g.lower_bound,
        "configured_mem": g.configured_mem,
        "owner_group": g.owner_group,
        "confidence": g.confidence,
        "matched_by": g.matched_by,
        "reclaim_candidate": is_reclaim_candidate(g) && !caller.contains(&g.id),
    })
}

fn object_args(args: &Value) -> Result<&Map<String, Value>, ToolFailure> {
    match args {
        Value::Object(m) => Ok(m),
        _ => Err(ToolFailure::Protocol(
            INVALID_PARAMS,
            "tool arguments must be an object".into(),
        )),
    }
}

fn reject_unknown(args: &Map<String, Value>, allowed: &[&str]) -> Result<(), ToolFailure> {
    match args.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(ToolFailure::Error(format!(
            "unknown argument {k:?} (allowed: {})",
            if allowed.is_empty() {
                "none".to_string()
            } else {
                allowed.join(", ")
            }
        ))),
        None => Ok(()),
    }
}

impl Server {
    pub fn new(opts: McpOptions) -> Self {
        Server {
            opts,
            session: Session::default(),
        }
    }

    /// Protocol version agreed in `initialize`, if any.
    pub fn negotiated_protocol(&self) -> Option<&str> {
        self.session.protocol.as_deref()
    }

    /// True when the client declared the elicitation capability under protocol 2025-06-18.
    pub fn client_supports_elicitation(&self) -> bool {
        self.session.client_elicitation && self.session.protocol.as_deref() == Some(PROTOCOL_VERSION)
    }

    /// `clientInfo.name` from `initialize`, if given.
    pub fn client_name(&self) -> Option<&str> {
        self.session.client_name.as_deref()
    }

    /// True once `notifications/initialized` was received.
    pub fn is_ready(&self) -> bool {
        self.session.ready
    }

    /// Handles one message without a client channel: server → client requests (elicitation) are
    /// unavailable, so `reclaim` returns the `oomtop reclaim` command. Returns the response, if any.
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        self.handle_with(msg, &mut NoPeer)
    }

    /// Handles one message; server → client requests go through `peer`. Returns the response (None for
    /// notifications, stray responses and cancelled requests).
    pub fn handle_with(&mut self, msg: &Value, peer: &mut dyn ClientPeer) -> Option<Value> {
        match classify(msg) {
            Incoming::Invalid { id, message } => Some(jsonrpc::err(&id, INVALID_REQUEST, &message)),
            Incoming::Response { .. } => None, // a response to nothing in flight: ignore
            Incoming::Notification { method, .. } => {
                if method == "notifications/initialized" && self.session.initialized {
                    self.session.ready = true;
                }
                // notifications/cancelled outside a round-trip: nothing is in flight (calls are synchronous).
                None
            }
            Incoming::Request { id, method, params } => self.request(&id, &method, &params, peer),
        }
    }

    fn request(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
        peer: &mut dyn ClientPeer,
    ) -> Option<Value> {
        match method {
            "initialize" => Some(self.initialize(id, params)),
            "ping" => Some(jsonrpc::ok(id, json!({}))),
            _ if !self.session.initialized => Some(jsonrpc::err(
                id,
                INVALID_REQUEST,
                "server not initialized: send \"initialize\" first",
            )),
            "tools/list" => Some(jsonrpc::ok(
                id,
                json!({ "tools": tool_definitions(self.opts.allow_actions) }),
            )),
            "tools/call" => self.tools_call(id, params, peer),
            _ => Some(jsonrpc::err(
                id,
                METHOD_NOT_FOUND,
                &format!("method not found: {method}"),
            )),
        }
    }

    fn initialize(&mut self, id: &Value, params: &Value) -> Value {
        if self.session.initialized {
            return jsonrpc::err(id, INVALID_REQUEST, "already initialized");
        }
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let protocol = match requested {
            Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v.to_string(),
            _ => PROTOCOL_VERSION.to_string(),
        };
        self.session = Session {
            initialized: true,
            ready: false,
            client_elicitation: params
                .pointer("/capabilities/elicitation")
                .map(Value::is_object)
                .unwrap_or(false),
            client_name: params
                .pointer("/clientInfo/name")
                .and_then(Value::as_str)
                .map(str::to_string),
            protocol: Some(protocol.clone()),
        };
        let mut instructions = String::from(
            "oomtop reports true memory use (footprint/PSS, GPU/Metal, compressed, swap) attributed to agent \
             sessions, apps, model servers, sandboxes and build daemons. Call can_fit before loading a large \
             model or starting heavy work; get_headroom for the current margin; explain_slowdown when things \
             are slow; suggest_reclaim lists idle memory hogs (no side effects). Answers are advisory and \
             valid for about 10 s.",
        );
        if self.opts.allow_actions {
            instructions.push_str(
                " reclaim stops groups only after the user confirms through elicitation; otherwise it returns \
                 the oomtop reclaim command for the user to run.",
            );
        }
        jsonrpc::ok(
            id,
            json!({
                "protocolVersion": protocol,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "oomtop", "title": "oomtop", "version": oomtop_core::VERSION },
                "instructions": instructions,
            }),
        )
    }

    fn tools_call(&mut self, id: &Value, params: &Value, peer: &mut dyn ClientPeer) -> Option<Value> {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return Some(jsonrpc::err(
                id,
                INVALID_PARAMS,
                "tools/call needs a string \"name\"",
            ));
        };
        if !tools::is_tool(name, self.opts.allow_actions) {
            return Some(jsonrpc::err(id, INVALID_PARAMS, &format!("Unknown tool: {name}")));
        }
        let args = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(a) => a.clone(),
        };
        let res = object_args(&args).and_then(|a| {
            let a = a.clone();
            self.call_tool(id, name, &a, peer)
        });
        match res {
            Ok(v) => Some(jsonrpc::ok(id, tool_result(v))),
            Err(ToolFailure::Error(e)) => Some(jsonrpc::ok(id, tool_error(&e))),
            Err(ToolFailure::Protocol(code, e)) => Some(jsonrpc::err(id, code, &e)),
            Err(ToolFailure::Cancelled) => None,
        }
    }

    fn call_tool(
        &mut self,
        id: &Value,
        name: &str,
        args: &Map<String, Value>,
        peer: &mut dyn ClientPeer,
    ) -> ToolResult {
        match name {
            tools::GET_HEADROOM => {
                reject_unknown(args, &[])?;
                self.get_headroom()
            }
            tools::CAN_FIT => {
                reject_unknown(args, &["bytes", "size", "model_path", "gpu_resident", "label"])?;
                self.can_fit(args)
            }
            tools::TOP_CONSUMERS => {
                reject_unknown(args, &["by", "n"])?;
                self.top_consumers(args)
            }
            tools::LIST_GROUPS => {
                reject_unknown(args, &["kind", "limit"])?;
                self.list_groups(args)
            }
            tools::LIST_MODEL_SERVERS => {
                reject_unknown(args, &[])?;
                self.list_model_servers()
            }
            tools::LIST_SANDBOXES => {
                reject_unknown(args, &[])?;
                self.list_sandboxes()
            }
            tools::EXPLAIN_SLOWDOWN => {
                reject_unknown(args, &[])?;
                self.explain_slowdown()
            }
            tools::SUGGEST_RECLAIM => {
                reject_unknown(args, &[])?;
                self.suggest_reclaim()
            }
            tools::RECLAIM if self.opts.allow_actions => {
                reject_unknown(args, &["group_ids"])?;
                self.reclaim(id, args, peer)
            }
            other => Err(ToolFailure::Protocol(
                INVALID_PARAMS,
                format!("Unknown tool: {other}"),
            )),
        }
    }

    // -----------------------------------------------------------------------------------------------------
    // sampling & caller identification
    // -----------------------------------------------------------------------------------------------------

    /// One fresh sample (the CLI's on-demand provider takes two samples 250 ms apart).
    fn sample(&mut self) -> Snapshot {
        self.opts.provider.snapshot()
    }

    /// Group ids that belong to the calling agent (the MCP process's parent chain) or to oomtop itself.
    /// These are never offered for reclaim and are refused by `reclaim`.
    pub fn caller_groups(&self, s: &Snapshot) -> BTreeSet<String> {
        let ctx = &self.opts.protect;
        let mut pids: BTreeSet<u32> = ctx.ancestor_pids.iter().copied().collect();
        if let Some(me) = ctx.self_pid.or(s.self_pid) {
            // Walk the parent chain inside the snapshot (bounded, cycle-safe).
            let mut seen = BTreeSet::new();
            let mut cur = Some(me);
            while let Some(p) = cur {
                if p <= 1 || seen.len() >= MAX_ANCESTRY || !seen.insert(p) {
                    break;
                }
                pids.insert(p);
                cur = s.process_by_pid(p).and_then(|x| x.ppid);
            }
        }
        pids.retain(|p| *p > 1);
        let mut out: BTreeSet<String> = s
            .groups
            .iter()
            .filter(|g| {
                g.is_self
                    || g.root.map(|r| pids.contains(&r.pid)).unwrap_or(false)
                    || g.members.iter().any(|m| pids.contains(&m.id.pid))
            })
            .map(|g| g.id.clone())
            .collect();
        if let Some(c) = &ctx.caller_group {
            out.insert(c.clone());
        }
        out
    }

    fn candidates(&self, s: &Snapshot, caller: &BTreeSet<String>) -> Vec<ReclaimCandidate> {
        let mut c = reclaim_candidates(s);
        c.retain(|x| !caller.contains(&x.group_id));
        c
    }

    fn headroom(&self, s: &Snapshot) -> Headroom {
        compute(s, &self.opts.provider.headroom_config())
    }

    // -----------------------------------------------------------------------------------------------------
    // read-only tools
    // -----------------------------------------------------------------------------------------------------

    fn get_headroom(&mut self) -> ToolResult {
        let s = export_snapshot(&self.sample());
        let h = self.headroom(&s);
        let mode = signals(&s, &h).desired();
        let mut hin = input_from(&s, &h, mode);
        hin.units = Some(units_name(self.opts.units).into());
        let summary = headroom_summary(&render(&hin).text, &h, self.opts.units);
        let m = &s.memory;
        Ok(json!({
            "schema_version": s.schema_version,
            "as_of_ms": s.taken_at_ms,
            "summary": summary,
            "mode": mode.as_str(),
            "headroom": h,
            "pressure": m.pressure,
            "swap": {
                "used": m.swap_used,
                "total": m.swap_total,
                "in_per_min": m.swap_in_per_min,
                "out_per_min": m.swap_out_per_min,
                "growing": h.swap_growing,
            },
            "forecast": s.oom.forecast,
            "oom_killer": s.oom.killer,
            "likely_victim": s.oom.likely_victim,
        }))
    }

    fn can_fit(&mut self, args: &Map<String, Value>) -> ToolResult {
        let given: Vec<&str> = ["bytes", "size", "model_path"]
            .into_iter()
            .filter(|k| args.get(*k).map(|v| !v.is_null()).unwrap_or(false))
            .collect();
        if given.len() != 1 {
            return Err(format!(
                "give exactly one of bytes, size or model_path (got {})",
                if given.is_empty() {
                    "none".to_string()
                } else {
                    given.join(", ")
                }
            )
            .into());
        }
        let source = given[0];
        let bytes = match source {
            "bytes" => args["bytes"]
                .as_u64()
                .ok_or("bytes must be a non-negative integer")?,
            "size" => {
                let s = args["size"]
                    .as_str()
                    .ok_or("size must be a string like \"13G\"")?;
                parse_bytes(s).map_err(|e| format!("size {s:?}: {e}"))?
            }
            _ => {
                let p = args["model_path"].as_str().ok_or("model_path must be a string")?;
                match &self.opts.model_estimator {
                    Some(est) => est(p).map_err(|e| format!("cannot estimate {p:?}: {e}"))?,
                    None => return Err("model_path estimates are not available in this build".into()),
                }
            }
        };
        let gpu_resident = match args.get("gpu_resident") {
            None | Some(Value::Null) => false,
            Some(v) => v.as_bool().ok_or("gpu_resident must be a boolean")?,
        };
        let label = match args.get("label") {
            None | Some(Value::Null) => None,
            Some(v) => Some(redact_text(v.as_str().ok_or("label must be a string")?)),
        };
        let label = label.or_else(|| match source {
            "model_path" => args["model_path"]
                .as_str()
                .map(|p| redact_text(p.rsplit('/').next().unwrap_or(p))),
            _ => None,
        });
        let need = Need {
            bytes,
            gpu_bytes: gpu_resident.then_some(bytes),
            label,
        };
        let s = export_snapshot(&self.sample());
        let caller = self.caller_groups(&s);
        let h = self.headroom(&s);
        let cands = self.candidates(&s, &caller);
        let mut a = can_fit(&need, &h, &cands);
        let u = self.opts.units;
        a.reason = reason_with(&a, &|b| amount(u, b));
        let exit = answer_exit_code(&a);
        let mut v = serde_json::to_value(&a).map_err(|e| e.to_string())?;
        let summary = can_fit_summary(&a, self.opts.units);
        if let Some(o) = v.as_object_mut() {
            o.insert("schema_version".into(), json!(s.schema_version));
            o.insert("as_of_ms".into(), json!(a.as_of_ms));
            o.insert("summary".into(), json!(summary));
            o.insert("need_source".into(), json!(source));
            o.insert("exit_code".into(), json!(exit));
            o.insert("advisory".into(), json!(true));
        }
        Ok(v)
    }

    fn top_consumers(&mut self, args: &Map<String, Value>) -> ToolResult {
        let by = match args.get("by") {
            None | Some(Value::Null) => "footprint",
            Some(v) => match v.as_str() {
                Some(b @ ("footprint" | "gpu" | "cpu")) => b,
                _ => return Err(format!("by must be footprint, gpu or cpu (got {v})").into()),
            },
        };
        let n = match args.get("n") {
            None | Some(Value::Null) => 10,
            Some(v) => match v.as_u64() {
                Some(n @ 1..=100) => n as usize,
                _ => return Err(format!("n must be an integer 1..=100 (got {v})").into()),
            },
        };
        let s = export_snapshot(&self.sample());
        let caller = self.caller_groups(&s);
        let mut g: Vec<&Group> = s.groups.iter().collect();
        match by {
            "gpu" => g.sort_by_key(|x| std::cmp::Reverse(bytes_opt(&x.totals.gpu).unwrap_or(0))),
            "cpu" => g.sort_by(|a, b| {
                let f = |x: &Group| x.totals.cpu_pct.value.unwrap_or(0.0);
                f(b).partial_cmp(&f(a)).unwrap_or(std::cmp::Ordering::Equal)
            }),
            _ => g.sort_by_key(|x| std::cmp::Reverse(bytes_opt(&x.totals.footprint).unwrap_or(0))),
        }
        if by == "gpu" {
            g.retain(|x| bytes_opt(&x.totals.gpu).unwrap_or(0) > 0);
        }
        g.truncate(n);
        let summary = match g.first() {
            None if by == "gpu" => "No group holds measurable GPU memory.".to_string(),
            None => "No groups attributed.".to_string(),
            Some(top) => {
                let what = match by {
                    "cpu" => format!("{:.0}% CPU", top.totals.cpu_pct.value.unwrap_or(0.0)),
                    "gpu" => format!(
                        "{} GPU",
                        amount(self.opts.units, bytes_opt(&top.totals.gpu).unwrap_or(0))
                    ),
                    _ => amount(self.opts.units, bytes_opt(&top.totals.footprint).unwrap_or(0)),
                };
                format!("Top by {by}: {} holds {what}.", redact_text(&top.label))
            }
        };
        Ok(json!({
            "schema_version": s.schema_version,
            "as_of_ms": s.taken_at_ms,
            "summary": summary,
            "by": by,
            "n": n,
            "memory_total": bytes_opt(&s.memory.total).or((s.host.mem_total > 0).then_some(s.host.mem_total)),
            "groups": g.iter().map(|x| group_summary(x, &caller)).collect::<Vec<_>>(),
        }))
    }

    fn list_groups(&mut self, args: &Map<String, Value>) -> ToolResult {
        let kind = match args.get("kind") {
            None | Some(Value::Null) => None,
            Some(v) => {
                let k = v.as_str().ok_or("kind must be a string")?;
                Some(GroupKind::parse(k).ok_or_else(|| {
                    format!("unknown kind {k:?} (agent, app, model, sandbox, daemon, system, other)")
                })?)
            }
        };
        let limit = match args.get("limit") {
            None | Some(Value::Null) => 50,
            Some(v) => match v.as_u64() {
                Some(n @ 1..=500) => n as usize,
                _ => return Err(format!("limit must be an integer 1..=500 (got {v})").into()),
            },
        };
        let s = export_snapshot(&self.sample());
        let caller = self.caller_groups(&s);
        let all: Vec<&Group> = s
            .groups
            .iter()
            .filter(|g| kind.map(|k| g.kind == k).unwrap_or(true))
            .collect();
        let total = all.len();
        let shown: Vec<Value> = all
            .iter()
            .take(limit)
            .map(|g| group_summary(g, &caller))
            .collect();
        let summary = format!(
            "{total} group{}{}{}.",
            if total == 1 { "" } else { "s" },
            kind.map(|k| format!(" of kind {}", k.as_str()))
                .unwrap_or_default(),
            if total > limit {
                format!(", showing the largest {limit}")
            } else {
                String::new()
            }
        );
        Ok(json!({
            "schema_version": s.schema_version,
            "as_of_ms": s.taken_at_ms,
            "summary": summary,
            "kind": kind.map(|k| k.as_str()),
            "total": total,
            "returned": shown.len(),
            "truncated": total > limit,
            "groups": shown,
        }))
    }

    fn list_model_servers(&mut self) -> ToolResult {
        let s = export_snapshot(&self.sample());
        let caller = self.caller_groups(&s);
        let servers: Vec<Value> = s
            .model_servers
            .iter()
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap_or_else(|_| json!({}));
                let group = m
                    .group_id
                    .as_deref()
                    .and_then(|id| s.group(id))
                    .map(|g| group_summary(g, &caller));
                if let Some(o) = v.as_object_mut() {
                    o.insert(
                        "pids".into(),
                        json!(m.pids.iter().map(|p| p.pid).collect::<Vec<_>>()),
                    );
                    o.insert("group".into(), group.unwrap_or(Value::Null));
                }
                v
            })
            .collect();
        let summary = if servers.is_empty() {
            "No local model servers detected.".to_string()
        } else {
            let names: Vec<String> = s
                .model_servers
                .iter()
                .map(|m| {
                    let mem = m
                        .group_id
                        .as_deref()
                        .and_then(|id| s.group(id))
                        .and_then(|g| bytes_opt(&g.totals.footprint))
                        .map(|b| format!(" ({})", amount(self.opts.units, b)))
                        .unwrap_or_default();
                    format!("{}{}", m.id, mem)
                })
                .collect();
            format!("{} model server(s): {}.", servers.len(), names.join(", "))
        };
        Ok(json!({
            "schema_version": s.schema_version,
            "as_of_ms": s.taken_at_ms,
            "summary": summary,
            "count": servers.len(),
            "model_servers": servers,
        }))
    }

    fn list_sandboxes(&mut self) -> ToolResult {
        let s = export_snapshot(&self.sample());
        let caller = self.caller_groups(&s);
        let boxes: Vec<Value> = s
            .sandboxes
            .iter()
            .map(|b| {
                let mut v = serde_json::to_value(b).unwrap_or_else(|_| json!({}));
                // Host cost: Σ footprint of the host processes backing the sandbox.
                let parts: Vec<Measured<u64>> = b
                    .host_pids
                    .iter()
                    .filter_map(|id| s.process(*id))
                    .map(|p| p.mem.footprint_or_pss.clone())
                    .collect();
                let mut cost = if parts.is_empty() {
                    Measured::unavailable("sandbox.host_pids", "host processes not found")
                } else {
                    oomtop_core::measured::sum_bytes(parts.iter(), "Σ host footprint")
                };
                if b.footprint_lower_bound {
                    cost = cost.into_estimate();
                }
                let group = s
                    .groups
                    .iter()
                    .find(|g| g.members.iter().any(|m| b.host_pids.contains(&m.id)))
                    .map(|g| group_summary(g, &caller));
                if let Some(o) = v.as_object_mut() {
                    o.insert(
                        "host_pids".into(),
                        json!(b.host_pids.iter().map(|p| p.pid).collect::<Vec<_>>()),
                    );
                    o.insert("label".into(), json!(redact_text(&b.label)));
                    o.insert("host_cost".into(), json!(cost));
                    o.insert("group".into(), group.unwrap_or(Value::Null));
                }
                v
            })
            .collect();
        let summary = if boxes.is_empty() {
            "No containers, VMs or sandboxes detected.".to_string()
        } else {
            format!("{} sandbox(es) running.", boxes.len())
        };
        Ok(json!({
            "schema_version": s.schema_version,
            "as_of_ms": s.taken_at_ms,
            "summary": summary,
            "count": boxes.len(),
            "sandboxes": boxes,
        }))
    }

    fn explain_slowdown(&mut self) -> ToolResult {
        let s = export_snapshot(&self.sample());
        let causes = explain(&s, self.opts.provider.history());
        let summary = match causes.first() {
            None => "Nothing notable: no memory pressure, swap storm or throttling detected.".to_string(),
            Some(c) => match &c.fix {
                Some(f) => format!("{} Fix: {f}", c.title),
                None => c.title.clone(),
            },
        };
        let th = &s.thermal;
        Ok(json!({
            "schema_version": s.schema_version,
            "as_of_ms": s.taken_at_ms,
            "summary": summary,
            "causes": causes,
            "thermal": {
                "pressure": th.pressure,
                "throttle_factor": th.throttle_factor,
                "low_power_mode": th.low_power_mode,
                "on_battery": th.on_battery,
                "battery_pct": th.battery_pct,
            },
            "cpu_total_pct": s.cpu.total_pct,
        }))
    }

    fn suggest_reclaim(&mut self) -> ToolResult {
        let s = export_snapshot(&self.sample());
        let caller = self.caller_groups(&s);
        let h = self.headroom(&s);
        let c = self.candidates(&s, &caller);
        let total: u64 = c.iter().map(|x| x.gain).sum();
        let total_swap: u64 = c.iter().filter_map(|x| x.swap_gain).sum();
        let ids: Vec<String> = c.iter().map(|x| x.group_id.clone()).collect();
        let summary = if c.is_empty() {
            "Nothing to reclaim: no idle build daemons, orphans or idle model servers.".to_string()
        } else {
            format!(
                "{} idle group{} could free ≈{} RAM{}.",
                c.len(),
                if c.len() == 1 { "" } else { "s" },
                amount(self.opts.units, total),
                h.headroom
                    .map(|x| format!(
                        " (headroom {} → {})",
                        amount_signed(self.opts.units, x),
                        amount_signed(self.opts.units, x + total as i64)
                    ))
                    .unwrap_or_default()
            )
        };
        let mut cands = serde_json::to_value(&c).map_err(|e| e.to_string())?;
        if let Some(arr) = cands.as_array_mut() {
            for v in arr {
                if let Some(l) = v.get("label").and_then(Value::as_str).map(redact_text) {
                    v["label"] = json!(l);
                }
            }
        }
        Ok(json!({
            "schema_version": s.schema_version,
            "as_of_ms": s.taken_at_ms,
            "summary": summary,
            "candidates": cands,
            "total_gain": total,
            "total_swap_gain": total_swap,
            "headroom": h.headroom,
            "headroom_after": h.headroom.map(|x| x + total as i64),
            "excluded": caller.iter().collect::<Vec<_>>(),
            "command": if ids.is_empty() { "oomtop reclaim --dry-run".to_string() } else { reclaim_command(&ids) },
            "side_effects": false,
        }))
    }

    // -----------------------------------------------------------------------------------------------------
    // reclaim (only with --allow-actions)
    // -----------------------------------------------------------------------------------------------------

    fn reclaim(
        &mut self,
        call_id: &Value,
        args: &Map<String, Value>,
        peer: &mut dyn ClientPeer,
    ) -> ToolResult {
        let raw_ids = args
            .get("group_ids")
            .and_then(Value::as_array)
            .ok_or("group_ids (array of group id strings) is required")?;
        if raw_ids.is_empty() || raw_ids.len() > 64 {
            return Err("group_ids must list 1..=64 group ids".into());
        }
        let mut ids: Vec<String> = Vec::new();
        for v in raw_ids {
            let id = v.as_str().ok_or("group_ids must be strings")?;
            if !ids.iter().any(|x| x == id) {
                ids.push(id.to_string());
            }
        }

        let s = self.sample();
        if let Some(a) = self.opts.actuator.as_mut() {
            a.observe(&s);
        }
        let caller = self.caller_groups(&s);
        let (plan, mut refused) = self.plan(&s, &ids, &caller);
        let h = self.headroom(&s);
        let expected: u64 = plan.targets.iter().filter_map(|t| t.expected_gain).sum();
        let target_ids: Vec<String> = plan.targets.iter().map(|t| t.group_id.clone()).collect();
        let command = (!target_ids.is_empty()).then(|| reclaim_command(&target_ids));
        let skeleton = json!({
            "schema_version": s.schema_version,
            "as_of_ms": s.taken_at_ms,
            "summary": "",
            "status": "nothing_to_do",
            "executed": false,
            "targets": targets_json(&plan, &s),
            "refused": [],
            "outcomes": [],
            "expected_gain": expected,
            "measured_gain": null,
            "available_before": bytes_opt(&s.memory.available),
            "available_after": null,
            "user_action": null,
            "command": command,
        });
        let base = |status: &str, executed: bool, summary: String, refused: &[Value]| {
            let mut v = skeleton.clone();
            v["status"] = json!(status);
            v["executed"] = json!(executed);
            v["summary"] = json!(summary);
            v["refused"] = json!(refused);
            v
        };

        if plan.targets.is_empty() {
            return Ok(base(
                "nothing_to_do",
                false,
                "Nothing stopped: none of the requested groups can be targeted (see refused).".into(),
                &refused,
            ));
        }
        let manual = |why: &str| {
            format!(
                "Nothing stopped: {why}. Ask the user to run `{}` (≈{} RAM).",
                command.clone().unwrap_or_default(),
                amount(self.opts.units, expected)
            )
        };
        if self.opts.actuator.is_none() {
            return Ok(base(
                "unavailable",
                false,
                manual("actions are not enabled in this oomtop"),
                &refused,
            ));
        }
        if !self.client_supports_elicitation() {
            return Ok(base(
                "needs_user",
                false,
                manual("the MCP client does not support elicitation, so oomtop cannot ask the user"),
                &refused,
            ));
        }

        let params = elicit::request_params(&plan, &s, h.headroom, self.opts.units);
        let answer = match peer.request(elicit::METHOD, params, call_id) {
            Ok(result) => elicit::parse_response(&result),
            Err(PeerError::Cancelled) => return Err(ToolFailure::Cancelled),
            Err(PeerError::Unsupported) => {
                return Ok(base(
                    "needs_user",
                    false,
                    manual("this transport cannot ask the user (no elicitation channel)"),
                    &refused,
                ))
            }
            Err(e) => {
                return Ok(base(
                    "not_confirmed",
                    false,
                    manual(&format!("the confirmation request failed ({e})")),
                    &refused,
                ))
            }
        };
        let mut result = match &answer {
            Answer::Accepted => Value::Null,
            Answer::Declined => base(
                "declined",
                false,
                "Nothing stopped: the user declined.".into(),
                &refused,
            ),
            Answer::Cancelled => base(
                "cancelled",
                false,
                "Nothing stopped: the user dismissed the request.".into(),
                &refused,
            ),
            Answer::NotConfirmed(why) => base("not_confirmed", false, manual(why), &refused),
        };
        if !result.is_null() {
            result["user_action"] = json!(answer.user_action());
            return Ok(result);
        }

        // Accepted. Re-verify identity and protection on a fresh sample right before acting: a target whose
        // root exited or was replaced (pid reuse) since the user saw the list is skipped, never re-planned.
        let now = self.sample();
        let caller_now = self.caller_groups(&now);
        let mut final_plan = ActionPlan::default();
        for t in &plan.targets {
            let same_root = now
                .group(&t.group_id)
                .map(|g| g.root == Some(t.root) && now.process(t.root).is_some())
                .unwrap_or(false);
            if !same_root {
                refused.push(json!({
                    "group_id": t.group_id,
                    "reason": "changed since confirmation (root exited or was replaced); not signalled",
                }));
                continue;
            }
            // The same checks as before asking, against the fresh sample (caller chain, protection).
            match self.plan_one(&now, &t.group_id, &caller_now) {
                Ok(mut t2) if t2.root == t.root => {
                    t2.expected_gain = t.expected_gain;
                    final_plan.targets.push(t2);
                }
                Ok(_) => refused.push(json!({
                    "group_id": t.group_id,
                    "reason": "changed since confirmation (root exited or was replaced); not signalled",
                })),
                Err(reason) => refused.push(json!({ "group_id": t.group_id, "reason": reason })),
            }
        }
        final_plan.confirmed = true;
        final_plan.confirmed_kill = false;
        let before = bytes_opt(&now.memory.available);
        let outcomes = match (&mut self.opts.actuator, final_plan.targets.is_empty()) {
            (Some(a), false) => a.execute(&final_plan),
            _ => Vec::new(),
        };

        // Re-measure ("freed 5.6 GB, est. 5.9"): re-sample until the signalled roots are gone (bounded).
        let signalled: Vec<_> = outcomes.iter().filter(|o| o.ok).map(|o| o.target.root).collect();
        let mut after = None;
        if !outcomes.is_empty() {
            for _ in 0..REMEASURE_SAMPLES {
                let snap = self.sample();
                let done = signalled.iter().all(|r| snap.process(*r).is_none());
                after = Some(snap);
                if done {
                    break;
                }
            }
        }
        let after_avail = after.as_ref().and_then(|a| bytes_opt(&a.memory.available));
        let measured = match (before, after_avail) {
            (Some(b), Some(a)) => Some(a.saturating_sub(b)),
            _ => None,
        };
        let outcomes_json: Vec<Value> = outcomes
            .iter()
            .map(|o| {
                json!({
                    "group_id": o.target.group_id,
                    "label": redact_text(&o.target.label),
                    "pid": o.target.root.pid,
                    "ok": o.ok,
                    "message": o.message,
                    "measured_gain": o.measured_gain,
                    "exited": after.as_ref().map(|a| a.process(o.target.root).is_none()).unwrap_or(false),
                })
            })
            .collect();
        let ok_count = outcomes.iter().filter(|o| o.ok).count();
        let status = if ok_count > 0 && ok_count == outcomes.len() && ok_count == plan.targets.len() {
            "done"
        } else if ok_count > 0 {
            "partial"
        } else if outcomes.is_empty() {
            // Confirmed, but every target changed or became protected before the signal.
            "nothing_to_do"
        } else {
            "failed"
        };
        let executed = ok_count > 0;
        let graceful_n = outcomes
            .iter()
            .filter(|o| o.ok && o.target.kind == ActionKind::Graceful)
            .count();
        let verb = match (graceful_n, ok_count - graceful_n.min(ok_count)) {
            (0, _) => format!(
                "Sent SIGTERM to {ok_count} group root{}",
                if ok_count == 1 { "" } else { "s" }
            ),
            (g, 0) => format!(
                "Unloaded models gracefully in {g} group{}",
                if g == 1 { "" } else { "s" }
            ),
            (g, t) => format!(
                "Unloaded {g} group{} gracefully and sent SIGTERM to {t}",
                if g == 1 { "" } else { "s" }
            ),
        };
        let summary = if executed {
            format!(
                "{verb}; {} (est. {}).",
                measured
                    .map(|m| format!("freed {}", amount(self.opts.units, m)))
                    .unwrap_or_else(|| "gain not measurable yet".into()),
                amount(self.opts.units, expected)
            )
        } else if outcomes.is_empty() {
            "Nothing stopped: every target changed or was refused after confirmation.".to_string()
        } else {
            let why: Vec<&str> = outcomes.iter().map(|o| o.message.as_str()).collect();
            format!("Nothing stopped: signalling failed ({}).", why.join("; "))
        };
        let mut r = base(status, executed, summary, &refused);
        r["user_action"] = json!("accept");
        r["refused"] = json!(refused);
        r["outcomes"] = json!(outcomes_json);
        r["measured_gain"] = json!(measured);
        r["available_before"] = json!(before);
        r["available_after"] = json!(after_avail);
        r["command"] = Value::Null;
        Ok(r)
    }

    /// Plans SIGTERM for each id, refusing the caller's session, oomtop itself and protected groups.
    fn plan(&self, s: &Snapshot, ids: &[String], caller: &BTreeSet<String>) -> (ActionPlan, Vec<Value>) {
        let mut plan = ActionPlan::default();
        let mut refused = Vec::new();
        for id in ids {
            match self.plan_one(s, id, caller) {
                Ok(t) => plan.targets.push(t),
                Err(reason) => refused.push(json!({ "group_id": id, "reason": reason })),
            }
        }
        (plan, refused)
    }

    /// Plans SIGTERM for one group root, or says (redacted) why not. Besides the group-level checks of
    /// `plan_group` (self, protected/system, no root, ancestors), the root **process** must be in the sample
    /// and must not be protected itself (another user's process, pid ≤ 1, built-in and configured
    /// protected names — SPEC §13).
    fn plan_one(&self, s: &Snapshot, id: &str, caller: &BTreeSet<String>) -> Result<ActionTarget, String> {
        let Some(g) = s.group(id) else {
            return Err(Refusal::NotFound(id.to_string()).to_string());
        };
        let label = redact_text(&g.label);
        let name = if label.is_empty() {
            id.to_string()
        } else {
            label.clone()
        };
        if caller.contains(id) {
            return Err(if g.is_self {
                Refusal::SelfGroup(label)
            } else {
                Refusal::CallerSession(label)
            }
            .to_string());
        }
        // Model servers with an adapter action are unloaded gently first (SPEC §13), never signalled.
        let gentle = self
            .opts
            .actuator
            .as_ref()
            .map(|a| a.graceful_for(id))
            .unwrap_or_default();
        let kind = if gentle.is_empty() {
            ActionKind::Terminate
        } else {
            ActionKind::Graceful
        };
        let mut t = plan_group(g, kind, &self.opts.protect).map_err(|r| {
            match r {
                Refusal::NotFound(_) => Refusal::NotFound(id.to_string()),
                Refusal::Protected(_) => Refusal::Protected(label.clone()),
                Refusal::NoRoot(_) => Refusal::NoRoot(label.clone()),
                Refusal::CallerSession(_) => Refusal::CallerSession(label.clone()),
                Refusal::SelfGroup(_) => Refusal::SelfGroup(label.clone()),
            }
            .to_string()
        })?;
        let Some(root) = s.process(t.root) else {
            return Err(format!(
                "{label}: root process {} is not in the current sample; not signalled",
                t.root.pid
            ));
        };
        if is_protected_process(root, s, &self.opts.protect) {
            return Err(Refusal::Protected(format!("{label} (root pid {})", t.root.pid)).to_string());
        }
        // Only what `oomtop reclaim` would offer: idle build daemons, orphans and idle model servers. An agent
        // must not be able to stop an active app or another agent's live session through MCP (SPEC §12.3).
        if !reclaim_candidates(s).iter().any(|c| c.group_id == id) {
            return Err(format!(
                "{name}: not a reclaim candidate (not idle, orphaned or an idle model server); \
                 MCP only stops what `oomtop reclaim` offers — the user can stop it from the TUI"
            ));
        }
        // SPEC §10: never interrupt a running job silently — a busy server is refused through MCP (an unload
        // would interrupt the job just like a signal).
        if let Some(ms) = s.model_servers.iter().find(|m| m.group_id.as_deref() == Some(id)) {
            if let oomtop_adapters::StopGuard::Busy(why) = oomtop_adapters::stop_guard(ms) {
                return Err(format!(
                    "{name}: busy ({}); not stopped while a job runs",
                    redact_text(&why)
                ));
            }
        }
        t.label = label;
        if kind == ActionKind::Graceful {
            t.graceful = Some(redact_text(&gentle.join("; ")));
        }
        Ok(t)
    }
}

/// "Idle not confirmed" note for a model-server target whose busy state is only a heuristic (SPEC §10).
pub(crate) fn guard_note(s: &Snapshot, group_id: &str) -> Option<String> {
    let ms = s
        .model_servers
        .iter()
        .find(|m| m.group_id.as_deref() == Some(group_id))?;
    match oomtop_adapters::stop_guard(ms) {
        oomtop_adapters::StopGuard::Unknown(why) => {
            Some(format!("idle not confirmed: {}", redact_text(&why)))
        }
        oomtop_adapters::StopGuard::Busy(why) => Some(format!("busy: {}", redact_text(&why))),
        oomtop_adapters::StopGuard::Allowed => None,
    }
}

/// One-line answer for `can_fit`, in the headline's units (the core `reason` stays alongside it).
/// `get_headroom`'s summary: the UX headline ("All good — 4.8 GiB free", i.e. available now), followed by the
/// headroom `can_fit` and `oomtop headroom` decide with (available minus the safety margin), so an agent
/// reading only the summary does not plan against memory the margin keeps back.
fn headroom_summary(headline: &str, h: &Headroom, u: UnitSystem) -> String {
    let mut s = headline.trim_end().to_string();
    if !s.is_empty() && !s.ends_with(['.', '!', '?']) {
        s.push('.');
    }
    let avail = h.available_now.value.map(|v| amount(u, v));
    let margin = amount(u, h.safety_margin);
    let tail = match (h.headroom, avail) {
        (Some(x), Some(a)) if x >= 0 => format!(
            "Headroom {} ({a} available minus {margin} safety margin).",
            amount(u, x as u64)
        ),
        (Some(x), Some(a)) => format!(
            "No headroom: {} below the {margin} safety margin ({a} available).",
            amount(u, x.unsigned_abs())
        ),
        _ => "Headroom unavailable: available memory could not be measured.".to_string(),
    };
    if s.is_empty() {
        tail
    } else {
        format!("{s} {tail}")
    }
}

fn can_fit_summary(a: &oomtop_core::can_fit::CanFitAnswer, u: UnitSystem) -> String {
    use oomtop_core::can_fit::Fit;
    if let Some(e) = &a.error {
        return format!("Cannot answer: {e}");
    }
    let need = amount(u, a.host_need.max(a.need.bytes));
    let what = a
        .need
        .label
        .as_deref()
        .filter(|l| !l.is_empty())
        .map(|l| format!(" for {l}"))
        .unwrap_or_default();
    let room = a
        .headroom
        .map(|h| format!(" (headroom {})", amount_signed(u, h)))
        .unwrap_or_default();
    match &a.fit {
        Fit::Yes => format!("Yes: {need}{what} fits now{room}."),
        Fit::YesAfterReclaim { reclaim, gain } => {
            let names: Vec<&str> = reclaim
                .iter()
                .map(|c| {
                    if c.label.is_empty() {
                        c.group_id.as_str()
                    } else {
                        c.label.as_str()
                    }
                })
                .collect();
            format!(
                "Yes after reclaim: {need}{what} fits if {} {} stopped (frees ≈{}){room}.",
                names.join(", "),
                if names.len() == 1 { "is" } else { "are" },
                amount(u, *gain)
            )
        }
        Fit::No { .. } => {
            // Same sentence as `oomtop headroom` (the core's reason in these units), plus the headroom.
            let reason = reason_with(a, &|b| amount(u, b));
            format!("No: {need}{what} does not fit — {reason}{room}.")
        }
    }
}

fn targets_json(plan: &ActionPlan, s: &Snapshot) -> Vec<Value> {
    plan.targets
        .iter()
        .map(|t: &ActionTarget| {
            let g = s.group(&t.group_id);
            json!({
                "group_id": t.group_id,
                "label": redact_text(&t.label),
                "kind": g.map(|g| g.kind.as_str()).unwrap_or("other"),
                "pid": t.root.pid,
                "signal": if t.kind == ActionKind::Graceful { "none" } else { "SIGTERM" },
                "action": if t.kind == ActionKind::Graceful { "graceful" } else { "sigterm" },
                "graceful": t.graceful,
                "note": guard_note(s, &t.group_id),
                "expected_gain": t.expected_gain,
                "idle": g.map(|g| g.idle).unwrap_or(false),
                "idle_for_s": g.and_then(|g| g.idle_for_s),
                "orphan": g.map(|g| g.orphan).unwrap_or(false),
                "process_count": g.map(|g| if g.totals.process_count > 0 { g.totals.process_count as usize } else { g.members.len() }).unwrap_or(0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::actions::ProtectContext;
    use oomtop_core::provider::StaticProvider;
    use oomtop_core::{Member, ProcId, Process};

    fn snap() -> Snapshot {
        let p = |pid: u32, ppid: u32| Process {
            id: ProcId::new(pid, 1),
            ppid: Some(ppid),
            ..Default::default()
        };
        let g = |id: &str, root: u32, members: &[u32]| Group {
            id: id.into(),
            root: Some(ProcId::new(root, 1)),
            members: members
                .iter()
                .map(|m| Member {
                    id: ProcId::new(*m, 1),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        Snapshot {
            self_pid: Some(30),
            // terminal 10 → agent 20 → mcp 30; unrelated daemon 40; a ppid cycle 50 ↔ 51.
            processes: vec![p(10, 1), p(20, 10), p(30, 20), p(40, 1), p(50, 51), p(51, 50)],
            groups: vec![
                g("app:terminal", 10, &[10]),
                g("agent:me", 20, &[20, 30]),
                Group {
                    kind: oomtop_core::GroupKind::BuildDaemon,
                    idle: true,
                    reclaim_gain: oomtop_core::Measured::estimate(1 << 30, "test"),
                    ..g("daemon:x", 40, &[40])
                },
                g("other:cycle", 50, &[50, 51]),
            ],
            ..Default::default()
        }
    }

    fn server(protect: ProtectContext) -> Server {
        Server::new(McpOptions {
            provider: Box::new(StaticProvider::new(snap())),
            actuator: None,
            allow_actions: true,
            protect,
            model_estimator: None,
            units: Default::default(),
        })
    }

    #[test]
    fn caller_groups_follow_the_parent_chain_in_the_snapshot() {
        // No ancestors from the CLI: the chain is walked from snapshot.self_pid.
        let sv = server(ProtectContext::default());
        let c = sv.caller_groups(&snap());
        assert_eq!(c.into_iter().collect::<Vec<_>>(), ["agent:me", "app:terminal"]);
        // A ppid cycle terminates.
        let mut s = snap();
        s.self_pid = Some(50);
        let c = sv.caller_groups(&s);
        assert!(c.contains("other:cycle") && !c.contains("daemon:x"));
        // Explicit caller_group is always included.
        let sv = server(ProtectContext {
            caller_group: Some("daemon:x".into()),
            ..Default::default()
        });
        assert!(sv.caller_groups(&snap()).contains("daemon:x"));
    }

    #[test]
    fn reclaim_commands_are_shell_safe() {
        assert_eq!(reclaim_command(&[]), "oomtop reclaim");
        assert_eq!(
            reclaim_command(&["daemon:gradle:5".into(), "app:google-chrome".into()]),
            "oomtop reclaim --groups daemon:gradle:5,app:google-chrome"
        );
        assert_eq!(
            reclaim_command(&["app:it's $(bad)".into()]),
            "oomtop reclaim --groups 'app:it'\\''s $(bad)'"
        );
    }

    #[test]
    fn handle_without_a_peer_never_acts() {
        let mut sv = server(ProtectContext::default());
        let init = sv
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-06-18","capabilities":{"elicitation":{}},"clientInfo":{"name":"t"}}}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(sv.client_supports_elicitation());
        assert_eq!(sv.client_name(), Some("t"));
        assert!(!sv.is_ready());
        assert!(sv
            .handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
        assert!(sv.is_ready());
        let r = sv
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"reclaim","arguments":{"group_ids":["daemon:x","agent:me"]}}}))
            .unwrap();
        let sc = &r["result"]["structuredContent"];
        // No actuator configured → unavailable; the caller's own session is refused either way.
        assert_eq!(sc["status"], "unavailable", "{sc}");
        assert_eq!(sc["refused"][0]["group_id"], "agent:me");
        assert_eq!(sc["command"], "oomtop reclaim --groups daemon:x");
    }
}

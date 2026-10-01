//! Machine interfaces: `json`, `ndjson`, `serve` (HTTP + Prometheus) and `mcp` (stdio). Every snapshot that
//! leaves the process is redacted (SPEC §13); environments are never part of a snapshot.

use super::{out, Ctx};
use crate::actuator::SignalActuator;
use crate::engine::Engine;
use anyhow::Result;
use oomtop_core::actions::Actuator;
use oomtop_core::model_estimate::KvParams;
use oomtop_core::provider::SnapshotProvider;
use oomtop_core::redact::redact_snapshot;
use oomtop_core::Snapshot;
use std::io::Write;
use std::time::{Duration, Instant};

/// Minimum ndjson interval (the process tier's own cadence floor).
pub const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Redacted unless the user turned `privacy.redact_exports` off for local stdout exports.
fn exported(ctx: &Ctx, s: &Snapshot) -> Snapshot {
    if ctx.config().privacy.redact_exports {
        redact_snapshot(s)
    } else {
        s.clone()
    }
}

pub fn json(ctx: &Ctx, compact: bool) -> Result<i32> {
    let mut e = ctx.engine(true)?;
    let s = exported(ctx, &e.snapshot());
    let text = if compact {
        serde_json::to_string(&s)? + "\n"
    } else {
        serde_json::to_string_pretty(&s)? + "\n"
    };
    out(&text);
    Ok(0)
}

/// `discard`: sample and enrich exactly like the TUI but serialize nothing (the perf job's steady-state probe).
pub fn ndjson(ctx: &Ctx, interval: Duration, count: Option<u64>, discard: bool) -> Result<i32> {
    let iv = interval.max(MIN_INTERVAL);
    let mut e = ctx.engine(false)?;
    let stdout = std::io::stdout();
    let mut n = 0u64;
    loop {
        let started = Instant::now();
        let snap = e.snapshot();
        if !discard {
            let line = serde_json::to_string(&exported(ctx, &snap))?;
            let mut o = stdout.lock();
            if writeln!(o, "{line}").and_then(|_| o.flush()).is_err() {
                return Ok(0); // reader went away (`| head`)
            }
        }
        n += 1;
        if count.is_some_and(|c| n >= c) {
            return Ok(0);
        }
        std::thread::sleep(iv.saturating_sub(started.elapsed()));
    }
}

pub fn serve(ctx: &Ctx, listen: Option<String>, max_requests: Option<usize>) -> Result<i32> {
    let cfg = ctx.config();
    let listen = listen.unwrap_or_else(|| cfg.serve.listen.clone());
    let e = ctx.engine(false)?;
    oomtop_serve::serve_http(oomtop_serve::ServeOptions {
        listen,
        provider: Box::new(e),
        refresh: Duration::from_millis(cfg.general.refresh_ms.max(500)),
        max_requests,
        on_ready: Some(Box::new(|addr| {
            eprintln!(
                "oomtop: serving http://{addr}/ (/api/snapshot /api/headroom /api/why /metrics /healthz)"
            );
        })),
    })?;
    Ok(0)
}

pub fn mcp(ctx: &Ctx, allow_actions: bool) -> Result<i32> {
    let allow = allow_actions || ctx.config().mcp.allow_actions;
    let mut e = ctx.engine(true)?;
    // First sample: fills the protect context (oomtop's ancestry = the hosting agent) and finds the caller's
    // session group, which is never offered for reclaim.
    let s = e.snapshot();
    let mut protect = e.protect.clone();
    protect.caller_group = Engine::self_group(&s);
    let est: oomtop_mcp::ModelEstimator = Box::new(|p: &str| {
        oomtop_adapters::estimate_model_file(std::path::Path::new(p), &KvParams::default())
            .map(|e| e.need)
            .map_err(|e| e.to_string())
    });
    let actuator: Option<Box<dyn Actuator>> = if allow && e.is_live() {
        let mut a = SignalActuator::new();
        a.register_gentle(&s);
        Some(Box::new(a))
    } else {
        None
    };
    oomtop_mcp::serve_stdio(oomtop_mcp::McpOptions {
        provider: Box::new(e),
        actuator,
        allow_actions: allow,
        protect,
        model_estimator: Some(est),
        units: ctx.units(),
    })?;
    Ok(0)
}

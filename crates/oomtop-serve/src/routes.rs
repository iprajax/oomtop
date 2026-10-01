//! Request routing (pure given a snapshot). All JSON bodies are versioned (`schema_version`) and every
//! snapshot, headline and cause list is built from the export view ([`crate::export_snapshot`], SPEC §13).

use crate::export::export_snapshot;
use crate::metrics::prometheus_text;
use oomtop_core::headline::{input_from, render};
use oomtop_core::headroom::{compute, HeadroomConfig};
use oomtop_core::history::History;
use oomtop_core::modes::signals;
use oomtop_core::why::{explain, Cause};
use oomtop_core::Snapshot;
use serde_json::json;

pub const JSON: &str = "application/json; charset=utf-8";
pub const TEXT: &str = "text/plain; charset=utf-8";
pub const PROMETHEUS: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Endpoint paths (the `/api/*` names are canonical; `/snapshot`, `/headroom`, `/why` are aliases).
pub const ENDPOINTS: [&str; 6] = [
    "/api/snapshot",
    "/api/headroom",
    "/api/why",
    "/metrics",
    "/healthz",
    "/api",
];

fn index_text() -> String {
    "oomtop serve — see the OOM coming.\n\n\
     GET /api/snapshot   full snapshot (redacted, schema_version 1)\n\
     GET /api/headroom   headroom, pressure, swap trend, OOM forecast\n\
     GET /api/why        ranked causes of slowness / memory pressure\n\
     GET /metrics        Prometheus text format\n\
     GET /healthz        liveness (503 when sampling is stale)\n"
        .to_string()
}

fn json_body(v: &serde_json::Value) -> String {
    let mut s = serde_json::to_string(v).unwrap_or_else(|_| "{}".into());
    s.push('\n');
    s
}

/// `/api/headroom` body. Redacts internally: the headline names groups.
pub fn headroom_json(s: &Snapshot, hcfg: &HeadroomConfig) -> serde_json::Value {
    let s = &export_snapshot(s);
    let h = compute(s, hcfg);
    let mode = signals(s, &h).desired();
    let m = &s.memory;
    json!({
        "schema_version": s.schema_version,
        "as_of_ms": s.taken_at_ms,
        "summary": render(&input_from(s, &h, mode)).text,
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
    })
}

/// `/api/why` body.
pub fn why_json(s: &Snapshot, causes: &[Cause]) -> serde_json::Value {
    let summary = match causes.first() {
        None => "Nothing notable: no memory pressure, swap storm or throttling detected.".to_string(),
        Some(c) => match &c.fix {
            Some(f) => format!("{} Fix: {f}", c.title),
            None => c.title.clone(),
        },
    };
    let t = &s.thermal;
    json!({
        "schema_version": s.schema_version,
        "as_of_ms": s.taken_at_ms,
        "summary": summary,
        "causes": causes,
        "thermal": {
            "pressure": t.pressure,
            "throttle_factor": t.throttle_factor,
            "low_power_mode": t.low_power_mode,
            "on_battery": t.on_battery,
            "battery_pct": t.battery_pct,
        },
        "cpu_total_pct": s.cpu.total_pct,
    })
}

/// Routes one GET request (pure given the snapshot): (status, content type, body). `/api/why` uses an
/// empty history here; [`route_with`] takes the causes computed with the sampler's history.
pub fn route(path: &str, s: &Snapshot, hcfg: &HeadroomConfig) -> (u16, &'static str, String) {
    route_with(path, s, hcfg, None)
}

/// Like [`route`], with pre-computed `why` causes (from the provider's history) when available. The causes
/// must be computed from [`crate::export_snapshot`] (their evidence names groups); `serve_http` does.
pub fn route_with(
    path: &str,
    s: &Snapshot,
    hcfg: &HeadroomConfig,
    causes: Option<&[Cause]>,
) -> (u16, &'static str, String) {
    let p = path.split(['?', '#']).next().unwrap_or("");
    let p = if p.len() > 1 { p.trim_end_matches('/') } else { p };
    match p {
        "/" => (200, TEXT, index_text()),
        "/healthz" => (200, TEXT, "ok\n".into()),
        "/api" => (
            200,
            JSON,
            json_body(&json!({ "schema_version": s.schema_version, "endpoints": ENDPOINTS })),
        ),
        "/api/snapshot" | "/snapshot" => (
            200,
            JSON,
            serde_json::to_string(&export_snapshot(s)).unwrap_or_default() + "\n",
        ),
        "/api/headroom" | "/headroom" => (200, JSON, json_body(&headroom_json(s, hcfg))),
        "/api/why" | "/why" => {
            let owned;
            let c = match causes {
                Some(c) => c,
                None => {
                    owned = explain(&export_snapshot(s), &History::default());
                    &owned
                }
            };
            (200, JSON, json_body(&why_json(s, c)))
        }
        "/metrics" => (200, PROMETHEUS, prometheus_text(s, &compute(s, hcfg))),
        _ => (
            404,
            TEXT,
            "not found: try /api/snapshot, /api/headroom, /api/why, /metrics, /healthz\n".into(),
        ),
    }
}

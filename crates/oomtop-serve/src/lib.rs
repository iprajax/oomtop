//! # oomtop-serve
//!
//! HTTP JSON + Prometheus `/metrics` (SPEC §12.2). Binds `127.0.0.1:9469` unless `--listen` says otherwise
//! (SPEC §13). A background thread samples on the refresh cadence; requests read the latest snapshot, and
//! every snapshot leaves redacted.
//!
//! Endpoints (GET/HEAD): `/api/snapshot` (versioned, redacted), `/api/headroom`, `/api/why`, `/metrics`
//! (Prometheus text: host series plus per-group series labeled by `kind` and `label`; no per-pid series),
//! `/healthz` (503 when sampling has stalled), `/api` (index). `/snapshot`, `/headroom` and `/why` are
//! aliases kept from M0.
//!
//! Hardening: on a loopback bind, requests whose `Host` header is not a loopback name are refused (DNS
//! rebinding); no CORS headers are sent; responses are `Cache-Control: no-store`.

mod export;
mod metrics;
mod routes;

pub use export::export_snapshot;
pub use metrics::{escape_label, prometheus_text, MAX_GROUP_SERIES, OTHER_LABEL};
pub use routes::{headroom_json, route, route_with, why_json, ENDPOINTS};

use oomtop_core::provider::SnapshotProvider;
use oomtop_core::why::{explain, Cause};
use oomtop_core::Snapshot;
use std::net::IpAddr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:9469";
/// Fastest sampling cadence accepted.
pub const MIN_REFRESH: Duration = Duration::from_millis(250);

#[derive(Debug, Error)]
pub enum ServeError {
    #[error("cannot listen on {addr}: {msg}")]
    Bind { addr: String, msg: String },
}

pub struct ServeOptions {
    pub listen: String,
    pub provider: Box<dyn SnapshotProvider>,
    pub refresh: Duration,
    /// Stop after this many requests (tests); `None` = forever.
    pub max_requests: Option<usize>,
    /// Called with the bound address once listening (tests use port 0).
    pub on_ready: Option<Box<dyn FnOnce(std::net::SocketAddr) + Send>>,
}

struct Latest {
    snap: Snapshot,
    causes: Vec<Cause>,
    at: Instant,
}

/// True when a `Host` header names this machine's loopback (`localhost`, `127.0.0.0/8`, `::1`), any port.
pub fn host_is_loopback(host: &str) -> bool {
    let h = host.trim();
    let name = if let Some(rest) = h.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else if h.matches(':').count() > 1 {
        h // bare IPv6 without port
    } else {
        h.rsplit_once(':').map(|(a, _)| a).unwrap_or(h)
    };
    let name = name.trim_end_matches('.');
    name.eq_ignore_ascii_case("localhost")
        || name.parse::<IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false)
}

/// How long without a fresh sample before `/healthz` reports 503.
fn stale_after(refresh: Duration) -> Duration {
    (refresh * 3).max(Duration::from_secs(10))
}

fn header(name: &str, value: &str) -> Option<tiny_http::Header> {
    tiny_http::Header::from_bytes(name.as_bytes(), value.as_bytes()).ok()
}

/// Serves until `max_requests` (if set) is reached.
pub fn serve_http(opts: ServeOptions) -> Result<(), ServeError> {
    let server = tiny_http::Server::http(&opts.listen).map_err(|e| ServeError::Bind {
        addr: opts.listen.clone(),
        msg: e.to_string(),
    })?;
    let bound = server.server_addr().to_ip();
    let loopback = bound.map(|a| a.ip().is_loopback()).unwrap_or(false);
    if !loopback {
        eprintln!(
            "oomtop serve: warning: listening on {} (not loopback) — anyone who can reach it can read \
             memory and process data",
            opts.listen
        );
    }

    let mut provider = opts.provider;
    let hcfg = provider.headroom_config();
    let refresh = opts.refresh.max(MIN_REFRESH);
    // The shared state only ever holds the export view: every endpoint reads redacted data.
    let first = export_snapshot(&provider.snapshot());
    let causes = explain(&first, provider.history());
    let latest = Arc::new(Mutex::new(Arc::new(Latest {
        snap: first,
        causes,
        at: Instant::now(),
    })));
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let l2 = latest.clone();
    let sampler = std::thread::Builder::new()
        .name("oomtop-serve-sampler".into())
        .spawn(move || {
            // Sleep one refresh; any message or a dropped sender (server done) stops the sampler.
            while let Err(RecvTimeoutError::Timeout) = stop_rx.recv_timeout(refresh) {
                let snap = export_snapshot(&provider.snapshot());
                let causes = explain(&snap, provider.history());
                let next = Arc::new(Latest {
                    snap,
                    causes,
                    at: Instant::now(),
                });
                *l2.lock().unwrap_or_else(|e| e.into_inner()) = next;
            }
        })
        .ok();

    if let (Some(cb), Some(addr)) = (opts.on_ready, bound) {
        cb(addr);
    }

    for (i, req) in server.incoming_requests().enumerate() {
        let cur = latest.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let host_ok = !loopback
            || req
                .headers()
                .iter()
                .find(|h| h.field.equiv("Host"))
                .map(|h| host_is_loopback(h.value.as_str()))
                .unwrap_or(true);
        let method_ok = matches!(req.method(), tiny_http::Method::Get | tiny_http::Method::Head);
        let (code, ctype, body): (u16, &str, String) = if !method_ok {
            (405, routes::TEXT, "GET or HEAD only\n".into())
        } else if !host_ok {
            (
                403,
                routes::TEXT,
                "forbidden: Host must be a loopback name\n".into(),
            )
        } else {
            let path = req.url().to_string();
            let is_health = path.split('?').next() == Some("/healthz");
            let age = cur.at.elapsed();
            if is_health && age > stale_after(refresh) {
                (
                    503,
                    routes::TEXT,
                    format!("stale: last sample {} s ago\n", age.as_secs()),
                )
            } else {
                route_with(&path, &cur.snap, &hcfg, Some(&cur.causes))
            }
        };
        let mut resp = tiny_http::Response::from_string(body).with_status_code(code);
        for h in [
            header("Content-Type", ctype),
            header("Cache-Control", "no-store"),
            header("X-Content-Type-Options", "nosniff"),
            (code == 405).then(|| header("Allow", "GET, HEAD")).flatten(),
        ]
        .into_iter()
        .flatten()
        {
            resp = resp.with_header(h);
        }
        let _ = req.respond(resp);
        if opts.max_requests.map(|m| i + 1 >= m).unwrap_or(false) {
            break;
        }
    }
    drop(stop_tx);
    if let Some(s) = sampler {
        let _ = s.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for h in [
            "localhost",
            "localhost:9469",
            "LOCALHOST.",
            "127.0.0.1:9469",
            "127.1.2.3",
            "[::1]:9469",
            "::1",
        ] {
            assert!(host_is_loopback(h), "{h}");
        }
        for h in [
            "evil.example",
            "evil.example:9469",
            "10.0.0.2:9469",
            "[fe80::1]",
            "",
            "localhost.evil.example",
        ] {
            assert!(!host_is_loopback(h), "{h}");
        }
    }

    #[test]
    fn staleness_window() {
        assert_eq!(stale_after(Duration::from_secs(2)), Duration::from_secs(10));
        assert_eq!(stale_after(Duration::from_secs(5)), Duration::from_secs(15));
    }
}

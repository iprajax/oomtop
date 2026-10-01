//! Regression tests for the spec audit: every endpoint (pure router and live server) exports only the
//! redacted view, and unmeasured swap is not exported as a 0 trend.

mod support;

use oomtop_core::headroom::{compute, HeadroomConfig};
use oomtop_core::provider::StaticProvider;
use oomtop_core::{LoadedModel, ModelServer, ProcId, Snapshot, Victim};
use oomtop_serve::{export_snapshot, prometheus_text, route, serve_http, ServeOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use support::*;

const SECRET: &str = "sk-ant-api03-verysecretvalue1234567890abcdef";

/// The serve fixture with command-line-derived labels that carry a secret.
fn leaky() -> Snapshot {
    let mut s = fixture();
    for g in &mut s.groups {
        g.label = format!("{} --token={SECRET}", g.label);
    }
    s.oom.likely_victim = Some(Victim {
        id: ProcId::new(400, 1),
        name: format!("sd-server {SECRET}"),
        ..Default::default()
    });
    s.model_servers.push(ModelServer {
        id: "sd-server:7861".into(),
        models: vec![LoadedModel {
            name: format!("https://u:{SECRET}@hf.example/qwen"),
            ..Default::default()
        }],
        ..Default::default()
    });
    s
}

const PATHS: [&str; 7] = [
    "/api/snapshot",
    "/api/headroom",
    "/api/why",
    "/metrics",
    "/snapshot",
    "/headroom",
    "/why",
];

#[test]
fn pure_router_never_exports_secret_labels() {
    let s = leaky();
    for p in PATHS {
        let (code, _, body) = route(p, &s, &HeadroomConfig::default());
        assert_eq!(code, 200, "{p}");
        assert!(!body.contains(SECRET), "{p} leaked a secret:\n{body}");
    }
    // Labels stay readable.
    let (_, _, body) = route("/api/snapshot", &s, &HeadroomConfig::default());
    assert!(body.contains("GradleDaemon --token=<redacted>"), "{body}");
    // The export view is idempotent and keeps ids and numbers.
    let e = export_snapshot(&s);
    assert_eq!(export_snapshot(&e), e);
    assert_eq!(
        e.groups.iter().map(|g| &g.id).collect::<Vec<_>>(),
        s.groups.iter().map(|g| &g.id).collect::<Vec<_>>()
    );
}

#[test]
fn live_server_never_exports_secret_labels() {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        serve_http(ServeOptions {
            listen: "127.0.0.1:0".into(),
            provider: Box::new(StaticProvider::new(leaky())),
            refresh: Duration::from_millis(250),
            max_requests: Some(PATHS.len()),
            on_ready: Some(Box::new(move |a| tx.send(a).unwrap())),
        })
    });
    let addr = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    for p in PATHS {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "GET {p} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 200"), "{p}: {out}");
        assert!(!out.contains(SECRET), "{p} leaked a secret:\n{out}");
    }
    h.join().unwrap().unwrap();
}

#[test]
fn unmeasured_swap_has_no_trend_series() {
    let mut s = fixture();
    let text = prometheus_text(&s, &compute(&s, &HeadroomConfig::default()));
    assert!(text.contains("\noomtop_swap_growing 0\n"), "{text}");
    s.memory.swap_used = oomtop_core::Measured::unavailable("vm.swapusage", "sysctl failed");
    let text = prometheus_text(&s, &compute(&s, &HeadroomConfig::default()));
    assert!(!text.contains("oomtop_swap_growing"), "{text}");
    check_prometheus(&text);
}

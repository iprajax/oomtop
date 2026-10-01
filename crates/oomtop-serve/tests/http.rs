//! End-to-end HTTP tests against a real listener on 127.0.0.1 (port 0).

mod support;

use oomtop_core::history::History;
use oomtop_core::provider::{SnapshotProvider, StaticProvider};
use oomtop_core::Snapshot;
use oomtop_serve::{serve_http, ServeOptions, DEFAULT_LISTEN};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

fn start(
    provider: Box<dyn SnapshotProvider>,
    max: usize,
) -> (
    SocketAddr,
    std::thread::JoinHandle<Result<(), oomtop_serve::ServeError>>,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    let h = std::thread::spawn(move || {
        serve_http(ServeOptions {
            listen: "127.0.0.1:0".into(),
            provider,
            refresh: Duration::from_millis(250),
            max_requests: Some(max),
            on_ready: Some(Box::new(move |a| tx.send(a).unwrap())),
        })
    });
    (rx.recv_timeout(Duration::from_secs(5)).unwrap(), h)
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

fn get(a: &ureq::Agent, addr: SocketAddr, path: &str) -> (u16, String, String) {
    let mut r = a.get(&format!("http://{addr}{path}")).call().unwrap();
    let ct = r
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    (r.status().as_u16(), ct, r.body_mut().read_to_string().unwrap())
}

/// Raw HTTP/1.1 request (lets the test choose method and Host header).
fn raw(addr: SocketAddr, method: &str, path: &str, host: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

#[test]
fn default_listen_is_loopback() {
    assert_eq!(DEFAULT_LISTEN, "127.0.0.1:9469");
}

#[test]
fn api_endpoints_are_versioned_redacted_json() {
    let (addr, h) = start(Box::new(StaticProvider::new(fixture())), 9);
    let a = agent();

    let (code, ct, body) = get(&a, addr, "/api/snapshot");
    assert_eq!(code, 200);
    assert!(ct.starts_with("application/json"), "{ct}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["groups"].as_array().unwrap().len(), 5);
    assert!(body.contains("<redacted>") && !body.contains("s3cr3t"));
    assert_eq!(v["memory"]["available"]["source"], "vm_statistics64");

    let (code, _, body) = get(&a, addr, "/api/headroom");
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["headroom"]["available_now"]["value"], 10 * GIB);
    assert_eq!(v["swap"]["used"]["value"], GIB);
    assert_eq!(v["forecast"]["eta_s"], 360);
    assert!(v["summary"].as_str().unwrap().len() > 5);

    let (code, _, body) = get(&a, addr, "/api/why");
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert!(v["causes"].is_array());
    assert!(v["summary"].is_string());

    let (code, ct, body) = get(&a, addr, "/metrics");
    assert_eq!(code, 200);
    assert!(ct.starts_with("text/plain; version=0.0.4"), "{ct}");
    check_prometheus(&body);
    assert!(body.contains("oomtop_headroom_bytes"));

    let (code, _, body) = get(&a, addr, "/healthz");
    assert_eq!((code, body.as_str()), (200, "ok\n"));

    // Aliases from M0 and trailing slashes.
    assert_eq!(get(&a, addr, "/snapshot").0, 200);
    assert_eq!(get(&a, addr, "/api/headroom/?x=1").0, 200);
    let (code, _, body) = get(&a, addr, "/api");
    assert_eq!(code, 200);
    assert!(body.contains("/api/why"));

    assert_eq!(get(&a, addr, "/nope").0, 404);
    h.join().unwrap().unwrap();
}

#[test]
fn methods_and_host_header_are_enforced() {
    let (addr, h) = start(Box::new(StaticProvider::new(fixture())), 5);
    let r = raw(addr, "POST", "/api/snapshot", "127.0.0.1");
    assert!(r.starts_with("HTTP/1.1 405"), "{r}");
    assert!(r.to_ascii_lowercase().contains("allow: get, head"), "{r}");
    // DNS rebinding: a foreign Host on a loopback bind is refused.
    let r = raw(addr, "GET", "/api/snapshot", "evil.example:9469");
    assert!(r.starts_with("HTTP/1.1 403"), "{r}");
    assert!(!r.contains("sd-server"));
    let r = raw(addr, "GET", "/healthz", &format!("localhost:{}", addr.port()));
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    assert!(r.to_ascii_lowercase().contains("cache-control: no-store"), "{r}");
    assert!(!r.to_ascii_lowercase().contains("access-control-allow-origin"));
    // HEAD: headers only.
    let r = raw(addr, "HEAD", "/metrics", "127.0.0.1");
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    assert!(!r.contains("# HELP"), "HEAD has no body: {r}");
    let r = raw(addr, "GET", "/metrics", "[::1]");
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    h.join().unwrap().unwrap();
}

/// Counts samples; the served snapshot changes as the sampler refreshes.
struct Counting {
    n: Arc<Mutex<u64>>,
    history: History,
}

impl SnapshotProvider for Counting {
    fn snapshot(&mut self) -> Snapshot {
        let mut n = self.n.lock().unwrap();
        *n += 1;
        let mut s = fixture();
        s.taken_at_ms = *n;
        self.history.push_snapshot(&s);
        s
    }
    fn history(&self) -> &History {
        &self.history
    }
}

#[test]
fn sampler_refreshes_in_the_background_and_stops() {
    let n = Arc::new(Mutex::new(0));
    let (addr, h) = start(
        Box::new(Counting {
            n: n.clone(),
            history: History::default(),
        }),
        2,
    );
    let a = agent();
    let first: Value = serde_json::from_str(&get(&a, addr, "/api/headroom").2).unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let later: Value = serde_json::from_str(&get(&a, addr, "/api/headroom").2).unwrap();
    assert!(
        later["as_of_ms"].as_u64() > first["as_of_ms"].as_u64(),
        "{first} vs {later}"
    );
    h.join().unwrap().unwrap();
    // After the server returns, the sampler is stopped: no more samples.
    let after = *n.lock().unwrap();
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(*n.lock().unwrap(), after, "sampler thread stopped");
}

#[test]
fn bind_errors_are_reported() {
    let err = serve_http(ServeOptions {
        listen: "256.0.0.1:1".into(),
        provider: Box::new(StaticProvider::new(Snapshot::default())),
        refresh: Duration::from_secs(1),
        max_requests: Some(1),
        on_ready: None,
    })
    .unwrap_err();
    assert!(err.to_string().contains("256.0.0.1:1"), "{err}");
}

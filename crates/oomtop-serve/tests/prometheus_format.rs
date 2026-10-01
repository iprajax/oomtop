//! Prometheus exposition: format validity, content, cardinality bounds and a golden file.

mod support;

use oomtop_core::headroom::{compute, HeadroomConfig};
use oomtop_core::{GroupKind, Snapshot};
use oomtop_serve::{prometheus_text, MAX_GROUP_SERIES, OTHER_LABEL};
use support::*;

fn metrics(s: &Snapshot) -> String {
    prometheus_text(s, &compute(s, &HeadroomConfig::default()))
}

#[test]
fn exposition_is_valid_and_complete() {
    let s = fixture();
    let text = metrics(&s);
    let m = check_prometheus(&text);
    let get = |name: &str, labels: &str| {
        *m.get(&(name.to_string(), labels.to_string()))
            .unwrap_or_else(|| panic!("missing {name}{{{labels}}}\n{text}"))
    };
    assert_eq!(get("oomtop_memory_total_bytes", ""), (24 * GIB) as f64);
    assert_eq!(get("oomtop_memory_available_bytes", ""), (10 * GIB) as f64);
    assert_eq!(get("oomtop_memory_pressure_level", ""), 1.0);
    assert_eq!(get("oomtop_swap_used_bytes", ""), GIB as f64);
    assert!(get("oomtop_headroom_bytes", "") > 0.0);
    assert_eq!(
        get(
            "oomtop_oom_eta_seconds",
            r#"target="swap_exhaustion",killer="jetsam""#
        ),
        360.0
    );
    assert_eq!(
        get("oomtop_gpu_budget_bytes", r#"gpu="gpu0",name="Apple M5""#),
        (16 * GIB) as f64
    );
    assert_eq!(
        get(
            "oomtop_source_status",
            r#"source="macos.ioreport",status="unavailable""#
        ),
        1.0
    );
    // Two GradleDaemon groups aggregate into one (kind, label) series.
    let gd = r#"kind="build_daemon",label="GradleDaemon""#;
    assert_eq!(get("oomtop_group_footprint_bytes", gd), (6 * GIB) as f64);
    assert_eq!(get("oomtop_group_count", gd), 2.0);
    assert_eq!(get("oomtop_group_idle_count", gd), 2.0);
    assert_eq!(get("oomtop_group_processes", gd), 2.0);
    assert_eq!(
        get(
            "oomtop_group_footprint_bytes",
            r#"kind="model_server",label="sd-server""#
        ),
        (10 * GIB) as f64
    );
    // Label escaping (quote, backslash, newline).
    assert_eq!(
        get(
            "oomtop_group_footprint_bytes",
            r#"kind="app",label="we\"ird\\app\nx""#
        ),
        GIB as f64
    );
    // Unavailable values are omitted, not zero.
    assert!(!text.contains("oomtop_memory_wired_bytes"));
    // No per-pid or per-process series, no command lines.
    assert!(!text.contains("pid=") && !text.contains("s3cr3t"));
}

#[test]
fn empty_snapshot_is_still_valid() {
    let text = metrics(&Snapshot::default());
    let m = check_prometheus(&text);
    assert!(m.contains_key(&(
        "oomtop_info".to_string(),
        m.keys().find(|k| k.0 == "oomtop_info").unwrap().1.clone()
    )));
    assert!(
        !text.contains("oomtop_memory_total_bytes"),
        "unknown total is omitted"
    );
}

#[test]
fn group_series_are_bounded() {
    let mut s = fixture();
    let template = s.groups[3].clone();
    for i in 0..300u64 {
        let mut g = template.clone();
        g.id = format!("app:bulk{i}");
        g.label = format!("bulk app {i}");
        g.kind = if i % 2 == 0 {
            GroupKind::App
        } else {
            GroupKind::Other
        };
        g.totals.footprint.value = Some(1_000_000 + i);
        s.groups.push(g);
    }
    let text = metrics(&s);
    let m = check_prometheus(&text);
    let fp: Vec<&(String, String)> = m
        .keys()
        .filter(|k| k.0 == "oomtop_group_footprint_bytes")
        .collect();
    assert!(
        fp.len() <= MAX_GROUP_SERIES + GroupKind::ALL.len(),
        "{} series",
        fp.len()
    );
    // Nothing is lost: the folded remainder keeps the total.
    let total: f64 = m
        .iter()
        .filter(|(k, _)| k.0 == "oomtop_group_footprint_bytes")
        .map(|(_, v)| v)
        .sum();
    let expected: u64 = s.groups.iter().map(|g| g.totals.footprint.value.unwrap()).sum();
    assert_eq!(total, expected as f64);
    assert!(fp.iter().any(|k| k.1.contains(OTHER_LABEL)));
    // The biggest groups keep their own series.
    assert!(fp.iter().any(|k| k.1.contains(r#"label="sd-server""#)));
}

#[test]
fn golden_metrics() {
    // The build version is filtered so the golden file survives version bumps.
    let text = metrics(&fixture()).replace(
        &format!("version=\"{}\"", oomtop_core::VERSION),
        "version=\"[version]\"",
    );
    insta::assert_snapshot!("metrics_fixture", text);
}

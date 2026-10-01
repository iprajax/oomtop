//! Shared fixture for the serve tests and a Prometheus text-format (0.0.4) checker.
#![allow(dead_code)]

use oomtop_core::{
    Accelerator, Forecast, ForecastTarget, GpuVendor, Group, GroupKind, GroupTotals, Measured, Member,
    OomKiller, PressureLevel, ProcId, Process, Snapshot, SourceStatus,
};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};

pub const GIB: u64 = 1 << 30;

fn group(id: &str, kind: GroupKind, label: &str, pid: u32, footprint: u64) -> Group {
    Group {
        id: id.into(),
        kind,
        label: label.into(),
        root: Some(ProcId::new(pid, 1)),
        members: vec![Member {
            id: ProcId::new(pid, 1),
            ..Default::default()
        }],
        totals: GroupTotals {
            footprint: Measured::exact(footprint, "Σ footprint"),
            resident: Measured::exact(footprint, "Σ resident"),
            cpu_pct: Measured::exact(1.5, "Σ cpu"),
            process_count: 1,
            ..Default::default()
        },
        reclaim_gain: Measured::estimate(footprint / 2, "private resident"),
        idle: kind == GroupKind::BuildDaemon,
        idle_for_s: (kind == GroupKind::BuildDaemon).then_some(7200),
        ..Default::default()
    }
}

pub fn fixture() -> Snapshot {
    let mut s = Snapshot {
        taken_at_ms: 1_790_000_000_000,
        ..Default::default()
    };
    s.host.os = oomtop_core::OsKind::Macos;
    s.host.arch = "aarch64".into();
    s.host.mem_total = 24 * GIB;
    s.memory.total = Measured::exact(24 * GIB, "hw.memsize");
    s.memory.available = Measured::exact(10 * GIB, "vm_statistics64");
    s.memory.compressed = Measured::exact(2 * GIB, "vm_statistics64");
    s.memory.swap_used = Measured::exact(GIB, "vm.swapusage");
    s.memory.swap_total = Measured::exact(4 * GIB, "vm.swapusage");
    // Unavailable values must be omitted, never exported as 0.
    s.memory.wired = Measured::unavailable("vm_statistics64", "not collected");
    s.memory.pressure = Measured::exact(PressureLevel::Warn, "memorystatus");
    s.oom.forecast = Some(Forecast {
        target: ForecastTarget::SwapExhaustion,
        killer: Some(OomKiller::Jetsam),
        eta_s: 360,
        confidence: 0.91,
        rate_per_min: 512.0 * 1024.0 * 1024.0,
        window_s: 300,
    });
    s.accelerators = vec![Accelerator {
        id: "gpu0".into(),
        vendor: GpuVendor::Apple,
        name: "Apple M5".into(),
        unified: true,
        mem_used: Measured::exact(10 * GIB, "ioreg"),
        gpu_budget: Measured::exact(16 * GIB, "recommendedMaxWorkingSetSize"),
        ..Default::default()
    }];
    s.source_status
        .insert("macos.procs".into(), SourceStatus::Available);
    s.source_status.insert(
        "macos.ioreport".into(),
        SourceStatus::Unavailable("not built".into()),
    );
    s.processes = vec![Process {
        id: ProcId::new(400, 1),
        name: "sd-server".into(),
        cmdline: vec!["sd-server".into(), "--api-key".into(), "s3cr3t".into()],
        ..Default::default()
    }];
    s.groups = vec![
        group("model:sd", GroupKind::ModelServer, "sd-server", 400, 10 * GIB),
        group("daemon:g1", GroupKind::BuildDaemon, "GradleDaemon", 300, 3 * GIB),
        group("daemon:g2", GroupKind::BuildDaemon, "GradleDaemon", 301, 3 * GIB),
        group("app:weird", GroupKind::App, "we\"ird\\app\nx", 500, GIB),
        group("agent:a", GroupKind::AgentSession, "Claude Code", 100, GIB / 2),
    ];
    s
}

/// Checks Prometheus text exposition 0.0.4 rules; returns the parsed samples `(name, labels) → value`.
pub fn check_prometheus(text: &str) -> BTreeMap<(String, String), f64> {
    assert!(text.ends_with('\n'), "exposition must end with a newline");
    let name_re = Regex::new(r"^[a-zA-Z_:][a-zA-Z0-9_:]*$").unwrap();
    let sample_re = Regex::new(
        r#"^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{((?:[a-zA-Z_][a-zA-Z0-9_]*="(?:[^"\\\n]|\\["\\n])*",?)*)\})? (-?[0-9.e+]+|NaN|\+Inf|-Inf)$"#,
    )
    .unwrap();
    let label_re = Regex::new(r#"([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\\n]|\\["\\n])*)""#).unwrap();
    let mut typed: BTreeSet<String> = BTreeSet::new();
    let mut helped: BTreeSet<String> = BTreeSet::new();
    let mut done: BTreeSet<String> = BTreeSet::new(); // families already closed (samples must be contiguous)
    let mut current: Option<String> = None;
    let mut samples = BTreeMap::new();
    for line in text.lines() {
        assert!(!line.is_empty(), "no blank lines");
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let name = rest.split(' ').next().unwrap();
            assert!(name_re.is_match(name), "bad name {name}");
            assert!(helped.insert(name.to_string()), "HELP twice for {name}");
            continue;
        }
        if let Some(rest) = line.strip_prefix("# TYPE ") {
            let mut it = rest.split(' ');
            let name = it.next().unwrap().to_string();
            let ty = it.next().unwrap();
            assert!(
                ["gauge", "counter", "untyped", "histogram", "summary"].contains(&ty),
                "bad type {ty}"
            );
            assert!(typed.insert(name.clone()), "TYPE twice for {name}");
            assert!(!done.contains(&name), "family {name} split");
            if let Some(c) = current.replace(name) {
                done.insert(c);
            }
            continue;
        }
        assert!(!line.starts_with('#'), "unknown comment {line}");
        let caps = sample_re
            .captures(line)
            .unwrap_or_else(|| panic!("malformed sample line: {line:?}"));
        let name = caps[1].to_string();
        assert!(typed.contains(&name), "sample before TYPE: {line}");
        assert_eq!(
            current.as_deref(),
            Some(name.as_str()),
            "samples not contiguous: {line}"
        );
        let lbls = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let mut keys = BTreeSet::new();
        for l in label_re.captures_iter(lbls) {
            assert!(
                keys.insert(l[1].to_string()),
                "duplicate label {} in {line}",
                &l[1]
            );
            assert_ne!(&l[1], "pid", "no per-pid series: {line}");
        }
        let value: f64 = match &caps[4] {
            "NaN" => f64::NAN,
            "+Inf" => f64::INFINITY,
            "-Inf" => f64::NEG_INFINITY,
            v => v.parse().unwrap_or_else(|_| panic!("bad value {v}")),
        };
        let key = (name, lbls.to_string());
        assert!(
            samples.insert(key.clone(), value).is_none(),
            "duplicate series {key:?}"
        );
    }
    samples
}

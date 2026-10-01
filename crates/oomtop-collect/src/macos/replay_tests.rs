//! Replay tests over the recorded macOS fixtures (SPEC §17): the pure decoders (incl. [`super::extras`])
//! applied to `fixtures/macos/*.json`, no hardware involved.

use super::extras::{self, decode_extras, decode_host_full, status_keys};
use crate::raw::{names, RawPayload};
use crate::replay::{load_fixture, replay, Fixture};
use oomtop_core::{GpuVendor, OomKiller, Quality, SourceStatus};
use std::path::PathBuf;

fn fixture(name: &str) -> Fixture {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/macos")
        .join(name);
    load_fixture(&p).unwrap_or_else(|e| panic!("{e}"))
}

fn last_host(f: &Fixture) -> (&crate::raw::MacHostRaw, u64) {
    f.frames
        .last()
        .and_then(|fr| {
            fr.iter().find_map(|r| match &r.payload {
                RawPayload::MacHost(h) if r.source == names::MACOS_HOST => Some((h, r.taken_at_ms)),
                _ => None,
            })
        })
        .expect("host sample in last frame")
}

#[test]
fn baseline_replays_to_a_full_snapshot() {
    let f = fixture("m5-air-baseline.json");
    assert_eq!(f.meta.os, "macos");
    assert!(f.frames.len() >= 3);
    let snaps = replay(&f);
    let s = snaps.last().unwrap();
    assert_eq!(s.host.mem_total, 24 << 30);
    assert_eq!(s.host.model.as_deref(), Some("Mac17,3"));
    assert_eq!(s.host.hostname, "<host>");
    assert!(s.processes.len() > 300, "{}", s.processes.len());
    assert!(s.memory.available.value.is_some());
    assert!(s.cpu.total_pct.value.is_some(), "second frame gives CPU %");
    // Same-user processes have exact footprints; other users' are unavailable, never 0.
    let exact = s
        .processes
        .iter()
        .filter(|p| p.mem.footprint_or_pss.quality == Quality::Exact)
        .count();
    assert!(exact > 100);
    for p in &s.processes {
        if p.mem.footprint_or_pss.value.is_none() {
            assert!(matches!(p.mem.footprint_or_pss.quality, Quality::Unavailable(_)));
        }
    }
    // Environments are never in fixtures: only hashed markers.
    let text = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/macos/m5-air-baseline.json"),
    )
    .unwrap();
    for needle in ["HOME=", "PATH=/", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
        assert!(!text.contains(needle), "fixture contains {needle}");
    }
}

#[test]
fn baseline_extras_decode() {
    let f = fixture("m5-air-baseline.json");
    let (h, at) = last_host(&f);
    let x = decode_extras(h, at);
    let t = &x.thermal;
    assert!(t.pressure.value.is_some(), "{:?}", t.pressure);
    assert_eq!(t.pressure.quality, Quality::Exact);
    assert!(t.low_power_mode.value.is_some());
    assert!(t.on_battery.value.is_some());
    assert!(t.battery_pct.value.is_some());
    // IOReport: E/P clusters + GPU with the pmgr frequency tables.
    let names: Vec<&str> = t.clusters.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["E-cluster", "P-cluster", "gpu"], "{:?}", t.clusters);
    for c in &t.clusters {
        assert!(c.max_mhz > 0.0 && (0.0..=100.0).contains(&c.active_pct), "{c:?}");
        assert!(c.cur_mhz <= c.max_mhz + 1e-6, "{c:?}");
    }
    assert!(t
        .package_power_w
        .value
        .map(|w| (0.0..200.0).contains(&w))
        .unwrap_or(false));
    assert_eq!(x.status[status_keys::IOREPORT], SourceStatus::Available);
    // Host GPU memory in use from IOAccelerator.
    assert!(x.gpu.mem_used.value.is_some());
    assert_eq!(x.gpu.name.as_deref(), Some("Apple M5 GPU (10-core)"));
    assert_eq!(x.fanless, Some(true));
    // The JetsamEvent of 2026-09-29 17:13 (+0530) is < 24 h before the capture.
    assert!(x
        .recent_kills
        .iter()
        .any(|k| k.killer == OomKiller::Jetsam && k.victim_name.as_deref() == Some("spotlightknowledged")));

    let part = decode_host_full(names::MACOS_HOST, h, None, None, at);
    let acc = part
        .accelerators
        .as_ref()
        .and_then(|a| a.iter().find(|a| a.vendor == GpuVendor::Apple))
        .expect("apple gpu");
    assert!(acc.mem_used.value.is_some());
    assert!(acc.gpu_budget.value.is_some());
    assert_eq!(part.host.as_ref().unwrap().fanless, Some(true));
    assert!(part.thermal.is_some());
}

#[test]
fn early_frames_explain_missing_ioreport() {
    let f = fixture("m5-air-baseline.json");
    let host0 = f.frames[0]
        .iter()
        .find_map(|r| match &r.payload {
            RawPayload::MacHost(h) => Some(h),
            _ => None,
        })
        .unwrap();
    let x = decode_extras(host0, 0);
    assert!(x.thermal.clusters.is_empty());
    assert!(matches!(
        &x.status[status_keys::IOREPORT],
        SourceStatus::Unavailable(r) if r == "initializing" || r == "needs two samples"
    ));
    assert!(x.thermal.throttle_factor.value.is_none());
}

/// The M0 fixture predates the extras: everything decodes to `unavailable`, nothing panics or reads 0.
#[test]
fn old_fixture_without_extras_is_unavailable() {
    let f = fixture("m5-air-agents.json");
    let snaps = replay(&f);
    assert!(!snaps.is_empty());
    let (h, at) = last_host(&f);
    let x = decode_extras(h, at);
    assert!(x.thermal.pressure.value.is_none());
    assert!(x.thermal.clusters.is_empty());
    assert!(x.gpu.mem_used.value.is_none());
    assert!(x.recent_kills.is_empty());
    assert!(h
        .sysctl_str
        .keys()
        .all(|k| !k.starts_with(extras::keys::JETSAM_PREFIX)));
    assert!(matches!(
        x.status[status_keys::THERMAL],
        SourceStatus::Unavailable(_)
    ));
}

/// Fixtures recorded before per-core ticks existed still load (serde default): no per-core values, task
/// counts derived from the replayed process list. Injecting ticks into the recorded host samples yields
/// per-core % from the second frame on.
#[test]
fn per_core_and_tasks_replay() {
    let mut f = fixture("m5-air-baseline.json");
    let s = replay(&f).pop().unwrap();
    assert!(s.cpu.per_core_pct.is_empty());
    assert_eq!(s.cpu.tasks.processes as usize, s.processes.len());
    assert!(s.cpu.tasks.threads.unwrap_or(0) > 0);

    let mut n = 0u64;
    for frame in f.frames.iter_mut() {
        for r in frame.iter_mut() {
            if let RawPayload::MacHost(h) = &mut r.payload {
                n += 1;
                // Core 0 busy 50 %, core 1 idle.
                h.per_cpu_ticks = vec![[n * 50, 0, n * 50, 0], [0, 0, n * 100, 0]];
                h.core_clusters = vec!["E".into(), "P".into()];
            }
        }
    }
    let s = replay(&f).pop().unwrap();
    assert_eq!(s.cpu.per_core_pct.len(), 2, "{:?}", s.cpu.per_core_pct);
    assert!((s.cpu.per_core_pct[0] - 50.0).abs() < 1e-9);
    assert_eq!(s.cpu.per_core_pct[1], 0.0);
    assert_eq!(
        s.cpu.core_kinds,
        vec![
            oomtop_core::CoreKind::Efficiency,
            oomtop_core::CoreKind::Performance
        ]
    );
}

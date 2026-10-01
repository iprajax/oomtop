//! Golden tests for OOM forecast decisions (SPEC §8.3, §17 acceptance #7: with swap growing steadily the
//! forecast appears with an ETA within ±30 % of actual, and never on a single spike).

mod memory_common;

use memory_common::Lcg;
use oomtop_core::forecast::*;
use oomtop_core::history::{History, HistoryPoint};
use oomtop_core::units::{format_duration, GIB, MIB};
use oomtop_core::*;
use serde::Serialize;

const STEP_S: u64 = 5;

fn settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    s.set_snapshot_path("memory_snapshots");
    s.set_prepend_module_to_snapshot(false);
    s
}

/// Triangle wave in [-1, 1] with the given period (no libm, so goldens are bit-stable across platforms).
fn tri(t: f64, period: f64) -> f64 {
    let x = (t / period).fract();
    if x < 0.5 {
        4.0 * x - 1.0
    } else {
        3.0 - 4.0 * x
    }
}

fn linux_oom() -> Oom {
    oom_setup(&OomInputs {
        os: OsKind::Linux,
        ..Default::default()
    })
}

#[derive(Serialize)]
struct Row {
    t: String,
    forecast: String,
    actual: String,
    err_pct: Option<f64>,
}

/// Acceptance #7 on one OS: swap grows steadily until it hits the killer's ceiling; returns the golden.
/// Linux: fixed 8 GiB swap (`swap_total`). macOS: swap files are added on demand (`swap_total` tracks use),
/// and the ceiling is `swap_limit` = swap used + free space on the swap volume (8 GiB here: a nearly full
/// disk), which jetsam can't exceed.
fn acceptance_7(os: OsKind) -> serde_json::Value {
    let total = 8 * GIB;
    let mut noise = Lcg(7);
    let mut h = History::default();
    let oom = oom_setup(&OomInputs {
        os,
        ..Default::default()
    });
    let expected_killer = if os == OsKind::Macos {
        OomKiller::Jetsam
    } else {
        OomKiller::Kernel
    };
    // Ground truth: 2 GiB used, growing 150 MiB/min ± 15 % (slow triangle wave), plus ±16 MiB sample noise.
    let mut truth = 2.0 * GIB as f64;
    let mut series = Vec::new();
    let mut t = 0u64;
    let crossing = loop {
        let rate_per_s = 150.0 * MIB as f64 / 60.0 * (1.0 + 0.15 * tri(t as f64, 600.0));
        if truth >= total as f64 {
            break t;
        }
        series.push((t, truth));
        truth += rate_per_s * STEP_S as f64;
        t += STEP_S;
    };
    assert!(crossing > 35 * 60, "crossing at {crossing}s");

    let mut rows = Vec::new();
    let mut first_forecast: Option<u64> = None;
    let mut max_err: f64 = 0.0;
    let mut n_forecasts = 0;
    for (t, y) in &series {
        let observed = (y + noise.next_unit() * 16.0 * MIB as f64).max(0.0) as u64;
        let (swap_total, swap_limit) = match os {
            // dynamic swap: the next whole GiB above use; the limit is the disk.
            OsKind::Macos => ((observed / GIB + 1) * GIB, Some(total)),
            _ => (total, None),
        };
        h.push(HistoryPoint {
            t_ms: t * 1000,
            swap_used: Some(observed),
            swap_total: Some(swap_total),
            swap_limit,
            available: Some((1.5 * GIB as f64 + noise.next_unit() * 30.0 * MIB as f64) as u64),
            psi_some_avg10: (os == OsKind::Linux).then(|| 2.0 + noise.next_unit()),
            ..Default::default()
        });
        if !t.is_multiple_of(30) {
            continue;
        }
        let actual = (crossing - t) as f64;
        let f = forecast_oom(&h, &oom);
        if let Some(f) = &f {
            assert_eq!(f.target, ForecastTarget::SwapExhaustion);
            assert_eq!(f.killer, Some(expected_killer));
            let err = (f.eta_s as f64 - actual) / actual;
            // ±30 % of actual; in the last 2 minutes sample noise (±16 MiB ≈ ±6 s of growth) dominates the
            // relative error, so the bound there is ±30 s absolute.
            if actual >= 120.0 {
                assert!(
                    err.abs() <= 0.30,
                    "{os:?} t={t}s eta={} actual={actual} err={err:.3}",
                    f.eta_s
                );
                max_err = max_err.max(err.abs());
            } else {
                assert!((f.eta_s as f64 - actual).abs() <= 30.0, "t={t}s eta={}", f.eta_s);
            }
            n_forecasts += 1;
            first_forecast.get_or_insert(*t);
        } else if actual < 25.0 * 60.0 && actual > 60.0 && *t >= 120 {
            panic!("{os:?}: no forecast at t={t}s with {actual}s to go");
        }
        if t.is_multiple_of(120) {
            rows.push(Row {
                t: format_duration(*t),
                forecast: f
                    .as_ref()
                    .map(|f| format!("{} (R² {:.3})", format_duration(f.eta_s), f.confidence))
                    .unwrap_or_else(|| "stable".into()),
                actual: format_duration(actual as u64),
                err_pct: f
                    .as_ref()
                    .map(|f| ((f.eta_s as f64 - actual) / actual * 1000.0).round() / 10.0),
            });
        }
    }
    assert!(n_forecasts > 30);
    let first = first_forecast.unwrap();
    // The forecast appears only once the ETA is under 30 min.
    assert!(crossing - first <= 30 * 60 + 30 * 60 * 3 / 10);
    serde_json::json!({
        "crossing": format_duration(crossing),
        "first_forecast_at": format_duration(first),
        "forecasts": n_forecasts,
        "max_abs_err_pct": (max_err * 1000.0).round() / 10.0,
        "timeline": rows,
    })
}

#[test]
fn acceptance_7_steady_swap_growth() {
    let golden = acceptance_7(OsKind::Linux);
    settings().bind(|| {
        insta::assert_yaml_snapshot!("forecast_acceptance_7_steady", golden);
    });
}

#[test]
fn acceptance_7_steady_swap_growth_macos_jetsam() {
    let golden = acceptance_7(OsKind::Macos);
    settings().bind(|| {
        insta::assert_yaml_snapshot!("forecast_acceptance_7_steady_macos", golden);
    });
}

#[test]
fn acceptance_7_rise_then_drop_clears_the_eta() {
    // Swap grows 300 MiB/min for 6 min, then an OOM kill frees ~2.8 GiB in one step; the ETA must vanish on
    // the first sample after the drop (never "swap full soon" right after a reclaim).
    let oom = linux_oom();
    let mut h = History::default();
    let mut noise = Lcg(11);
    let mut out = Vec::new();
    let mut after_drop = Vec::new();
    for i in 0..=(10 * 60 / STEP_S) {
        let t = i * STEP_S;
        let grown = 2.0 * GIB as f64 + 300.0 * MIB as f64 * t.min(360) as f64 / 60.0;
        let used = if t > 360 { 1.0 * GIB as f64 } else { grown };
        h.push(HistoryPoint {
            t_ms: t * 1000,
            swap_used: Some((used + noise.next_unit() * 16.0 * MIB as f64).max(0.0) as u64),
            swap_total: Some(8 * GIB),
            available: Some(3 * GIB),
            ..Default::default()
        });
        let f = forecast_oom(&h, &oom);
        if t > 360 {
            after_drop.push((t, f.clone()));
        }
        if t.is_multiple_of(60) || t == 365 {
            out.push(serde_json::json!({
                "t_s": t,
                "swap_used_mib": (used / MIB as f64).round(),
                "forecast": f.map(|f| format_duration(f.eta_s)).unwrap_or_else(|| "stable".into()),
            }));
        }
    }
    let before = forecast_oom_at(&oom, &h, 360);
    assert!(before.is_some(), "the rise forecasts before the drop");
    assert!(
        after_drop.iter().all(|(_, f)| f.is_none()),
        "ETA after the drop: {after_drop:?}"
    );
    settings().bind(|| {
        insta::assert_yaml_snapshot!("forecast_rise_then_drop", out);
    });
}

/// Forecast on the history truncated at `t_s` (the state a frontend saw at that time).
fn forecast_oom_at(oom: &Oom, h: &History, t_s: u64) -> Option<Forecast> {
    let mut cut = History::default();
    for p in h.iter().filter(|p| p.t_ms <= t_s * 1000) {
        cut.push(p.clone());
    }
    forecast_oom(&cut, oom)
}

/// Runs a flat-with-noise scenario with injected events and returns every time a forecast appeared.
fn run_flat(events: impl Fn(u64, &mut HistoryPoint)) -> Vec<(u64, Forecast)> {
    let mut noise = Lcg(42);
    let mut h = History::default();
    let oom = linux_oom();
    let mut hits = Vec::new();
    for i in 0..(40 * 60 / STEP_S) {
        let t = i * STEP_S;
        let mut p = HistoryPoint {
            t_ms: t * 1000,
            swap_used: Some((3.0 * GIB as f64 + noise.next_unit() * 16.0 * MIB as f64) as u64),
            swap_total: Some(4 * GIB),
            available: Some((2.0 * GIB as f64 + noise.next_unit() * 30.0 * MIB as f64) as u64),
            psi_some_avg10: Some(5.0 + noise.next_unit()),
            ..Default::default()
        };
        events(t, &mut p);
        h.push(p);
        if let Some(f) = forecast_oom(&h, &oom) {
            hits.push((t, f));
        }
    }
    hits
}

#[test]
fn acceptance_7_never_on_a_single_spike() {
    // One sample: swap +900 MiB (to 97 % of total) and, later, available −1.5 GiB.
    let spikes = run_flat(|t, p| {
        if t == 20 * 60 {
            p.swap_used = Some(p.swap_used.unwrap() + 900 * MIB);
        }
        if t == 30 * 60 {
            p.available = Some(p.available.unwrap() - 1536 * MIB);
        }
    });
    assert!(spikes.is_empty(), "forecast on a spike: {spikes:?}");

    // A one-off step (a model load): swap +800 MiB and stays; available −1 GiB and stays.
    let steps = run_flat(|t, p| {
        if t >= 15 * 60 {
            p.swap_used = Some(p.swap_used.unwrap() + 800 * MIB);
        }
        if t >= 25 * 60 {
            p.available = Some(p.available.unwrap() - GIB);
        }
    });
    assert!(steps.is_empty(), "forecast on a step: {steps:?}");

    // A short burst of three samples is still not a trend.
    let burst = run_flat(|t, p| {
        if (20 * 60..20 * 60 + 15).contains(&t) {
            p.swap_used = Some(p.swap_used.unwrap() + 700 * MIB);
        }
    });
    assert!(burst.is_empty(), "forecast on a burst: {burst:?}");

    // Pure noise never forecasts.
    assert!(run_flat(|_, _| {}).is_empty());

    settings().bind(|| {
        insta::assert_yaml_snapshot!(
            "forecast_acceptance_7_spikes",
            serde_json::json!({
                "single_spike": spikes.len(),
                "step": steps.len(),
                "burst": burst.len(),
                "noise": 0,
            })
        );
    });
}

#[test]
fn macos_available_decline_and_killer_thresholds() {
    // macOS: available falls 200 MiB/min from 6 GiB; swap total grows with use (dynamic) and is ignored.
    let mut h = History::default();
    let mut noise = Lcg(3);
    let jetsam = oom_setup(&OomInputs {
        os: OsKind::Macos,
        ..Default::default()
    });
    let mut out = Vec::new();
    for i in 0..=(8 * 60 / STEP_S) {
        let t = i * STEP_S;
        let avail = 6.0 * GIB as f64 - 200.0 * MIB as f64 * t as f64 / 60.0;
        let swap = 2.0 * GIB as f64 + 100.0 * MIB as f64 * t as f64 / 60.0;
        h.push(HistoryPoint {
            t_ms: t * 1000,
            available: Some((avail + noise.next_unit() * 20.0 * MIB as f64) as u64),
            swap_used: Some(swap as u64),
            swap_total: Some((swap as u64 / GIB + 1) * GIB),
            ..Default::default()
        });
        if t.is_multiple_of(120) && t > 0 {
            let actual = avail / (200.0 * MIB as f64 / 60.0);
            let f = forecast_oom(&h, &jetsam);
            if let Some(f) = &f {
                assert_eq!(f.target, ForecastTarget::AvailableExhaustion);
                assert_eq!(f.killer, Some(OomKiller::Jetsam));
                assert!(((f.eta_s as f64 - actual) / actual).abs() <= 0.30);
            }
            out.push(serde_json::json!({
                "t": format_duration(t),
                "forecast": f.map(|f| format_duration(f.eta_s)).unwrap_or_else(|| "stable".into()),
                "actual": format_duration(actual as u64),
            }));
        }
    }

    // Linux with systemd-oomd defaults and earlyoom -m 5 -s 10, 32 GiB RAM, 8 GiB swap.
    let oom = oom_setup(&OomInputs {
        os: OsKind::Linux,
        oomd: Some((OomdConfig::default(), vec![])),
        earlyoom_argv: Some(vec![
            "/usr/bin/earlyoom".into(),
            "-m".into(),
            "5".into(),
            "-s".into(),
            "10".into(),
        ]),
        mem_total: Some(32 * GIB),
        swap_total: Some(8 * GIB),
    });
    // Swap grows 250 MiB/min from 5 GiB; available falls 150 MiB/min from 3 GiB; PSI flat.
    let mut h = History::default();
    for i in 0..=(4 * 60 / STEP_S) {
        let t = i * STEP_S;
        h.push(HistoryPoint {
            t_ms: t * 1000,
            swap_used: Some(5 * GIB + 250 * MIB * t / 60),
            swap_total: Some(8 * GIB),
            available: Some(3 * GIB - 150 * MIB * t / 60),
            psi_some_avg10: Some(12.0),
            ..Default::default()
        });
    }
    let f = forecast_oom(&h, &oom).expect("forecast");
    // oomd swap 90 % (7.2 GiB) is reached before earlyoom's AND (available ≤ 1.6 GiB and swap ≥ 90 %)
    // and before swap exhaustion.
    assert_eq!(f.killer, Some(OomKiller::SystemdOomd));
    assert_eq!(f.target, ForecastTarget::KillerThreshold);

    settings().bind(|| {
        insta::assert_yaml_snapshot!(
            "forecast_killers",
            serde_json::json!({
                "macos_jetsam_available_decline": out,
                "linux_setup": {
                    "killer": oom.killer,
                    "killers": oom.killers,
                    "thresholds": oom.thresholds,
                },
                "linux_forecast": {
                    "target": f.target,
                    "killer": f.killer,
                    "eta": format_duration(f.eta_s),
                    "rate_per_min_mib": (f.rate_per_min / MIB as f64).round(),
                    "confidence": (f.confidence * 1000.0).round() / 1000.0,
                },
            })
        );
    });
}

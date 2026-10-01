//! UX §3 situation modes replayed on the motivating machine: enter after 10 s, leave after 30 s calm,
//! per-OS triggers, priority, pinning.

mod ux_common;

use oomtop_core::headroom::{compute, HeadroomConfig};
use oomtop_core::model::*;
use oomtop_core::modes::{reasons, signals, Mode, ModeMachine};
use ux_common::*;

/// Replays frames at a 2 s cadence and returns (t_s, mode) per refresh.
fn replay(frames: &[Snapshot]) -> Vec<(u64, Mode)> {
    let mut m = ModeMachine::new();
    frames
        .iter()
        .map(|s| {
            let h = compute(s, &HeadroomConfig::default());
            (
                (s.taken_at_ms - T0_MS) / 1000,
                m.update(s.taken_at_ms, &signals(s, &h)),
            )
        })
        .collect()
}

fn frame(i: u64, f: impl Fn(&mut Snapshot)) -> Snapshot {
    let mut s = machine(false);
    s.taken_at_ms = T0_MS + i * 2_000;
    f(&mut s);
    s
}

fn first_t(run: &[(u64, Mode)], mode: Mode) -> Option<u64> {
    run.iter().find(|(_, m)| *m == mode).map(|r| r.0)
}

#[test]
fn macos_throttle_episode_enters_after_10s_and_leaves_after_30s() {
    // Calm for 20 s, heavily throttled under load 20..80 s (factor wobbling around 0.5), then cool.
    let frames: Vec<Snapshot> = (0..=90u64)
        .map(|i| {
            frame(i, |s| {
                let t = i * 2;
                if (20..80).contains(&t) {
                    s.thermal.throttle_factor =
                        Measured::estimate(if i % 2 == 0 { 0.48 } else { 0.55 }, "ioreport");
                    s.thermal.pressure = Measured::exact(ThermalPressure::Heavy, "notify thermal");
                }
            })
        })
        .collect();
    let run = replay(&frames);
    assert_eq!(
        first_t(&run, Mode::Throttle),
        Some(30),
        "entered 10 s after onset at 20 s"
    );
    let left = run
        .iter()
        .skip_while(|(t, _)| *t < 30)
        .find(|(_, m)| *m == Mode::Calm)
        .unwrap()
        .0;
    assert_eq!(
        left, 110,
        "left 30 s after the last throttled sample (78 s → calm from 80 s)"
    );
}

#[test]
fn idle_machine_never_throttles() {
    // Apple Silicon down-clocks at idle: factor unavailable("idle") must never trigger Throttle.
    let frames: Vec<Snapshot> = (0..60u64)
        .map(|i| {
            frame(i, |s| {
                s.thermal.throttle_factor = Measured::unavailable("throttle", "idle")
            })
        })
        .collect();
    assert!(replay(&frames).iter().all(|(_, m)| *m == Mode::Calm));
}

#[test]
fn linux_psi_pressure_with_priority_over_working() {
    let frames: Vec<Snapshot> = (0..=40u64)
        .map(|i| {
            frame(i, |s| {
                s.host.os = OsKind::Linux;
                s.memory.pressure = Measured::unavailable("pressure", "derived from PSI");
                s.memory.psi = Measured::exact(
                    Psi {
                        some_avg10: if i >= 5 { 34.0 } else { 3.0 },
                        ..Default::default()
                    },
                    "/proc/pressure/memory",
                );
                s.model_servers[0].busy = Measured::exact(true, "llama.cpp /slots");
            })
        })
        .collect();
    let run = replay(&frames);
    assert_eq!(run[0].1, Mode::Working, "first sample adopted");
    assert_eq!(
        first_t(&run, Mode::Pressure),
        Some(20),
        "PSI > 20 % from t=10 s → Pressure at 20 s"
    );
    assert!(run
        .iter()
        .skip_while(|(t, _)| *t < 20)
        .all(|(_, m)| *m == Mode::Pressure));
    let h = compute(&frames[10], &HeadroomConfig::default());
    let r = reasons(&frames[10], &h);
    assert!(
        r.iter().any(|r| r.key == "psi" && r.text == "PSI mem some 34%"),
        "{r:?}"
    );
}

#[test]
fn pinned_mode_overrides_but_tracking_continues() {
    let mut m = ModeMachine::new();
    let calm = machine(false);
    let h = compute(&calm, &HeadroomConfig::default());
    m.update(T0_MS, &signals(&calm, &h));
    m.pin(Some(Mode::Pressure));
    let mut t = T0_MS;
    let mut hot = machine(false);
    hot.thermal.low_power_mode = Measured::exact(true, "NSProcessInfo");
    for _ in 0..6 {
        t += 2_000;
        assert_eq!(m.update(t, &signals(&hot, &h)), Mode::Pressure);
    }
    assert_eq!(
        m.underlying(),
        Mode::Throttle,
        "Low Power Mode latched underneath the pin"
    );
    m.pin(None);
    assert_eq!(m.current(), Mode::Throttle);
}

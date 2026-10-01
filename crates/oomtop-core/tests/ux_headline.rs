//! UX §9 headline templates end-to-end (snapshot → headroom → mode → headline) and UX §11 test 13:
//! with swap growing steadily in a replayed fixture the headline shows the OOM ETA and the best reclaim
//! action; on a fixture with a single spike it doesn't.

mod ux_common;

use oomtop_core::forecast::forecast_oom;
use oomtop_core::headline::{input_from, render, your_things_chips, Headline};
use oomtop_core::headroom::{compute, HeadroomConfig};
use oomtop_core::model::*;
use oomtop_core::modes::{signals, Mode, ModeMachine};
use oomtop_core::provider::{SequenceProvider, SnapshotProvider};
use ux_common::*;

/// One refresh of the frontend pipeline: forecast → headroom → mode → headline.
fn step(p: &mut SequenceProvider, modes: &mut ModeMachine) -> (Snapshot, Headline) {
    let mut s = p.snapshot();
    s.oom.forecast = forecast_oom(p.history(), &s.oom);
    let h = compute(&s, &HeadroomConfig::default());
    let mode = modes.update(s.taken_at_ms, &signals(&s, &h));
    let line = render(&input_from(&s, &h, mode));
    (s, line)
}

fn replay(frames: Vec<Snapshot>) -> Vec<(u64, Mode, String)> {
    let n = frames.len();
    let mut p = SequenceProvider::new(frames);
    let mut m = ModeMachine::new();
    (0..n)
        .map(|_| {
            let (s, h) = step(&mut p, &mut m);
            ((s.taken_at_ms - T0_MS) / 1000, h.mode, h.text)
        })
        .collect()
}

/// Only the refreshes where the headline changed, as "t=…s [mode] text" lines.
fn transitions(run: &[(u64, Mode, String)]) -> String {
    let mut out = Vec::new();
    let mut last = String::new();
    for (t, m, text) in run {
        if *text != last {
            out.push(format!("t={t:>3}s [{}] {text}", m.as_str()));
            last = text.clone();
        }
    }
    out.join("\n")
}

fn steady_swap_growth() -> Vec<Snapshot> {
    // +400 MB/min for 5 min; swap 7 GB total → at the end 4.6 GB used, full in 6 min.
    (0..=60u64)
        .map(|i| {
            let mut s = at(machine(true), i);
            s.memory.available = Measured::exact(4_300_000_000, "vm_statistics64");
            s.memory.swap_used = Measured::exact(2_600_000_000 + i * 400_000_000 / 12, "vm.swapusage");
            s.memory.swap_out_per_min = Measured::exact(400_000_000, "Δ swapouts");
            s
        })
        .collect()
}

fn single_spike() -> Vec<Snapshot> {
    (0..=60u64)
        .map(|i| {
            let mut s = at(machine(true), i);
            if i == 60 {
                s.memory.swap_used = Measured::exact(5_500_000_000, "vm.swapusage");
                s.memory.swap_out_per_min = Measured::exact(2_900_000_000, "Δ swapouts");
            }
            s
        })
        .collect()
}

#[test]
fn test13_steady_swap_growth_shows_oom_eta_and_reclaim() {
    let run = replay(steady_swap_growth());
    let (_, mode, last) = run.last().unwrap();
    assert_eq!(*mode, Mode::Pressure);
    assert_eq!(
        last,
        "Swap full in ~6 min at this rate — sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB."
    );
    // No ETA before the forecast has ≥ 2 min of consistent data.
    assert!(run
        .iter()
        .take_while(|(t, _, _)| *t < 120)
        .all(|(_, _, h)| !h.contains("at this rate")));
    insta::assert_snapshot!("test13_steady_growth_transitions", transitions(&run));
}

#[test]
fn test13_single_spike_shows_no_eta() {
    let run = replay(single_spike());
    assert!(
        run.iter().all(|(_, _, h)| !h.contains("at this rate")),
        "{run:#?}"
    );
    let (_, mode, last) = run.last().unwrap();
    // One pressured sample never enters Pressure (needs 10 s); the idle daemons keep Leftovers.
    assert_eq!(*mode, Mode::Leftovers);
    assert_eq!(last, "Idle leftovers — 2 idle build daemons could free 5.9 GB.");
    insta::assert_snapshot!("test13_single_spike_transitions", transitions(&run));
}

#[test]
fn forecast_clears_and_pressure_leaves_after_growth_stops() {
    let mut frames = steady_swap_growth();
    let plateau = frames.last().unwrap().memory.swap_used.clone();
    for i in 61..=160u64 {
        let mut s = at(machine(true), i);
        s.memory.available = Measured::exact(4_300_000_000, "vm_statistics64");
        s.memory.swap_used = plateau.clone();
        s.memory.swap_out_per_min = Measured::exact(0, "Δ swapouts");
        frames.push(s);
    }
    let run = replay(frames);
    // The forecast keeps Pressure on while the 5-min fit window still sees the rise; once it clears
    // (t_clear), Pressure leaves after 30 s of calm, and the latched Leftovers mode shows.
    let t_clear = run
        .iter()
        .find(|(t, _, h)| *t > 300 && !h.contains("at this rate"))
        .map(|r| r.0)
        .expect("forecast clears on a plateau");
    assert!(
        t_clear < 300 + 5 * 60,
        "cleared within the fit window ({t_clear}s)"
    );
    for (t, m, _) in &run {
        if *t >= t_clear && *t < t_clear + 30 {
            assert_eq!(*m, Mode::Pressure, "t={t}");
        }
        if *t >= t_clear + 30 {
            assert_eq!(*m, Mode::Leftovers, "t={t}");
        }
    }
    insta::assert_snapshot!("growth_then_plateau_transitions", transitions(&run));
}

#[test]
fn ux9_tight_on_memory() {
    let mut s = machine(true);
    s.memory.pressure = Measured::exact(PressureLevel::Warn, "kern.memorystatus_vm_pressure_level");
    s.memory.swap_out_per_min = Measured::exact(420_000_000, "Δ swapouts");
    // margin = 8 % of 24 GiB × 1.5 (pressured) ≈ 3.09 GB → 1.2 GB headroom.
    s.memory.available = Measured::exact(4_290_000_000, "vm_statistics64");
    let h = compute(&s, &HeadroomConfig::default());
    let mut m = ModeMachine::new();
    let mode = m.update(s.taken_at_ms, &signals(&s, &h));
    let line = render(&input_from(&s, &h, mode));
    assert_eq!(mode, Mode::Pressure);
    assert_eq!(
        line.text,
        "Tight on memory — 1.2 GB headroom. sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB."
    );
    assert_eq!(line.action_key, Some('r'));
}

#[test]
fn ux9_throttled_on_battery_while_generating() {
    let mut s = machine(false);
    s.thermal.throttle_factor = Measured::estimate(0.45, "Σ(cur/max × residency)");
    s.thermal.pressure = Measured::exact(ThermalPressure::Heavy, "notify thermal");
    s.thermal.on_battery = Measured::exact(true, "IOPowerSources");
    s.thermal.battery_pct = Measured::exact(29.0, "IOPowerSources");
    s.model_servers[0].busy = Measured::exact(true, "sd-server /health");
    s.model_servers[0].progress = Some(JobProgress {
        done: 3,
        total: 6,
        label: "generating".into(),
    });
    s.model_servers[0].s_per_step = Measured::estimate(8.1, "sd-server log");
    let h = compute(&s, &HeadroomConfig::default());
    let mode = ModeMachine::new().update(s.taken_at_ms, &signals(&s, &h));
    assert_eq!(mode, Mode::Throttle);
    let input = input_from(&s, &h, mode);
    assert_eq!(
        input.working.as_deref(),
        Some("sd-server generating 3/6 · 8.1 s/step, ~24s left")
    );
    assert_eq!(
        render(&input).text,
        "Running at 45% speed — thermal pressure heavy, on battery (29%). Plug in or pause generation."
    );
    // The same machine once it cools down is Working (the model is still generating).
    let mut cool = s.clone();
    cool.thermal.throttle_factor = Measured::estimate(0.97, "Σ(cur/max × residency)");
    cool.thermal.pressure = Measured::exact(ThermalPressure::Nominal, "notify thermal");
    let h = compute(&cool, &HeadroomConfig::default());
    let mode = ModeMachine::new().update(cool.taken_at_ms, &signals(&cool, &h));
    assert_eq!(mode, Mode::Working);
    insta::assert_snapshot!("working_generating", render(&input_from(&cool, &h, mode)).text);
}

#[test]
fn ux9_all_good_with_your_things() {
    let s = machine(false);
    let h = compute(&s, &HeadroomConfig::default());
    let mode = ModeMachine::new().update(s.taken_at_ms, &signals(&s, &h));
    assert_eq!(mode, Mode::Calm);
    let mut input = input_from(&s, &h, mode);
    input.your_things = your_things_chips(&s, &[FP_SD.to_string(), FP_CLAUDE.to_string()]);
    assert_eq!(
        render(&input).text,
        "All good — 11 GB free. Your things: sd-server idle, 4 Claude Code sessions."
    );
    assert_eq!(render(&input).action_key, None);
}

#[test]
fn headline_texts_golden() {
    // Every mode's template on the motivating machine, in both unit systems.
    let mut cases: Vec<(String, Snapshot)> = Vec::new();
    let mut p = machine(true);
    p.memory.pressure = Measured::exact(PressureLevel::Critical, "kern.memorystatus_vm_pressure_level");
    p.memory.available = Measured::exact(2_000_000_000, "vm_statistics64");
    cases.push(("pressure_negative_headroom".into(), p));
    let mut o = machine(false);
    o.groups.iter_mut().find(|g| g.label == "clang").unwrap().orphan = true;
    o.groups
        .iter_mut()
        .find(|g| g.label == "clang")
        .unwrap()
        .reclaim_gain = Measured::estimate(180_000_000, "resident");
    cases.push(("leftovers_orphan".into(), o));
    let mut l = machine(false);
    l.thermal.low_power_mode = Measured::exact(true, "NSProcessInfo");
    cases.push(("throttle_low_power_idle".into(), l));
    let mut u = machine(false);
    u.memory.available = Measured::unavailable("vm_statistics64", "host_statistics64 failed");
    cases.push(("calm_memory_unavailable".into(), u));
    let mut out = Vec::new();
    for (name, s) in cases {
        let h = compute(&s, &HeadroomConfig::default());
        let mode = ModeMachine::new().update(s.taken_at_ms, &signals(&s, &h));
        let mut input = input_from(&s, &h, mode);
        let si = render(&input).text;
        input.units = Some("iec".into());
        let iec = render(&input).text;
        out.push(format!("{name} [{}]\n  si:  {si}\n  iec: {iec}", mode.as_str()));
    }
    insta::assert_snapshot!("headline_texts", out.join("\n"));
}

/// The calm headline's "N free" is the resolved available-now value that headroom uses: capped by the own
/// cgroup's memory.max, and by macOS memorystatus when that view is tighter than vm_statistics64.
#[test]
fn calm_free_uses_the_resolved_available_value() {
    let calm = |s: &Snapshot| {
        let h = compute(s, &HeadroomConfig::default());
        let text = render(&input_from(s, &h, Mode::Calm)).text;
        (text, h)
    };
    let base = machine(false);
    let (plain, _) = calm(&base);
    assert!(plain.starts_with("All good — 11 GB free."), "{plain}");

    let mut capped = base.clone();
    capped.memory.own_cgroup_limit = Measured::exact(2_000_000_000, "cgroup memory.max");
    let (text, h) = calm(&capped);
    assert_eq!(h.available_now.value, Some(2_000_000_000));
    assert!(text.starts_with("All good — 2.0 GB free."), "{text}");

    let mut tight = base.clone();
    // memorystatus says 25 % of 24 GiB ≈ 6.4 GB available — tighter than vm_statistics64's 11 GB.
    tight.memory.memorystatus_level = Measured::exact(25, "kern.memorystatus_level");
    let (text, h) = calm(&tight);
    let ms = h.available_now.value.unwrap();
    assert!(ms < 11_000_000_000, "{ms}");
    assert!(text.starts_with("All good — 6.4 GB free."), "{text}");
    insta::assert_snapshot!(
        "calm_free_resolved",
        format!("{plain}\n{}\n{text}", calm(&capped).0)
    );
}

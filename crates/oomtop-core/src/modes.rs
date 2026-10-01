//! Situation modes (UX §3): Calm · Pressure · Throttle · Working · Leftovers. Adaptive, not personalized.
//!
//! Each non-calm mode has its own trigger (per OS, see [`signals`]) and its own latch: a mode **enters** after
//! its condition has held for [`ENTER_AFTER_MS`] (10 s) and **leaves** after the condition has been false for
//! [`LEAVE_AFTER_MS`] (30 s). The shown mode is the highest-priority latched mode
//! (Pressure > Throttle > Working > Leftovers), else Calm. So a mode never flaps, and when a higher mode
//! clears, an already-latched lower one shows immediately. The first sample is adopted as-is (there is no
//! history to flap against). A mode can be pinned (`M`), which overrides what is shown but keeps tracking.
//!
//! Modes change emphasis and default sort only.

use crate::headroom::{is_reclaim_candidate, Headroom};
use crate::model::{GroupKind, OsKind, PressureLevel, Snapshot, ThermalPressure};
use crate::throttle::THROTTLED_BELOW;
use crate::units::{format_bytes, UnitSystem};
use serde::{Deserialize, Serialize};

pub const ENTER_AFTER_MS: u64 = 10_000;
pub const LEAVE_AFTER_MS: u64 = 30_000;
/// Linux PSI `some avg10` above this (%) is memory pressure (UX §3).
pub const PSI_SOME_PRESSURE_PCT: f64 = 20.0;
/// A build daemon / model server group above this CPU (% of one core) counts as actively working.
pub const WORKING_CPU_PCT: f64 = 50.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Calm,
    Pressure,
    Throttle,
    Working,
    Leftovers,
}

impl Mode {
    /// Non-calm modes in priority order (highest first).
    pub const PRIORITY: [Mode; 4] = [Mode::Pressure, Mode::Throttle, Mode::Working, Mode::Leftovers];
    pub const ALL: [Mode; 5] = [
        Mode::Calm,
        Mode::Pressure,
        Mode::Throttle,
        Mode::Working,
        Mode::Leftovers,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Calm => "calm",
            Mode::Pressure => "pressure",
            Mode::Throttle => "throttle",
            Mode::Working => "working",
            Mode::Leftovers => "leftovers",
        }
    }

    /// Header label ("Pressure").
    pub fn title(self) -> &'static str {
        match self {
            Mode::Calm => "Calm",
            Mode::Pressure => "Pressure",
            Mode::Throttle => "Throttle",
            Mode::Working => "Working",
            Mode::Leftovers => "Leftovers",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        let s = s.trim().to_ascii_lowercase();
        Mode::ALL.into_iter().find(|m| m.as_str() == s)
    }

    fn index(self) -> usize {
        match self {
            Mode::Calm => 0,
            Mode::Pressure => 1,
            Mode::Throttle => 2,
            Mode::Working => 3,
            Mode::Leftovers => 4,
        }
    }
}

/// Raw conditions for this sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeSignals {
    pub pressure: bool,
    pub throttle: bool,
    pub working: bool,
    pub leftovers: bool,
}

impl ModeSignals {
    /// Highest-priority active mode: Pressure > Throttle > Working > Leftovers > Calm.
    pub fn desired(&self) -> Mode {
        Mode::PRIORITY
            .into_iter()
            .find(|m| self.is(*m))
            .unwrap_or(Mode::Calm)
    }

    /// Whether the condition for `mode` holds (Calm holds when nothing else does).
    pub fn is(&self, mode: Mode) -> bool {
        match mode {
            Mode::Calm => !(self.pressure || self.throttle || self.working || self.leftovers),
            Mode::Pressure => self.pressure,
            Mode::Throttle => self.throttle,
            Mode::Working => self.working,
            Mode::Leftovers => self.leftovers,
        }
    }

    /// Every active non-calm mode, highest priority first.
    pub fn active(&self) -> Vec<Mode> {
        Mode::PRIORITY.into_iter().filter(|m| self.is(*m)).collect()
    }
}

/// One trigger that fired, for the header tooltip / `why` ("swap growing +420 MB/min").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeReason {
    pub mode: Mode,
    /// Stable key, e.g. "headroom_negative", "swap_growing", "oom_forecast", "psi", "memorystatus",
    /// "throttle_factor", "thermal_pressure", "trip_point", "low_power_mode", "model_busy", "build_active",
    /// "orphans", "idle_reclaimable".
    pub key: String,
    pub text: String,
}

fn fmt(b: u64) -> String {
    format_bytes(b, UnitSystem::Si, 1)
}

/// All triggers that currently fire, per UX §3 (per-OS where the spec says so):
/// - **Pressure**: headroom < 0, swap growing, an OOM forecast; plus Linux PSI mem `some avg10` > 20 %
///   (falling back to the derived pressure level when PSI is unreadable) and macOS memorystatus level ≥ warn.
/// - **Throttle**: throttle factor < 0.8 under load (the factor is only available while busy), thermal
///   pressure ≥ heavy (macOS) / a trip point hit (Linux), or Low Power Mode.
/// - **Working**: a model server reports busy, or a build daemon / model server group is using ≥ 50 % of a
///   core (only when no adapter reports on that model server).
/// - **Leftovers**: orphans, or idle groups that are reclaim candidates.
///
/// On an OS that is neither macOS nor Linux (e.g. replayed fixtures without host info) both OS-specific
/// variants apply.
pub fn reasons(s: &Snapshot, h: &Headroom) -> Vec<ModeReason> {
    let mut out = Vec::new();
    let mut add = |mode: Mode, key: &str, text: String| {
        out.push(ModeReason {
            mode,
            key: key.to_string(),
            text,
        })
    };
    let os = s.host.os;
    let linux = matches!(os, OsKind::Linux | OsKind::Other);
    let macos = matches!(os, OsKind::Macos | OsKind::Other);

    // Pressure.
    if let Some(x) = h.headroom.filter(|x| *x < 0) {
        add(
            Mode::Pressure,
            "headroom_negative",
            format!("{} into the safety margin", fmt(x.unsigned_abs())),
        );
    }
    if h.swap_growing {
        let rate = s
            .memory
            .swap_out_per_min
            .value
            .map(|r| format!(" +{}/min", fmt(r)));
        add(
            Mode::Pressure,
            "swap_growing",
            format!("swap growing{}", rate.unwrap_or_default()),
        );
    }
    if let Some(f) = &s.oom.forecast {
        add(
            Mode::Pressure,
            "oom_forecast",
            format!("OOM forecast in {}", crate::units::format_duration(f.eta_s)),
        );
    }
    if linux {
        match s.memory.psi.value {
            Some(p) if p.some_avg10 > PSI_SOME_PRESSURE_PCT => add(
                Mode::Pressure,
                "psi",
                format!("PSI mem some {:.0}%", p.some_avg10),
            ),
            Some(_) => {}
            None if os == OsKind::Linux => {
                if let Some(level) = s.memory.pressure.value.filter(|l| *l >= PressureLevel::Warn) {
                    add(
                        Mode::Pressure,
                        "pressure_level",
                        format!("memory pressure {}", level_str(level)),
                    );
                }
            }
            None => {}
        }
    }
    if macos {
        if let Some(level) = s.memory.pressure.value.filter(|l| *l >= PressureLevel::Warn) {
            add(
                Mode::Pressure,
                "memorystatus",
                format!("memory pressure {}", level_str(level)),
            );
        }
    }

    // Throttle.
    let t = &s.thermal;
    if let Some(f) = t.throttle_factor.value.filter(|f| *f < THROTTLED_BELOW) {
        add(
            Mode::Throttle,
            "throttle_factor",
            format!("running at {:.0}% speed", f * 100.0),
        );
    }
    if macos {
        if let Some(p) = t.pressure.value.filter(|p| *p >= ThermalPressure::Heavy) {
            add(
                Mode::Throttle,
                "thermal_pressure",
                format!("thermal pressure {}", thermal_str(p)),
            );
        }
    }
    if linux && t.trip_point_hit.value == Some(true) {
        add(Mode::Throttle, "trip_point", "thermal trip point hit".into());
    }
    if t.low_power_mode.value == Some(true) {
        add(Mode::Throttle, "low_power_mode", "Low Power Mode on".into());
    }

    // Working.
    for m in &s.model_servers {
        if m.busy.value == Some(true) {
            let name = m
                .group_id
                .as_deref()
                .and_then(|g| s.group(g))
                .map(|g| g.label.clone())
                .unwrap_or_else(|| m.id.clone());
            add(Mode::Working, "model_busy", format!("{name} busy"));
        }
    }
    for g in &s.groups {
        let cpu = g.totals.cpu_pct.value.unwrap_or(0.0);
        if g.idle || cpu < WORKING_CPU_PCT {
            continue;
        }
        match g.kind {
            GroupKind::BuildDaemon => add(Mode::Working, "build_active", format!("{} building", g.label)),
            GroupKind::ModelServer => {
                let adapter_knows = s
                    .model_servers
                    .iter()
                    .any(|m| m.group_id.as_deref() == Some(g.id.as_str()) && m.busy.value.is_some());
                if !adapter_knows {
                    add(Mode::Working, "model_cpu", format!("{} active", g.label));
                }
            }
            _ => {}
        }
    }

    // Leftovers.
    let orphans = s
        .groups
        .iter()
        .filter(|g| g.orphan && !g.protected && !g.is_self)
        .count();
    if orphans > 0 {
        add(
            Mode::Leftovers,
            "orphans",
            format!("{orphans} orphan{}", if orphans == 1 { "" } else { "s" }),
        );
    }
    let idle = s
        .groups
        .iter()
        .filter(|g| g.idle && !g.orphan && is_reclaim_candidate(g))
        .count();
    if idle > 0 {
        add(
            Mode::Leftovers,
            "idle_reclaimable",
            format!(
                "{idle} idle reclaimable group{}",
                if idle == 1 { "" } else { "s" }
            ),
        );
    }
    out
}

fn level_str(l: PressureLevel) -> &'static str {
    match l {
        PressureLevel::Normal => "normal",
        PressureLevel::Warn => "warn",
        PressureLevel::Critical => "critical",
    }
}

fn thermal_str(p: ThermalPressure) -> &'static str {
    match p {
        ThermalPressure::Nominal => "nominal",
        ThermalPressure::Moderate => "moderate",
        ThermalPressure::Heavy => "heavy",
        ThermalPressure::Trapping => "trapping",
        ThermalPressure::Sleeping => "sleeping",
    }
}

/// Derives mode signals from a snapshot and its headroom (UX §3 triggers, per OS; see [`reasons`]).
pub fn signals(s: &Snapshot, h: &Headroom) -> ModeSignals {
    let mut sig = ModeSignals::default();
    for r in reasons(s, h) {
        match r.mode {
            Mode::Pressure => sig.pressure = true,
            Mode::Throttle => sig.throttle = true,
            Mode::Working => sig.working = true,
            Mode::Leftovers => sig.leftovers = true,
            Mode::Calm => {}
        }
    }
    sig
}

/// Per-mode latch: when the condition last became true / false.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Latch {
    on: bool,
    true_since: Option<u64>,
    false_since: Option<u64>,
}

/// Hysteresis state machine (enter after 10 s, leave after 30 s, per mode).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModeMachine {
    current: Mode,
    latches: [Latch; 5],
    pinned: Option<Mode>,
    initialized: bool,
    last_ms: u64,
}

impl ModeMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// The mode to show: the pinned one if pinned, else the latched situation.
    pub fn current(&self) -> Mode {
        self.pinned.unwrap_or(self.current)
    }

    /// The latched situation, ignoring a pin.
    pub fn underlying(&self) -> Mode {
        self.current
    }

    /// Every latched non-calm mode, highest priority first.
    pub fn latched(&self) -> Vec<Mode> {
        Mode::PRIORITY
            .into_iter()
            .filter(|m| self.latches[m.index()].on)
            .collect()
    }

    /// Pins a mode (`M`); `None` unpins.
    pub fn pin(&mut self, mode: Option<Mode>) {
        self.pinned = mode;
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned.is_some()
    }

    /// Feeds one sample and returns the mode to show. The first sample is adopted as-is; afterwards each
    /// mode enters once its condition has held ≥ 10 s and leaves once it has been false ≥ 30 s. If the clock
    /// goes backwards (replay restart, clock change), timers restart from `now_ms`.
    pub fn update(&mut self, now_ms: u64, sig: &ModeSignals) -> Mode {
        if !self.initialized {
            self.initialized = true;
            self.last_ms = now_ms;
            for m in Mode::PRIORITY {
                let on = sig.is(m);
                self.latches[m.index()] = Latch {
                    on,
                    true_since: on.then_some(now_ms),
                    false_since: (!on).then_some(now_ms),
                };
            }
            self.recompute();
            return self.current();
        }
        if now_ms < self.last_ms {
            for l in &mut self.latches {
                l.true_since = l.true_since.map(|_| now_ms);
                l.false_since = l.false_since.map(|_| now_ms);
            }
        }
        self.last_ms = now_ms;
        for m in Mode::PRIORITY {
            let l = &mut self.latches[m.index()];
            if sig.is(m) {
                l.false_since = None;
                let since = *l.true_since.get_or_insert(now_ms);
                if !l.on && now_ms.saturating_sub(since) >= ENTER_AFTER_MS {
                    l.on = true;
                }
            } else {
                l.true_since = None;
                let since = *l.false_since.get_or_insert(now_ms);
                if l.on && now_ms.saturating_sub(since) >= LEAVE_AFTER_MS {
                    l.on = false;
                }
            }
        }
        self.recompute();
        self.current()
    }

    fn recompute(&mut self) {
        self.current = Mode::PRIORITY
            .into_iter()
            .find(|m| self.latches[m.index()].on)
            .unwrap_or(Mode::Calm);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Group, Measured, ModelServer, Psi};
    use crate::units::GIB;

    fn sig(pressure: bool, throttle: bool) -> ModeSignals {
        ModeSignals {
            pressure,
            throttle,
            ..Default::default()
        }
    }

    #[test]
    fn hysteresis() {
        let mut m = ModeMachine::new();
        let p = sig(true, false);
        let calm = ModeSignals::default();
        assert_eq!(
            ModeMachine::new().update(0, &p),
            Mode::Pressure,
            "first sample adopted"
        );
        assert_eq!(m.update(0, &calm), Mode::Calm);
        assert_eq!(m.update(0, &p), Mode::Calm);
        assert_eq!(m.update(5_000, &p), Mode::Calm);
        assert_eq!(m.update(10_000, &p), Mode::Pressure);
        assert_eq!(m.update(20_000, &calm), Mode::Pressure);
        assert_eq!(m.update(45_000, &calm), Mode::Pressure);
        assert_eq!(m.update(50_000, &calm), Mode::Calm);
        m.pin(Some(Mode::Throttle));
        assert!(m.is_pinned());
        assert_eq!(m.update(60_000, &calm), Mode::Throttle);
        assert_eq!(m.underlying(), Mode::Calm);
        m.pin(None);
        assert_eq!(m.current(), Mode::Calm);
        assert_eq!(Mode::parse("Leftovers"), Some(Mode::Leftovers));
    }

    #[test]
    fn brief_blips_never_enter_and_short_calm_never_leaves() {
        let mut m = ModeMachine::new();
        m.update(0, &ModeSignals::default());
        // 8 s of pressure, 2 s calm, 8 s pressure: never 10 s continuous.
        let mut t = 0;
        for _ in 0..5 {
            for _ in 0..4 {
                t += 2_000;
                assert_eq!(m.update(t, &sig(true, false)), Mode::Calm);
            }
            t += 2_000;
            assert_eq!(m.update(t, &ModeSignals::default()), Mode::Calm);
        }
        // Enter, then flicker calm for < 30 s repeatedly: stays.
        for _ in 0..6 {
            t += 2_000;
            m.update(t, &sig(true, false));
        }
        assert_eq!(m.current(), Mode::Pressure);
        for _ in 0..5 {
            for _ in 0..10 {
                t += 2_000;
                assert_eq!(m.update(t, &ModeSignals::default()), Mode::Pressure);
            }
            t += 2_000;
            m.update(t, &sig(true, false));
        }
    }

    #[test]
    fn lower_latched_mode_shows_when_higher_clears() {
        let mut m = ModeMachine::new();
        assert_eq!(m.update(0, &sig(true, true)), Mode::Pressure);
        assert_eq!(m.latched(), vec![Mode::Pressure, Mode::Throttle]);
        // Pressure clears; throttle persists → shows throttle 30 s later.
        assert_eq!(m.update(10_000, &sig(false, true)), Mode::Pressure);
        assert_eq!(m.update(40_000, &sig(false, true)), Mode::Throttle);
    }

    #[test]
    fn clock_going_backwards_restarts_timers() {
        let mut m = ModeMachine::new();
        m.update(100_000, &ModeSignals::default());
        m.update(105_000, &sig(true, false));
        // Replay restarts at t=0: needs a full 10 s again.
        assert_eq!(m.update(0, &sig(true, false)), Mode::Calm);
        assert_eq!(m.update(9_000, &sig(true, false)), Mode::Calm);
        assert_eq!(m.update(10_000, &sig(true, false)), Mode::Pressure);
    }

    #[test]
    fn per_os_triggers() {
        let h = Headroom {
            headroom: Some(4 * GIB as i64),
            ..Default::default()
        };
        // Linux: PSI > 20 % → pressure; memorystatus-style level alone is not used while PSI is readable.
        let mut s = Snapshot::default();
        s.host.os = OsKind::Linux;
        s.memory.psi = Measured::exact(
            Psi {
                some_avg10: 31.0,
                ..Default::default()
            },
            "/proc/pressure/memory",
        );
        assert!(signals(&s, &h).pressure);
        s.memory.psi = Measured::exact(
            Psi {
                some_avg10: 5.0,
                ..Default::default()
            },
            "/proc/pressure/memory",
        );
        s.memory.pressure = Measured::estimate(PressureLevel::Warn, "derived");
        assert!(!signals(&s, &h).pressure);
        // macOS: memorystatus ≥ warn.
        let mut s = Snapshot::default();
        s.host.os = OsKind::Macos;
        s.memory.pressure = Measured::exact(PressureLevel::Warn, "kern.memorystatus");
        assert!(signals(&s, &h).pressure);
        // macOS thermal heavy → throttle; Linux ignores thermal pressure but uses trip points.
        s.memory.pressure = Measured::exact(PressureLevel::Normal, "kern.memorystatus");
        s.thermal.pressure = Measured::exact(ThermalPressure::Heavy, "notify");
        assert!(signals(&s, &h).throttle);
        s.host.os = OsKind::Linux;
        assert!(!signals(&s, &h).throttle);
        s.thermal.trip_point_hit = Measured::exact(true, "thermal_zone");
        assert!(signals(&s, &h).throttle);
        // Idle machine: throttle factor unavailable ("idle") never triggers.
        let mut s = Snapshot::default();
        s.thermal.throttle_factor = Measured::unavailable("throttle", "idle");
        assert_eq!(signals(&s, &h), ModeSignals::default());
    }

    #[test]
    fn working_and_leftovers() {
        let h = Headroom::default();
        let mut s = Snapshot::default();
        s.groups.push(Group {
            id: "model:sd".into(),
            kind: GroupKind::ModelServer,
            label: "sd-server".into(),
            ..Default::default()
        });
        s.model_servers.push(ModelServer {
            id: "sd".into(),
            group_id: Some("model:sd".into()),
            busy: Measured::exact(true, "adapter"),
            ..Default::default()
        });
        assert!(signals(&s, &h).working);
        s.model_servers[0].busy = Measured::exact(false, "adapter");
        s.groups[0].totals.cpu_pct = Measured::exact(300.0, "cpu");
        assert!(
            !signals(&s, &h).working,
            "adapter says idle; CPU alone doesn't override it"
        );
        s.groups.push(Group {
            id: "daemon:gradle".into(),
            kind: GroupKind::BuildDaemon,
            label: "GradleDaemon".into(),
            idle: true,
            ..Default::default()
        });
        let sig = signals(&s, &h);
        assert!(sig.leftovers && !sig.working);
        s.groups[1].idle = false;
        s.groups[1].totals.cpu_pct = Measured::exact(180.0, "cpu");
        let sig = signals(&s, &h);
        assert!(sig.working && !sig.leftovers);
        let r = reasons(&s, &h);
        assert!(r
            .iter()
            .any(|r| r.key == "build_active" && r.text == "GradleDaemon building"));
    }
}

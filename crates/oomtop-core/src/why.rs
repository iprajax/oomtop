//! `oomtop why` / MCP `explain_slowdown` (SPEC §9): ranked likely causes of slowness / memory trouble, each
//! with evidence lines ("swap in +600M/min for 5m", "GPU at 42% of max clock with thermal pressure heavy",
//! "Low Power Mode on", "on battery at 29%") and, where there is one, a concrete fix.
//!
//! Throttle-related causes (thermal, Low Power Mode, battery, GPU clocks) are reported **only under load**:
//! Apple Silicon down-clocks at idle and Low Power Mode costs nothing while nothing runs (acceptance #5).

use crate::headroom::is_reclaim_candidate;
use crate::history::History;
use crate::model::{ForecastTarget, GroupKind, OomKiller, PressureLevel, Snapshot, ThermalPressure};
use crate::throttle::{is_busy, speed_label, unit_factor, BUSY_PCT, THROTTLED_BELOW};
use crate::units::{format_bytes_short, format_duration};
use serde::{Deserialize, Serialize};

/// Swap traffic (in + out per minute) above which swapping is reported as a cause.
pub const SWAP_STORM_BYTES_PER_MIN: u64 = 100 * 1024 * 1024;
/// Linux PSI memory `some avg10` above which memory pressure is reported (UX §3).
pub const PSI_SOME_PCT: f64 = 20.0;
/// Battery charge below which being on battery is reported under load.
pub const LOW_BATTERY_PCT: f64 = 30.0;
/// Whole-machine CPU utilization above which the CPU is reported as saturated.
pub const CPU_SATURATED_PCT: f64 = 90.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CauseKind {
    OomImminent,
    SwapStorm,
    MemoryPressure,
    ThermalThrottle,
    LowPowerMode,
    LowBattery,
    GpuThrottle,
    CpuSaturated,
}

impl CauseKind {
    /// True for causes that are only meaningful under load (never reported on an idle machine).
    pub fn is_throttle_related(self) -> bool {
        matches!(
            self,
            CauseKind::ThermalThrottle
                | CauseKind::LowPowerMode
                | CauseKind::LowBattery
                | CauseKind::GpuThrottle
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cause {
    pub kind: CauseKind,
    /// 0..1, higher = more likely / more severe. Causes are sorted by this, descending.
    pub score: f64,
    pub title: String,
    pub evidence: Vec<String>,
    /// Suggested fix, e.g. "stop 2 idle build daemons (≈5.9G)".
    pub fix: Option<String>,
}

fn thermal_label(p: ThermalPressure) -> &'static str {
    match p {
        ThermalPressure::Nominal => "nominal",
        ThermalPressure::Moderate => "moderate",
        ThermalPressure::Heavy => "heavy",
        ThermalPressure::Trapping => "trapping",
        ThermalPressure::Sleeping => "sleeping",
    }
}

fn killer_label(k: OomKiller) -> &'static str {
    match k {
        OomKiller::Kernel => "the kernel OOM killer",
        OomKiller::SystemdOomd => "systemd-oomd",
        OomKiller::Earlyoom => "earlyoom",
        OomKiller::Jetsam => "jetsam",
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// True if anything is working hard enough for clocks/power to matter: a busy CPU cluster or GPU unit, an
/// available throttle factor, a busy accelerator, or ≥ 50 % whole-machine CPU.
pub fn under_load(s: &Snapshot) -> bool {
    s.thermal.throttle_factor.value.is_some()
        || s.thermal.clusters.iter().any(is_busy)
        || s.accelerators
            .iter()
            .any(|a| a.util_pct.value.map(|u| u >= BUSY_PCT).unwrap_or(false))
        || s.cpu.total_pct.value.map(|c| c >= BUSY_PCT).unwrap_or(false)
}

/// "stop 2 idle build daemons + 1 orphan (≈5.9G)" from the snapshot's reclaim candidates.
pub fn reclaim_fix(s: &Snapshot) -> Option<String> {
    let cands: Vec<_> = s
        .groups
        .iter()
        .filter(|g| is_reclaim_candidate(g))
        .filter_map(|g| Some((g, g.reclaim_gain.value.filter(|v| *v > 0)?)))
        .collect();
    if cands.is_empty() {
        return None;
    }
    let total: u64 = cands.iter().map(|(_, v)| *v).sum();
    let orphans = cands.iter().filter(|(g, _)| g.orphan).count();
    let daemons = cands
        .iter()
        .filter(|(g, _)| !g.orphan && g.kind == GroupKind::BuildDaemon)
        .count();
    let idle_daemons = cands
        .iter()
        .filter(|(g, _)| !g.orphan && g.kind == GroupKind::BuildDaemon && g.idle)
        .count();
    let models = cands
        .iter()
        .filter(|(g, _)| !g.orphan && g.kind == GroupKind::ModelServer)
        .count();
    let mut parts = Vec::new();
    if daemons > 0 {
        parts.push(if idle_daemons == daemons {
            plural(daemons, "idle build daemon", "idle build daemons")
        } else {
            plural(daemons, "build daemon", "build daemons")
        });
    }
    if models > 0 {
        parts.push(plural(models, "idle model server", "idle model servers"));
    }
    if orphans > 0 {
        parts.push(plural(orphans, "orphan", "orphans"));
    }
    Some(format!(
        "stop {} (≈{})",
        parts.join(" + "),
        format_bytes_short(total)
    ))
}

/// "largest: sd-server 9.9G, Google Chrome 2.1G".
fn largest_line(s: &Snapshot) -> Option<String> {
    let mut gs: Vec<_> = s
        .groups
        .iter()
        .filter_map(|g| Some((g, g.totals.footprint.value?)))
        .filter(|(_, v)| *v > 0)
        .collect();
    gs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
    let top: Vec<String> = gs
        .iter()
        .take(3)
        .map(|(g, v)| {
            format!(
                "{} {}{}",
                g.label,
                format_bytes_short(*v),
                if g.lower_bound { "+" } else { "" }
            )
        })
        .collect();
    (!top.is_empty()).then(|| format!("largest: {}", top.join(", ")))
}

/// How long swap used has been rising (seconds), from the history tail.
fn swap_growth_s(history: &History) -> Option<(u64, u64)> {
    let pts: Vec<(u64, u64)> = history
        .iter()
        .filter_map(|p| Some((p.t_ms, p.swap_used?)))
        .collect();
    let (&(t_last, v_last), rest) = pts.split_last()?;
    let mut start = (t_last, v_last);
    let mut next_v = v_last;
    for &(t, v) in rest.iter().rev() {
        // tolerate small dips (1 %) so sampling noise doesn't cut the run
        if v > next_v.saturating_add(next_v / 100) {
            break;
        }
        start = (t, v);
        next_v = v;
    }
    let dur = t_last.saturating_sub(start.0) / 1000;
    let grown = v_last.saturating_sub(start.1);
    (dur >= 60 && grown > 0).then_some((dur, grown))
}

/// True if `evidence` says nothing the title does not already say (case-insensitive, ignoring "is").
fn title_repeats(title: &str, evidence: &str) -> bool {
    let norm = |x: &str| {
        x.to_lowercase()
            .split_whitespace()
            .filter(|w| *w != "is")
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (t, e) = (norm(title), norm(evidence));
    !e.is_empty() && t.contains(&e)
}

/// Ranks causes. `history` is used for sustained trends (e.g. swap growth over minutes).
pub fn explain(s: &Snapshot, history: &History) -> Vec<Cause> {
    let mut out = Vec::new();
    let mem = &s.memory;
    let reclaim = reclaim_fix(s);
    let loaded = under_load(s);

    // --- OOM forecast --------------------------------------------------------------------------------
    if let Some(f) = &s.oom.forecast {
        let eta = format_duration(f.eta_s);
        let title = match f.target {
            ForecastTarget::SwapExhaustion => format!("Swap full in ~{eta} at this rate"),
            ForecastTarget::AvailableExhaustion => format!("Out of memory in ~{eta} at this rate"),
            ForecastTarget::KillerThreshold => format!(
                "{} acts in ~{eta} at this rate",
                f.killer.map(killer_label).unwrap_or("the OOM killer")
            ),
        };
        let rate = format_bytes_short(f.rate_per_min.abs() as u64);
        let trend = match f.target {
            ForecastTarget::SwapExhaustion => format!("swap used +{rate}/min"),
            ForecastTarget::AvailableExhaustion => format!("available −{rate}/min"),
            ForecastTarget::KillerThreshold => format!("{rate}/min toward the threshold"),
        };
        let mut evidence = vec![format!(
            "{trend} over {} (R² {:.2})",
            format_duration(f.window_s),
            f.confidence
        )];
        if let Some(v) = &s.oom.likely_victim {
            evidence.push(format!("likely victim: {} ({})", v.name, v.reason));
        }
        out.push(Cause {
            kind: CauseKind::OomImminent,
            score: 1.0,
            title,
            evidence,
            fix: reclaim.clone(),
        });
    }

    // --- swap storm ------------------------------------------------------------------------------------
    let swap_in = mem.swap_in_per_min.value.unwrap_or(0);
    let swap_out = mem.swap_out_per_min.value.unwrap_or(0);
    let traffic = swap_in.saturating_add(swap_out);
    let rates_known = mem.swap_in_per_min.value.is_some() || mem.swap_out_per_min.value.is_some();
    let growth = swap_growth_s(history);
    // Without swap-in/out rates, sustained growth of swap used at storm pace is the evidence.
    let growth_rate = growth
        .map(|(d, grown)| grown.saturating_mul(60) / d.max(1))
        .unwrap_or(0);
    if !rates_known && growth_rate >= SWAP_STORM_BYTES_PER_MIN {
        let (d, grown) = growth.unwrap_or_default();
        let mut ev = vec![format!(
            "swap used +{} in {} (≈{}/min; swap in/out rates unavailable)",
            format_bytes_short(grown),
            format_duration(d),
            format_bytes_short(growth_rate)
        )];
        if let (Some(u), Some(t)) = (mem.swap_used.value, mem.swap_total.value) {
            ev.push(format!(
                "swap {} / {}",
                format_bytes_short(u),
                format_bytes_short(t)
            ));
        }
        ev.extend(largest_line(s));
        let gib = growth_rate as f64 / (1024.0 * 1024.0 * 1024.0);
        out.push(Cause {
            kind: CauseKind::SwapStorm,
            score: (0.45 + gib * 0.4 + (d as f64 / 600.0).min(0.1)).clamp(0.45, 0.95),
            title: "Swapping heavily".into(),
            evidence: ev,
            fix: reclaim.clone(),
        });
    }
    if traffic >= SWAP_STORM_BYTES_PER_MIN {
        let dur = growth
            .map(|(d, _)| format!(" for {}", format_duration(d)))
            .unwrap_or_default();
        let mut ev = vec![format!(
            "swap in +{}/min, out +{}/min{dur}",
            format_bytes_short(swap_in),
            format_bytes_short(swap_out)
        )];
        if let Some((d, grown)) = growth {
            ev.push(format!(
                "swap used +{} in {}",
                format_bytes_short(grown),
                format_duration(d)
            ));
        }
        if let (Some(u), Some(t)) = (mem.swap_used.value, mem.swap_total.value) {
            ev.push(format!(
                "swap {} / {}",
                format_bytes_short(u),
                format_bytes_short(t)
            ));
        }
        ev.extend(largest_line(s));
        let gib = traffic as f64 / (1024.0 * 1024.0 * 1024.0);
        let sustained = growth.map(|(d, _)| (d as f64 / 600.0).min(0.1)).unwrap_or(0.0);
        out.push(Cause {
            kind: CauseKind::SwapStorm,
            score: (0.45 + gib * 0.4 + sustained).clamp(0.45, 0.95),
            title: "Swapping heavily".into(),
            evidence: ev,
            fix: reclaim.clone(),
        });
    }

    // --- memory pressure -------------------------------------------------------------------------------
    let pressure = mem.pressure.value.unwrap_or_default();
    let psi = mem.psi.value.map(|p| p.some_avg10).unwrap_or(0.0);
    let psi_full = mem.psi.value.map(|p| p.full_avg10).unwrap_or(0.0);
    if pressure >= PressureLevel::Warn || psi > PSI_SOME_PCT {
        let mut ev = Vec::new();
        if pressure >= PressureLevel::Warn {
            ev.push(format!(
                "memory pressure {}",
                if pressure == PressureLevel::Critical {
                    "critical"
                } else {
                    "warn"
                }
            ));
        }
        if let Some(level) = mem.memorystatus_level.value {
            ev.push(format!("memorystatus level {level}% available"));
        }
        if mem.psi.value.is_some() && psi > 0.0 {
            ev.push(format!("memory PSI some {psi:.0}%, full {psi_full:.0}% (10 s)"));
        }
        if let Some(a) = mem.available.value {
            ev.push(format!("available {}", format_bytes_short(a)));
        }
        if let (Some(u), Some(t)) = (mem.swap_used.value, mem.swap_total.value) {
            ev.push(format!(
                "swap {} / {}",
                format_bytes_short(u),
                format_bytes_short(t)
            ));
        }
        ev.extend(largest_line(s));
        out.push(Cause {
            kind: CauseKind::MemoryPressure,
            score: if pressure == PressureLevel::Critical {
                0.9
            } else {
                (0.6 + (psi / 250.0).min(0.25) + (psi_full / 500.0).min(0.05)).min(0.9)
            },
            title: if pressure == PressureLevel::Critical {
                "Critical memory pressure".into()
            } else {
                "Memory pressure".into()
            },
            evidence: ev,
            fix: reclaim.clone(),
        });
    }

    // --- throttling (only under load) ------------------------------------------------------------------
    let t = &s.thermal;
    let on_battery = t.on_battery.value == Some(true);
    let battery = t.battery_pct.value;
    if loaded {
        let factor = t.throttle_factor.value;
        let tp = t.pressure.value;
        let trip = t.trip_point_hit.value == Some(true);
        let hot = tp.map(|p| p >= ThermalPressure::Heavy).unwrap_or(false) || trip;
        let slow = factor.map(|f| f < THROTTLED_BELOW).unwrap_or(false);
        let lpm = t.low_power_mode.value == Some(true);
        // Any thermal evidence: a fanless Mac already clocks down at *moderate* pressure.
        let warm = tp.map(|p| p >= ThermalPressure::Moderate).unwrap_or(false) || trip;
        // Slow clocks with Low Power Mode on and no thermal evidence are Low Power Mode, not heat.
        if hot || (slow && (warm || !lpm)) {
            let mut ev = Vec::new();
            if let Some(f) = factor {
                ev.push(speed_label(f));
            }
            let with_pressure = tp
                .filter(|p| *p >= ThermalPressure::Moderate)
                .map(|p| format!(" with thermal pressure {}", thermal_label(p)))
                .unwrap_or_default();
            for u in &t.clusters {
                if let Some(uf) = unit_factor(u).filter(|f| *f < THROTTLED_BELOW) {
                    ev.push(format!(
                        "{} at {:.0}% of max clock ({:.0}% busy){with_pressure}",
                        u.name,
                        uf * 100.0,
                        u.active_pct
                    ));
                }
            }
            if let Some(p) = tp {
                ev.push(format!("thermal pressure {}", thermal_label(p)));
            }
            if trip {
                ev.push("thermal trip point hit".into());
            }
            if let Some(hottest) = t
                .temps
                .iter()
                .filter(|x| x.celsius.is_finite())
                .max_by(|a, b| a.celsius.total_cmp(&b.celsius))
            {
                ev.push(format!(
                    "hottest sensor {} {:.0}°C",
                    hottest.name, hottest.celsius
                ));
            }
            if let Some(w) = t.package_power_w.value {
                ev.push(format!("package power {w:.1} W"));
            }
            if on_battery {
                ev.push(match battery {
                    Some(b) => format!("on battery at {b:.0}%"),
                    None => "on battery".into(),
                });
            }
            // "running at 49% speed" outranks a mere pressure warning, but not a swap storm
            let base = factor
                .map(|f| (0.35 + (1.0 - f) * 0.6).clamp(0.3, 0.85))
                .unwrap_or(0.5);
            let bump = if hot { 0.05 } else { 0.0 };
            out.push(Cause {
                kind: CauseKind::ThermalThrottle,
                score: (base + bump).min(0.9),
                title: match (factor, warm) {
                    (Some(f), true) if slow => format!("Thermal throttling — {}", speed_label(f)),
                    (_, true) => "Thermal throttling".into(),
                    // clocks are down without any thermal evidence (power limits, governor, unknown)
                    (Some(f), false) => format!("Clocks reduced — {}", speed_label(f)),
                    (None, false) => "Clocks reduced".into(),
                },
                evidence: ev,
                fix: Some(match (warm, on_battery) {
                    (true, true) => "plug in and pause heavy work until it cools".into(),
                    (true, false) => "improve airflow or pause heavy work until it cools".into(),
                    (false, true) => "plug in or pause heavy work".into(),
                    (false, false) => "pause heavy work or check the power profile".into(),
                }),
            });
        }

        if lpm {
            let mut ev = vec!["Low Power Mode on".to_string()];
            if let Some(f) = factor {
                ev.push(speed_label(f));
            }
            ev.extend(t.clusters.iter().filter_map(|u| {
                let uf = unit_factor(u).filter(|f| *f < THROTTLED_BELOW)?;
                Some(format!(
                    "{} at {:.0}% of max clock ({:.0}% busy)",
                    u.name,
                    uf * 100.0,
                    u.active_pct
                ))
            }));
            // the main cause when clocks are down without heat
            let score = match (slow, warm) {
                (true, false) => factor
                    .map(|f| (0.4 + (1.0 - f) * 0.6).clamp(0.6, 0.85))
                    .unwrap_or(0.6),
                (true, true) => 0.6,
                _ => 0.5,
            };
            out.push(Cause {
                kind: CauseKind::LowPowerMode,
                score,
                title: "Low Power Mode is on".into(),
                evidence: ev,
                fix: Some("turn off Low Power Mode (System Settings → Battery)".into()),
            });
        }

        if on_battery {
            if let Some(pct) = battery.filter(|p| *p < LOW_BATTERY_PCT) {
                let mut ev = vec![format!("on battery at {pct:.0}%")];
                if let Some(w) = t.adapter_watts.value.filter(|w| *w > 0.0) {
                    ev.push(format!("adapter {w:.0} W"));
                }
                out.push(Cause {
                    kind: CauseKind::LowBattery,
                    score: if slow { 0.45 } else { 0.35 },
                    title: "On battery, low charge".into(),
                    evidence: ev,
                    fix: Some("plug in".into()),
                });
            }
        }

        for a in &s.accelerators {
            let busy = a.util_pct.value.map(|u| u >= BUSY_PCT).unwrap_or(false);
            if !busy {
                continue;
            }
            if let (Some(c), Some(m)) = (a.clock_mhz.value, a.max_clock_mhz.value) {
                if m > 0.0 && c / m < THROTTLED_BELOW {
                    let ratio = (c / m).clamp(0.0, 1.0);
                    let with_pressure = tp
                        .filter(|p| *p >= ThermalPressure::Moderate)
                        .map(|p| format!(" with thermal pressure {}", thermal_label(p)))
                        .unwrap_or_default();
                    let mut ev = vec![format!(
                        "{} at {:.0}% of max clock{with_pressure}",
                        a.name,
                        ratio * 100.0
                    )];
                    if let Some(temp) = a.temp_c.value {
                        ev.push(format!("{} at {temp:.0}°C", a.name));
                    }
                    if let Some(w) = a.power_w.value {
                        ev.push(format!("{} drawing {w:.0} W", a.name));
                    }
                    ev.extend(a.throttle_reasons.iter().map(|r| format!("throttle reason: {r}")));
                    out.push(Cause {
                        kind: CauseKind::GpuThrottle,
                        score: (1.0 - ratio).clamp(0.3, 0.85),
                        title: format!("{} is throttled", if a.name.is_empty() { "GPU" } else { &a.name }),
                        evidence: ev,
                        fix: None,
                    });
                }
            }
        }
    }

    // --- CPU saturation --------------------------------------------------------------------------------
    if let Some(cpu) = s.cpu.total_pct.value.filter(|c| *c > CPU_SATURATED_PCT) {
        let mut ev = vec![format!("CPU {cpu:.0}% of all cores")];
        let mut top: Vec<_> = s
            .groups
            .iter()
            .filter_map(|g| Some((g, g.totals.cpu_pct.value?)))
            .filter(|(_, c)| *c >= 10.0)
            .collect();
        top.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
        let names: Vec<String> = top
            .iter()
            .take(3)
            .map(|(g, c)| format!("{} {c:.0}%", g.label))
            .collect();
        if !names.is_empty() {
            ev.push(format!("busiest: {}", names.join(", ")));
        }
        if let Some(l) = s.cpu.load_avg_1.value {
            ev.push(format!("load average {l:.1}"));
        }
        out.push(Cause {
            kind: CauseKind::CpuSaturated,
            score: 0.4,
            title: "CPU saturated".into(),
            evidence: ev,
            fix: None,
        });
    }

    for c in &mut out {
        c.score = (c.score.clamp(0.0, 1.0) * 1000.0).round() / 1000.0;
        // The title already states it ("Clocks reduced — running at 35% speed", "Low Power Mode is on"):
        // an evidence bullet that only repeats the headline adds nothing.
        let title = c.title.clone();
        c.evidence.retain(|e| !title_repeats(&title, e));
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.kind.cmp(&b.kind)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ClusterFreq, Measured};

    #[test]
    fn idle_machine_reports_nothing_throttle_related() {
        let mut s = Snapshot::default();
        s.thermal.clusters = vec![ClusterFreq {
            name: "p".into(),
            cur_mhz: 600.0,
            max_mhz: 4000.0,
            active_pct: 2.0,
        }];
        s.thermal.throttle_factor = crate::throttle::throttle_factor(&s.thermal.clusters);
        s.thermal.low_power_mode = Measured::exact(true, "t");
        s.thermal.pressure = Measured::exact(ThermalPressure::Heavy, "t");
        s.thermal.on_battery = Measured::exact(true, "t");
        s.thermal.battery_pct = Measured::exact(12.0, "t");
        assert!(explain(&s, &History::default()).is_empty());
    }

    #[test]
    fn low_power_mode_is_not_called_heat() {
        let mut s = Snapshot::default();
        s.thermal.clusters = vec![ClusterFreq {
            name: "P-cluster".into(),
            cur_mhz: 1800.0,
            max_mhz: 4000.0,
            active_pct: 90.0,
        }];
        s.thermal.throttle_factor = crate::throttle::throttle_factor(&s.thermal.clusters);
        s.thermal.low_power_mode = Measured::exact(true, "t");
        s.thermal.pressure = Measured::exact(ThermalPressure::Nominal, "t");
        let c = explain(&s, &History::default());
        assert_eq!(c[0].kind, CauseKind::LowPowerMode, "{c:#?}");
        assert!(c[0].evidence.iter().any(|e| e.starts_with("running at 45%")));
        assert!(c.iter().all(|c| c.kind != CauseKind::ThermalThrottle));
        // with heat as well, both are reported and heat is called heat
        s.thermal.pressure = Measured::exact(ThermalPressure::Heavy, "t");
        let c = explain(&s, &History::default());
        let th = c.iter().find(|c| c.kind == CauseKind::ThermalThrottle).unwrap();
        assert!(th.title.starts_with("Thermal throttling"));
        assert!(c.iter().any(|c| c.kind == CauseKind::LowPowerMode));
        // a fanless Mac at *moderate* pressure with slow clocks is thermal throttling
        s.thermal.low_power_mode = Measured::exact(false, "t");
        s.thermal.pressure = Measured::exact(ThermalPressure::Moderate, "t");
        let c = explain(&s, &History::default());
        assert_eq!(c[0].title, "Thermal throttling — running at 45% speed");
        // slow clocks without heat or LPM: "clocks reduced", not "thermal"
        s.thermal.pressure = Measured::exact(ThermalPressure::Nominal, "t");
        s.thermal.low_power_mode = Measured::exact(false, "t");
        let c = explain(&s, &History::default());
        assert_eq!(c[0].title, "Clocks reduced — running at 45% speed");
        assert_eq!(
            c[0].evidence,
            [
                "P-cluster at 45% of max clock (90% busy)",
                "thermal pressure nominal"
            ],
            "the headline is not repeated as the first bullet"
        );
    }

    #[test]
    fn evidence_never_repeats_the_title() {
        assert!(title_repeats(
            "Clocks reduced — running at 35% speed",
            "running at 35% speed"
        ));
        assert!(title_repeats("Low Power Mode is on", "Low Power Mode on"));
        assert!(!title_repeats("Low Power Mode is on", "running at 35% speed"));
        assert!(!title_repeats("Swapping heavily", ""));
        let mut s = Snapshot::default();
        s.thermal.clusters = vec![ClusterFreq {
            name: "P-cluster".into(),
            cur_mhz: 1800.0,
            max_mhz: 4000.0,
            active_pct: 90.0,
        }];
        s.thermal.throttle_factor = crate::throttle::throttle_factor(&s.thermal.clusters);
        s.thermal.low_power_mode = Measured::exact(true, "t");
        for c in explain(&s, &History::default()) {
            for e in &c.evidence {
                assert!(!title_repeats(&c.title, e), "{c:?}");
            }
        }
    }

    #[test]
    fn swap_storm_from_history_when_rates_are_unavailable() {
        use crate::history::HistoryPoint;
        let mut h = History::default();
        let t0 = 1_000_000u64;
        for k in 0..11u64 {
            h.push(HistoryPoint {
                t_ms: t0 + k * 30_000,
                swap_used: Some((1024 + k * 150) * 1024 * 1024),
                ..Default::default()
            });
        }
        let mut s = Snapshot {
            taken_at_ms: t0 + 300_000,
            ..Default::default()
        };
        s.memory.swap_in_per_min = Measured::unavailable("vm_stat", "n/a");
        let c = explain(&s, &h);
        let sw = c
            .iter()
            .find(|c| c.kind == CauseKind::SwapStorm)
            .expect("swap storm");
        assert!(sw.evidence[0].contains("in 5m"), "{:?}", sw.evidence);
        assert!(sw.evidence[0].contains("rates unavailable"));
        // with measured (quiet) rates, growth alone does not duplicate the cause
        s.memory.swap_in_per_min = Measured::exact(0, "t");
        s.memory.swap_out_per_min = Measured::exact(0, "t");
        assert!(explain(&s, &h).iter().all(|c| c.kind != CauseKind::SwapStorm));
    }

    #[test]
    fn throttled_run_and_swap() {
        let mut s = Snapshot::default();
        s.thermal.throttle_factor = Measured::estimate(0.45, "t");
        s.thermal.pressure = Measured::exact(ThermalPressure::Heavy, "t");
        s.memory.swap_in_per_min = Measured::exact(600 * 1024 * 1024, "t");
        let c = explain(&s, &History::default());
        let kinds: Vec<CauseKind> = c.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&CauseKind::ThermalThrottle));
        assert!(kinds.contains(&CauseKind::SwapStorm));
        let th = c.iter().find(|c| c.kind == CauseKind::ThermalThrottle).unwrap();
        // the headline carries the speed; the evidence does not repeat it
        assert_eq!(th.title, "Thermal throttling — running at 45% speed");
        assert!(!th.evidence.iter().any(|e| e == "running at 45% speed"), "{th:?}");
        assert!(th.evidence.contains(&"thermal pressure heavy".to_string()));
        assert!(c.windows(2).all(|w| w[0].score >= w[1].score));
    }
}

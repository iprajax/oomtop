//! OOM forecast (SPEC §8.3): which killer applies, its thresholds, a linear-trend ETA, the likely victim and
//! recent kills — all pure functions over history, config text and snapshots.
//!
//! Forecast rules:
//! - Linear least-squares fit of the last 2–5 min of available memory, swap used and PSI (every window of
//!   5, 4, 3 and 2 min is tried; the earliest qualifying ETA wins) against the nearest applicable threshold.
//! - Shown only when the trend is consistent: R² ≥ 0.6, ≥ 8 points over ≥ 2 min, both halves of the window
//!   trend the same way, and no single step accounts for more than half of the change — so a single spike
//!   (or a one-off step such as a model load) never raises an alarm. ETA must be < 30 min.
//! - Killer semantics: the kernel, systemd-oomd and jetsam act when **any** of their rules is met (oomd
//!   pressure adds its sustain duration); earlyoom acts only when **all** of its thresholds are met
//!   (available memory *and* free swap), so its ETA is the latest of its per-metric ETAs. The systemd-oomd
//!   swap rule is itself an AND: memory used *and* swap used both above `SwapUsedLimit` (systemd
//!   `oomd_mem_available_below && oomd_swap_free_below`); the memory half is checked when RAM size is known.
//! - A series whose newest value is older than [`MAX_STALE_S`] is not forecast; a slightly stale one has its
//!   ETA measured from the newest history point, not from its own last value.
//! - macOS swap is dynamic (swap files are added on demand), so "swap used → swap total" is not a limit
//!   when jetsam is the killer; the ceiling there is `swap_limit` (swap used + free space on the swap
//!   volume — jetsam kills when swap can't grow). With ample disk that ETA is far beyond 30 min: "stable".

use crate::history::{History, HistoryPoint};
use crate::model::{
    Forecast, ForecastTarget, GroupKind, Oom, OomKill, OomKiller, OomThreshold, OsKind, ProcId, Snapshot,
    ThresholdMetric, Victim,
};
use crate::units::{parse_duration_s, parse_percent};
use serde::{Deserialize, Serialize};

pub const MIN_R2: f64 = 0.6;
pub const MAX_ETA_S: f64 = 30.0 * 60.0;
/// Fit window (5 min) and minimum span of data required (2 min).
pub const WINDOW_MS: u64 = 5 * 60 * 1000;
pub const MIN_SPAN_S: f64 = 120.0;
/// Minimum number of points for a fit.
pub const MIN_POINTS: usize = 8;
/// Windows tried, longest first (SPEC §8.3 "last 2–5 min").
pub const WINDOWS_MS: [u64; 4] = [5 * 60_000, 4 * 60_000, 3 * 60_000, 2 * 60_000];
/// A single step may account for at most this share of the fitted change over the window.
pub const MAX_STEP_SHARE: f64 = 0.5;
/// A metric whose newest sample is older than this (relative to the newest history point) is not forecast.
pub const MAX_STALE_S: f64 = 30.0;

/// systemd-oomd defaults (oomd.conf(5)).
pub const OOMD_DEFAULT_SWAP_USED_LIMIT_PCT: f64 = 90.0;
pub const OOMD_DEFAULT_MEM_PRESSURE_LIMIT_PCT: f64 = 60.0;
pub const OOMD_DEFAULT_MEM_PRESSURE_DURATION_S: u32 = 30;
/// earlyoom defaults (`-m 10 -s 10`; SIGKILL at half).
pub const EARLYOOM_DEFAULT_MEM_PCT: f64 = 10.0;
pub const EARLYOOM_DEFAULT_SWAP_PCT: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct LinearFit {
    /// Units per second.
    pub slope: f64,
    pub intercept: f64,
    pub r2: f64,
    /// Time span covered, seconds.
    pub span_s: f64,
    pub n: usize,
}

impl LinearFit {
    /// Fitted value at time `t`.
    pub fn at(&self, t: f64) -> f64 {
        self.intercept + self.slope * t
    }
}

/// Least-squares fit of `(t_s, y)` points.
pub fn linear_fit(points: &[(f64, f64)]) -> Option<LinearFit> {
    let n = points.len();
    if n < 2 {
        return None;
    }
    if points.iter().any(|(t, y)| !t.is_finite() || !y.is_finite()) {
        return None;
    }
    let nf = n as f64;
    let mean_t = points.iter().map(|p| p.0).sum::<f64>() / nf;
    let mean_y = points.iter().map(|p| p.1).sum::<f64>() / nf;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    let mut syy = 0.0;
    for (t, y) in points {
        sxx += (t - mean_t).powi(2);
        sxy += (t - mean_t) * (y - mean_y);
        syy += (y - mean_y).powi(2);
    }
    if sxx == 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    let intercept = mean_y - slope * mean_t;
    let r2 = if syy == 0.0 {
        0.0
    } else {
        (sxy * sxy) / (sxx * syy)
    };
    let span_s = points.last()?.0 - points.first()?.0;
    Some(LinearFit {
        slope,
        intercept,
        r2,
        span_s,
        n,
    })
}

/// Spike/step guard: both halves of the window trend in the fit's direction, no single step between
/// consecutive points — **in either direction** — accounts for more than [`MAX_STEP_SHARE`] of the fitted
/// change, and the newest value is still on the fitted line. A drop against the trend (an OOM kill, a stopped
/// hog, a reclaim) therefore clears the forecast instead of leaving a stale ETA.
pub fn trend_is_consistent(points: &[(f64, f64)], fit: &LinearFit) -> bool {
    if points.len() < MIN_POINTS || fit.slope == 0.0 {
        return false;
    }
    let total_change = (fit.slope * fit.span_s).abs();
    if total_change <= 0.0 {
        return false;
    }
    let max_step = points
        .windows(2)
        .map(|w| (w[1].1 - w[0].1).abs())
        .fold(0.0, f64::max);
    if max_step > MAX_STEP_SHARE * total_change {
        return false;
    }
    // The newest actual value must sit near the line (a recent reversal the fit hasn't caught up with).
    if let Some((t_last, y_last)) = points.last() {
        if (y_last - fit.at(*t_last)).abs() > MAX_STEP_SHARE * total_change {
            return false;
        }
    }
    let mid = points.len() / 2;
    let halves = [&points[..mid], &points[mid..]];
    halves.iter().all(|h| match linear_fit(h) {
        Some(f) => f.slope.signum() == fit.slope.signum() && f.slope != 0.0,
        None => false,
    })
}

/// ETA (seconds from the last point) for a series to reach `threshold`, rising (`rising = true`) or falling.
/// Returns `None` unless the trend is consistent, heading towards the threshold, and ETA < 30 min. The ETA
/// is measured from the newest **actual** value (not the fitted one) at the fitted rate.
pub fn eta_to_threshold(points: &[(f64, f64)], threshold: f64, rising: bool) -> Option<(f64, LinearFit)> {
    if points.len() < MIN_POINTS || !threshold.is_finite() {
        return None;
    }
    let fit = linear_fit(points)?;
    if fit.span_s < MIN_SPAN_S || fit.r2 < MIN_R2 {
        return None;
    }
    if !trend_is_consistent(points, &fit) {
        return None;
    }
    let (_, y_now) = *points.last()?;
    let towards = if rising {
        fit.slope > 0.0 && y_now < threshold
    } else {
        fit.slope < 0.0 && y_now > threshold
    };
    if !towards {
        return None;
    }
    let eta = (threshold - y_now) / fit.slope;
    (eta.is_finite() && eta > 0.0 && eta < MAX_ETA_S).then_some((eta, fit))
}

/// Tries every window in [`WINDOWS_MS`] and returns the earliest qualifying ETA (conservative: a trend that
/// only started recently is not diluted by the flat minutes before it).
pub fn eta_in_history(
    history: &History,
    f: impl Fn(&HistoryPoint) -> Option<f64>,
    threshold: f64,
    rising: bool,
) -> Option<(f64, LinearFit)> {
    let now_ms = history.last()?.t_ms;
    WINDOWS_MS
        .iter()
        .filter_map(|w| {
            // `series` is relative to the window's first point; points where the metric is missing are
            // dropped, so the series may end before "now".
            let t0 = history.window(*w).first()?.t_ms;
            let points = history.series(*w, &f);
            let stale_s = now_ms.saturating_sub(t0) as f64 / 1000.0 - points.last()?.0;
            if stale_s > MAX_STALE_S {
                return None;
            }
            let (eta, fit) = eta_to_threshold(&points, threshold, rising)?;
            let eta = eta - stale_s.max(0.0);
            (eta > 0.0).then_some((eta, fit))
        })
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
}

fn metric_value(p: &HistoryPoint, metric: ThresholdMetric, mem_total: Option<u64>) -> Option<f64> {
    match metric {
        ThresholdMetric::SwapUsedPct => {
            let t = p.swap_total.filter(|t| *t > 0)? as f64;
            Some(p.swap_used? as f64 / t * 100.0)
        }
        ThresholdMetric::MemPressurePct => p.psi_some_avg10,
        ThresholdMetric::AvailableBytes => p.available.map(|v| v as f64),
        ThresholdMetric::AvailablePct => {
            let t = mem_total.filter(|t| *t > 0)? as f64;
            Some(p.available? as f64 / t * 100.0)
        }
    }
}

/// Swap/available metrics fall towards their threshold or rise towards it.
fn metric_rising(metric: ThresholdMetric) -> bool {
    matches!(
        metric,
        ThresholdMetric::SwapUsedPct | ThresholdMetric::MemPressurePct
    )
}

/// Converts a fitted slope (units/s) into the forecast rate (bytes/min; 0 for PSI, which has no byte rate).
fn rate_per_min(metric: ThresholdMetric, slope: f64, swap_total: Option<u64>, mem_total: Option<u64>) -> f64 {
    let per_min = slope.abs() * 60.0;
    match metric {
        ThresholdMetric::AvailableBytes => per_min,
        ThresholdMetric::SwapUsedPct => swap_total.map(|t| per_min * t as f64 / 100.0).unwrap_or(0.0),
        ThresholdMetric::AvailablePct => mem_total.map(|t| per_min * t as f64 / 100.0).unwrap_or(0.0),
        ThresholdMetric::MemPressurePct => 0.0,
    }
}

/// Whether the threshold is already met at the latest point.
fn already_met(p: &HistoryPoint, th: &OomThreshold, mem_total: Option<u64>) -> Option<bool> {
    let v = metric_value(p, th.metric, mem_total)?;
    Some(if metric_rising(th.metric) {
        v >= th.value
    } else {
        v <= th.value
    })
}

/// ETA for one metric to reach `value`: `(0, None)` when already met at the newest point, `(eta, fit)` when
/// trending towards it, `None` otherwise.
fn metric_eta(
    history: &History,
    last: &HistoryPoint,
    metric: ThresholdMetric,
    value: f64,
    mem_total: Option<u64>,
) -> Option<(f64, Option<LinearFit>)> {
    let th = OomThreshold {
        metric,
        value,
        ..Default::default()
    };
    if already_met(last, &th, mem_total) == Some(true) {
        return Some((0.0, None));
    }
    let (eta, fit) = eta_in_history(
        history,
        |p| metric_value(p, metric, mem_total),
        value,
        metric_rising(metric),
    )?;
    Some((eta, Some(fit)))
}

/// Forecasts the nearest OOM target from history and the host's killer thresholds.
pub fn forecast_oom(history: &History, oom: &Oom) -> Option<Forecast> {
    forecast_oom_with(history, oom, None)
}

/// [`forecast_oom`] with the RAM size, which enables `AvailablePct` thresholds (earlyoom `-m`).
pub fn forecast_oom_with(history: &History, oom: &Oom, mem_total: Option<u64>) -> Option<Forecast> {
    let last = history.last()?;
    // Latest known swap size (one sample without swap data must not hide the swap forecast).
    let swap_total = history.iter().filter_map(|p| p.swap_total).last();
    let killers: Vec<OomKiller> = if oom.killers.is_empty() {
        vec![oom.killer]
    } else {
        oom.killers.clone()
    };
    let jetsam = killers.contains(&OomKiller::Jetsam);
    let base_killer = if killers.contains(&OomKiller::Kernel) {
        OomKiller::Kernel
    } else {
        oom.killer
    };
    let mut best: Option<Forecast> = None;
    let mut consider = |f: Forecast| {
        let better = match &best {
            None => true,
            Some(b) => f.eta_s < b.eta_s || (f.eta_s == b.eta_s && f.confidence > b.confidence),
        };
        if better {
            best = Some(f);
        }
    };

    // Swap exhaustion: swap_used → the swap ceiling. Fixed-size swap: swap_total. Jetsam hosts (macOS) add
    // swap files on demand, so the ceiling is `swap_limit` (swap used + free space on the swap volume).
    let ceiling = if jetsam {
        history.iter().filter_map(|p| p.swap_limit).last()
    } else {
        swap_total
    };
    if let Some(total) = ceiling.filter(|t| *t > 0) {
        if let Some((eta, fit)) =
            eta_in_history(history, |p| p.swap_used.map(|v| v as f64), total as f64, true)
        {
            consider(Forecast {
                target: ForecastTarget::SwapExhaustion,
                killer: Some(base_killer),
                eta_s: eta as u64,
                confidence: fit.r2,
                rate_per_min: fit.slope.abs() * 60.0,
                window_s: fit.span_s as u64,
            });
        }
    }
    // Available memory → 0.
    if let Some((eta, fit)) = eta_in_history(history, |p| p.available.map(|v| v as f64), 0.0, false) {
        consider(Forecast {
            target: ForecastTarget::AvailableExhaustion,
            killer: Some(base_killer),
            eta_s: eta as u64,
            confidence: fit.r2,
            rate_per_min: fit.slope.abs() * 60.0,
            window_s: fit.span_s as u64,
        });
    }

    // Killer thresholds.
    for killer in &killers {
        let ths: Vec<&OomThreshold> = oom.thresholds.iter().filter(|t| t.killer == *killer).collect();
        if ths.is_empty() {
            continue;
        }
        // Per threshold: (eta, fit of the metric that decides it, that metric).
        let per: Vec<Option<(f64, Option<LinearFit>, ThresholdMetric)>> = ths
            .iter()
            .map(|th| {
                // earlyoom without swap: "free swap below X %" always holds.
                let no_swap = th.metric == ThresholdMetric::SwapUsedPct && swap_total == Some(0);
                if no_swap && *killer == OomKiller::Earlyoom {
                    return Some((0.0, None, th.metric));
                }
                let (eta, fit) = metric_eta(history, last, th.metric, th.value, mem_total)?;
                let mut out = (eta, fit, th.metric);
                if *killer == OomKiller::SystemdOomd && th.metric == ThresholdMetric::SwapUsedPct {
                    // Swap rule = swap used ≥ limit AND memory used ≥ limit (available ≤ 100 − limit).
                    // Without the RAM size only the swap half can be checked (earlier = conservative).
                    if mem_total.is_some() {
                        let avail_pct = (100.0 - th.value).clamp(0.0, 100.0);
                        let (m_eta, m_fit) =
                            metric_eta(history, last, ThresholdMetric::AvailablePct, avail_pct, mem_total)?;
                        // The later half decides ("already met" is ETA 0). Both met → (0, None): oomd is
                        // acting now, which is not a trend forecast.
                        if m_eta > out.0 {
                            out = (m_eta, m_fit, ThresholdMetric::AvailablePct);
                        }
                    }
                }
                Some((out.0 + th.duration_s.unwrap_or(0) as f64, out.1, out.2))
            })
            .collect();
        let as_forecast = |eta: f64, fit: &LinearFit, metric: ThresholdMetric| Forecast {
            target: ForecastTarget::KillerThreshold,
            killer: Some(*killer),
            eta_s: eta as u64,
            confidence: fit.r2,
            rate_per_min: rate_per_min(metric, fit.slope, swap_total, mem_total),
            window_s: fit.span_s as u64,
        };
        if *killer == OomKiller::Earlyoom {
            // AND: every threshold must be met or forecast; ETA = latest; needs at least one real trend.
            if per.iter().any(|p| p.is_none()) {
                continue;
            }
            let all: Vec<(f64, Option<LinearFit>, ThresholdMetric)> = per.into_iter().flatten().collect();
            let latest = all
                .iter()
                .filter(|(_, fit, _)| fit.is_some())
                .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            if let Some((eta, Some(fit), metric)) = latest {
                if *eta < MAX_ETA_S {
                    let mut f = as_forecast(*eta, fit, *metric);
                    f.confidence = all
                        .iter()
                        .filter_map(|(_, fit, _)| fit.map(|x| x.r2))
                        .fold(1.0, f64::min);
                    consider(f);
                }
            }
        } else {
            // OR: the earliest trending threshold.
            for (eta, fit, metric) in per.into_iter().flatten() {
                if let Some(fit) = fit {
                    if eta < MAX_ETA_S {
                        consider(as_forecast(eta, &fit, metric));
                    }
                }
            }
        }
    }
    best
}

// -----------------------------------------------------------------------------------------------------
// Killer configuration (pure parsers; the collector reads the files / argv)
// -----------------------------------------------------------------------------------------------------

/// systemd-oomd global settings (`/etc/systemd/oomd.conf` + drop-ins, `[OOM]` section).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OomdConfig {
    pub swap_used_limit_pct: f64,
    pub mem_pressure_limit_pct: f64,
    pub mem_pressure_duration_s: u32,
    /// "default" or the last file that set a value.
    pub source: String,
}

impl Default for OomdConfig {
    fn default() -> Self {
        OomdConfig {
            swap_used_limit_pct: OOMD_DEFAULT_SWAP_USED_LIMIT_PCT,
            mem_pressure_limit_pct: OOMD_DEFAULT_MEM_PRESSURE_LIMIT_PCT,
            mem_pressure_duration_s: OOMD_DEFAULT_MEM_PRESSURE_DURATION_S,
            source: "systemd-oomd default".into(),
        }
    }
}

/// Iterates `key=value` pairs of an INI-style systemd file, yielding `(section, key, value)`.
fn systemd_pairs(text: &str) -> Vec<(String, String, String)> {
    let mut section = String::new();
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((section.clone(), k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

/// Parses oomd.conf layers in precedence order (main file first, then drop-ins alphabetically). Each layer
/// is `(origin, text)`. Invalid values are ignored (the previous value stays).
pub fn parse_oomd_conf(layers: &[(&str, &str)]) -> OomdConfig {
    let mut c = OomdConfig::default();
    for (origin, text) in layers {
        for (section, k, v) in systemd_pairs(text) {
            if section != "OOM" {
                continue;
            }
            match k.as_str() {
                "SwapUsedLimit" => {
                    if let Ok(p) = parse_percent(&v) {
                        c.swap_used_limit_pct = p;
                        c.source = origin.to_string();
                    }
                }
                "DefaultMemoryPressureLimit" => {
                    if let Ok(p) = parse_percent(&v) {
                        c.mem_pressure_limit_pct = p;
                        c.source = origin.to_string();
                    }
                }
                "DefaultMemoryPressureDurationSec" => {
                    if let Ok(s) = parse_duration_s(&v) {
                        c.mem_pressure_duration_s = s.round().clamp(0.0, u32::MAX as f64) as u32;
                        c.source = origin.to_string();
                    }
                }
                _ => {}
            }
        }
    }
    c
}

/// A unit's `ManagedOOM*` settings (slice/service/scope files or `systemctl show` output).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManagedOom {
    pub unit: String,
    pub swap_kill: bool,
    pub pressure_kill: bool,
    pub pressure_limit_pct: Option<f64>,
    pub pressure_duration_s: Option<u32>,
}

/// Parses `ManagedOOMSwap=`, `ManagedOOMMemoryPressure=`, `ManagedOOMMemoryPressureLimit=` and
/// `ManagedOOMMemoryPressureDurationSec=` from a unit file or `systemctl show -p …` output.
pub fn parse_managed_oom(unit: &str, text: &str) -> ManagedOom {
    let mut m = ManagedOom {
        unit: unit.to_string(),
        ..Default::default()
    };
    for (_, k, v) in systemd_pairs(text) {
        match k.as_str() {
            "ManagedOOMSwap" => m.swap_kill = v == "kill",
            "ManagedOOMMemoryPressure" => m.pressure_kill = v == "kill",
            "ManagedOOMMemoryPressureLimit" => {
                // "0" / "0%" means "use the default".
                m.pressure_limit_pct = parse_percent(&v).ok().filter(|p| *p > 0.0);
            }
            "ManagedOOMMemoryPressureDurationSec" => {
                m.pressure_duration_s = parse_duration_s(&v)
                    .ok()
                    .filter(|s| *s > 0.0)
                    .map(|s| s.round().clamp(0.0, u32::MAX as f64) as u32);
            }
            _ => {}
        }
    }
    m
}

/// systemd-oomd thresholds. Without unit information both kinds are assumed to be managed (conservative);
/// with units, only managed kinds apply and the tightest pressure limit wins.
pub fn oomd_thresholds(cfg: &OomdConfig, units: &[ManagedOom]) -> Vec<OomThreshold> {
    let mut out = Vec::new();
    let swap = units.is_empty() || units.iter().any(|u| u.swap_kill);
    if swap {
        out.push(OomThreshold {
            killer: OomKiller::SystemdOomd,
            metric: ThresholdMetric::SwapUsedPct,
            value: cfg.swap_used_limit_pct,
            duration_s: None,
            source: cfg.source.clone(),
        });
    }
    let pressure_units: Vec<&ManagedOom> = units.iter().filter(|u| u.pressure_kill).collect();
    if units.is_empty() {
        out.push(OomThreshold {
            killer: OomKiller::SystemdOomd,
            metric: ThresholdMetric::MemPressurePct,
            value: cfg.mem_pressure_limit_pct,
            duration_s: Some(cfg.mem_pressure_duration_s),
            source: cfg.source.clone(),
        });
    } else if let Some(u) = pressure_units.iter().min_by(|a, b| {
        let la = a.pressure_limit_pct.unwrap_or(cfg.mem_pressure_limit_pct);
        let lb = b.pressure_limit_pct.unwrap_or(cfg.mem_pressure_limit_pct);
        la.partial_cmp(&lb).unwrap_or(std::cmp::Ordering::Equal)
    }) {
        out.push(OomThreshold {
            killer: OomKiller::SystemdOomd,
            metric: ThresholdMetric::MemPressurePct,
            value: u.pressure_limit_pct.unwrap_or(cfg.mem_pressure_limit_pct),
            duration_s: Some(u.pressure_duration_s.unwrap_or(cfg.mem_pressure_duration_s)),
            source: if u.pressure_limit_pct.is_some() {
                format!("{} ManagedOOMMemoryPressureLimit", u.unit)
            } else {
                cfg.source.clone()
            },
        });
    }
    out
}

/// earlyoom settings from its argv (`-m PERCENT[,KILL]`, `-s PERCENT[,KILL]`, `-M KiB[,KILL]`,
/// `-S KiB[,KILL]`). SIGTERM thresholds are the ones that matter for a forecast.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EarlyoomConfig {
    pub mem_term_pct: f64,
    pub mem_kill_pct: f64,
    pub swap_term_pct: f64,
    pub swap_kill_pct: f64,
    pub mem_term_kib: Option<u64>,
    pub mem_kill_kib: Option<u64>,
    pub swap_term_kib: Option<u64>,
    pub swap_kill_kib: Option<u64>,
}

impl Default for EarlyoomConfig {
    fn default() -> Self {
        EarlyoomConfig {
            mem_term_pct: EARLYOOM_DEFAULT_MEM_PCT,
            mem_kill_pct: EARLYOOM_DEFAULT_MEM_PCT / 2.0,
            swap_term_pct: EARLYOOM_DEFAULT_SWAP_PCT,
            swap_kill_pct: EARLYOOM_DEFAULT_SWAP_PCT / 2.0,
            mem_term_kib: None,
            mem_kill_kib: None,
            swap_term_kib: None,
            swap_kill_kib: None,
        }
    }
}

fn split_pair(v: &str) -> Result<(f64, Option<f64>), String> {
    let mut it = v.splitn(2, ',');
    let a: f64 = it
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .map_err(|_| format!("invalid value {v:?}"))?;
    let b = match it.next() {
        Some(s) => Some(
            s.trim()
                .parse::<f64>()
                .map_err(|_| format!("invalid value {v:?}"))?,
        ),
        None => None,
    };
    if !a.is_finite() || a < 0.0 || b.map(|x| !x.is_finite() || x < 0.0).unwrap_or(false) {
        return Err(format!("invalid value {v:?}"));
    }
    Ok((a, b))
}

/// Parses an earlyoom command line (argv[0] may be included). Unknown options are ignored.
pub fn parse_earlyoom_argv(argv: &[String]) -> Result<EarlyoomConfig, String> {
    // Short options that take an argument (so their value is not mistaken for an option).
    const WITH_ARG: &[&str] = &["-r", "-N", "--prefer", "--avoid", "--ignore"];
    let mut c = EarlyoomConfig::default();
    let mut i = 0;
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    while i < args.len() {
        let a = args[i];
        let (flag, attached) = if a.len() > 2 && a.starts_with('-') && !a.starts_with("--") {
            (&a[..2], Some(&a[2..]))
        } else {
            (a, None)
        };
        let value = |i: &mut usize| -> Result<String, String> {
            if let Some(v) = attached {
                return Ok(v.to_string());
            }
            *i += 1;
            args.get(*i)
                .map(|s| s.to_string())
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "-m" => {
                let (t, k) = split_pair(&value(&mut i)?)?;
                if t > 100.0 {
                    return Err(format!("-m {t} out of range"));
                }
                c.mem_term_pct = t;
                c.mem_kill_pct = k.unwrap_or(t / 2.0);
            }
            "-s" => {
                let (t, k) = split_pair(&value(&mut i)?)?;
                if t > 100.0 {
                    return Err(format!("-s {t} out of range"));
                }
                c.swap_term_pct = t;
                c.swap_kill_pct = k.unwrap_or(t / 2.0);
            }
            "-M" => {
                let (t, k) = split_pair(&value(&mut i)?)?;
                c.mem_term_kib = Some(t as u64);
                c.mem_kill_kib = Some(k.unwrap_or(t / 2.0) as u64);
            }
            "-S" => {
                let (t, k) = split_pair(&value(&mut i)?)?;
                c.swap_term_kib = Some(t as u64);
                c.swap_kill_kib = Some(k.unwrap_or(t / 2.0) as u64);
            }
            f if WITH_ARG.contains(&f) && attached.is_none() => {
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(c)
}

/// earlyoom SIGTERM thresholds. `-m`/`-M` and `-s`/`-S`: the lower of the two applies. With known totals
/// the thresholds are resolved to bytes / swap-used %; without swap (`swap_total == Some(0)`) the swap
/// condition is always met and no swap threshold is emitted.
pub fn earlyoom_thresholds(
    c: &EarlyoomConfig,
    mem_total: Option<u64>,
    swap_total: Option<u64>,
) -> Vec<OomThreshold> {
    let mut out = Vec::new();
    let source = "earlyoom argv".to_string();
    match mem_total.filter(|t| *t > 0) {
        Some(total) => {
            let pct_bytes = (total as f64 * c.mem_term_pct / 100.0) as u64;
            let bytes = match c.mem_term_kib {
                Some(k) => pct_bytes.min(k.saturating_mul(1024)),
                None => pct_bytes,
            };
            out.push(OomThreshold {
                killer: OomKiller::Earlyoom,
                metric: ThresholdMetric::AvailableBytes,
                value: bytes as f64,
                duration_s: None,
                source: source.clone(),
            });
        }
        None => out.push(OomThreshold {
            killer: OomKiller::Earlyoom,
            metric: ThresholdMetric::AvailablePct,
            value: c.mem_term_pct,
            duration_s: None,
            source: source.clone(),
        }),
    }
    if swap_total != Some(0) {
        let mut free_pct = c.swap_term_pct;
        if let (Some(k), Some(t)) = (c.swap_term_kib, swap_total.filter(|t| *t > 0)) {
            free_pct = free_pct.min(k as f64 * 1024.0 / t as f64 * 100.0);
        }
        out.push(OomThreshold {
            killer: OomKiller::Earlyoom,
            metric: ThresholdMetric::SwapUsedPct,
            value: (100.0 - free_pct).clamp(0.0, 100.0),
            duration_s: None,
            source,
        });
    }
    out
}

/// What the collector found about user-space OOM killers.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OomInputs {
    pub os: OsKind,
    /// systemd-oomd is active: its config and the managed units (empty = unknown).
    pub oomd: Option<(OomdConfig, Vec<ManagedOom>)>,
    /// earlyoom is running with this argv.
    pub earlyoom_argv: Option<Vec<String>>,
    pub mem_total: Option<u64>,
    pub swap_total: Option<u64>,
}

/// Builds `Oom { killer, killers, thresholds }` (forecast/victim/kills are filled later). The nearest
/// killer is the most proactive one present: earlyoom, then systemd-oomd, then the kernel; jetsam on macOS.
/// An unparsable earlyoom argv falls back to earlyoom's defaults.
pub fn oom_setup(inputs: &OomInputs) -> Oom {
    let mut oom = Oom::default();
    match inputs.os {
        OsKind::Macos => {
            oom.killer = OomKiller::Jetsam;
            oom.killers = vec![OomKiller::Jetsam];
        }
        _ => {
            oom.killers.push(OomKiller::Kernel);
            oom.killer = OomKiller::Kernel;
            if let Some((cfg, units)) = &inputs.oomd {
                oom.killers.push(OomKiller::SystemdOomd);
                oom.thresholds.extend(oomd_thresholds(cfg, units));
                oom.killer = OomKiller::SystemdOomd;
            }
            if let Some(argv) = &inputs.earlyoom_argv {
                oom.killers.push(OomKiller::Earlyoom);
                let cfg = parse_earlyoom_argv(argv).unwrap_or_default();
                oom.thresholds
                    .extend(earlyoom_thresholds(&cfg, inputs.mem_total, inputs.swap_total));
                oom.killer = OomKiller::Earlyoom;
            }
        }
    }
    oom
}

// -----------------------------------------------------------------------------------------------------
// Likely victim & recent kills
// -----------------------------------------------------------------------------------------------------

/// Processes jetsam never picks (kernel, launchd) or that sit in the highest bands.
const JETSAM_EXEMPT: &[&str] = &["kernel_task", "launchd", "WindowServer", "loginwindow"];

/// Who the killer would likely pick. Linux: highest `oom_score` (readable unprivileged). macOS: jetsam bands
/// need root, so the largest-footprint non-system group (its root process), labeled as a heuristic.
pub fn likely_victim(s: &Snapshot) -> Option<Victim> {
    let footprint = |p: &crate::model::Process| p.mem.footprint_or_pss.usable().unwrap_or(0);
    let scored = s
        .processes
        .iter()
        .filter(|p| p.oom_score.unwrap_or(0) > 0 && p.id.pid > 1)
        .max_by(|a, b| {
            a.oom_score
                .cmp(&b.oom_score)
                .then_with(|| footprint(a).cmp(&footprint(b)))
                .then_with(|| b.id.pid.cmp(&a.id.pid))
        });
    if s.host.os != OsKind::Macos {
        if let Some(p) = scored {
            return Some(Victim {
                id: p.id,
                name: p.name.clone(),
                group_id: s.group_of(p.id).map(|g| g.id.clone()),
                reason: format!("highest oom_score ({})", p.oom_score.unwrap_or(0)),
                heuristic: false,
            });
        }
        if s.host.os == OsKind::Linux {
            return None;
        }
    }
    // macOS (or no oom_score data): largest non-system group by footprint.
    let group = s
        .groups
        .iter()
        .filter(|g| g.kind != GroupKind::System && !g.is_self && !g.protected)
        .filter(|g| g.totals.footprint.usable().unwrap_or(0) > 0)
        .max_by_key(|g| {
            (
                g.totals.footprint.usable().unwrap_or(0),
                std::cmp::Reverse(g.id.clone()),
            )
        });
    let pick = |id: ProcId| {
        s.process(id)
            .filter(|p| !JETSAM_EXEMPT.contains(&p.name.as_str()) && p.id.pid > 1)
    };
    if let Some(g) = group {
        let proc_ = g.root.and_then(pick).or_else(|| {
            g.members
                .iter()
                .filter_map(|m| pick(m.id))
                .max_by_key(|p| footprint(p))
        });
        if let Some(p) = proc_ {
            return Some(Victim {
                id: p.id,
                name: if g.label.is_empty() {
                    p.name.clone()
                } else {
                    g.label.clone()
                },
                group_id: Some(g.id.clone()),
                reason: "largest footprint among apps (heuristic: jetsam bands need root)".into(),
                heuristic: true,
            });
        }
    }
    let p = s
        .processes
        .iter()
        .filter(|p| p.id.pid > 1 && !JETSAM_EXEMPT.contains(&p.name.as_str()) && Some(p.id.pid) != s.self_pid)
        .max_by_key(|p| footprint(p))
        .filter(|p| footprint(p) > 0)?;
    Some(Victim {
        id: p.id,
        name: p.name.clone(),
        group_id: s.group_of(p.id).map(|g| g.id.clone()),
        reason: "largest footprint (heuristic: jetsam bands need root)".into(),
        heuristic: true,
    })
}

/// cgroup v2 `memory.events` counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryEvents {
    pub low: u64,
    pub high: u64,
    pub max: u64,
    pub oom: u64,
    pub oom_kill: u64,
    pub oom_group_kill: u64,
}

/// Parses `memory.events` ("oom_kill 3" lines; unknown keys ignored).
pub fn parse_memory_events(text: &str) -> MemoryEvents {
    let mut e = MemoryEvents::default();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else {
            continue;
        };
        let Ok(v) = v.parse::<u64>() else { continue };
        match k {
            "low" => e.low = v,
            "high" => e.high = v,
            "max" => e.max = v,
            "oom" => e.oom = v,
            "oom_kill" => e.oom_kill = v,
            "oom_group_kill" => e.oom_group_kill = v,
            _ => {}
        }
    }
    e
}

/// New OOM kills between two reads (counter resets → 0).
pub fn oom_kills_since(prev: &MemoryEvents, cur: &MemoryEvents) -> u64 {
    cur.oom_kill.saturating_sub(prev.oom_kill)
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses an `.ips` timestamp like `"2026-09-29 10:10:10.00 +0100"` to ms since the epoch.
pub fn parse_ips_timestamp_ms(s: &str) -> Option<u64> {
    let mut parts = s.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let tz = parts.next().unwrap_or("+0000");
    let mut d = date.split('-');
    let y: i64 = d.next()?.parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next()?.parse().ok()?;
    if !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0.0..61.0).contains(&sec) {
        return None;
    }
    let (sign, digits) = match tz.as_bytes().first()? {
        b'+' => (1, &tz[1..]),
        b'-' => (-1, &tz[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let tzh: i64 = digits[..2].parse().ok()?;
    let tzm: i64 = digits[2..].parse().ok()?;
    let offset_s = sign * (tzh * 3600 + tzm * 60);
    let secs = days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 - offset_s;
    let ms = secs as f64 * 1000.0 + (sec * 1000.0).round();
    (ms >= 0.0).then_some(ms as u64)
}

/// Parses a macOS `JetsamEvent-*.ips` report into kills (processes carrying a `reason`). The first line is
/// a JSON header, the rest the JSON body; older single-document reports are accepted too.
pub fn parse_jetsam_event(text: &str, source: &str) -> Vec<OomKill> {
    let (header, body) = match text.split_once('\n') {
        Some((h, b)) if !b.trim().is_empty() => (
            serde_json::from_str::<serde_json::Value>(h).ok(),
            serde_json::from_str::<serde_json::Value>(b).ok(),
        ),
        _ => (None, serde_json::from_str::<serde_json::Value>(text).ok()),
    };
    let Some(body) = body else {
        return Vec::new();
    };
    let at_ms = body
        .get("date")
        .and_then(|d| d.as_str())
        .and_then(parse_ips_timestamp_ms)
        .or_else(|| {
            header
                .as_ref()?
                .get("timestamp")?
                .as_str()
                .and_then(parse_ips_timestamp_ms)
        });
    body.get("processes")
        .and_then(|p| p.as_array())
        .map(|procs| {
            procs
                .iter()
                .filter(|p| p.get("reason").and_then(|r| r.as_str()).is_some())
                .map(|p| OomKill {
                    at_ms,
                    killer: OomKiller::Jetsam,
                    victim_name: p.get("name").and_then(|n| n.as_str()).map(str::to_string),
                    victim_pid: p
                        .get("pid")
                        .and_then(|n| n.as_u64())
                        .and_then(|n| u32::try_from(n).ok()),
                    source: format!(
                        "{source} ({})",
                        p.get("reason").and_then(|r| r.as_str()).unwrap_or("?")
                    ),
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{GIB, MIB};

    fn hist(f: impl Fn(u64) -> u64) -> History {
        let mut h = History::default();
        for i in 0..=60u64 {
            h.push(HistoryPoint {
                t_ms: i * 5_000,
                swap_used: Some(f(i)),
                swap_total: Some(7 * GIB),
                available: Some(4 * GIB),
                ..Default::default()
            });
        }
        h
    }

    #[test]
    fn steady_growth_forecasts() {
        // +400 MiB/min: 5 s steps → 400/12 MiB per step, starting at 4 GiB.
        let h = hist(|i| 4 * GIB + i * 400 * MIB / 12);
        let f = forecast_oom(&h, &Oom::default()).expect("forecast");
        assert_eq!(f.target, ForecastTarget::SwapExhaustion);
        // remaining at t=300s: 7G - (4G + 2000M) = 1072 MiB → ~160 s
        assert!((150..=170).contains(&f.eta_s), "eta {}", f.eta_s);
        assert!(f.confidence > 0.99);
        assert!((f.rate_per_min - 400.0 * MIB as f64).abs() < MIB as f64);
    }

    #[test]
    fn single_spike_does_not_forecast() {
        let h = hist(|i| if i == 60 { 6 * GIB } else { 4 * GIB });
        assert!(forecast_oom(&h, &Oom::default()).is_none());
    }

    #[test]
    fn step_does_not_forecast() {
        // A model load: swap jumps once by 2 GiB halfway and stays (R² of a line through it is ~0.75).
        let h = hist(|i| if i >= 30 { 6 * GIB } else { 4 * GIB });
        let s = h.series(WINDOW_MS, |p| p.swap_used.map(|v| v as f64));
        assert!(linear_fit(&s).unwrap().r2 > MIN_R2);
        assert!(forecast_oom(&h, &Oom::default()).is_none());
    }

    #[test]
    fn flat_is_stable() {
        let h = hist(|_| 4 * GIB);
        assert!(forecast_oom(&h, &Oom::default()).is_none());
    }

    #[test]
    fn too_little_data() {
        let mut h = History::default();
        for i in 0..10u64 {
            h.push(HistoryPoint {
                t_ms: i * 5_000, // 45 s < 2 min
                swap_used: Some(GIB + i * 100 * MIB),
                swap_total: Some(2 * GIB),
                ..Default::default()
            });
        }
        assert!(forecast_oom(&h, &Oom::default()).is_none());
        assert!(forecast_oom(&History::default(), &Oom::default()).is_none());
    }

    #[test]
    fn recent_trend_uses_shorter_window() {
        // Flat for 3 min, then +300 MiB/min for 2.5 min: the 5-min window fails the halves check, 2 min qualifies.
        let h = hist(|i| {
            if i <= 36 {
                4 * GIB
            } else {
                4 * GIB + (i - 36) * 25 * MIB
            }
        });
        let f = forecast_oom(&h, &Oom::default()).expect("forecast from recent trend");
        assert_eq!(f.target, ForecastTarget::SwapExhaustion);
        assert!(f.window_s <= 180, "window {}", f.window_s);
    }

    #[test]
    fn jetsam_ignores_dynamic_swap_total() {
        let h = hist(|i| 4 * GIB + i * 400 * MIB / 12);
        let oom = Oom {
            killer: OomKiller::Jetsam,
            killers: vec![OomKiller::Jetsam],
            ..Default::default()
        };
        assert!(forecast_oom(&h, &oom).is_none());
    }

    #[test]
    fn jetsam_forecasts_against_the_swap_ceiling() {
        // macOS: swap grows 400 MiB/min; the swap volume leaves room for 7 GiB of swap in total.
        let mut h = History::default();
        for i in 0..=60u64 {
            h.push(HistoryPoint {
                t_ms: i * 5_000,
                swap_used: Some(4 * GIB + i * 400 * MIB / 12),
                swap_total: Some(5 * GIB + i * 400 * MIB / 12), // grows with use: not a limit
                swap_limit: Some(7 * GIB),
                available: Some(4 * GIB),
                ..Default::default()
            });
        }
        let oom = oom_setup(&OomInputs {
            os: OsKind::Macos,
            ..Default::default()
        });
        let f = forecast_oom(&h, &oom).expect("forecast against the swap ceiling");
        assert_eq!(f.target, ForecastTarget::SwapExhaustion);
        assert_eq!(f.killer, Some(OomKiller::Jetsam));
        assert!((150..=170).contains(&f.eta_s), "eta {}", f.eta_s);
        // Plenty of disk: the ceiling is far away → stable.
        let mut roomy = History::default();
        for p in h.iter() {
            roomy.push(HistoryPoint {
                swap_limit: Some(700 * GIB),
                ..p.clone()
            });
        }
        assert!(forecast_oom(&roomy, &oom).is_none());
    }

    #[test]
    fn drop_after_growth_clears_the_forecast() {
        // Swap grows steadily for 4 min, then an OOM kill (or a stopped hog) frees most of it at once.
        let grow = |i: u64| 4 * GIB + i * 400 * MIB / 12;
        let h = hist(|i| if i < 50 { grow(i) } else { 2 * GIB });
        assert!(
            forecast_oom(&h, &Oom::default()).is_none(),
            "a drop against the trend must not leave an ETA"
        );
        // The same growth without the drop does forecast.
        assert!(forecast_oom(&hist(grow), &Oom::default()).is_some());
    }

    #[test]
    fn eta_is_measured_from_the_actual_last_value() {
        // A perfect line except the last point, which sits a little above it (within the guard).
        let mut pts: Vec<(f64, f64)> = (0..=36).map(|i| (i as f64 * 5.0, 100.0 + i as f64)).collect();
        pts.last_mut().unwrap().1 += 3.0; // actual 139 vs fitted ~136.x
        let (eta, fit) = eta_to_threshold(&pts, 200.0, true).unwrap();
        let from_actual = (200.0 - 139.0) / fit.slope;
        assert!((eta - from_actual).abs() < 1e-9, "eta {eta} vs {from_actual}");
    }

    #[test]
    fn available_falling_forecasts() {
        let mut h = History::default();
        for i in 0..=36u64 {
            h.push(HistoryPoint {
                t_ms: i * 5_000,
                available: Some(4 * GIB - i * 50 * MIB),
                ..Default::default()
            });
        }
        let f = forecast_oom(&h, &Oom::default()).unwrap();
        assert_eq!(f.target, ForecastTarget::AvailableExhaustion);
        assert_eq!(f.killer, Some(OomKiller::Kernel));
        // 4096 − 1800 = 2296 MiB left at 600 MiB/min → ≈ 230 s
        assert!((225..=235).contains(&f.eta_s), "eta {}", f.eta_s);
    }

    #[test]
    fn oomd_pressure_threshold_adds_duration() {
        let mut h = History::default();
        for i in 0..=36u64 {
            h.push(HistoryPoint {
                t_ms: i * 5_000,
                psi_some_avg10: Some(10.0 + i as f64),
                ..Default::default()
            });
        }
        let oom = oom_setup(&OomInputs {
            os: OsKind::Linux,
            oomd: Some((OomdConfig::default(), vec![])),
            ..Default::default()
        });
        let f = forecast_oom(&h, &oom).unwrap();
        assert_eq!(f.target, ForecastTarget::KillerThreshold);
        assert_eq!(f.killer, Some(OomKiller::SystemdOomd));
        // PSI at 46 → 60 at +1/5 s = 70 s, + 30 s sustain = 100 s.
        assert!((98..=102).contains(&f.eta_s), "eta {}", f.eta_s);
    }

    #[test]
    fn earlyoom_needs_both_conditions() {
        let total = 16 * GIB;
        let mut h = History::default();
        for i in 0..=36u64 {
            h.push(HistoryPoint {
                t_ms: i * 5_000,
                // available falls 3 GiB → 1.2 GiB …
                available: Some(3 * GIB - i * 50 * MIB),
                // … swap has plenty free and is flat
                swap_used: Some(GIB),
                swap_total: Some(8 * GIB),
                ..Default::default()
            });
        }
        let oom = oom_setup(&OomInputs {
            os: OsKind::Linux,
            earlyoom_argv: Some(vec!["earlyoom".into(), "-m".into(), "5".into()]),
            mem_total: Some(total),
            swap_total: Some(8 * GIB),
            ..Default::default()
        });
        assert_eq!(oom.killer, OomKiller::Earlyoom);
        // Swap never reaches 90 % used → earlyoom does not act; the kernel forecast (available → 0) remains.
        let f = forecast_oom(&h, &oom).unwrap();
        assert_eq!(f.killer, Some(OomKiller::Kernel));
        assert_eq!(f.target, ForecastTarget::AvailableExhaustion);

        // Without swap the memory condition alone decides (also when the collector still emits a swap
        // threshold for a swapless host).
        let mut with_swap_rule = oom_setup(&OomInputs {
            os: OsKind::Linux,
            earlyoom_argv: Some(vec!["-m".into(), "5".into()]),
            mem_total: Some(total),
            swap_total: None,
            ..Default::default()
        });
        assert_eq!(with_swap_rule.thresholds.len(), 2);
        with_swap_rule.killer = OomKiller::Earlyoom;
        let mut swapless = History::default();
        for p in h.iter() {
            swapless.push(HistoryPoint {
                swap_used: Some(0),
                swap_total: Some(0),
                ..p.clone()
            });
        }
        let f = forecast_oom(&swapless, &with_swap_rule).unwrap();
        assert_eq!(f.killer, Some(OomKiller::Earlyoom));
        let oom = oom_setup(&OomInputs {
            os: OsKind::Linux,
            earlyoom_argv: Some(vec!["-m".into(), "5".into()]),
            mem_total: Some(total),
            swap_total: Some(0),
            ..Default::default()
        });
        let f = forecast_oom(&h, &oom).unwrap();
        assert_eq!(f.killer, Some(OomKiller::Earlyoom));
        assert_eq!(f.target, ForecastTarget::KillerThreshold);
        // 1272 MiB now → 819.2 MiB (5 % of 16 GiB) at 600 MiB/min ≈ 45 s
        assert!((43..=47).contains(&f.eta_s), "eta {}", f.eta_s);
    }

    #[test]
    fn oomd_swap_rule_needs_memory_used_too() {
        // Swap grows 300 MiB/min from 6 GiB of 8 GiB; oomd SwapUsedLimit 90 % (7.2 GiB) in ≈ 4 min.
        let total = 16 * GIB;
        let build = |avail: &dyn Fn(u64) -> u64| {
            let mut h = History::default();
            for i in 0..=36u64 {
                h.push(HistoryPoint {
                    t_ms: i * 5_000,
                    swap_used: Some(5 * GIB + i * 25 * MIB),
                    swap_total: Some(8 * GIB),
                    available: Some(avail(i)),
                    psi_some_avg10: Some(1.0),
                    ..Default::default()
                });
            }
            h
        };
        let oom = oom_setup(&OomInputs {
            os: OsKind::Linux,
            oomd: Some((OomdConfig::default(), vec![])),
            ..Default::default()
        });
        // Plenty of RAM available (50 %): memory used is far below 90 % → oomd's swap rule cannot fire.
        let roomy = build(&|_| 8 * GIB);
        let f = forecast_oom_with(&roomy, &oom, Some(total)).expect("kernel swap exhaustion");
        assert_eq!(f.killer, Some(OomKiller::Kernel));
        assert_eq!(f.target, ForecastTarget::SwapExhaustion);
        // Without the RAM size only the swap half is checked (conservative: oomd forecast).
        let f = forecast_oom(&roomy, &oom).unwrap();
        assert_eq!(f.killer, Some(OomKiller::SystemdOomd));
        // Memory already ≥ 90 % used (available 1 GiB < 1.6 GiB): the swap half decides.
        let tight = build(&|_| GIB);
        let f = forecast_oom_with(&tight, &oom, Some(total)).unwrap();
        assert_eq!(f.killer, Some(OomKiller::SystemdOomd));
        assert_eq!(f.target, ForecastTarget::KillerThreshold);
        // remaining 7.2 GiB − 5.88 GiB ≈ 1352 MiB at 300 MiB/min ≈ 270 s
        assert!((260..=280).contains(&f.eta_s), "eta {}", f.eta_s);
        // Swap near the limit already, memory falling towards 10 % available: the memory half decides.
        let late = {
            let mut h = History::default();
            for i in 0..=36u64 {
                h.push(HistoryPoint {
                    t_ms: i * 5_000,
                    swap_used: Some(7 * GIB + 512 * MIB),
                    swap_total: Some(8 * GIB),
                    available: Some(4 * GIB - i * 50 * MIB),
                    ..Default::default()
                });
            }
            h
        };
        let f = forecast_oom_with(&late, &oom, Some(total)).unwrap();
        assert_eq!(f.killer, Some(OomKiller::SystemdOomd));
        // 2296 MiB now → 1638.4 MiB (10 % of 16 GiB) at 600 MiB/min ≈ 66 s
        assert!((60..=72).contains(&f.eta_s), "eta {}", f.eta_s);
        assert!((f.rate_per_min - 600.0 * MIB as f64).abs() < 2.0 * MIB as f64);
    }

    #[test]
    fn stale_series_is_not_forecast_from_its_own_end() {
        // Swap data stops 20 s before the newest point: the ETA is shortened by those 20 s.
        let mut h = hist(|i| 4 * GIB + i * 400 * MIB / 12);
        let fresh = forecast_oom(&h, &Oom::default()).unwrap().eta_s;
        let mut g = History::default();
        for p in h.iter() {
            g.push(p.clone());
        }
        for k in 1..=4u64 {
            g.push(HistoryPoint {
                t_ms: 300_000 + k * 5_000,
                available: Some(4 * GIB),
                ..Default::default()
            });
        }
        let f = forecast_oom(&g, &Oom::default()).unwrap();
        assert_eq!(f.target, ForecastTarget::SwapExhaustion);
        assert!(
            (fresh - 22..=fresh - 18).contains(&f.eta_s),
            "{} vs {fresh}",
            f.eta_s
        );
        // Stale for more than 30 s → no forecast from that metric.
        for k in 5..=12u64 {
            h.push(HistoryPoint {
                t_ms: 300_000 + k * 5_000,
                available: Some(4 * GIB),
                ..Default::default()
            });
        }
        let mut stale = History::default();
        for p in g.iter().chain(h.iter().filter(|p| p.t_ms > 320_000)) {
            stale.push(p.clone());
        }
        assert!(forecast_oom(&stale, &Oom::default()).is_none());
    }

    #[test]
    fn ips_timestamp_never_panics_on_odd_timezones() {
        for tz in ["+1é1", "+é12", "+12345", "+ab12", "Z", "", "+"] {
            let s = format!("2026-09-29 12:10:10.00 {tz}");
            let _ = parse_ips_timestamp_ms(&s);
        }
        assert_eq!(parse_ips_timestamp_ms("2026-09-29 12:10:10.00 +1é1"), None);
    }

    #[test]
    fn earlyoom_argv_forms() {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let c =
            parse_earlyoom_argv(&v(&["/usr/bin/earlyoom", "-r", "3600", "-m", "4,2", "-s", "20"])).unwrap();
        assert_eq!((c.mem_term_pct, c.mem_kill_pct), (4.0, 2.0));
        assert_eq!((c.swap_term_pct, c.swap_kill_pct), (20.0, 10.0));
        let c = parse_earlyoom_argv(&v(&["-m5", "-M", "1048576", "--prefer", "-m 1", "-n"])).unwrap();
        assert_eq!(c.mem_term_pct, 5.0);
        assert_eq!(c.mem_term_kib, Some(1_048_576));
        let d = parse_earlyoom_argv(&v(&["earlyoom"])).unwrap();
        assert_eq!(d, EarlyoomConfig::default());
        assert!(parse_earlyoom_argv(&v(&["-m"])).is_err());
        assert!(parse_earlyoom_argv(&v(&["-m", "x"])).is_err());
        assert!(parse_earlyoom_argv(&v(&["-s", "150"])).is_err());
        // -M lower than -m wins.
        let th = earlyoom_thresholds(&c, Some(64 * GIB), Some(0));
        assert_eq!(th.len(), 1);
        assert_eq!(th[0].value, GIB as f64);
        let th = earlyoom_thresholds(&EarlyoomConfig::default(), None, None);
        assert_eq!(th[0].metric, ThresholdMetric::AvailablePct);
        assert_eq!(th[1].value, 90.0);
    }

    #[test]
    fn oomd_config_parsing() {
        let main = "[OOM]\n#SwapUsedLimit=90%\nDefaultMemoryPressureLimit=50%\n";
        let dropin = "[OOM]\nSwapUsedLimit=80%\nDefaultMemoryPressureDurationSec=1min\nBogus=1\n";
        let c = parse_oomd_conf(&[
            ("/etc/systemd/oomd.conf", main),
            ("oomd.conf.d/10-x.conf", dropin),
        ]);
        assert_eq!(c.swap_used_limit_pct, 80.0);
        assert_eq!(c.mem_pressure_limit_pct, 50.0);
        assert_eq!(c.mem_pressure_duration_s, 60);
        assert_eq!(c.source, "oomd.conf.d/10-x.conf");
        let d = parse_oomd_conf(&[("x", "[OOM]\nSwapUsedLimit=lots\n")]);
        assert_eq!(d.swap_used_limit_pct, 90.0);

        let slice = parse_managed_oom(
            "user@1000.service",
            "[Service]\nManagedOOMMemoryPressure=kill\nManagedOOMMemoryPressureLimit=40%\n",
        );
        let root = parse_managed_oom("-.slice", "ManagedOOMSwap=kill\n");
        let th = oomd_thresholds(&OomdConfig::default(), &[slice, root]);
        assert_eq!(th.len(), 2);
        assert_eq!(th[0].metric, ThresholdMetric::SwapUsedPct);
        assert_eq!(th[1].value, 40.0);
        assert_eq!(th[1].duration_s, Some(30));
        let defaults = oomd_thresholds(&OomdConfig::default(), &[]);
        assert_eq!(defaults.len(), 2);
        let none = oomd_thresholds(&OomdConfig::default(), &[ManagedOom::default()]);
        assert!(none.is_empty());
    }

    #[test]
    fn setup_per_os() {
        let mac = oom_setup(&OomInputs {
            os: OsKind::Macos,
            ..Default::default()
        });
        assert_eq!(mac.killers, vec![OomKiller::Jetsam]);
        let linux = oom_setup(&OomInputs {
            os: OsKind::Linux,
            ..Default::default()
        });
        assert_eq!(linux.killers, vec![OomKiller::Kernel]);
        assert!(linux.thresholds.is_empty());
    }

    #[test]
    fn memory_events() {
        let e = parse_memory_events(
            "low 0\nhigh 12\nmax 3\noom 2\noom_kill 2\noom_group_kill 0\nnew_key 9\nbad\n",
        );
        assert_eq!(e.oom_kill, 2);
        assert_eq!(e.high, 12);
        let later = MemoryEvents { oom_kill: 5, ..e };
        assert_eq!(oom_kills_since(&e, &later), 3);
        assert_eq!(oom_kills_since(&later, &e), 0);
    }

    #[test]
    fn ips_timestamps_and_jetsam() {
        assert_eq!(parse_ips_timestamp_ms("1970-01-01 00:00:00.00 +0000"), Some(0));
        assert_eq!(parse_ips_timestamp_ms("1970-01-01 01:00:00.50 +0100"), Some(500));
        // 2026-09-29 10:10:10 UTC = 1790676610
        assert_eq!(
            parse_ips_timestamp_ms("2026-09-29 12:10:10.00 +0200"),
            Some(1_790_676_610_000)
        );
        assert_eq!(parse_ips_timestamp_ms("garbage"), None);
        assert_eq!(parse_ips_timestamp_ms("2026-13-01 00:00:00 +0000"), None);
        let ips = r#"{"bug_type":"298","timestamp":"2026-09-29 12:10:10.00 +0200","os_version":"macOS 26.6"}
{"date":"2026-09-29 12:10:09.50 +0200","largestProcess":"java","processes":[{"name":"java","pid":78921,"reason":"vm-pageshortage","rpages":200000},{"name":"Finder","pid":400,"rpages":1000}]}"#;
        let kills = parse_jetsam_event(ips, "JetsamEvent-2026-09-29-121010.ips");
        assert_eq!(kills.len(), 1);
        assert_eq!(kills[0].victim_name.as_deref(), Some("java"));
        assert_eq!(kills[0].victim_pid, Some(78921));
        assert_eq!(kills[0].at_ms, Some(1_790_676_609_500));
        assert!(kills[0].source.contains("vm-pageshortage"));
        assert!(parse_jetsam_event("not json", "x").is_empty());
    }
}

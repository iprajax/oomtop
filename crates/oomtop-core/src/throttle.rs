//! Throttle factor (SPEC §9): observed ÷ max frequency per cluster/GPU, computed **only while that unit is
//! busy** (active residency ≥ 50 %); otherwise `n/a (idle)`. Blended by utilization (residency-weighted
//! mean over busy units). Apple Silicon down-clocks at idle, so idle must never read as throttling.

use crate::model::{ClusterFreq, Measured, Thermal, ThermalPressure};

/// Minimum active residency for a unit to count.
pub const BUSY_PCT: f64 = 50.0;
/// Below this factor the machine is considered throttled (UX §3 Throttle mode).
pub const THROTTLED_BELOW: f64 = 0.8;
/// Source string of a computed throttle factor.
pub const SOURCE: &str = "Σ(cur/max × residency) / Σ residency over busy units";

fn has_freq(u: &ClusterFreq) -> bool {
    u.max_mhz.is_finite() && u.cur_mhz.is_finite() && u.max_mhz > 0.0 && u.cur_mhz > 0.0
}

/// True if the unit is busy enough for its frequency to mean anything.
pub fn is_busy(u: &ClusterFreq) -> bool {
    u.active_pct.is_finite() && u.active_pct >= BUSY_PCT
}

/// Observed ÷ max frequency of one unit while busy (clamped to 0..1); `None` when idle or without data.
pub fn unit_factor(u: &ClusterFreq) -> Option<f64> {
    (is_busy(u) && has_freq(u)).then(|| (u.cur_mhz / u.max_mhz).clamp(0.0, 1.0))
}

/// Blended throttle factor over busy units. `Unavailable("idle")` when no unit is busy (idle clusters may
/// report 0 MHz), `Unavailable("no frequency data")` when no unit reports a max frequency.
pub fn throttle_factor(units: &[ClusterFreq]) -> Measured<f64> {
    if !units.iter().any(|u| u.max_mhz.is_finite() && u.max_mhz > 0.0) {
        return Measured::unavailable("throttle", "no frequency data");
    }
    let busy: Vec<(&ClusterFreq, f64)> = units.iter().filter_map(|u| Some((u, unit_factor(u)?))).collect();
    if busy.is_empty() {
        // busy units without a current frequency are "unknown", not "idle"
        let reason = if units.iter().any(is_busy) {
            "no frequency data"
        } else {
            "idle"
        };
        return Measured::unavailable("throttle", reason);
    }
    let wsum: f64 = busy.iter().map(|(u, _)| u.active_pct.min(100.0)).sum();
    let f = busy.iter().map(|(u, f)| f * u.active_pct.min(100.0)).sum::<f64>() / wsum;
    Measured::estimate(f.clamp(0.0, 1.0), SOURCE)
}

/// Throttle-mode trigger (UX §3): factor < 0.8 under load, thermal pressure ≥ heavy, a Linux trip point
/// hit, or Low Power Mode. (`why` additionally requires load before reporting any of these as a cause.)
pub fn is_throttled(t: &Thermal) -> bool {
    let factor = t
        .throttle_factor
        .value
        .map(|f| f < THROTTLED_BELOW)
        .unwrap_or(false);
    let pressure = t
        .pressure
        .value
        .map(|p| p >= ThermalPressure::Heavy)
        .unwrap_or(false);
    let lpm = t.low_power_mode.value.unwrap_or(false);
    let trip = t.trip_point_hit.value.unwrap_or(false);
    factor || pressure || lpm || trip
}

/// "running at 45% speed".
pub fn speed_label(factor: f64) -> String {
    format!("running at {:.0}% speed", factor.clamp(0.0, 1.0) * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(cur: f64, max: f64, act: f64) -> ClusterFreq {
        ClusterFreq {
            name: "p".into(),
            cur_mhz: cur,
            max_mhz: max,
            active_pct: act,
        }
    }

    #[test]
    fn idle_is_not_throttling() {
        let m = throttle_factor(&[u(600.0, 4000.0, 3.0)]);
        assert_eq!(m.value, None);
        assert_eq!(m.unavailable_reason(), Some("idle"));
        assert_eq!(
            throttle_factor(&[]).unavailable_reason(),
            Some("no frequency data")
        );
        assert_eq!(
            throttle_factor(&[u(0.0, 4000.0, 0.0)]).unavailable_reason(),
            Some("idle")
        );
        assert_eq!(unit_factor(&u(600.0, 4000.0, 3.0)), None);
        // busy but no current clock reported: unknown, never "idle" (and never a factor of 0)
        assert_eq!(
            throttle_factor(&[u(0.0, 4000.0, 95.0)]).unavailable_reason(),
            Some("no frequency data")
        );
        assert_eq!(
            throttle_factor(&[u(f64::NAN, 4000.0, 95.0), u(900.0, 3000.0, f64::NAN)]).unavailable_reason(),
            Some("no frequency data")
        );
    }

    #[test]
    fn busy_blend() {
        let m = throttle_factor(&[
            u(2000.0, 4000.0, 100.0),
            u(1000.0, 1000.0, 50.0),
            u(100.0, 4000.0, 10.0),
        ]);
        let f = m.value.unwrap();
        assert!((f - (0.5 * 100.0 + 1.0 * 50.0) / 150.0).abs() < 1e-9);
        assert_eq!(speed_label(0.45), "running at 45% speed");
        // turbo above nominal max never reads as > 100 %
        assert_eq!(throttle_factor(&[u(4500.0, 4000.0, 90.0)]).value, Some(1.0));
    }

    #[test]
    fn mode_trigger() {
        let mut t = Thermal::default();
        assert!(!is_throttled(&t));
        t.low_power_mode = Measured::exact(true, "t");
        assert!(is_throttled(&t));
        t.low_power_mode = Measured::exact(false, "t");
        t.throttle_factor = Measured::estimate(0.5, "t");
        assert!(is_throttled(&t));
    }
}

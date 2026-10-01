//! Number formatting for the TUI (UX §8 "numbers right-aligned, units always shown", §12.5 `[format]`).
//! Unavailable values render as `n/a`, never as zero; estimates and lower bounds carry a prefix glyph.

use oomtop_config::model::{CpuFormat, Format, MemoryUnits, TimeFormat};
use oomtop_core::units::{format_bytes, format_duration, UnitSystem};
use oomtop_core::{Measured, Quality};

/// Formatting preferences resolved from config (`[format]`) plus host facts.
#[derive(Debug, Clone, PartialEq)]
pub struct Fmt {
    pub units: MemoryUnits,
    pub decimals: usize,
    pub cpu: CpuFormat,
    pub time: TimeFormat,
    /// Logical cores, for `cpu = "total"`.
    pub cores: u32,
}

impl Default for Fmt {
    fn default() -> Self {
        Fmt::from_config(&Format::default(), 1)
    }
}

impl Fmt {
    pub fn from_config(f: &Format, cores: u32) -> Self {
        Fmt {
            units: f.memory_units,
            decimals: f.decimals.min(3) as usize,
            cpu: f.cpu,
            time: f.time,
            cores: cores.max(1),
        }
    }

    fn system(&self) -> UnitSystem {
        match self.units {
            MemoryUnits::Iec => UnitSystem::Iec,
            MemoryUnits::Si => UnitSystem::Si,
        }
    }

    /// Compact table form: "9.9G" (IEC, binary) or "10.6GB" (SI, decimal), with `format.decimals` places
    /// below 100 of a unit (values ≥ 100 are whole numbers so columns stay narrow).
    pub fn bytes(&self, b: u64) -> String {
        match self.units {
            MemoryUnits::Iec => short(b, 1024.0, &["B", "K", "M", "G", "T"], self.decimals),
            MemoryUnits::Si => short(b, 1000.0, &["B", "KB", "MB", "GB", "TB"], self.decimals),
        }
    }

    /// Optional bytes; `n/a` when unavailable.
    pub fn opt_bytes(&self, b: Option<u64>) -> String {
        b.map(|v| self.bytes(v)).unwrap_or_else(|| "n/a".into())
    }

    /// Long form with configured decimals: "9.9 GiB" / "10.6 GB".
    pub fn bytes_long(&self, b: u64) -> String {
        format_bytes(b, self.system(), self.decimals)
    }

    /// Measured bytes with quality prefix: `≈` estimate (`~` in ASCII), `n/a (reason)` when unavailable.
    pub fn measured_bytes(&self, m: &Measured<u64>, ascii: bool) -> String {
        match (&m.value, &m.quality) {
            (Some(v), Quality::Estimate) => format!("{}{}", if ascii { "~" } else { "≈" }, self.bytes(*v)),
            (Some(v), _) => self.bytes(*v),
            (None, Quality::Unavailable(r)) if !r.is_empty() => format!("n/a ({r})"),
            (None, _) => "n/a".into(),
        }
    }

    /// Signed headroom: "1.2G" / "-1.2G".
    pub fn signed(&self, v: i64) -> String {
        if v < 0 {
            format!("-{}", self.bytes(v.unsigned_abs()))
        } else {
            self.bytes(v as u64)
        }
    }

    /// CPU percent per the `[format] cpu` setting (input is per-core percent: 100 = one core).
    pub fn cpu(&self, per_core_pct: f64) -> String {
        match self.cpu {
            CpuFormat::PerCore => format!("{per_core_pct:.0}%"),
            CpuFormat::Total => format!("{:.1}%", per_core_pct / self.cores as f64),
        }
    }

    pub fn opt_cpu(&self, v: Option<f64>) -> String {
        v.map(|c| self.cpu(c)).unwrap_or_else(|| "n/a".into())
    }

    /// Relative age ("5h") — clock mode shows the same relative form for durations.
    pub fn duration(&self, secs: u64) -> String {
        format_duration(secs)
    }

    /// A point in time relative to `now_ms`: "-3m20s" (relative) or "12:04:10 UTC" (clock).
    pub fn at(&self, t_ms: u64, now_ms: u64) -> String {
        match self.time {
            TimeFormat::Relative => {
                let d = now_ms.saturating_sub(t_ms) / 1000;
                if d == 0 {
                    "now".into()
                } else {
                    format!("-{}", precise_duration(d))
                }
            }
            TimeFormat::Clock => {
                let s = (t_ms / 1000) % 86_400;
                format!("{:02}:{:02}:{:02} UTC", s / 3600, (s % 3600) / 60, s % 60)
            }
        }
    }
}

/// "3m20s", "45s", "1h02m".
pub fn precise_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let (m, s) = (secs / 60, secs % 60);
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s:02}s")
        }
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Short byte form in `base` (1024 or 1000). Promotes when the printed value would round up to the base
/// ("1024K" → "1.0M", "1000KB" → "1.0MB").
fn short(b: u64, base: f64, units: &[&str], decimals: usize) -> String {
    let mut v = b as f64;
    let mut i = 0;
    let rounds_up = |v: f64| {
        let p = 10f64.powi(decimals as i32);
        let shown = if v >= 100.0 {
            v.round()
        } else {
            (v * p).round() / p
        };
        shown >= base
    };
    while i < units.len() - 1 && (v >= base || (i > 0 && rounds_up(v))) {
        v /= base;
        i += 1;
    }
    if i == 0 {
        format!("{b}B")
    } else if v >= 100.0 {
        format!("{v:.0}{}", units[i])
    } else {
        format!("{v:.decimals$}{}", units[i])
    }
}

/// Ordinal for "why ranked here": 1st, 2nd, 3rd, 4th, 11th, 21st.
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::units::GIB;

    #[test]
    fn units_and_quality() {
        let f = Fmt::default();
        assert_eq!(f.bytes(3 * GIB), "3.0G");
        assert_eq!(f.opt_bytes(None), "n/a");
        let si = Fmt {
            units: MemoryUnits::Si,
            ..Fmt::default()
        };
        assert_eq!(si.bytes(10_600_000_000), "10.6GB");
        assert_eq!(f.measured_bytes(&Measured::estimate(GIB, "x"), false), "≈1.0G");
        assert_eq!(f.measured_bytes(&Measured::estimate(GIB, "x"), true), "~1.0G");
        assert_eq!(
            f.measured_bytes(&Measured::unavailable("x", "needs root"), false),
            "n/a (needs root)"
        );
        assert_eq!(f.signed(-(GIB as i64)), "-1.0G");
        assert_eq!(f.bytes_long(GIB + GIB / 2), "1.5 GiB");
    }

    /// `format.decimals` is honored in every table number, and the default (1) matches the core short form.
    #[test]
    fn decimals_and_promotion() {
        let f = Fmt::default();
        for b in [
            0,
            512,
            1023,
            1024,
            1_048_575,
            22 << 20,
            450 << 20,
            1_073_741_000,
            9_900_000_000,
            24 * GIB,
        ] {
            assert_eq!(f.bytes(b), oomtop_core::units::format_bytes_short(b), "{b}");
        }
        let two = Fmt {
            decimals: 2,
            ..Fmt::default()
        };
        assert_eq!(two.bytes(GIB + GIB / 4), "1.25G");
        assert_eq!(two.bytes(450 << 20), "450M");
        let zero = Fmt {
            decimals: 0,
            ..Fmt::default()
        };
        assert_eq!(zero.bytes(GIB + GIB / 4), "1G");
        let si = Fmt {
            units: MemoryUnits::Si,
            ..Fmt::default()
        };
        assert_eq!(si.bytes(999_960), "1.0MB", "no \"1000.0KB\"");
        assert_eq!(si.bytes(999_400), "999KB");
        assert_eq!(si.bytes(999), "999B");
        let cfg = Format {
            decimals: 2,
            ..Format::default()
        };
        assert_eq!(Fmt::from_config(&cfg, 8).decimals, 2);
    }

    #[test]
    fn cpu_and_time() {
        let mut f = Fmt {
            cores: 10,
            ..Fmt::default()
        };
        assert_eq!(f.cpu(250.0), "250%");
        f.cpu = CpuFormat::Total;
        assert_eq!(f.cpu(250.0), "25.0%");
        assert_eq!(f.at(1_000, 201_000), "-3m20s");
        assert_eq!(f.at(5_000, 5_000), "now");
        f.time = TimeFormat::Clock;
        assert_eq!(f.at(3_723_000, 0), "01:02:03 UTC");
        assert_eq!(ordinal(1), "1st");
        assert_eq!(ordinal(12), "12th");
        assert_eq!(ordinal(23), "23rd");
    }
}

//! Units: parsing and formatting of sizes, durations and percentages (SPEC §12.2, UX §5.2, UX §12.5).
//!
//! Size suffixes: `K`/`M`/`G`/`T` and `KiB`/`MiB`/`GiB`/`TiB` are binary (1024ⁿ); `KB`/`MB`/`GB`/`TB` are
//! decimal (1000ⁿ). A bare number is bytes. Case-insensitive: `13g` = `13G` = 13 GiB.

use thiserror::Error;

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;
pub const TIB: u64 = 1024 * GIB;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UnitError {
    #[error("empty value")]
    Empty,
    #[error("invalid number in {0:?}")]
    Number(String),
    #[error("unknown unit {0:?}")]
    Unit(String),
}

/// Memory unit system for display (UX §12.5 `format.memory_units`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnitSystem {
    /// GiB (1024³), shown as "GiB".
    #[default]
    Iec,
    /// GB (10⁹), shown as "GB".
    Si,
}

fn split_number(s: &str) -> (&str, &str) {
    let idx = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '_'))
        .unwrap_or(s.len());
    (&s[..idx], s[idx..].trim())
}

/// Parses "13G", "500M", "1.5GiB", "2GB", "4096".
pub fn parse_bytes(input: &str) -> Result<u64, UnitError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(UnitError::Empty);
    }
    let (num, unit) = split_number(s);
    let n: f64 = num
        .replace('_', "")
        .parse()
        .map_err(|_| UnitError::Number(input.to_string()))?;
    let mult: f64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "ki" | "kib" => KIB as f64,
        "m" | "mi" | "mib" => MIB as f64,
        "g" | "gi" | "gib" => GIB as f64,
        "t" | "ti" | "tib" => TIB as f64,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        other => return Err(UnitError::Unit(other.to_string())),
    };
    let bytes = (n * mult).round();
    if !bytes.is_finite() || bytes >= u64::MAX as f64 {
        return Err(UnitError::Number(input.to_string()));
    }
    Ok(bytes as u64)
}

/// Parses a percentage as used by systemd and earlyoom: `"90%"`, `"90"`, `"900‰"` (permille),
/// `"9000‱"` (permyriad). Returns percent (0..=100 for valid limits; values above 100 are rejected).
pub fn parse_percent(input: &str) -> Result<f64, UnitError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(UnitError::Empty);
    }
    let (num, div) = if let Some(n) = s.strip_suffix('%') {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('‰') {
        (n, 10.0)
    } else if let Some(n) = s.strip_suffix('‱') {
        (n, 100.0)
    } else {
        (s, 1.0)
    };
    let v: f64 = num
        .trim()
        .parse()
        .map_err(|_| UnitError::Number(input.to_string()))?;
    let pct = v / div;
    if !(0.0..=100.0).contains(&pct) {
        return Err(UnitError::Number(input.to_string()));
    }
    Ok(pct)
}

/// Parses durations: "500ms", "2s", "30m", "5h", "7d", "1h30m", bare number = seconds. Returns seconds (f64).
pub fn parse_duration_s(input: &str) -> Result<f64, UnitError> {
    let mut s = input.trim();
    if s.is_empty() {
        return Err(UnitError::Empty);
    }
    let mut total = 0.0;
    while !s.is_empty() {
        let (num, rest) = split_number(s);
        if num.is_empty() {
            return Err(UnitError::Number(input.to_string()));
        }
        let n: f64 = num.parse().map_err(|_| UnitError::Number(input.to_string()))?;
        let unit_len = rest.find(|c: char| c.is_ascii_digit()).unwrap_or(rest.len());
        let unit = &rest[..unit_len];
        // Unit names follow systemd.time(7) plus the short forms used in oomtop config.
        let mult = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "s" | "sec" | "secs" | "second" | "seconds" => 1.0,
            "us" | "usec" | "µs" | "μs" => 0.000_001,
            "ms" | "msec" => 0.001,
            "m" | "min" | "mins" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3600.0,
            "d" | "day" | "days" => 86400.0,
            "w" | "week" | "weeks" => 7.0 * 86400.0,
            other => return Err(UnitError::Unit(other.to_string())),
        };
        total += n * mult;
        s = rest[unit_len..].trim_start();
    }
    // "1e400" cannot reach here (the exponent is not a unit), but a very long digit string can overflow.
    if !total.is_finite() {
        return Err(UnitError::Number(input.to_string()));
    }
    Ok(total)
}

/// Formats bytes with one unit per value, e.g. "9.9 GiB" (Iec) / "10.6 GB" (Si).
pub fn format_bytes(bytes: u64, system: UnitSystem, decimals: usize) -> String {
    let (base, units): (f64, [&str; 5]) = match system {
        UnitSystem::Iec => (1024.0, ["B", "KiB", "MiB", "GiB", "TiB"]),
        UnitSystem::Si => (1000.0, ["B", "KB", "MB", "GB", "TB"]),
    };
    let mut v = bytes as f64;
    let mut i = 0;
    // Promote when rounding to `decimals` would print "1024.0 KiB" instead of "1.0 MiB".
    let scale = 10f64.powi(decimals.min(9) as i32);
    while i < units.len() - 1 && (v >= base || (i > 0 && (v * scale).round() / scale >= base)) {
        v /= base;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.decimals$} {}", units[i])
    }
}

/// Compact form used in tables and headlines: "9.9G", "512M", "0B". Binary units.
pub fn format_bytes_short(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut i = 0;
    // Promote when the printed value would round up to 1024 ("1024K" → "1.0M").
    while i < units.len() - 1 && (v >= 1024.0 || (i > 0 && v.round() >= 1024.0)) {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes}B")
    } else if v >= 100.0 {
        format!("{v:.0}{}", units[i])
    } else {
        format!("{v:.1}{}", units[i])
    }
}

/// Signed variant for headroom ("-1.2 GiB").
pub fn format_bytes_signed(bytes: i64, system: UnitSystem, decimals: usize) -> String {
    if bytes < 0 {
        format!("-{}", format_bytes(bytes.unsigned_abs(), system, decimals))
    } else {
        format_bytes(bytes as u64, system, decimals)
    }
}

/// Relative duration: "45s", "6m", "3h40m", "5h", "2d".
pub fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 || h >= 10 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    } else {
        format!("{}d", secs / 86400)
    }
}

/// "45%" ("n/a" for NaN / infinite values, which are never rendered as numbers).
pub fn format_pct(pct: f64) -> String {
    if pct.is_finite() {
        format!("{:.0}%", pct)
    } else {
        "n/a".to_string()
    }
}

/// Byte rate per minute for trends and forecasts: "+400M/min", "-1.2G/min".
pub fn format_rate_per_min(bytes_per_min: f64) -> String {
    if !bytes_per_min.is_finite() {
        return "n/a".to_string();
    }
    let sign = if bytes_per_min < 0.0 { "-" } else { "+" };
    format!(
        "{sign}{}/min",
        format_bytes_short(bytes_per_min.abs().round() as u64)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sizes() {
        assert_eq!(parse_bytes("13G").unwrap(), 13 * GIB);
        assert_eq!(parse_bytes("13g").unwrap(), 13 * GIB);
        assert_eq!(parse_bytes("500M").unwrap(), 500 * MIB);
        assert_eq!(parse_bytes("1.5GiB").unwrap(), 3 * GIB / 2);
        assert_eq!(parse_bytes("2GB").unwrap(), 2_000_000_000);
        assert_eq!(parse_bytes("4096").unwrap(), 4096);
        assert_eq!(parse_bytes("13e9"), Err(UnitError::Unit("e9".into())));
        assert!(parse_bytes("").is_err());
        assert!(parse_bytes("abc").is_err());
    }

    #[test]
    fn parse_durations() {
        assert_eq!(parse_duration_s("30m").unwrap(), 1800.0);
        assert_eq!(parse_duration_s("5h").unwrap(), 18000.0);
        assert_eq!(parse_duration_s("2s").unwrap(), 2.0);
        assert_eq!(parse_duration_s("500ms").unwrap(), 0.5);
        assert_eq!(parse_duration_s("1h30m").unwrap(), 5400.0);
        assert!(parse_duration_s("5x").is_err());
    }

    #[test]
    fn parse_sizes_edge_cases() {
        assert_eq!(parse_bytes("13 G").unwrap(), 13 * GIB);
        assert_eq!(parse_bytes("2Gi").unwrap(), 2 * GIB);
        assert_eq!(parse_bytes("1_000").unwrap(), 1000);
        assert_eq!(parse_bytes("0.5k").unwrap(), 512);
        assert!(parse_bytes("-5G").is_err());
        assert!(parse_bytes("1.2.3G").is_err());
        assert!(parse_bytes("99999999999T").is_err());
        assert!(parse_bytes("G").is_err());
    }

    #[test]
    fn parse_percents() {
        assert_eq!(parse_percent("90%").unwrap(), 90.0);
        assert_eq!(parse_percent(" 60 % ").unwrap(), 60.0);
        assert_eq!(parse_percent("50").unwrap(), 50.0);
        assert_eq!(parse_percent("900‰").unwrap(), 90.0);
        assert_eq!(parse_percent("9000‱").unwrap(), 90.0);
        assert!(parse_percent("101%").is_err());
        assert!(parse_percent("x%").is_err());
        assert!(parse_percent("").is_err());
    }

    #[test]
    fn parse_systemd_timespans() {
        assert_eq!(parse_duration_s("30s").unwrap(), 30.0);
        assert_eq!(parse_duration_s("1min 30s").unwrap(), 90.0);
        assert_eq!(parse_duration_s("20sec").unwrap(), 20.0);
        assert_eq!(parse_duration_s("250msec").unwrap(), 0.25);
        assert_eq!(parse_duration_s("2 minutes").unwrap(), 120.0);
        assert_eq!(parse_duration_s("1w").unwrap(), 604800.0);
        assert!(parse_duration_s("infinity").is_err());
    }

    #[test]
    fn formatting() {
        assert_eq!(format_pct(f64::NAN), "n/a");
        assert_eq!(format_rate_per_min(400.0 * MIB as f64), "+400M/min");
        assert_eq!(format_rate_per_min(-1.5 * GIB as f64), "-1.5G/min");
        assert_eq!(format_bytes(0, UnitSystem::Iec, 1), "0 B");
        assert_eq!(format_bytes(9_900 * MIB, UnitSystem::Iec, 1), "9.7 GiB");
        assert_eq!(format_bytes(9_900_000_000, UnitSystem::Si, 1), "9.9 GB");
        assert_eq!(format_bytes_short(3 * GIB), "3.0G");
        assert_eq!(format_bytes_short(512 * MIB), "512M");
        assert_eq!(format_duration(13200), "3h40m");
        assert_eq!(format_duration(18000), "5h");
        assert_eq!(format_duration(360), "6m");
        assert_eq!(format_bytes_signed(-(GIB as i64), UnitSystem::Iec, 1), "-1.0 GiB");
    }

    #[test]
    fn formatting_rolls_over_instead_of_printing_1024() {
        // 1023.9 KiB used to print as "1024K" / "1024.0 KiB".
        let b = 1023 * KIB + 1000;
        assert_eq!(format_bytes_short(b), "1.0M");
        assert_eq!(format_bytes(b, UnitSystem::Iec, 1), "1.0 MiB");
        assert_eq!(format_bytes(MIB - 1, UnitSystem::Iec, 3), "1023.999 KiB");
        assert_eq!(format_bytes_short(1023 * KIB), "1023K");
        assert_eq!(format_bytes_short(u64::MAX), "16777216T");
        assert_eq!(format_bytes(999_960_000, UnitSystem::Si, 1), "1.0 GB");
        assert_eq!(format_bytes(1023, UnitSystem::Iec, 1), "1023 B");
    }

    #[test]
    fn durations_reject_overflow_and_accept_greek_mu() {
        let huge = format!("{}w", "9".repeat(400));
        assert!(parse_duration_s(&huge).is_err());
        assert_eq!(parse_duration_s("500μs").unwrap(), 0.0005);
        assert_eq!(parse_duration_s("500µs").unwrap(), 0.0005);
    }
}

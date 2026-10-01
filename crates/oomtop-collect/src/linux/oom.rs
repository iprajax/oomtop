//! Which OOM killer applies and at which thresholds (SPEC §8.3), pure over recorded files:
//! - kernel OOM killer: always;
//! - `systemd-oomd`: when running; thresholds from `oomd.conf` (+ drop-ins) and the unit `ManagedOOM*`
//!   drop-ins distributions ship (`-.slice`, `user@.service`); defaults swap used > 90 %, memory pressure > 60 %
//!   for 30 s (`oomd.conf(5)`). The swap rule needs memory used **and** swap used above `SwapUsedLimit`;
//!   the pressure rule compares a cgroup's PSI `full avg10` with the limit;
//! - `earlyoom`: when running; thresholds from its argv (`-m/-s/-M/-S`), else `/etc/default/earlyoom`, else
//!   its defaults (mem ≤ 10 % **and** swap ≤ 10 %).

use super::parse::{parse_env_lines, parse_ini_section, parse_percent, parse_timespan_s, shell_words};
use oomtop_core::{HostMemory, OomKiller, OomThreshold, ThresholdMetric};
use std::collections::BTreeMap;

/// Main `oomd.conf` locations, lowest priority first. systemd reads only the highest-priority one that
/// exists (`systemd.syntax(7)`); drop-ins then override it key by key.
pub const OOMD_CONF_FILES: &[&str] = &[
    "/usr/lib/systemd/oomd.conf",
    "/usr/local/lib/systemd/oomd.conf",
    "/run/systemd/oomd.conf",
    "/etc/systemd/oomd.conf",
];
/// `oomd.conf.d` drop-in directories, lowest priority first (same file name: later directory wins).
pub const OOMD_DROPIN_DIRS: &[&str] = &[
    "/usr/lib/systemd/oomd.conf.d",
    "/lib/systemd/oomd.conf.d",
    "/usr/local/lib/systemd/oomd.conf.d",
    "/run/systemd/oomd.conf.d",
    "/etc/systemd/oomd.conf.d",
];
/// Unit directories searched for `ManagedOOM*` drop-ins, lowest priority first.
pub const UNIT_DIRS: &[&str] = &[
    "/usr/lib/systemd/system",
    "/lib/systemd/system",
    "/run/systemd/system",
    "/etc/systemd/system",
];
/// Units whose drop-ins commonly carry `ManagedOOM*` (Fedora/Ubuntu defaults).
pub const OOMD_UNITS: &[&str] = &["-.slice", "user.slice", "user@.service", "system.slice"];
/// Debian/Ubuntu/Fedora earlyoom defaults file.
pub const EARLYOOM_DEFAULTS: &str = "/etc/default/earlyoom";

/// Daemon process names (`comm`) detected by the host scan.
pub const OOMD_COMM: &str = "systemd-oomd";
pub const EARLYOOM_COMM: &str = "earlyoom";

/// Resolved systemd-oomd configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct OomdConfig {
    pub swap_used_limit_pct: f64,
    pub pressure_limit_pct: f64,
    pub pressure_duration_s: f64,
    /// Any unit has `ManagedOOMSwap=kill`.
    pub managed_swap: bool,
    /// Any unit has `ManagedOOMMemoryPressure=kill`.
    pub managed_pressure: bool,
    /// Whether any unit drop-in with `ManagedOOM*` was found.
    pub units_found: bool,
    /// Files that contributed, for `OomThreshold::source`.
    pub sources: Vec<String>,
}

impl Default for OomdConfig {
    fn default() -> Self {
        OomdConfig {
            swap_used_limit_pct: 90.0,
            pressure_limit_pct: 60.0,
            pressure_duration_s: 30.0,
            managed_swap: false,
            managed_pressure: false,
            units_found: false,
            sources: Vec::new(),
        }
    }
}

/// Files of `dirs` (keys `<dir>/<name>.conf`), resolved by name (later dir wins) and sorted by name.
fn dropins<'a>(files: &'a BTreeMap<String, String>, dirs: &[String]) -> Vec<(&'a String, &'a String)> {
    let mut by_name: BTreeMap<&str, (usize, &String, &String)> = BTreeMap::new();
    for (prio, dir) in dirs.iter().enumerate() {
        let prefix = format!("{dir}/");
        for (k, v) in files.range(prefix.clone()..) {
            let Some(name) = k.strip_prefix(&prefix) else {
                break;
            };
            if name.contains('/') || !name.ends_with(".conf") {
                continue;
            }
            match by_name.get(name) {
                Some((p, _, _)) if *p > prio => {}
                _ => {
                    by_name.insert(name, (prio, k, v));
                }
            }
        }
    }
    by_name.into_values().map(|(_, k, v)| (k, v)).collect()
}

/// Resolves the oomd configuration from recorded files (keys = absolute paths).
pub fn resolve_oomd(files: &BTreeMap<String, String>) -> OomdConfig {
    let mut c = OomdConfig::default();
    let apply = |c: &mut OomdConfig, key: &str, text: &str| {
        let s = parse_ini_section(text, "OOM");
        let mut used = false;
        if let Some(v) = s.get("SwapUsedLimit").and_then(|v| parse_percent(v)) {
            c.swap_used_limit_pct = v;
            used = true;
        }
        if let Some(v) = s.get("DefaultMemoryPressureLimit").and_then(|v| parse_percent(v)) {
            c.pressure_limit_pct = v;
            used = true;
        }
        if let Some(v) = s
            .get("DefaultMemoryPressureDurationSec")
            .and_then(|v| parse_timespan_s(v))
        {
            c.pressure_duration_s = v;
            used = true;
        }
        if used {
            c.sources.push(key.to_string());
        }
    };
    // Only the highest-priority main file counts (a /usr/lib default is ignored once /etc has one).
    if let Some((f, t)) = OOMD_CONF_FILES
        .iter()
        .rev()
        .find_map(|f| files.get(*f).map(|t| (*f, t)))
    {
        apply(&mut c, f, t);
    }
    let dirs: Vec<String> = OOMD_DROPIN_DIRS.iter().map(|d| d.to_string()).collect();
    for (k, v) in dropins(files, &dirs) {
        apply(&mut c, k, v);
    }
    // Unit drop-ins: a per-unit ManagedOOMMemoryPressureLimit overrides the default limit.
    let mut unit_limit: Option<f64> = None;
    let mut unit_duration: Option<f64> = None;
    for unit in OOMD_UNITS {
        let dirs: Vec<String> = UNIT_DIRS.iter().map(|d| format!("{d}/{unit}.d")).collect();
        for (k, v) in dropins(files, &dirs) {
            let kv = parse_env_lines(v);
            let mut used = false;
            if let Some(m) = kv.get("ManagedOOMSwap") {
                c.managed_swap |= m == "kill";
                used = true;
            }
            if let Some(m) = kv.get("ManagedOOMMemoryPressure") {
                c.managed_pressure |= m == "kill";
                used = true;
            }
            if let Some(l) = kv
                .get("ManagedOOMMemoryPressureLimit")
                .and_then(|v| parse_percent(v))
            {
                // Several units: the lowest limit acts first.
                unit_limit = Some(unit_limit.map_or(l, |u| u.min(l)));
                used = true;
            }
            if let Some(d) = kv
                .get("ManagedOOMMemoryPressureDurationSec")
                .and_then(|v| parse_timespan_s(v))
            {
                unit_duration = Some(unit_duration.map_or(d, |u| u.min(d)));
                used = true;
            }
            if used {
                c.units_found = true;
                c.sources.push(k.clone());
            }
        }
    }
    if let Some(l) = unit_limit {
        c.pressure_limit_pct = l;
    }
    if let Some(d) = unit_duration {
        c.pressure_duration_s = d;
    }
    c
}

/// Thresholds systemd-oomd acts on. Conservative: when no unit drop-ins are readable, both the swap and
/// the pressure rules are assumed active (the distribution defaults), so the forecast never misses them.
pub fn oomd_thresholds(c: &OomdConfig) -> Vec<OomThreshold> {
    let origin = if c.sources.is_empty() {
        "systemd-oomd defaults".to_string()
    } else {
        c.sources.join(", ")
    };
    let assume = !c.units_found;
    let mut out = Vec::new();
    if c.managed_swap || assume {
        let and_mem = format!("acts when memory used is also ≥ {:.0} %", c.swap_used_limit_pct);
        out.push(OomThreshold {
            killer: OomKiller::SystemdOomd,
            metric: ThresholdMetric::SwapUsedPct,
            value: c.swap_used_limit_pct,
            duration_s: None,
            source: if assume {
                format!("{origin} (ManagedOOMSwap assumed; {and_mem})")
            } else {
                format!("{origin} ({and_mem})")
            },
        });
    }
    if c.managed_pressure || assume {
        out.push(OomThreshold {
            killer: OomKiller::SystemdOomd,
            metric: ThresholdMetric::MemPressurePct,
            value: c.pressure_limit_pct,
            duration_s: Some(c.pressure_duration_s.round().clamp(0.0, u32::MAX as f64) as u32),
            source: if assume {
                format!("{origin} (ManagedOOMMemoryPressure assumed)")
            } else {
                origin
            },
        });
    }
    out
}

/// earlyoom trigger levels (SIGTERM levels; SIGKILL levels are lower and act later).
#[derive(Debug, Clone, PartialEq)]
pub struct EarlyoomConfig {
    pub mem_pct: f64,
    pub swap_free_pct: f64,
    pub mem_kib: Option<u64>,
    pub swap_free_kib: Option<u64>,
    pub source: String,
}

impl Default for EarlyoomConfig {
    fn default() -> Self {
        EarlyoomConfig {
            mem_pct: 10.0,
            swap_free_pct: 10.0,
            mem_kib: None,
            swap_free_kib: None,
            source: "earlyoom defaults".into(),
        }
    }
}

fn first_number(v: &str) -> Option<f64> {
    v.split(',').next()?.trim().parse().ok()
}

/// Parses earlyoom argv (`argv[0]` optional). Unknown options are ignored.
pub fn parse_earlyoom_args(args: &[String], source: &str) -> EarlyoomConfig {
    let mut c = EarlyoomConfig {
        source: source.to_string(),
        ..Default::default()
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let (flag, attached) = match a.as_bytes() {
            [b'-', f, rest @ ..] if *f != b'-' && matches!(f, b'm' | b's' | b'M' | b'S') => {
                (*f as char, (!rest.is_empty()).then(|| a[2..].to_string()))
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let value = match attached {
            Some(v) => v,
            None => {
                i += 1;
                match args.get(i) {
                    Some(v) => v.clone(),
                    None => break,
                }
            }
        };
        if let Some(n) = first_number(&value) {
            match flag {
                'm' if (0.0..=100.0).contains(&n) => c.mem_pct = n,
                's' if (0.0..=100.0).contains(&n) => c.swap_free_pct = n,
                'M' if n >= 0.0 => c.mem_kib = Some(n as u64),
                'S' if n >= 0.0 => c.swap_free_kib = Some(n as u64),
                _ => {}
            }
        }
        i += 1;
    }
    c
}

/// Resolves earlyoom's configuration: running argv (NUL-separated `cmdline`) → defaults file → defaults.
pub fn resolve_earlyoom(cmdline: Option<&str>, defaults_file: Option<&str>) -> EarlyoomConfig {
    if let Some(cl) = cmdline {
        let argv: Vec<String> = cl
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if argv.len() > 1 {
            return parse_earlyoom_args(&argv[1..], "earlyoom argv");
        }
    }
    if let Some(t) = defaults_file {
        if let Some(a) = parse_env_lines(t).get("EARLYOOM_ARGS") {
            return parse_earlyoom_args(&shell_words(a), EARLYOOM_DEFAULTS);
        }
    }
    EarlyoomConfig::default()
}

/// earlyoom acts when available memory **and** free swap are both below their levels; both thresholds are
/// listed (the source says so). `-M/-S` sizes are converted with the host totals when known; earlyoom uses
/// the lower of the percent and size levels.
pub fn earlyoom_thresholds(
    c: &EarlyoomConfig,
    mem_total: Option<u64>,
    swap_total: Option<u64>,
) -> Vec<OomThreshold> {
    let mut mem_pct = c.mem_pct;
    if let (Some(kib), Some(t)) = (c.mem_kib, mem_total.filter(|t| *t > 0)) {
        mem_pct = mem_pct.min(kib as f64 * 1024.0 / t as f64 * 100.0);
    }
    let mut swap_free_pct = c.swap_free_pct;
    if let (Some(kib), Some(t)) = (c.swap_free_kib, swap_total.filter(|t| *t > 0)) {
        swap_free_pct = swap_free_pct.min(kib as f64 * 1024.0 / t as f64 * 100.0);
    }
    let src = format!("{} (acts when both hold)", c.source);
    let mut out = vec![OomThreshold {
        killer: OomKiller::Earlyoom,
        metric: ThresholdMetric::AvailablePct,
        value: (mem_pct * 100.0).round() / 100.0,
        duration_s: None,
        source: src.clone(),
    }];
    if let Some(kib) = c.mem_kib {
        out.push(OomThreshold {
            killer: OomKiller::Earlyoom,
            metric: ThresholdMetric::AvailableBytes,
            value: kib as f64 * 1024.0,
            duration_s: None,
            source: src.clone(),
        });
    }
    out.push(OomThreshold {
        killer: OomKiller::Earlyoom,
        metric: ThresholdMetric::SwapUsedPct,
        value: ((100.0 - swap_free_pct) * 100.0).round() / 100.0,
        duration_s: None,
        source: src,
    });
    out
}

/// Current value of a threshold metric, `None` when not measurable (e.g. no swap).
fn current(metric: ThresholdMetric, m: &HostMemory) -> Option<f64> {
    let total = m.total.value.filter(|t| *t > 0)? as f64;
    match metric {
        ThresholdMetric::AvailablePct => Some(m.available.value? as f64 / total * 100.0),
        ThresholdMetric::AvailableBytes => Some(m.available.value? as f64),
        ThresholdMetric::SwapUsedPct => {
            let st = m.swap_total.value.filter(|t| *t > 0)? as f64;
            Some(m.swap_used.value? as f64 / st * 100.0)
        }
        ThresholdMetric::MemPressurePct => m.psi.value.map(|p| p.full_avg10),
    }
}

/// Distance to a threshold in percentage points (≤ 0 = crossed); `None` when not measurable.
pub fn distance_pct(t: &OomThreshold, m: &HostMemory) -> Option<f64> {
    let cur = current(t.metric, m)?;
    Some(match t.metric {
        ThresholdMetric::AvailablePct => cur - t.value,
        ThresholdMetric::AvailableBytes => {
            let total = m.total.value? as f64;
            (cur - t.value) / total * 100.0
        }
        ThresholdMetric::SwapUsedPct | ThresholdMetric::MemPressurePct => t.value - cur,
    })
}

/// The killer that will act first: per killer, oomd acts on any rule (min distance) — its swap rule itself
/// needs swap used **and** memory used above `SwapUsedLimit` (max of the two) — and earlyoom needs both of
/// its rules (max distance); the kernel acts at ~0 available. Ties prefer userspace killers (they act first).
pub fn nearest_killer(killers: &[OomKiller], thresholds: &[OomThreshold], m: &HostMemory) -> OomKiller {
    let mut best = (OomKiller::Kernel, f64::INFINITY);
    let kernel_d = m
        .available
        .value
        .zip(m.total.value.filter(|t| *t > 0))
        .map(|(a, t)| a as f64 / t as f64 * 100.0)
        .unwrap_or(f64::INFINITY);
    best.1 = kernel_d;
    for k in killers {
        let ds: Vec<f64> = thresholds
            .iter()
            .filter(|t| t.killer == *k)
            .map(|t| {
                match (*k, t.metric) {
                    // earlyoom with no swap: the swap condition is always met.
                    (OomKiller::Earlyoom, ThresholdMetric::SwapUsedPct) => {
                        distance_pct(t, m).unwrap_or(f64::NEG_INFINITY)
                    }
                    // oomd's swap rule: swap used ≥ limit AND memory used ≥ limit (available ≤ 100 − limit).
                    // No swap → the rule never fires.
                    (OomKiller::SystemdOomd, ThresholdMetric::SwapUsedPct) => {
                        let swap = distance_pct(t, m).unwrap_or(f64::INFINITY);
                        let mem = OomThreshold {
                            metric: ThresholdMetric::AvailablePct,
                            value: (100.0 - t.value).clamp(0.0, 100.0),
                            ..t.clone()
                        };
                        swap.max(distance_pct(&mem, m).unwrap_or(f64::INFINITY))
                    }
                    _ => distance_pct(t, m).unwrap_or(f64::INFINITY),
                }
            })
            .collect();
        if ds.is_empty() {
            continue;
        }
        let d = match k {
            OomKiller::Earlyoom => ds.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            _ => ds.iter().copied().fold(f64::INFINITY, f64::min),
        };
        if d <= best.1 && d.is_finite() {
            best = (*k, d);
        }
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::{Measured, Psi};

    fn files(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn oomd_defaults_and_overrides() {
        let c = resolve_oomd(&BTreeMap::new());
        assert_eq!(c, OomdConfig::default());
        let t = oomd_thresholds(&c);
        assert_eq!(t.len(), 2);
        assert!(t[0].source.contains("assumed"));

        let f = files(&[
            ("/etc/systemd/oomd.conf", "[OOM]\nSwapUsedLimit=85%\n"),
            (
                "/usr/lib/systemd/oomd.conf.d/10-a.conf",
                "[OOM]\nDefaultMemoryPressureDurationSec=20s\n",
            ),
            (
                "/etc/systemd/oomd.conf.d/10-a.conf",
                "[OOM]\nDefaultMemoryPressureDurationSec=10s\n",
            ),
            (
                "/usr/lib/systemd/system/-.slice.d/10-oomd-root-slice-defaults.conf",
                "[Slice]\nManagedOOMSwap=kill\n",
            ),
            (
                "/usr/lib/systemd/system/user@.service.d/10-oomd-user-service-defaults.conf",
                "[Service]\nManagedOOMMemoryPressure=kill\nManagedOOMMemoryPressureLimit=50%\n",
            ),
        ]);
        let c = resolve_oomd(&f);
        assert_eq!(c.swap_used_limit_pct, 85.0);
        assert_eq!(
            c.pressure_duration_s, 10.0,
            "/etc drop-in overrides /usr/lib of the same name"
        );
        assert_eq!(c.pressure_limit_pct, 50.0);
        assert!(c.managed_swap && c.managed_pressure && c.units_found);
        let t = oomd_thresholds(&c);
        assert_eq!(t[0].metric, ThresholdMetric::SwapUsedPct);
        assert_eq!(t[0].value, 85.0);
        assert_eq!(t[1].value, 50.0);
        assert_eq!(t[1].duration_s, Some(10));
        assert!(!t[1].source.contains("assumed"));

        // Only the highest-priority main file is read: /etc wins over /usr/lib entirely.
        let f = files(&[
            (
                "/usr/lib/systemd/oomd.conf",
                "[OOM]\nSwapUsedLimit=70%\nDefaultMemoryPressureLimit=40%\n",
            ),
            ("/etc/systemd/oomd.conf", "[OOM]\nSwapUsedLimit=85%\n"),
        ]);
        let c = resolve_oomd(&f);
        assert_eq!((c.swap_used_limit_pct, c.pressure_limit_pct), (85.0, 60.0));
        assert_eq!(c.sources, vec!["/etc/systemd/oomd.conf".to_string()]);

        // Only the swap rule configured → only the swap threshold.
        let f = files(&[(
            "/etc/systemd/system/-.slice.d/x.conf",
            "[Slice]\nManagedOOMSwap=kill\n",
        )]);
        let t = oomd_thresholds(&resolve_oomd(&f));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn earlyoom_argv() {
        let c = resolve_earlyoom(Some("/usr/bin/earlyoom\0-m\x005,2\0-s\x0020\0-r\x003600\0"), None);
        assert_eq!((c.mem_pct, c.swap_free_pct), (5.0, 20.0));
        assert_eq!(c.source, "earlyoom argv");
        let c = resolve_earlyoom(Some("earlyoom\0-m3\0-M\x00400000\0"), None);
        assert_eq!(c.mem_pct, 3.0);
        assert_eq!(c.mem_kib, Some(400_000));
        let c = resolve_earlyoom(
            Some("earlyoom\0"),
            Some("EARLYOOM_ARGS=\"-m 7 -s 50 --avoid '(^|/)(init|Xorg)$'\"\n"),
        );
        assert_eq!((c.mem_pct, c.swap_free_pct), (7.0, 50.0));
        assert_eq!(c.source, EARLYOOM_DEFAULTS);
        assert_eq!(resolve_earlyoom(None, None), EarlyoomConfig::default());
        let t = earlyoom_thresholds(&c, Some(16 << 30), Some(4 << 30));
        assert_eq!(t[0].metric, ThresholdMetric::AvailablePct);
        assert_eq!(t[0].value, 7.0);
        assert_eq!(t.last().unwrap().value, 50.0);
        // -M lower than -m → the size level wins.
        let c = parse_earlyoom_args(&["-m".into(), "10".into(), "-M".into(), "838861".into()], "argv");
        let t = earlyoom_thresholds(&c, Some(16 << 30), None);
        assert!((t[0].value - 5.0).abs() < 0.01, "{}", t[0].value);
    }

    fn mem(avail_pct: f64, swap_used_pct: Option<f64>, full10: f64) -> HostMemory {
        let total = 100u64 << 30;
        let swap_total = 10u64 << 30;
        HostMemory {
            total: Measured::exact(total, "t"),
            available: Measured::exact((total as f64 * avail_pct / 100.0) as u64, "t"),
            swap_total: Measured::exact(if swap_used_pct.is_some() { swap_total } else { 0 }, "t"),
            swap_used: Measured::exact(
                (swap_total as f64 * swap_used_pct.unwrap_or(0.0) / 100.0) as u64,
                "t",
            ),
            psi: Measured::exact(
                Psi {
                    full_avg10: full10,
                    ..Default::default()
                },
                "t",
            ),
            ..Default::default()
        }
    }

    #[test]
    fn nearest() {
        let mut th = oomd_thresholds(&OomdConfig::default());
        assert!(th[0].source.contains("memory used is also"), "{}", th[0].source);
        let killers = [OomKiller::Kernel, OomKiller::SystemdOomd];
        // Swap 95 % used but 30 % of RAM available: oomd's swap rule also needs memory used ≥ 90 %, so the
        // rule is 20 pts away and the kernel (30 pts) is not nearer either → oomd, at distance 20.
        assert_eq!(
            nearest_killer(&killers, &th, &mem(30.0, Some(95.0), 1.0)),
            OomKiller::SystemdOomd
        );
        // Swap 95 % used, 12 % available: the kernel needs 12 pts, oomd's AND rule only 2 → oomd.
        assert_eq!(
            nearest_killer(&killers, &th, &mem(12.0, Some(95.0), 1.0)),
            OomKiller::SystemdOomd
        );
        // Swap 50 % used (40 pts from the limit) and 5 % available: the kernel (5 pts) is nearer.
        assert_eq!(
            nearest_killer(&killers, &th, &mem(5.0, Some(50.0), 1.0)),
            OomKiller::Kernel
        );
        // No swap, low pressure, 2 % available → kernel is nearer than oomd pressure (59 pts).
        assert_eq!(
            nearest_killer(&killers, &th, &mem(2.0, None, 1.0)),
            OomKiller::Kernel
        );
        // earlyoom needs both: 8 % available (−2) but swap only 20 % used (70 pts) → distance 70.
        th.extend(earlyoom_thresholds(&EarlyoomConfig::default(), None, None));
        let all = [OomKiller::Kernel, OomKiller::SystemdOomd, OomKiller::Earlyoom];
        assert_eq!(
            nearest_killer(&all, &th, &mem(8.0, Some(20.0), 1.0)),
            OomKiller::Kernel
        );
        // No swap: earlyoom acts on memory alone.
        assert_eq!(
            nearest_killer(
                &[OomKiller::Kernel, OomKiller::Earlyoom],
                &th,
                &mem(12.0, None, 0.0)
            ),
            OomKiller::Earlyoom
        );
    }
}

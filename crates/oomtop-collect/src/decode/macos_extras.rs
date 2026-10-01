//! Pure decoder for the macOS thermal / power / GPU / jetsam data carried in [`MacHostRaw`] (SPEC §5, §8.3,
//! §9). **No OS dependencies** — this file compiles on every platform and can be moved next to
//! `decode/macos.rs` unchanged (see the collect-macos report: raw.rs/decode are owned elsewhere).
//!
//! The raw values live in `MacHostRaw`'s existing maps under the namespaced keys of [`keys`], so fixtures
//! recorded today already contain them and older fixtures (without the keys) decode to `unavailable`:
//!
//! | key (map) | meaning |
//! |---|---|
//! | `notify.com.apple.system.thermalpressurelevel` (sysctl) | `kOSThermalPressureLevel*` 0 nominal … 4 sleeping |
//! | `nsprocessinfo.thermalState` (sysctl) | `NSProcessInfoThermalState` 0 nominal … 3 critical |
//! | `nsprocessinfo.isLowPowerModeEnabled` (sysctl) | 0/1 |
//! | `iops.providing_ac` (sysctl), `iops.providing` (str) | `IOPSGetProvidingPowerSourceType` = "AC Power" |
//! | `iops.battery.{present,current_capacity,max_capacity,is_charging,time_to_empty_min,on_ac}` (sysctl) | internal battery |
//! | `iops.adapter.watts` (sysctl) | `IOPSCopyExternalPowerAdapterDetails` Watts |
//! | `ioaccel.<PerformanceStatistics key>` (sysctl), `ioaccel.model` (str), `ioaccel.gpu-core-count` | IOAccelerator |
//! | `ioreport.window_us`, `ioreport.energy_nj.<channel>` (sysctl) | IOReport Energy Model delta |
//! | `ioreport.residency.<cpu|gpu>.<channel>` (str) | ordered `STATE:ticks,…` residency delta |
//! | `dvfs.<pmgr table>` (str) | `MHz,MHz,…` performance-state frequencies |
//! | `jetsam.<JetsamEvent-*.ips>` (str) | JSON [`JetsamRecord`] |
//! | `unavailable.<part>` (str) | why a part could not be read |

use crate::decode::PartialSnapshot;
use crate::raw::MacHostRaw;
use oomtop_core::{
    ClusterFreq, GpuVendor, Measured, OomKill, OomKiller, SourceStatus, Thermal, ThermalPressure,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Raw key names (shared by the readers and this decoder).
pub mod keys {
    pub const UNAVAILABLE_PREFIX: &str = "unavailable.";
    pub const PART_THERMAL: &str = "thermal";
    pub const PART_LOW_POWER: &str = "low_power";
    pub const PART_POWER: &str = "power";
    pub const PART_IOACCEL: &str = "ioaccel";
    pub const PART_IOREPORT: &str = "ioreport";
    pub const PART_DVFS: &str = "dvfs";
    pub const PART_JETSAM: &str = "jetsam";

    pub const THERMAL_NOTIFY: &str = "notify.com.apple.system.thermalpressurelevel";
    pub const THERMAL_STATE: &str = "nsprocessinfo.thermalState";
    pub const LOW_POWER_MODE: &str = "nsprocessinfo.isLowPowerModeEnabled";

    pub const PS_ON_AC: &str = "iops.providing_ac";
    pub const PS_PROVIDING: &str = "iops.providing";
    pub const BATT_PRESENT: &str = "iops.battery.present";
    pub const BATT_CURRENT: &str = "iops.battery.current_capacity";
    pub const BATT_MAX: &str = "iops.battery.max_capacity";
    pub const BATT_CHARGING: &str = "iops.battery.is_charging";
    pub const BATT_TIME_TO_EMPTY: &str = "iops.battery.time_to_empty_min";
    pub const BATT_ON_AC: &str = "iops.battery.on_ac";
    pub const ADAPTER_WATTS: &str = "iops.adapter.watts";

    pub const IOACCEL_PREFIX: &str = "ioaccel.";
    pub const IOACCEL_MODEL: &str = "ioaccel.model";
    pub const IOACCEL_CORES: &str = "ioaccel.gpu-core-count";
    /// `PerformanceStatistics` keys kept (others dropped).
    pub const IOACCEL_STATS: &[&str] = &[
        "In use system memory",
        "In use system memory (driver)",
        "Alloc system memory",
        "Device Utilization %",
        "Renderer Utilization %",
        "Tiler Utilization %",
    ];

    pub const IOREPORT_WINDOW_US: &str = "ioreport.window_us";
    pub const IOREPORT_ENERGY_PREFIX: &str = "ioreport.energy_nj.";
    pub const IOREPORT_RESIDENCY_PREFIX: &str = "ioreport.residency.";
    /// Energy channels kept (the Energy Model group has ~100 per-block channels).
    pub const IOREPORT_ENERGY_KEEP: &[&str] = &["CPU Energy", "GPU Energy", "ANE", "DRAM"];

    pub const DVFS_PREFIX: &str = "dvfs.";
    pub const JETSAM_PREFIX: &str = "jetsam.";
}

/// Source-status keys contributed by the extras (alongside `macos.host`).
pub mod status_keys {
    pub const THERMAL: &str = "macos.thermal";
    pub const POWER: &str = "macos.power";
    pub const IOREPORT: &str = "macos.ioreport";
    pub const IOACCEL: &str = "macos.ioaccel";
    pub const JETSAM: &str = "macos.jetsam";
}

/// One killed process in a JetsamEvent report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JetsamVictim {
    pub name: String,
    pub pid: Option<u32>,
    /// e.g. "per-process-limit", "vm-pageshortage", "highwater".
    pub reason: String,
}

/// What oomtop keeps from one `JetsamEvent-*.ips` (names only — no paths, argv or environments).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct JetsamRecord {
    /// Report time (header `timestamp`, else file mtime), ms since epoch.
    pub at_ms: u64,
    pub largest_process: Option<String>,
    pub killed: Vec<JetsamVictim>,
}

/// Recent-kill window shown in `Oom::recent_kills`.
pub const RECENT_KILLS_WINDOW_MS: u64 = 24 * 3600 * 1000;

const SRC_NOTIFY: &str = "notify com.apple.system.thermalpressurelevel";
const SRC_PI_THERMAL: &str = "NSProcessInfo.thermalState";
const SRC_LPM: &str = "NSProcessInfo.isLowPowerModeEnabled";
const SRC_IOPS: &str = "IOPSCopyPowerSourcesInfo";
const SRC_ADAPTER: &str = "IOPSCopyExternalPowerAdapterDetails.Watts";
const SRC_IOREPORT: &str = "IOReport";
const SRC_IOACCEL: &str = "IOAccelerator.PerformanceStatistics";

fn reason(h: &MacHostRaw, part: &str) -> Option<String> {
    h.sysctl_str
        .get(&format!("{}{part}", keys::UNAVAILABLE_PREFIX))
        .cloned()
}

fn int(h: &MacHostRaw, k: &str) -> Option<i64> {
    h.sysctl.get(k).copied()
}

/// Fanless Apple laptops by `hw.model` (MacBook Air since M1). `None` = unknown model.
pub fn fanless_model(model: &str) -> Option<bool> {
    const FANLESS: &[&str] = &[
        "MacBookAir10,1", // M1
        "Mac14,2",        // M2 13"
        "Mac14,15",       // M2 15"
        "Mac15,12",       // M3 13"
        "Mac15,13",       // M3 15"
        "Mac16,12",       // M4 13"
        "Mac16,13",       // M4 15"
        "Mac17,3",        // M5 13" (J813, verified on the dev machine)
    ];
    const FANNED_PREFIXES: &[&str] = &["MacBookPro", "Macmini", "MacPro", "iMac"];
    if FANLESS.contains(&model) {
        Some(true)
    } else if FANNED_PREFIXES.iter().any(|p| model.starts_with(p)) {
        Some(false)
    } else {
        None
    }
}

/// Parses `STATE:ticks,STATE:ticks,…` (order preserved).
pub fn parse_residency(s: &str) -> Vec<(String, i64)> {
    s.split(',')
        .filter_map(|kv| {
            let (k, v) = kv.rsplit_once(':')?;
            Some((k.to_string(), v.trim().parse().ok()?))
        })
        .collect()
}

/// Encodes residencies for [`parse_residency`].
pub fn encode_residency(states: &[(String, i64)]) -> String {
    states
        .iter()
        .map(|(k, v)| format!("{}:{v}", k.replace([',', ':'], "_")))
        .collect::<Vec<_>>()
        .join(",")
}

/// Parses `MHz,MHz,…`.
pub fn parse_mhz_list(s: &str) -> Vec<u32> {
    s.split(',').filter_map(|v| v.trim().parse().ok()).collect()
}

fn is_idle_state(name: &str) -> bool {
    matches!(name, "IDLE" | "OFF" | "DOWN")
}

/// Residency-weighted frequency of one cluster. `freqs_mhz` lists the active states' frequencies in state
/// order (zero entries = the "off" state are dropped). Active states beyond the table count as the table
/// maximum (conservative: never invents throttling from a short table).
pub fn cluster_freq(name: &str, states: &[(String, i64)], freqs_mhz: &[u32]) -> Option<ClusterFreq> {
    let freqs: Vec<u32> = freqs_mhz.iter().copied().filter(|f| *f > 0).collect();
    let max = *freqs.iter().max()?;
    // Sums in f64: raw values come from fixtures/IOReport and must never overflow (i64 sums panic in
    // debug builds and would skew the ratios if saturated).
    let total: f64 = states.iter().map(|s| s.1.max(0) as f64).sum();
    if total <= 0.0 {
        return None;
    }
    let active: Vec<f64> = states
        .iter()
        .filter(|s| !is_idle_state(&s.0))
        .map(|s| s.1.max(0) as f64)
        .collect();
    let busy: f64 = active.iter().sum();
    let cur = if busy > 0.0 {
        active
            .iter()
            .enumerate()
            .map(|(i, r)| r * freqs.get(i).copied().unwrap_or(max) as f64)
            .sum::<f64>()
            / busy
    } else {
        0.0
    };
    Some(ClusterFreq {
        name: name.to_string(),
        cur_mhz: cur,
        max_mhz: max as f64,
        active_pct: busy / total * 100.0,
    })
}

/// DVFS table used for an IOReport residency channel.
pub fn dvfs_table_for(channel: &str) -> Option<&'static str> {
    if channel.starts_with("cpu.ECPU") {
        Some("voltage-states1-sram")
    } else if channel.starts_with("cpu.PCPU") {
        Some("voltage-states5-sram")
    } else if channel == "gpu.GPUPH" {
        Some("voltage-states9")
    } else {
        None
    }
}

fn cluster_label(channel: &str) -> String {
    let c = channel.split_once('.').map(|x| x.1).unwrap_or(channel);
    if c == "GPUPH" {
        return "gpu".into();
    }
    let (kind, rest) = if let Some(r) = c.strip_prefix("ECPU") {
        ("E-cluster", r)
    } else if let Some(r) = c.strip_prefix("PCPU") {
        ("P-cluster", r)
    } else {
        return c.to_string();
    };
    if rest.is_empty() {
        kind.to_string()
    } else {
        format!("{kind} {rest}")
    }
}

/// Clusters (CPU first, then GPU) from IOReport residencies + DVFS tables.
pub fn clusters(h: &MacHostRaw) -> Vec<ClusterFreq> {
    let mut out: Vec<(bool, ClusterFreq)> = Vec::new();
    for (k, v) in &h.sysctl_str {
        let Some(channel) = k.strip_prefix(keys::IOREPORT_RESIDENCY_PREFIX) else {
            continue;
        };
        let Some(table) = dvfs_table_for(channel) else {
            continue;
        };
        let Some(freqs) = h.sysctl_str.get(&format!("{}{table}", keys::DVFS_PREFIX)) else {
            continue;
        };
        if let Some(c) = cluster_freq(
            &cluster_label(channel),
            &parse_residency(v),
            &parse_mhz_list(freqs),
        ) {
            out.push((channel.starts_with("gpu."), c));
        }
    }
    out.sort_by(|a, b| (a.0, &a.1.name).cmp(&(b.0, &b.1.name)));
    out.into_iter().map(|x| x.1).collect()
}

fn energy_w(h: &MacHostRaw, channels: &[&str]) -> Option<f64> {
    let window = int(h, keys::IOREPORT_WINDOW_US).filter(|w| *w > 0)? as f64;
    let mut sum = 0i64;
    let mut any = false;
    for c in channels {
        if let Some(v) = int(h, &format!("{}{c}", keys::IOREPORT_ENERGY_PREFIX)) {
            sum = sum.saturating_add(v.max(0));
            any = true;
        }
    }
    // nJ / µs = mW
    any.then(|| sum as f64 / window / 1000.0)
}

fn pressure_from_notify(v: i64) -> Option<ThermalPressure> {
    Some(match v {
        0 => ThermalPressure::Nominal,
        1 => ThermalPressure::Moderate,
        2 => ThermalPressure::Heavy,
        3 => ThermalPressure::Trapping,
        4 => ThermalPressure::Sleeping,
        _ => return None,
    })
}

fn pressure_from_thermal_state(v: i64) -> Option<ThermalPressure> {
    Some(match v {
        0 => ThermalPressure::Nominal,
        1 => ThermalPressure::Moderate,
        2 => ThermalPressure::Heavy,
        3 => ThermalPressure::Trapping,
        _ => return None,
    })
}

/// Everything the extras contribute to a snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extras {
    pub thermal: Thermal,
    /// Patch for the Apple GPU accelerator (`gpu0`).
    pub gpu: GpuPatch,
    pub recent_kills: Vec<OomKill>,
    pub fanless: Option<bool>,
    pub status: BTreeMap<String, SourceStatus>,
}

/// Fields of the Apple GPU `Accelerator` filled from IOAccelerator / IOReport.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpuPatch {
    pub name: Option<String>,
    pub util_pct: Measured<f64>,
    pub mem_used: Measured<u64>,
    pub power_w: Measured<f64>,
    pub clock_mhz: Measured<f64>,
    pub max_clock_mhz: Measured<f64>,
    pub throttle_reasons: Vec<String>,
}

fn part_status(h: &MacHostRaw, part: &str, have: bool) -> SourceStatus {
    match (have, reason(h, part)) {
        (true, None) => SourceStatus::Available,
        (true, Some(r)) => SourceStatus::Partial(r),
        (false, Some(r)) => SourceStatus::Unavailable(r),
        (false, None) => SourceStatus::Unavailable("not collected".into()),
    }
}

/// Jetsam records from the raw sample (sorted newest first).
pub fn jetsam_records(h: &MacHostRaw) -> Vec<(String, JetsamRecord)> {
    let mut v: Vec<(String, JetsamRecord)> = h
        .sysctl_str
        .iter()
        .filter_map(|(k, v)| {
            let file = k.strip_prefix(keys::JETSAM_PREFIX)?;
            Some((file.to_string(), serde_json::from_str(v).ok()?))
        })
        .collect();
    v.sort_by(|a, b| b.1.at_ms.cmp(&a.1.at_ms).then(a.0.cmp(&b.0)));
    v
}

/// Decodes the extras. `now_ms` = the sample's `taken_at_ms` (for the recent-kills window).
pub fn decode_extras(h: &MacHostRaw, now_ms: u64) -> Extras {
    let mut x = Extras::default();
    let t = &mut x.thermal;

    // Thermal pressure.
    t.pressure = match (
        int(h, keys::THERMAL_NOTIFY).and_then(pressure_from_notify),
        int(h, keys::THERMAL_STATE).and_then(pressure_from_thermal_state),
    ) {
        (Some(p), _) => Measured::exact(p, SRC_NOTIFY),
        (None, Some(p)) => Measured::estimate(p, format!("{SRC_PI_THERMAL} (notify key unreadable)")),
        (None, None) => Measured::unavailable(
            SRC_NOTIFY,
            reason(h, keys::PART_THERMAL).unwrap_or_else(|| "not collected".into()),
        ),
    };
    x.status.insert(
        status_keys::THERMAL.into(),
        part_status(h, keys::PART_THERMAL, t.pressure.value.is_some()),
    );

    t.low_power_mode = match int(h, keys::LOW_POWER_MODE) {
        Some(v) => Measured::exact(v != 0, SRC_LPM),
        None => Measured::unavailable(
            SRC_LPM,
            reason(h, keys::PART_LOW_POWER).unwrap_or_else(|| "not collected".into()),
        ),
    };

    // Power sources.
    let power_reason = reason(h, keys::PART_POWER).unwrap_or_else(|| "not collected".into());
    let battery_present = int(h, keys::BATT_PRESENT);
    let on_ac = int(h, keys::PS_ON_AC).or(int(h, keys::BATT_ON_AC));
    t.on_battery = match on_ac {
        Some(ac) => Measured::exact(ac == 0, format!("{SRC_IOPS}: providing power source")),
        None if battery_present == Some(0) => Measured::exact(false, format!("{SRC_IOPS}: no battery")),
        None => Measured::unavailable(SRC_IOPS, power_reason.clone()),
    };
    t.battery_pct = match (
        battery_present,
        int(h, keys::BATT_CURRENT),
        int(h, keys::BATT_MAX),
    ) {
        (Some(1), Some(cur), Some(max)) if max > 0 => Measured::exact(
            ((cur as f64 * 100.0) / max as f64).clamp(0.0, 100.0),
            format!("{SRC_IOPS}: Current/Max Capacity"),
        ),
        (Some(0), ..) => Measured::unavailable(SRC_IOPS, "no internal battery"),
        _ => Measured::unavailable(SRC_IOPS, power_reason.clone()),
    };
    t.adapter_watts = match int(h, keys::ADAPTER_WATTS) {
        Some(w) if w > 0 => Measured::exact(w as f64, SRC_ADAPTER),
        _ if on_ac == Some(0) => Measured::unavailable(SRC_ADAPTER, "on battery (no adapter)"),
        _ => Measured::unavailable(SRC_ADAPTER, "adapter wattage not reported"),
    };
    x.status.insert(
        status_keys::POWER.into(),
        part_status(h, keys::PART_POWER, t.on_battery.value.is_some()),
    );

    // IOReport: clusters, throttle factor, package power.
    let ioreport_reason = reason(h, keys::PART_IOREPORT)
        .or_else(|| reason(h, keys::PART_DVFS))
        .unwrap_or_else(|| "not collected".into());
    t.clusters = clusters(h);
    t.throttle_factor = if t.clusters.is_empty() {
        Measured::unavailable(SRC_IOREPORT, ioreport_reason.clone())
    } else {
        oomtop_core::throttle::throttle_factor(&t.clusters)
    };
    t.package_power_w = match energy_w(h, &["CPU Energy", "GPU Energy", "ANE"]) {
        Some(w) => Measured::estimate(w, "IOReport Energy Model: CPU + GPU + ANE"),
        None => Measured::unavailable(SRC_IOREPORT, ioreport_reason.clone()),
    };
    t.trip_point_hit = Measured::unavailable("thermal zones", "Linux only");
    x.status.insert(
        status_keys::IOREPORT.into(),
        if t.clusters.is_empty() && t.package_power_w.value.is_none() {
            SourceStatus::Unavailable(ioreport_reason.clone())
        } else if t.clusters.is_empty() {
            SourceStatus::Partial(format!("no residency: {ioreport_reason}"))
        } else {
            SourceStatus::Available
        },
    );

    // GPU patch.
    let g = &mut x.gpu;
    let gpu_cluster = t.clusters.iter().find(|c| c.name == "gpu").cloned();
    let ioaccel_reason = reason(h, keys::PART_IOACCEL).unwrap_or_else(|| "not collected".into());
    g.name = h
        .sysctl_str
        .get(keys::IOACCEL_MODEL)
        .map(|m| match int(h, keys::IOACCEL_CORES) {
            Some(n) if n > 0 => format!("{m} GPU ({n}-core)"),
            _ => format!("{m} GPU"),
        });
    let stat = |k: &str| int(h, &format!("{}{k}", keys::IOACCEL_PREFIX));
    g.util_pct = match (stat("Device Utilization %"), &gpu_cluster) {
        (Some(v), _) => Measured::exact(
            v.clamp(0, 100) as f64,
            format!("{SRC_IOACCEL}.Device Utilization %"),
        ),
        (None, Some(c)) => Measured::estimate(c.active_pct, "IOReport GPUPH active residency"),
        (None, None) => Measured::unavailable(SRC_IOACCEL, ioaccel_reason.clone()),
    };
    g.mem_used = match stat("In use system memory") {
        Some(v) => Measured::exact(v.max(0) as u64, format!("{SRC_IOACCEL}.In use system memory")),
        None => Measured::unavailable(SRC_IOACCEL, ioaccel_reason.clone()),
    };
    g.power_w = match energy_w(h, &["GPU Energy"]) {
        Some(w) => Measured::estimate(w, "IOReport Energy Model: GPU Energy"),
        None => Measured::unavailable(SRC_IOREPORT, ioreport_reason.clone()),
    };
    match &gpu_cluster {
        Some(c) if c.active_pct > 0.0 => {
            g.clock_mhz = Measured::estimate(c.cur_mhz, "IOReport GPUPH residency × pmgr voltage-states9");
            g.max_clock_mhz = Measured::exact(c.max_mhz, "pmgr voltage-states9");
        }
        Some(c) => {
            g.clock_mhz = Measured::unavailable(SRC_IOREPORT, "idle");
            g.max_clock_mhz = Measured::exact(c.max_mhz, "pmgr voltage-states9");
        }
        None => {
            g.clock_mhz = Measured::unavailable(SRC_IOREPORT, ioreport_reason.clone());
            g.max_clock_mhz = Measured::unavailable(SRC_IOREPORT, ioreport_reason);
        }
    }
    if let Some(p) = t.pressure.value.filter(|p| *p > ThermalPressure::Nominal) {
        g.throttle_reasons
            .push(format!("thermal pressure {}", pressure_name(p)));
    }
    if t.low_power_mode.value == Some(true) {
        g.throttle_reasons.push("Low Power Mode".into());
    }
    x.status.insert(
        status_keys::IOACCEL.into(),
        part_status(h, keys::PART_IOACCEL, g.mem_used.value.is_some()),
    );

    // Jetsam.
    let records = jetsam_records(h);
    for (file, r) in &records {
        // Outside the window, or implausibly in the future (corrupt header / clock jump): skipped.
        if r.at_ms.saturating_add(RECENT_KILLS_WINDOW_MS) < now_ms
            || r.at_ms > now_ms.saturating_add(3_600_000)
        {
            continue;
        }
        if r.killed.is_empty() {
            x.recent_kills.push(OomKill {
                at_ms: Some(r.at_ms),
                killer: OomKiller::Jetsam,
                victim_name: None,
                victim_pid: None,
                source: file.clone(),
            });
        }
        for v in &r.killed {
            x.recent_kills.push(OomKill {
                at_ms: Some(r.at_ms),
                killer: OomKiller::Jetsam,
                victim_name: Some(v.name.clone()),
                victim_pid: v.pid,
                source: if v.reason.is_empty() {
                    file.clone()
                } else {
                    format!("{file} ({})", v.reason)
                },
            });
        }
    }
    x.status.insert(
        status_keys::JETSAM.into(),
        match reason(h, keys::PART_JETSAM) {
            Some(r) if records.is_empty() => SourceStatus::Unavailable(r),
            Some(r) => SourceStatus::Partial(r),
            None => SourceStatus::Available,
        },
    );

    x.fanless = h.sysctl_str.get("hw.model").and_then(|m| fanless_model(m));
    x
}

fn pressure_name(p: ThermalPressure) -> &'static str {
    match p {
        ThermalPressure::Nominal => "nominal",
        ThermalPressure::Moderate => "moderate",
        ThermalPressure::Heavy => "heavy",
        ThermalPressure::Trapping => "trapping",
        ThermalPressure::Sleeping => "sleeping",
    }
}

/// Merges the extras into the `PartialSnapshot` produced by `decode::macos::decode_host`.
pub fn apply_extras(part: &mut PartialSnapshot, x: Extras) {
    if let Some(host) = part.host.as_mut() {
        host.fanless = x.fanless.or(host.fanless);
    }
    if let Some(accs) = part.accelerators.as_mut() {
        if let Some(a) = accs.iter_mut().find(|a| a.vendor == GpuVendor::Apple) {
            let g = x.gpu;
            if let Some(n) = g.name {
                a.name = n;
            }
            a.util_pct = g.util_pct;
            a.mem_used = g.mem_used;
            a.mem_total = Measured::unavailable("unified memory", "shares host RAM; see gpu_budget");
            a.power_w = g.power_w;
            a.clock_mhz = g.clock_mhz;
            a.max_clock_mhz = g.max_clock_mhz;
            a.temp_c = Measured::unavailable("SMC", "not collected");
            a.throttle_reasons = g.throttle_reasons;
        }
    }
    let oom = part.oom.get_or_insert_with(Default::default);
    oom.recent_kills = x.recent_kills;
    part.thermal = Some(x.thermal);
    part.status.extend(x.status);
}

/// `decode::macos::decode_host` + the extras: the drop-in decoder for `RawPayload::MacHost`.
pub fn decode_host_full(
    source: &str,
    h: &MacHostRaw,
    prev: Option<&MacHostRaw>,
    dt_ms: Option<u64>,
    taken_at_ms: u64,
) -> PartialSnapshot {
    let mut part = crate::decode::macos::decode_host(source, h, prev, dt_ms);
    apply_extras(&mut part, decode_extras(h, taken_at_ms));
    part
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> MacHostRaw {
        let mut h = MacHostRaw::default();
        h.sysctl_str.insert("hw.model".into(), "Mac17,3".into());
        h.sysctl_str
            .insert("machdep.cpu.brand_string".into(), "Apple M5".into());
        h.sysctl.insert("hw.memsize".into(), 24 << 30);
        h
    }

    #[test]
    fn residency_roundtrip_and_freq() {
        let st = vec![
            ("IDLE".to_string(), 50),
            ("V0P1".to_string(), 25),
            ("V1P0".to_string(), 25),
        ];
        assert_eq!(parse_residency(&encode_residency(&st)), st);
        let c = cluster_freq("E-cluster", &st, &[1000, 3000]).unwrap();
        assert_eq!(c.active_pct, 50.0);
        assert_eq!(c.cur_mhz, 2000.0);
        assert_eq!(c.max_mhz, 3000.0);
        // GPU table with a leading 0 ("off") entry and more states than frequencies.
        let g = vec![
            ("OFF".to_string(), 0),
            ("P1".to_string(), 10),
            ("P2".to_string(), 0),
            ("P3".to_string(), 10),
        ];
        let c = cluster_freq("gpu", &g, &[0, 400, 800]).unwrap();
        assert_eq!(c.active_pct, 100.0);
        assert_eq!(c.cur_mhz, 600.0); // P3 beyond the table counts as max (800)
        assert!(cluster_freq("x", &[], &[1]).is_none());
        assert!(cluster_freq("x", &st, &[]).is_none());
    }

    #[test]
    fn labels_and_tables() {
        assert_eq!(cluster_label("cpu.ECPU"), "E-cluster");
        assert_eq!(cluster_label("cpu.PCPU1"), "P-cluster 1");
        assert_eq!(cluster_label("gpu.GPUPH"), "gpu");
        assert_eq!(dvfs_table_for("cpu.ECPM"), None);
        assert_eq!(dvfs_table_for("cpu.PCPU"), Some("voltage-states5-sram"));
        assert_eq!(fanless_model("Mac17,3"), Some(true));
        assert_eq!(fanless_model("MacBookPro18,3"), Some(false));
        assert_eq!(fanless_model("Mac99,1"), None);
    }

    #[test]
    fn nothing_collected_is_unavailable_not_zero() {
        let x = decode_extras(&host(), 0);
        assert!(x.thermal.pressure.value.is_none());
        assert!(x.thermal.low_power_mode.value.is_none());
        assert!(x.thermal.battery_pct.value.is_none());
        assert!(x.thermal.throttle_factor.value.is_none());
        assert!(x.thermal.package_power_w.value.is_none());
        assert!(x.gpu.mem_used.value.is_none());
        assert!(x.recent_kills.is_empty());
        assert_eq!(x.fanless, Some(true));
        assert!(matches!(
            x.status[status_keys::THERMAL],
            SourceStatus::Unavailable(_)
        ));
    }

    #[test]
    fn full_decode() {
        let mut h = host();
        for (k, v) in [
            (keys::THERMAL_NOTIFY, 2),
            (keys::THERMAL_STATE, 2),
            (keys::LOW_POWER_MODE, 1),
            (keys::PS_ON_AC, 0),
            (keys::BATT_PRESENT, 1),
            (keys::BATT_CURRENT, 29),
            (keys::BATT_MAX, 100),
            (keys::IOREPORT_WINDOW_US, 1_000_000),
        ] {
            h.sysctl.insert(k.into(), v);
        }
        h.sysctl
            .insert("ioreport.energy_nj.CPU Energy".into(), 3_000_000_000);
        h.sysctl
            .insert("ioreport.energy_nj.GPU Energy".into(), 1_500_000_000);
        h.sysctl.insert("ioreport.energy_nj.ANE".into(), 500_000_000);
        h.sysctl.insert("ioaccel.In use system memory".into(), 10 << 30);
        h.sysctl.insert("ioaccel.Device Utilization %".into(), 97);
        h.sysctl.insert(keys::IOACCEL_CORES.into(), 10);
        h.sysctl_str.insert(keys::IOACCEL_MODEL.into(), "Apple M5".into());
        h.sysctl_str
            .insert("dvfs.voltage-states5-sram".into(), "1000,2000,4000".into());
        h.sysctl_str
            .insert("dvfs.voltage-states9".into(), "0,500,1000".into());
        // P-cluster fully busy at 2000 of 4000 MHz → throttled to 50 %.
        h.sysctl_str.insert(
            "ioreport.residency.cpu.PCPU".into(),
            "IDLE:0,V0:0,V1:100,V2:0".into(),
        );
        h.sysctl_str
            .insert("ioreport.residency.gpu.GPUPH".into(), "OFF:0,P1:100,P2:0".into());
        let rec = JetsamRecord {
            at_ms: 1_000,
            largest_process: Some("sd-server".into()),
            killed: vec![JetsamVictim {
                name: "spotlightknowledged".into(),
                pid: Some(73341),
                reason: "per-process-limit".into(),
            }],
        };
        h.sysctl_str.insert(
            "jetsam.JetsamEvent-2026-09-29-171321.ips".into(),
            serde_json::to_string(&rec).unwrap(),
        );
        let x = decode_extras(&h, 2_000);
        let t = &x.thermal;
        assert_eq!(t.pressure.value, Some(ThermalPressure::Heavy));
        assert_eq!(t.low_power_mode.value, Some(true));
        assert_eq!(t.on_battery.value, Some(true));
        assert_eq!(t.battery_pct.value, Some(29.0));
        assert!(t.adapter_watts.value.is_none());
        assert!((t.package_power_w.value.unwrap() - 5.0).abs() < 1e-9);
        assert_eq!(t.clusters.len(), 2);
        assert_eq!(t.clusters[0].name, "P-cluster");
        assert_eq!(t.clusters[1].name, "gpu");
        let f = t.throttle_factor.value.unwrap();
        assert!(f < 0.8, "throttle factor {f}");
        assert_eq!(x.gpu.name.as_deref(), Some("Apple M5 GPU (10-core)"));
        assert_eq!(x.gpu.mem_used.value, Some(10 << 30));
        assert_eq!(x.gpu.util_pct.value, Some(97.0));
        assert!((x.gpu.power_w.value.unwrap() - 1.5).abs() < 1e-9);
        assert_eq!(x.gpu.clock_mhz.value, Some(500.0));
        assert_eq!(x.gpu.max_clock_mhz.value, Some(1000.0));
        assert_eq!(
            x.gpu.throttle_reasons,
            vec!["thermal pressure heavy".to_string(), "Low Power Mode".to_string()]
        );
        assert_eq!(x.recent_kills.len(), 1);
        assert_eq!(x.recent_kills[0].victim_pid, Some(73341));
        assert_eq!(x.recent_kills[0].killer, OomKiller::Jetsam);
        // Outside the 24 h window → dropped.
        assert!(decode_extras(&h, 1_000 + RECENT_KILLS_WINDOW_MS + 1)
            .recent_kills
            .is_empty());

        // Merged into the host partial.
        let part = decode_host_full("macos.host", &h, None, None, 2_000);
        let acc = &part.accelerators.as_ref().unwrap()[0];
        assert_eq!(acc.mem_used.value, Some(10 << 30));
        assert_eq!(acc.name, "Apple M5 GPU (10-core)");
        assert_eq!(part.host.as_ref().unwrap().fanless, Some(true));
        assert_eq!(part.oom.as_ref().unwrap().recent_kills.len(), 1);
        assert_eq!(
            part.thermal.as_ref().unwrap().pressure.value,
            Some(ThermalPressure::Heavy)
        );
        assert_eq!(
            part.status.get(status_keys::IOREPORT),
            Some(&SourceStatus::Available)
        );
    }

    #[test]
    fn idle_clusters_have_no_throttle_factor() {
        let mut h = host();
        h.sysctl.insert(keys::IOREPORT_WINDOW_US.into(), 1_000_000);
        h.sysctl_str
            .insert("dvfs.voltage-states1-sram".into(), "1000,2000".into());
        h.sysctl_str
            .insert("ioreport.residency.cpu.ECPU".into(), "IDLE:95,V0:5,V1:0".into());
        let x = decode_extras(&h, 0);
        assert_eq!(x.thermal.clusters.len(), 1);
        assert!(x.thermal.throttle_factor.value.is_none());
    }

    /// Hostile raw values (corrupt fixture, future OS format) decode without overflow panics.
    #[test]
    fn extreme_values_never_panic() {
        let mut h = host();
        h.sysctl.insert(keys::IOREPORT_WINDOW_US.into(), 1);
        for c in ["CPU Energy", "GPU Energy", "ANE"] {
            h.sysctl
                .insert(format!("{}{c}", keys::IOREPORT_ENERGY_PREFIX), i64::MAX);
        }
        h.sysctl.insert("ioaccel.In use system memory".into(), i64::MIN);
        h.sysctl.insert("ioaccel.Device Utilization %".into(), i64::MAX);
        h.sysctl.insert(keys::BATT_PRESENT.into(), 1);
        h.sysctl.insert(keys::BATT_CURRENT.into(), i64::MAX);
        h.sysctl.insert(keys::BATT_MAX.into(), 1);
        h.sysctl.insert(keys::THERMAL_NOTIFY.into(), 99);
        h.sysctl_str
            .insert("dvfs.voltage-states5-sram".into(), "4000,x,,99999999999".into());
        h.sysctl_str.insert(
            "ioreport.residency.cpu.PCPU".into(),
            format!("IDLE:{m},V0:{m},V1:{m},bad,:,V2:-5", m = i64::MAX),
        );
        let rec = JetsamRecord {
            at_ms: u64::MAX,
            ..Default::default()
        };
        h.sysctl_str.insert(
            "jetsam.JetsamEvent-x.ips".into(),
            serde_json::to_string(&rec).unwrap(),
        );
        h.sysctl_str
            .insert("jetsam.JetsamEvent-bad.ips".into(), "{not json".into());
        let x = decode_extras(&h, u64::MAX);
        assert!(x.thermal.package_power_w.value.unwrap().is_finite());
        assert_eq!(x.gpu.mem_used.value, Some(0));
        assert_eq!(x.gpu.util_pct.value, Some(100.0));
        assert_eq!(x.thermal.battery_pct.value, Some(100.0));
        // Unknown notify level → not a made-up pressure.
        assert!(x.thermal.pressure.value.is_none());
        let c = &x.thermal.clusters[0];
        assert!(
            c.cur_mhz.is_finite() && (0.0..=100.0).contains(&c.active_pct),
            "{c:?}"
        );
        assert!(c.cur_mhz <= c.max_mhz, "{c:?}");
        // A record "from the future" (u64::MAX) is not a recent kill.
        let x = decode_extras(&h, 5_000);
        assert!(x.recent_kills.is_empty());
    }
}

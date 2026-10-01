//! Pure Linux enrichment on top of [`crate::decode::linux::decode_files`] (SPEC §5, §8, §9):
//! - host: cgroup v2 limit + `available_now` cap, OOM killers/thresholds/likely victim/recent kills,
//!   thermal (cpufreq clusters, thermal zones + trip points, hwmon temps, RAPL package power, battery,
//!   platform_profile), accelerators (NVML, amdgpu sysfs + `gpu_metrics`, Intel i915/xe), sub-source status;
//! - processes: GPU memory (NVML per-process, DRM fdinfo), disk I/O + rates, smaps estimates between
//!   rotated reads, mapped/open model files, user names.
//!
//! [`enrich`] is idempotent, so it can be called both from `decode::linux::decode_files` and from
//! [`decode_full`]. No I/O; runs on any OS.

use super::gpu::{
    nvml_throttle_labels, parse_cards, parse_drm_fdinfo, parse_gpu_metrics, parse_nvml_procs,
    parse_nvml_text, DrmClient, GpuMetrics, NvmlDevice,
};
use super::host::{parse_daemons, parse_kills, parse_scan};
use super::oom::{
    earlyoom_thresholds, nearest_killer, oomd_thresholds, resolve_earlyoom, resolve_oomd, EARLYOOM_COMM,
    EARLYOOM_DEFAULTS, OOMD_COMM,
};
use super::parse::{
    cgroup_ancestors, cgroup_v2_path, from_hex, milli_c, parse_cgroup_limit, parse_cpu_list, parse_env_lines,
    parse_pp_dpm, parse_proc_io, parse_u64, per_cpu_busy_pct, Trip,
};
use super::smaps::{estimate_pss, FULL_CYCLE_MS};
use super::{keys, natural_key};
use crate::decode::linux::{parse_kv_kb, start_time_ms};
use crate::decode::{decode, PartialSnapshot};
use crate::raw::{LinuxFilesRaw, RawSample};
use crate::replay::Fixture;
use oomtop_core::{
    Accelerator, ClusterFreq, DiskIo, GpuVendor, HostMemory, Measured, Oom, OomKill, OomKiller, ProcId,
    Snapshot, SourceStatus, TempSensor, Thermal, ThermalPressure, Victim,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Cached smaps values older than this are not used for the estimate (RSS − Shared is used instead).
pub const SMAPS_CACHE_MAX_AGE_MS: u64 = 2 * FULL_CYCLE_MS;
/// Temperatures within this many °C below a passive trip / hwmon crit count as "moderate".
pub const MODERATE_MARGIN_C: f64 = 5.0;
const MAX_TEMPS: usize = 48;

fn status_of(f: &LinuxFilesRaw, name: &str) -> Option<SourceStatus> {
    let t = f.files.get(&keys::status(name))?;
    let t = t.trim();
    Some(if let Some(r) = t.strip_prefix("unavailable:") {
        SourceStatus::Unavailable(r.trim().to_string())
    } else if let Some(r) = t.strip_prefix("partial:") {
        SourceStatus::Partial(r.trim().to_string())
    } else {
        SourceStatus::Available
    })
}

fn get<'a>(f: &'a LinuxFilesRaw, k: &str) -> Option<&'a str> {
    f.files.get(k).map(String::as_str)
}

fn btime_s(f: &LinuxFilesRaw) -> Option<u64> {
    get(f, "stat")?
        .lines()
        .find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok()))
}

/// Keys under `prefix` whose remainder is `<dir>/<leaf>`: returns the distinct `<dir>` names.
fn dirs_with(f: &LinuxFilesRaw, prefix: &str, leaf: &str) -> Vec<String> {
    let mut out: Vec<String> = f
        .files
        .range(prefix.to_string()..)
        .take_while(|(k, _)| k.starts_with(prefix))
        .filter_map(|(k, _)| {
            let rest = &k[prefix.len()..];
            let (dir, l) = rest.split_once('/')?;
            (l == leaf).then(|| dir.to_string())
        })
        .collect();
    out.dedup();
    out.sort_by_key(|d| natural_key(d));
    out
}

/// Enriches a decoded Linux partial with everything `decode_files` does not interpret. Idempotent.
pub fn enrich(
    source: &str,
    f: &LinuxFilesRaw,
    prev: Option<&LinuxFilesRaw>,
    dt_ms: Option<u64>,
    part: &mut PartialSnapshot,
) {
    let _ = source; // part of the hook signature; statuses are keyed by sub-source
    let dt_ms = dt_ms.filter(|d| *d > 0);
    if f.files.contains_key("meminfo") {
        enrich_host(f, prev, dt_ms, part);
    }
    if part.processes.is_some() {
        enrich_procs(f, prev, dt_ms, part);
    }
}

/// `decode()` + [`enrich`] for Linux samples. [`crate::decode::decode`] now runs [`enrich`] itself; this is
/// kept as an alias for existing callers.
pub fn decode_full(raw: &RawSample, prev: Option<&RawSample>) -> PartialSnapshot {
    decode(raw, prev)
}

/// [`crate::replay::replay`] with [`decode_full`].
pub fn replay_full(fixture: &Fixture) -> Vec<Snapshot> {
    let mut prev: HashMap<String, RawSample> = HashMap::new();
    let mut current = Snapshot::default();
    let mut out = Vec::with_capacity(fixture.frames.len());
    for frame in &fixture.frames {
        let mut t = current.taken_at_ms;
        for raw in frame {
            decode_full(raw, prev.get(&raw.source)).apply(&mut current);
            t = t.max(raw.taken_at_ms);
            prev.insert(raw.source.clone(), raw.clone());
        }
        current.taken_at_ms = t;
        out.push(current.clone());
    }
    out
}

// ---------------------------------------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------------------------------------

fn enrich_host(
    f: &LinuxFilesRaw,
    prev: Option<&LinuxFilesRaw>,
    dt_ms: Option<u64>,
    part: &mut PartialSnapshot,
) {
    let mut status: BTreeMap<String, SourceStatus> = BTreeMap::new();
    if let Some(mem) = part.memory.as_mut() {
        status.insert("linux.cgroup".into(), apply_cgroup(f, mem));
    }
    status.insert(
        "linux.psi".into(),
        if f.files.contains_key("pressure/memory") {
            SourceStatus::Available
        } else {
            SourceStatus::Unavailable(match f.missing.get("pressure/memory") {
                Some(r) => format!("/proc/pressure/memory: {r} (kernel < 4.20 or psi=0)"),
                None => "PSI not recorded".into(),
            })
        },
    );
    let (oom, oom_status) = decode_oom(f, part.memory.as_ref());
    part.oom = Some(oom);
    status.insert("linux.oom".into(), oom_status);

    let (accels, gpu_status) = decode_accelerators(f);
    status.extend(gpu_status);
    let (thermal, th_status) = decode_thermal(f, prev, dt_ms, &accels);
    status.extend(th_status);
    part.thermal = Some(thermal);
    part.accelerators = Some(accels);
    part.status.extend(status);
}

/// Applies cgroup v2 limits: `own_cgroup_limit` = tightest ancestor `memory.max`; `available` capped by
/// the smallest `memory.max − memory.current` (SPEC §8.1).
fn apply_cgroup(f: &LinuxFilesRaw, mem: &mut HostMemory) -> SourceStatus {
    let Some(path) = get(f, "self/cgroup").and_then(cgroup_v2_path) else {
        mem.own_cgroup_limit =
            Measured::unavailable("/proc/self/cgroup", "no cgroup v2 membership (cgroup v1?)");
        return SourceStatus::Unavailable("no cgroup v2 membership (cgroup v1?)".into());
    };
    let mounted = f.files.contains_key("/sys/fs/cgroup/cgroup.controllers");
    let mut limit: Option<(u64, String)> = None;
    let mut room: Option<(u64, String)> = None;
    for a in cgroup_ancestors(&path) {
        let base = format!("/sys/fs/cgroup{a}");
        let Some(Some(max)) = get(f, &format!("{base}/memory.max")).and_then(parse_cgroup_limit) else {
            continue;
        };
        let label = if a.is_empty() { "/".to_string() } else { a.clone() };
        if limit.as_ref().is_none_or(|(l, _)| max < *l) {
            limit = Some((max, label.clone()));
        }
        if let Some(cur) = get(f, &format!("{base}/memory.current")).and_then(parse_u64) {
            let r = max.saturating_sub(cur);
            if room.as_ref().is_none_or(|(x, _)| r < *x) {
                room = Some((r, label));
            }
        }
    }
    mem.own_cgroup_limit = match &limit {
        Some((l, cg)) => Measured::exact(*l, format!("cgroup memory.max ({cg})")),
        None => Measured::unavailable(
            "cgroup memory.max",
            format!("no memory.max limit on {path} or its ancestors"),
        ),
    };
    if let (Some((r, cg)), Some(avail)) = (&room, mem.available.value) {
        if *r < avail {
            mem.available = Measured::estimate(
                *r,
                format!("min(MemAvailable, memory.max − memory.current of {cg})"),
            );
        }
    }
    if mounted || limit.is_some() {
        SourceStatus::Available
    } else {
        SourceStatus::Partial("cgroup v2 hierarchy not readable at /sys/fs/cgroup".into())
    }
}

fn decode_oom(f: &LinuxFilesRaw, mem: Option<&HostMemory>) -> (Oom, SourceStatus) {
    let daemons = get(f, keys::OOM_DAEMONS).map(parse_daemons).unwrap_or_default();
    let mut killers = vec![OomKiller::Kernel];
    let mut thresholds = Vec::new();
    let mut status = SourceStatus::Available;
    if daemons.contains_key(OOMD_COMM) {
        killers.push(OomKiller::SystemdOomd);
        let c = resolve_oomd(&f.files);
        if c.sources.is_empty() && !c.units_found {
            status = SourceStatus::Partial("systemd-oomd config not readable; assuming its defaults".into());
        }
        thresholds.extend(oomd_thresholds(&c));
    }
    if let Some(pid) = daemons.get(EARLYOOM_COMM) {
        killers.push(OomKiller::Earlyoom);
        let c = resolve_earlyoom(get(f, &format!("{pid}/cmdline")), get(f, EARLYOOM_DEFAULTS));
        thresholds.extend(earlyoom_thresholds(
            &c,
            mem.and_then(|m| m.total.value),
            mem.and_then(|m| m.swap_total.value),
        ));
    }
    killers.sort();
    let killer = match mem {
        Some(m) => nearest_killer(&killers, &thresholds, m),
        None => OomKiller::Kernel,
    };
    let btime = btime_s(f);
    let likely_victim = get(f, keys::OOM_SCAN)
        .map(parse_scan)
        .and_then(|v| {
            v.into_iter()
                .max_by_key(|e| (e.oom_score, std::cmp::Reverse(e.pid)))
        })
        .filter(|e| e.oom_score > 0)
        .map(|e| Victim {
            id: ProcId::new(
                e.pid,
                start_time_ms(btime.unwrap_or(0), e.starttime, f.clk_tck.max(1)),
            ),
            name: e.comm.clone(),
            group_id: None,
            reason: format!("highest oom_score ({})", e.oom_score),
            heuristic: false,
        });
    let recent_kills = get(f, keys::OOM_KILLS)
        .map(parse_kills)
        .unwrap_or_default()
        .into_iter()
        .map(|k| OomKill {
            at_ms: Some(k.at_ms),
            killer: OomKiller::Kernel,
            victim_name: k.name,
            victim_pid: k.pid,
            source: k.source,
        })
        .collect();
    (
        Oom {
            killer,
            killers,
            thresholds,
            forecast: None,
            likely_victim,
            recent_kills,
        },
        status,
    )
}

// ---------------------------------------------------------------------------------------------------------
// Accelerators
// ---------------------------------------------------------------------------------------------------------

/// `"0000:03:00.0"` / `"00000000:03:00.0"` → `"03:00.0"` (NVML pads the domain to 8 digits).
fn pci_short(bus_id: &str) -> String {
    let b = bus_id.trim().to_ascii_lowercase();
    match b.split_once(':') {
        Some((dom, rest)) if dom.len() >= 4 && rest.contains(':') => rest.to_string(),
        _ => b,
    }
}

fn nvml_accel(d: &NvmlDevice) -> Accelerator {
    let src = "nvml";
    let m64 = |v: Option<u64>, what: &str| match v {
        Some(v) => Measured::exact(v, format!("{src}:{what}")),
        None => Measured::unavailable(
            src,
            d.errors
                .get(what)
                .cloned()
                .unwrap_or_else(|| "not reported".into()),
        ),
    };
    let mf = |v: Option<f64>, what: &str| match v {
        Some(v) => Measured::exact(v, format!("{src}:{what}")),
        None => Measured::unavailable(
            src,
            d.errors
                .get(what)
                .cloned()
                .unwrap_or_else(|| "not reported".into()),
        ),
    };
    let mut throttle_reasons = d.throttle_reasons.map(nvml_throttle_labels).unwrap_or_default();
    if let (Some(p), Some(l)) = (d.power_mw, d.power_limit_mw) {
        if l > 0 && p as f64 >= l as f64 * 0.98 && !throttle_reasons.iter().any(|r| r.contains("power")) {
            throttle_reasons.push("at power limit".into());
        }
    }
    Accelerator {
        id: String::new(),
        vendor: GpuVendor::Nvidia,
        name: if d.name.is_empty() {
            format!("NVIDIA GPU {}", d.index)
        } else {
            d.name.clone()
        },
        unified: false,
        util_pct: mf(d.util_gpu.map(f64::from), "util_gpu"),
        mem_used: m64(d.mem_used, "mem_used"),
        mem_total: m64(d.mem_total, "mem_total"),
        gpu_budget: match d.mem_total {
            Some(t) => Measured::exact(t, "nvml:mem_total (discrete VRAM)"),
            None => Measured::unavailable(src, "VRAM size not reported"),
        },
        power_w: mf(d.power_mw.map(|p| f64::from(p) / 1000.0), "power_mw"),
        temp_c: mf(d.temp_c.map(f64::from), "temp_c"),
        clock_mhz: mf(d.clock_sm_mhz.map(f64::from), "clock_sm_mhz"),
        max_clock_mhz: mf(d.max_clock_sm_mhz.map(f64::from), "max_clock_sm_mhz"),
        throttle_reasons,
    }
}

/// hwmon chip of a DRM card (`/sys/class/drm/cardN/device/hwmon/` listing).
fn card_hwmon(f: &LinuxFilesRaw, card: &str) -> Option<String> {
    let listing = get(f, &format!("/sys/class/drm/{card}/device/hwmon/"))?;
    let chip = listing.lines().find(|l| l.starts_with("hwmon"))?;
    Some(format!("/sys/class/hwmon/{chip}"))
}

/// First temperature of a hwmon chip, preferring a label (`edge`), plus its crit.
fn hwmon_temp(f: &LinuxFilesRaw, chip: &str, prefer: &str) -> Option<(f64, Option<f64>, String)> {
    let mut first = None;
    for n in 1..=16 {
        let Some(t) = get(f, &format!("{chip}/temp{n}_input")).and_then(milli_c) else {
            continue;
        };
        let crit = get(f, &format!("{chip}/temp{n}_crit")).and_then(milli_c);
        let label = get(f, &format!("{chip}/temp{n}_label"))
            .map(|l| l.trim().to_string())
            .unwrap_or_else(|| format!("temp{n}"));
        let src = format!("{chip}/temp{n}_input");
        if label == prefer {
            return Some((t, crit, src));
        }
        first.get_or_insert((t, crit, src));
    }
    first
}

fn hwmon_power_w(f: &LinuxFilesRaw, chip: &str) -> Option<(f64, String)> {
    for leaf in ["power1_average", "power1_input"] {
        let k = format!("{chip}/{leaf}");
        if let Some(uw) = get(f, &k).and_then(parse_u64) {
            return Some((uw as f64 / 1e6, k));
        }
    }
    None
}

fn una<T>((src, why): (&str, String)) -> Measured<T> {
    Measured::unavailable(src, why)
}

fn drm_accel(f: &LinuxFilesRaw, card: &str, nvml_reason: Option<&str>) -> (Accelerator, Option<String>) {
    let base = format!("/sys/class/drm/{card}");
    let dev = |leaf: &str| get(f, &format!("{base}/device/{leaf}")).map(str::trim);
    let vendor_id = dev("vendor").and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok());
    let vendor = match vendor_id {
        Some(0x1002) => GpuVendor::Amd,
        Some(0x10de) => GpuVendor::Nvidia,
        Some(0x8086) => GpuVendor::Intel,
        _ => GpuVendor::Other,
    };
    let uevent = dev("uevent").map(parse_env_lines).unwrap_or_default();
    let driver = uevent.get("DRIVER").cloned().unwrap_or_default();
    let src = format!("{base} ({driver})");
    let na = |why: &str| (src.as_str(), why.to_string());
    let name = dev("product_name")
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            let v = match vendor {
                GpuVendor::Amd => "AMD",
                GpuVendor::Nvidia => "NVIDIA",
                GpuVendor::Intel => "Intel",
                _ => "GPU",
            };
            match dev("device") {
                Some(d) => format!("{v} GPU {d} ({driver})"),
                None => format!("{v} GPU ({driver})"),
            }
        });
    let mut a = Accelerator {
        vendor,
        name,
        util_pct: una(na("not exposed by this driver")),
        mem_used: una(na("not exposed by this driver")),
        mem_total: una(na("not exposed by this driver")),
        gpu_budget: una(na("not exposed by this driver")),
        power_w: una(na("no hwmon power sensor")),
        temp_c: una(na("no hwmon temperature")),
        clock_mhz: una(na("no clock readout")),
        max_clock_mhz: una(na("no clock readout")),
        ..Default::default()
    };
    let chip = card_hwmon(f, card);
    let mut crit = None;
    if let Some(chip) = &chip {
        if let Some((t, c, src)) = hwmon_temp(f, chip, "edge") {
            a.temp_c = Measured::exact(t, src);
            crit = c;
        }
        if let Some((w, src)) = hwmon_power_w(f, chip) {
            a.power_w = Measured::exact(w, src);
        }
    }
    match vendor {
        GpuVendor::Amd => {
            let metrics: Option<Result<GpuMetrics, String>> = get(f, &format!("{base}/device/gpu_metrics"))
                .and_then(from_hex)
                .map(|b| parse_gpu_metrics(&b));
            let m = metrics.as_ref().and_then(|r| r.as_ref().ok());
            if let Some(b) = dev("gpu_busy_percent").and_then(parse_u64) {
                a.util_pct =
                    Measured::exact((b as f64).min(100.0), format!("{base}/device/gpu_busy_percent"));
            } else if let Some(g) = m.and_then(|m| m.gfx_activity_pct) {
                a.util_pct = Measured::exact(g, "amdgpu gpu_metrics:average_gfx_activity");
            }
            let vram_total = dev("mem_info_vram_total").and_then(parse_u64);
            let gtt_total = dev("mem_info_gtt_total").and_then(parse_u64);
            let vram_used = dev("mem_info_vram_used").and_then(parse_u64);
            let gtt_used = dev("mem_info_gtt_used").and_then(parse_u64);
            if let Some(v) = vram_used {
                a.mem_used = Measured::exact(v, format!("{base}/device/mem_info_vram_used"));
            }
            if let Some(v) = vram_total {
                a.mem_total = Measured::exact(v, format!("{base}/device/mem_info_vram_total"));
            }
            // APU: gpu_metrics format 2/3, else a small BIOS carve-out.
            a.unified = m.map(|m| m.apu).unwrap_or(false) || vram_total.is_some_and(|v| v <= 2 << 30);
            // On an APU most GPU allocations live in GTT (host RAM): used and total cover both, matching
            // the budget below (budget − used = GPU headroom).
            if a.unified {
                if let (Some(v), Some(g)) = (vram_used, gtt_used) {
                    a.mem_used = Measured::exact(v + g, "APU: mem_info_vram_used + mem_info_gtt_used");
                }
                if let (Some(v), Some(g)) = (vram_total, gtt_total) {
                    a.mem_total = Measured::exact(v + g, "APU: mem_info_vram_total + mem_info_gtt_total");
                }
            }
            a.gpu_budget = match (a.unified, vram_total, gtt_total) {
                (true, Some(v), Some(g)) => Measured::estimate(v + g, "APU: VRAM carve-out + GTT (host RAM)"),
                (false, Some(v), _) => Measured::exact(v, "discrete VRAM (mem_info_vram_total)"),
                _ => una(na("VRAM size not exposed")),
            };
            let (cur, max) = dev("pp_dpm_sclk").map(parse_pp_dpm).unwrap_or((None, None));
            if let Some(c) = cur {
                a.clock_mhz = Measured::exact(c, format!("{base}/device/pp_dpm_sclk"));
            } else if let Some(c) = m.and_then(|m| m.cur_gfxclk_mhz) {
                a.clock_mhz = Measured::exact(c, "amdgpu gpu_metrics:current_gfxclk");
            } else if let Some(c) = m.and_then(|m| m.avg_gfxclk_mhz) {
                a.clock_mhz = Measured::exact(c, "amdgpu gpu_metrics:average_gfxclk_frequency");
            }
            if let Some(mx) = max {
                a.max_clock_mhz = Measured::exact(mx, format!("{base}/device/pp_dpm_sclk"));
            }
            if let Some(m) = m {
                if !a.temp_c.is_available() {
                    if let Some(t) = m.temp_edge_c {
                        a.temp_c = Measured::exact(t, "amdgpu gpu_metrics:temperature_edge");
                    }
                }
                if !a.power_w.is_available() {
                    if let Some(w) = m.socket_power_w {
                        a.power_w = Measured::exact(w, "amdgpu gpu_metrics:average_socket_power");
                    }
                }
                if let Some(s) = m.throttle_status.filter(|s| *s != 0) {
                    a.throttle_reasons.push(format!("SMU throttle status {s:#x}"));
                }
            }
            if let Some(Err(e)) = &metrics {
                return (a, Some(e.clone()));
            }
        }
        GpuVendor::Intel => {
            a.unified = true;
            a.util_pct = una(na("i915/xe busyness needs perf PMU access"));
            a.mem_used = una(na("shared host memory (see per-process drm fdinfo)"));
            a.mem_total = una(na("shared host memory"));
            a.gpu_budget = una(na("shared host memory; no fixed budget"));
            let cur = get(f, &format!("{base}/gt_act_freq_mhz"))
                .map(|v| (v, format!("{base}/gt_act_freq_mhz")))
                .or_else(|| {
                    get(f, &format!("{base}/device/tile0/gt0/freq0/act_freq"))
                        .map(|v| (v, format!("{base}/device/tile0/gt0/freq0/act_freq")))
                })
                .or_else(|| {
                    get(f, &format!("{base}/gt_cur_freq_mhz")).map(|v| (v, format!("{base}/gt_cur_freq_mhz")))
                });
            if let Some((v, src)) = cur.and_then(|(v, s)| Some((parse_u64(v)?, s))) {
                a.clock_mhz = Measured::exact(v as f64, src);
            }
            let max = get(f, &format!("{base}/gt_RP0_freq_mhz"))
                .map(|v| (v, "gt_RP0_freq_mhz"))
                .or_else(|| {
                    get(f, &format!("{base}/device/tile0/gt0/freq0/rp0_freq")).map(|v| (v, "rp0_freq"))
                })
                .or_else(|| get(f, &format!("{base}/gt_max_freq_mhz")).map(|v| (v, "gt_max_freq_mhz")));
            if let Some((v, src)) = max.and_then(|(v, s)| Some((parse_u64(v)?, s))) {
                a.max_clock_mhz = Measured::exact(v as f64, format!("{base}/{src}"));
            }
        }
        GpuVendor::Nvidia => {
            let why = format!("NVML unavailable: {}", nvml_reason.unwrap_or("no NVML data"));
            for m in [&mut a.util_pct, &mut a.clock_mhz, &mut a.max_clock_mhz] {
                *m = Measured::unavailable("nvml", why.clone());
            }
            for m in [&mut a.mem_used, &mut a.mem_total, &mut a.gpu_budget] {
                *m = Measured::unavailable("nvml", why.clone());
            }
        }
        _ => {}
    }
    if let (Some(t), Some(c)) = (a.temp_c.value, crit) {
        if t >= c {
            a.throttle_reasons.push("thermal (at hwmon crit)".into());
        }
    }
    (a, None)
}

/// Accelerators from NVML + DRM sysfs, sorted by PCI address, ids `gpu0…`.
fn decode_accelerators(f: &LinuxFilesRaw) -> (Vec<Accelerator>, BTreeMap<String, SourceStatus>) {
    let mut status = BTreeMap::new();
    let nvml = get(f, keys::NVML).map(parse_nvml_text);
    let nvml_status = status_of(f, "nvml");
    let nvml_reason = match &nvml_status {
        Some(SourceStatus::Unavailable(r)) | Some(SourceStatus::Partial(r)) => Some(r.clone()),
        _ => None,
    };
    let mut list: Vec<(String, Accelerator)> = Vec::new();
    let mut nvml_pci: BTreeSet<String> = BTreeSet::new();
    if let Some(h) = &nvml {
        for d in &h.devices {
            let pci = d.pci_bus_id.as_deref().map(pci_short);
            if let Some(p) = &pci {
                nvml_pci.insert(p.clone());
            }
            list.push((pci.unwrap_or_else(|| format!("~nvml{}", d.index)), nvml_accel(d)));
        }
        status.insert("linux.nvml".into(), SourceStatus::Available);
    } else if let Some(s) = nvml_status {
        status.insert("linux.nvml".into(), s);
    }
    let mut gpu_errors = Vec::new();
    for card in dirs_with(f, "/sys/class/drm/", "device/vendor") {
        let uevent = get(f, &format!("/sys/class/drm/{card}/device/uevent"))
            .map(parse_env_lines)
            .unwrap_or_default();
        let pci = uevent.get("PCI_SLOT_NAME").map(|p| pci_short(p));
        if pci.as_ref().is_some_and(|p| nvml_pci.contains(p)) {
            continue; // covered by NVML
        }
        let is_nvidia =
            get(f, &format!("/sys/class/drm/{card}/device/vendor")).map(str::trim) == Some("0x10de");
        if is_nvidia && nvml.is_some() && pci.is_none() {
            continue;
        }
        if list.iter().any(|(k, _)| Some(k) == pci.as_ref()) {
            continue;
        }
        let (a, err) = drm_accel(f, &card, nvml_reason.as_deref());
        if let Some(e) = err {
            gpu_errors.push(format!("{card}: {e}"));
        }
        list.push((pci.unwrap_or_else(|| format!("~{card}")), a));
    }
    list.sort_by(|a, b| a.0.cmp(&b.0));
    let accels: Vec<Accelerator> = list
        .into_iter()
        .enumerate()
        .map(|(i, (_, mut a))| {
            a.id = format!("gpu{i}");
            a
        })
        .collect();
    status.insert(
        "linux.gpu".into(),
        if accels.is_empty() {
            SourceStatus::Unavailable("no GPU in /sys/class/drm and no NVML device".into())
        } else if !gpu_errors.is_empty() {
            SourceStatus::Partial(gpu_errors.join("; "))
        } else {
            SourceStatus::Available
        },
    );
    (accels, status)
}

// ---------------------------------------------------------------------------------------------------------
// Thermal
// ---------------------------------------------------------------------------------------------------------

struct Policy {
    cur_mhz: f64,
    max_mhz: f64,
    cpus: Vec<u32>,
}

fn cpu_clusters(f: &LinuxFilesRaw, prev: Option<&LinuxFilesRaw>) -> Vec<ClusterFreq> {
    let base = "/sys/devices/system/cpu/cpufreq/";
    let busy = match (get(f, "stat"), prev.and_then(|p| get(p, "stat"))) {
        (Some(a), Some(b)) => per_cpu_busy_pct(a, b),
        _ => BTreeMap::new(),
    };
    let policies: Vec<Policy> = dirs_with(f, base, "scaling_cur_freq")
        .into_iter()
        .filter_map(|p| {
            let d = format!("{base}{p}");
            let cur = get(f, &format!("{d}/scaling_cur_freq")).and_then(parse_u64)? as f64 / 1000.0;
            let max = get(f, &format!("{d}/cpuinfo_max_freq")).and_then(parse_u64)? as f64 / 1000.0;
            let mut cpus = get(f, &format!("{d}/related_cpus"))
                .map(parse_cpu_list)
                .unwrap_or_default();
            if cpus.is_empty() {
                cpus = p
                    .strip_prefix("policy")
                    .and_then(|n| n.parse().ok())
                    .into_iter()
                    .collect();
            }
            (max > 0.0).then_some(Policy {
                cur_mhz: cur,
                max_mhz: max,
                cpus,
            })
        })
        .collect();
    // Aggregate policies with (nearly) the same max frequency: hybrid P/E cores stay apart, per-core
    // intel_pstate / amd-pstate policies merge. Preferred-core parts report per-core maxima a few MHz apart
    // (e.g. 5137 / 5100 / 5050), hence the 100 MHz buckets.
    let mut groups: BTreeMap<u64, Vec<&Policy>> = BTreeMap::new();
    for p in &policies {
        groups
            .entry((p.max_mhz / 100.0).round() as u64)
            .or_default()
            .push(p);
    }
    let multi = groups.len() > 1;
    groups
        .into_values()
        .rev()
        .map(|ps| {
            let max = ps.iter().map(|p| p.max_mhz).fold(0.0, f64::max).round() as u64;
            let ncpu: usize = ps.iter().map(|p| p.cpus.len().max(1)).sum();
            let cur = ps
                .iter()
                .map(|p| p.cur_mhz * p.cpus.len().max(1) as f64)
                .sum::<f64>()
                / ncpu as f64;
            let cpus: Vec<u32> = ps.iter().flat_map(|p| p.cpus.iter().copied()).collect();
            let b: Vec<f64> = cpus.iter().filter_map(|c| busy.get(c).copied()).collect();
            let active = if b.is_empty() {
                0.0
            } else {
                b.iter().sum::<f64>() / b.len() as f64
            };
            ClusterFreq {
                name: if multi {
                    format!("cpu {ncpu}× ≤{:.1} GHz", max as f64 / 1000.0)
                } else {
                    format!("cpu {ncpu}×")
                },
                cur_mhz: (cur * 10.0).round() / 10.0,
                max_mhz: max as f64,
                active_pct: (active * 10.0).round() / 10.0,
            }
        })
        .collect()
}

struct Zone {
    name: String,
    temp: f64,
    trips: Vec<Trip>,
}

fn thermal_zones(f: &LinuxFilesRaw) -> Vec<Zone> {
    let base = "/sys/class/thermal/";
    dirs_with(f, base, "temp")
        .into_iter()
        .filter_map(|z| {
            let d = format!("{base}{z}");
            if get(f, &format!("{d}/mode")).map(str::trim) == Some("disabled") {
                return None;
            }
            let temp = get(f, &format!("{d}/temp")).and_then(milli_c)?;
            let name = get(f, &format!("{d}/type"))
                .map(|t| t.trim().to_string())
                .unwrap_or(z.clone());
            let mut trips = Vec::new();
            for i in 0..32 {
                let Some(kind) = get(f, &format!("{d}/trip_point_{i}_type")) else {
                    break;
                };
                // Unset trips report 0 or absurd values; ignore them.
                if let Some(c) = get(f, &format!("{d}/trip_point_{i}_temp"))
                    .and_then(milli_c)
                    .filter(|c| *c > 20.0 && *c < 200.0)
                {
                    trips.push(Trip {
                        kind: kind.trim().to_string(),
                        celsius: c,
                    });
                }
            }
            Some(Zone { name, temp, trips })
        })
        .collect()
}

fn raise(lvl: ThermalPressure, level: &mut Option<ThermalPressure>) {
    *level = Some(level.map_or(lvl, |l| l.max(lvl)));
}

fn decode_thermal(
    f: &LinuxFilesRaw,
    prev: Option<&LinuxFilesRaw>,
    dt_ms: Option<u64>,
    accels: &[Accelerator],
) -> (Thermal, BTreeMap<String, SourceStatus>) {
    let mut status = BTreeMap::new();
    let mut t = Thermal::default();

    // Clusters: CPU (cpufreq) + GPUs (busy units feed the throttle factor).
    let mut clusters = cpu_clusters(f, prev);
    status.insert(
        "linux.cpufreq".into(),
        if clusters.is_empty() {
            SourceStatus::Unavailable("no cpufreq policies (VM or driver without scaling_cur_freq)".into())
        } else {
            SourceStatus::Available
        },
    );
    for a in accels {
        if let (Some(cur), Some(max), Some(u)) = (a.clock_mhz.value, a.max_clock_mhz.value, a.util_pct.value)
        {
            clusters.push(ClusterFreq {
                name: a.id.clone(),
                cur_mhz: cur,
                max_mhz: max,
                active_pct: u,
            });
        }
    }
    t.throttle_factor = if clusters.is_empty() {
        Measured::unavailable("cpufreq", "no frequency readouts")
    } else if prev.is_none() && accels.iter().all(|a| !a.util_pct.is_available()) {
        Measured::unavailable("cpufreq", "needs two samples")
    } else {
        oomtop_core::throttle::throttle_factor(&clusters)
    };
    t.clusters = clusters;
    for a in accels {
        if let Some(c) = a.temp_c.value {
            t.temps.push(TempSensor {
                name: format!("{} {}", a.id, a.name),
                celsius: c,
            });
        }
    }

    // Temperatures, trip points and derived pressure.
    let zones = thermal_zones(f);
    let mut level: Option<ThermalPressure> = None;
    let mut hit = false;
    let mut hit_src = String::new();
    let mut any_trip = false;
    for z in &zones {
        t.temps.push(TempSensor {
            name: format!("tz {}", z.name),
            celsius: z.temp,
        });
        for tr in &z.trips {
            let acts = matches!(tr.kind.as_str(), "passive" | "hot" | "critical");
            if !acts {
                continue;
            }
            any_trip = true;
            if z.temp >= tr.celsius {
                hit = true;
                hit_src = format!(
                    "thermal zone {} {:.0}°C ≥ {} trip {:.0}°C",
                    z.name, z.temp, tr.kind, tr.celsius
                );
                raise(
                    if tr.kind == "passive" {
                        ThermalPressure::Heavy
                    } else {
                        ThermalPressure::Trapping
                    },
                    &mut level,
                );
            } else if tr.kind == "passive" && z.temp >= tr.celsius - MODERATE_MARGIN_C {
                raise(ThermalPressure::Moderate, &mut level);
            } else {
                raise(ThermalPressure::Nominal, &mut level);
            }
        }
    }
    let hw_base = "/sys/class/hwmon/";
    let chips = dirs_with(f, hw_base, "name");
    for c in &chips {
        let d = format!("{hw_base}{c}");
        let chip = get(f, &format!("{d}/name")).map(str::trim).unwrap_or(c);
        for n in 1..=64 {
            let Some(temp) = get(f, &format!("{d}/temp{n}_input")).and_then(milli_c) else {
                continue;
            };
            let label = get(f, &format!("{d}/temp{n}_label"))
                .map(|l| l.trim().to_string())
                .unwrap_or_else(|| format!("temp{n}"));
            t.temps.push(TempSensor {
                name: format!("{chip}/{label}"),
                celsius: temp,
            });
            if let Some(crit) = get(f, &format!("{d}/temp{n}_crit"))
                .and_then(milli_c)
                .filter(|c| (40.0..=150.0).contains(c))
            {
                any_trip = true;
                if temp >= crit {
                    hit = true;
                    hit_src = format!("{chip}/{label} {temp:.0}°C ≥ crit {crit:.0}°C");
                    raise(ThermalPressure::Heavy, &mut level);
                } else if temp >= crit - MODERATE_MARGIN_C {
                    raise(ThermalPressure::Moderate, &mut level);
                } else {
                    raise(ThermalPressure::Nominal, &mut level);
                }
            }
        }
    }
    t.temps.truncate(MAX_TEMPS);
    status.insert(
        "linux.thermal".into(),
        if zones.is_empty() && chips.is_empty() {
            SourceStatus::Unavailable("no thermal zones or hwmon sensors".into())
        } else {
            SourceStatus::Available
        },
    );
    t.trip_point_hit = if any_trip {
        Measured::exact(
            hit,
            if hit {
                hit_src
            } else {
                "thermal zone trip points / hwmon crit".to_string()
            },
        )
    } else {
        Measured::unavailable("/sys/class/thermal", "no trip points exposed")
    };
    t.pressure = match level {
        Some(l) => Measured::estimate(
            l,
            "derived from trip points (passive → heavy, hot/critical → trapping)",
        ),
        None => Measured::unavailable("/sys/class/thermal", "no trip points exposed"),
    };

    // RAPL package power.
    let (power, pstatus) = rapl_power(f, prev, dt_ms);
    t.package_power_w = power;
    status.insert("linux.powercap".into(), pstatus);

    // Battery / AC.
    let (on_batt, pct, bstatus) = power_supply(f);
    t.on_battery = on_batt;
    t.battery_pct = pct;
    status.insert("linux.power_supply".into(), bstatus);
    t.adapter_watts =
        Measured::unavailable("/sys/class/power_supply", "adapter wattage not exposed by sysfs");
    t.low_power_mode = match get(f, "/sys/firmware/acpi/platform_profile") {
        Some(p) => Measured::exact(
            p.trim() == "low-power",
            format!("platform_profile = {}", p.trim()),
        ),
        None => Measured::unavailable("/sys/firmware/acpi/platform_profile", "no ACPI platform_profile"),
    };
    (t, status)
}

fn rapl_power(
    f: &LinuxFilesRaw,
    prev: Option<&LinuxFilesRaw>,
    dt_ms: Option<u64>,
) -> (Measured<f64>, SourceStatus) {
    let base = "/sys/class/powercap/";
    let zones: Vec<String> = dirs_with(f, base, "name")
        .into_iter()
        .filter(|z| get(f, &format!("{base}{z}/name")).is_some_and(|n| n.trim().starts_with("package")))
        .collect();
    if zones.is_empty() {
        return (
            Measured::unavailable("/sys/class/powercap", "no RAPL package zones"),
            SourceStatus::Unavailable("no RAPL package zones".into()),
        );
    }
    let denied = zones
        .iter()
        .any(|z| f.missing.contains_key(&format!("{base}{z}/energy_uj")));
    let (Some(prev), Some(dt)) = (prev, dt_ms) else {
        return if denied {
            let r = "powercap energy_uj is root-only (Platypus mitigation)";
            (
                Measured::unavailable("/sys/class/powercap", r),
                SourceStatus::Unavailable(r.into()),
            )
        } else {
            (
                Measured::unavailable("/sys/class/powercap", "needs two samples"),
                SourceStatus::Available,
            )
        };
    };
    let mut watts = 0.0;
    let mut n = 0;
    for z in &zones {
        let k = format!("{base}{z}/energy_uj");
        let (Some(a), Some(b)) = (get(f, &k).and_then(parse_u64), get(prev, &k).and_then(parse_u64)) else {
            continue;
        };
        let range = get(f, &format!("{base}{z}/max_energy_range_uj")).and_then(parse_u64);
        let d = if a >= b {
            a - b
        } else {
            match range {
                Some(r) if r > b => r - b + a,
                _ => continue,
            }
        };
        watts += d as f64 / 1e6 / (dt as f64 / 1000.0);
        n += 1;
    }
    if n == 0 {
        let r = if denied {
            "powercap energy_uj is root-only (Platypus mitigation)"
        } else {
            "energy counters missing"
        };
        return (
            Measured::unavailable("/sys/class/powercap", r),
            SourceStatus::Unavailable(r.into()),
        );
    }
    let q = if n == zones.len() {
        Measured::exact(
            (watts * 100.0).round() / 100.0,
            "Δ powercap intel-rapl package energy_uj",
        )
    } else {
        Measured::estimate(
            (watts * 100.0).round() / 100.0,
            "Δ powercap energy_uj (some packages unreadable)",
        )
    };
    (q, SourceStatus::Available)
}

fn power_supply(f: &LinuxFilesRaw) -> (Measured<bool>, Measured<f64>, SourceStatus) {
    let base = "/sys/class/power_supply/";
    let supplies = dirs_with(f, base, "type");
    let mut batteries: Vec<(String, f64)> = Vec::new();
    let mut discharging = false;
    let mut mains_online: Option<bool> = None;
    for s in &supplies {
        let d = format!("{base}{s}");
        let ty = get(f, &format!("{d}/type")).map(str::trim).unwrap_or("");
        let scope = get(f, &format!("{d}/scope")).map(str::trim);
        match ty {
            // Peripheral batteries (mice, keyboards) have scope=Device.
            "Battery" if scope != Some("Device") => {
                let st = get(f, &format!("{d}/status")).map(str::trim).unwrap_or("");
                discharging |= st == "Discharging";
                if let Some(c) = get(f, &format!("{d}/capacity")).and_then(parse_u64) {
                    batteries.push((s.clone(), (c as f64).min(100.0)));
                }
            }
            "Mains" | "USB" | "USB_C" | "USB_PD" => {
                if let Some(o) = get(f, &format!("{d}/online")).and_then(parse_u64) {
                    mains_online = Some(mains_online.unwrap_or(false) || o == 1);
                }
            }
            _ => {}
        }
    }
    let has_batt = supplies.iter().any(|s| {
        get(f, &format!("{base}{s}/type")).map(str::trim) == Some("Battery")
            && get(f, &format!("{base}{s}/scope")).map(str::trim) != Some("Device")
    });
    if !has_batt {
        return (
            Measured::exact(false, "no system battery in /sys/class/power_supply"),
            Measured::unavailable("/sys/class/power_supply", "no battery"),
            SourceStatus::Available,
        );
    }
    let on_batt = discharging || mains_online == Some(false);
    let pct = if batteries.is_empty() {
        Measured::unavailable("/sys/class/power_supply", "battery capacity not exposed")
    } else {
        let v = batteries.iter().map(|(_, c)| c).sum::<f64>() / batteries.len() as f64;
        Measured::exact(v, format!("/sys/class/power_supply/{}/capacity", batteries[0].0))
    };
    (
        Measured::exact(on_batt, "/sys/class/power_supply status/online"),
        pct,
        SourceStatus::Available,
    )
}

// ---------------------------------------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------------------------------------

fn per_pid<'a>(f: &'a LinuxFilesRaw, pid: u32, leaf: &str) -> Option<&'a str> {
    get(f, &format!("{pid}/{leaf}"))
}

fn stat_starttime(f: &LinuxFilesRaw, pid: u32) -> Option<u64> {
    crate::decode::linux::parse_stat(per_pid(f, pid, "stat")?).map(|s| s.starttime)
}

fn enrich_procs(
    f: &LinuxFilesRaw,
    prev: Option<&LinuxFilesRaw>,
    dt_ms: Option<u64>,
    part: &mut PartialSnapshot,
) {
    if part.processes.is_none() {
        return;
    }
    let gpu_present = f.files.contains_key(keys::GPU_DEVICES);
    let devices = get(f, keys::GPU_DEVICES).unwrap_or("");
    // NVIDIA memory is only visible through NVML (nvidia-drm fdinfo carries no drm-memory-* keys).
    let nvidia = devices.lines().any(|l| l.trim() == "nvidia 1");
    let cards = parse_cards(devices);
    let drm_cards = cards.iter().any(|c| c.driver != "nvidia");
    let nvml_reason = match status_of(f, "nvml") {
        Some(SourceStatus::Unavailable(r)) | Some(SourceStatus::Partial(r)) => r,
        _ => "no NVML data".into(),
    };
    // Always emitted, so a deferral reported by one sample does not stick once markers are complete.
    part.status.insert(
        "linux.markers".into(),
        status_of(f, "markers").unwrap_or(SourceStatus::Available),
    );
    // NVML per-process.
    let nvml_procs = get(f, keys::NVML_PROCS).map(parse_nvml_procs);
    let mut nvml_by_pid: HashMap<u32, (u64, bool)> = HashMap::new();
    for (_, pid, b) in nvml_procs.iter().flatten() {
        let e = nvml_by_pid.entry(*pid).or_insert((0, true));
        match b {
            Some(b) => e.0 += b,
            None => e.1 = false,
        }
    }
    // DRM clients: dedupe by (pdev, client id) across the whole snapshot, attributing to the lowest pid.
    let mut clients: BTreeMap<(String, u64), (u32, DrmClient)> = BTreeMap::new();
    let mut anon = 0u64;
    for (k, v) in &f.files {
        let Some((pid, rest)) = k.split_once('/') else {
            continue;
        };
        if !rest.starts_with("fdinfo/") {
            continue;
        }
        let (Ok(pid), Some(c)) = (pid.parse::<u32>(), parse_drm_fdinfo(v)) else {
            continue;
        };
        let key = match c.client_id {
            Some(id) => (c.pdev.clone().unwrap_or_default(), id),
            None => {
                anon += 1;
                (format!("~{pid}"), anon)
            }
        };
        match clients.get(&key) {
            Some((p, _)) if *p <= pid => {}
            _ => {
                clients.insert(key, (pid, c));
            }
        }
    }
    let unified_pci: BTreeSet<String> = cards
        .iter()
        .filter(|c| c.unified())
        .map(|c| pci_short(&c.pci))
        .collect();
    let mut drm_by_pid: HashMap<u32, (u64, BTreeSet<String>, BTreeSet<String>)> = HashMap::new();
    for (pid, c) in clients.values() {
        let unified = c
            .pdev
            .as_deref()
            .is_some_and(|p| unified_pci.contains(&pci_short(p)));
        let (b, regions) = c.gpu_bytes_for(unified);
        let e = drm_by_pid.entry(*pid).or_default();
        e.0 += b;
        e.1.extend(regions);
        e.2.insert(c.driver.clone());
    }
    let btime = btime_s(f).unwrap_or(0);
    let clk = f.clk_tck.max(1);
    let scanned: BTreeSet<(u32, u64)> = get(f, keys::DRM_SCANNED)
        .map(|t| {
            t.split_whitespace()
                .filter_map(|tok| {
                    let (p, s) = tok.split_once(':')?;
                    Some((p.parse().ok()?, start_time_ms(btime, s.parse().ok()?, clk)))
                })
                .collect()
        })
        .unwrap_or_default();
    let users: HashMap<u32, String> = get(f, keys::USERS)
        .map(|t| {
            t.lines()
                .filter_map(|l| {
                    let (u, n) = l.split_once(' ')?;
                    Some((u.parse().ok()?, n.trim().to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let dt_s = dt_ms.map(|d| d as f64 / 1000.0);
    let Some(procs) = part.processes.as_mut() else {
        return;
    };

    for p in procs.iter_mut() {
        let pid = p.id.pid;
        // GPU memory.
        let nv = nvml_by_pid.get(&pid);
        let drm = drm_by_pid.get(&pid);
        p.mem.gpu = match (nv, drm) {
            (None, None) => {
                // An exact zero needs every GPU kind on the host covered: NVIDIA by NVML, others by a scan
                // of this process's fds.
                let nvidia_ok = !nvidia || nvml_procs.is_some();
                let drm_ok = !drm_cards || scanned.contains(&(pid, p.id.start_time));
                if !gpu_present && nvml_procs.is_none() {
                    Measured::unavailable("gpu", "no GPU")
                } else if nvidia_ok && drm_ok {
                    Measured::exact(0, "no GPU clients (nvml / drm fdinfo)")
                } else if !nvidia_ok {
                    Measured::unavailable("nvml", format!("NVML unavailable: {nvml_reason}"))
                } else {
                    Measured::unavailable("drm fdinfo", "fds not readable (other user) or not scanned yet")
                }
            }
            (nv, drm) => {
                let mut total = 0;
                let mut src = Vec::new();
                let mut exact = true;
                if let Some((b, complete)) = nv {
                    total += b;
                    exact &= *complete;
                    src.push("nvml usedGpuMemory".to_string());
                }
                if let Some((b, regions, drivers)) = drm {
                    total += b;
                    src.push(format!(
                        "drm fdinfo {} [{}]",
                        drivers.iter().cloned().collect::<Vec<_>>().join("+"),
                        regions.iter().cloned().collect::<Vec<_>>().join(",")
                    ));
                }
                if exact {
                    Measured::exact(total, src.join(" + "))
                } else {
                    Measured::estimate(total, format!("{} (some values not available)", src.join(" + ")))
                }
            }
        };
        // Disk I/O.
        match per_pid(f, pid, "io").and_then(parse_proc_io) {
            Some(io) => {
                let mut d = DiskIo {
                    read_bytes: Measured::exact(io.read_bytes, "/proc/<pid>/io:read_bytes"),
                    write_bytes: Measured::exact(io.write_bytes, "/proc/<pid>/io:write_bytes"),
                    read_rate: Measured::unavailable("/proc/<pid>/io", "needs two samples"),
                    write_rate: Measured::unavailable("/proc/<pid>/io", "needs two samples"),
                };
                let same = prev.and_then(|pv| {
                    let st = stat_starttime(pv, pid)?;
                    (Some(st) == stat_starttime(f, pid)).then_some(pv)
                });
                if let (Some(pv), Some(dt)) = (same, dt_s) {
                    if let Some(pio) = per_pid(pv, pid, "io").and_then(parse_proc_io) {
                        d.read_rate = Measured::exact(
                            io.read_bytes.saturating_sub(pio.read_bytes) as f64 / dt,
                            "Δ /proc/<pid>/io:read_bytes",
                        );
                        d.write_rate = Measured::exact(
                            io.write_bytes.saturating_sub(pio.write_bytes) as f64 / dt,
                            "Δ /proc/<pid>/io:write_bytes",
                        );
                    }
                }
                p.disk_io = d;
            }
            None => {
                let why = "not readable (other user)";
                p.disk_io = DiskIo {
                    read_bytes: Measured::unavailable("/proc/<pid>/io", why),
                    write_bytes: Measured::unavailable("/proc/<pid>/io", why),
                    read_rate: Measured::unavailable("/proc/<pid>/io", why),
                    write_rate: Measured::unavailable("/proc/<pid>/io", why),
                };
            }
        }
        // smaps estimate between rotated reads.
        if per_pid(f, pid, "smaps_rollup").is_none() {
            if let (Some(c), Some(rss_now)) = (
                per_pid(f, pid, keys::SMAPS_CACHE).map(parse_kv_kb),
                p.mem.resident.value,
            ) {
                let age = c.get("AgeMs").copied().unwrap_or(u64::MAX);
                if let (Some(pss), Some(rss_then)) = (c.get("Pss"), c.get("Rss")) {
                    if age <= SMAPS_CACHE_MAX_AGE_MS {
                        p.mem.footprint_or_pss = Measured::estimate(
                            estimate_pss(*pss, *rss_then, rss_now),
                            format!("smaps_rollup:Pss read {}s ago, adjusted by ΔRSS", age / 1000),
                        );
                    }
                }
            }
        }
        // Model files.
        if let Some(t) = per_pid(f, pid, keys::MODEL_FILES) {
            let mut files: Vec<String> = p.model_files.clone();
            files.extend(t.lines().filter(|l| !l.is_empty()).map(str::to_string));
            files.sort();
            files.dedup();
            p.model_files = files;
        }
        if p.user.is_none() {
            p.user = p.uid.and_then(|u| users.get(&u).cloned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(f: &mut LinuxFilesRaw, n: u32, cur_khz: u64, max_khz: u64) {
        let d = format!("/sys/devices/system/cpu/cpufreq/policy{n}");
        f.files
            .insert(format!("{d}/scaling_cur_freq"), format!("{cur_khz}\n"));
        f.files
            .insert(format!("{d}/cpuinfo_max_freq"), format!("{max_khz}\n"));
        f.files.insert(format!("{d}/related_cpus"), format!("{n}\n"));
    }

    #[test]
    fn preferred_core_maxima_merge_but_hybrid_clusters_stay_apart() {
        let mut f = LinuxFilesRaw::default();
        // Four "P" cores with preferred-core maxima a few MHz apart, two "E" cores at 3.8 GHz.
        for (n, max) in [(0, 5_137_000), (1, 5_100_000), (2, 5_050_000), (3, 5_120_000)] {
            policy(&mut f, n, 4_000_000, max);
        }
        policy(&mut f, 4, 2_000_000, 3_800_000);
        policy(&mut f, 5, 3_000_000, 3_800_000);
        let c = cpu_clusters(&f, None);
        assert_eq!(c.len(), 2, "{c:?}");
        assert_eq!((c[0].max_mhz, c[0].cur_mhz), (5137.0, 4000.0));
        assert!(c[0].name.starts_with("cpu 4×"), "{}", c[0].name);
        assert_eq!((c[1].max_mhz, c[1].cur_mhz), (3800.0, 2500.0));
    }

    #[test]
    fn apu_memory_includes_gtt_and_budget_matches() {
        let mut f = LinuxFilesRaw::default();
        let d = "/sys/class/drm/card0/device";
        for (k, v) in [
            ("vendor", "0x1002"),
            ("uevent", "DRIVER=amdgpu\nPCI_SLOT_NAME=0000:c4:00.0\n"),
            ("mem_info_vram_total", "536870912"),
            ("mem_info_vram_used", "268435456"),
            ("mem_info_gtt_total", "4294967296"),
            ("mem_info_gtt_used", "1073741824"),
        ] {
            f.files.insert(format!("{d}/{k}"), format!("{v}\n"));
        }
        let (a, err) = drm_accel(&f, "card0", None);
        assert!(err.is_none());
        assert!(a.unified);
        assert_eq!(a.mem_used.value, Some((256 << 20) + (1 << 30)));
        assert_eq!(a.mem_total.value, Some((512 << 20) + (4 << 30)));
        assert_eq!(a.gpu_budget.value, a.mem_total.value);
        // Discrete card (8 GiB VRAM): GTT is host RAM, not GPU memory.
        f.files
            .insert(format!("{d}/mem_info_vram_total"), format!("{}\n", 8u64 << 30));
        let (a, _) = drm_accel(&f, "card0", None);
        assert!(!a.unified);
        assert_eq!(a.mem_used.value, Some(256 << 20));
    }
}

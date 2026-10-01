//! Linux decoders (pure) over `/proc` file contents (SPEC §5 Linux column):
//! footprint = `Pss` from smaps_rollup when present (exact), else `RSS − Shared` from statm (estimate);
//! swapped = `SwapPss`; host available = `MemAvailable`.

use super::PartialSnapshot;
use crate::raw::LinuxFilesRaw;
use oomtop_core::{
    CoreKind, DiskIo, HostCpu, HostInfo, HostMemory, Measured, MemBreakdown, Oom, OomKiller, OsKind,
    PressureLevel, ProcId, ProcState, Process, Psi, SourceStatus,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Parses `key: value [kB]` files (meminfo, status, smaps_rollup) into bytes / raw numbers.
pub fn parse_kv_kb(text: &str) -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    for line in text.lines() {
        let Some((k, rest)) = line.split_once(':') else {
            continue;
        };
        let mut it = rest.split_whitespace();
        let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        let v = if it.next() == Some("kB") { v * 1024 } else { v };
        m.insert(k.trim().to_string(), v);
    }
    m
}

/// Parses `/proc/pressure/memory`.
pub fn parse_psi(text: &str) -> Option<Psi> {
    let mut psi = Psi::default();
    let mut any = false;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let kind = parts.next()?;
        for kv in parts {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            let Ok(v) = v.parse::<f64>() else { continue };
            any = true;
            match (kind, k) {
                ("some", "avg10") => psi.some_avg10 = v,
                ("some", "avg60") => psi.some_avg60 = v,
                ("some", "avg300") => psi.some_avg300 = v,
                ("full", "avg10") => psi.full_avg10 = v,
                ("full", "avg60") => psi.full_avg60 = v,
                ("full", "avg300") => psi.full_avg300 = v,
                _ => {}
            }
        }
    }
    any.then_some(psi)
}

/// Fields of `/proc/<pid>/stat` we use.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stat {
    pub pid: u32,
    pub comm: String,
    pub state: char,
    pub ppid: u32,
    pub utime: u64,
    pub stime: u64,
    pub threads: u32,
    pub starttime: u64,
}

/// Parses `/proc/<pid>/stat` (comm may contain spaces and parentheses).
pub fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let pid = text[..open].trim().parse().ok()?;
    let comm = text[open + 1..close].to_string();
    let rest: Vec<&str> = text[close + 1..].split_whitespace().collect();
    // rest[0] = state (field 3); field n is rest[n - 3]
    let f = |n: usize| rest.get(n - 3).and_then(|v| v.parse::<u64>().ok());
    Some(Stat {
        pid,
        comm,
        state: rest.first()?.chars().next()?,
        ppid: f(4)? as u32,
        utime: f(14)?,
        stime: f(15)?,
        threads: f(20).unwrap_or(0) as u32,
        starttime: f(22)?,
    })
}

fn btime_s(stat: &str) -> Option<u64> {
    stat.lines()
        .find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok()))
}

fn cpu_line(stat: &str) -> Option<(u64, u64)> {
    let l = stat.lines().find(|l| l.starts_with("cpu "))?;
    let v: Vec<u64> = l
        .split_whitespace()
        .skip(1)
        .filter_map(|x| x.parse().ok())
        .collect();
    let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
    let total: u64 = v.iter().take(8).sum();
    Some((total - idle, total))
}

/// `cpuN` lines of `/proc/stat` as `(N, busy, total)` ticks, in CPU-number order.
pub fn per_cpu_lines(stat: &str) -> Vec<(usize, u64, u64)> {
    let mut v: Vec<(usize, u64, u64)> = stat
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("cpu")?;
            let (n, vals) = rest.split_once(char::is_whitespace)?;
            let n: usize = n.parse().ok()?;
            let t: Vec<u64> = vals.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            let idle = t.get(3).copied().unwrap_or(0) + t.get(4).copied().unwrap_or(0);
            let total: u64 = t.iter().take(8).sum();
            Some((n, total.saturating_sub(idle), total))
        })
        .collect();
    v.sort_unstable_by_key(|x| x.0);
    v
}

/// Per-core busy % between two `/proc/stat` samples. Empty on the first sample or when the set of online
/// CPUs changed (hotplug) between samples.
pub fn per_core_pct(now: &[(usize, u64, u64)], prev: Option<&[(usize, u64, u64)]>) -> Vec<f64> {
    let Some(prev) = prev else {
        return Vec::new();
    };
    if prev.len() != now.len() || prev.iter().zip(now).any(|(a, b)| a.0 != b.0) {
        return Vec::new();
    }
    now.iter()
        .zip(prev)
        .map(|(a, b)| {
            let dt = a.2.saturating_sub(b.2);
            if dt == 0 {
                0.0
            } else {
                (a.1.saturating_sub(b.1) as f64 / dt as f64 * 100.0).clamp(0.0, 100.0)
            }
        })
        .collect()
}

/// Kernel cpulist ("0-3,8,10-11") → CPU numbers.
fn cpulist(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                    if b >= a && b - a < 4096 {
                        out.extend(a..=b);
                    }
                }
            }
            None => out.extend(part.parse::<usize>().ok()),
        }
    }
    out
}

/// Core kinds from the hybrid-CPU sysfs lists (Intel `cpu_core` = P, `cpu_atom` = E). Empty when neither
/// exists (homogeneous CPU or unknown).
pub fn core_kinds(ncpu: usize, p_list: Option<&str>, e_list: Option<&str>) -> Vec<CoreKind> {
    if p_list.is_none() && e_list.is_none() {
        return Vec::new();
    }
    let mut v = vec![CoreKind::Unknown; ncpu];
    for (list, kind) in [(p_list, CoreKind::Performance), (e_list, CoreKind::Efficiency)] {
        for i in list.map(cpulist).unwrap_or_default() {
            if let Some(slot) = v.get_mut(i) {
                *slot = kind;
            }
        }
    }
    v
}

/// Start time in ms since epoch, matching `signal::process_start_time_ms`.
pub fn start_time_ms(btime_s: u64, starttime_ticks: u64, clk_tck: u64) -> u64 {
    btime_s * 1000 + starttime_ticks * 1000 / clk_tck.max(1)
}

fn state(c: char) -> ProcState {
    match c {
        'R' => ProcState::Running,
        'S' | 'D' => ProcState::Sleeping,
        'I' => ProcState::Idle,
        'T' | 't' => ProcState::Stopped,
        'Z' | 'X' => ProcState::Zombie,
        _ => ProcState::Unknown,
    }
}

fn pids(files: &BTreeMap<String, String>) -> BTreeSet<u32> {
    files
        .keys()
        .filter_map(|k| k.strip_suffix("/stat"))
        .filter_map(|p| p.parse().ok())
        .collect()
}

pub fn decode_files(
    source: &str,
    f: &LinuxFilesRaw,
    prev: Option<&LinuxFilesRaw>,
    dt_ms: Option<u64>,
) -> PartialSnapshot {
    let mut part = PartialSnapshot::default();
    let page = f.page_size.max(1);
    let clk = f.clk_tck.max(1);
    let stat_txt = f.files.get("stat");
    let btime = stat_txt.and_then(|s| btime_s(s));

    // Host memory.
    if let Some(mi) = f.files.get("meminfo").map(|t| parse_kv_kb(t)) {
        let get = |k: &str| mi.get(k).copied();
        let ex = |k: &str| match get(k) {
            Some(v) => Measured::exact(v, format!("/proc/meminfo:{k}")),
            None => Measured::unavailable("/proc/meminfo", format!("{k} missing")),
        };
        let mut m = HostMemory {
            total: ex("MemTotal"),
            available: ex("MemAvailable"),
            free: ex("MemFree"),
            cached: ex("Cached"),
            app: ex("AnonPages"),
            swap_total: ex("SwapTotal"),
            ..Default::default()
        };
        m.wired = match (get("Unevictable"), get("Mlocked")) {
            (Some(u), _) => Measured::estimate(u, "/proc/meminfo:Unevictable"),
            _ => Measured::unavailable("/proc/meminfo", "Unevictable missing"),
        };
        m.swap_used = match (get("SwapTotal"), get("SwapFree")) {
            (Some(t), Some(fr)) => Measured::exact(t.saturating_sub(fr), "/proc/meminfo:SwapTotal−SwapFree"),
            _ => Measured::unavailable("/proc/meminfo", "Swap* missing"),
        };
        m.compressed = match get("Zswap") {
            Some(z) => Measured::exact(z, "/proc/meminfo:Zswap"),
            None => Measured::unavailable("/proc/meminfo", "no zswap"),
        };
        m.compressed_logical = match get("Zswapped") {
            Some(z) => Measured::exact(z, "/proc/meminfo:Zswapped"),
            None => Measured::unavailable("/proc/meminfo", "no zswap"),
        };
        m.memorystatus_level = Measured::unavailable("memorystatus", "macOS only");
        m.swap_limit = Measured::unavailable("swap_limit", "fixed-size swap: swap_total is the limit");
        m.own_cgroup_limit = Measured::unavailable("cgroup", "not read yet (M1)");
        match f.files.get("pressure/memory").and_then(|t| parse_psi(t)) {
            Some(psi) => {
                m.pressure = Measured::estimate(
                    if psi.some_avg10 > 40.0 {
                        PressureLevel::Critical
                    } else if psi.some_avg10 > 10.0 {
                        PressureLevel::Warn
                    } else {
                        PressureLevel::Normal
                    },
                    "derived from PSI some avg10",
                );
                m.psi = Measured::exact(psi, "/proc/pressure/memory");
            }
            None => {
                m.psi = Measured::unavailable("/proc/pressure/memory", "PSI unavailable");
                m.pressure = Measured::unavailable("/proc/pressure/memory", "PSI unavailable");
            }
        }
        let vm_now = f.files.get("vmstat").map(|t| parse_space_kv(t));
        let vm_prev = prev
            .and_then(|p| p.files.get("vmstat"))
            .map(|t| parse_space_kv(t));
        match (vm_now, vm_prev, dt_ms.filter(|d| *d > 0)) {
            (Some(a), Some(b), Some(dt)) => {
                let rate = |k: &str| match (a.get(k), b.get(k)) {
                    (Some(x), Some(y)) => Measured::exact(
                        (x.saturating_sub(*y) as f64 * page as f64 * 60_000.0 / dt as f64) as u64,
                        format!("Δ /proc/vmstat:{k}"),
                    ),
                    _ => Measured::unavailable("/proc/vmstat", format!("{k} missing")),
                };
                m.swap_in_per_min = rate("pswpin");
                m.swap_out_per_min = rate("pswpout");
            }
            _ => {
                m.swap_in_per_min = Measured::unavailable("/proc/vmstat", "needs two samples");
                m.swap_out_per_min = Measured::unavailable("/proc/vmstat", "needs two samples");
            }
        }
        let total = get("MemTotal").unwrap_or(0);
        part.memory = Some(m);
        part.host = Some(HostInfo {
            os: OsKind::Linux,
            mem_total: total,
            page_size: page,
            boot_time_ms: btime.map(|b| b * 1000),
            cores_logical: stat_txt
                .map(|s| {
                    s.lines()
                        .filter(|l| l.starts_with("cpu") && !l.starts_with("cpu "))
                        .count() as u32
                })
                .unwrap_or(0),
            os_version: f
                .files
                .get("sys/kernel/osrelease")
                .map(|v| format!("Linux {}", v.trim()))
                .unwrap_or_default(),
            hostname: f
                .files
                .get("sys/kernel/hostname")
                .map(|v| v.trim().to_string())
                .unwrap_or_default(),
            model: linux_model(&f.files),
            ..Default::default()
        });
        part.oom = Some(Oom {
            killer: OomKiller::Kernel,
            killers: vec![OomKiller::Kernel],
            ..Default::default()
        });
    }

    // Host CPU.
    let mut cpu = HostCpu::default();
    if let (Some(a), Some(b)) = (
        stat_txt.and_then(|s| cpu_line(s)),
        prev.and_then(|p| p.files.get("stat")).and_then(|s| cpu_line(s)),
    ) {
        let dt = a.1.saturating_sub(b.1);
        if dt > 0 {
            cpu.total_pct = Measured::exact(
                a.0.saturating_sub(b.0) as f64 / dt as f64 * 100.0,
                "Δ /proc/stat cpu",
            );
        }
    } else {
        cpu.total_pct = Measured::unavailable("/proc/stat", "needs two samples");
    }
    if let Some(now) = stat_txt {
        let prev_lines = prev.and_then(|p| p.files.get("stat")).map(|s| per_cpu_lines(s));
        let now_lines = per_cpu_lines(now);
        cpu.per_core_pct = per_core_pct(&now_lines, prev_lines.as_deref());
        cpu.core_kinds = core_kinds(
            now_lines.len(),
            f.files.get("/sys/devices/cpu_core/cpus").map(String::as_str),
            f.files.get("/sys/devices/cpu_atom/cpus").map(String::as_str),
        );
    }
    if let Some(la) = f.files.get("loadavg") {
        let v: Vec<f64> = la
            .split_whitespace()
            .take(3)
            .filter_map(|x| x.parse().ok())
            .collect();
        if v.len() == 3 {
            cpu.load_avg_1 = Measured::exact(v[0], "/proc/loadavg");
            cpu.load_avg_5 = Measured::exact(v[1], "/proc/loadavg");
            cpu.load_avg_15 = Measured::exact(v[2], "/proc/loadavg");
        }
    }
    if stat_txt.is_some() || f.files.contains_key("loadavg") {
        part.cpu = Some(cpu);
    }

    // Processes.
    let pid_set = pids(&f.files);
    if !pid_set.is_empty() {
        let prev_stats: HashMap<u32, Stat> = prev
            .map(|p| {
                pids(&p.files)
                    .into_iter()
                    .filter_map(|pid| Some((pid, parse_stat(p.files.get(&format!("{pid}/stat"))?)?)))
                    .collect()
            })
            .unwrap_or_default();
        let mut procs = Vec::with_capacity(pid_set.len());
        for pid in pid_set {
            let Some(st) = f.files.get(&format!("{pid}/stat")).and_then(|t| parse_stat(t)) else {
                continue;
            };
            let id = ProcId::new(pid, start_time_ms(btime.unwrap_or(0), st.starttime, clk));
            let file = |name: &str| f.files.get(&format!("{pid}/{name}"));
            let cmdline: Vec<String> = file("cmdline")
                .map(|c| {
                    c.split('\0')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let status = file("status").map(|t| parse_kv_kb(t)).unwrap_or_default();
            let uid = file("status").and_then(|t| {
                t.lines().find_map(|l| {
                    l.strip_prefix("Uid:")
                        .and_then(|v| v.split_whitespace().next()?.parse().ok())
                })
            });
            let mut p = Process {
                id,
                ppid: Some(st.ppid),
                name: st.comm.clone(),
                exe: file("exe").cloned().unwrap_or_default(),
                cmdline,
                cwd: file("cwd").cloned(),
                uid,
                state: state(st.state),
                threads: Some(st.threads),
                markers: f.markers.get(&pid).cloned().unwrap_or_default(),
                cgroup: file("cgroup")
                    .and_then(|c| c.lines().find_map(|l| l.strip_prefix("0::").map(str::to_string))),
                oom_score: file("oom_score").and_then(|s| s.trim().parse().ok()),
                ..Default::default()
            };
            let statm: Vec<u64> = file("statm")
                .map(|s| s.split_whitespace().filter_map(|x| x.parse().ok()).collect())
                .unwrap_or_default();
            let resident = statm.get(1).map(|r| r * page);
            let shared = statm.get(2).map(|s| s * page);
            let rollup = file("smaps_rollup").map(|t| parse_kv_kb(t));
            p.mem = MemBreakdown {
                resident: resident
                    .map(|r| Measured::exact(r, "/proc/<pid>/statm"))
                    .or_else(|| {
                        status
                            .get("VmRSS")
                            .map(|r| Measured::exact(*r, "/proc/<pid>/status:VmRSS"))
                    })
                    .unwrap_or_else(|| Measured::unavailable("/proc/<pid>/statm", "unreadable")),
                footprint_or_pss: match (rollup.as_ref().and_then(|r| r.get("Pss")), resident, shared) {
                    (Some(pss), _, _) => Measured::exact(*pss, "/proc/<pid>/smaps_rollup:Pss"),
                    (None, Some(r), Some(s)) => Measured::estimate(r.saturating_sub(s), "statm RSS − Shared"),
                    _ => Measured::unavailable("/proc/<pid>/smaps_rollup", "unreadable"),
                },
                swapped: match rollup.as_ref().and_then(|r| r.get("SwapPss")) {
                    Some(s) => Measured::exact(*s, "/proc/<pid>/smaps_rollup:SwapPss"),
                    None => match status.get("VmSwap") {
                        Some(s) => Measured::estimate(*s, "/proc/<pid>/status:VmSwap"),
                        None => Measured::unavailable("/proc/<pid>/smaps_rollup", "unreadable"),
                    },
                },
                gpu: Measured::unavailable("gpu", "NVML/drm fdinfo not collected yet (M4)"),
                compressed: Measured::unavailable("zswap", "host-level only on Linux"),
                non_resident_est: Measured::unavailable("non_resident_est", "macOS only"),
            };
            p.disk_io = DiskIo::default();
            match (
                prev_stats.get(&pid).filter(|ps| ps.starttime == st.starttime),
                dt_ms.filter(|d| *d > 0),
            ) {
                (Some(ps), Some(dt)) => {
                    let ticks = (st.utime + st.stime).saturating_sub(ps.utime + ps.stime);
                    let secs = ticks as f64 / clk as f64;
                    p.cpu_pct = Measured::exact(
                        secs / (dt as f64 / 1000.0) * 100.0,
                        "Δ /proc/<pid>/stat utime+stime",
                    );
                }
                _ => p.cpu_pct = Measured::unavailable("/proc/<pid>/stat", "needs two samples"),
            }
            procs.push(p);
        }
        part.processes = Some(procs);
    }

    let status = if f.files.is_empty() {
        SourceStatus::Unavailable(
            f.missing
                .iter()
                .next()
                .map(|(k, v)| format!("{k}: {v}"))
                .unwrap_or_else(|| "nothing readable".into()),
        )
    } else if f.truncated {
        SourceStatus::Partial("time budget exceeded; listing truncated".into())
    } else {
        SourceStatus::Available
    };
    part.status.insert(source.to_string(), status);
    part
}

fn parse_space_kv(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.parse().ok()?))
        })
        .collect()
}

/// Where the machine model is read from, in order: DMI (x86 and most arm64 servers/VMs), then the device
/// tree (Raspberry Pi and other boards).
pub const LINUX_MODEL_FILES: [&str; 2] = [
    "/sys/devices/virtual/dmi/id/product_name",
    "/sys/firmware/devicetree/base/model",
];

/// Machine model from DMI `product_name` or the device tree, skipping firmware placeholders.
fn linux_model(files: &BTreeMap<String, String>) -> Option<String> {
    const PLACEHOLDERS: [&str; 6] = [
        "to be filled by o.e.m.",
        "system product name",
        "default string",
        "not applicable",
        "none",
        "o.e.m.",
    ];
    LINUX_MODEL_FILES.iter().find_map(|k| {
        let v = files
            .get(*k)?
            .trim_matches(|c: char| c == '\0' || c.is_whitespace());
        (!v.is_empty() && !PLACEHOLDERS.contains(&v.to_ascii_lowercase().as_str())).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT0: &str = "cpu  300 0 100 1600 0 0 0 0 0 0\ncpu0 200 0 50 750 0 0 0 0 0 0\ncpu1 100 0 50 850 0 0 0 0 0 0\nintr 1\n";
    const STAT1: &str = "cpu  400 0 150 1650 0 0 0 0 0 0\ncpu0 280 0 70 750 0 0 0 0 0 0\ncpu1 120 0 80 900 0 0 0 0 0 0\nintr 2\n";

    #[test]
    fn per_core_from_proc_stat() {
        let now = per_cpu_lines(STAT1);
        assert_eq!(now.len(), 2, "the aggregate `cpu ` line is not a core");
        assert!(per_core_pct(&now, None).is_empty());
        let v = per_core_pct(&now, Some(&per_cpu_lines(STAT0)));
        assert!((v[0] - 100.0).abs() < 1e-9, "{v:?}");
        assert!((v[1] - 50.0).abs() < 1e-9, "{v:?}");
        // Hotplug between samples → no per-core values rather than misaligned ones.
        let one = per_cpu_lines("cpu0 1 0 1 1 0 0 0 0\n");
        assert!(per_core_pct(&now, Some(&one)).is_empty());
    }

    #[test]
    fn hybrid_core_lists() {
        let k = core_kinds(4, Some("0-1"), Some("2,3"));
        assert_eq!(
            k,
            vec![
                CoreKind::Performance,
                CoreKind::Performance,
                CoreKind::Efficiency,
                CoreKind::Efficiency
            ]
        );
        assert!(core_kinds(4, None, None).is_empty());
        assert_eq!(cpulist("0-2,5\n"), vec![0, 1, 2, 5]);
        assert!(cpulist("9-1").is_empty());
    }

    fn raw() -> LinuxFilesRaw {
        let mut f = LinuxFilesRaw {
            clk_tck: 100,
            page_size: 4096,
            ..Default::default()
        };
        f.files.insert(
            "meminfo".into(),
            "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:    8000000 kB\nCached:          3000000 kB\nSwapTotal:       2000000 kB\nSwapFree:        1500000 kB\nAnonPages:       5000000 kB\n".into(),
        );
        f.files.insert(
            "stat".into(),
            "cpu  100 0 100 800 0 0 0 0 0 0\ncpu0 100 0 100 800 0 0 0 0 0 0\nbtime 1700000000\n".into(),
        );
        f.files.insert("pressure/memory".into(), "some avg10=31.00 avg60=20.00 avg300=5.00 total=1\nfull avg10=2.00 avg60=1.00 avg300=0.50 total=1\n".into());
        f.files.insert(
            "1234/stat".into(),
            "1234 (my (odd) proc) S 1 1234 1234 0 -1 0 0 0 0 0 50 50 0 0 20 0 3 0 500 1000 100".into(),
        );
        f.files
            .insert("1234/statm".into(), "1000 200 50 1 0 100 0".into());
        f.files
            .insert("1234/cmdline".into(), "python3\0studio.py\0".into());
        f.files.insert(
            "1234/status".into(),
            "Name:\tx\nUid:\t1000\t1000\t1000\t1000\n".into(),
        );
        f.files.insert(
            "1234/smaps_rollup".into(),
            "Rss: 800 kB\nPss:  700 kB\nSwapPss: 64 kB\n".into(),
        );
        f
    }

    #[test]
    fn decodes_host_and_procs() {
        let f = raw();
        let p = decode_files("linux.procs", &f, None, None);
        let m = p.memory.unwrap();
        assert_eq!(m.available.value, Some(8_000_000 * 1024));
        assert_eq!(m.swap_used.value, Some(500_000 * 1024));
        assert_eq!(m.psi.value.unwrap().some_avg10, 31.0);
        assert_eq!(m.pressure.value, Some(PressureLevel::Warn));
        let procs = p.processes.unwrap();
        assert_eq!(procs.len(), 1);
        let x = &procs[0];
        assert_eq!(x.name, "my (odd) proc");
        assert_eq!(x.ppid, Some(1));
        assert_eq!(x.id.start_time, 1_700_000_000_000 + 5_000);
        assert_eq!(x.cmdline, vec!["python3", "studio.py"]);
        assert_eq!(x.uid, Some(1000));
        assert_eq!(x.mem.footprint_or_pss.value, Some(700 * 1024));
        assert_eq!(x.mem.swapped.value, Some(64 * 1024));
        assert_eq!(x.mem.resident.value, Some(200 * 4096));
    }

    #[test]
    fn cpu_from_two_samples() {
        let a = raw();
        let mut b = raw();
        b.files.insert(
            "1234/stat".into(),
            "1234 (p) S 1 1 1 0 -1 0 0 0 0 0 150 50 0 0 20 0 3 0 500 1000 100".into(),
        );
        let p = decode_files("linux.procs", &b, Some(&a), Some(2000));
        let x = &p.processes.unwrap()[0];
        // 100 ticks / 100 Hz = 1 s over 2 s = 50 %
        assert!((x.cpu_pct.value.unwrap() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn empty_is_unavailable() {
        let mut f = LinuxFilesRaw::default();
        f.missing.insert("meminfo".into(), "permission denied".into());
        let p = decode_files("linux.host", &f, None, None);
        assert!(matches!(
            p.status.get("linux.host"),
            Some(SourceStatus::Unavailable(_))
        ));
    }

    #[test]
    fn machine_model_from_dmi_or_device_tree() {
        let mut m = BTreeMap::new();
        assert_eq!(linux_model(&m), None);
        m.insert(
            "/sys/firmware/devicetree/base/model".to_string(),
            "Raspberry Pi 5 Model B Rev 1.0\0".to_string(),
        );
        assert_eq!(linux_model(&m).as_deref(), Some("Raspberry Pi 5 Model B Rev 1.0"));
        m.insert(
            "/sys/devices/virtual/dmi/id/product_name".to_string(),
            "To Be Filled By O.E.M.\n".to_string(),
        );
        assert_eq!(linux_model(&m).as_deref(), Some("Raspberry Pi 5 Model B Rev 1.0"));
        m.insert(
            "/sys/devices/virtual/dmi/id/product_name".to_string(),
            "Apple Virtualization Generic Platform\n".to_string(),
        );
        assert_eq!(
            linux_model(&m).as_deref(),
            Some("Apple Virtualization Generic Platform")
        );
    }
}

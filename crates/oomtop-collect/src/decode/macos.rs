//! macOS decoders (pure). Memory taxonomy per SPEC §5 (macOS column):
//! footprint = `ri_phys_footprint`; per-process compressed needs a task port (root) → unavailable;
//! per-process swapped is not attributable → unavailable; `non_resident_est = max(0, footprint − resident)`.
//! Host `available` = (free − speculative) + file-backed (⊇ speculative) + purgeable pages, via
//! `oomtop_core::headroom::macos_available_now`; anonymous inactive pages are NOT counted (SPEC §8.1).

use super::PartialSnapshot;
use crate::raw::{MacHostRaw, MacProcRaw, MacProcsRaw};
use oomtop_core::{
    Accelerator, CoreKind, DiskIo, GpuVendor, HostCpu, HostInfo, HostMemory, Measured, MemBreakdown, Oom,
    OomKiller, OsKind, PressureLevel, ProcId, ProcState, Process, SourceStatus,
};
use std::collections::HashMap;

const SRC_VM: &str = "host_statistics64(HOST_VM_INFO64)";

pub fn start_time_ms(p: &MacProcRaw) -> u64 {
    p.start_tvsec * 1000 + p.start_tvusec / 1000
}

fn vm_bytes(h: &MacHostRaw, field: &str, page: u64) -> Measured<u64> {
    match h.vm.get(field) {
        Some(v) => Measured::exact(v * page, format!("{SRC_VM}.{field}")),
        None => Measured::unavailable(SRC_VM, format!("{field} missing")),
    }
}

pub fn decode_host(
    source: &str,
    h: &MacHostRaw,
    prev: Option<&MacHostRaw>,
    dt_ms: Option<u64>,
) -> PartialSnapshot {
    let page = h
        .sysctl
        .get("vm.pagesize")
        .or(h.sysctl.get("hw.pagesize"))
        .copied()
        .unwrap_or(16384)
        .max(1) as u64;
    let total = h.sysctl.get("hw.memsize").map(|v| *v as u64);
    let mut mem = HostMemory {
        total: total
            .map(|t| Measured::exact(t, "sysctl hw.memsize"))
            .unwrap_or_else(|| Measured::unavailable("sysctl hw.memsize", "missing")),
        ..Default::default()
    };
    let g = |k: &str| h.vm.get(k).copied();
    if let (Some(free), Some(purg), Some(ext)) =
        (g("free_count"), g("purgeable_count"), g("external_page_count"))
    {
        // free_count includes speculative pages and external ⊇ speculative: count them once.
        let pages = oomtop_core::headroom::MacVmPages {
            free_count: free,
            speculative_count: g("speculative_count").unwrap_or(0),
            purgeable_count: purg,
            external_page_count: ext,
        };
        mem.available = oomtop_core::headroom::macos_available_now(&pages, page);
    } else {
        mem.available = Measured::unavailable(SRC_VM, "vm statistics unavailable");
    }
    if let (Some(free), Some(spec)) = (g("free_count"), g("speculative_count")) {
        mem.free = Measured::exact(
            free.saturating_sub(spec) * page,
            format!("{SRC_VM}.free_count−speculative"),
        );
    }
    mem.cached = vm_bytes(h, "external_page_count", page);
    mem.wired = vm_bytes(h, "wire_count", page);
    mem.compressed = vm_bytes(h, "compressor_page_count", page);
    mem.compressed_logical = vm_bytes(h, "total_uncompressed_pages_in_compressor", page);
    if let (Some(int), Some(purg)) = (g("internal_page_count"), g("purgeable_count")) {
        mem.app = Measured::exact(
            int.saturating_sub(purg) * page,
            "vm_statistics64: internal − purgeable",
        );
    }
    match &h.swap {
        Some(s) => {
            mem.swap_used = Measured::exact(s.used, "sysctl vm.swapusage");
            mem.swap_total = Measured::exact(s.total, "sysctl vm.swapusage");
            // Swap grows on demand until the swap volume is full: that is jetsam's swap ceiling.
            mem.swap_limit = match h.swap_volume_free {
                Some(free) => Measured::estimate(
                    s.used.saturating_add(free),
                    "vm.swapusage used + statfs /System/Volumes/VM free",
                ),
                None => Measured::unavailable("statfs /System/Volumes/VM", "not sampled"),
            };
        }
        None => {
            mem.swap_used = Measured::unavailable("sysctl vm.swapusage", "unreadable");
            mem.swap_total = Measured::unavailable("sysctl vm.swapusage", "unreadable");
            mem.swap_limit = Measured::unavailable("sysctl vm.swapusage", "unreadable");
        }
    }
    match (prev, dt_ms.filter(|d| *d > 0)) {
        (Some(p), Some(dt)) => {
            let rate = |k: &str| -> Measured<u64> {
                match (h.vm.get(k), p.vm.get(k)) {
                    (Some(a), Some(b)) => Measured::exact(
                        (a.saturating_sub(*b) as f64 * page as f64 * 60_000.0 / dt as f64) as u64,
                        format!("Δ {SRC_VM}.{k}"),
                    ),
                    _ => Measured::unavailable(SRC_VM, format!("{k} missing")),
                }
            };
            mem.swap_in_per_min = rate("swapins");
            mem.swap_out_per_min = rate("swapouts");
        }
        _ => {
            mem.swap_in_per_min = Measured::unavailable(SRC_VM, "needs two samples");
            mem.swap_out_per_min = Measured::unavailable(SRC_VM, "needs two samples");
        }
    }
    mem.pressure = match h.sysctl.get("kern.memorystatus_vm_pressure_level") {
        Some(1) => Measured::exact(PressureLevel::Normal, "kern.memorystatus_vm_pressure_level"),
        Some(2) => Measured::exact(PressureLevel::Warn, "kern.memorystatus_vm_pressure_level"),
        Some(4) => Measured::exact(PressureLevel::Critical, "kern.memorystatus_vm_pressure_level"),
        Some(v) => Measured::unavailable(
            "kern.memorystatus_vm_pressure_level",
            format!("unknown level {v}"),
        ),
        None => Measured::unavailable("kern.memorystatus_vm_pressure_level", "unreadable"),
    };
    mem.memorystatus_level = match h.sysctl.get("kern.memorystatus_level") {
        Some(v) => Measured::exact((*v).clamp(0, 100) as u8, "kern.memorystatus_level"),
        None => Measured::unavailable("kern.memorystatus_level", "unreadable"),
    };
    mem.psi = Measured::unavailable("psi", "Linux only");
    mem.own_cgroup_limit = Measured::unavailable("cgroup", "Linux only");

    let mut cpu = HostCpu::default();
    if let (Some(a), Some(b)) = (h.cpu_ticks, prev.and_then(|p| p.cpu_ticks)) {
        let busy = |t: [u64; 4]| t[0] + t[1] + t[3];
        let tot = |t: [u64; 4]| t[0] + t[1] + t[2] + t[3];
        let dt = tot(a).saturating_sub(tot(b));
        if dt > 0 {
            let pct = busy(a).saturating_sub(busy(b)) as f64 / dt as f64 * 100.0;
            cpu.total_pct = Measured::exact(pct, "host_statistics(HOST_CPU_LOAD_INFO)");
        }
    } else {
        cpu.total_pct = Measured::unavailable("HOST_CPU_LOAD_INFO", "needs two samples");
    }
    cpu.per_core_pct = per_core_pct(&h.per_cpu_ticks, prev.map(|p| p.per_cpu_ticks.as_slice()));
    cpu.core_kinds = core_kinds(&h.core_clusters);
    if let Some(l) = h.load_avg {
        cpu.load_avg_1 = Measured::exact(l[0], "getloadavg");
        cpu.load_avg_5 = Measured::exact(l[1], "getloadavg");
        cpu.load_avg_15 = Measured::exact(l[2], "getloadavg");
    }

    let ncpu = h.sysctl.get("hw.ncpu").copied().unwrap_or(0).max(0) as u32;
    let arm = h
        .sysctl_str
        .get("machdep.cpu.brand_string")
        .map(|b| b.starts_with("Apple"))
        .unwrap_or(false);
    let model = h.sysctl_str.get("hw.model").cloned();
    let host = HostInfo {
        hostname: h.sysctl_str.get("kern.hostname").cloned().unwrap_or_default(),
        os: OsKind::Macos,
        os_version: h
            .sysctl_str
            .get("kern.osproductversion")
            .map(|v| format!("macOS {v}"))
            .unwrap_or_default(),
        arch: if arm { "aarch64".into() } else { String::new() },
        model: model.clone(),
        cpu_brand: h.sysctl_str.get("machdep.cpu.brand_string").cloned(),
        cores_logical: ncpu,
        cores_performance: h.sysctl.get("hw.perflevel0.logicalcpu").map(|v| *v as u32),
        cores_efficiency: h.sysctl.get("hw.perflevel1.logicalcpu").map(|v| *v as u32),
        mem_total: total.unwrap_or(0),
        unified_memory: arm,
        // Set from the model table by `macos_extras::apply_extras`.
        fanless: None,
        page_size: page,
        boot_time_ms: h.boot_time_ms,
    };

    // Apple GPU: budget from iogpu.wired_limit_mb (0 = OS default ≈ recommendedMaxWorkingSetSize, which
    // needs Metal; approximated as 75 % of RAM until the M4 collector reads it).
    let mut accelerators = Vec::new();
    if arm {
        let budget = match (h.sysctl.get("iogpu.wired_limit_mb"), total) {
            (Some(mb), _) if *mb > 0 => {
                Measured::exact(*mb as u64 * 1024 * 1024, "sysctl iogpu.wired_limit_mb")
            }
            // iogpu.wired_limit_mb = 0 means "OS default" (recommendedMaxWorkingSetSize).
            (_, Some(t)) => Measured::estimate(
                oomtop_core::headroom::unified_budget_estimate(t),
                "≈2/3–3/4 of RAM (recommendedMaxWorkingSetSize default; iogpu.wired_limit_mb = 0)",
            ),
            _ => Measured::unavailable("gpu_budget", "unknown"),
        };
        accelerators.push(Accelerator {
            id: "gpu0".into(),
            vendor: GpuVendor::Apple,
            name: h
                .sysctl_str
                .get("machdep.cpu.brand_string")
                .map(|b| format!("{b} GPU"))
                .unwrap_or_default(),
            unified: true,
            gpu_budget: budget,
            util_pct: Measured::unavailable("IOReport", "no IOReport sample"),
            mem_used: Measured::unavailable("IOAccelerator", "no PerformanceStatistics sample"),
            ..Default::default()
        });
    }

    let oom = Oom {
        killer: OomKiller::Jetsam,
        killers: vec![OomKiller::Jetsam],
        ..Default::default()
    };
    let mut p = PartialSnapshot {
        host: Some(host),
        memory: Some(mem),
        cpu: Some(cpu),
        oom: Some(oom),
        accelerators: Some(accelerators),
        ..Default::default()
    };
    let status = if h.errors.is_empty() {
        SourceStatus::Available
    } else {
        SourceStatus::Partial(h.errors.keys().cloned().collect::<Vec<_>>().join(", ") + " unreadable")
    };
    p.status.insert(source.to_string(), status);
    p
}

/// Per-core busy % from two tick snapshots (user + system + nice over all four states). Empty on the first
/// sample, when the core count changed, or when either side is missing.
pub fn per_core_pct(now: &[[u64; 4]], prev: Option<&[[u64; 4]]>) -> Vec<f64> {
    let Some(prev) = prev.filter(|p| p.len() == now.len() && !now.is_empty()) else {
        return Vec::new();
    };
    now.iter()
        .zip(prev)
        .map(|(a, b)| {
            let d = |i: usize| a[i].saturating_sub(b[i]);
            let busy = d(0) + d(1) + d(3);
            let total = busy + d(2);
            if total == 0 {
                0.0
            } else {
                (busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
            }
        })
        .collect()
}

/// IOKit cluster types → core kinds. Empty when no cluster type is known at all.
pub fn core_kinds(clusters: &[String]) -> Vec<CoreKind> {
    if clusters.iter().all(|c| c.is_empty()) {
        return Vec::new();
    }
    clusters
        .iter()
        .map(|c| match c.as_str() {
            "P" => CoreKind::Performance,
            "E" => CoreKind::Efficiency,
            _ => CoreKind::Unknown,
        })
        .collect()
}

/// Process display name. `pbi_name` when PROC_PIDTBSDINFO answered; otherwise (other users' processes, read via
/// the kinfo_proc fallback) `p_comm` is cut at MAXCOMLEN (16 bytes: "Google Chrome He"), so prefer the
/// executable's basename from `proc_pidpath`, which works for any pid without root.
fn display_name(name: &str, comm: &str, path: &str) -> String {
    if !name.is_empty() {
        return name.to_string();
    }
    match path.rsplit('/').next().filter(|b| !b.is_empty()) {
        Some(base) if comm.is_empty() || base.starts_with(comm) => base.to_string(),
        // MAXCOMLEN (16 bytes) means the kernel cut it: say so instead of showing "Google Chrome He".
        _ if comm.len() >= 16 => format!("{comm}…"),
        _ => comm.to_string(),
    }
}

/// Process state. `pbi_status` is SRUN (2) for nearly every live process on macOS (it only means "not
/// stopped or a zombie"), so runnable-now comes from `pti_numrunning` when readable; without it an SRUN
/// process is reported `Unknown` rather than claimed to be running.
fn proc_state(status: u32, running_threads: Option<u32>) -> ProcState {
    match (status, running_threads) {
        (2, Some(0)) => ProcState::Sleeping,
        (2, Some(_)) => ProcState::Running,
        (2, None) => ProcState::Unknown,
        (s, _) => state(s),
    }
}

fn state(status: u32) -> ProcState {
    match status {
        1 => ProcState::Idle,
        2 => ProcState::Running,
        3 => ProcState::Sleeping,
        4 => ProcState::Stopped,
        5 => ProcState::Zombie,
        _ => ProcState::Unknown,
    }
}

pub fn decode_procs(
    source: &str,
    ps: &MacProcsRaw,
    prev: Option<&MacProcsRaw>,
    dt_ms: Option<u64>,
) -> PartialSnapshot {
    let numer = ps.timebase_numer.max(1) as f64;
    let denom = ps.timebase_denom.max(1) as f64;
    let prev_map: HashMap<(u32, u64), &MacProcRaw> = prev
        .map(|p| p.procs.iter().map(|x| ((x.pid, start_time_ms(x)), x)).collect())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(ps.procs.len());
    let mut denied = 0usize;
    for r in &ps.procs {
        let id = ProcId::new(r.pid, start_time_ms(r));
        let mut p = Process {
            id,
            ppid: Some(r.ppid),
            responsible_pid: r.responsible_pid,
            name: display_name(&r.name, &r.comm, &r.path),
            exe: r.path.clone(),
            cmdline: r.argv.clone().unwrap_or_default(),
            uid: Some(r.uid),
            state: proc_state(r.status, r.running_threads),
            threads: r.threads,
            markers: r.markers.clone().unwrap_or_default(),
            bundle_id: None,
            ..Default::default()
        };
        match &r.rusage {
            Some(ru) => {
                let fp = ru.phys_footprint;
                p.mem = MemBreakdown {
                    resident: Measured::exact(ru.resident_size, "proc_pid_rusage.ri_resident_size"),
                    footprint_or_pss: Measured::exact(fp, "proc_pid_rusage.ri_phys_footprint"),
                    gpu: Measured::unavailable(
                        "IOAccelerator",
                        "folded into footprint on macOS (SPEC §19 Q3)",
                    ),
                    compressed: Measured::unavailable("task_info", "needs root (task port)"),
                    swapped: Measured::unavailable("vm", "not attributable per process on macOS"),
                    non_resident_est: Measured::estimate(
                        fp.saturating_sub(ru.resident_size),
                        "max(0, footprint − resident): compressed or swapped (est.)",
                    ),
                };
                p.disk_io = DiskIo {
                    read_bytes: Measured::exact(ru.diskio_bytesread, "ri_diskio_bytesread"),
                    write_bytes: Measured::exact(ru.diskio_byteswritten, "ri_diskio_byteswritten"),
                    ..Default::default()
                };
                let prev_ru = prev_map
                    .get(&(r.pid, id.start_time))
                    .and_then(|x| x.rusage.as_ref());
                // Per-process interval when both reads are timestamped (an idle process's rusage may be
                // repeated from an earlier read): the same reading twice means no CPU was used.
                let dt = match (prev_ru.and_then(|pr| pr.read_ms), ru.read_ms) {
                    (Some(a), Some(b)) if b > a => Some(b - a),
                    (Some(a), Some(b)) if b == a => None,
                    _ => dt_ms.filter(|d| *d > 0),
                };
                let repeated =
                    matches!((prev_ru.and_then(|pr| pr.read_ms), ru.read_ms), (Some(a), Some(b)) if a == b);
                match (prev_ru, dt) {
                    _ if repeated => {
                        p.cpu_pct = Measured::exact(0.0, "Δ ri_user_time+ri_system_time (idle: not re-read)");
                        p.disk_io.read_rate = Measured::exact(0.0, "Δ ri_diskio_bytesread");
                        p.disk_io.write_rate = Measured::exact(0.0, "Δ ri_diskio_byteswritten");
                    }
                    (Some(pr), Some(dt)) => {
                        let ticks =
                            (ru.user_time + ru.system_time).saturating_sub(pr.user_time + pr.system_time);
                        let ns = ticks as f64 * numer / denom;
                        p.cpu_pct =
                            Measured::exact(ns / (dt as f64 * 1e6) * 100.0, "Δ ri_user_time+ri_system_time");
                        let secs = dt as f64 / 1000.0;
                        p.disk_io.read_rate = Measured::exact(
                            ru.diskio_bytesread.saturating_sub(pr.diskio_bytesread) as f64 / secs,
                            "Δ ri_diskio_bytesread",
                        );
                        p.disk_io.write_rate = Measured::exact(
                            ru.diskio_byteswritten.saturating_sub(pr.diskio_byteswritten) as f64 / secs,
                            "Δ ri_diskio_byteswritten",
                        );
                    }
                    _ => p.cpu_pct = Measured::unavailable("proc_pid_rusage", "needs two samples"),
                }
            }
            None => {
                denied += 1;
                let why = r
                    .rusage_error
                    .clone()
                    .unwrap_or_else(|| "other user's process (needs root)".into());
                p.mem = MemBreakdown {
                    resident: Measured::unavailable("proc_pid_rusage", why.clone()),
                    footprint_or_pss: Measured::unavailable("proc_pid_rusage", why.clone()),
                    gpu: Measured::unavailable("proc_pid_rusage", why.clone()),
                    compressed: Measured::unavailable("proc_pid_rusage", why.clone()),
                    swapped: Measured::unavailable("proc_pid_rusage", why.clone()),
                    non_resident_est: Measured::unavailable("proc_pid_rusage", why.clone()),
                };
                p.cpu_pct = Measured::unavailable("proc_pid_rusage", why);
            }
        }
        out.push(p);
    }
    let mut part = PartialSnapshot {
        processes: Some(out),
        ..Default::default()
    };
    let status = if ps.truncated {
        SourceStatus::Partial("time budget exceeded; listing truncated".into())
    } else if denied > 0 {
        SourceStatus::Partial(format!("{denied} processes of other users need root"))
    } else {
        SourceStatus::Available
    };
    part.status.insert(source.to_string(), status);
    part
}

#[cfg(test)]
mod tests {
    #[test]
    fn truncated_comm_uses_executable_basename() {
        let path = "/Applications/Google Chrome.app/Contents/Frameworks/x/Google Chrome Helper (Renderer)";
        assert_eq!(
            super::display_name("", "Google Chrome He", path),
            "Google Chrome Helper (Renderer)"
        );
        assert_eq!(super::display_name("Full Name", "Full", path), "Full Name");
        assert_eq!(super::display_name("", "python3", "/usr/bin/env"), "python3");
        assert_eq!(super::display_name("", "launchd", ""), "launchd");
        assert_eq!(
            super::display_name("", "Google Chrome He", ""),
            "Google Chrome He…"
        );
    }

    use super::*;
    use crate::raw::{MacRusage, MacSwap};

    #[test]
    fn per_core_needs_two_samples_and_uses_deltas() {
        let t0 = vec![[100, 50, 850, 0], [0, 0, 1000, 0]];
        let t1 = vec![[150, 100, 850, 0], [10, 0, 1090, 0]];
        assert!(
            per_core_pct(&t1, None).is_empty(),
            "first sample has no per-core %"
        );
        assert!(per_core_pct(&t1, Some(&t0[..1])).is_empty(), "core count changed");
        let v = per_core_pct(&t1, Some(&t0));
        assert_eq!(v.len(), 2);
        assert!((v[0] - 100.0).abs() < 1e-9, "{v:?}");
        assert!((v[1] - 10.0).abs() < 1e-9, "{v:?}");
        // A counter that went backwards (wrap / reset) never yields a negative or >100 value.
        let v = per_core_pct(&t0, Some(&t1));
        assert!(v.iter().all(|x| (0.0..=100.0).contains(x)), "{v:?}");
    }

    #[test]
    fn core_kinds_follow_iokit_cluster_types_not_perflevel_order() {
        // M5 Air device tree: cpu0–5 "E", cpu6–9 "P".
        let c: Vec<String> = ["E"; 6]
            .iter()
            .chain(["P"; 4].iter())
            .map(|s| s.to_string())
            .collect();
        let k = core_kinds(&c);
        assert_eq!(k[0], CoreKind::Efficiency);
        assert_eq!(k[9], CoreKind::Performance);
        assert!(
            core_kinds(&[String::new(), String::new()]).is_empty(),
            "unknown → empty, not a guess"
        );
        assert!(core_kinds(&[]).is_empty());
    }

    #[test]
    fn srun_is_only_running_with_runnable_threads() {
        assert_eq!(proc_state(2, Some(1)), ProcState::Running);
        assert_eq!(proc_state(2, Some(0)), ProcState::Sleeping);
        assert_eq!(proc_state(2, None), ProcState::Unknown);
        assert_eq!(proc_state(4, None), ProcState::Stopped);
        assert_eq!(proc_state(5, Some(0)), ProcState::Zombie);
    }

    #[test]
    fn host_available_excludes_anonymous() {
        let mut h = MacHostRaw::default();
        h.sysctl.insert("vm.pagesize".into(), 16384);
        h.sysctl.insert("hw.memsize".into(), 24 << 30);
        h.sysctl.insert("kern.memorystatus_vm_pressure_level".into(), 2);
        for (k, v) in [
            ("free_count", 10u64),
            ("speculative_count", 2),
            ("purgeable_count", 3),
            ("external_page_count", 5),
            ("internal_page_count", 100),
            ("compressor_page_count", 7),
            ("total_uncompressed_pages_in_compressor", 21),
            ("swapins", 0),
        ] {
            h.vm.insert(k.into(), v);
        }
        h.swap = Some(MacSwap {
            total: 100,
            used: 40,
            avail: 60,
            encrypted: true,
        });
        let p = decode_host("macos.host", &h, None, None);
        let m = p.memory.unwrap();
        // (free − speculative) + external (⊇ speculative) + purgeable: speculative counted once.
        assert_eq!(m.available.value, Some((8 + 5 + 3) * 16384));
        assert_eq!(m.free.value, Some(8 * 16384));
        assert_eq!(m.app.value, Some(97 * 16384));
        assert_eq!(m.pressure.value, Some(PressureLevel::Warn));
        assert_eq!(m.swap_used.value, Some(40));
        assert!(m.swap_in_per_min.value.is_none());
    }

    #[test]
    fn procs_footprint_and_cpu() {
        let mk = |ut: u64| MacProcsRaw {
            timebase_numer: 125,
            timebase_denom: 3,
            ncpu: 10,
            truncated: false,
            procs: vec![MacProcRaw {
                pid: 42,
                ppid: 1,
                start_tvsec: 100,
                comm: "sd-server".into(),
                rusage: Some(MacRusage {
                    user_time: ut,
                    resident_size: 500,
                    phys_footprint: 10_000,
                    ..Default::default()
                }),
                ..Default::default()
            }],
        };
        let a = mk(0);
        // 24_000_000 ticks × 125/3 = 1 s of CPU over a 2 s window → 50 %
        let b = mk(24_000_000);
        let p = decode_procs("macos.procs", &b, Some(&a), Some(2000));
        let procs = p.processes.unwrap();
        let x = &procs[0];
        assert_eq!(x.id, ProcId::new(42, 100_000));
        assert_eq!(x.mem.footprint_or_pss.value, Some(10_000));
        assert_eq!(x.mem.non_resident_est.value, Some(9_500));
        assert!((x.cpu_pct.value.unwrap() - 50.0).abs() < 1e-6);
        assert!(x.mem.compressed.value.is_none());
    }

    #[test]
    fn repeated_idle_rusage_uses_the_real_read_interval() {
        let mk = |ut: u64, read_ms: u64| MacProcsRaw {
            timebase_numer: 125,
            timebase_denom: 3,
            procs: vec![MacProcRaw {
                pid: 7,
                start_tvsec: 100,
                rusage: Some(MacRusage {
                    user_time: ut,
                    phys_footprint: 1,
                    read_ms: Some(read_ms),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        // The source repeated an idle process's rusage (same read time): 0 %, not "needs two samples".
        let p = decode_procs("macos.procs", &mk(5, 1_000), Some(&mk(5, 1_000)), Some(2000));
        assert_eq!(p.processes.unwrap()[0].cpu_pct.value, Some(0.0));
        // Re-read 6 s after the last real read with 1 s of CPU: 1/6, not 1/2 (the 2 s sample interval).
        let p = decode_procs(
            "macos.procs",
            &mk(24_000_000, 7_000),
            Some(&mk(0, 1_000)),
            Some(2000),
        );
        let cpu = p.processes.unwrap()[0].cpu_pct.value.unwrap();
        assert!((cpu - 100.0 / 6.0).abs() < 1e-6, "{cpu}");
    }
}

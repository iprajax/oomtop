//! `linux.host` Source: host-level files on two internal tiers (SPEC §6.2).
//! - **every read** (1 s): meminfo, vmstat, stat, loadavg, PSI (`/proc/pressure/{memory,cpu,io}`), and the
//!   *dynamic* sysfs files found by discovery (cgroup `memory.current/events`, cpufreq cur, thermal and
//!   hwmon inputs, RAPL energy, power_supply status, platform_profile, GPU busy/VRAM/`gpu_metrics`), NVML.
//! - **every [`SLOW_EVERY_MS`]**: discovery + static files (limits, trip points, labels, max clocks,
//!   systemd-oomd/earlyoom config), and a resumable scan of `/proc/*/{stat,oom_score}` for the likely OOM
//!   victim and running OOM daemons. Static files are cached and re-emitted, so every sample decodes alone.
//!
//! The Source also tracks kernel OOM kills (`/proc/vmstat:oom_kill`, cgroup `memory.events:oom_kill`) across
//! samples and emits the recent ones (`@oom/kills`), naming the victim when a top `oom_score` candidate
//! vanished in the same interval.

use super::oom::{
    EARLYOOM_COMM, EARLYOOM_DEFAULTS, OOMD_COMM, OOMD_CONF_FILES, OOMD_DROPIN_DIRS, OOMD_UNITS, UNIT_DIRS,
};
use super::parse::{cgroup_ancestors, cgroup_v2_path, parse_space_kv};
use super::{clk_tck, keys, natural_key, nvml, page_size, LinuxRoots, Reader};
use crate::decode::linux::parse_stat;
use crate::raw::{names, LinuxFilesRaw, RawPayload, RawSample};
use crate::{now_ms, Source, SourceError};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Period of discovery, static files and the OOM scan.
pub const SLOW_EVERY_MS: u64 = 10_000;
/// Top `oom_score` candidates kept for the likely-victim report.
pub const OOM_CANDIDATES: usize = 5;
/// Kill records are kept this long.
pub const KILL_RETENTION_MS: u64 = 3_600_000;
const MAX_KILLS: usize = 16;
const MAX_ZONES: usize = 64;
const MAX_TRIPS: usize = 32;
const MAX_HWMON: usize = 32;
const MAX_HWMON_INPUTS: usize = 16;
/// Share of the budget the OOM pid scan may use (it resumes on the next slow tick).
const OOM_SCAN_FRAC: f64 = 0.4;
/// Discovery stops at this share of the budget and resumes on the next read.
const DISCOVERY_FRAC: f64 = 0.8;
/// hwmon drivers never polled.
pub const HWMON_SKIP: &[&str] = &["drivetemp"];

/// One `/proc/<pid>` seen by the OOM scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanEntry {
    pub pid: u32,
    pub starttime: u64,
    pub oom_score: i64,
    pub comm: String,
}

#[derive(Debug, Clone, Default)]
struct OomScan {
    /// Resume point (first pid not yet visited in this pass).
    cursor: u32,
    acc_top: Vec<ScanEntry>,
    acc_daemons: Vec<ScanEntry>,
    /// Results of the last complete pass.
    top: Vec<ScanEntry>,
    daemons: Vec<ScanEntry>,
    complete_once: bool,
}

impl OomScan {
    fn push_top(v: &mut Vec<ScanEntry>, e: ScanEntry) {
        v.push(e);
        v.sort_by(|a, b| b.oom_score.cmp(&a.oom_score).then(a.pid.cmp(&b.pid)));
        v.truncate(OOM_CANDIDATES);
    }

    fn current_top(&self) -> Vec<ScanEntry> {
        if self.complete_once {
            self.top.clone()
        } else {
            self.acc_top.clone()
        }
    }

    fn current_daemons(&self) -> Vec<ScanEntry> {
        if self.complete_once {
            let mut d = self.daemons.clone();
            for e in &self.acc_daemons {
                if !d.iter().any(|x| x.pid == e.pid) {
                    d.push(e.clone());
                }
            }
            d
        } else {
            self.acc_daemons.clone()
        }
    }
}

/// A kernel OOM kill observed between two samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillRecord {
    pub at_ms: u64,
    pub pid: Option<u32>,
    pub name: Option<String>,
    pub source: String,
}

#[derive(Debug, Clone, Default)]
struct KillTracker {
    vm_prev: Option<u64>,
    cg_prev: BTreeMap<String, u64>,
    /// `(pid, starttime)` of the candidates that were alive at the previous read.
    alive_prev: Vec<(u32, u64)>,
    kills: Vec<KillRecord>,
}

impl KillTracker {
    fn observe(
        &mut self,
        now: u64,
        files: &BTreeMap<String, String>,
        candidates: &[ScanEntry],
        alive: impl Fn(&ScanEntry) -> bool,
    ) {
        let vm = files
            .get("vmstat")
            .and_then(|t| parse_space_kv(t).get("oom_kill").copied());
        let mut cg_now = BTreeMap::new();
        for (k, v) in files.range("/sys/fs/cgroup".to_string()..) {
            if !k.starts_with("/sys/fs/cgroup") {
                break;
            }
            if let Some(dir) = k.strip_suffix("/memory.events") {
                if let Some(n) = parse_space_kv(v).get("oom_kill") {
                    cg_now.insert(dir.trim_start_matches("/sys/fs/cgroup").to_string(), *n);
                }
            }
        }
        // A handful of stat reads (≤ OOM_CANDIDATES) per sample.
        let alive_now: Vec<bool> = candidates.iter().map(&alive).collect();
        let vm_delta = match (vm, self.vm_prev) {
            (Some(a), Some(b)) => a.saturating_sub(b),
            _ => 0,
        };
        // Deepest cgroup whose counter moved (the kill happened inside our own subtree).
        let cg_moved = cg_now
            .iter()
            .filter(|(k, v)| self.cg_prev.get(*k).is_some_and(|p| *v > p))
            .map(|(k, v)| (k.clone(), v - self.cg_prev[k]))
            .max_by_key(|(k, _)| k.len());
        if vm_delta > 0 || cg_moved.is_some() {
            let mut source = if vm_delta > 0 {
                format!("/proc/vmstat:oom_kill +{vm_delta}")
            } else {
                String::new()
            };
            if let Some((cg, d)) = &cg_moved {
                let cg = if cg.is_empty() { "/" } else { cg.as_str() };
                if !source.is_empty() {
                    source.push_str("; ");
                }
                source.push_str(&format!("memory.events:oom_kill +{d} ({cg})"));
            }
            // The victim: the highest-scoring candidate that was alive at the previous read and is gone now
            // (one that exited earlier, e.g. a finished job, is never blamed).
            let victim = candidates
                .iter()
                .zip(&alive_now)
                .find(|(c, a)| !**a && self.alive_prev.contains(&(c.pid, c.starttime)))
                .map(|(c, _)| c);
            if victim.is_some() {
                source.push_str("; victim = top oom_score candidate that exited");
            }
            self.kills.push(KillRecord {
                at_ms: now,
                pid: victim.map(|v| v.pid),
                name: victim.map(|v| v.comm.clone()),
                source,
            });
        }
        if vm.is_some() {
            self.vm_prev = vm;
        }
        self.alive_prev = candidates
            .iter()
            .zip(&alive_now)
            .filter(|(_, a)| **a)
            .map(|(c, _)| (c.pid, c.starttime))
            .collect();
        self.cg_prev = cg_now;
        self.kills
            .retain(|k| now.saturating_sub(k.at_ms) <= KILL_RETENTION_MS);
        let n = self.kills.len();
        if n > MAX_KILLS {
            self.kills.drain(..n - MAX_KILLS);
        }
    }

    fn to_text(&self) -> String {
        self.kills
            .iter()
            .map(|k| {
                format!(
                    "{}\t{}\t{}\t{}\n",
                    k.at_ms,
                    k.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                    k.name
                        .clone()
                        .unwrap_or_else(|| "-".into())
                        .replace(['\t', '\n'], " "),
                    k.source
                )
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
struct Slow {
    at_ms: Option<u64>,
    raw: LinuxFilesRaw,
    dynamic: Vec<String>,
    dynamic_hex: Vec<String>,
    nvidia_driver: bool,
    /// Discovery finished within its budget share (otherwise it re-runs on the next read).
    complete: bool,
}

/// Host-level Source.
#[derive(Debug, Clone)]
pub struct LinuxHostSource {
    roots: LinuxRoots,
    nvml: bool,
    slow: Slow,
    scan: OomScan,
    kills: KillTracker,
    clk_tck: u64,
    page_size: u64,
}

impl Default for LinuxHostSource {
    fn default() -> Self {
        Self::with_roots(LinuxRoots::system())
    }
}

impl LinuxHostSource {
    /// `root` = procfs root; `/proc` (or any `…/proc`) also enables sysfs reads under its parent.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_roots(LinuxRoots::from_proc_root(root))
    }

    pub fn with_roots(roots: LinuxRoots) -> Self {
        LinuxHostSource {
            nvml: roots.is_system(),
            roots,
            slow: Slow::default(),
            scan: OomScan::default(),
            kills: KillTracker::default(),
            clk_tck: clk_tck(),
            page_size: page_size(),
        }
    }

    /// Enables/disables NVML (default: only when reading the live system).
    pub fn with_nvml(mut self, on: bool) -> Self {
        self.nvml = on;
        self
    }

    /// Overrides CLK_TCK / page size (fixture trees recorded on other machines).
    pub fn with_units(mut self, clk_tck: u64, page_size: u64) -> Self {
        self.clk_tck = clk_tck.max(1);
        self.page_size = page_size.max(1);
        self
    }

    pub fn roots(&self) -> &LinuxRoots {
        &self.roots
    }

    fn slow_scan(&mut self, now: u64, start: Instant, budget: Duration) {
        let mut raw = LinuxFilesRaw::default();
        let mut dynamic = Vec::new();
        let mut dynamic_hex = Vec::new();
        let roots = self.roots.clone();
        let mut r = Reader::new(&mut raw, &roots, start, budget);
        r.proc_quiet("sys/kernel/osrelease");
        r.proc_quiet("sys/kernel/hostname");
        let nvidia = r.proc_quiet("driver/nvidia/version").is_some();
        let own_cg = r.proc_quiet("self/cgroup").and_then(|t| cgroup_v2_path(&t));

        // OOM scan first (daemon detection decides which configs to read).
        scan_procs(&mut self.scan, &mut r, OOM_SCAN_FRAC);

        if roots.fs.is_some() {
            discover_cgroup(&mut r, own_cg.as_deref(), &mut dynamic);
            discover_cpufreq(&mut r, &mut dynamic);
            discover_thermal(&mut r, &mut dynamic);
            discover_hwmon(&mut r, &mut dynamic);
            discover_power_supply(&mut r, &mut dynamic);
            discover_powercap(&mut r, &mut dynamic);
            // Machine model for `doctor`/the header (tiny, world-readable; absent in containers).
            for m in crate::decode::linux::LINUX_MODEL_FILES {
                if r.abs(m).is_some() {
                    break;
                }
            }
            if r.abs_exists("/sys/firmware/acpi/platform_profile") {
                dynamic.push("/sys/firmware/acpi/platform_profile".into());
            }
            discover_drm(&mut r, &mut dynamic, &mut dynamic_hex);
            let daemons = self.scan.current_daemons();
            if daemons.iter().any(|d| d.comm == OOMD_COMM) {
                read_oomd_config(&mut r);
            }
            if daemons.iter().any(|d| d.comm == EARLYOOM_COMM) {
                r.abs(EARLYOOM_DEFAULTS);
            }
        }
        for d in self.scan.current_daemons() {
            r.proc_quiet(&format!("{}/cmdline", d.pid));
        }
        let complete = !r.spent(DISCOVERY_FRAC);
        // Per-pid noise is not a host-level failure.
        raw.missing.retain(|k, _| k.starts_with('/'));
        if !complete {
            // Keep what the previous pass found beyond this one's cut-off.
            for (k, v) in std::mem::take(&mut self.slow.raw.files) {
                raw.files.entry(k).or_insert(v);
            }
            for d in std::mem::take(&mut self.slow.dynamic) {
                if !dynamic.contains(&d) {
                    dynamic.push(d);
                }
            }
            for d in std::mem::take(&mut self.slow.dynamic_hex) {
                if !dynamic_hex.contains(&d) {
                    dynamic_hex.push(d);
                }
            }
        }
        self.slow = Slow {
            at_ms: Some(now),
            raw,
            dynamic,
            dynamic_hex,
            nvidia_driver: nvidia,
            complete,
        };
    }
}

fn scan_procs(scan: &mut OomScan, r: &mut Reader<'_>, frac: f64) {
    let mut pids: Vec<u32> = super::list_dir(&r.roots.proc)
        .iter()
        .filter_map(|n| n.parse().ok())
        .collect();
    pids.sort_unstable();
    let mut finished = true;
    for pid in pids.iter().copied().filter(|p| *p >= scan.cursor) {
        if r.spent(frac) {
            scan.cursor = pid;
            finished = false;
            break;
        }
        let stat_path = r.roots.proc_path(&format!("{pid}/stat"));
        let Some(st) = super::read_text(&stat_path).ok().and_then(|t| parse_stat(&t)) else {
            continue;
        };
        let score = super::read_text(&r.roots.proc_path(&format!("{pid}/oom_score")))
            .ok()
            .and_then(|t| t.trim().parse::<i64>().ok());
        let e = ScanEntry {
            pid,
            starttime: st.starttime,
            oom_score: score.unwrap_or(0),
            comm: st.comm.clone(),
        };
        if st.comm == OOMD_COMM || st.comm == EARLYOOM_COMM {
            scan.acc_daemons.push(e.clone());
        }
        // Kernel threads (ppid 2 / pid 2) have no memory to reclaim and score 0.
        if score.is_some() && st.ppid != 2 && pid != 2 {
            OomScan::push_top(&mut scan.acc_top, e);
        }
    }
    if finished {
        scan.top = std::mem::take(&mut scan.acc_top);
        scan.daemons = std::mem::take(&mut scan.acc_daemons);
        scan.cursor = 0;
        scan.complete_once = true;
    }
}

fn exists_push(r: &Reader<'_>, path: String, into: &mut Vec<String>) {
    if r.abs_exists(&path) {
        into.push(path);
    }
}

fn discover_cgroup(r: &mut Reader<'_>, own: Option<&str>, dynamic: &mut Vec<String>) {
    r.abs("/sys/fs/cgroup/cgroup.controllers");
    let Some(own) = own else { return };
    for a in cgroup_ancestors(own) {
        if r.spent(DISCOVERY_FRAC) {
            break;
        }
        let base = format!("/sys/fs/cgroup{a}");
        for f in ["memory.max", "memory.high", "memory.swap.max"] {
            r.abs(&format!("{base}/{f}"));
        }
        for f in ["memory.current", "memory.swap.current", "memory.events"] {
            exists_push(r, format!("{base}/{f}"), dynamic);
        }
    }
}

fn discover_cpufreq(r: &mut Reader<'_>, dynamic: &mut Vec<String>) {
    let base = "/sys/devices/system/cpu/cpufreq";
    let mut policies: Vec<String> = r
        .abs_list(base, false)
        .into_iter()
        .filter(|n| n.starts_with("policy"))
        .collect();
    policies.sort_by_key(|n| natural_key(n));
    for p in policies.into_iter().take(512) {
        if r.spent(DISCOVERY_FRAC) {
            break;
        }
        let d = format!("{base}/{p}");
        for f in [
            "cpuinfo_max_freq",
            "cpuinfo_min_freq",
            "related_cpus",
            "scaling_driver",
        ] {
            r.abs(&format!("{d}/{f}"));
        }
        for f in ["scaling_cur_freq", "scaling_max_freq"] {
            exists_push(r, format!("{d}/{f}"), dynamic);
        }
    }
}

fn discover_thermal(r: &mut Reader<'_>, dynamic: &mut Vec<String>) {
    let base = "/sys/class/thermal";
    let mut zones: Vec<String> = r
        .abs_list(base, false)
        .into_iter()
        .filter(|n| n.starts_with("thermal_zone"))
        .collect();
    zones.sort_by_key(|n| natural_key(n));
    for z in zones.into_iter().take(MAX_ZONES) {
        if r.spent(DISCOVERY_FRAC) {
            break;
        }
        let d = format!("{base}/{z}");
        r.abs(&format!("{d}/type"));
        r.abs(&format!("{d}/mode"));
        for i in 0..MAX_TRIPS {
            if r.abs(&format!("{d}/trip_point_{i}_type")).is_none() {
                break;
            }
            r.abs(&format!("{d}/trip_point_{i}_temp"));
        }
        exists_push(r, format!("{d}/temp"), dynamic);
    }
}

fn discover_hwmon(r: &mut Reader<'_>, dynamic: &mut Vec<String>) {
    let base = "/sys/class/hwmon";
    let mut chips: Vec<String> = r
        .abs_list(base, false)
        .into_iter()
        .filter(|n| n.starts_with("hwmon"))
        .collect();
    chips.sort_by_key(|n| natural_key(n));
    for c in chips.into_iter().take(MAX_HWMON) {
        if r.spent(DISCOVERY_FRAC) {
            break;
        }
        let d = format!("{base}/{c}");
        let Some(name) = r.abs(&format!("{d}/name")) else {
            continue;
        };
        // Reading drivetemp issues SMART/SCT commands that can wake sleeping disks every second.
        if HWMON_SKIP.contains(&name.trim()) {
            r.raw.files.remove(&format!("{d}/name"));
            continue;
        }
        let files = r.abs_list(&d, false);
        let mut temps = 0;
        for f in &files {
            let is_temp = f.starts_with("temp") && f.ends_with("_input");
            let is_power = f.starts_with("power") && (f.ends_with("_average") || f.ends_with("_input"));
            let is_freq = f.starts_with("freq") && f.ends_with("_input");
            if is_temp {
                if temps >= MAX_HWMON_INPUTS {
                    continue;
                }
                temps += 1;
                let stem = f.trim_end_matches("_input");
                r.abs(&format!("{d}/{stem}_label"));
                r.abs(&format!("{d}/{stem}_crit"));
                dynamic.push(format!("{d}/{f}"));
            } else if is_power || is_freq {
                dynamic.push(format!("{d}/{f}"));
                if is_freq {
                    r.abs(&format!("{d}/{}_label", f.trim_end_matches("_input")));
                }
            }
        }
    }
}

fn discover_power_supply(r: &mut Reader<'_>, dynamic: &mut Vec<String>) {
    let base = "/sys/class/power_supply";
    for s in r.abs_list(base, false).into_iter().take(16) {
        if r.spent(DISCOVERY_FRAC) {
            break;
        }
        let d = format!("{base}/{s}");
        r.abs(&format!("{d}/type"));
        r.abs(&format!("{d}/scope"));
        for f in ["status", "capacity", "online", "power_now"] {
            exists_push(r, format!("{d}/{f}"), dynamic);
        }
    }
}

fn discover_powercap(r: &mut Reader<'_>, dynamic: &mut Vec<String>) {
    let base = "/sys/class/powercap";
    for z in r.abs_list(base, false) {
        if r.spent(DISCOVERY_FRAC) {
            break;
        }
        // Top-level RAPL zones only ("intel-rapl:0"; subzones "intel-rapl:0:1" are parts of a package).
        let Some(rest) = z.strip_prefix("intel-rapl:") else {
            continue;
        };
        if rest.contains(':') {
            continue;
        }
        let d = format!("{base}/{z}");
        r.abs(&format!("{d}/name"));
        r.abs(&format!("{d}/max_energy_range_uj"));
        // energy_uj is root-only since the Platypus mitigation: a permission failure is recorded as missing.
        if r.abs_exists(&format!("{d}/energy_uj")) {
            dynamic.push(format!("{d}/energy_uj"));
        }
    }
}

fn discover_drm(r: &mut Reader<'_>, dynamic: &mut Vec<String>, hex: &mut Vec<String>) {
    let base = "/sys/class/drm";
    let mut cards: Vec<String> = r
        .abs_list(base, false)
        .into_iter()
        .filter(|n| {
            n.strip_prefix("card")
                .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
        })
        .collect();
    cards.sort_by_key(|n| natural_key(n));
    for c in cards.into_iter().take(16) {
        if r.spent(DISCOVERY_FRAC) {
            break;
        }
        let d = format!("{base}/{c}");
        for f in [
            "device/vendor",
            "device/device",
            "device/uevent",
            "device/product_name",
            "device/mem_info_vram_total",
            "device/mem_info_gtt_total",
            "gt_RP0_freq_mhz",
            "gt_max_freq_mhz",
            "device/tile0/gt0/freq0/rp0_freq",
            "device/tile0/gt0/freq0/max_freq",
        ] {
            r.abs(&format!("{d}/{f}"));
        }
        r.abs_list(&format!("{d}/device/hwmon"), true);
        for f in [
            "device/gpu_busy_percent",
            "device/mem_info_vram_used",
            "device/mem_info_gtt_used",
            "device/pp_dpm_sclk",
            "gt_act_freq_mhz",
            "gt_cur_freq_mhz",
            "device/tile0/gt0/freq0/act_freq",
        ] {
            exists_push(r, format!("{d}/{f}"), dynamic);
        }
        exists_push(r, format!("{d}/device/gpu_metrics"), hex);
    }
}

fn read_conf_dir(r: &mut Reader<'_>, dir: &str) {
    for f in r.abs_list(dir, false).into_iter().take(64) {
        if r.spent(DISCOVERY_FRAC) {
            break;
        }
        if f.ends_with(".conf") {
            r.abs(&format!("{dir}/{f}"));
        }
    }
}

fn read_oomd_config(r: &mut Reader<'_>) {
    for f in OOMD_CONF_FILES {
        r.abs(f);
    }
    for d in OOMD_DROPIN_DIRS {
        read_conf_dir(r, d);
    }
    for d in UNIT_DIRS {
        for u in OOMD_UNITS {
            read_conf_dir(r, &format!("{d}/{u}.d"));
        }
    }
}

impl Source for LinuxHostSource {
    fn name(&self) -> &'static str {
        names::LINUX_HOST
    }

    fn read(&mut self, budget: Duration) -> Result<RawSample, SourceError> {
        let start = Instant::now();
        let now = now_ms();
        if self
            .slow
            .at_ms
            .is_none_or(|t| now.saturating_sub(t) + 50 >= SLOW_EVERY_MS || now < t)
            || !self.slow.complete
        {
            self.slow_scan(now, start, budget);
        }
        let mut raw = LinuxFilesRaw {
            clk_tck: self.clk_tck,
            page_size: self.page_size,
            ..Default::default()
        };
        let roots = self.roots.clone();
        let mut r = Reader::new(&mut raw, &roots, start, budget);
        for rel in [
            "meminfo",
            "vmstat",
            "stat",
            "loadavg",
            "pressure/memory",
            "pressure/cpu",
            "pressure/io",
        ] {
            r.proc(rel);
        }
        let mut truncated = false;
        for p in &self.slow.dynamic {
            if r.spent(0.9) {
                truncated = true;
                break;
            }
            r.abs(p);
        }
        for p in &self.slow.dynamic_hex {
            if r.spent(0.9) {
                truncated = true;
                break;
            }
            r.abs_hex(p);
        }
        if !raw.files.contains_key("meminfo") {
            return Err(SourceError::Unavailable(format!(
                "{}/meminfo unreadable",
                self.roots.proc.display()
            )));
        }
        // Keep only actionable failures: PSI (kernel/config) and permission-denied sysfs files.
        raw.missing
            .retain(|k, _| k.starts_with("pressure/") || k.starts_with('/'));

        // NVML (dlopen) only where the NVIDIA kernel driver is loaded.
        if self.nvml && self.slow.nvidia_driver {
            match nvml::query_host(now) {
                Ok(h) => {
                    raw.files.insert(keys::NVML.into(), super::gpu::nvml_to_text(&h));
                }
                Err(e) => {
                    raw.files
                        .insert(keys::status("nvml"), format!("unavailable: {e}"));
                }
            }
        } else if self.slow.nvidia_driver {
            raw.files.insert(
                keys::status("nvml"),
                "unavailable: NVML disabled for this root".into(),
            );
        }

        // Cached static files + scan results.
        for (k, v) in &self.slow.raw.files {
            raw.files.entry(k.clone()).or_insert_with(|| v.clone());
        }
        for (k, v) in &self.slow.raw.missing {
            raw.missing.entry(k.clone()).or_insert_with(|| v.clone());
        }
        let top = self.scan.current_top();
        let scan_text: String = top
            .iter()
            .map(|e| {
                format!(
                    "{}\t{}\t{}\t{}\n",
                    e.pid,
                    e.starttime,
                    e.oom_score,
                    e.comm.replace(['\t', '\n'], " ")
                )
            })
            .collect();
        if !scan_text.is_empty() {
            raw.files.insert(keys::OOM_SCAN.into(), scan_text);
        }
        let daemons: String = self
            .scan
            .current_daemons()
            .iter()
            .map(|d| format!("{}\t{}\n", d.comm, d.pid))
            .collect();
        if !daemons.is_empty() {
            raw.files.insert(keys::OOM_DAEMONS.into(), daemons);
        }
        let proc_root = self.roots.proc.clone();
        let alive = |c: &ScanEntry| {
            super::read_text(&proc_root.join(format!("{}/stat", c.pid)))
                .ok()
                .and_then(|t| parse_stat(&t))
                .is_some_and(|s| s.starttime == c.starttime)
        };
        self.kills.observe(now, &raw.files, &top, alive);
        let kills = self.kills.to_text();
        if !kills.is_empty() {
            raw.files.insert(keys::OOM_KILLS.into(), kills);
        }
        raw.truncated = truncated;
        Ok(RawSample {
            source: names::LINUX_HOST.into(),
            taken_at_ms: now,
            read_us: start.elapsed().as_micros() as u64,
            payload: RawPayload::LinuxFiles(raw),
        })
    }
}

/// Parses `@oom/scan` lines (`pid starttime oom_score comm`, tab-separated).
pub fn parse_scan(text: &str) -> Vec<ScanEntry> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(4, '\t');
            Some(ScanEntry {
                pid: f.next()?.parse().ok()?,
                starttime: f.next()?.parse().ok()?,
                oom_score: f.next()?.parse().ok()?,
                comm: f.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

/// Parses `@oom/daemons` lines (`comm pid`).
pub fn parse_daemons(text: &str) -> HashMap<String, u32> {
    text.lines()
        .filter_map(|l| {
            let (c, p) = l.split_once('\t')?;
            Some((c.to_string(), p.trim().parse().ok()?))
        })
        .collect()
}

/// Parses `@oom/kills` lines.
pub fn parse_kills(text: &str) -> Vec<KillRecord> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(4, '\t');
            let at_ms = f.next()?.parse().ok()?;
            let pid = f.next()?.parse().ok();
            let name = f.next().filter(|n| *n != "-").map(str::to_string);
            Some(KillRecord {
                at_ms,
                pid,
                name,
                source: f.next().unwrap_or("").to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_tracker_counts_deltas_and_names_the_victim() {
        let mut k = KillTracker::default();
        let mut files = BTreeMap::new();
        files.insert("vmstat".to_string(), "pgfault 1\noom_kill 3\n".to_string());
        files.insert(
            "/sys/fs/cgroup/user.slice/memory.events".to_string(),
            "low 0\nhigh 0\nmax 4\noom 1\noom_kill 1\n".to_string(),
        );
        let cands = vec![
            ScanEntry {
                pid: 10,
                starttime: 5,
                oom_score: 900,
                comm: "python3".into(),
            },
            ScanEntry {
                pid: 11,
                starttime: 6,
                oom_score: 100,
                comm: "bash".into(),
            },
        ];
        k.observe(1_000, &files, &cands, |_| true);
        assert!(
            k.kills.is_empty(),
            "baseline counters since boot are not recent kills"
        );
        files.insert("vmstat".to_string(), "oom_kill 4\n".to_string());
        files.insert(
            "/sys/fs/cgroup/user.slice/memory.events".to_string(),
            "oom_kill 2\n".to_string(),
        );
        k.observe(2_000, &files, &cands, |c| c.pid != 10);
        assert_eq!(k.kills.len(), 1);
        let rec = &k.kills[0];
        assert_eq!(rec.pid, Some(10));
        assert_eq!(rec.name.as_deref(), Some("python3"));
        assert!(rec.source.contains("oom_kill +1"));
        assert!(rec.source.contains("/user.slice"));
        let back = parse_kills(&k.to_text());
        assert_eq!(back, k.kills);
        // Retention.
        k.observe(2_000 + KILL_RETENTION_MS + 1, &files, &cands, |_| true);
        assert!(k.kills.is_empty());

        // A candidate that had already exited before the interval of the kill is never blamed.
        let mut k = KillTracker::default();
        files.insert("vmstat".to_string(), "oom_kill 4\n".to_string());
        k.observe(10_000, &files, &cands, |c| c.pid != 10);
        files.insert("vmstat".to_string(), "oom_kill 5\n".to_string());
        k.observe(11_000, &files, &cands, |c| c.pid != 10);
        assert_eq!(k.kills.len(), 1);
        assert_eq!(k.kills[0].pid, None, "pid 10 was gone before the kill");
        assert!(!k.kills[0].source.contains("victim"));
    }

    #[test]
    fn scan_text_roundtrip() {
        let s = parse_scan("42\t100\t812\tllama server\nbad\n");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].comm, "llama server");
        assert_eq!(parse_daemons("earlyoom\t77\n")["earlyoom"], 77);
    }
}

//! `RawSample`: the serde-serializable output of a [`crate::Source`] (SPEC §6.1). File contents on Linux,
//! syscall result structs on macOS. Recorded by `tools/capture` into `fixtures/`, replayed through
//! [`crate::decode::decode`] on any OS.
//!
//! Privacy: environments never appear here. Sources convert allowlisted environment keys into
//! [`oomtop_core::Markers`] (hashed session ids) before building a `RawSample`.

use oomtop_core::Markers;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Source names used as `RawSample::source` and `Snapshot::source_status` keys.
pub mod names {
    pub const MACOS_HOST: &str = "macos.host";
    pub const MACOS_PROCS: &str = "macos.procs";
    pub const LINUX_HOST: &str = "linux.host";
    pub const LINUX_PROCS: &str = "linux.procs";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawSample {
    /// Source name, e.g. "macos.host".
    pub source: String,
    /// Wall clock, ms since the Unix epoch.
    pub taken_at_ms: u64,
    /// Time the read took, µs (for the ≤ 50 ms budget, SPEC §14).
    #[serde(default)]
    pub read_us: u64,
    pub payload: RawPayload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RawPayload {
    MacHost(MacHostRaw),
    MacProcs(MacProcsRaw),
    LinuxFiles(LinuxFilesRaw),
    /// The source could not read anything; decoders turn this into `SourceStatus::Unavailable`.
    Unavailable {
        reason: String,
    },
}

/// `xsw_usage` from `sysctl vm.swapusage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MacSwap {
    pub total: u64,
    pub avail: u64,
    pub used: u64,
    pub encrypted: bool,
}

/// macOS host-level data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MacHostRaw {
    /// Integer sysctls by name (hw.memsize, vm.pagesize, kern.memorystatus_level,
    /// kern.memorystatus_vm_pressure_level, iogpu.wired_limit_mb, hw.ncpu, hw.perflevel{0,1}.logicalcpu).
    pub sysctl: BTreeMap<String, i64>,
    /// String sysctls (hw.model, machdep.cpu.brand_string, kern.osproductversion, kern.hostname).
    pub sysctl_str: BTreeMap<String, String>,
    /// `host_statistics64(HOST_VM_INFO64)` fields by name (page counts / counters).
    pub vm: BTreeMap<String, u64>,
    pub swap: Option<MacSwap>,
    /// Free bytes on the swap volume (`statfs("/System/Volumes/VM")`, else `/`): how much further swap can
    /// grow (macOS adds swap files on demand). `None` in fixtures captured before it existed.
    pub swap_volume_free: Option<u64>,
    /// `host_statistics(HOST_CPU_LOAD_INFO)`: user, system, idle, nice ticks.
    pub cpu_ticks: Option<[u64; 4]>,
    /// `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`: user, system, idle, nice ticks per logical CPU, in
    /// CPU-number order. Empty when unreadable or in older fixtures.
    pub per_cpu_ticks: Vec<[u64; 4]>,
    /// IOKit `cluster-type` per logical CPU ("E" / "P"; empty = unknown), parallel to `per_cpu_ticks`.
    pub core_clusters: Vec<String>,
    pub load_avg: Option<[f64; 3]>,
    /// `kern.boottime`, ms since epoch.
    pub boot_time_ms: Option<u64>,
    /// Per-field read errors (name → reason).
    pub errors: BTreeMap<String, String>,
}

/// `proc_pid_rusage(RUSAGE_INFO_V4)` subset. Times are Mach absolute units (see `MacProcsRaw::timebase`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MacRusage {
    pub user_time: u64,
    pub system_time: u64,
    pub resident_size: u64,
    pub phys_footprint: u64,
    pub wired_size: u64,
    pub lifetime_max_phys_footprint: u64,
    pub diskio_bytesread: u64,
    pub diskio_byteswritten: u64,
    /// When this rusage was read (ms since epoch). The source re-reads an idle process only every few
    /// refreshes and repeats its last rusage in between; the decoder then takes CPU deltas over the real
    /// interval between reads. `None` in older fixtures (the sample interval is used).
    pub read_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MacProcRaw {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub start_tvsec: u64,
    pub start_tvusec: u64,
    /// `pbi_status`: 1 idle, 2 run, 3 sleep, 4 stop, 5 zombie.
    pub status: u32,
    pub comm: String,
    pub name: String,
    pub path: String,
    pub threads: Option<u32>,
    /// `pti_numrunning` (threads currently runnable); `Some(0)` for a process idle since its last read,
    /// `None` when unreadable (other user) or in older fixtures.
    pub running_threads: Option<u32>,
    /// `None` when the process belongs to another user (EPERM) or exited mid-read.
    pub rusage: Option<MacRusage>,
    pub rusage_error: Option<String>,
    /// argv from `KERN_PROCARGS2` (same-user only). Redacted by `tools/capture` before storing fixtures.
    pub argv: Option<Vec<String>>,
    /// Allowlisted marker keys from the process environment (values hashed).
    pub markers: Option<Markers>,
    pub responsible_pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MacProcsRaw {
    pub procs: Vec<MacProcRaw>,
    /// `mach_timebase_info` numer/denom (Mach units → ns). Apple Silicon: 125/3.
    pub timebase_numer: u32,
    pub timebase_denom: u32,
    pub ncpu: u32,
    /// true if the time budget cut the listing short (→ `SourceStatus::Partial`).
    pub truncated: bool,
}

/// Linux: file contents keyed by path relative to `/proc` ("meminfo", "1234/stat", "1234/exe" = readlink).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LinuxFilesRaw {
    pub files: BTreeMap<String, String>,
    /// Files that could not be read (path → reason).
    pub missing: BTreeMap<String, String>,
    /// Allowlisted markers per pid (environ is never stored).
    pub markers: BTreeMap<u32, Markers>,
    pub clk_tck: u64,
    pub page_size: u64,
    /// true if the time budget cut the listing short.
    pub truncated: bool,
}

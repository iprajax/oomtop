//! The oomtop data model (SPEC §5). **FROZEN**: additive changes only.
//!
//! New fields must be `Option<_>` or carry a serde default (every struct here is `#[serde(default)]` and
//! implements `Default`), so older JSON keeps deserializing and older consumers ignore new fields.
//!
//! Invariants:
//! - Every *measured* number is a [`Measured<T>`] carrying `source` and `quality`; an unavailable value is
//!   `value: None` + `Quality::Unavailable(reason)` and is never rendered as zero.
//! - Bytes are `u64`, times are milliseconds since the Unix epoch (`*_ms`) or seconds (`*_s`).
//! - Process identity is always [`ProcId`] `(pid, start_time)`, so pid reuse never merges two processes.
//! - Command lines held in a live `Snapshot` are raw (needed for rule matching and adapter argv parsing);
//!   every export path (json, ndjson, serve, MCP) must pass through [`crate::redact::redact_snapshot`].
//! - Environments are never stored: only [`Markers`] derived from allowlisted keys, with values hashed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Output schema version for `json`, `ndjson`, `serve` and MCP (SPEC §6.2).
pub const SCHEMA_VERSION: u32 = 1;

/// Byte counts are always integers.
pub type Bytes = u64;

// ---------------------------------------------------------------------------------------------------------
// Measured<T>
// ---------------------------------------------------------------------------------------------------------

/// How much a measured value can be trusted.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// Read directly from an authoritative OS source.
    Exact,
    /// Derived, interpolated between expensive reads, or a heuristic / lower bound.
    Estimate,
    /// Not available; the string says why (e.g. "needs root", "not supported on this OS").
    Unavailable(String),
}

impl Default for Quality {
    fn default() -> Self {
        Quality::Unavailable("not collected".to_string())
    }
}

/// A measured value with its provenance (SPEC §5). Helpers live in [`crate::measured`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Measured<T> {
    /// `None` whenever `quality` is `Unavailable`.
    pub value: Option<T>,
    /// Where the value came from, e.g. `"proc_pid_rusage.ri_phys_footprint"`, `"/proc/meminfo:MemAvailable"`.
    pub source: String,
    pub quality: Quality,
}

impl<T> Default for Measured<T> {
    fn default() -> Self {
        Measured {
            value: None,
            source: String::new(),
            quality: Quality::default(),
        }
    }
}

/// Availability of one data source for the current snapshot (SPEC §6.1).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    #[default]
    Available,
    Partial(String),
    Unavailable(String),
}

// ---------------------------------------------------------------------------------------------------------
// Host
// ---------------------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum OsKind {
    Macos,
    Linux,
    #[default]
    Other,
}

/// Static-ish facts about the machine.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct HostInfo {
    pub hostname: String,
    pub os: OsKind,
    /// e.g. "macOS 26.6", "Linux 6.8.0".
    pub os_version: String,
    /// e.g. "aarch64", "x86_64".
    pub arch: String,
    /// Hardware model, e.g. "Mac17,3".
    pub model: Option<String>,
    /// e.g. "Apple M5".
    pub cpu_brand: Option<String>,
    pub cores_logical: u32,
    pub cores_performance: Option<u32>,
    pub cores_efficiency: Option<u32>,
    pub mem_total: Bytes,
    /// Unified memory (Apple Silicon) vs discrete VRAM.
    pub unified_memory: bool,
    /// `Some(true)` for fanless machines (thermal-prone), `None` if unknown.
    pub fanless: Option<bool>,
    pub page_size: u64,
    pub boot_time_ms: Option<u64>,
}

/// Host memory pressure level (macOS memorystatus / Linux derived from PSI).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum PressureLevel {
    #[default]
    Normal,
    Warn,
    Critical,
}

/// Linux PSI (`/proc/pressure/memory`), percentages 0..100.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Psi {
    pub some_avg10: f64,
    pub some_avg60: f64,
    pub some_avg300: f64,
    pub full_avg10: f64,
    pub full_avg60: f64,
    pub full_avg300: f64,
}

/// Host memory (SPEC §5, §8.1).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct HostMemory {
    pub total: Measured<Bytes>,
    /// Headroom input `available_now` (SPEC §8.1). macOS: free + speculative + purgeable + file-backed;
    /// anonymous inactive pages are NOT counted. Linux: MemAvailable (capped by own cgroup when set).
    pub available: Measured<Bytes>,
    pub free: Measured<Bytes>,
    pub cached: Measured<Bytes>,
    pub wired: Measured<Bytes>,
    /// Physical memory occupied by the compressor (macOS) / zswap+zram pool (Linux).
    pub compressed: Measured<Bytes>,
    /// Logical (uncompressed) bytes held in the compressor; `compressed / compressed_logical` = ratio.
    pub compressed_logical: Measured<Bytes>,
    /// Anonymous "app" memory (macOS Activity Monitor "App Memory"; Linux AnonPages).
    pub app: Measured<Bytes>,
    pub swap_used: Measured<Bytes>,
    pub swap_total: Measured<Bytes>,
    /// Swap-ins per minute (pages→bytes), from deltas (SPEC §9 swap storm).
    pub swap_in_per_min: Measured<Bytes>,
    pub swap_out_per_min: Measured<Bytes>,
    pub pressure: Measured<PressureLevel>,
    /// macOS `kern.memorystatus_level` (% available), 0..100.
    pub memorystatus_level: Measured<u8>,
    pub psi: Measured<Psi>,
    /// Linux: `memory.max` of oomtop's own cgroup when set.
    pub own_cgroup_limit: Measured<Bytes>,
    /// macOS: how far swap can grow before jetsam runs out of swap space — swap used + free space on the
    /// swap volume (`/System/Volumes/VM`), an estimate. macOS adds swap files on demand, so `swap_total` is
    /// not a limit there. Unavailable on Linux (fixed-size swap: `swap_total` is the limit).
    #[serde(default)]
    pub swap_limit: Measured<Bytes>,
}

/// Host CPU.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct HostCpu {
    /// Whole-machine utilization, 0..100 (100 = all cores busy).
    pub total_pct: Measured<f64>,
    pub load_avg_1: Measured<f64>,
    pub load_avg_5: Measured<f64>,
    pub load_avg_15: Measured<f64>,
    /// Per-logical-core utilization, 0..100 each, in OS core order (htop-style meters). Empty until two samples
    /// exist or when the source is unavailable.
    pub per_core_pct: Vec<f64>,
    /// Kind of each core, parallel to `per_core_pct` when known (Apple Silicon P/E clusters); empty if unknown.
    pub core_kinds: Vec<CoreKind>,
    /// Process / thread counts for the header "Tasks" line.
    pub tasks: TaskCounts,
}

/// Core class on heterogeneous CPUs.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum CoreKind {
    Performance,
    Efficiency,
    #[default]
    Unknown,
}

/// Counts shown in the htop-style header ("Tasks: 867, 1625 thr; 1 running").
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct TaskCounts {
    pub processes: u32,
    /// Sum of per-process thread counts where readable; `None` when no process reported threads.
    pub threads: Option<u32>,
    pub running: u32,
}

// ---------------------------------------------------------------------------------------------------------
// OOM (SPEC §8.3)
// ---------------------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum OomKiller {
    /// Linux kernel OOM killer (always applies on Linux).
    #[default]
    Kernel,
    SystemdOomd,
    Earlyoom,
    /// macOS memorystatus / jetsam.
    Jetsam,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdMetric {
    /// Swap used as % of swap total.
    #[default]
    SwapUsedPct,
    /// Memory pressure (PSI some/full) %.
    MemPressurePct,
    /// Available memory in bytes.
    AvailableBytes,
    /// Available memory as % of total.
    AvailablePct,
}

/// A threshold at which a killer acts, e.g. systemd-oomd "swap used > 90 %".
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct OomThreshold {
    pub killer: OomKiller,
    pub metric: ThresholdMetric,
    pub value: f64,
    /// Sustained duration required, if any (oomd: 30 s).
    pub duration_s: Option<u32>,
    /// Where the threshold came from ("oomd.conf", "earlyoom argv", "default").
    pub source: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ForecastTarget {
    /// Swap used reaches swap total.
    #[default]
    SwapExhaustion,
    /// Available memory reaches zero (after which the killer acts).
    AvailableExhaustion,
    /// A killer threshold (see `Oom::thresholds`) is crossed.
    KillerThreshold,
}

/// OOM forecast. Present only when the trend is consistent (R² ≥ 0.6) and ETA < 30 min (SPEC §8.3).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Forecast {
    pub target: ForecastTarget,
    /// Which killer acts at the target.
    pub killer: Option<OomKiller>,
    pub eta_s: u64,
    /// 0..1, the R² of the linear fit.
    pub confidence: f64,
    /// Trend rate in bytes per minute (positive = growing towards the target).
    pub rate_per_min: f64,
    /// Window used for the fit, seconds.
    pub window_s: u64,
}

/// Who the OOM killer would likely pick.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Victim {
    pub id: ProcId,
    pub name: String,
    pub group_id: Option<String>,
    /// e.g. "highest oom_score (812)" or "largest background app (heuristic)".
    pub reason: String,
    /// true on macOS (jetsam bands unreadable without root).
    pub heuristic: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct OomKill {
    pub at_ms: Option<u64>,
    pub killer: OomKiller,
    pub victim_name: Option<String>,
    pub victim_pid: Option<u32>,
    /// e.g. "memory.events:oom_kill", "JetsamEvent-2026-09-29-101010.ips".
    pub source: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Oom {
    /// The nearest applicable killer.
    pub killer: OomKiller,
    /// All killers that apply on this host (kernel is always present on Linux).
    pub killers: Vec<OomKiller>,
    pub thresholds: Vec<OomThreshold>,
    /// `None` = stable / not enough data.
    pub forecast: Option<Forecast>,
    pub likely_victim: Option<Victim>,
    pub recent_kills: Vec<OomKill>,
}

// ---------------------------------------------------------------------------------------------------------
// Accelerators & thermal (SPEC §5, §9)
// ---------------------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum GpuVendor {
    Apple,
    Nvidia,
    Amd,
    Intel,
    #[default]
    Other,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Accelerator {
    /// Stable id within the host, e.g. "gpu0".
    pub id: String,
    pub vendor: GpuVendor,
    pub name: String,
    /// Shares host RAM (Apple Silicon, iGPUs).
    pub unified: bool,
    pub util_pct: Measured<f64>,
    pub mem_used: Measured<Bytes>,
    pub mem_total: Measured<Bytes>,
    /// Metal working-set limit (`recommendedMaxWorkingSetSize` / `iogpu.wired_limit_mb`) (SPEC §8.1).
    pub gpu_budget: Measured<Bytes>,
    pub power_w: Measured<f64>,
    pub temp_c: Measured<f64>,
    pub clock_mhz: Measured<f64>,
    pub max_clock_mhz: Measured<f64>,
    /// e.g. "SW power cap", "HW slowdown", "thermal".
    pub throttle_reasons: Vec<String>,
}

/// Thermal pressure levels (macOS `OSThermalNotification.h`; Linux derived from trip points).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThermalPressure {
    #[default]
    Nominal,
    Moderate,
    Heavy,
    Trapping,
    Sleeping,
}

/// Frequency/residency of one CPU cluster or GPU (input for the throttle factor, SPEC §9).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct ClusterFreq {
    /// e.g. "P-cluster", "E-cluster", "cpu0", "gpu".
    pub name: String,
    pub cur_mhz: f64,
    pub max_mhz: f64,
    /// Active residency 0..100.
    pub active_pct: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct TempSensor {
    pub name: String,
    pub celsius: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Thermal {
    pub pressure: Measured<ThermalPressure>,
    /// 0..1 = observed ÷ max frequency, only while busy; `Unavailable("idle")` otherwise (SPEC §9).
    pub throttle_factor: Measured<f64>,
    pub low_power_mode: Measured<bool>,
    pub on_battery: Measured<bool>,
    /// 0..100.
    pub battery_pct: Measured<f64>,
    pub adapter_watts: Measured<f64>,
    pub package_power_w: Measured<f64>,
    pub clusters: Vec<ClusterFreq>,
    pub temps: Vec<TempSensor>,
    /// Linux: a thermal trip point was hit.
    pub trip_point_hit: Measured<bool>,
}

// ---------------------------------------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------------------------------------

/// Process identity: `(pid, start_time)`. `start_time` is milliseconds since the Unix epoch
/// (Linux: btime + starttime/CLK_TCK; macOS: `pbi_start_tvsec/usec`). Used for identity only.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ProcId {
    pub pid: u32,
    pub start_time: u64,
}

impl ProcId {
    pub fn new(pid: u32, start_time: u64) -> Self {
        ProcId { pid, start_time }
    }
}

/// Per-process memory (SPEC §5 memory taxonomy).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct MemBreakdown {
    pub resident: Measured<Bytes>,
    /// macOS `ri_phys_footprint`; Linux `Pss` (estimate `RSS − Shared` between smaps reads).
    pub footprint_or_pss: Measured<Bytes>,
    pub gpu: Measured<Bytes>,
    pub compressed: Measured<Bytes>,
    pub swapped: Measured<Bytes>,
    /// macOS: `max(0, footprint − resident)`, "compressed or swapped (est.)".
    pub non_resident_est: Measured<Bytes>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct DiskIo {
    /// Cumulative bytes read/written since process start.
    pub read_bytes: Measured<Bytes>,
    pub write_bytes: Measured<Bytes>,
    /// Bytes per second over the last interval.
    pub read_rate: Measured<f64>,
    pub write_rate: Measured<f64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProcState {
    Running,
    Sleeping,
    Idle,
    Stopped,
    Zombie,
    #[default]
    Unknown,
}

/// Session markers derived from allowlisted environment keys (SPEC §7.4). Values are never stored raw.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct Markers {
    /// Hashed session id (e.g. of `CLAUDE_CODE_SESSION_ID`), see [`crate::redact::hash_marker`].
    pub session_id: Option<String>,
    /// Agent name inferred from marker keys, e.g. "claude-code".
    pub agent: Option<String>,
    /// Allowlisted marker keys that were present (keys only, no values).
    pub keys: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Process {
    pub id: ProcId,
    pub ppid: Option<u32>,
    /// macOS responsible pid (helpers/XPC → launching app).
    pub responsible_pid: Option<u32>,
    /// Short name (comm / basename).
    pub name: String,
    /// Executable path (may be empty when unreadable).
    pub exe: String,
    /// argv. Raw in a live snapshot; redacted in every export.
    pub cmdline: Vec<String>,
    pub cwd: Option<String>,
    pub uid: Option<u32>,
    pub user: Option<String>,
    /// Per-core percent (100 = one core busy); needs two samples.
    pub cpu_pct: Measured<f64>,
    pub mem: MemBreakdown,
    pub disk_io: DiskIo,
    pub state: ProcState,
    pub threads: Option<u32>,
    /// Seconds idle; `Estimate` quality means "≥ observed window" (no lineage data).
    pub idle_for_s: Measured<u64>,
    pub markers: Markers,
    /// Linux cgroup v2 path.
    pub cgroup: Option<String>,
    /// Linux `/proc/<pid>/oom_score`.
    pub oom_score: Option<i32>,
    /// macOS bundle id of the owning app, when known.
    pub bundle_id: Option<String>,
    /// Model weight files mapped/open (`*.gguf`, `*.safetensors`), when known.
    pub model_files: Vec<String>,
}

// ---------------------------------------------------------------------------------------------------------
// Groups (SPEC §5, §7)
// ---------------------------------------------------------------------------------------------------------

/// Group kinds with short aliases used in queries, themes and rules.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum GroupKind {
    #[serde(alias = "agent")]
    AgentSession,
    App,
    #[serde(alias = "model")]
    ModelServer,
    Sandbox,
    #[serde(alias = "daemon")]
    BuildDaemon,
    System,
    #[default]
    Other,
}

impl GroupKind {
    pub const ALL: [GroupKind; 7] = [
        GroupKind::AgentSession,
        GroupKind::App,
        GroupKind::ModelServer,
        GroupKind::Sandbox,
        GroupKind::BuildDaemon,
        GroupKind::System,
        GroupKind::Other,
    ];

    /// Short alias: agent | app | model | sandbox | daemon | system | other.
    pub fn alias(self) -> &'static str {
        match self {
            GroupKind::AgentSession => "agent",
            GroupKind::App => "app",
            GroupKind::ModelServer => "model",
            GroupKind::Sandbox => "sandbox",
            GroupKind::BuildDaemon => "daemon",
            GroupKind::System => "system",
            GroupKind::Other => "other",
        }
    }

    /// Canonical serde name: agent_session | app | model_server | sandbox | build_daemon | system | other.
    pub fn as_str(self) -> &'static str {
        match self {
            GroupKind::AgentSession => "agent_session",
            GroupKind::App => "app",
            GroupKind::ModelServer => "model_server",
            GroupKind::Sandbox => "sandbox",
            GroupKind::BuildDaemon => "build_daemon",
            GroupKind::System => "system",
            GroupKind::Other => "other",
        }
    }

    /// Parses either the canonical name or the short alias (case-insensitive).
    pub fn parse(s: &str) -> Option<GroupKind> {
        let s = s.trim().to_ascii_lowercase();
        GroupKind::ALL
            .into_iter()
            .find(|k| k.alias() == s || k.as_str() == s)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    #[default]
    Low,
    Medium,
    High,
}

/// Which attribution signal placed a process in its group (SPEC §7, strongest first).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum AttributionSignal {
    Adapter,
    Cgroup,
    Responsible,
    Marker,
    Ancestry,
    Lineage,
    Rule,
    #[default]
    Heuristic,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Member {
    pub id: ProcId,
    pub confidence: Confidence,
    pub via: AttributionSignal,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct GroupTotals {
    /// Σ `footprint_or_pss` of members.
    pub footprint: Measured<Bytes>,
    pub resident: Measured<Bytes>,
    pub gpu: Measured<Bytes>,
    pub swapped: Measured<Bytes>,
    /// Σ per-core CPU % of members.
    pub cpu_pct: Measured<f64>,
    pub process_count: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Group {
    /// Unique within a snapshot and stable across samples for the same root, e.g. "agent:3f2a9c01".
    pub id: String,
    pub kind: GroupKind,
    pub label: String,
    /// Entity fingerprint (UX §4), hex.
    pub fingerprint: String,
    pub root: Option<ProcId>,
    pub members: Vec<Member>,
    pub totals: GroupTotals,
    /// RAM freed if stopped (SPEC §8.1); always `Estimate`.
    pub reclaim_gain: Measured<Bytes>,
    /// Swap freed if stopped, reported separately (SPEC §8.1).
    pub swap_gain: Measured<Bytes>,
    pub confidence: Confidence,
    /// Group that started/owns this one (e.g. sandbox started by an agent session).
    pub owner_group: Option<String>,
    pub orphan: bool,
    pub idle: bool,
    pub idle_for_s: Option<u64>,
    /// Protected groups are never offered for actions (SPEC §13).
    pub protected: bool,
    /// oomtop's own processes.
    pub is_self: bool,
    /// Footprint is a lower bound (Virtualization.framework VMs, SPEC §5).
    pub lower_bound: bool,
    /// Configured VM memory for sandbox groups.
    pub configured_mem: Option<Bytes>,
    /// Rule/adapter that defined this group, for "why" explanations.
    pub matched_by: Option<String>,
}

// ---------------------------------------------------------------------------------------------------------
// Model servers (SPEC §10) and sandboxes (SPEC §11)
// ---------------------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelServerKind {
    Ollama,
    LlamaCpp,
    SdCpp,
    Vllm,
    LmStudio,
    Mlx,
    #[default]
    Generic,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    Gpu,
    Cpu,
    Mixed,
    #[default]
    Unknown,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct LoadedModel {
    pub name: String,
    pub file: Option<String>,
    pub weights_bytes: Measured<Bytes>,
    pub kv_bytes: Measured<Bytes>,
    pub device: Device,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct JobProgress {
    pub done: u32,
    pub total: u32,
    /// Human label, e.g. "generating".
    pub label: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct ModelServer {
    pub id: String,
    pub kind: ModelServerKind,
    /// Always a loopback URL, e.g. "http://127.0.0.1:11434".
    pub endpoint: Option<String>,
    pub pids: Vec<ProcId>,
    pub group_id: Option<String>,
    pub models: Vec<LoadedModel>,
    pub tok_s: Measured<f64>,
    pub s_per_step: Measured<f64>,
    pub queue: Measured<u32>,
    pub busy: Measured<bool>,
    pub progress: Option<JobProgress>,
    pub status: SourceStatus,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxKind {
    Container,
    Vm,
    MicroVm,
    #[default]
    ProcessSandbox,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct SandboxLimits {
    pub mem_max: Option<Bytes>,
    pub cpus: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Sandbox {
    pub id: String,
    pub kind: SandboxKind,
    /// e.g. "docker", "podman", "virtualization.framework", "firecracker", "bwrap".
    pub runtime: String,
    pub label: String,
    pub host_pids: Vec<ProcId>,
    pub configured_mem: Measured<Bytes>,
    pub guest_mem: Measured<Bytes>,
    pub limits: SandboxLimits,
    pub started_by_group: Option<String>,
    /// Host footprint under-counts this sandbox (SPEC §5).
    pub footprint_lower_bound: bool,
}

// ---------------------------------------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------------------------------------

/// One consistent view of the machine. Output of the sampler + attribution + adapters.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Snapshot {
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    pub taken_at_ms: u64,
    pub host: HostInfo,
    pub memory: HostMemory,
    pub cpu: HostCpu,
    pub oom: Oom,
    pub accelerators: Vec<Accelerator>,
    pub thermal: Thermal,
    pub processes: Vec<Process>,
    pub groups: Vec<Group>,
    pub model_servers: Vec<ModelServer>,
    pub sandboxes: Vec<Sandbox>,
    /// Per source name (e.g. "macos.procs", "linux.meminfo", "adapter.ollama").
    pub source_status: BTreeMap<String, SourceStatus>,
    /// pid of the oomtop process that produced this snapshot.
    pub self_pid: Option<u32>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Snapshot {
            schema_version: SCHEMA_VERSION,
            taken_at_ms: 0,
            host: HostInfo::default(),
            memory: HostMemory::default(),
            cpu: HostCpu::default(),
            oom: Oom::default(),
            accelerators: Vec::new(),
            thermal: Thermal::default(),
            processes: Vec::new(),
            groups: Vec::new(),
            model_servers: Vec::new(),
            sandboxes: Vec::new(),
            source_status: BTreeMap::new(),
            self_pid: None,
        }
    }
}

impl Snapshot {
    /// Finds a process by identity.
    pub fn process(&self, id: ProcId) -> Option<&Process> {
        self.processes.iter().find(|p| p.id == id)
    }

    /// Finds a process by pid only (for ppid/responsible lookups inside one snapshot).
    pub fn process_by_pid(&self, pid: u32) -> Option<&Process> {
        self.processes.iter().find(|p| p.id.pid == pid)
    }

    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// The group containing a process, if attributed.
    pub fn group_of(&self, id: ProcId) -> Option<&Group> {
        self.groups.iter().find(|g| g.members.iter().any(|m| m.id == id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_snapshot_roundtrips_and_has_schema_version() {
        let s = Snapshot::default();
        assert_eq!(s.schema_version, SCHEMA_VERSION);
        let json = serde_json::to_string(&s).unwrap();
        let back: Snapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn missing_fields_deserialize_with_defaults() {
        let back: Snapshot = serde_json::from_str(r#"{"taken_at_ms": 5}"#).unwrap();
        assert_eq!(back.taken_at_ms, 5);
        assert_eq!(back.schema_version, SCHEMA_VERSION);
        assert!(back.memory.total.value.is_none());
    }

    #[test]
    fn group_kind_aliases() {
        for k in GroupKind::ALL {
            assert_eq!(GroupKind::parse(k.alias()), Some(k));
            assert_eq!(GroupKind::parse(k.as_str()), Some(k));
        }
        let k: GroupKind = serde_json::from_str("\"daemon\"").unwrap();
        assert_eq!(k, GroupKind::BuildDaemon);
        assert_eq!(
            serde_json::to_string(&GroupKind::ModelServer).unwrap(),
            "\"model_server\""
        );
    }

    #[test]
    fn quality_serde_shape() {
        assert_eq!(serde_json::to_string(&Quality::Exact).unwrap(), "\"exact\"");
        assert_eq!(
            serde_json::to_string(&Quality::Unavailable("needs root".into())).unwrap(),
            r#"{"unavailable":"needs root"}"#
        );
    }
}

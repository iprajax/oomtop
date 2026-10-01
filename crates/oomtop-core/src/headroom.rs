//! Headroom (SPEC §8.1): `headroom = available_now − safety_margin`, GPU budget, reclaim gain.
//!
//! ```text
//! available_now  Linux: MemAvailable, capped by (memory.max − memory.current) of oomtop's own cgroup
//!                macOS: free + speculative + purgeable + external (file-backed) pages, cross-checked
//!                       against kern.memorystatus_level; anonymous inactive pages are NOT counted
//! safety_margin  max(1.5 GiB, 8 % of RAM); +50 % when pressure ≥ warn or swap is growing
//! headroom       available_now − safety_margin
//! reclaim_gain   per group: private resident (+ compressed share when known); swap separate; estimate
//! reclaimable    Σ reclaim_gain of groups oomtop may suggest (build daemons, orphans, idle model servers)
//! ```
//!
//! The per-OS `available_now` formulas are exposed as pure helpers ([`macos_available_now`],
//! [`linux_available_now`]) so collectors compute `HostMemory::available` the same way everywhere;
//! [`compute`] then applies the conservative cross-checks that only need the snapshot (cgroup cap,
//! memorystatus, fallbacks). Everything here is pure (no I/O).

use crate::history::History;
use crate::measured::sum_bytes;
use crate::model::{
    Accelerator, Device, GpuVendor, Group, GroupKind, Measured, OsKind, PressureLevel, Process, Quality,
    Snapshot,
};
use crate::units::{format_bytes, UnitSystem, GIB, MIB};

/// Byte amounts inside stored notes: IEC with one decimal ("13.5 GiB"), like `CanFitAnswer::reason`, so a
/// note printed next to a GiB headline never switches to the compact table form ("13.5G").
fn note_bytes(v: u64) -> String {
    format_bytes(v, UnitSystem::Iec, 1)
}
use serde::{Deserialize, Serialize};

/// Swap is "growing" when swap-outs exceed this many bytes per minute (SPEC §8.1 margin boost).
pub const SWAP_GROWING_BYTES_PER_MIN: u64 = 50 * MIB;
/// A build daemon using at least this much CPU (per-core %) is considered busy (mid-build) and is not
/// offered for reclaim unless it is also flagged idle.
pub const BUSY_DAEMON_CPU_PCT: f64 = 10.0;
/// memorystatus cross-check: disagreements larger than this (percent of RAM) are noted.
pub const MEMORYSTATUS_TOLERANCE_PCT: f64 = 10.0;
/// Discrete GPUs keep a VRAM safety margin of `max(256 MiB, 5 % of VRAM)` (driver context, fragmentation).
pub const DISCRETE_GPU_MIN_MARGIN: u64 = 256 * MIB;
pub const DISCRETE_GPU_MARGIN_PCT: f64 = 5.0;
/// Apple Silicon default Metal working set (`recommendedMaxWorkingSetSize`) when the collector could not
/// read it: ≈ 2/3 of RAM up to 36 GiB, ≈ 3/4 above (used only as an `Estimate`).
pub const UNIFIED_BUDGET_SMALL_FRACTION: f64 = 2.0 / 3.0;
pub const UNIFIED_BUDGET_LARGE_FRACTION: f64 = 0.75;
pub const UNIFIED_BUDGET_LARGE_ABOVE: u64 = 36 * GIB;

/// Tunables (config `[headroom]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeadroomConfig {
    /// Minimum safety margin in bytes (default 1.5 GiB).
    pub min_margin: u64,
    /// Safety margin as % of RAM (default 8).
    pub margin_pct: f64,
    /// Extra % added to the margin when pressure ≥ warn or swap is growing (default 50).
    pub pressure_boost_pct: f64,
    /// Fixed margin override (bytes); replaces the computed margin before the boost.
    pub margin_override: Option<u64>,
}

impl Default for HeadroomConfig {
    fn default() -> Self {
        HeadroomConfig {
            min_margin: 3 * GIB / 2,
            margin_pct: 8.0,
            pressure_boost_pct: 50.0,
            margin_override: None,
        }
    }
}

/// GPU budget view (Apple Silicon Metal working set or discrete VRAM), SPEC §8.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GpuHeadroom {
    pub accelerator_id: String,
    /// Unified: Metal working-set limit; discrete: VRAM total.
    pub budget: Measured<u64>,
    pub in_use: Measured<u64>,
    /// budget − in_use (saturating).
    pub free: Measured<u64>,
    pub unified: bool,
    pub vendor: GpuVendor,
    pub name: String,
    /// VRAM safety margin (discrete only; on unified memory the host margin applies).
    pub margin: u64,
    /// free − margin; `None` when `free` is unavailable.
    pub headroom: Option<i64>,
}

/// Result of [`compute`]. Serialized by MCP `get_headroom` and `oomtop headroom --json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Headroom {
    pub available_now: Measured<u64>,
    pub safety_margin: u64,
    /// available_now − safety_margin; negative when tight. `None` if available_now is unavailable.
    pub headroom: Option<i64>,
    pub pressure: Option<PressureLevel>,
    pub swap_growing: bool,
    /// Σ reclaim_gain of groups oomtop may suggest (RAM, always `Estimate`).
    pub reclaimable: Measured<u64>,
    /// Σ swap freed by the same groups (reported separately, SPEC §8.1).
    pub reclaimable_swap: Measured<u64>,
    pub gpu: Vec<GpuHeadroom>,
    pub as_of_ms: u64,
    /// Physical RAM used for the margin.
    pub total: Option<u64>,
    /// macOS: `kern.memorystatus_level` × RAM, for the cross-check.
    pub memorystatus_available: Option<u64>,
    /// Whether the margin boost (+pressure_boost_pct) was applied.
    pub margin_boosted: bool,
    /// Human-readable notes about adjustments (cgroup cap, memorystatus, fallbacks).
    pub notes: Vec<String>,
    /// GPU memory is host RAM (Apple Silicon / iGPU): a GPU-resident need also counts against host RAM,
    /// even when no accelerator data was collected.
    pub unified_memory: bool,
}

// -----------------------------------------------------------------------------------------------------
// available_now per OS
// -----------------------------------------------------------------------------------------------------

/// Raw `vm_statistics64` page counters needed for macOS `available_now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MacVmPages {
    /// `free_count` as returned by `host_statistics64` — it **includes** speculative pages
    /// (`vm_stat` prints `free_count − speculative_count` as "Pages free").
    pub free_count: u64,
    pub speculative_count: u64,
    pub purgeable_count: u64,
    /// File-backed pages. Includes speculative pages (on the fixture
    /// `internal + external == active + inactive + speculative` holds exactly).
    pub external_page_count: u64,
}

/// macOS `available_now` = free + speculative + purgeable + file-backed (SPEC §8.1), computed without
/// double counting: `(free_count − speculative) + external (⊇ speculative) + purgeable`.
/// Anonymous inactive pages are not counted (reclaiming them means compression).
pub fn macos_available_now(p: &MacVmPages, page_size: u64) -> Measured<u64> {
    if page_size == 0 {
        return Measured::unavailable("vm_statistics64", "page size unknown");
    }
    let truly_free = p.free_count.saturating_sub(p.speculative_count);
    // Speculative pages are file-backed; count them once even if a kernel reports them outside `external`.
    let file_backed = p.external_page_count.max(p.speculative_count);
    let pages = truly_free
        .saturating_add(file_backed)
        .saturating_add(p.purgeable_count);
    Measured::exact(
        pages.saturating_mul(page_size),
        "vm_statistics64: (free−speculative) + external(⊇speculative) + purgeable",
    )
}

/// Linux `available_now` = MemAvailable, capped by `memory.max − memory.current` of oomtop's own cgroup
/// when a limit is set (SPEC §8.1). A cap makes the value an `Estimate` only if `current` is unknown.
pub fn linux_available_now(
    mem_available: &Measured<u64>,
    cgroup_max: Option<u64>,
    cgroup_current: Option<u64>,
) -> Measured<u64> {
    let Some(avail) = mem_available
        .value
        .filter(|_| mem_available.quality.is_available())
    else {
        return mem_available.clone();
    };
    match (cgroup_max, cgroup_current) {
        (Some(max), Some(cur)) => {
            let room = max.saturating_sub(cur);
            if room < avail {
                Measured {
                    value: Some(room),
                    source: "cgroup memory.max − memory.current (< MemAvailable)".into(),
                    quality: mem_available.quality.clone(),
                }
            } else {
                mem_available.clone()
            }
        }
        (Some(max), None) if max < avail => {
            Measured::estimate(max, "cgroup memory.max (memory.current unknown)")
        }
        _ => mem_available.clone(),
    }
}

/// `kern.memorystatus_level` (% of RAM available) in bytes.
pub fn memorystatus_bytes(level: u8, total: u64) -> u64 {
    (total as f64 * (level.min(100) as f64) / 100.0) as u64
}

struct Available {
    value: Measured<u64>,
    memorystatus: Option<u64>,
    notes: Vec<String>,
}

/// Applies the snapshot-level rules on top of `HostMemory::available`: fallbacks, own-cgroup cap, and the
/// macOS memorystatus cross-check. Always picks the more conservative (smaller) value.
fn resolve_available(s: &Snapshot, total: Option<u64>) -> Available {
    let mem = &s.memory;
    let mut notes = Vec::new();
    let memorystatus = match (mem.memorystatus_level.value, total) {
        (Some(level), Some(t)) if mem.memorystatus_level.quality.is_available() && t > 0 => {
            Some(memorystatus_bytes(level, t))
        }
        _ => None,
    };
    let mut value = if mem.available.is_available() {
        mem.available.clone()
    } else if let Some(ms) = memorystatus {
        notes.push("available_now from kern.memorystatus_level (vm statistics unavailable)".into());
        Measured::estimate(ms, "kern.memorystatus_level × RAM")
    } else if let (Some(free), Some(cached)) = (mem.free.usable(), mem.cached.usable()) {
        notes.push("available_now estimated as free + cached (MemAvailable unavailable)".into());
        Measured::estimate(free.saturating_add(cached), "free + cached")
    } else {
        mem.available.clone()
    };

    // Own cgroup: never promise more than the cgroup limit (memory.current is folded in by the collector).
    if let (Some(limit), Some(avail)) = (mem.own_cgroup_limit.value, value.value) {
        if mem.own_cgroup_limit.quality.is_available() && limit < avail {
            notes.push(format!("capped by own cgroup memory.max ({})", note_bytes(limit)));
            value = Measured::estimate(limit, "cgroup memory.max");
        }
    }

    // macOS cross-check: if the kernel's own view is tighter, trust it.
    if let (Some(ms), Some(avail)) = (memorystatus, value.value) {
        if ms < avail {
            notes.push(format!(
                "kern.memorystatus_level reports less available ({} < {}); using it",
                note_bytes(ms),
                note_bytes(avail)
            ));
            value = Measured::estimate(ms, "kern.memorystatus_level × RAM (tighter than vm_statistics64)");
        } else if let Some(t) = total {
            let delta_pct = (ms - avail) as f64 / t as f64 * 100.0;
            if delta_pct > MEMORYSTATUS_TOLERANCE_PCT && s.host.os == OsKind::Macos {
                notes.push(format!(
                    "memorystatus counts {} more (compressible anonymous pages); not counted as headroom",
                    note_bytes(ms - avail)
                ));
            }
        }
    }
    Available {
        value,
        memorystatus,
        notes,
    }
}

// -----------------------------------------------------------------------------------------------------
// Margin & pressure
// -----------------------------------------------------------------------------------------------------

/// Computes the safety margin: `max(min_margin, pct × RAM)`, +boost when under pressure / swap growing.
pub fn safety_margin(total_ram: u64, pressured: bool, cfg: &HeadroomConfig) -> u64 {
    let pct = if cfg.margin_pct.is_finite() {
        cfg.margin_pct.max(0.0)
    } else {
        0.0
    };
    let base = cfg
        .margin_override
        .unwrap_or_else(|| cfg.min_margin.max((total_ram as f64 * pct / 100.0) as u64));
    let boost = if cfg.pressure_boost_pct.is_finite() {
        cfg.pressure_boost_pct.max(0.0)
    } else {
        0.0
    };
    if pressured {
        (base as f64 * (1.0 + boost / 100.0)) as u64
    } else {
        base
    }
}

/// Swap is considered growing when swap-outs exceed [`SWAP_GROWING_BYTES_PER_MIN`].
pub fn swap_growing(snapshot: &Snapshot) -> bool {
    snapshot
        .memory
        .swap_out_per_min
        .value
        .filter(|_| snapshot.memory.swap_out_per_min.quality.is_available())
        .map(|v| v > SWAP_GROWING_BYTES_PER_MIN)
        .unwrap_or(false)
}

/// History-based variant: swap used grew by more than [`SWAP_GROWING_BYTES_PER_MIN`] on average over the
/// last `window_ms` (needs ≥ 30 s of data).
pub fn swap_growing_in_history(history: &History, window_ms: u64) -> bool {
    let s = history.series(window_ms, |p| p.swap_used.map(|v| v as f64));
    let (Some(first), Some(last)) = (s.first(), s.last()) else {
        return false;
    };
    let span = last.0 - first.0;
    if span < 30.0 {
        return false;
    }
    (last.1 - first.1) / span * 60.0 > SWAP_GROWING_BYTES_PER_MIN as f64
}

// -----------------------------------------------------------------------------------------------------
// Reclaim
// -----------------------------------------------------------------------------------------------------

/// Whether a group may be suggested for reclaim (build daemons, orphans, idle model servers; never protected,
/// never oomtop itself). A build daemon that is visibly busy (mid-build) is skipped unless flagged idle.
pub fn is_reclaim_candidate(g: &Group) -> bool {
    // System groups are never suggested, even when a member looks orphaned (re-parented to launchd/init is
    // normal for system daemons).
    if g.protected || g.is_self || g.kind == GroupKind::System {
        return false;
    }
    if g.orphan {
        return true;
    }
    match g.kind {
        GroupKind::BuildDaemon => {
            let busy = g
                .totals
                .cpu_pct
                .value
                .filter(|_| g.totals.cpu_pct.quality.is_available())
                .map(|c| c >= BUSY_DAEMON_CPU_PCT)
                .unwrap_or(false);
            g.idle || !busy
        }
        GroupKind::ModelServer => g.idle,
        _ => false,
    }
}

/// Host facts needed to turn per-process memory into "RAM freed if stopped".
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReclaimModel {
    /// Compressor physical ÷ logical bytes (≈ 0.2–0.4); `None` = unknown → compressed share not counted.
    pub compressor_ratio: Option<f64>,
    /// Share (0..1) of a process's non-resident bytes that sit in the compressor rather than in swap.
    pub compressed_fraction: f64,
    /// GPU memory is host RAM (Apple Silicon, iGPU): GPU bytes freed count as RAM freed.
    pub unified_gpu: bool,
}

impl ReclaimModel {
    /// Derives the model from host memory: ratio = compressed / compressed_logical,
    /// fraction = compressed_logical / (compressed_logical + swap_used).
    pub fn from_snapshot(s: &Snapshot) -> Self {
        let ratio = compressor_ratio(s);
        let logical = s.memory.compressed_logical.usable().unwrap_or(0) as f64;
        let swap = s.memory.swap_used.usable().unwrap_or(0) as f64;
        let fraction = if logical + swap > 0.0 {
            logical / (logical + swap)
        } else if ratio.is_some() {
            1.0
        } else {
            0.0
        };
        ReclaimModel {
            compressor_ratio: ratio,
            compressed_fraction: fraction.clamp(0.0, 1.0),
            unified_gpu: s.host.unified_memory || s.accelerators.iter().any(|a| a.unified),
        }
    }
}

/// RAM freed if a process stops: resident + non-resident × compressor ratio (compressed share), capped by
/// the footprint. Always `Estimate` (SPEC §8.1). Assumes every non-resident byte is compressed; prefer
/// [`reclaim_gain_with`] with a [`ReclaimModel`] from the snapshot.
pub fn process_reclaim_gain(p: &Process, compressor_ratio: Option<f64>) -> Measured<u64> {
    reclaim_gain_with(
        p,
        &ReclaimModel {
            compressor_ratio,
            compressed_fraction: 1.0,
            unified_gpu: false,
        },
    )
}

fn avail(m: &Measured<u64>) -> Option<u64> {
    m.usable()
}

/// RAM freed if `p` stops = private resident (+ unified GPU buffers) + compressed share. Swap is separate
/// ([`group_swap_gain`]). Always `Estimate`.
pub fn reclaim_gain_with(p: &Process, model: &ReclaimModel) -> Measured<u64> {
    let resident = avail(&p.mem.resident);
    let footprint = avail(&p.mem.footprint_or_pss);
    let mut non_res = avail(&p.mem.non_resident_est).unwrap_or_else(|| match (footprint, resident) {
        (Some(f), Some(r)) => f.saturating_sub(r),
        _ => 0,
    });
    let (mut private, mut source) = match (resident, footprint) {
        // footprint/PSS excludes shared pages; resident caps what is physically present.
        (Some(r), Some(f)) => (r.min(f), String::from("min(resident, footprint)")),
        (Some(r), None) => (r, String::from("resident (incl. shared)")),
        (None, Some(f)) => (
            f.saturating_sub(non_res),
            String::from("footprint − non-resident"),
        ),
        (None, None) => return Measured::unavailable("reclaim_gain", "no memory data"),
    };
    // Unified GPU buffers inside the footprint but outside resident are wired, not compressed.
    if model.unified_gpu {
        if let Some(gpu) = avail(&p.mem.gpu) {
            let extra = gpu.min(non_res);
            private = private.saturating_add(extra);
            non_res -= extra;
            if extra > 0 {
                source.push_str(" + GPU");
            }
        }
    }
    let share = match (avail(&p.mem.compressed), model.compressor_ratio) {
        (Some(c), Some(ratio)) => {
            source.push_str(" + compressed × ratio");
            (c as f64 * ratio) as u64
        }
        (None, Some(ratio)) if non_res > 0 => {
            source.push_str(" + compressed share (est.)");
            (non_res as f64 * model.compressed_fraction.clamp(0.0, 1.0) * ratio) as u64
        }
        _ => 0,
    };
    let cap = footprint.unwrap_or(0).max(resident.unwrap_or(0));
    Measured::estimate(private.saturating_add(share).min(cap), source)
}

/// Reclaim gain for a group: Σ member gains, quality `Estimate`.
pub fn group_reclaim_gain(group: &Group, snapshot: &Snapshot) -> Measured<u64> {
    let model = ReclaimModel::from_snapshot(snapshot);
    group_reclaim_gain_by(group, &model, |id| snapshot.process(id))
}

/// [`group_reclaim_gain`] with a caller-supplied process lookup (attribution passes its pid index, so a
/// sample of N processes costs O(N), not O(N²)).
pub fn group_reclaim_gain_by<'a>(
    group: &Group,
    model: &ReclaimModel,
    lookup: impl Fn(crate::model::ProcId) -> Option<&'a Process>,
) -> Measured<u64> {
    let gains: Vec<Measured<u64>> = group
        .members
        .iter()
        .filter_map(|m| lookup(m.id))
        .map(|p| reclaim_gain_with(p, model))
        .collect();
    if gains.is_empty() {
        return Measured::unavailable("Σ member reclaim gain", "no member processes in snapshot");
    }
    sum_bytes(gains.iter(), "Σ member reclaim gain").into_estimate()
}

/// Swap freed if the group stops (Linux `SwapPss`; not attributable per process on macOS).
pub fn group_swap_gain(group: &Group, snapshot: &Snapshot) -> Measured<u64> {
    let swaps: Vec<&Measured<u64>> = group
        .members
        .iter()
        .filter_map(|m| snapshot.process(m.id))
        .map(|p| &p.mem.swapped)
        .collect();
    if swaps.is_empty() {
        return Measured::unavailable("Σ member swapped", "no member processes in snapshot");
    }
    sum_bytes(swaps, "Σ member swapped").into_estimate()
}

/// GPU memory freed if the group stops: group GPU totals, else Σ member GPU, else the GPU-resident models
/// of the group's model servers. `None` when unknown.
pub fn group_gpu_gain(group: &Group, snapshot: &Snapshot) -> Option<u64> {
    if let Some(v) = avail(&group.totals.gpu) {
        return Some(v);
    }
    let members: Vec<&Measured<u64>> = group
        .members
        .iter()
        .filter_map(|m| snapshot.process(m.id))
        .map(|p| &p.mem.gpu)
        .collect();
    if let Some(v) = sum_bytes(members, "Σ member gpu").value {
        return Some(v);
    }
    let models: u64 = snapshot
        .model_servers
        .iter()
        .filter(|ms| ms.group_id.as_deref() == Some(group.id.as_str()))
        .flat_map(|ms| ms.models.iter())
        .filter(|m| matches!(m.device, Device::Gpu | Device::Mixed))
        .map(|m| {
            m.weights_bytes
                .usable()
                .unwrap_or(0)
                .saturating_add(m.kv_bytes.usable().unwrap_or(0))
        })
        .fold(0u64, u64::saturating_add);
    (models > 0).then_some(models)
}

/// Physical ÷ logical bytes in the compressor, when both known.
pub fn compressor_ratio(snapshot: &Snapshot) -> Option<f64> {
    let phys = avail(&snapshot.memory.compressed)?;
    let logical = avail(&snapshot.memory.compressed_logical)?;
    if logical == 0 {
        None
    } else {
        Some((phys as f64 / logical as f64).clamp(0.0, 1.0))
    }
}

// -----------------------------------------------------------------------------------------------------
// GPU budget
// -----------------------------------------------------------------------------------------------------

/// Default Metal working-set estimate for unified memory when the collector could not read it.
pub fn unified_budget_estimate(total_ram: u64) -> u64 {
    let f = if total_ram > UNIFIED_BUDGET_LARGE_ABOVE {
        UNIFIED_BUDGET_LARGE_FRACTION
    } else {
        UNIFIED_BUDGET_SMALL_FRACTION
    };
    (total_ram as f64 * f) as u64
}

/// VRAM safety margin for a discrete GPU.
pub fn discrete_gpu_margin(vram: u64) -> u64 {
    DISCRETE_GPU_MIN_MARGIN.max((vram as f64 * DISCRETE_GPU_MARGIN_PCT / 100.0) as u64)
}

/// GPU headroom for one accelerator (SPEC §8.1 "GPU budget").
/// Unified: budget = Metal working-set limit (estimated from RAM when unreadable); discrete: VRAM total
/// (or a lower configured budget), minus a VRAM safety margin.
pub fn gpu_headroom(a: &Accelerator, total_ram: Option<u64>) -> GpuHeadroom {
    // A budget of 0 is never a real limit: `iogpu.wired_limit_mb = 0` means "OS default", so a collector that
    // passes it through must not make every GPU need fail.
    let configured = avail(&a.gpu_budget).filter(|b| *b > 0);
    let budget = if a.unified {
        if configured.is_some() {
            a.gpu_budget.clone()
        } else if let Some(t) = total_ram.filter(|t| *t > 0) {
            Measured::estimate(
                unified_budget_estimate(t),
                "default Metal working set (≈2/3–3/4 of RAM)",
            )
        } else {
            a.gpu_budget.clone()
        }
    } else {
        match (avail(&a.mem_total), configured) {
            (Some(t), Some(b)) if b < t => a.gpu_budget.clone(),
            (Some(_), _) => a.mem_total.clone(),
            (None, Some(_)) => a.gpu_budget.clone(),
            (None, None) => Measured::unavailable("gpu", "VRAM total unavailable"),
        }
    };
    let free = match (avail(&budget), avail(&a.mem_used)) {
        (Some(b), Some(u)) => Measured {
            value: Some(b.saturating_sub(u)),
            source: "gpu budget − in use".into(),
            quality: budget.quality.clone().worst(a.mem_used.quality.clone()),
        },
        (Some(_), None) => Measured::unavailable("gpu", "GPU memory in use unavailable"),
        _ => Measured::unavailable("gpu", "GPU budget unavailable"),
    };
    let margin = if a.unified {
        0
    } else {
        avail(&budget).map(discrete_gpu_margin).unwrap_or(0)
    };
    let headroom = free.usable().map(|f| sub_i64(f, margin));
    GpuHeadroom {
        accelerator_id: a.id.clone(),
        budget,
        in_use: a.mem_used.clone(),
        free,
        unified: a.unified,
        vendor: a.vendor,
        name: a.name.clone(),
        margin,
        headroom,
    }
}

// -----------------------------------------------------------------------------------------------------
// compute
// -----------------------------------------------------------------------------------------------------

/// Computes headroom for the snapshot. Groups should already carry `reclaim_gain`; missing gains are
/// computed on the fly.
pub fn compute(snapshot: &Snapshot, cfg: &HeadroomConfig) -> Headroom {
    let mem = &snapshot.memory;
    let total = avail(&mem.total).or((snapshot.host.mem_total > 0).then_some(snapshot.host.mem_total));
    let pressure = mem.pressure.value.filter(|_| mem.pressure.quality.is_available());
    let growing = swap_growing(snapshot);
    let pressured = growing || pressure.map(|p| p >= PressureLevel::Warn).unwrap_or(false);
    let margin = safety_margin(total.unwrap_or(0), pressured, cfg);
    let Available {
        value: available_now,
        memorystatus,
        mut notes,
    } = resolve_available(snapshot, total);
    let headroom = available_now.usable().map(|a| sub_i64(a, margin));
    if pressured {
        notes.push(format!(
            "safety margin +{:.0}% ({})",
            cfg.pressure_boost_pct,
            if growing {
                "swap growing"
            } else {
                "memory pressure"
            }
        ));
    }

    let candidates: Vec<&Group> = snapshot
        .groups
        .iter()
        .filter(|g| is_reclaim_candidate(g))
        .collect();
    let gains: Vec<Measured<u64>> = candidates
        .iter()
        .map(|g| {
            if g.reclaim_gain.is_available() {
                g.reclaim_gain.clone()
            } else {
                group_reclaim_gain(g, snapshot)
            }
        })
        .collect();
    let reclaimable = if gains.is_empty() {
        Measured::estimate(0, "no reclaim candidates")
    } else {
        sum_bytes(gains.iter(), "Σ reclaim_gain").into_estimate()
    };
    let swaps: Vec<Measured<u64>> = candidates
        .iter()
        .map(|g| {
            if g.swap_gain.is_available() {
                g.swap_gain.clone()
            } else {
                group_swap_gain(g, snapshot)
            }
        })
        .collect();
    let reclaimable_swap = if swaps.is_empty() {
        Measured::estimate(0, "no reclaim candidates")
    } else {
        sum_bytes(swaps.iter(), "Σ swap_gain").into_estimate()
    };
    let gpu = snapshot
        .accelerators
        .iter()
        .map(|a| gpu_headroom(a, total))
        .collect();
    Headroom {
        available_now,
        safety_margin: margin,
        headroom,
        pressure,
        swap_growing: growing,
        reclaimable,
        reclaimable_swap,
        gpu,
        as_of_ms: snapshot.taken_at_ms,
        total,
        memorystatus_available: memorystatus,
        margin_boosted: pressured,
        notes,
        unified_memory: snapshot.host.unified_memory || snapshot.accelerators.iter().any(|a| a.unified),
    }
}

/// `a − b` as i64 without wrapping (values beyond i64 saturate).
pub fn sub_i64(a: u64, b: u64) -> i64 {
    (a as i128 - b as i128).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

/// True when every input needed for headroom is exact.
pub fn is_exact(h: &Headroom) -> bool {
    h.available_now.quality == Quality::Exact
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Member, ProcId};

    #[test]
    fn margin_rules() {
        let cfg = HeadroomConfig::default();
        // 8 % of 24 GiB = 1.92 GiB > 1.5 GiB
        assert_eq!(
            safety_margin(24 * GIB, false, &cfg),
            (24.0 * GIB as f64 * 0.08) as u64
        );
        // 8 % of 8 GiB = 0.64 GiB < 1.5 GiB
        assert_eq!(safety_margin(8 * GIB, false, &cfg), 3 * GIB / 2);
        assert_eq!(
            safety_margin(8 * GIB, true, &cfg),
            (1.5 * 1.5 * GIB as f64) as u64
        );
        let o = HeadroomConfig {
            margin_override: Some(GIB),
            ..Default::default()
        };
        assert_eq!(safety_margin(64 * GIB, false, &o), GIB);
        assert_eq!(safety_margin(64 * GIB, true, &o), 3 * GIB / 2);
        let bad = HeadroomConfig {
            margin_pct: f64::NAN,
            pressure_boost_pct: -5.0,
            ..Default::default()
        };
        assert_eq!(safety_margin(64 * GIB, true, &bad), 3 * GIB / 2);
    }

    #[test]
    fn headroom_basic() {
        let mut s = Snapshot::default();
        s.memory.total = Measured::exact(24 * GIB, "t");
        s.memory.available = Measured::exact(10 * GIB, "a");
        let h = compute(&s, &HeadroomConfig::default());
        let margin = safety_margin(24 * GIB, false, &HeadroomConfig::default());
        assert_eq!(h.headroom, Some(10 * GIB as i64 - margin as i64));
        assert_eq!(h.reclaimable.value, Some(0));
        assert!(!h.margin_boosted);
    }

    #[test]
    fn macos_formula_does_not_double_count_speculative() {
        // Numbers from fixtures/macos/m5-air-agents.json (16 KiB pages).
        let p = MacVmPages {
            free_count: 64_893,
            speculative_count: 52_052,
            purgeable_count: 3_335,
            external_page_count: 404_144,
        };
        let a = macos_available_now(&p, 16_384);
        assert_eq!(a.value, Some((64_893 - 52_052 + 404_144 + 3_335) * 16_384));
        assert_eq!(a.quality, Quality::Exact);
        assert!(!macos_available_now(&p, 0).is_available());
    }

    #[test]
    fn linux_cgroup_cap() {
        let m = Measured::exact(10 * GIB, "/proc/meminfo:MemAvailable");
        assert_eq!(linux_available_now(&m, None, None).value, Some(10 * GIB));
        let c = linux_available_now(&m, Some(8 * GIB), Some(6 * GIB));
        assert_eq!(c.value, Some(2 * GIB));
        assert_eq!(c.quality, Quality::Exact);
        let c = linux_available_now(&m, Some(4 * GIB), None);
        assert_eq!(c.value, Some(4 * GIB));
        assert_eq!(c.quality, Quality::Estimate);
        assert_eq!(
            linux_available_now(&m, Some(64 * GIB), Some(GIB)).value,
            Some(10 * GIB)
        );
        let u: Measured<u64> = Measured::unavailable("/proc/meminfo", "missing");
        assert!(!linux_available_now(&u, Some(GIB), None).is_available());
    }

    #[test]
    fn memorystatus_cross_check_takes_tighter_value() {
        let mut s = Snapshot::default();
        s.host.os = OsKind::Macos;
        s.memory.total = Measured::exact(24 * GIB, "hw.memsize");
        s.memory.available = Measured::exact(8 * GIB, "vm");
        s.memory.memorystatus_level = Measured::exact(20, "kern.memorystatus_level");
        let h = compute(&s, &HeadroomConfig::default());
        assert_eq!(h.available_now.value, Some(memorystatus_bytes(20, 24 * GIB)));
        assert_eq!(h.available_now.quality, Quality::Estimate);
        assert_eq!(
            h.notes,
            ["kern.memorystatus_level reports less available (4.8 GiB < 8.0 GiB); using it"]
        );
        // Looser memorystatus (counts compressible anon pages) is not used.
        s.memory.memorystatus_level = Measured::exact(77, "kern.memorystatus_level");
        let h = compute(&s, &HeadroomConfig::default());
        assert_eq!(h.available_now.value, Some(8 * GIB));
        // Notes use the same IEC units as every headline, never the compact table form ("10.5G").
        assert_eq!(
            h.notes,
            ["memorystatus counts 10.5 GiB more (compressible anonymous pages); not counted as headroom"]
        );
        assert_eq!(h.memorystatus_available, Some(memorystatus_bytes(77, 24 * GIB)));
    }

    #[test]
    fn fallbacks_and_cgroup_limit() {
        let mut s = Snapshot::default();
        s.memory.total = Measured::exact(16 * GIB, "t");
        s.memory.free = Measured::exact(GIB, "f");
        s.memory.cached = Measured::exact(3 * GIB, "c");
        let h = compute(&s, &HeadroomConfig::default());
        assert_eq!(h.available_now.value, Some(4 * GIB));
        assert_eq!(h.available_now.quality, Quality::Estimate);
        s.memory.own_cgroup_limit = Measured::exact(2 * GIB, "cgroup");
        let h = compute(&s, &HeadroomConfig::default());
        assert_eq!(h.available_now.value, Some(2 * GIB));
        let empty = compute(&Snapshot::default(), &HeadroomConfig::default());
        assert_eq!(empty.headroom, None);
    }

    #[test]
    fn pressure_boosts_margin() {
        let mut s = Snapshot::default();
        s.memory.total = Measured::exact(24 * GIB, "t");
        s.memory.available = Measured::exact(10 * GIB, "a");
        s.memory.swap_out_per_min = Measured::exact(200 * MIB, "vm");
        let h = compute(&s, &HeadroomConfig::default());
        assert!(h.swap_growing && h.margin_boosted);
        assert_eq!(
            h.safety_margin,
            safety_margin(24 * GIB, true, &HeadroomConfig::default())
        );
        s.memory.swap_out_per_min = Measured::exact(0, "vm");
        s.memory.pressure = Measured::exact(PressureLevel::Warn, "p");
        assert!(compute(&s, &HeadroomConfig::default()).margin_boosted);
    }

    fn proc(pid: u32, resident: u64, footprint: u64) -> Process {
        let mut p = Process {
            id: ProcId::new(pid, 1),
            ..Default::default()
        };
        p.mem.resident = Measured::exact(resident, "r");
        p.mem.footprint_or_pss = Measured::exact(footprint, "f");
        p.mem.non_resident_est = Measured::estimate(footprint.saturating_sub(resident), "f−r");
        p
    }

    #[test]
    fn reclaim_gain_rules() {
        // JVM daemon: footprint 3.5 GiB, resident 3.0 GiB, 0.5 GiB non-resident, ratio 0.25, 80 % compressed.
        let p = proc(1, 3 * GIB, 7 * GIB / 2);
        let m = ReclaimModel {
            compressor_ratio: Some(0.25),
            compressed_fraction: 0.8,
            unified_gpu: false,
        };
        let g = reclaim_gain_with(&p, &m);
        assert_eq!(g.value, Some(3 * GIB + (GIB as f64 / 2.0 * 0.8 * 0.25) as u64));
        assert_eq!(g.quality, Quality::Estimate);
        // Unknown ratio → compressed share not counted (conservative).
        let g = reclaim_gain_with(&p, &ReclaimModel::default());
        assert_eq!(g.value, Some(3 * GIB));
        // Shared-heavy process: resident > footprint → footprint.
        let p2 = proc(2, 2 * GIB, GIB);
        assert_eq!(process_reclaim_gain(&p2, Some(0.3)).value, Some(GIB));
        // Unified GPU buffers inside footprint count fully.
        let mut p3 = proc(3, GIB, 10 * GIB);
        p3.mem.gpu = Measured::exact(8 * GIB, "gpu");
        let g = reclaim_gain_with(
            &p3,
            &ReclaimModel {
                compressor_ratio: Some(0.5),
                compressed_fraction: 1.0,
                unified_gpu: true,
            },
        );
        assert_eq!(g.value, Some(9 * GIB + GIB / 2));
        assert!(!process_reclaim_gain(&Process::default(), None).is_available());
    }

    #[test]
    fn candidates_skip_busy_daemons_and_protected() {
        let mut g = Group {
            kind: GroupKind::BuildDaemon,
            ..Default::default()
        };
        assert!(is_reclaim_candidate(&g));
        g.totals.cpu_pct = Measured::exact(85.0, "cpu");
        assert!(!is_reclaim_candidate(&g));
        g.idle = true;
        assert!(is_reclaim_candidate(&g));
        g.protected = true;
        assert!(!is_reclaim_candidate(&g));
        let m = Group {
            kind: GroupKind::ModelServer,
            ..Default::default()
        };
        assert!(!is_reclaim_candidate(&m));
        let o = Group {
            kind: GroupKind::App,
            orphan: true,
            ..Default::default()
        };
        assert!(is_reclaim_candidate(&o));
        let me = Group {
            kind: GroupKind::BuildDaemon,
            is_self: true,
            ..Default::default()
        };
        assert!(!is_reclaim_candidate(&me));
    }

    #[test]
    fn group_gains_and_swap() {
        let mut s = Snapshot::default();
        let mut p = proc(10, 2 * GIB, 2 * GIB);
        p.mem.swapped = Measured::exact(300 * MIB, "SwapPss");
        s.processes.push(p);
        s.processes.push(proc(11, GIB, GIB));
        let g = Group {
            id: "daemon:gradle".into(),
            kind: GroupKind::BuildDaemon,
            members: vec![
                Member {
                    id: ProcId::new(10, 1),
                    ..Default::default()
                },
                Member {
                    id: ProcId::new(11, 1),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(group_reclaim_gain(&g, &s).value, Some(3 * GIB));
        let sw = group_swap_gain(&g, &s);
        assert_eq!(sw.value, Some(300 * MIB));
        assert_eq!(sw.quality, Quality::Estimate);
        s.groups.push(g);
        let h = compute(&s, &HeadroomConfig::default());
        assert_eq!(h.reclaimable.value, Some(3 * GIB));
        assert_eq!(h.reclaimable_swap.value, Some(300 * MIB));
        let lonely = Group::default();
        assert!(!group_reclaim_gain(&lonely, &s).is_available());
    }

    #[test]
    fn gpu_budgets() {
        let mut apple = Accelerator {
            id: "gpu0".into(),
            vendor: GpuVendor::Apple,
            unified: true,
            mem_used: Measured::exact(2 * GIB, "ioaccel"),
            ..Default::default()
        };
        let h = gpu_headroom(&apple, Some(24 * GIB));
        assert_eq!(h.budget.value, Some(16 * GIB));
        assert_eq!(h.budget.quality, Quality::Estimate);
        assert_eq!(h.free.value, Some(14 * GIB));
        assert_eq!(h.margin, 0);
        apple.gpu_budget = Measured::exact(20 * GIB, "iogpu.wired_limit_mb");
        assert_eq!(gpu_headroom(&apple, Some(24 * GIB)).free.value, Some(18 * GIB));
        assert_eq!(unified_budget_estimate(64 * GIB), 48 * GIB);

        let nv = Accelerator {
            id: "gpu1".into(),
            vendor: GpuVendor::Nvidia,
            unified: false,
            mem_total: Measured::exact(24 * GIB, "nvml"),
            mem_used: Measured::exact(20 * GIB, "nvml"),
            ..Default::default()
        };
        let h = gpu_headroom(&nv, Some(64 * GIB));
        assert_eq!(h.free.value, Some(4 * GIB));
        assert_eq!(h.margin, discrete_gpu_margin(24 * GIB));
        assert_eq!(h.headroom, Some(4 * GIB as i64 - h.margin as i64));
        let blind = Accelerator {
            unified: false,
            ..Default::default()
        };
        assert!(!gpu_headroom(&blind, None).free.is_available());
    }

    #[test]
    fn zero_gpu_budget_is_not_a_limit() {
        // iogpu.wired_limit_mb = 0 ("OS default") passed through as an exact 0 must not zero the budget.
        let apple = Accelerator {
            id: "gpu0".into(),
            unified: true,
            mem_used: Measured::exact(GIB, "ioaccel"),
            gpu_budget: Measured::exact(0, "iogpu.wired_limit_mb"),
            ..Default::default()
        };
        let h = gpu_headroom(&apple, Some(24 * GIB));
        assert_eq!(h.budget.value, Some(16 * GIB));
        assert_eq!(h.budget.quality, Quality::Estimate);
        assert_eq!(h.free.value, Some(15 * GIB));
        let nv = Accelerator {
            unified: false,
            mem_total: Measured::exact(24 * GIB, "nvml"),
            mem_used: Measured::exact(4 * GIB, "nvml"),
            gpu_budget: Measured::exact(0, "config"),
            ..Default::default()
        };
        assert_eq!(gpu_headroom(&nv, None).budget.value, Some(24 * GIB));
    }

    #[test]
    fn inconsistent_measurements_are_not_used() {
        // value present but quality unavailable (stale) must not feed the free + cached fallback.
        let stale = |v: u64| Measured {
            value: Some(v),
            source: "x".into(),
            quality: Quality::Unavailable("stale".into()),
        };
        let mut s = Snapshot::default();
        s.memory.total = Measured::exact(16 * GIB, "t");
        s.memory.free = stale(8 * GIB);
        s.memory.cached = Measured::exact(GIB, "c");
        let h = compute(&s, &HeadroomConfig::default());
        assert_eq!(h.available_now.value, None);
        assert_eq!(h.headroom, None);
        // Stale swap / logical sizes do not feed the reclaim model.
        s.memory.compressed = Measured::exact(GIB, "c");
        s.memory.compressed_logical = Measured::exact(4 * GIB, "c");
        s.memory.swap_used = stale(12 * GIB);
        let m = ReclaimModel::from_snapshot(&s);
        assert_eq!(m.compressed_fraction, 1.0);
        assert_eq!(m.compressor_ratio, Some(0.25));
    }

    #[test]
    fn system_groups_are_never_candidates() {
        let g = Group {
            kind: GroupKind::System,
            orphan: true,
            ..Default::default()
        };
        assert!(!is_reclaim_candidate(&g));
    }

    #[test]
    fn headroom_never_wraps() {
        assert_eq!(sub_i64(u64::MAX, 0), i64::MAX);
        assert_eq!(sub_i64(0, u64::MAX), i64::MIN);
        assert_eq!(sub_i64(5, 7), -2);
        let mut s = Snapshot::default();
        s.memory.available = Measured::exact(u64::MAX, "a");
        assert_eq!(compute(&s, &HeadroomConfig::default()).headroom, Some(i64::MAX));
        s.host.unified_memory = true;
        assert!(compute(&s, &HeadroomConfig::default()).unified_memory);
    }

    #[test]
    fn history_swap_growth() {
        use crate::history::HistoryPoint;
        let mut h = History::default();
        for i in 0..20u64 {
            h.push(HistoryPoint {
                t_ms: i * 5_000,
                swap_used: Some(GIB + i * 10 * MIB),
                ..Default::default()
            });
        }
        // 10 MiB per 5 s = 120 MiB/min
        assert!(swap_growing_in_history(&h, 120_000));
        assert!(!swap_growing_in_history(&History::default(), 120_000));
    }
}

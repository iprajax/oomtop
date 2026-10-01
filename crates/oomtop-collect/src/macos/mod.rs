//! macOS Sources (I/O only; decoding lives in `decode::macos` and the pure [`extras`] decoder). SPEC §6
//! `macos/`: libproc (`proc_pid_rusage`, `proc_pidinfo`, `proc_pidpath`), sysctl (`KERN_PROCARGS2`,
//! memorystatus, iogpu, `vm.swapusage`), `host_statistics64`, responsible pid (resolved via `dlsym`), and —
//! in the host source — the thermal-pressure notify key, `NSProcessInfo` (Low Power Mode, thermal state),
//! IOKit power sources (battery, adapter), `IOAccelerator` GPU memory/utilization, IOReport CPU/GPU
//! residency + energy (`dlsym`, [`ioreport`]) with the `pmgr` DVFS tables, and JetsamEvent reports
//! ([`jetsam`]).
//!
//! Every call degrades to `unavailable`/`partial` on failure; nothing here panics on missing OS data. No
//! root is needed for anything; other users' processes get identity only (`kinfo_proc`).

mod cf;
pub use crate::decode::macos_extras as extras;
pub mod groundtruth;
pub mod ioreport;
pub mod jetsam;
pub mod sensors;

#[cfg(test)]
mod replay_tests;

use crate::raw::{names, MacHostRaw, MacProcRaw, MacProcsRaw, MacRusage, MacSwap, RawPayload, RawSample};
use crate::{now_ms, Source, SourceError};
use oomtop_core::redact::{default_allowlist, markers_from_env};
use oomtop_core::Markers;
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::mem::{size_of, MaybeUninit};
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct MachTimebase {
    numer: u32,
    denom: u32,
}

extern "C" {
    fn mach_host_self() -> libc::mach_port_t;
    static mach_task_self_: libc::mach_port_t;
    fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
}

/// Reads an integer sysctl (4 or 8 bytes).
pub fn sysctl_int(name: &str) -> Option<i64> {
    let c = CString::new(name).ok()?;
    let mut buf = [0u8; 8];
    let mut len: libc::size_t = buf.len();
    // SAFETY: buffer and length are valid for the duration of the call.
    let r = unsafe {
        libc::sysctlbyname(
            c.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    match len {
        4 => Some(i32::from_ne_bytes(buf[..4].try_into().ok()?) as i64),
        8 => Some(i64::from_ne_bytes(buf)),
        _ => None,
    }
}

/// Reads a string sysctl.
pub fn sysctl_string(name: &str) -> Option<String> {
    let c = CString::new(name).ok()?;
    let mut len: libc::size_t = 0;
    // SAFETY: size query with null buffer.
    if unsafe {
        libc::sysctlbyname(
            c.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || len == 0
    {
        return None;
    }
    let mut buf = vec![0u8; len];
    // SAFETY: buffer of `len` bytes.
    if unsafe {
        libc::sysctlbyname(
            c.as_ptr(),
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(len);
    let s = CStr::from_bytes_until_nul(&buf)
        .ok()?
        .to_string_lossy()
        .into_owned();
    Some(s)
}

fn sysctl_struct<T: Copy>(name: &str) -> Option<T> {
    let c = CString::new(name).ok()?;
    let mut v = MaybeUninit::<T>::zeroed();
    let mut len: libc::size_t = size_of::<T>();
    // SAFETY: T is plain-old-data sized buffer.
    let r = unsafe {
        libc::sysctlbyname(
            c.as_ptr(),
            v.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    // SAFETY: zero-initialized and (partially) written by the kernel; T is POD.
    (r == 0 && len == size_of::<T>()).then(|| unsafe { v.assume_init() })
}

/// `struct xsw_usage` (`<sys/sysctl.h>`): `xsu_encrypted` is a `boolean_t` (a 4-byte `int`), not a C `bool`.
/// Verified with `offsetof` on macOS 26 / arm64: size 32, `xsu_encrypted` at offset 28.
#[repr(C)]
#[derive(Clone, Copy)]
struct XswUsage {
    total: u64,
    avail: u64,
    used: u64,
    pagesize: u32,
    encrypted: u32,
}

/// `kern.boottime` → ms since epoch; `None` for a nonsensical (negative / out-of-range) value.
fn boot_time_ms(sec: i64, usec: i64) -> Option<u64> {
    if sec <= 0 || !(0..1_000_000).contains(&usec) {
        return None;
    }
    (sec as u64)
        .checked_mul(1000)
        .and_then(|ms| ms.checked_add(usec as u64 / 1000))
}

pub fn timebase() -> (u32, u32) {
    let mut tb = MachTimebase::default();
    // SAFETY: valid out pointer.
    unsafe { mach_timebase_info(&mut tb) };
    if tb.denom == 0 {
        (1, 1)
    } else {
        (tb.numer, tb.denom)
    }
}

/// Which optional host readers run (all on by default).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostExtras {
    /// Thermal-pressure notify key + NSProcessInfo (Low Power Mode, thermal state).
    pub thermal: bool,
    /// IOKit power sources (battery %, AC, adapter watts).
    pub power: bool,
    /// IOAccelerator PerformanceStatistics (host GPU memory in use, utilization).
    pub ioaccel: bool,
    /// IOReport residency/energy + pmgr DVFS tables.
    pub ioreport: bool,
    /// JetsamEvent report listing.
    pub jetsam: bool,
}

impl Default for HostExtras {
    fn default() -> Self {
        HostExtras {
            thermal: true,
            power: true,
            ioaccel: true,
            ioreport: true,
            jetsam: true,
        }
    }
}

impl HostExtras {
    /// Only the core memory/CPU sysctls.
    pub fn none() -> Self {
        HostExtras {
            thermal: false,
            power: false,
            ioaccel: false,
            ioreport: false,
            jetsam: false,
        }
    }
}

/// IOReport subscription lifecycle: created on a helper thread on first use, then owned by the source.
enum IoReportSlot {
    NotStarted,
    Pending(std::sync::mpsc::Receiver<Result<ioreport::IoReport, String>>),
    Ready(ioreport::IoReport),
    Failed(String),
}

impl IoReportSlot {
    /// Returns the live subscription, or why there is none yet / at all.
    fn poll(&mut self) -> Result<&mut ioreport::IoReport, String> {
        if let IoReportSlot::NotStarted = self {
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("oomtop-ioreport-init".into())
                .spawn(move || {
                    // Take the baseline sample here too, so the very next read already has a delta
                    // (one-shot commands sample twice, 250 ms apart).
                    let r = ioreport::IoReport::new().map(|mut r| {
                        let _ = r.delta();
                        r
                    });
                    let _ = tx.send(r);
                });
            *self = match spawned {
                Ok(_) => IoReportSlot::Pending(rx),
                Err(e) => IoReportSlot::Failed(format!("init thread: {e}")),
            };
        }
        if let IoReportSlot::Pending(rx) = self {
            match rx.try_recv() {
                Ok(Ok(r)) => *self = IoReportSlot::Ready(r),
                Ok(Err(e)) => *self = IoReportSlot::Failed(e),
                Err(std::sync::mpsc::TryRecvError::Empty) => return Err("initializing".into()),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    *self = IoReportSlot::Failed("init thread exited".into())
                }
            }
        }
        match self {
            IoReportSlot::Ready(r) => Ok(r),
            IoReportSlot::Failed(e) => Err(e.clone()),
            _ => Err("initializing".into()),
        }
    }
}

/// Free bytes (available to unprivileged users) on the volume that holds the swap files.
fn swap_volume_free() -> Option<u64> {
    for path in [c"/System/Volumes/VM", c"/"] {
        // SAFETY: statfs fills a zeroed struct for a valid NUL-terminated path.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statfs(path.as_ptr(), &mut st) } == 0 {
            return Some((st.f_bavail as u64).saturating_mul(st.f_bsize as u64));
        }
    }
    None
}

/// Host-level source: sysctls, `host_statistics64`, swap usage, CPU ticks, load average, plus the
/// [`HostExtras`] readers (thermal, power, GPU, IOReport, jetsam) within the same ≤ 50 ms budget.
pub struct MacHostSource {
    extras: HostExtras,
    notify: Option<sensors::ThermalNotify>,
    process_info: Option<sensors::ProcessInfo>,
    /// IOReport subscription (created off the sampling path: subscribing takes ~140 ms once).
    ioreport: IoReportSlot,
    dvfs: Option<Result<std::collections::BTreeMap<String, Vec<u32>>, String>>,
    jetsam: jetsam::JetsamScanner,
    /// Last power-source reading (battery %, AC, adapter watts change slowly): re-used for
    /// [`POWER_REFRESH`] instead of querying IOKit on every host sample (SPEC §14).
    power_cache: Option<(Instant, MacHostRaw)>,
    /// Last IOReport reading that had a delta (residency/energy averaged over its window), re-used for
    /// [`IOREPORT_REFRESH`] (SPEC §14).
    ioreport_cache: Option<(Instant, MacHostRaw)>,
    /// Cluster type per logical CPU from the IOKit device tree, read once (it never changes).
    core_clusters: Option<Vec<String>>,
}

/// How long a power-source reading is re-used.
pub const POWER_REFRESH: Duration = Duration::from_secs(10);

/// How long an IOReport reading is re-used. Sampling IOReport is the host source's most expensive step
/// (one IOKit call that copies every subscribed channel); at the default 2 s refresh this reads it every
/// other sample, so power, clusters and GPU residency are averages over ~4 s windows (SPEC §6.2 "expensive
/// reads every 5–10 s"), refreshed every 4 s. A reading without a delta (the first one) is never re-used,
/// so one-shot commands still get their second-sample delta.
pub const IOREPORT_REFRESH: Duration = Duration::from_millis(3_900);

impl std::fmt::Debug for MacHostSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacHostSource")
            .field("extras", &self.extras)
            .finish()
    }
}

impl Default for MacHostSource {
    fn default() -> Self {
        Self::new()
    }
}

impl MacHostSource {
    pub fn new() -> Self {
        Self::with_extras(HostExtras::default())
    }

    pub fn with_extras(extras: HostExtras) -> Self {
        MacHostSource {
            extras,
            notify: extras.thermal.then(sensors::ThermalNotify::new),
            process_info: if extras.thermal {
                sensors::ProcessInfo::new()
            } else {
                None
            },
            ioreport: IoReportSlot::NotStarted,
            dvfs: None,
            jetsam: jetsam::JetsamScanner::default(),
            power_cache: None,
            ioreport_cache: None,
            core_clusters: None,
        }
    }

    /// Optional readers, in cost order; each runs only while budget remains.
    fn read_extras(&mut self, h: &mut MacHostRaw, start: Instant, budget: Duration) {
        use extras::keys;
        let over = |part: &str, h: &mut MacHostRaw| {
            h.sysctl_str.insert(
                format!("{}{part}", keys::UNAVAILABLE_PREFIX),
                "skipped: sample time budget spent".into(),
            );
        };
        if self.extras.thermal {
            // Cheap (a notify-state read + two ObjC messages, ~10 µs): always runs.
            match &self.notify {
                Some(n) => sensors::read_thermal(h, n, self.process_info.as_ref()),
                None => {
                    h.sysctl_str.insert(
                        format!("{}{}", keys::UNAVAILABLE_PREFIX, keys::PART_THERMAL),
                        "thermal notify key not registered".into(),
                    );
                }
            }
        }
        if self.extras.power {
            let fresh = self
                .power_cache
                .as_ref()
                .filter(|(at, _)| at.elapsed() < POWER_REFRESH)
                .map(|(_, p)| p.clone());
            match fresh {
                Some(p) => merge_raw(h, p),
                None if start.elapsed() < budget => {
                    let mut p = MacHostRaw::default();
                    sensors::read_power(&mut p);
                    self.power_cache = Some((Instant::now(), p.clone()));
                    merge_raw(h, p);
                }
                None => over(keys::PART_POWER, h),
            }
        }
        if self.extras.ioaccel {
            if start.elapsed() < budget {
                sensors::read_ioaccel(h);
            } else {
                over(keys::PART_IOACCEL, h);
            }
        }
        if self.extras.ioreport {
            let fresh = self
                .ioreport_cache
                .as_ref()
                .filter(|(at, _)| at.elapsed() < IOREPORT_REFRESH)
                .map(|(_, r)| r.clone());
            match fresh {
                Some(r) => merge_raw(h, r),
                None if start.elapsed() < budget => {
                    let mut r = MacHostRaw::default();
                    self.read_ioreport(&mut r);
                    let has_delta = r.sysctl.contains_key(keys::IOREPORT_WINDOW_US);
                    self.ioreport_cache = has_delta.then(|| (Instant::now(), r.clone()));
                    merge_raw(h, r);
                }
                None => over(keys::PART_IOREPORT, h),
            }
        }
        if self.extras.jetsam {
            let left = budget.saturating_sub(start.elapsed());
            match self.jetsam.scan(left) {
                Ok(recs) => {
                    for (file, r) in recs {
                        if let Ok(j) = serde_json::to_string(&r) {
                            h.sysctl_str.insert(format!("{}{file}", keys::JETSAM_PREFIX), j);
                        }
                    }
                }
                Err(e) => {
                    h.sysctl_str
                        .insert(format!("{}{}", keys::UNAVAILABLE_PREFIX, keys::PART_JETSAM), e);
                }
            }
        }
    }

    fn read_ioreport(&mut self, h: &mut MacHostRaw) {
        use extras::keys;
        let dvfs = self.dvfs.get_or_insert_with(ioreport::dvfs_tables);
        match dvfs {
            Ok(t) => {
                for (name, mhz) in t.iter() {
                    h.sysctl_str.insert(
                        format!("{}{name}", keys::DVFS_PREFIX),
                        mhz.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(","),
                    );
                }
            }
            Err(e) => {
                h.sysctl_str.insert(
                    format!("{}{}", keys::UNAVAILABLE_PREFIX, keys::PART_DVFS),
                    e.clone(),
                );
            }
        }
        let why = format!("{}{}", keys::UNAVAILABLE_PREFIX, keys::PART_IOREPORT);
        let r = match self.ioreport.poll() {
            Ok(r) => r,
            Err(e) => {
                h.sysctl_str.insert(why, e);
                return;
            }
        };
        match r.delta() {
            Ok(Some(d)) => {
                h.sysctl
                    .insert(keys::IOREPORT_WINDOW_US.into(), d.window_us as i64);
                for (name, nj) in &d.energy_nj {
                    if keys::IOREPORT_ENERGY_KEEP.contains(&name.as_str()) {
                        h.sysctl
                            .insert(format!("{}{name}", keys::IOREPORT_ENERGY_PREFIX), *nj);
                    }
                }
                for (ch, states) in &d.residency {
                    if extras::dvfs_table_for(ch).is_some() {
                        h.sysctl_str.insert(
                            format!("{}{ch}", keys::IOREPORT_RESIDENCY_PREFIX),
                            extras::encode_residency(states),
                        );
                    }
                }
            }
            Ok(None) => {
                h.sysctl_str.insert(why, "needs two samples".into());
            }
            Err(e) => {
                h.sysctl_str.insert(why, e);
            }
        }
    }
}

/// Adds a partial reading's map entries to `h`.
fn merge_raw(h: &mut MacHostRaw, p: MacHostRaw) {
    h.sysctl.extend(p.sysctl);
    h.sysctl_str.extend(p.sysctl_str);
}

/// Integer host sysctls: `(name, required)`. A missing required key marks `macos.host` partial.
const HOST_INT_SYSCTLS: &[(&str, bool)] = &[
    ("hw.memsize", true),
    ("vm.pagesize", true),
    ("hw.ncpu", true),
    ("kern.memorystatus_level", true),
    ("kern.memorystatus_vm_pressure_level", true),
    // Apple Silicon only.
    ("hw.perflevel0.logicalcpu", false),
    ("hw.perflevel1.logicalcpu", false),
    ("iogpu.wired_limit_mb", false),
];

fn vm_stats() -> Option<libc::vm_statistics64> {
    let mut stats = MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count =
        (size_of::<libc::vm_statistics64>() / size_of::<libc::integer_t>()) as libc::mach_msg_type_number_t;
    // SAFETY: out buffer sized per count; host port from mach_host_self.
    let r = unsafe {
        libc::host_statistics64(
            mach_host_self(),
            libc::HOST_VM_INFO64,
            stats.as_mut_ptr() as *mut libc::integer_t,
            &mut count,
        )
    };
    // SAFETY: zeroed + filled by the kernel.
    (r == libc::KERN_SUCCESS).then(|| unsafe { stats.assume_init() })
}

fn cpu_ticks() -> Option<[u64; 4]> {
    let mut info = MaybeUninit::<libc::host_cpu_load_info>::zeroed();
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: as above.
    let r = unsafe {
        libc::host_statistics(
            mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            info.as_mut_ptr() as *mut libc::integer_t,
            &mut count,
        )
    };
    if r != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: filled by the kernel.
    let t = unsafe { info.assume_init() }.cpu_ticks;
    // CPU_STATE_USER=0, SYSTEM=1, IDLE=2, NICE=3
    Some([t[0] as u64, t[1] as u64, t[2] as u64, t[3] as u64])
}

/// Cluster type ("E", "P", …) per logical CPU, from the `cpuN` IOPlatformDevice nodes of the IOKit device
/// tree (`cluster-type` data + `logical-cpu-id`). Verified on an M5 Air: cpu0–5 are "E", cpu6–9 "P", so
/// perflevel order (P first) is *not* CPU-number order. Entries stay empty when a node is missing; the
/// decoder then reports the kind as unknown instead of guessing.
fn core_clusters(ncpu: usize) -> Vec<String> {
    let mut out = vec![String::new(); ncpu];
    for i in 0..ncpu {
        for dev in cf::services(&format!("cpu{i}"), true) {
            let kind = cf::registry_property(&dev, "cluster-type")
                .and_then(|c| cf::data(c.as_ptr()))
                .map(|b| {
                    String::from_utf8_lossy(&b)
                        .trim_end_matches('\0')
                        .trim()
                        .to_string()
                });
            let Some(kind) = kind.filter(|k| !k.is_empty()) else {
                continue;
            };
            let id = cf::registry_property(&dev, "logical-cpu-id")
                .and_then(|c| cf::int(c.as_ptr()))
                .map(|v| v as usize)
                .unwrap_or(i);
            if let Some(slot) = out.get_mut(id) {
                *slot = kind;
            }
            break;
        }
    }
    out
}

/// Per-logical-CPU ticks via `host_processor_info(PROCESSOR_CPU_LOAD_INFO)`; the kernel-allocated array is
/// released with `vm_deallocate` before returning.
fn per_cpu_ticks() -> Vec<[u64; 4]> {
    let mut ncpu: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut count: libc::mach_msg_type_number_t = 0;
    // SAFETY: out-pointers are valid; on success the kernel maps `count` integers at `info`.
    let r = unsafe {
        libc::host_processor_info(
            mach_host_self(),
            libc::PROCESSOR_CPU_LOAD_INFO,
            &mut ncpu,
            &mut info,
            &mut count,
        )
    };
    if r != libc::KERN_SUCCESS || info.is_null() {
        return Vec::new();
    }
    let per = libc::CPU_STATE_MAX as usize;
    let n = (ncpu as usize).min(count as usize / per);
    // SAFETY: the kernel returned `count` integer_t values at `info`; we read at most n*per of them.
    let slice = unsafe { std::slice::from_raw_parts(info as *const libc::integer_t, n * per) };
    let out = slice
        .chunks_exact(per)
        // CPU_STATE_USER=0, SYSTEM=1, IDLE=2, NICE=3 (unsigned tick counters).
        .map(|c| {
            [
                c[0] as u32 as u64,
                c[1] as u32 as u64,
                c[2] as u32 as u64,
                c[3] as u32 as u64,
            ]
        })
        .collect();
    // SAFETY: releases exactly the region host_processor_info allocated in our task.
    unsafe {
        libc::vm_deallocate(
            mach_task_self_,
            info as libc::vm_address_t,
            count as libc::vm_size_t * size_of::<libc::integer_t>() as libc::vm_size_t,
        );
    }
    out
}

impl Source for MacHostSource {
    fn name(&self) -> &'static str {
        names::MACOS_HOST
    }

    fn read(&mut self, budget: Duration) -> Result<RawSample, SourceError> {
        let start = Instant::now();
        let mut h = MacHostRaw::default();
        for (k, required) in HOST_INT_SYSCTLS {
            match sysctl_int(k) {
                Some(v) => {
                    h.sysctl.insert(k.to_string(), v);
                }
                // Optional keys (absent on Intel Macs) are simply left out; the decoder reports them
                // per value (e.g. the GPU budget falls back to an estimate) instead of degrading the source.
                None if *required => {
                    h.errors.insert(k.to_string(), "unreadable".into());
                }
                None => {}
            }
        }
        for k in [
            "hw.model",
            "machdep.cpu.brand_string",
            "kern.osproductversion",
            "kern.osversion",
            "kern.hostname",
            "hw.perflevel0.name",
            "hw.perflevel1.name",
        ] {
            if let Some(v) = sysctl_string(k) {
                h.sysctl_str.insert(k.to_string(), v);
            }
        }
        if let Some(s) = sysctl_struct::<XswUsage>("vm.swapusage") {
            let _ = s.pagesize;
            h.swap = Some(MacSwap {
                total: s.total,
                avail: s.avail,
                used: s.used,
                encrypted: s.encrypted != 0,
            });
        } else {
            h.errors.insert("vm.swapusage".into(), "unreadable".into());
        }
        h.swap_volume_free = swap_volume_free();
        if let Some(tv) = sysctl_struct::<libc::timeval>("kern.boottime") {
            h.boot_time_ms = boot_time_ms(tv.tv_sec, i64::from(tv.tv_usec));
        }
        match vm_stats() {
            Some(v) => {
                for (k, val) in [
                    ("free_count", v.free_count as u64),
                    ("active_count", v.active_count as u64),
                    ("inactive_count", v.inactive_count as u64),
                    ("wire_count", v.wire_count as u64),
                    ("purgeable_count", v.purgeable_count as u64),
                    ("speculative_count", v.speculative_count as u64),
                    ("throttled_count", v.throttled_count as u64),
                    ("external_page_count", v.external_page_count as u64),
                    ("internal_page_count", v.internal_page_count as u64),
                    ("compressor_page_count", v.compressor_page_count as u64),
                    (
                        "total_uncompressed_pages_in_compressor",
                        v.total_uncompressed_pages_in_compressor,
                    ),
                    ("pageins", v.pageins),
                    ("pageouts", v.pageouts),
                    ("swapins", v.swapins),
                    ("swapouts", v.swapouts),
                    ("compressions", v.compressions),
                    ("decompressions", v.decompressions),
                ] {
                    h.vm.insert(k.to_string(), val);
                }
            }
            None => {
                h.errors.insert("host_statistics64".into(), "failed".into());
            }
        }
        h.cpu_ticks = cpu_ticks();
        h.per_cpu_ticks = per_cpu_ticks();
        let ncpu = h.per_cpu_ticks.len();
        if ncpu > 0 {
            if self.core_clusters.as_ref().is_none_or(|c| c.len() != ncpu) {
                self.core_clusters = Some(core_clusters(ncpu));
            }
            h.core_clusters = self.core_clusters.clone().unwrap_or_default();
        }
        let mut la = [0f64; 3];
        // SAFETY: 3-element buffer.
        if unsafe { libc::getloadavg(la.as_mut_ptr(), 3) } == 3 {
            h.load_avg = Some(la);
        }
        if h.sysctl.is_empty() && h.vm.is_empty() {
            return Err(SourceError::Unavailable(
                "sysctl and host_statistics64 unreadable".into(),
            ));
        }
        self.read_extras(&mut h, start, budget);
        Ok(RawSample {
            source: names::MACOS_HOST.into(),
            taken_at_ms: now_ms(),
            read_us: start.elapsed().as_micros() as u64,
            payload: RawPayload::MacHost(h),
        })
    }
}

type ResponsibleFn = unsafe extern "C" fn(libc::pid_t) -> libc::pid_t;

fn responsible_fn() -> Option<ResponsibleFn> {
    let name = CString::new("responsibility_get_pid_responsible_for_pid").ok()?;
    // SAFETY: dlsym with RTLD_DEFAULT; returns null when absent.
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    if p.is_null() {
        None
    } else {
        // SAFETY: symbol has signature pid_t(pid_t) on all macOS versions that export it.
        Some(unsafe { std::mem::transmute::<*mut c_void, ResponsibleFn>(p) })
    }
}

/// Parses a `KERN_PROCARGS2` buffer into (argv, env pairs filtered to the allowlist → markers).
pub fn parse_procargs2(buf: &[u8], allowlist: &[String]) -> Option<(Vec<String>, Markers)> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?).max(0) as usize;
    // Every argument needs at least its NUL byte: a larger argc is corrupt (and must not drive the
    // allocation below — `i32::MAX` strings would abort on allocation failure).
    if argc > buf.len() {
        return None;
    }
    let mut pos = 4;
    // exec path
    while pos < buf.len() && buf[pos] != 0 {
        pos += 1;
    }
    while pos < buf.len() && buf[pos] == 0 {
        pos += 1;
    }
    let mut strings = buf[pos..].split(|b| *b == 0);
    let mut argv = Vec::with_capacity(argc);
    for _ in 0..argc {
        argv.push(String::from_utf8_lossy(strings.next()?).into_owned());
    }
    let env = strings.take_while(|s| !s.is_empty()).filter_map(|kv| {
        let s = std::str::from_utf8(kv).ok()?;
        s.split_once('=')
    });
    // Only allowlisted keys are kept (values hashed); everything else is dropped here.
    let markers = markers_from_env(env, allowlist);
    Some((argv, markers))
}

fn procargs2(pid: i32, argmax: usize, allowlist: &[String]) -> Option<(Vec<String>, Markers)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut buf = vec![0u8; argmax];
    let mut len: libc::size_t = argmax;
    // SAFETY: mib and buffer valid.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    buf.truncate(len);
    parse_procargs2(&buf, allowlist)
}

fn bsd_info(pid: i32) -> Option<libc::proc_bsdinfo> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: buffer sized for proc_bsdinfo.
    let r = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr() as *mut c_void,
            size,
        )
    };
    // SAFETY: fully written when r == size.
    (r == size).then(|| unsafe { info.assume_init() })
}

/// Basic identity fields, from `proc_pidinfo(PROC_PIDTBSDINFO)` or the `kinfo_proc` fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicInfo {
    pub ppid: u32,
    pub uid: u32,
    pub start_tvsec: u64,
    pub start_tvusec: u64,
    pub status: u32,
    pub comm: String,
    pub name: String,
}

// `struct kinfo_proc` layout (macOS SDK, LP64; verified with offsetof on macOS 26 / arm64).
const KINFO_SIZE: usize = 648;
const KP_STARTTIME: usize = 0;
const KP_STAT: usize = 36;
const KP_PID: usize = 40;
const KP_COMM: usize = 243;
const KP_UID: usize = 420;
const KP_PPID: usize = 560;

/// Decodes one `struct kinfo_proc` record: `(pid, info)`.
fn parse_kinfo(buf: &[u8]) -> Option<(i32, BasicInfo)> {
    if buf.len() < KINFO_SIZE {
        return None;
    }
    let u32_at = |o: usize| u32::from_ne_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    let pid = u32_at(KP_PID) as i32;
    let sec = i64::from_ne_bytes(buf[KP_STARTTIME..KP_STARTTIME + 8].try_into().ok()?);
    let usec = i32::from_ne_bytes(buf[KP_STARTTIME + 8..KP_STARTTIME + 12].try_into().ok()?);
    let comm: Vec<u8> = buf[KP_COMM..KP_COMM + 17]
        .iter()
        .take_while(|b| **b != 0)
        .copied()
        .collect();
    let comm = String::from_utf8_lossy(&comm).into_owned();
    Some((
        pid,
        BasicInfo {
            ppid: u32_at(KP_PPID),
            uid: u32_at(KP_UID),
            start_tvsec: sec.max(0) as u64,
            start_tvusec: usec.max(0) as u64,
            status: buf[KP_STAT] as u32,
            // kinfo_proc has only p_comm (cut at MAXCOMLEN); leave `name` empty so the decoder can use the
            // executable's basename from proc_pidpath instead.
            name: String::new(),
            comm,
        },
    ))
}

/// `sysctl(KERN_PROC_PID)`: works for every user's processes (what `ps` uses), unlike PROC_PIDTBSDINFO.
pub fn kinfo(pid: i32) -> Option<BasicInfo> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut buf = [0u8; KINFO_SIZE];
    let mut len: libc::size_t = KINFO_SIZE;
    // SAFETY: buffer of KINFO_SIZE bytes; the kernel writes at most `len`.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 || len != KINFO_SIZE {
        return None;
    }
    parse_kinfo(&buf).filter(|(p, _)| *p == pid).map(|(_, b)| b)
}

/// `sysctl(KERN_PROC_ALL)`: every process's `kinfo_proc` in one call. Replaces ~400 per-pid sysctls for
/// other users' processes (PROC_PIDTBSDINFO is refused for them) — the largest collector cost on a
/// multi-user Mac (SPEC §14). Empty on failure; callers fall back to [`kinfo`].
pub fn kinfo_all() -> HashMap<i32, BasicInfo> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL, 0];
    for _ in 0..3 {
        let mut len: libc::size_t = 0;
        // SAFETY: size query (null buffer).
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 || len == 0 {
            return HashMap::new();
        }
        // Room for processes started between the two calls.
        len += 64 * KINFO_SIZE;
        let mut buf = vec![0u8; len];
        // SAFETY: buffer of `len` bytes; the kernel writes at most `len` and updates it.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr() as *mut c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOMEM) {
                continue; // grew again: retry
            }
            return HashMap::new();
        }
        buf.truncate(len);
        return buf
            .as_chunks::<KINFO_SIZE>()
            .0
            .iter()
            .filter_map(|c| parse_kinfo(c))
            .collect();
    }
    HashMap::new()
}

/// Identity info: PROC_PIDTBSDINFO when permitted, else `kinfo_proc`.
pub fn basic_info(pid: i32) -> Option<BasicInfo> {
    basic_info_bsd(pid).or_else(|| kinfo(pid))
}

/// PROC_PIDTBSDINFO only (full `pbi_name`); `None` when refused.
fn basic_info_bsd(pid: i32) -> Option<BasicInfo> {
    bsd_info(pid).map(|b| BasicInfo {
        ppid: b.pbi_ppid,
        uid: b.pbi_uid,
        start_tvsec: b.pbi_start_tvsec,
        start_tvusec: b.pbi_start_tvusec,
        status: b.pbi_status,
        comm: cstr_field(&b.pbi_comm),
        name: cstr_field(&b.pbi_name),
    })
}

/// `(pti_threadnum, pti_numrunning)` from `PROC_PIDTASKINFO` (same-user processes only).
fn task_threads(pid: i32) -> Option<(u32, u32)> {
    let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = size_of::<libc::proc_taskinfo>() as i32;
    // SAFETY: as above.
    let r = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr() as *mut c_void,
            size,
        )
    };
    // SAFETY: fully written when r == size.
    (r == size).then(|| {
        // SAFETY: fully written when r == size.
        let i = unsafe { info.assume_init() };
        (i.pti_threadnum.max(0) as u32, i.pti_numrunning.max(0) as u32)
    })
}

fn rusage(pid: i32) -> Result<MacRusage, String> {
    let mut ri = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: buffer for rusage_info_v4.
    let r = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V4,
            ri.as_mut_ptr() as *mut libc::rusage_info_t,
        )
    };
    if r != 0 {
        let e = std::io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EPERM) => EPERM_REASON.into(),
            Some(libc::ESRCH) => "exited".into(),
            _ => e.to_string(),
        });
    }
    // SAFETY: written by the kernel.
    let ri = unsafe { ri.assume_init() };
    Ok(MacRusage {
        user_time: ri.ri_user_time,
        system_time: ri.ri_system_time,
        resident_size: ri.ri_resident_size,
        phys_footprint: ri.ri_phys_footprint,
        wired_size: ri.ri_wired_size,
        lifetime_max_phys_footprint: ri.ri_lifetime_max_phys_footprint,
        diskio_bytesread: ri.ri_diskio_bytesread,
        diskio_byteswritten: ri.ri_diskio_byteswritten,
        read_ms: Some(now_ms()),
    })
}

fn pid_path(pid: i32) -> String {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buffer of PROC_PIDPATHINFO_MAXSIZE bytes.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut c_void, buf.len() as u32) };
    if n <= 0 {
        return String::new();
    }
    buf.truncate(n as usize);
    String::from_utf8_lossy(&buf).into_owned()
}

fn cstr_field(bytes: &[libc::c_char]) -> String {
    let b: Vec<u8> = bytes.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
    String::from_utf8_lossy(&b).into_owned()
}

/// Lists all pids.
pub fn list_pids() -> Vec<i32> {
    // SAFETY: size query.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut pids = vec![0i32; n as usize + 64];
    let bytes = (pids.len() * size_of::<i32>()) as i32;
    // SAFETY: buffer of `bytes` bytes.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut c_void, bytes) };
    if got <= 0 {
        return Vec::new();
    }
    pids.truncate(got as usize);
    pids.retain(|p| *p > 0);
    pids
}

/// Start time (ms since epoch) of a pid, for identity re-verification.
pub fn start_time_ms(pid: u32) -> Option<u64> {
    let b = basic_info(pid as i32)?;
    Some(b.start_tvsec * 1000 + b.start_tvusec / 1000)
}

/// Cached per `(pid, start_time)`: what never changes for a process (argv, markers, executable path,
/// responsible pid), plus the uid for which `proc_pid_rusage` / `PROC_PIDTASKINFO` were refused with
/// EPERM, so other users' processes cost one syscall per sample instead of three (SPEC §14).
struct CachedIdentity {
    argv: Option<Vec<String>>,
    markers: Option<Markers>,
    path: String,
    responsible_pid: Option<u32>,
    denied_uid: Option<u32>,
    /// `p_comm` and full `pbi_name` at the last PROC_PIDTBSDINFO read (a changed comm means an exec).
    comm: String,
    name: String,
    /// Thread count and the reads left before it is refreshed.
    threads: Option<u32>,
    threads_age: u32,
    /// Last `proc_pid_rusage`, whether the process used no CPU between the last two reads, and how many
    /// reads in a row reused it.
    last_ru: Option<MacRusage>,
    idle: bool,
    reused: u32,
}

/// Thread counts change slowly and cost one syscall per process: refreshed every Nth read (SPEC §14).
const THREADS_EVERY: u32 = 5;
/// A process that used no CPU between its last two reads has its rusage re-read only every
/// `IDLE_RUSAGE_EVERY`th read (footprint of an idle process is at most that many refreshes old; any CPU
/// use shows at the next real read). Busy processes are read every time (SPEC §14 CPU budget).
const IDLE_RUSAGE_EVERY: u32 = 3;

const EPERM_REASON: &str = "other user's process (needs root)";

/// Per-process source. argv/markers are cached per `(pid, start_time)` since they never change.
pub struct MacProcSource {
    allowlist: Vec<String>,
    argmax: usize,
    cache: HashMap<(u32, u64), CachedIdentity>,
    responsible: Option<ResponsibleFn>,
    timebase: (u32, u32),
    ncpu: u32,
}

impl std::fmt::Debug for MacProcSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacProcSource")
            .field("cached", &self.cache.len())
            .finish()
    }
}

impl Default for MacProcSource {
    fn default() -> Self {
        Self::new(default_allowlist())
    }
}

impl MacProcSource {
    pub fn new(allowlist: Vec<String>) -> Self {
        MacProcSource {
            allowlist,
            argmax: sysctl_int("kern.argmax").unwrap_or(1 << 20).clamp(4096, 4 << 20) as usize,
            cache: HashMap::new(),
            responsible: responsible_fn(),
            timebase: timebase(),
            ncpu: sysctl_int("hw.ncpu").unwrap_or(1).max(1) as u32,
        }
    }
}

impl Source for MacProcSource {
    fn name(&self) -> &'static str {
        names::MACOS_PROCS
    }

    fn read(&mut self, budget: Duration) -> Result<RawSample, SourceError> {
        let start = Instant::now();
        let pids = list_pids();
        if pids.is_empty() {
            return Err(SourceError::Unavailable(
                "proc_listallpids returned nothing".into(),
            ));
        }
        // SAFETY: trivial libc calls.
        let my_uid = unsafe { libc::getuid() };
        let mut procs = Vec::with_capacity(pids.len());
        let mut truncated = false;
        let mut seen = std::collections::HashSet::new();
        let batch = kinfo_all();
        for pid in pids {
            if start.elapsed() > budget {
                truncated = true;
                break;
            }
            // Other users' processes: straight from the batch (PROC_PIDTBSDINFO would be refused). Same
            // user: the full name needs PROC_PIDTBSDINFO once per identity, and again after an exec.
            let b = match batch.get(&pid) {
                Some(k) if k.uid != my_uid => Some(k.clone()),
                Some(k) => {
                    let key = (pid as u32, k.start_tvsec * 1000 + k.start_tvusec / 1000);
                    match self.cache.get(&key) {
                        Some(c) if c.comm == k.comm && !c.name.is_empty() => Some(BasicInfo {
                            name: c.name.clone(),
                            ..k.clone()
                        }),
                        _ => basic_info_bsd(pid).or_else(|| Some(k.clone())),
                    }
                }
                None => basic_info(pid),
            };
            let Some(b) = b else { continue };
            let start_ms = b.start_tvsec * 1000 + b.start_tvusec / 1000;
            let key = (pid as u32, start_ms);
            seen.insert(key);
            let responsible = self.responsible;
            // An exec keeps pid and start time but changes argv: re-read the identity.
            if self.cache.get(&key).is_some_and(|c| c.comm != b.comm) {
                self.cache.remove(&key);
            }
            let entry = self.cache.entry(key).or_insert_with(|| {
                let (argv, markers) = if b.uid == my_uid {
                    match procargs2(pid, self.argmax, &self.allowlist) {
                        Some((a, m)) => (Some(a), Some(m)),
                        None => (None, None),
                    }
                } else {
                    (None, None)
                };
                let responsible_pid = responsible.and_then(|f| {
                    // SAFETY: symbol resolved via dlsym with the documented signature.
                    let r = unsafe { f(pid) };
                    (r > 0).then_some(r as u32)
                });
                CachedIdentity {
                    argv,
                    markers,
                    path: pid_path(pid),
                    responsible_pid,
                    denied_uid: None,
                    comm: b.comm.clone(),
                    name: b.name.clone(),
                    threads: None,
                    threads_age: 0,
                    last_ru: None,
                    idle: false,
                    reused: 0,
                }
            });
            let denied = entry.denied_uid == Some(b.uid);
            let reuse = entry.idle && entry.reused + 1 < IDLE_RUSAGE_EVERY && entry.last_ru.is_some();
            let (ru, ru_err) = if denied {
                (None, Some(EPERM_REASON.to_string()))
            } else if reuse {
                entry.reused += 1;
                (entry.last_ru.clone(), None)
            } else {
                match rusage(pid) {
                    Ok(r) => {
                        let cpu = |x: &MacRusage| x.user_time.saturating_add(x.system_time);
                        entry.idle = entry.last_ru.as_ref().is_some_and(|l| cpu(l) == cpu(&r));
                        entry.reused = 0;
                        entry.last_ru = Some(r.clone());
                        (Some(r), None)
                    }
                    Err(e) => {
                        if e == EPERM_REASON {
                            entry.denied_uid = Some(b.uid);
                        }
                        entry.last_ru = None;
                        (None, Some(e))
                    }
                }
            };
            // Running threads (htop's "running" count, and the process state): a process that used no CPU
            // between its last two reads has none; busy processes get a fresh PROC_PIDTASKINFO every read
            // (few per refresh), idle ones reuse the cached thread count (SPEC §14 CPU budget).
            let (threads, running_threads) = if ru.is_none() {
                (None, None)
            } else if entry.idle && entry.threads.is_some() && entry.threads_age + 1 < THREADS_EVERY {
                entry.threads_age += 1;
                (entry.threads, Some(0))
            } else {
                entry.threads_age = 0;
                match task_threads(pid) {
                    Some((t, r)) => {
                        entry.threads = Some(t);
                        (Some(t), Some(if entry.idle { 0 } else { r }))
                    }
                    None => {
                        entry.threads = None;
                        (None, None)
                    }
                }
            };
            procs.push(MacProcRaw {
                pid: pid as u32,
                ppid: b.ppid,
                uid: b.uid,
                start_tvsec: b.start_tvsec,
                start_tvusec: b.start_tvusec,
                status: b.status,
                comm: b.comm.clone(),
                name: b.name.clone(),
                path: entry.path.clone(),
                threads,
                running_threads,
                rusage: ru,
                rusage_error: ru_err,
                argv: entry.argv.clone(),
                markers: entry.markers.clone(),
                responsible_pid: entry.responsible_pid,
            });
        }
        if !truncated {
            self.cache.retain(|k, _| seen.contains(k));
        }
        Ok(RawSample {
            source: names::MACOS_PROCS.into(),
            taken_at_ms: now_ms(),
            read_us: start.elapsed().as_micros() as u64,
            payload: RawPayload::MacProcs(MacProcsRaw {
                procs,
                timebase_numer: self.timebase.0,
                timebase_denom: self.timebase.1,
                ncpu: self.ncpu,
                truncated,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinfo_batch_matches_per_pid_kinfo() {
        let all = kinfo_all();
        assert!(all.len() > 10, "KERN_PROC_ALL returned {} processes", all.len());
        let me = std::process::id() as i32;
        let one = kinfo(me).expect("kinfo(self)");
        assert_eq!(all.get(&me), Some(&one));
        // launchd is visible to everyone.
        assert_eq!(all.get(&1).map(|b| b.uid), Some(0));
    }

    #[test]
    fn procargs_parsing_keeps_only_allowlisted_env() {
        let mut b = Vec::new();
        b.extend_from_slice(&2i32.to_ne_bytes());
        b.extend_from_slice(b"/usr/bin/node\0\0\0");
        b.extend_from_slice(b"node\0cli.js\0CLAUDECODE=1\0CLAUDE_CODE_SESSION_ID=abc\0SECRET_TOKEN=xyz\0\0");
        let (argv, m) = parse_procargs2(&b, &default_allowlist()).unwrap();
        assert_eq!(argv, vec!["node", "cli.js"]);
        assert_eq!(m.keys, vec!["CLAUDECODE", "CLAUDE_CODE_SESSION_ID"]);
        assert!(m.session_id.is_some());
    }

    /// Wall-clock bound for a live host read in tests. SPEC §14's 50 ms is the release budget (measured
    /// ~2 ms); unoptimized builds on a loaded fanless machine (parallel cargo builds) get 4× headroom so
    /// the test checks the code path, not the machine's load. The budget *logic* is tested
    /// deterministically in `zero_budget_skips_optional_readers`.
    const HOST_READ_LIMIT_US: u64 = if cfg!(debug_assertions) { 200_000 } else { 50_000 };

    /// A spent budget skips every optional reader with a reason instead of overrunning (SPEC §6.2).
    #[test]
    fn zero_budget_skips_optional_readers() {
        let mut h = MacHostSource::new();
        let raw = h.read(Duration::ZERO).expect("host sample");
        let RawPayload::MacHost(x) = &raw.payload else {
            panic!("unexpected payload")
        };
        // Core memory data is always read.
        assert!(x.sysctl.contains_key("hw.memsize") && !x.vm.is_empty());
        for part in [
            extras::keys::PART_POWER,
            extras::keys::PART_IOACCEL,
            extras::keys::PART_IOREPORT,
        ] {
            let k = format!("{}{part}", extras::keys::UNAVAILABLE_PREFIX);
            assert_eq!(
                x.sysctl_str.get(&k).map(String::as_str),
                Some("skipped: sample time budget spent"),
                "{part}"
            );
        }
        assert!(!x.sysctl.contains_key(extras::keys::BATT_PRESENT));
        assert!(!x
            .sysctl
            .keys()
            .any(|k| k.starts_with(extras::keys::IOACCEL_PREFIX)));
        // …and the decoder turns that into unavailable-with-reason, not zeros.
        let d = extras::decode_extras(x, raw.taken_at_ms);
        assert!(d.thermal.on_battery.value.is_none());
        assert!(d.gpu.mem_used.value.is_none());
        assert!(matches!(
            &d.status[extras::status_keys::POWER],
            oomtop_core::SourceStatus::Unavailable(r) if r.contains("budget")
        ));
    }

    /// Corrupt/truncated KERN_PROCARGS2 buffers never panic or over-allocate.
    #[test]
    fn procargs_parsing_survives_garbage() {
        let mut b = Vec::new();
        b.extend_from_slice(&i32::MAX.to_ne_bytes());
        b.extend_from_slice(b"/bin/x\0\0a\0b\0");
        assert!(parse_procargs2(&b, &default_allowlist()).is_none());
        let mut neg = Vec::new();
        neg.extend_from_slice(&(-7i32).to_ne_bytes());
        neg.extend_from_slice(b"/bin/x\0\0CLAUDECODE=1\0");
        let (argv, m) = parse_procargs2(&neg, &default_allowlist()).unwrap();
        assert!(argv.is_empty());
        assert_eq!(m.keys, vec!["CLAUDECODE"]);
        // Every prefix of a valid buffer: no panic; argc > available strings → None.
        let mut ok = Vec::new();
        ok.extend_from_slice(&3i32.to_ne_bytes());
        ok.extend_from_slice(b"/usr/bin/node\0\0\0node\0a\xffb\0c\0CLAUDECODE=1\0PATH=/x\0\0\0");
        for n in 0..=ok.len() {
            let _ = parse_procargs2(&ok[..n], &default_allowlist());
        }
        let (argv, m) = parse_procargs2(&ok, &default_allowlist()).unwrap();
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[1], "a\u{fffd}b");
        // PATH is not allowlisted: dropped, never stored.
        assert_eq!(m.keys, vec!["CLAUDECODE"]);
        assert!(parse_procargs2(&[1, 0], &default_allowlist()).is_none());
    }

    /// `struct xsw_usage` / libc struct sizes match the macOS SDK (checked with `offsetof` on macOS 26).
    #[test]
    fn ffi_struct_layouts_match_sdk() {
        assert_eq!(size_of::<XswUsage>(), 32);
        assert_eq!(std::mem::offset_of!(XswUsage, encrypted), 28);
        assert_eq!(size_of::<libc::rusage_info_v4>(), 296);
        assert_eq!(std::mem::offset_of!(libc::rusage_info_v4, ri_phys_footprint), 72);
        assert_eq!(size_of::<libc::proc_bsdinfo>(), 136);
        assert_eq!(size_of::<libc::proc_taskinfo>(), 96);
        // Offsets of every field we read, from `offsetof` against the macOS 26.5 SDK.
        use std::mem::offset_of;
        type Ri = libc::rusage_info_v4;
        for (got, want) in [
            (offset_of!(Ri, ri_user_time), 16),
            (offset_of!(Ri, ri_system_time), 24),
            (offset_of!(Ri, ri_wired_size), 56),
            (offset_of!(Ri, ri_resident_size), 64),
            (offset_of!(Ri, ri_diskio_bytesread), 144),
            (offset_of!(Ri, ri_diskio_byteswritten), 152),
            (offset_of!(Ri, ri_lifetime_max_phys_footprint), 240),
            (offset_of!(libc::proc_bsdinfo, pbi_status), 4),
            (offset_of!(libc::proc_bsdinfo, pbi_ppid), 16),
            (offset_of!(libc::proc_bsdinfo, pbi_uid), 20),
            (offset_of!(libc::proc_bsdinfo, pbi_comm), 48),
            (offset_of!(libc::proc_bsdinfo, pbi_name), 64),
            (offset_of!(libc::proc_bsdinfo, pbi_start_tvsec), 120),
            (offset_of!(libc::proc_taskinfo, pti_threadnum), 84),
        ] {
            assert_eq!(got, want);
        }
        // libc's `vm_statistics64` is a newer superset of the SDK's (248 bytes = HOST_VM_INFO64_COUNT 62):
        // the kernel fills only the count it knows, the tail stays zeroed. Every field we read sits in the
        // common prefix at the SDK offset.
        type Vm = libc::vm_statistics64;
        assert!(size_of::<Vm>() >= 248);
        for (got, want) in [
            (offset_of!(Vm, free_count), 0),
            (offset_of!(Vm, wire_count), 12),
            (offset_of!(Vm, pageins), 32),
            (offset_of!(Vm, pageouts), 40),
            (offset_of!(Vm, purgeable_count), 88),
            (offset_of!(Vm, speculative_count), 92),
            (offset_of!(Vm, decompressions), 96),
            (offset_of!(Vm, compressions), 104),
            (offset_of!(Vm, swapins), 112),
            (offset_of!(Vm, swapouts), 120),
            (offset_of!(Vm, compressor_page_count), 128),
            (offset_of!(Vm, throttled_count), 132),
            (offset_of!(Vm, external_page_count), 136),
            (offset_of!(Vm, internal_page_count), 140),
            (offset_of!(Vm, total_uncompressed_pages_in_compressor), 144),
        ] {
            assert_eq!(got, want);
        }
        assert!(vm_stats().is_some());
        // Live: vm.swapusage reads with the exact struct size (a size mismatch would return None).
        assert!(sysctl_struct::<XswUsage>("vm.swapusage").is_some());
    }

    #[test]
    fn boot_time_is_validated() {
        assert_eq!(boot_time_ms(1_790_000_000, 500_000), Some(1_790_000_000_500));
        assert_eq!(boot_time_ms(-1, 0), None);
        assert_eq!(boot_time_ms(0, 0), None);
        assert_eq!(boot_time_ms(1, 2_000_000), None);
        assert_eq!(boot_time_ms(i64::MAX, 0), None);
    }

    #[test]
    fn host_extras_decode_live() {
        let mut h = MacHostSource::new();
        let a = h.read(Duration::from_millis(50)).expect("host sample");
        assert!(
            a.read_us < HOST_READ_LIMIT_US,
            "first host read took {} µs",
            a.read_us
        );
        // IOReport initializes off-thread; poll like the sampler would (1 s cadence, faster here).
        let mut b = a.clone();
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(100));
            b = h.read(Duration::from_millis(50)).expect("host sample");
            let RawPayload::MacHost(raw) = &b.payload else {
                panic!("unexpected payload")
            };
            if raw.sysctl.contains_key(extras::keys::IOREPORT_WINDOW_US)
                || raw
                    .sysctl_str
                    .get("unavailable.ioreport")
                    .map(|r| r != "initializing" && r != "needs two samples")
                    .unwrap_or(false)
            {
                break;
            }
        }
        let RawPayload::MacHost(raw) = &b.payload else {
            panic!("unexpected payload")
        };
        if std::env::var_os("OOMTOP_PRINT_SENSORS").is_some() {
            eprintln!("first read {} µs, second read {} µs", a.read_us, b.read_us);
            eprintln!("{:#?}", (&raw.sysctl, &raw.sysctl_str.keys().collect::<Vec<_>>()));
        }
        let x = extras::decode_extras(raw, b.taken_at_ms);
        // Thermal pressure is public API: it must be readable on every supported macOS.
        assert!(x.thermal.pressure.value.is_some(), "{:?}", x.thermal.pressure);
        assert!(x.thermal.low_power_mode.value.is_some());
        assert!(x.thermal.on_battery.value.is_some());
        // Second sample: IOReport has a delta (or a reason), never a silent zero.
        match &x.status[extras::status_keys::IOREPORT] {
            oomtop_core::SourceStatus::Available => assert!(!x.thermal.clusters.is_empty()),
            other => eprintln!("ioreport: {other:?}"),
        }
        assert!(b.read_us < HOST_READ_LIMIT_US, "host read took {} µs", b.read_us);

        // IOReport cadence (SPEC §14): a reading with a delta is re-used for IOREPORT_REFRESH, then re-read.
        let window = |r: &RawSample| match &r.payload {
            RawPayload::MacHost(raw) => raw.sysctl.get(extras::keys::IOREPORT_WINDOW_US).copied(),
            _ => None,
        };
        if let Some(w) = window(&b) {
            let c = h.read(Duration::from_millis(50)).expect("host sample");
            assert_eq!(window(&c), Some(w), "re-used within {IOREPORT_REFRESH:?}");
            if let Some((at, _)) = h.ioreport_cache.as_mut() {
                *at = at.checked_sub(IOREPORT_REFRESH).unwrap_or(*at);
            }
            std::thread::sleep(Duration::from_millis(20));
            let d = h.read(Duration::from_millis(50)).expect("host sample");
            assert!(window(&d).is_some_and(|x| x != w), "re-read once stale");
        }
    }

    /// Per-reader timing (printed with OOMTOP_PRINT_SENSORS=1); release numbers are what SPEC §14 budgets.
    #[test]
    fn extras_timing() {
        let t = Instant::now();
        let r = ioreport::IoReport::new();
        let t_sub = t.elapsed();
        let t = Instant::now();
        let d = ioreport::dvfs_tables();
        let t_dvfs = t.elapsed();
        let mut r = r.ok();
        let t = Instant::now();
        if let Some(r) = r.as_mut() {
            let _ = r.delta();
        }
        let t_sample = t.elapsed();
        let t = Instant::now();
        let mut j = jetsam::JetsamScanner::default();
        let _ = j.scan(Duration::from_secs(1));
        let t_jet = t.elapsed();
        let mut h = MacHostRaw::default();
        let t = Instant::now();
        sensors::read_power(&mut h);
        let t_power = t.elapsed();
        let t = Instant::now();
        sensors::read_ioaccel(&mut h);
        let t_accel = t.elapsed();
        let n = sensors::ThermalNotify::new();
        let pi = sensors::ProcessInfo::new();
        let t = Instant::now();
        sensors::read_thermal(&mut h, &n, pi.as_ref());
        let t_thermal = t.elapsed();
        if std::env::var_os("OOMTOP_PRINT_SENSORS").is_some() {
            eprintln!(
                "subscribe {t_sub:?} dvfs {t_dvfs:?} (ok={}) sample {t_sample:?} jetsam {t_jet:?} power {t_power:?} ioaccel {t_accel:?} thermal {t_thermal:?}",
                d.is_ok()
            );
        }
    }

    #[test]
    fn procs_timing() {
        let mut p = MacProcSource::default();
        let a = p.read(Duration::from_millis(200)).expect("procs");
        let b = p.read(Duration::from_millis(200)).expect("procs");
        let (RawPayload::MacProcs(x), RawPayload::MacProcs(y)) = (&a.payload, &b.payload) else {
            panic!("unexpected payload")
        };
        if std::env::var_os("OOMTOP_PRINT_SENSORS").is_some() {
            let with_ru = y.procs.iter().filter(|p| p.rusage.is_some()).count();
            let with_resp = y.procs.iter().filter(|p| p.responsible_pid.is_some()).count();
            eprintln!(
                "procs: cold {} µs ({} procs, truncated={}), warm {} µs ({} procs, {with_ru} with rusage, {with_resp} with responsible pid)",
                a.read_us,
                x.procs.len(),
                x.truncated,
                b.read_us,
                y.procs.len()
            );
        }
        assert!(!y.truncated, "warm listing truncated in {} µs", b.read_us);
    }

    /// CF/IOKit/ObjC objects are released every sample: own footprint stays flat over many reads.
    #[test]
    #[ignore = "slow leak check; run explicitly"]
    fn host_reads_do_not_leak() {
        let me = std::process::id() as i32;
        let fp = || rusage(me).map(|r| r.phys_footprint).unwrap_or(0);
        let mut h = MacHostSource::new();
        for _ in 0..50 {
            let _ = h.read(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(5));
        }
        let n: u32 = std::env::var("OOMTOP_LEAK_READS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(400);
        let before = fp();
        let t = Instant::now();
        for _ in 0..n {
            let _ = h.read(Duration::from_millis(50));
        }
        let per = t.elapsed() / n;
        let after = fp();
        eprintln!(
            "footprint before {before} after {after} (Δ {} KiB), {per:?} per read",
            (after as i64 - before as i64) / 1024
        );
        assert!(
            after < before + (2 << 20),
            "footprint grew {} → {}",
            before,
            after
        );
    }

    #[test]
    fn host_without_extras_has_no_extra_keys() {
        let mut h = MacHostSource::with_extras(HostExtras::none());
        let raw = h.read(Duration::from_millis(50)).expect("host sample");
        let RawPayload::MacHost(x) = &raw.payload else {
            panic!("unexpected payload")
        };
        assert!(!x
            .sysctl
            .keys()
            .any(|k| k.starts_with("notify.") || k.starts_with("ioreport.")));
        assert!(x.sysctl.contains_key("hw.memsize"));
    }

    #[test]
    fn reads_this_machine() {
        let mut h = MacHostSource::new();
        let raw = h.read(Duration::from_millis(50)).expect("host sample");
        match &raw.payload {
            RawPayload::MacHost(x) => {
                assert!(x.sysctl.contains_key("hw.memsize"));
                // Every required sysctl + host_statistics64 + vm.swapusage reads on a supported Mac.
                assert!(x.errors.is_empty(), "{:?}", x.errors);
                assert!(x.swap.is_some());
            }
            other => panic!("unexpected {other:?}"),
        }
        let mut p = MacProcSource::default();
        let raw = p.read(Duration::from_millis(500)).expect("procs sample");
        let RawPayload::MacProcs(ps) = &raw.payload else {
            panic!("unexpected payload")
        };
        let me = std::process::id();
        let mine = ps.procs.iter().find(|x| x.pid == me).expect("own process listed");
        assert!(mine
            .rusage
            .as_ref()
            .map(|r| r.phys_footprint > 0)
            .unwrap_or(false));
        assert_eq!(
            start_time_ms(me),
            Some(mine.start_tvsec * 1000 + mine.start_tvusec / 1000)
        );
        // kinfo_proc fallback agrees with PROC_PIDTBSDINFO on our own process.
        let k = kinfo(me as i32).expect("kinfo_proc");
        assert_eq!(
            (k.ppid, k.uid, k.start_tvsec, k.start_tvusec),
            (mine.ppid, mine.uid, mine.start_tvsec, mine.start_tvusec)
        );
        assert!(k.comm.starts_with(&mine.comm), "{} vs {}", k.comm, mine.comm);
        // pid 1 (launchd, root) is visible via the fallback.
        assert!(basic_info(1).is_some());
        assert!(ps.procs.iter().any(|x| x.pid == 1));
    }
}

//! IOReport (private, sudoless — the approach used by macmon/mactop; SPEC §9, §16): CPU-cluster and GPU
//! performance-state residency plus the "Energy Model" energy counters. Symbols are resolved at runtime
//! with `dlopen`/`dlsym`; if the library or any symbol is missing, everything degrades to
//! `unavailable` and the rest of oomtop keeps working.
//!
//! The DVFS frequency tables (MHz per performance state) come from the `pmgr` IORegistry entry
//! (`voltage-states*-sram`), read once and cached.
//!
//! Output (raw, not interpreted): per channel the ordered residency deltas `NAME:ticks,…` and per energy
//! channel the energy delta in nanojoules, over `window_us`. Mapping states to frequencies and computing
//! residency/active %/watts is done by the pure decoder (`macos::extras`).

use super::cf::{self, CFDictionaryRef, CFMutableDictionaryRef, CFStringRef, CFTypeRef, Cf};
use std::collections::BTreeMap;
use std::ffi::{c_void, CString};
use std::time::Instant;

type CopyChannelsInGroup = unsafe extern "C" fn(CFStringRef, CFStringRef, u64, u64, u64) -> CFDictionaryRef;
type MergeChannels = unsafe extern "C" fn(CFDictionaryRef, CFDictionaryRef, CFTypeRef);
type CreateSubscription = unsafe extern "C" fn(
    *const c_void,
    CFMutableDictionaryRef,
    *mut CFMutableDictionaryRef,
    u64,
    CFTypeRef,
) -> *const c_void;
type CreateSamples =
    unsafe extern "C" fn(*const c_void, CFMutableDictionaryRef, CFTypeRef) -> CFDictionaryRef;
type CreateSamplesDelta =
    unsafe extern "C" fn(CFDictionaryRef, CFDictionaryRef, CFTypeRef) -> CFDictionaryRef;
type ChannelGetStr = unsafe extern "C" fn(CFDictionaryRef) -> CFStringRef;
type SimpleGetInteger = unsafe extern "C" fn(CFDictionaryRef, i32) -> i64;
type StateGetCount = unsafe extern "C" fn(CFDictionaryRef) -> i32;
type StateGetName = unsafe extern "C" fn(CFDictionaryRef, i32) -> CFStringRef;
type StateGetResidency = unsafe extern "C" fn(CFDictionaryRef, i32) -> i64;

/// Resolved IOReport entry points.
#[derive(Clone, Copy)]
struct Api {
    copy_channels_in_group: CopyChannelsInGroup,
    merge_channels: MergeChannels,
    create_subscription: CreateSubscription,
    create_samples: CreateSamples,
    create_samples_delta: CreateSamplesDelta,
    get_group: ChannelGetStr,
    get_subgroup: ChannelGetStr,
    get_channel_name: ChannelGetStr,
    get_unit_label: ChannelGetStr,
    simple_get_integer: SimpleGetInteger,
    state_get_count: StateGetCount,
    state_get_name: StateGetName,
    state_get_residency: StateGetResidency,
}

const LIB_PATHS: &[&str] = &["/usr/lib/libIOReport.dylib", "libIOReport.dylib"];

fn sym(handle: *mut c_void, name: &str) -> Result<*mut c_void, String> {
    let c = CString::new(name).map_err(|e| e.to_string())?;
    // SAFETY: valid handle from dlopen and C string.
    let p = unsafe { libc::dlsym(handle, c.as_ptr()) };
    if p.is_null() {
        Err(format!("IOReport symbol {name} missing"))
    } else {
        Ok(p)
    }
}

impl Api {
    fn load() -> Result<Api, String> {
        let mut handle = std::ptr::null_mut();
        for p in LIB_PATHS {
            let c = CString::new(*p).map_err(|e| e.to_string())?;
            // SAFETY: dlopen with a valid path; the handle is intentionally never closed (process lifetime).
            handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            return Err("libIOReport.dylib not loadable".into());
        }
        // SAFETY (all transmutes): the symbols have these C signatures on every macOS release that ships
        // them (as used by macmon/mactop/powermetrics clients); a missing symbol aborts loading instead.
        unsafe {
            Ok(Api {
                copy_channels_in_group: std::mem::transmute::<*mut c_void, CopyChannelsInGroup>(sym(
                    handle,
                    "IOReportCopyChannelsInGroup",
                )?),
                merge_channels: std::mem::transmute::<*mut c_void, MergeChannels>(sym(
                    handle,
                    "IOReportMergeChannels",
                )?),
                create_subscription: std::mem::transmute::<*mut c_void, CreateSubscription>(sym(
                    handle,
                    "IOReportCreateSubscription",
                )?),
                create_samples: std::mem::transmute::<*mut c_void, CreateSamples>(sym(
                    handle,
                    "IOReportCreateSamples",
                )?),
                create_samples_delta: std::mem::transmute::<*mut c_void, CreateSamplesDelta>(sym(
                    handle,
                    "IOReportCreateSamplesDelta",
                )?),
                get_group: std::mem::transmute::<*mut c_void, ChannelGetStr>(sym(
                    handle,
                    "IOReportChannelGetGroup",
                )?),
                get_subgroup: std::mem::transmute::<*mut c_void, ChannelGetStr>(sym(
                    handle,
                    "IOReportChannelGetSubGroup",
                )?),
                get_channel_name: std::mem::transmute::<*mut c_void, ChannelGetStr>(sym(
                    handle,
                    "IOReportChannelGetChannelName",
                )?),
                get_unit_label: std::mem::transmute::<*mut c_void, ChannelGetStr>(sym(
                    handle,
                    "IOReportChannelGetUnitLabel",
                )?),
                simple_get_integer: std::mem::transmute::<*mut c_void, SimpleGetInteger>(sym(
                    handle,
                    "IOReportSimpleGetIntegerValue",
                )?),
                state_get_count: std::mem::transmute::<*mut c_void, StateGetCount>(sym(
                    handle,
                    "IOReportStateGetCount",
                )?),
                state_get_name: std::mem::transmute::<*mut c_void, StateGetName>(sym(
                    handle,
                    "IOReportStateGetNameForIndex",
                )?),
                state_get_residency: std::mem::transmute::<*mut c_void, StateGetResidency>(sym(
                    handle,
                    "IOReportStateGetResidency",
                )?),
            })
        }
    }
}

/// Channel groups subscribed: (group, subgroup).
pub const GROUPS: &[(&str, Option<&str>)] = &[
    ("Energy Model", None),
    ("CPU Stats", Some("CPU Complex Performance States")),
    ("GPU Stats", Some("GPU Performance States")),
];

/// One IOReport delta, raw.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IoReportDelta {
    /// Wall-clock length of the delta window, µs.
    pub window_us: u64,
    /// "cpu.<channel>" / "gpu.<channel>" → ordered `(state name, residency ticks)`.
    pub residency: BTreeMap<String, Vec<(String, i64)>>,
    /// Energy channel name (as reported, e.g. "CPU Energy", "GPU Energy", "ANE") → nanojoules.
    pub energy_nj: BTreeMap<String, i64>,
}

/// A live IOReport subscription holding the previous sample for deltas.
pub struct IoReport {
    api: Api,
    channels: Cf,
    subscription: *const c_void,
    prev: Option<(Cf, Instant)>,
}

// SAFETY: the subscription and CF objects are only used from the owning source (one thread at a time).
unsafe impl Send for IoReport {}

impl std::fmt::Debug for IoReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoReport")
            .field("primed", &self.prev.is_some())
            .finish()
    }
}

/// Energy channels the decoder uses (`extras::keys::IOREPORT_ENERGY_KEEP`); the rest are skipped unread.
fn keep_energy(name: &str) -> bool {
    super::extras::keys::IOREPORT_ENERGY_KEEP.contains(&name)
}

fn energy_to_nj(value: i64, unit: &str) -> Option<i64> {
    let mul = match unit.trim() {
        "nJ" => 1,
        "uJ" | "µJ" => 1_000,
        "mJ" => 1_000_000,
        "J" => 1_000_000_000,
        _ => return None,
    };
    value.checked_mul(mul)
}

impl IoReport {
    /// Resolves IOReport and subscribes to [`GROUPS`]. `Err(reason)` → the source reports unavailable.
    pub fn new() -> Result<IoReport, String> {
        let api = Api::load()?;
        let mut merged: Option<Cf> = None;
        for (group, subgroup) in GROUPS {
            let g = cf::cfstr(group).ok_or("cfstr")?;
            let sg = match subgroup {
                Some(s) => Some(cf::cfstr(s).ok_or("cfstr")?),
                None => None,
            };
            // SAFETY: valid CFStrings (or null subgroup); returns +1 dictionary or null.
            let ch = unsafe {
                (api.copy_channels_in_group)(
                    g.as_ptr(),
                    sg.as_ref().map(|s| s.as_ptr()).unwrap_or(std::ptr::null()),
                    0,
                    0,
                    0,
                )
            };
            let Some(ch) = Cf::owned(ch) else { continue };
            match &merged {
                None => merged = Some(ch),
                // SAFETY: both are IOReport channel dictionaries.
                Some(m) => unsafe { (api.merge_channels)(m.as_ptr(), ch.as_ptr(), std::ptr::null()) },
            }
        }
        let merged = merged.ok_or("no IOReport channel groups available")?;
        // SAFETY: copy into a mutable dictionary as the subscription API expects.
        let channels = Cf::owned(unsafe {
            cf::CFDictionaryCreateMutableCopy(cf::kCFAllocatorDefault, 0, merged.as_ptr()) as CFTypeRef
        })
        .ok_or("CFDictionaryCreateMutableCopy failed")?;
        // Subscribing to only the 4 kept Energy Model channels (instead of the whole groups) was measured
        // to cost *more* CPU per sample on macOS 26 (whole-process A/B: 0.89 % vs 1.09 % of one core), so the
        // whole groups are subscribed and unused channels are skipped in `delta`.
        let mut subbed: CFMutableDictionaryRef = std::ptr::null_mut();
        // SAFETY: arguments per the IOReport client convention; returns null on failure.
        let subscription = unsafe {
            (api.create_subscription)(
                std::ptr::null(),
                channels.as_ptr() as CFMutableDictionaryRef,
                &mut subbed,
                0,
                std::ptr::null(),
            )
        };
        // The ownership of the "subscribed channels" out-dictionary is undocumented (private API). We sample
        // with our own `channels` copy and deliberately never release `subbed`: if it were +0 (owned by the
        // subscription), releasing it would be an over-release → use-after-free at the subscription's
        // dealloc. Worst case this leaks one dictionary once per source lifetime (macmon does the same).
        let _ = subbed;
        if subscription.is_null() {
            return Err("IOReportCreateSubscription failed".into());
        }
        Ok(IoReport {
            api,
            channels,
            subscription,
            prev: None,
        })
    }

    fn sample(&self) -> Option<Cf> {
        // SAFETY: live subscription and channel dictionary.
        Cf::owned(unsafe {
            (self.api.create_samples)(
                self.subscription,
                self.channels.as_ptr() as CFMutableDictionaryRef,
                std::ptr::null(),
            )
        })
    }

    /// Takes a sample; returns the delta against the previous one (`Ok(None)` on the first call).
    pub fn delta(&mut self) -> Result<Option<IoReportDelta>, String> {
        let now = Instant::now();
        let cur = self.sample().ok_or("IOReportCreateSamples failed")?;
        let Some((prev, t0)) = self.prev.replace((cur, now)) else {
            return Ok(None);
        };
        let cur_ptr = self
            .prev
            .as_ref()
            .map(|p| p.0.as_ptr())
            .unwrap_or(std::ptr::null());
        // SAFETY: two sample dictionaries from the same subscription.
        let delta =
            Cf::owned(unsafe { (self.api.create_samples_delta)(prev.as_ptr(), cur_ptr, std::ptr::null()) })
                .ok_or("IOReportCreateSamplesDelta failed")?;
        let mut out = IoReportDelta {
            window_us: now.duration_since(t0).as_micros() as u64,
            ..Default::default()
        };
        let items = cf::dict_get(delta.as_ptr(), "IOReportChannels")
            .map(cf::array_items)
            .unwrap_or_default();
        for item in items {
            if !cf::is_dict(item) {
                continue;
            }
            // SAFETY: `item` is an IOReport channel dictionary from the delta sample.
            unsafe {
                // Strings are converted lazily: the Energy Model group alone has ~100 per-block channels, and
                // only a few of them are kept (SPEC §14: this loop was the costliest part of a host sample).
                let group = cf::string((self.api.get_group)(item)).unwrap_or_default();
                match group.as_str() {
                    "Energy Model" => {
                        let name = cf::string((self.api.get_channel_name)(item)).unwrap_or_default();
                        if !keep_energy(&name) {
                            continue;
                        }
                        let unit = cf::string((self.api.get_unit_label)(item)).unwrap_or_default();
                        let v = (self.api.simple_get_integer)(item, 0);
                        if let Some(nj) = energy_to_nj(v, &unit) {
                            *out.energy_nj.entry(name).or_insert(0) += nj;
                        }
                    }
                    "CPU Stats" | "GPU Stats" => {
                        let prefix = if group == "CPU Stats" { "cpu" } else { "gpu" };
                        let subgroup = cf::string((self.api.get_subgroup)(item)).unwrap_or_default();
                        if !(subgroup.contains("Performance States")) {
                            continue;
                        }
                        let name = cf::string((self.api.get_channel_name)(item)).unwrap_or_default();
                        let n = (self.api.state_get_count)(item).clamp(0, 64);
                        let states: Vec<(String, i64)> = (0..n)
                            .map(|i| {
                                (
                                    cf::string((self.api.state_get_name)(item, i)).unwrap_or_default(),
                                    (self.api.state_get_residency)(item, i).max(0),
                                )
                            })
                            .collect();
                        if !states.is_empty() {
                            out.residency.insert(format!("{prefix}.{name}"), states);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(Some(out))
    }
}

impl Drop for IoReport {
    fn drop(&mut self) {
        if !self.subscription.is_null() {
            // SAFETY: the subscription is a CF object owned by us.
            unsafe { cf::CFRelease(self.subscription) };
        }
    }
}

/// Parses a `voltage-states*` blob: little-endian `(freq u32, voltage u32)` pairs → MHz. Frequencies are
/// in kHz on some SoCs (CPU tables on M4/M5) and Hz on others (GPU, M1–M3): values ≥ 10⁷ are Hz.
pub fn parse_dvfs_mhz(blob: &[u8]) -> Vec<u32> {
    blob.as_chunks::<8>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u64)
        .map(|f| {
            if f >= 10_000_000 {
                (f / 1_000_000) as u32
            } else {
                (f / 1_000) as u32
            }
        })
        .collect()
}

/// DVFS table names read from `pmgr` (ECPU, PCPU, GPU on every Apple Silicon generation so far).
pub const DVFS_TABLES: &[&str] = &["voltage-states1-sram", "voltage-states5-sram", "voltage-states9"];

/// Reads the DVFS frequency tables from the `pmgr` registry entry.
pub fn dvfs_tables() -> Result<BTreeMap<String, Vec<u32>>, String> {
    let pm = cf::services("pmgr", true);
    let entry = pm.first().ok_or("IORegistry entry pmgr not found")?;
    let mut out = BTreeMap::new();
    for t in DVFS_TABLES {
        if let Some(v) = cf::registry_property(entry, t) {
            if let Some(bytes) = cf::data(v.as_ptr()) {
                let mhz = parse_dvfs_mhz(&bytes);
                if !mhz.is_empty() {
                    out.insert(t.to_string(), mhz);
                }
            }
        }
    }
    if out.is_empty() {
        Err("pmgr has no voltage-states tables".into())
    } else {
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dvfs_units() {
        // kHz CPU table (M5 ECPU): 972000 kHz, 3048000 kHz
        let mut b = Vec::new();
        for (f, v) in [(972_000u32, 790u32), (3_048_000, 965), (0, 0), (338_000_000, 620)] {
            b.extend_from_slice(&f.to_le_bytes());
            b.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(parse_dvfs_mhz(&b), vec![972, 3048, 0, 338]);
        assert!(parse_dvfs_mhz(&[1, 2, 3]).is_empty());
    }

    #[test]
    fn energy_units() {
        assert_eq!(energy_to_nj(5, "mJ"), Some(5_000_000));
        assert_eq!(energy_to_nj(5, "uJ"), Some(5_000));
        assert_eq!(energy_to_nj(5, "nJ"), Some(5));
        assert_eq!(energy_to_nj(5, "furlongs"), None);
    }

    /// Live: IOReport either works or reports why (never panics).
    #[test]
    fn live_ioreport_degrades_or_samples() {
        match IoReport::new() {
            Ok(mut r) => {
                assert_eq!(r.delta().unwrap(), None);
                std::thread::sleep(std::time::Duration::from_millis(100));
                let d = r.delta().unwrap().expect("second sample yields a delta");
                assert!(d.window_us >= 90_000, "{d:?}");
                // The delta carries everything the decoder reads, and only the kept energy channels.
                if cfg!(target_arch = "aarch64") {
                    assert!(d.energy_nj.contains_key("CPU Energy"), "{d:?}");
                    assert!(d.residency.keys().any(|k| k.starts_with("cpu.")), "{d:?}");
                    assert!(d.residency.keys().any(|k| k.starts_with("gpu.")), "{d:?}");
                    assert!(
                        d.energy_nj.len() <= 4,
                        "only kept channels: {:?}",
                        d.energy_nj.keys()
                    );
                }
                if std::env::var_os("OOMTOP_PRINT_IOREPORT").is_some() {
                    eprintln!("{d:#?}");
                }
            }
            Err(e) => eprintln!("IOReport unavailable: {e}"),
        }
        match dvfs_tables() {
            Ok(t) => {
                if std::env::var_os("OOMTOP_PRINT_IOREPORT").is_some() {
                    eprintln!("{t:?}");
                }
            }
            Err(e) => eprintln!("dvfs unavailable: {e}"),
        }
    }
}

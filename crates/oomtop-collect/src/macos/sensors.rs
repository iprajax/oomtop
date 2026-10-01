//! Thermal / power / GPU readers (I/O only; decoding in [`super::extras`]). SPEC §9:
//! - thermal pressure: public notify key `com.apple.system.thermalpressurelevel` (`OSThermalNotification.h`),
//!   cross-checked with `NSProcessInfo.thermalState`;
//! - Low Power Mode: `NSProcessInfo.isLowPowerModeEnabled` via the Objective-C runtime;
//! - battery + adapter: IOKit power sources (`IOPSCopyPowerSourcesInfo`, `IOPSCopyExternalPowerAdapterDetails`);
//! - host GPU memory + utilization: `IOAccelerator` `PerformanceStatistics` in the IORegistry.
//!
//! Each reader writes raw values into [`MacHostRaw`] under the namespaced keys defined in
//! [`super::extras::keys`], and a reason under `unavailable.<part>` when it cannot.

use super::cf;
use super::extras::keys;
use crate::raw::MacHostRaw;
use std::ffi::{c_char, c_void, CString};

extern "C" {
    fn notify_register_check(name: *const c_char, out_token: *mut i32) -> u32;
    fn notify_get_state(token: i32, state: *mut u64) -> u32;
    fn notify_cancel(token: i32) -> u32;
}

#[link(name = "objc")]
extern "C" {
    fn objc_getClass(name: *const c_char) -> *mut c_void;
    fn sel_registerName(name: *const c_char) -> *mut c_void;
    fn objc_msgSend();
    // Returns an ObjC `BOOL` (1 byte: `bool` on arm64, `signed char` on x86_64) — read as i8, never as
    // Rust `bool` (any value other than 0/1 would be undefined behavior).
    fn class_respondsToSelector(cls: *mut c_void, sel: *mut c_void) -> i8;
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

/// Runs `f` inside an autorelease pool (sampler threads have none; nothing may leak per sample).
fn with_pool<T>(f: impl FnOnce() -> T) -> T {
    // SAFETY: balanced push/pop on the same thread.
    unsafe {
        let pool = objc_autoreleasePoolPush();
        let r = f();
        objc_autoreleasePoolPop(pool);
        r
    }
}

// NSProcessInfo lives in Foundation; linking it makes the class available to objc_getClass.
#[link(name = "Foundation", kind = "framework")]
extern "C" {}

const THERMAL_KEY: &str = "com.apple.system.thermalpressurelevel";

/// A registered notify token for the thermal-pressure key.
#[derive(Debug)]
pub struct ThermalNotify {
    token: Option<i32>,
    error: Option<String>,
}

impl ThermalNotify {
    pub fn new() -> Self {
        let Ok(c) = CString::new(THERMAL_KEY) else {
            return ThermalNotify {
                token: None,
                error: Some("bad key".into()),
            };
        };
        let mut token = 0i32;
        // SAFETY: valid C string and out pointer.
        let status = unsafe { notify_register_check(c.as_ptr(), &mut token) };
        if status == 0 {
            ThermalNotify {
                token: Some(token),
                error: None,
            }
        } else {
            ThermalNotify {
                token: None,
                error: Some(format!("notify_register_check failed ({status})")),
            }
        }
    }

    /// Current `kOSThermalPressureLevel*` value (0 nominal … 4 sleeping).
    pub fn level(&self) -> Result<u64, String> {
        let token = self.token.ok_or_else(|| self.error.clone().unwrap_or_default())?;
        let mut state = 0u64;
        // SAFETY: registered token, valid out pointer.
        let status = unsafe { notify_get_state(token, &mut state) };
        if status == 0 {
            Ok(state)
        } else {
            Err(format!("notify_get_state failed ({status})"))
        }
    }
}

impl Default for ThermalNotify {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ThermalNotify {
    fn drop(&mut self) {
        if let Some(t) = self.token {
            // SAFETY: token registered by us.
            unsafe { notify_cancel(t) };
        }
    }
}

/// `[NSProcessInfo processInfo]` queries. Selectors the running macOS does not implement are skipped
/// (`isLowPowerModeEnabled` appeared on macOS 12), never sent — an unknown selector would abort.
#[derive(Debug, Clone, Copy)]
pub struct ProcessInfo {
    obj: usize,
    sel_lpm: Option<usize>,
    sel_thermal: Option<usize>,
}

type MsgSendObj = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
/// `BOOL (*)(id, SEL)`; `BOOL` is 1 byte on both architectures, read as i8 (see `class_respondsToSelector`).
type MsgSendBool = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i8;
type MsgSendInt = unsafe extern "C" fn(*mut c_void, *mut c_void) -> isize;

impl ProcessInfo {
    pub fn new() -> Option<ProcessInfo> {
        let cls = CString::new("NSProcessInfo").ok()?;
        let s_pi = CString::new("processInfo").ok()?;
        let s_lpm = CString::new("isLowPowerModeEnabled").ok()?;
        let s_th = CString::new("thermalState").ok()?;
        // SAFETY: Objective-C runtime lookups with valid C strings; objc_msgSend is cast to the exact
        // signature of `+[NSProcessInfo processInfo]` (id (*)(Class, SEL)), a shared singleton (no
        // ownership transfer, nothing autoreleased that we keep).
        with_pool(|| unsafe {
            let class = objc_getClass(cls.as_ptr());
            if class.is_null() {
                return None;
            }
            let sel_pi = sel_registerName(s_pi.as_ptr());
            let send: MsgSendObj = std::mem::transmute::<unsafe extern "C" fn(), MsgSendObj>(objc_msgSend);
            let obj = send(class, sel_pi);
            if obj.is_null() {
                return None;
            }
            let known = |name: &CString| {
                let sel = sel_registerName(name.as_ptr());
                (class_respondsToSelector(class, sel) != 0).then_some(sel as usize)
            };
            Some(ProcessInfo {
                obj: obj as usize,
                sel_lpm: known(&s_lpm),
                sel_thermal: known(&s_th),
            })
        })
    }

    /// `isLowPowerModeEnabled` (BOOL); `None` when this macOS lacks the selector.
    pub fn low_power_mode(&self) -> Option<bool> {
        let sel = self.sel_lpm?;
        // SAFETY: the class responds to the selector (checked at construction); signature BOOL (*)(id, SEL).
        Some(with_pool(|| unsafe {
            let send: MsgSendBool = std::mem::transmute::<unsafe extern "C" fn(), MsgSendBool>(objc_msgSend);
            send(self.obj as *mut c_void, sel as *mut c_void) != 0
        }))
    }

    /// `thermalState` (NSProcessInfoThermalState: 0 nominal, 1 fair, 2 serious, 3 critical).
    pub fn thermal_state(&self) -> Option<i64> {
        let sel = self.sel_thermal?;
        // SAFETY: the class responds to the selector (checked at construction); NSInteger (*)(id, SEL).
        Some(with_pool(|| unsafe {
            let send: MsgSendInt = std::mem::transmute::<unsafe extern "C" fn(), MsgSendInt>(objc_msgSend);
            send(self.obj as *mut c_void, sel as *mut c_void) as i64
        }))
    }
}

fn unavailable(h: &mut MacHostRaw, part: &str, reason: impl Into<String>) {
    h.sysctl_str
        .insert(format!("{}{part}", keys::UNAVAILABLE_PREFIX), reason.into());
}

/// Thermal pressure + NSProcessInfo state + Low Power Mode.
pub fn read_thermal(h: &mut MacHostRaw, notify: &ThermalNotify, pi: Option<&ProcessInfo>) {
    match notify.level() {
        Ok(v) => {
            h.sysctl.insert(keys::THERMAL_NOTIFY.into(), v as i64);
        }
        Err(e) => unavailable(h, keys::PART_THERMAL, e),
    }
    match pi {
        Some(pi) => {
            if let Some(t) = pi.thermal_state() {
                h.sysctl.insert(keys::THERMAL_STATE.into(), t);
            }
            match pi.low_power_mode() {
                Some(l) => {
                    h.sysctl.insert(keys::LOW_POWER_MODE.into(), l as i64);
                }
                None => unavailable(
                    h,
                    keys::PART_LOW_POWER,
                    "NSProcessInfo.isLowPowerModeEnabled needs macOS 12+",
                ),
            }
        }
        None => unavailable(h, keys::PART_LOW_POWER, "NSProcessInfo unavailable"),
    }
}

/// Battery + external adapter via IOKit power sources.
pub fn read_power(h: &mut MacHostRaw) {
    // SAFETY: Copy functions return +1 or null; wrapped in Cf for release.
    let Some(blob) = cf::Cf::owned(unsafe { cf::IOPSCopyPowerSourcesInfo() }) else {
        unavailable(h, keys::PART_POWER, "IOPSCopyPowerSourcesInfo returned nothing");
        return;
    };
    // SAFETY: borrowed (+0) CFString from the snapshot blob.
    let providing = cf::string(unsafe { cf::IOPSGetProvidingPowerSourceType(blob.as_ptr()) });
    if let Some(p) = &providing {
        h.sysctl.insert(keys::PS_ON_AC.into(), (p == "AC Power") as i64);
        h.sysctl_str.insert(keys::PS_PROVIDING.into(), p.clone());
    }
    // SAFETY: +1 array or null.
    let list = cf::Cf::owned(unsafe { cf::IOPSCopyPowerSourcesList(blob.as_ptr()) });
    let mut found_battery = false;
    if let Some(list) = &list {
        for ps in cf::array_items(list.as_ptr()) {
            // SAFETY: borrowed description dictionary.
            let d = unsafe { cf::IOPSGetPowerSourceDescription(blob.as_ptr(), ps) };
            if d.is_null() || cf::dict_string(d, "Type").as_deref() != Some("InternalBattery") {
                continue;
            }
            found_battery = true;
            h.sysctl.insert(keys::BATT_PRESENT.into(), 1);
            for (src, dst) in [
                ("Current Capacity", keys::BATT_CURRENT),
                ("Max Capacity", keys::BATT_MAX),
                ("Is Charging", keys::BATT_CHARGING),
                ("Time to Empty", keys::BATT_TIME_TO_EMPTY),
            ] {
                if let Some(v) = cf::dict_int(d, src) {
                    h.sysctl.insert(dst.into(), v);
                }
            }
            if let Some(s) = cf::dict_string(d, "Power Source State") {
                h.sysctl.insert(keys::BATT_ON_AC.into(), (s == "AC Power") as i64);
            }
            break;
        }
    }
    if !found_battery {
        h.sysctl.insert(keys::BATT_PRESENT.into(), 0);
    }
    // SAFETY: +1 dictionary or null (no adapter).
    if let Some(ad) = cf::Cf::owned(unsafe { cf::IOPSCopyExternalPowerAdapterDetails() as cf::CFTypeRef }) {
        if let Some(w) = cf::dict_int(ad.as_ptr(), "Watts") {
            h.sysctl.insert(keys::ADAPTER_WATTS.into(), w);
        }
    }
}

/// `IOAccelerator` → `PerformanceStatistics` (host GPU memory in use, utilization) + model/core count.
pub fn read_ioaccel(h: &mut MacHostRaw) {
    let svcs = cf::services("IOAccelerator", false);
    let Some(svc) = svcs.first() else {
        unavailable(h, keys::PART_IOACCEL, "no IOAccelerator service");
        return;
    };
    match cf::registry_property(svc, "PerformanceStatistics") {
        Some(stats) if cf::is_dict(stats.as_ptr()) => {
            for k in keys::IOACCEL_STATS {
                if let Some(v) = cf::dict_int(stats.as_ptr(), k) {
                    h.sysctl.insert(format!("{}{k}", keys::IOACCEL_PREFIX), v);
                }
            }
        }
        _ => unavailable(h, keys::PART_IOACCEL, "PerformanceStatistics missing"),
    }
    if let Some(v) = cf::registry_property(svc, "gpu-core-count").and_then(|v| cf::int(v.as_ptr())) {
        h.sysctl.insert(keys::IOACCEL_CORES.into(), v);
    }
    if let Some(v) = cf::registry_property(svc, "model").and_then(|v| cf::string(v.as_ptr())) {
        h.sysctl_str.insert(keys::IOACCEL_MODEL.into(), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_readers_fill_keys_or_reasons() {
        let mut h = MacHostRaw::default();
        let n = ThermalNotify::new();
        let pi = ProcessInfo::new();
        read_thermal(&mut h, &n, pi.as_ref());
        read_power(&mut h);
        read_ioaccel(&mut h);
        let has = |k: &str| h.sysctl.contains_key(k);
        let why = |p: &str| {
            h.sysctl_str
                .contains_key(&format!("{}{p}", keys::UNAVAILABLE_PREFIX))
        };
        assert!(has(keys::THERMAL_NOTIFY) || why(keys::PART_THERMAL), "{h:?}");
        assert!(has(keys::LOW_POWER_MODE) || why(keys::PART_LOW_POWER), "{h:?}");
        assert!(has(keys::BATT_PRESENT) || why(keys::PART_POWER), "{h:?}");
        if let Some(v) = h.sysctl.get(keys::THERMAL_NOTIFY) {
            assert!((0..=4).contains(v), "thermal level {v}");
        }
        if let Some(v) = h.sysctl.get(keys::THERMAL_STATE) {
            assert!((0..=3).contains(v), "thermal state {v}");
        }
        if std::env::var_os("OOMTOP_PRINT_SENSORS").is_some() {
            eprintln!("{:#?}", (&h.sysctl, &h.sysctl_str));
        }
    }
}

//! NVIDIA NVML via `dlopen("libnvidia-ml.so.1")` (SPEC §5, §9, §16). No link-time dependency: the static
//! musl build and hosts without the driver degrade to `unavailable(reason)`. Symbols are resolved one by
//! one, so an old driver missing a call degrades that field only. The library is loaded once per process
//! and never unloaded; a failed load is retried after [`RETRY_MS`].
//!
//! Results are converted to the text form of [`super::gpu`] (`@nvml`, `@nvml/procs` keys) so the decoder
//! stays pure and NVIDIA fixtures replay anywhere.

use super::gpu::{NvmlDevice, NvmlHost};
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::sync::{Mutex, OnceLock};

/// Retry period after a failed load/init.
pub const RETRY_MS: u64 = 300_000;
/// NVML soname (the unversioned `libnvidia-ml.so` only exists with dev packages).
pub const LIBRARY: &CStr = c"libnvidia-ml.so.1";

type Ret = c_int;
type Dev = *mut c_void;
const SUCCESS: Ret = 0;
const ERROR_INSUFFICIENT_SIZE: Ret = 7;
const TEMPERATURE_GPU: c_int = 0;
const CLOCK_SM: c_int = 1;
/// `NVML_VALUE_NOT_AVAILABLE` for `usedGpuMemory`.
const VALUE_NOT_AVAILABLE: u64 = u64::MAX;

#[repr(C)]
#[allow(dead_code)] // FFI layout: every field must exist
struct Memory {
    total: u64,
    free: u64,
    used: u64,
}

#[repr(C)]
#[allow(dead_code)] // FFI layout: every field must exist
struct Utilization {
    gpu: c_uint,
    memory: c_uint,
}

/// `nvmlProcessInfo_t` as used by the `_v2`/`_v3` process calls.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // FFI layout: every field must exist
struct ProcInfo {
    pid: c_uint,
    used_gpu_memory: u64,
    gpu_instance_id: c_uint,
    compute_instance_id: c_uint,
}

/// `nvmlProcessInfo_v1_t` (legacy unversioned call).
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // FFI layout: every field must exist
struct ProcInfoV1 {
    pid: c_uint,
    used_gpu_memory: u64,
}

#[repr(C)]
#[allow(dead_code)] // FFI layout: every field must exist
struct PciInfo {
    bus_id_legacy: [c_char; 16],
    domain: c_uint,
    bus: c_uint,
    device: c_uint,
    pci_device_id: c_uint,
    pci_sub_system_id: c_uint,
    bus_id: [c_char; 32],
}

type FnVoid = unsafe extern "C" fn() -> Ret;
type FnCount = unsafe extern "C" fn(*mut c_uint) -> Ret;
type FnHandle = unsafe extern "C" fn(c_uint, *mut Dev) -> Ret;
type FnStr = unsafe extern "C" fn(Dev, *mut c_char, c_uint) -> Ret;
type FnSysStr = unsafe extern "C" fn(*mut c_char, c_uint) -> Ret;
type FnMem = unsafe extern "C" fn(Dev, *mut Memory) -> Ret;
type FnUtil = unsafe extern "C" fn(Dev, *mut Utilization) -> Ret;
type FnU32 = unsafe extern "C" fn(Dev, *mut c_uint) -> Ret;
type FnSensorU32 = unsafe extern "C" fn(Dev, c_int, *mut c_uint) -> Ret;
type FnU64 = unsafe extern "C" fn(Dev, *mut u64) -> Ret;
type FnPci = unsafe extern "C" fn(Dev, *mut PciInfo) -> Ret;
type FnProcs = unsafe extern "C" fn(Dev, *mut c_uint, *mut c_void) -> Ret;
type FnErr = unsafe extern "C" fn(Ret) -> *const c_char;

struct Api {
    init: FnVoid,
    count: FnCount,
    handle: FnHandle,
    error_string: Option<FnErr>,
    driver_version: Option<FnSysStr>,
    name: Option<FnStr>,
    memory: Option<FnMem>,
    utilization: Option<FnUtil>,
    power: Option<FnU32>,
    power_limit: Option<FnU32>,
    temperature: Option<FnSensorU32>,
    clock: Option<FnSensorU32>,
    max_clock: Option<FnSensorU32>,
    reasons: Option<FnU64>,
    pci: Option<FnPci>,
    /// (function, uses the v1 struct)
    compute_procs: Option<(FnProcs, bool)>,
    graphics_procs: Option<(FnProcs, bool)>,
}

// The Api only holds plain function pointers (already `Send`) into a library that is never unloaded; NVML
// calls are thread-safe (documented), and access is additionally serialized by the global Mutex.

/// The NUL-terminated string in a fixed-size C buffer, never reading past its end (NVML documents NUL
/// termination, but a buffer filled to the brim must not make us read beyond it).
fn buf_str(buf: &[c_char]) -> String {
    // `c_char` is `i8` on x86_64 and `u8` on aarch64 Linux: reinterpret the byte on both without a cast
    // (which clippy flags as a no-op on aarch64).
    let bytes: Vec<u8> = buf
        .iter()
        .map(|c| c.to_ne_bytes()[0])
        .take_while(|b| *b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

enum State {
    Untried,
    Ready(Api),
    Failed { reason: String, retry_at_ms: u64 },
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::Untried))
}

/// dlsym → typed function pointer.
///
/// # Safety
/// `T` must be the exact C signature of `name`.
unsafe fn sym<T: Copy>(lib: *mut c_void, name: &CStr) -> Option<T> {
    // SAFETY: lib is a live handle from dlopen; name is NUL-terminated.
    let p = unsafe { libc::dlsym(lib, name.as_ptr()) };
    if p.is_null() {
        return None;
    }
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<*mut c_void>());
    // SAFETY: caller guarantees T is a function pointer type matching the symbol.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

fn load() -> Result<Api, String> {
    // SAFETY: dlopen with a static NUL-terminated name; failure returns NULL.
    let lib = unsafe { libc::dlopen(LIBRARY.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if lib.is_null() {
        // SAFETY: dlerror returns a thread-local C string or NULL.
        let err = unsafe {
            let e = libc::dlerror();
            if e.is_null() {
                "not found".to_string()
            } else {
                CStr::from_ptr(e).to_string_lossy().into_owned()
            }
        };
        return Err(format!("libnvidia-ml.so.1 not loadable ({err})"));
    }
    // SAFETY (all `sym` calls): signatures transcribed from nvml.h (CUDA 12).
    unsafe {
        let init: FnVoid = sym(lib, c"nvmlInit_v2")
            .or_else(|| sym(lib, c"nvmlInit"))
            .ok_or("nvmlInit missing")?;
        let count: FnCount = sym(lib, c"nvmlDeviceGetCount_v2")
            .or_else(|| sym(lib, c"nvmlDeviceGetCount"))
            .ok_or("nvmlDeviceGetCount missing")?;
        let handle: FnHandle = sym(lib, c"nvmlDeviceGetHandleByIndex_v2")
            .or_else(|| sym(lib, c"nvmlDeviceGetHandleByIndex"))
            .ok_or("nvmlDeviceGetHandleByIndex missing")?;
        let procs = |v3: &CStr, v2: &CStr, v1: &CStr| -> Option<(FnProcs, bool)> {
            sym::<FnProcs>(lib, v3)
                .map(|f| (f, false))
                .or_else(|| sym::<FnProcs>(lib, v2).map(|f| (f, false)))
                .or_else(|| sym::<FnProcs>(lib, v1).map(|f| (f, true)))
        };
        let api = Api {
            init,
            count,
            handle,
            error_string: sym(lib, c"nvmlErrorString"),
            driver_version: sym(lib, c"nvmlSystemGetDriverVersion"),
            name: sym(lib, c"nvmlDeviceGetName"),
            memory: sym(lib, c"nvmlDeviceGetMemoryInfo"),
            utilization: sym(lib, c"nvmlDeviceGetUtilizationRates"),
            power: sym(lib, c"nvmlDeviceGetPowerUsage"),
            power_limit: sym(lib, c"nvmlDeviceGetEnforcedPowerLimit"),
            temperature: sym(lib, c"nvmlDeviceGetTemperature"),
            clock: sym(lib, c"nvmlDeviceGetClockInfo"),
            max_clock: sym(lib, c"nvmlDeviceGetMaxClockInfo"),
            reasons: sym(lib, c"nvmlDeviceGetCurrentClocksEventReasons")
                .or_else(|| sym(lib, c"nvmlDeviceGetCurrentClocksThrottleReasons")),
            pci: sym(lib, c"nvmlDeviceGetPciInfo_v3"),
            compute_procs: procs(
                c"nvmlDeviceGetComputeRunningProcesses_v3",
                c"nvmlDeviceGetComputeRunningProcesses_v2",
                c"nvmlDeviceGetComputeRunningProcesses",
            ),
            graphics_procs: procs(
                c"nvmlDeviceGetGraphicsRunningProcesses_v3",
                c"nvmlDeviceGetGraphicsRunningProcesses_v2",
                c"nvmlDeviceGetGraphicsRunningProcesses",
            ),
        };
        let r = (api.init)();
        if r != SUCCESS {
            return Err(format!("nvmlInit failed: {}", api.err(r)));
        }
        Ok(api)
    }
}

impl Api {
    fn err(&self, r: Ret) -> String {
        if let Some(f) = self.error_string {
            // SAFETY: nvmlErrorString returns a static C string for any code.
            let p = unsafe { f(r) };
            if !p.is_null() {
                // SAFETY: non-null static NUL-terminated string.
                return unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
            }
        }
        format!("NVML error {r}")
    }

    fn devices(&self) -> Result<Vec<Dev>, String> {
        let mut n: c_uint = 0;
        // SAFETY: valid out pointer.
        let r = unsafe { (self.count)(&mut n) };
        if r != SUCCESS {
            return Err(format!("nvmlDeviceGetCount: {}", self.err(r)));
        }
        let mut out = Vec::new();
        for i in 0..n.min(64) {
            let mut d: Dev = std::ptr::null_mut();
            // SAFETY: valid out pointer; index < count.
            if unsafe { (self.handle)(i, &mut d) } == SUCCESS {
                out.push(d);
            } else {
                out.push(std::ptr::null_mut());
            }
        }
        Ok(out)
    }

    fn device(&self, index: u32, d: Dev) -> NvmlDevice {
        let mut dev = NvmlDevice {
            index,
            ..Default::default()
        };
        if d.is_null() {
            dev.errors.insert("handle".into(), "unavailable".into());
            return dev;
        }
        let note = |dev: &mut NvmlDevice, k: &str, r: Ret| {
            if r != SUCCESS {
                dev.errors.insert(k.to_string(), self.err(r));
            }
            r == SUCCESS
        };
        // SAFETY (all calls below): d is a valid handle from nvmlDeviceGetHandleByIndex; out pointers are
        // valid for the declared struct sizes.
        unsafe {
            if let Some(f) = self.name {
                let mut buf = [0 as c_char; 96];
                if note(&mut dev, "name", f(d, buf.as_mut_ptr(), buf.len() as c_uint)) {
                    dev.name = buf_str(&buf);
                }
            }
            if let Some(f) = self.pci {
                let mut p: PciInfo = std::mem::zeroed();
                if note(&mut dev, "pci", f(d, &mut p)) {
                    dev.pci_bus_id = Some(buf_str(&p.bus_id)).filter(|b| !b.is_empty());
                }
            }
            if let Some(f) = self.memory {
                let mut m = Memory {
                    total: 0,
                    free: 0,
                    used: 0,
                };
                if note(&mut dev, "mem_total", f(d, &mut m)) {
                    dev.mem_total = Some(m.total);
                    dev.mem_used = Some(m.used);
                }
            }
            if let Some(f) = self.utilization {
                let mut u = Utilization { gpu: 0, memory: 0 };
                if note(&mut dev, "util_gpu", f(d, &mut u)) {
                    dev.util_gpu = Some(u.gpu);
                }
            }
            let mut v: c_uint = 0;
            if let Some(f) = self.power {
                if note(&mut dev, "power_mw", f(d, &mut v)) {
                    dev.power_mw = Some(v);
                }
            }
            if let Some(f) = self.power_limit {
                if note(&mut dev, "power_limit_mw", f(d, &mut v)) {
                    dev.power_limit_mw = Some(v);
                }
            }
            if let Some(f) = self.temperature {
                if note(&mut dev, "temp_c", f(d, TEMPERATURE_GPU, &mut v)) {
                    dev.temp_c = Some(v);
                }
            }
            if let Some(f) = self.clock {
                if note(&mut dev, "clock_sm_mhz", f(d, CLOCK_SM, &mut v)) {
                    dev.clock_sm_mhz = Some(v);
                }
            }
            if let Some(f) = self.max_clock {
                if note(&mut dev, "max_clock_sm_mhz", f(d, CLOCK_SM, &mut v)) {
                    dev.max_clock_sm_mhz = Some(v);
                }
            }
            if let Some(f) = self.reasons {
                let mut bits: u64 = 0;
                if note(&mut dev, "throttle_reasons", f(d, &mut bits)) {
                    dev.throttle_reasons = Some(bits);
                }
            }
        }
        dev
    }

    fn procs_of(&self, d: Dev, f: (FnProcs, bool)) -> Vec<(u32, Option<u64>)> {
        let (func, v1) = f;
        let mut cap: usize = 64;
        for _ in 0..3 {
            let mut n = cap as c_uint;
            let (res, out) = if v1 {
                let mut buf = vec![
                    ProcInfoV1 {
                        pid: 0,
                        used_gpu_memory: 0
                    };
                    cap
                ];
                // SAFETY: buffer holds `cap` v1 structs; n = capacity on input.
                let r = unsafe { func(d, &mut n, buf.as_mut_ptr().cast()) };
                let k = (n as usize).min(cap);
                (
                    r,
                    buf[..k]
                        .iter()
                        .map(|p| (p.pid, p.used_gpu_memory))
                        .collect::<Vec<_>>(),
                )
            } else {
                let mut buf = vec![
                    ProcInfo {
                        pid: 0,
                        used_gpu_memory: 0,
                        gpu_instance_id: 0,
                        compute_instance_id: 0
                    };
                    cap
                ];
                // SAFETY: buffer holds `cap` structs; n = capacity on input.
                let r = unsafe { func(d, &mut n, buf.as_mut_ptr().cast()) };
                let k = (n as usize).min(cap);
                (
                    r,
                    buf[..k]
                        .iter()
                        .map(|p| (p.pid, p.used_gpu_memory))
                        .collect::<Vec<_>>(),
                )
            };
            match res {
                SUCCESS => {
                    return out
                        .into_iter()
                        .map(|(pid, used)| (pid, (used != VALUE_NOT_AVAILABLE).then_some(used)))
                        .collect()
                }
                ERROR_INSUFFICIENT_SIZE => cap = (n as usize + 16).max(cap * 2).min(4096),
                _ => return Vec::new(),
            }
        }
        Vec::new()
    }
}

fn with_api<T>(now_ms: u64, f: impl FnOnce(&Api) -> Result<T, String>) -> Result<T, String> {
    let mut st = state().lock().unwrap_or_else(|p| p.into_inner());
    let retry = match &*st {
        State::Untried => true,
        State::Failed { retry_at_ms, .. } => now_ms >= *retry_at_ms,
        State::Ready(_) => false,
    };
    if retry {
        *st = match load() {
            Ok(api) => State::Ready(api),
            Err(reason) => State::Failed {
                reason,
                retry_at_ms: now_ms + RETRY_MS,
            },
        };
    }
    match &*st {
        State::Ready(api) => f(api),
        State::Failed { reason, .. } => Err(reason.clone()),
        State::Untried => Err("NVML not loaded".into()),
    }
}

/// Queries every device. `Err(reason)` when NVML is unavailable.
pub fn query_host(now_ms: u64) -> Result<NvmlHost, String> {
    with_api(now_ms, |api| {
        let driver = api.driver_version.and_then(|f| {
            let mut buf = [0 as c_char; 96];
            // SAFETY: buffer of the declared length.
            (unsafe { f(buf.as_mut_ptr(), buf.len() as c_uint) } == SUCCESS).then(|| buf_str(&buf))
        });
        let devices = api
            .devices()?
            .into_iter()
            .enumerate()
            .map(|(i, d)| api.device(i as u32, d))
            .collect();
        Ok(NvmlHost { driver, devices })
    })
}

/// Per-process GPU memory `(gpu index, pid, bytes)` over compute + graphics clients (a pid using both on
/// one GPU is reported once, with the larger value).
pub fn query_procs(now_ms: u64) -> Result<Vec<(u32, u32, Option<u64>)>, String> {
    with_api(now_ms, |api| {
        let mut out: Vec<(u32, u32, Option<u64>)> = Vec::new();
        for (i, d) in api.devices()?.into_iter().enumerate() {
            if d.is_null() {
                continue;
            }
            let mut per: std::collections::BTreeMap<u32, Option<u64>> = Default::default();
            for f in [api.compute_procs, api.graphics_procs].into_iter().flatten() {
                for (pid, used) in api.procs_of(d, f) {
                    let e = per.entry(pid).or_insert(None);
                    *e = match (*e, used) {
                        (Some(a), Some(b)) => Some(a.max(b)),
                        (a, b) => a.or(b),
                    };
                }
            }
            out.extend(per.into_iter().map(|(pid, b)| (i as u32, pid, b)));
        }
        Ok(out)
    })
}

/// Text for `@nvml/procs`.
pub fn procs_to_text(p: &[(u32, u32, Option<u64>)]) -> String {
    p.iter()
        .map(|(g, pid, b)| match b {
            Some(b) => format!("{g} {pid} {b}\n"),
            None => format!("{g} {pid} -\n"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_library_degrades_without_panicking() {
        // No NVIDIA driver on CI runners or macOS: both calls must return a reason, twice (cached).
        if std::path::Path::new("/proc/driver/nvidia/version").exists() {
            return; // a real NVIDIA host: nothing to assert here
        }
        let e = query_host(1).unwrap_err();
        assert!(e.contains("libnvidia-ml"), "{e}");
        assert!(query_procs(2).is_err());
    }

    #[test]
    fn fixed_buffers_are_read_within_bounds() {
        let mut buf = [b'x' as c_char; 8];
        assert_eq!(
            buf_str(&buf),
            "xxxxxxxx",
            "no NUL → stops at the end of the buffer"
        );
        buf[3] = 0;
        assert_eq!(buf_str(&buf), "xxx");
        assert_eq!(buf_str(&[0 as c_char; 4]), "");
    }

    #[test]
    fn struct_layouts_match_nvml_h() {
        assert_eq!(std::mem::size_of::<ProcInfo>(), 24);
        assert_eq!(std::mem::size_of::<ProcInfoV1>(), 16);
        assert_eq!(std::mem::size_of::<Memory>(), 24);
        assert_eq!(std::mem::size_of::<PciInfo>(), 68);
        assert_eq!(
            procs_to_text(&[(0, 42, Some(1024)), (1, 7, None)]),
            "0 42 1024\n1 7 -\n"
        );
    }
}

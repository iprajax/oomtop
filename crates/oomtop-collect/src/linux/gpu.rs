//! Pure GPU decoders: DRM client fdinfo (`Documentation/gpu/drm-usage-stats.rst`), amdgpu `gpu_metrics`
//! (`include/linux/kgd_pp_interface.h`), and the text form of NVML results that the Source stores in a
//! `RawSample` (so NVIDIA fixtures replay without a GPU).

use std::collections::BTreeMap;

// ---------------------------------------------------------------------------------------------------------
// DRM fdinfo
// ---------------------------------------------------------------------------------------------------------

/// One DRM client as seen through `/proc/<pid>/fdinfo/<fd>`. Keys are discovered at runtime: any
/// `drm-{memory,resident,total,shared,purgeable,active}-<region>` is kept, so new drivers/regions work.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DrmClient {
    pub driver: String,
    /// PCI address (`drm-pdev`), e.g. "0000:03:00.0".
    pub pdev: Option<String>,
    pub client_id: Option<u64>,
    /// region → bytes, per category (`memory` = legacy amdgpu resident, `resident`, `total`, `shared`).
    pub memory: BTreeMap<String, u64>,
    pub resident: BTreeMap<String, u64>,
    pub total: BTreeMap<String, u64>,
    pub shared: BTreeMap<String, u64>,
    /// engine → busy ns (`drm-engine-<name>`).
    pub engines: BTreeMap<String, u64>,
}

fn drm_size(v: &str) -> Option<u64> {
    let mut it = v.split_whitespace();
    let n: u64 = it.next()?.parse().ok()?;
    let mul = match it.next() {
        None => 1,
        Some("KiB") => 1 << 10,
        Some("MiB") => 1 << 20,
        Some("GiB") => 1 << 30,
        Some(_) => return None,
    };
    n.checked_mul(mul)
}

/// Parses one fdinfo file; `None` unless it belongs to a DRM client (`drm-driver:` present).
pub fn parse_drm_fdinfo(text: &str) -> Option<DrmClient> {
    let mut c = DrmClient::default();
    let mut is_drm = false;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k == "drm-driver" {
            is_drm = true;
            c.driver = v.to_string();
            continue;
        }
        if k == "drm-pdev" {
            c.pdev = Some(v.to_string());
            continue;
        }
        if k == "drm-client-id" {
            c.client_id = v.parse().ok();
            continue;
        }
        if let Some(e) = k.strip_prefix("drm-engine-") {
            if let Some(ns) = v.split_whitespace().next().and_then(|n| n.parse().ok()) {
                c.engines.insert(e.to_string(), ns);
            }
            continue;
        }
        let (map, region) = if let Some(r) = k.strip_prefix("drm-memory-") {
            (&mut c.memory, r)
        } else if let Some(r) = k.strip_prefix("drm-resident-") {
            (&mut c.resident, r)
        } else if let Some(r) = k.strip_prefix("drm-total-") {
            (&mut c.total, r)
        } else if let Some(r) = k.strip_prefix("drm-shared-") {
            (&mut c.shared, r)
        } else {
            continue;
        };
        if let Some(b) = drm_size(v) {
            map.insert(region.to_string(), b);
        }
    }
    is_drm.then_some(c)
}

/// Device-local memory regions (dGPU VRAM / Intel local memory). Everything else is host RAM.
pub fn is_device_region(region: &str) -> bool {
    region.starts_with("vram") || region.starts_with("local")
}

impl DrmClient {
    /// Resident bytes per region: `drm-resident-*` → legacy `drm-memory-*` → `drm-total-*`.
    pub fn resident_by_region(&self) -> BTreeMap<String, u64> {
        let mut regions: Vec<&String> = self
            .resident
            .keys()
            .chain(self.memory.keys())
            .chain(self.total.keys())
            .collect();
        regions.sort();
        regions.dedup();
        regions
            .into_iter()
            .filter_map(|r| {
                let v = self
                    .resident
                    .get(r)
                    .or_else(|| self.memory.get(r))
                    .or_else(|| self.total.get(r))?;
                Some((r.clone(), *v))
            })
            .collect()
    }

    /// GPU memory attributable to this client and the regions used (see [`DrmClient::gpu_bytes_for`]).
    pub fn gpu_bytes(&self) -> (u64, Vec<String>) {
        self.gpu_bytes_for(false)
    }

    /// Discrete GPUs: device-local regions (VRAM / local memory) when the driver reports any. Unified
    /// devices (APUs, iGPUs) or drivers without device regions: every region except amdgpu's CPU-domain
    /// `cpu` region — on an APU most allocations live in GTT (host RAM).
    pub fn gpu_bytes_for(&self, unified: bool) -> (u64, Vec<String>) {
        let by = self.resident_by_region();
        let device: Vec<(&String, &u64)> = by.iter().filter(|(r, _)| is_device_region(r)).collect();
        let chosen: Vec<(&String, &u64)> = if unified || device.is_empty() {
            by.iter().filter(|(r, _)| r.as_str() != "cpu").collect()
        } else {
            device
        };
        let total = chosen.iter().map(|(_, v)| **v).sum();
        (total, chosen.into_iter().map(|(r, _)| r.clone()).collect())
    }
}

/// A DRM card as listed in `@gpu/devices` (`card <pci> <driver> <vram_total|->`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardInfo {
    pub pci: String,
    pub driver: String,
    pub vram_total: Option<u64>,
}

impl CardInfo {
    /// Shares host RAM: Intel iGPUs, amdgpu APUs (BIOS carve-out ≤ 2 GiB).
    pub fn unified(&self) -> bool {
        match self.driver.as_str() {
            "i915" | "xe" => self.vram_total.is_none(),
            "amdgpu" => self.vram_total.is_some_and(|v| v <= 2 << 30),
            _ => false,
        }
    }
}

/// Parses the `card …` lines of `@gpu/devices`.
pub fn parse_cards(text: &str) -> Vec<CardInfo> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.strip_prefix("card ")?.split_whitespace();
            Some(CardInfo {
                pci: it.next()?.to_string(),
                driver: it.next()?.to_string(),
                vram_total: it.next().and_then(|v| v.parse().ok()),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------------------
// amdgpu gpu_metrics
// ---------------------------------------------------------------------------------------------------------

/// Decoded subset of amdgpu `gpu_metrics` (formats 1.0–1.3 dGPU, 2.0–2.4 and 3.0 APU). Offsets were checked
/// with `offsetof` against upstream `kgd_pp_interface.h` (2026-09); the tests pin them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GpuMetrics {
    pub format: u8,
    pub content: u8,
    /// Format 2/3 = APU (shares host RAM).
    pub apu: bool,
    pub temp_edge_c: Option<f64>,
    pub temp_hotspot_c: Option<f64>,
    pub gfx_activity_pct: Option<f64>,
    pub socket_power_w: Option<f64>,
    pub avg_gfxclk_mhz: Option<f64>,
    pub cur_gfxclk_mhz: Option<f64>,
    pub throttle_status: Option<u32>,
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let v = u16::from_le_bytes([*b.get(off)?, *b.get(off + 1)?]);
    // The SMU fills unsupported fields with 0xFFFF.
    (v != u16::MAX).then_some(v)
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
    (v != u32::MAX).then_some(v)
}

/// Byte offsets of the fields we decode, per layout.
struct Layout {
    edge: Option<usize>,
    hotspot: Option<usize>,
    activity: usize,
    /// Activity unit divisor (1 = %, 100 = centi-%).
    activity_div: f64,
    power: usize,
    /// Power is a u32 (v3.0) instead of a u16.
    power_u32: bool,
    /// Power unit divisor (1 = W, 1000 = mW).
    power_div: f64,
    /// Temperature divisor (1 = °C, 100 = centi-°C).
    temp_div: f64,
    avg_gfxclk: usize,
    cur_gfxclk: Option<usize>,
    throttle: Option<usize>,
}

/// Decodes a `gpu_metrics` blob. Unknown formats (1.4+ datacenter, 2.5+, 3.1+) return `Err(reason)`;
/// truncated blobs yield whatever fields fit.
pub fn parse_gpu_metrics(b: &[u8]) -> Result<GpuMetrics, String> {
    if b.len() < 4 {
        return Err("gpu_metrics shorter than its header".into());
    }
    let size = u16::from_le_bytes([b[0], b[1]]) as usize;
    let (format, content) = (b[2], b[3]);
    let b = &b[..size.clamp(4, b.len())];
    let l = match (format, content) {
        // v1.0: header(4) pad(4) system_clock_counter u64 @8, temps u16×6 @16, activity u16×3 @28,
        // socket_power u16 @34 (W), energy u32 @36, avg clocks u16×7 @40, cur clocks u16×7 @54,
        // throttle_status u32 @68.
        (1, 0) => Layout {
            edge: Some(16),
            hotspot: Some(18),
            activity: 28,
            activity_div: 1.0,
            power: 34,
            power_u32: false,
            power_div: 1.0,
            temp_div: 1.0,
            avg_gfxclk: 40,
            cur_gfxclk: Some(54),
            throttle: Some(68),
        },
        // v1.1–1.3: temps @4..16, activity @16..22, socket_power @22 (W), energy u64 @24, counter u64 @32,
        // avg clocks @40, cur clocks @54, throttle_status @68. (1.4+ is the MI300 layout: not decoded.)
        (1, 1..=3) => Layout {
            edge: Some(4),
            hotspot: Some(6),
            activity: 16,
            activity_div: 1.0,
            power: 22,
            power_u32: false,
            power_div: 1.0,
            temp_div: 1.0,
            avg_gfxclk: 40,
            cur_gfxclk: Some(54),
            throttle: Some(68),
        },
        // v2.0 (APU): header(4) pad(4) counter u64 @8, temperature_gfx @16, activity @40, socket_power @44,
        // avg clocks @68, cur clocks @80, throttle_status @112.
        (2, 0) => Layout {
            edge: Some(16),
            hotspot: None,
            activity: 40,
            activity_div: 100.0,
            power: 44,
            power_u32: false,
            power_div: 1000.0,
            temp_div: 100.0,
            avg_gfxclk: 68,
            cur_gfxclk: Some(80),
            throttle: Some(112),
        },
        // v2.1–2.4 (APU; the counter moved after the activity fields): temperature_gfx @4, activity @28,
        // counter u64 @32, socket_power @40, avg clocks @64, cur clocks @76, throttle_status @108.
        // Temperatures centi-°C, activity centi-% (`GfxActivity / 100` in the SMU drivers), power mW.
        (2, 1..=4) => Layout {
            edge: Some(4),
            hotspot: None,
            activity: 28,
            activity_div: 100.0,
            power: 40,
            power_u32: false,
            power_div: 1000.0,
            temp_div: 100.0,
            avg_gfxclk: 64,
            cur_gfxclk: Some(76),
            throttle: Some(108),
        },
        // v3.0 (Strix/Krackan APU): temperature_gfx @4 (centi-°C), average_gfx_activity @42 (%),
        // average_socket_power u32 @112 (mW), average_gfxclk_frequency @174. No current clock / status word.
        (3, 0) => Layout {
            edge: Some(4),
            hotspot: None,
            activity: 42,
            activity_div: 1.0,
            power: 112,
            power_u32: true,
            power_div: 1000.0,
            temp_div: 100.0,
            avg_gfxclk: 174,
            cur_gfxclk: None,
            throttle: None,
        },
        (f, c) => return Err(format!("gpu_metrics format {f}.{c} not decoded")),
    };
    let temp = |off: Option<usize>| {
        off.and_then(|o| u16_at(b, o))
            .map(|v| f64::from(v) / l.temp_div)
            .filter(|c| (-60.0..=250.0).contains(c))
    };
    Ok(GpuMetrics {
        format,
        content,
        apu: format >= 2,
        temp_edge_c: temp(l.edge),
        temp_hotspot_c: temp(l.hotspot),
        gfx_activity_pct: u16_at(b, l.activity).map(|v| (f64::from(v) / l.activity_div).min(100.0)),
        socket_power_w: if l.power_u32 {
            u32_at(b, l.power).map(|v| f64::from(v) / l.power_div)
        } else {
            u16_at(b, l.power).map(|v| f64::from(v) / l.power_div)
        },
        avg_gfxclk_mhz: u16_at(b, l.avg_gfxclk).map(f64::from),
        cur_gfxclk_mhz: l.cur_gfxclk.and_then(|o| u16_at(b, o)).map(f64::from),
        throttle_status: l.throttle.and_then(|o| u32_at(b, o)),
    })
}

// ---------------------------------------------------------------------------------------------------------
// NVML text form
// ---------------------------------------------------------------------------------------------------------

/// One NVIDIA device as recorded by the NVML reader (`@nvml` key). `None` = the call failed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NvmlDevice {
    pub index: u32,
    pub name: String,
    pub pci_bus_id: Option<String>,
    pub mem_total: Option<u64>,
    pub mem_used: Option<u64>,
    pub util_gpu: Option<u32>,
    pub power_mw: Option<u32>,
    pub power_limit_mw: Option<u32>,
    pub temp_c: Option<u32>,
    pub clock_sm_mhz: Option<u32>,
    pub max_clock_sm_mhz: Option<u32>,
    pub throttle_reasons: Option<u64>,
    /// field → error string for failed calls.
    pub errors: BTreeMap<String, String>,
}

/// Parsed `@nvml` text: driver version + devices.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NvmlHost {
    pub driver: Option<String>,
    pub devices: Vec<NvmlDevice>,
}

/// Serializes NVML results (`key: value` lines, one `device N` block each; failures as `key: !reason`).
pub fn nvml_to_text(h: &NvmlHost) -> String {
    let mut s = String::new();
    if let Some(d) = &h.driver {
        s.push_str(&format!("driver: {d}\n"));
    }
    for d in &h.devices {
        s.push_str(&format!("device {}\nname: {}\n", d.index, d.name));
        let mut kv = |k: &str, v: Option<String>| match (v, d.errors.get(k)) {
            (Some(v), _) => s.push_str(&format!("{k}: {v}\n")),
            (None, Some(e)) => s.push_str(&format!("{k}: !{e}\n")),
            (None, None) => {}
        };
        kv("pci", d.pci_bus_id.clone());
        kv("mem_total", d.mem_total.map(|v| v.to_string()));
        kv("mem_used", d.mem_used.map(|v| v.to_string()));
        kv("util_gpu", d.util_gpu.map(|v| v.to_string()));
        kv("power_mw", d.power_mw.map(|v| v.to_string()));
        kv("power_limit_mw", d.power_limit_mw.map(|v| v.to_string()));
        kv("temp_c", d.temp_c.map(|v| v.to_string()));
        kv("clock_sm_mhz", d.clock_sm_mhz.map(|v| v.to_string()));
        kv("max_clock_sm_mhz", d.max_clock_sm_mhz.map(|v| v.to_string()));
        kv("throttle_reasons", d.throttle_reasons.map(|v| format!("{v:#x}")));
    }
    s
}

/// Parses the `@nvml` text written by [`nvml_to_text`].
pub fn parse_nvml_text(text: &str) -> NvmlHost {
    let mut h = NvmlHost::default();
    for line in text.lines() {
        if let Some(i) = line.strip_prefix("device ") {
            h.devices.push(NvmlDevice {
                index: i.trim().parse().unwrap_or(h.devices.len() as u32),
                ..Default::default()
            });
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k == "driver" && h.devices.is_empty() {
            h.driver = Some(v.to_string());
            continue;
        }
        let Some(d) = h.devices.last_mut() else {
            continue;
        };
        if let Some(e) = v.strip_prefix('!') {
            d.errors.insert(k.to_string(), e.trim().to_string());
            continue;
        }
        let n = || v.parse::<u64>().ok();
        let n32 = || v.parse::<u32>().ok();
        match k {
            "name" => d.name = v.to_string(),
            "pci" => d.pci_bus_id = Some(v.to_string()),
            "mem_total" => d.mem_total = n(),
            "mem_used" => d.mem_used = n(),
            "util_gpu" => d.util_gpu = n32(),
            "power_mw" => d.power_mw = n32(),
            "power_limit_mw" => d.power_limit_mw = n32(),
            "temp_c" => d.temp_c = n32(),
            "clock_sm_mhz" => d.clock_sm_mhz = n32(),
            "max_clock_sm_mhz" => d.max_clock_sm_mhz = n32(),
            "throttle_reasons" => {
                d.throttle_reasons = u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()
            }
            _ => {}
        }
    }
    h
}

/// Per-process GPU memory recorded by the NVML reader (`@nvml/procs`: `gpu pid bytes|-` lines).
pub fn parse_nvml_procs(text: &str) -> Vec<(u32, u32, Option<u64>)> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let gpu = it.next()?.parse().ok()?;
            let pid = it.next()?.parse().ok()?;
            let bytes = it.next()?.parse().ok();
            Some((gpu, pid, bytes))
        })
        .collect()
}

/// NVML `nvmlClocksEventReasons` bits → human labels (idle and app-clock settings are not throttling).
pub fn nvml_throttle_labels(bits: u64) -> Vec<String> {
    const R: &[(u64, &str)] = &[
        (0x4, "SW power cap"),
        (0x8, "HW slowdown"),
        (0x10, "sync boost"),
        (0x20, "SW thermal slowdown"),
        (0x40, "HW thermal slowdown"),
        (0x80, "HW power brake"),
        (0x100, "display clock setting"),
    ];
    R.iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|(_, l)| l.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fdinfo_amdgpu_modern_and_legacy() {
        let t = "pos:\t0\nflags:\t02100002\nmnt_id:\t26\nino:\t1073\ndrm-driver:\tamdgpu\n\
                 drm-client-id:\t42\ndrm-pdev:\t0000:03:00.0\npasid:\t32790\n\
                 drm-memory-vram:\t8388608 KiB\ndrm-memory-gtt:\t 20480 KiB\ndrm-memory-cpu:\t0 KiB\n\
                 drm-total-vram:\t8392704 KiB\ndrm-resident-vram:\t8388608 KiB\ndrm-engine-gfx:\t123456789 ns\n";
        let c = parse_drm_fdinfo(t).unwrap();
        assert_eq!(c.driver, "amdgpu");
        assert_eq!(c.client_id, Some(42));
        assert_eq!(c.pdev.as_deref(), Some("0000:03:00.0"));
        assert_eq!(c.engines["gfx"], 123_456_789);
        let (b, regions) = c.gpu_bytes();
        assert_eq!(b, 8 << 30);
        assert_eq!(regions, vec!["vram"]);
        assert!(parse_drm_fdinfo("pos: 0\nflags: 1\n").is_none());
        // On an APU the GTT share counts too (host RAM used by the GPU); `cpu` never does.
        let (b, regions) = c.gpu_bytes_for(true);
        assert_eq!(b, (8 << 30) + 20480 * 1024);
        assert_eq!(regions, vec!["gtt", "vram"]);
        let cards = parse_cards("drm 2\nnvidia 0\ncard 0000:c4:00.0 amdgpu 536870912\ncard 0000:00:02.0 i915 -\ncard 0000:03:00.0 amdgpu 8589934592\n");
        assert_eq!(cards.len(), 3);
        assert!(cards[0].unified() && cards[1].unified() && !cards[2].unified());
    }

    #[test]
    fn fdinfo_i915_integrated_uses_host_regions() {
        let t = "drm-driver:\ti915\ndrm-client-id:\t7\ndrm-pdev:\t0000:00:02.0\n\
                 drm-total-system0:\t 512 MiB\ndrm-resident-system0:\t256 MiB\n\
                 drm-total-stolen-system0:\t0\ndrm-engine-render:\t1000 ns\n";
        let c = parse_drm_fdinfo(t).unwrap();
        let (b, regions) = c.gpu_bytes();
        assert_eq!(b, 256 << 20);
        assert_eq!(regions, vec!["stolen-system0", "system0"]);
    }

    fn metrics_v1_3() -> Vec<u8> {
        let mut b = vec![0u8; 120];
        b[0..2].copy_from_slice(&120u16.to_le_bytes());
        b[2] = 1;
        b[3] = 3;
        b[4..6].copy_from_slice(&71u16.to_le_bytes());
        b[6..8].copy_from_slice(&88u16.to_le_bytes());
        b[8..10].copy_from_slice(&u16::MAX.to_le_bytes());
        b[16..18].copy_from_slice(&97u16.to_le_bytes());
        b[22..24].copy_from_slice(&280u16.to_le_bytes());
        b[40..42].copy_from_slice(&1650u16.to_le_bytes());
        b[54..56].copy_from_slice(&1700u16.to_le_bytes());
        b[68..72].copy_from_slice(&0x0000_0004u32.to_le_bytes());
        b
    }

    fn put16(b: &mut [u8], off: usize, v: u16) {
        b[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn gpu_metrics_v1() {
        let m = parse_gpu_metrics(&metrics_v1_3()).unwrap();
        assert!(!m.apu);
        assert_eq!(m.temp_edge_c, Some(71.0));
        assert_eq!(m.temp_hotspot_c, Some(88.0));
        assert_eq!(m.gfx_activity_pct, Some(97.0));
        assert_eq!(m.socket_power_w, Some(280.0));
        assert_eq!(m.avg_gfxclk_mhz, Some(1650.0));
        assert_eq!(m.cur_gfxclk_mhz, Some(1700.0));
        assert_eq!(m.throttle_status, Some(4));
        // Truncated v1 blob: header says 120 bytes, only 30 present → fields past the end are None.
        let t = &metrics_v1_3()[..30];
        let m = parse_gpu_metrics(t).unwrap();
        assert_eq!(m.temp_edge_c, Some(71.0));
        assert_eq!(m.cur_gfxclk_mhz, None);

        // v1.0: the timestamp sits before the temperatures.
        let mut b = vec![0u8; 80];
        put16(&mut b, 0, 80);
        b[2] = 1;
        put16(&mut b, 16, 65);
        put16(&mut b, 28, 40);
        put16(&mut b, 34, 150);
        put16(&mut b, 54, 2100);
        let m = parse_gpu_metrics(&b).unwrap();
        assert_eq!(
            (
                m.temp_edge_c,
                m.gfx_activity_pct,
                m.socket_power_w,
                m.cur_gfxclk_mhz
            ),
            (Some(65.0), Some(40.0), Some(150.0), Some(2100.0))
        );

        // v1.4+ (MI300) has a different layout: refused, never misread.
        let mut v14 = metrics_v1_3();
        v14[3] = 4;
        assert!(parse_gpu_metrics(&v14).unwrap_err().contains("1.4"));
    }

    #[test]
    fn gpu_metrics_v2_layouts() {
        // v2.2 (Renoir/Cezanne/Phoenix via smu_v13_0_4 use 2.1): offsets from `offsetof` on the header.
        let mut b = vec![0u8; 128];
        put16(&mut b, 0, 128);
        b[2] = 2;
        b[3] = 2;
        put16(&mut b, 4, 6512); // temperature_gfx, centi-°C
        put16(&mut b, 28, 5500); // average_gfx_activity, centi-%
        b[32..40].copy_from_slice(&912_331_122u64.to_le_bytes()); // system_clock_counter
        put16(&mut b, 40, 18500); // average_socket_power, mW
        put16(&mut b, 64, 2100); // average_gfxclk
        put16(&mut b, 76, 2200); // current_gfxclk
        b[108..112].copy_from_slice(&u32::MAX.to_le_bytes());
        let m = parse_gpu_metrics(&b).unwrap();
        assert!(m.apu);
        assert_eq!(m.temp_edge_c, Some(65.12));
        assert_eq!(m.gfx_activity_pct, Some(55.0));
        assert_eq!(m.socket_power_w, Some(18.5));
        assert_eq!(m.avg_gfxclk_mhz, Some(2100.0));
        assert_eq!(m.cur_gfxclk_mhz, Some(2200.0));
        assert_eq!(m.throttle_status, None);

        // v2.0 keeps the timestamp right after the header.
        let mut b = vec![0u8; 120];
        put16(&mut b, 0, 120);
        b[2] = 2;
        put16(&mut b, 16, 7000);
        put16(&mut b, 40, 1200);
        put16(&mut b, 44, 9000);
        put16(&mut b, 80, 1800);
        b[112..116].copy_from_slice(&0x10u32.to_le_bytes());
        let m = parse_gpu_metrics(&b).unwrap();
        assert_eq!(
            (
                m.temp_edge_c,
                m.gfx_activity_pct,
                m.socket_power_w,
                m.cur_gfxclk_mhz,
                m.throttle_status
            ),
            (Some(70.0), Some(12.0), Some(9.0), Some(1800.0), Some(0x10))
        );

        // v3.0 (Strix): u32 socket power, activity in %, no current clock.
        let mut b = vec![0u8; 264];
        put16(&mut b, 0, 264);
        b[2] = 3;
        put16(&mut b, 4, 5800);
        put16(&mut b, 42, 37);
        b[112..116].copy_from_slice(&28_250u32.to_le_bytes());
        put16(&mut b, 174, 2600);
        let m = parse_gpu_metrics(&b).unwrap();
        assert!(m.apu);
        assert_eq!(
            (
                m.temp_edge_c,
                m.gfx_activity_pct,
                m.socket_power_w,
                m.avg_gfxclk_mhz,
                m.cur_gfxclk_mhz
            ),
            (Some(58.0), Some(37.0), Some(28.25), Some(2600.0), None)
        );

        let mut v31 = vec![0u8; 8];
        v31[2] = 3;
        v31[3] = 1;
        assert!(parse_gpu_metrics(&v31).is_err());
        let mut v25 = vec![0u8; 8];
        v25[2] = 2;
        v25[3] = 5;
        assert!(parse_gpu_metrics(&v25).is_err());
        assert!(parse_gpu_metrics(&[1, 2]).is_err());
    }

    #[test]
    fn nvml_text_roundtrip() {
        let mut d = NvmlDevice {
            index: 0,
            name: "NVIDIA GeForce RTX 4090".into(),
            pci_bus_id: Some("00000000:01:00.0".into()),
            mem_total: Some(25_757_220_864),
            mem_used: Some(21_000_000_000),
            util_gpu: Some(98),
            power_mw: Some(430_000),
            power_limit_mw: Some(450_000),
            temp_c: Some(83),
            clock_sm_mhz: Some(2100),
            max_clock_sm_mhz: Some(3105),
            throttle_reasons: Some(0x24),
            ..Default::default()
        };
        d.errors.insert("fan".into(), "not supported".into());
        let h = NvmlHost {
            driver: Some("550.54.14".into()),
            devices: vec![d.clone()],
        };
        let back = parse_nvml_text(&nvml_to_text(&h));
        let mut expect = h.clone();
        expect.devices[0].errors.clear();
        assert_eq!(back, expect);
        assert_eq!(
            nvml_throttle_labels(0x24),
            vec!["SW power cap".to_string(), "SW thermal slowdown".to_string()]
        );
        assert!(nvml_throttle_labels(0x1).is_empty());
        let t = "device 0\nname: X\nmem_used: !not supported\n";
        let p = parse_nvml_text(t);
        assert_eq!(p.devices[0].mem_used, None);
        assert_eq!(p.devices[0].errors["mem_used"], "not supported");
        assert_eq!(
            parse_nvml_procs("0 4242 21474836480\n0 77 -\nbad\n"),
            vec![(0, 4242, Some(21_474_836_480)), (0, 77, None)]
        );
    }
}

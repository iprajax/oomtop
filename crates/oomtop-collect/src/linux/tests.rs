//! End-to-end tests: the real Sources over the fake filesystem tree `fixtures/linux/tree-amd` (copied to a
//! temp dir, symlinks/permissions applied from its `links.txt`), and the NVIDIA OOM-trend replay fixture.

use super::*;
use crate::raw::RawSample;
use crate::replay::load_fixture;
use crate::Source;
use oomtop_core::redact::default_allowlist;
use oomtop_core::{GpuVendor, OomKiller, Quality, Snapshot, SourceStatus, ThermalPressure, ThresholdMetric};
use std::path::{Path, PathBuf};
use std::time::Duration;

const GIB: u64 = 1 << 30;
const KIB: u64 = 1 << 10;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/linux")
}

fn copy_tree(src: &Path, dst: &Path) {
    for e in std::fs::read_dir(src).unwrap().flatten() {
        let to = dst.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            std::fs::create_dir_all(&to).unwrap();
            copy_tree(&e.path(), &to);
        } else {
            std::fs::copy(e.path(), &to).unwrap();
        }
    }
}

/// Copies `tree-amd` to a temp dir and applies `links.txt`.
fn materialize() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = fixtures().join("tree-amd");
    copy_tree(&src, dir.path());
    let links = std::fs::read_to_string(src.join("links.txt")).unwrap();
    for l in links.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
        let parts: Vec<&str> = l.split_whitespace().collect();
        match parts.as_slice() {
            ["link", path, target] => {
                let p = dir.path().join(path);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::os::unix::fs::symlink(target, &p).unwrap();
            }
            ["chmod000", path] => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(dir.path().join(path), std::fs::Permissions::from_mode(0o000))
                    .unwrap();
            }
            _ => panic!("bad links.txt line {l}"),
        }
    }
    dir
}

fn is_root() -> bool {
    // SAFETY: geteuid never fails.
    unsafe { libc::geteuid() == 0 }
}

fn files(s: &RawSample) -> &crate::raw::LinuxFilesRaw {
    match &s.payload {
        crate::raw::RawPayload::LinuxFiles(f) => f,
        other => panic!("unexpected payload {other:?}"),
    }
}

fn host_source(root: &Path) -> LinuxHostSource {
    LinuxHostSource::with_roots(LinuxRoots::under(root)).with_units(100, 4096)
}

fn proc_source(root: &Path) -> LinuxProcSource {
    LinuxProcSource::with_roots(LinuxRoots::under(root), default_allowlist())
        .with_units(100, 4096)
        .with_access_uid(Some(1000))
}

/// All cpus 90 % busy between two samples.
fn bump_cpu(root: &Path) {
    let p = root.join("proc/stat");
    let t = std::fs::read_to_string(&p).unwrap();
    let out: Vec<String> = t
        .lines()
        .map(|l| {
            if !l.starts_with("cpu") {
                return l.to_string();
            }
            let mut f: Vec<String> = l.split_whitespace().map(str::to_string).collect();
            let user: u64 = f[1].parse().unwrap();
            let idle: u64 = f[4].parse().unwrap();
            let scale = if l.starts_with("cpu ") { 16 } else { 1 };
            f[1] = (user + 90 * scale).to_string();
            f[4] = (idle + 10 * scale).to_string();
            f.join(" ")
        })
        .collect();
    std::fs::write(&p, out.join("\n") + "\n").unwrap();
}

fn pause() {
    // Distinct taken_at_ms between samples (decode needs prev.taken_at_ms < raw.taken_at_ms).
    std::thread::sleep(Duration::from_millis(5));
}

fn apply(snap: &mut Snapshot, part: crate::decode::PartialSnapshot) {
    part.apply(snap);
}

#[test]
fn tree_host_end_to_end() {
    let dir = materialize();
    let mut src = host_source(dir.path());
    let a = src.read(Duration::from_millis(500)).unwrap();
    bump_cpu(dir.path());
    pause();
    let b = src.read(Duration::from_millis(500)).unwrap();
    let fb = files(&b);
    assert!(fb.files.contains_key("/sys/class/drm/card1/device/gpu_metrics"));
    assert!(fb.files["/sys/class/drm/card1/device/gpu_metrics"].starts_with("hex:"));
    assert!(!fb
        .files
        .keys()
        .any(|k| k.contains("card1-eDP-1") || k.contains("renderD128")));
    let mut s = Snapshot::default();
    apply(&mut s, decode_full(&a, None));
    apply(&mut s, decode_full(&b, Some(&a)));

    // cgroup: user@1000.service has memory.max 12 GiB with 11 GiB used → available capped at 1 GiB.
    assert_eq!(s.memory.own_cgroup_limit.value, Some(12 * GIB));
    assert_eq!(s.memory.available.value, Some(GIB));
    assert_eq!(s.memory.available.quality, Quality::Estimate);
    assert!(
        s.memory.available.source.contains("user@1000.service"),
        "{}",
        s.memory.available.source
    );
    assert_eq!(s.memory.psi.value.unwrap().some_avg10, 12.4);

    // OOM: kernel + systemd-oomd (swap limit overridden to 80 %, pressure 50 % from the unit drop-in).
    assert_eq!(s.oom.killers, vec![OomKiller::Kernel, OomKiller::SystemdOomd]);
    let swap = s
        .oom
        .thresholds
        .iter()
        .find(|t| t.metric == ThresholdMetric::SwapUsedPct)
        .unwrap();
    assert_eq!(swap.value, 80.0);
    assert!(swap.source.contains("50-local.conf"), "{}", swap.source);
    let pr = s
        .oom
        .thresholds
        .iter()
        .find(|t| t.metric == ThresholdMetric::MemPressurePct)
        .unwrap();
    assert_eq!((pr.value, pr.duration_s), (50.0, Some(30)));
    // Capped headroom (≈3 % of RAM) is closer than oomd's rules → the kernel (memcg) acts first.
    assert_eq!(s.oom.killer, OomKiller::Kernel);
    let v = s.oom.likely_victim.as_ref().unwrap();
    assert_eq!((v.id.pid, v.name.as_str()), (2400, "java"));
    assert_eq!(v.id.start_time, 1_790_000_000_000 + 600_000);
    assert!(
        s.oom.recent_kills.is_empty(),
        "counters at start are not recent kills"
    );

    // Thermal: acpitz 88 °C ≥ passive 85 °C → heavy; low-power profile; on battery at 41 % (mouse ignored).
    let t = &s.thermal;
    assert_eq!(t.pressure.value, Some(ThermalPressure::Heavy));
    assert_eq!(t.trip_point_hit.value, Some(true));
    assert!(t.trip_point_hit.source.contains("acpitz"));
    assert_eq!(t.low_power_mode.value, Some(true));
    assert_eq!(t.on_battery.value, Some(true));
    assert_eq!(t.battery_pct.value, Some(41.0));
    assert!(t
        .temps
        .iter()
        .any(|x| x.name == "k10temp/Tctl" && x.celsius == 91.5));
    assert!(
        !t.temps.iter().any(|x| x.name.contains("iwlwifi")),
        "disabled zone skipped"
    );
    let cpu = &t.clusters[0];
    assert_eq!(cpu.max_mhz, 5137.0);
    assert!((cpu.cur_mhz - 1475.0).abs() < 0.1, "{}", cpu.cur_mhz);
    assert!((cpu.active_pct - 90.0).abs() < 0.1, "{}", cpu.active_pct);
    let f = t.throttle_factor.value.unwrap();
    assert!(
        f < 0.8,
        "busy at 1.5 of 5.1 GHz + dGPU at 1.8 of 2.5 GHz → throttled, got {f}"
    );
    if is_root() {
        assert!(t.package_power_w.is_available() || t.package_power_w.unavailable_reason().is_some());
    } else {
        assert!(t
            .package_power_w
            .unavailable_reason()
            .unwrap()
            .contains("root-only"));
    }

    // Accelerators sorted by PCI: gpu0 = RX 7700S (03:00.0), gpu1 = 780M APU (c4:00.0).
    assert_eq!(s.accelerators.len(), 2);
    let d = &s.accelerators[0];
    assert_eq!(
        (d.id.as_str(), d.name.as_str(), d.vendor),
        ("gpu0", "AMD Radeon RX 7700S", GpuVendor::Amd)
    );
    assert!(!d.unified);
    assert_eq!(d.mem_used.value, Some(7_900 << 20));
    assert_eq!(d.gpu_budget.value, Some(8 * GIB));
    assert_eq!(d.util_pct.value, Some(99.0));
    assert_eq!(
        (d.clock_mhz.value, d.max_clock_mhz.value),
        (Some(1800.0), Some(2500.0))
    );
    assert_eq!(d.temp_c.value, Some(78.0));
    assert_eq!(d.power_w.value, Some(95.0));
    assert_eq!(d.throttle_reasons, vec!["SMU throttle status 0x4".to_string()]);
    let apu = &s.accelerators[1];
    assert!(apu.unified);
    assert_eq!(apu.gpu_budget.value, Some((512 << 20) + 12 * GIB));
    assert_eq!(apu.gpu_budget.quality, Quality::Estimate);
    assert_eq!(apu.power_w.value, Some(18.5));
    assert!(apu.name.contains("0x15bf"));
    // hwmon wins over gpu_metrics for temperature.
    assert_eq!(apu.temp_c.value, Some(70.0));
    assert!(apu.temp_c.source.contains("hwmon2"), "{}", apu.temp_c.source);
    // The recorded gpu_metrics blobs decode with the verified layouts (v2.2 = v2.1+ APU layout).
    let blob = |card: &str| {
        super::parse::from_hex(&fb.files[&format!("/sys/class/drm/{card}/device/gpu_metrics")]).unwrap()
    };
    let m = super::gpu::parse_gpu_metrics(&blob("card0")).unwrap();
    assert_eq!((m.format, m.content, m.apu), (2, 2, true));
    assert_eq!(
        (
            m.temp_edge_c,
            m.gfx_activity_pct,
            m.socket_power_w,
            m.cur_gfxclk_mhz
        ),
        (Some(70.12), Some(12.0), Some(18.5), Some(1600.0))
    );
    let m = super::gpu::parse_gpu_metrics(&blob("card1")).unwrap();
    assert_eq!(
        (
            m.temp_edge_c,
            m.temp_hotspot_c,
            m.gfx_activity_pct,
            m.throttle_status
        ),
        (Some(78.0), Some(95.0), Some(99.0), Some(4))
    );
    // APU memory covers VRAM carve-out + GTT, consistent with its budget.
    assert_eq!(apu.mem_used.value, Some((430 << 20) + 3 * GIB));
    assert_eq!(apu.mem_total.value, Some((512 << 20) + 12 * GIB));
    assert!(apu.mem_used.value <= apu.gpu_budget.value);

    for k in [
        "linux.cgroup",
        "linux.psi",
        "linux.oom",
        "linux.gpu",
        "linux.thermal",
        "linux.cpufreq",
    ] {
        assert_eq!(s.source_status.get(k), Some(&SourceStatus::Available), "{k}");
    }
    assert!(
        !s.source_status.contains_key("linux.nvml"),
        "no NVIDIA driver → no NVML status"
    );
    assert_eq!(s.host.os_version, "Linux 6.11.4-301.fc41.x86_64");
}

#[test]
fn tree_host_records_kills_and_victim() {
    let dir = materialize();
    let mut src = host_source(dir.path());
    let a = src.read(Duration::from_millis(500)).unwrap();
    // The kernel kills the Gradle daemon (top oom_score) inside user@1000.service.
    let vm = dir.path().join("proc/vmstat");
    let t = std::fs::read_to_string(&vm)
        .unwrap()
        .replace("oom_kill 0", "oom_kill 1");
    std::fs::write(&vm, t).unwrap();
    let ev = dir
        .path()
        .join("sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/memory.events");
    let t = std::fs::read_to_string(&ev)
        .unwrap()
        .replace("oom_kill 1", "oom_kill 2");
    std::fs::write(&ev, t).unwrap();
    std::fs::remove_dir_all(dir.path().join("proc/2400")).unwrap();
    pause();
    let b = src.read(Duration::from_millis(500)).unwrap();
    let part = decode_full(&b, Some(&a));
    let oom = part.oom.unwrap();
    assert_eq!(oom.recent_kills.len(), 1);
    let k = &oom.recent_kills[0];
    assert_eq!(
        (k.victim_pid, k.victim_name.as_deref()),
        (Some(2400), Some("java"))
    );
    assert!(k.source.contains("/proc/vmstat:oom_kill +1"), "{}", k.source);
    assert!(k.source.contains("user@1000.service"), "{}", k.source);
    assert_eq!(k.at_ms, Some(b.taken_at_ms));
    // Still reported on the next sample (the tracker keeps recent kills).
    pause();
    let c = src.read(Duration::from_millis(500)).unwrap();
    assert_eq!(decode_full(&c, Some(&b)).oom.unwrap().recent_kills.len(), 1);
}

#[test]
fn tree_procs_end_to_end() {
    let dir = materialize();
    let mut src = proc_source(dir.path());
    let a = src.read(Duration::from_millis(500)).unwrap();
    let fa = files(&a);
    let json = serde_json::to_string(&a).unwrap();
    assert!(
        !json.contains("tok-DO-NOT-LEAK"),
        "environ values are never stored"
    );
    assert!(
        !json.contains("3b6f0c1e-4d2a"),
        "raw session ids are never stored"
    );
    assert!(!fa.files.keys().any(|k| k.ends_with("/environ")));
    // Other users' private files are not read (no ptrace-read access as uid 1000).
    for pid in [1, 812] {
        for leaf in ["io", "smaps_rollup"] {
            assert!(!fa.files.contains_key(&format!("{pid}/{leaf}")), "{pid}/{leaf}");
        }
    }
    assert!(fa.files.contains_key("2200/smaps_rollup"));
    assert!(fa.files.contains_key("2200/fdinfo/5") && fa.files.contains_key("2200/fdinfo/6"));
    assert!(
        !fa.files.contains_key("2200/fdinfo/7"),
        "only DRM fds get fdinfo reads"
    );
    assert_eq!(
        fa.markers[&2100].keys,
        vec!["CLAUDECODE", "CLAUDE_CODE_SESSION_ID"]
    );
    assert_eq!(fa.markers[&2100].session_id, fa.markers[&2200].session_id);

    let pa = decode_full(&a, None);
    assert_eq!(pa.status.get("linux.markers"), Some(&SourceStatus::Available));
    let p = pa.processes.unwrap();
    let by = |pid: u32| p.iter().find(|x| x.id.pid == pid).unwrap();
    assert_eq!(p.len(), 9);
    let llama = by(2200);
    assert_eq!(llama.mem.footprint_or_pss.value, Some(1_402_112 * KIB));
    assert_eq!(llama.mem.footprint_or_pss.quality, Quality::Exact);
    // Two fds, one DRM client (same client id) → 7.5 GiB of VRAM counted once.
    assert_eq!(llama.mem.gpu.value, Some(7_864_320 * KIB));
    assert!(
        llama.mem.gpu.source.contains("amdgpu"),
        "{}",
        llama.mem.gpu.source
    );
    assert_eq!(
        llama.model_files,
        vec!["/home/dev/models/qwen2.5-coder-14b-q4_k_m.gguf".to_string()]
    );
    assert_eq!(llama.disk_io.read_bytes.value, Some(81_233_112 + 2200));
    assert_eq!(llama.user.as_deref(), Some("dev"));
    assert_eq!(llama.exe, "/usr/bin/llama-server");
    // APU client: VRAM carve-out + GTT (host RAM) both count on a unified device.
    assert_eq!(by(2300).mem.gpu.value, Some((409_600 + 3_145_728) * KIB));
    // Scanned, no GPU clients → an exact zero; other users → unavailable, never zero.
    assert_eq!(by(2400).mem.gpu.value, Some(0));
    assert_eq!(by(2400).mem.gpu.quality, Quality::Exact);
    assert!(by(1).mem.gpu.value.is_none());
    assert!(by(812).disk_io.read_bytes.value.is_none());
    assert_eq!(by(812).user.as_deref(), Some("systemd-oom"));
    assert_eq!(by(812).oom_score, Some(0));
    assert_eq!(by(57).mem.resident.value, Some(0), "kernel thread");

    // Second sample right away: the top-N is not due for smaps → cached values, marked estimate.
    pause();
    let b = src.read(Duration::from_millis(500)).unwrap();
    let fb = files(&b);
    assert!(!fb.files.contains_key("2200/smaps_rollup"));
    assert!(fb.files.contains_key(&format!("2200/{}", keys::SMAPS_CACHE)));
    assert!(
        !fb.files.contains_key("2200/maps"),
        "maps is parsed in flight, never stored"
    );
    let p = decode_full(&b, Some(&a)).processes.unwrap();
    let llama = p.iter().find(|x| x.id.pid == 2200).unwrap();
    assert_eq!(llama.mem.footprint_or_pss.value, Some(1_402_112 * KIB));
    assert_eq!(llama.mem.footprint_or_pss.quality, Quality::Estimate);
    assert!(llama.mem.footprint_or_pss.source.contains("adjusted by ΔRSS"));
    assert_eq!(llama.disk_io.read_rate.value, Some(0.0));
    assert!(llama.cpu_pct.is_available());
}

#[test]
fn proc_root_constructor_is_backward_compatible() {
    let dir = materialize();
    // `new(<root>/proc)` implies `<root>` for /sys (the sampler passes "/proc").
    let mut src = LinuxHostSource::new(dir.path().join("proc")).with_units(100, 4096);
    let s = src.read(Duration::from_millis(500)).unwrap();
    assert!(files(&s).files.contains_key("/sys/fs/cgroup/cgroup.controllers"));
    // A bare procfs directory (not named "proc") never reads outside it.
    let bare = tempfile::tempdir().unwrap();
    std::fs::write(
        bare.path().join("meminfo"),
        "MemTotal: 1000 kB\nMemAvailable: 500 kB\n",
    )
    .unwrap();
    let mut src = LinuxHostSource::new(bare.path());
    let s = src.read(Duration::from_millis(50)).unwrap();
    assert!(!files(&s).files.keys().any(|k| k.starts_with('/')));
    let part = decode_full(&s, None);
    assert_eq!(part.memory.unwrap().available.value, Some(500 * 1024));
    assert!(matches!(
        part.status.get("linux.gpu"),
        Some(SourceStatus::Unavailable(_))
    ));
    assert!(matches!(
        part.status.get("linux.psi"),
        Some(SourceStatus::Unavailable(_))
    ));
    assert!(part.thermal.unwrap().pressure.unavailable_reason().is_some());
}

#[test]
fn zero_budget_truncates_without_panicking() {
    let dir = materialize();
    let mut src = proc_source(dir.path());
    let s = src.read(Duration::ZERO).unwrap();
    assert!(files(&s).truncated);
    let part = decode_full(&s, None);
    assert!(matches!(
        part.status.get("linux.procs"),
        Some(SourceStatus::Partial(_))
    ));
    let mut host = host_source(dir.path());
    let h = host.read(Duration::ZERO).unwrap();
    assert!(
        decode_full(&h, None).memory.is_some(),
        "memory is read before the budget check"
    );
}

#[test]
fn garbage_and_missing_files_never_panic() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (rel, text) in [
        ("proc/meminfo", "MemTotal: lots\nMemAvailable: 12 kB\n"),
        ("proc/stat", "cpu x y\nbtime nope\n"),
        ("proc/pressure/memory", "some avg10=abc\n"),
        ("proc/self/cgroup", "0::/../../etc\n"),
        ("proc/42/stat", "42 (x"),
        (
            "proc/43/stat",
            "43 (ok) S 1 1 1 0 -1 0 0 0 0 0 1 1 0 0 20 0 1 0 100 1 1",
        ),
        ("proc/43/statm", "garbage"),
        ("proc/43/io", "read_bytes: -1\n"),
        ("proc/43/smaps_rollup", "Pss: many kB\n"),
        ("proc/43/fdinfo/3", "drm-driver: x\ndrm-memory-vram: 12 PiB\n"),
        ("sys/class/thermal/thermal_zone0/temp", "hot\n"),
        ("sys/class/drm/card0/device/vendor", "0xzz\n"),
        ("sys/class/drm/card0/device/gpu_metrics", "\x01"),
        ("sys/class/powercap/intel-rapl:0/name", "package-0\n"),
        ("sys/class/powercap/intel-rapl:0/energy_uj", "x\n"),
        ("sys/devices/system/cpu/cpufreq/policy0/scaling_cur_freq", "-5\n"),
        ("etc/systemd/oomd.conf", "[OOM]\nSwapUsedLimit=lots\n"),
    ] {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    let mut host = host_source(root);
    let mut procs = proc_source(root).with_access_uid(None);
    let h1 = host.read(Duration::from_millis(200)).unwrap();
    let p1 = procs.read(Duration::from_millis(200)).unwrap();
    pause();
    let h2 = host.read(Duration::from_millis(200)).unwrap();
    let p2 = procs.read(Duration::from_millis(200)).unwrap();
    let mut s = Snapshot::default();
    for (x, prev) in [(&h1, None), (&p1, None), (&h2, Some(&h1)), (&p2, Some(&p1))] {
        decode_full(x, prev).apply(&mut s);
    }
    assert_eq!(s.memory.available.value, Some(12 * 1024));
    assert!(s.memory.total.value.is_none());
    assert_eq!(s.processes.len(), 1);
    assert!(s.processes[0].mem.resident.value.is_none());
    assert!(s.accelerators.len() <= 1);
    assert!(s.thermal.package_power_w.value.is_none());
}

#[test]
fn nvidia_oom_trend_replay() {
    let fx = load_fixture(&fixtures().join("nvidia-oom-trend.json")).unwrap();
    assert_eq!(fx.frames.len(), 16);
    let snaps = replay_full(&fx);
    let first = &snaps[0];
    let before = &snaps[14];
    let after = &snaps[15];

    // NVML device (the nvidia DRM card with the same PCI address is not listed twice).
    assert_eq!(first.accelerators.len(), 1);
    let g = &first.accelerators[0];
    assert_eq!(
        (g.id.as_str(), g.name.as_str(), g.vendor),
        ("gpu0", "NVIDIA GeForce RTX 4090", GpuVendor::Nvidia)
    );
    assert_eq!(g.mem_used.value, Some(22_548_578_304));
    assert_eq!(g.gpu_budget.value, Some(25_757_220_864));
    assert_eq!(g.power_w.value, Some(441.0));
    assert_eq!(g.throttle_reasons, vec!["SW power cap".to_string()]);
    assert_eq!(
        first.source_status.get("linux.nvml"),
        Some(&SourceStatus::Available)
    );

    // earlyoom thresholds from its argv (-m 5 -s 10).
    assert_eq!(first.oom.killers, vec![OomKiller::Kernel, OomKiller::Earlyoom]);
    let th = &first.oom.thresholds;
    assert!(th
        .iter()
        .any(|t| t.metric == ThresholdMetric::AvailablePct && t.value == 5.0));
    assert!(th
        .iter()
        .any(|t| t.metric == ThresholdMetric::SwapUsedPct && t.value == 90.0));
    assert!(th[0].source.starts_with("earlyoom argv"));
    assert_eq!(
        first.oom.killer,
        OomKiller::Kernel,
        "swap nearly empty → earlyoom's AND is far away"
    );
    assert_eq!(
        before.oom.killer,
        OomKiller::Earlyoom,
        "both earlyoom conditions close → it acts first"
    );

    // The trend: available falls, swap fills, PSI rises, swap-out is sustained.
    for w in snaps[..15].windows(2) {
        assert!(w[1].memory.available.value < w[0].memory.available.value);
        assert!(w[1].memory.swap_used.value > w[0].memory.swap_used.value);
        assert!(w[1].memory.psi.value.unwrap().full_avg10 > w[0].memory.psi.value.unwrap().full_avg10);
    }
    assert_eq!(snaps[1].memory.swap_out_per_min.value, Some(60_000 * 4096 * 6));

    // Victim and kill.
    assert_eq!(before.oom.likely_victim.as_ref().unwrap().name, "python3");
    assert_eq!(after.oom.likely_victim.as_ref().unwrap().name, "llama-server");
    let k = &after.oom.recent_kills;
    assert_eq!(k.len(), 1);
    assert_eq!(
        (k[0].victim_pid, k[0].victim_name.as_deref()),
        (Some(3100), Some("python3"))
    );
    assert!(before.processes.iter().any(|p| p.id.pid == 3100));
    assert!(!after.processes.iter().any(|p| p.id.pid == 3100));
    assert!(after.memory.available.value > first.memory.available.value);

    // Per-process GPU (NVML) and model files; exact zero for known non-GPU processes.
    let by = |s: &Snapshot, pid: u32| s.processes.iter().find(|p| p.id.pid == pid).cloned().unwrap();
    assert_eq!(by(first, 2900).mem.gpu.value, Some(20 * GIB));
    assert_eq!(by(first, 3100).mem.gpu.value, Some(GIB));
    assert_eq!(by(first, 3300).mem.gpu.value, Some(0));
    assert_eq!(
        by(first, 2900).model_files,
        vec!["/data/models/Qwen2.5-32B-Instruct-Q4_K_M.gguf"]
    );
    assert_eq!(by(first, 640).user.as_deref(), Some("root"));

    // smaps rotation: exact on even frames, ΔRSS-adjusted estimate on odd frames (equal to the truth here).
    let py1 = by(&snaps[1], 3100);
    assert_eq!(py1.mem.footprint_or_pss.quality, Quality::Estimate);
    let truth = ((30.0 + 1.5) * GIB as f64 / 1024.0) as u64 - 150_000;
    assert_eq!(py1.mem.footprint_or_pss.value, Some(truth * KIB));
    assert_eq!(by(&snaps[2], 3100).mem.footprint_or_pss.quality, Quality::Exact);
    // Disk I/O rate of the job: 40 MiB per 10 s.
    let r = by(&snaps[3], 3100).disk_io.read_rate.value.unwrap();
    assert!((r - 40.0 * 1024.0 * 1024.0 / 10.0).abs() < 1_000.0, "{r}");

    // Busy CPUs at 5.1 of 5.45 GHz + the GPU at 72 % of max clock → a throttle factor below 1.
    let f = before.thermal.throttle_factor.value.unwrap();
    assert!(f > 0.5 && f < 1.0, "{f}");
    assert!(before.thermal.temps.iter().any(|t| t.name == "k10temp/Tctl"));

    // The core forecast sees the trend before the kill (SPEC §8.3, acceptance #7).
    let mut hist = oomtop_core::history::History::new(oomtop_core::history::DEFAULT_RETENTION_MS);
    for s in &snaps[..15] {
        hist.push_snapshot(s);
    }
    let fc = oomtop_core::forecast::forecast_oom_with(&hist, &before.oom, before.memory.total.value)
        .expect("forecast before the kill");
    assert!(fc.eta_s < 300, "{fc:?}");
    assert!(fc.confidence >= 0.6);
}

/// NVIDIA memory is only visible through NVML: when NVML fails, processes are "unavailable", never an exact
/// zero, even though their fds were scanned. On a mixed host, NVML alone cannot vouch for DRM GPUs.
#[test]
fn gpu_zero_needs_every_gpu_kind_covered() {
    let fx = load_fixture(&fixtures().join("nvidia-oom-trend.json")).unwrap();
    let mut procs = fx.frames[0]
        .iter()
        .find(|s| s.source == crate::raw::names::LINUX_PROCS)
        .unwrap()
        .clone();
    let crate::raw::RawPayload::LinuxFiles(f) = &mut procs.payload else {
        panic!("linux payload")
    };
    let with_nvml = f.clone();
    f.files.remove(keys::NVML_PROCS);
    f.files.insert(
        keys::status("nvml"),
        "unavailable: nvmlInit failed: Driver/library version mismatch".into(),
    );
    let part = decode_full(&procs, None);
    let p = part.processes.unwrap();
    let x = p.iter().find(|p| p.id.pid == 3300).unwrap();
    assert!(x.mem.gpu.value.is_none());
    assert!(
        x.mem
            .gpu
            .unavailable_reason()
            .unwrap()
            .contains("version mismatch"),
        "{:?}",
        x.mem.gpu
    );

    // NVML fine, but an AMD card too and this process's fds were never scanned → unavailable.
    let mut mixed = with_nvml;
    let dev = mixed.files[keys::GPU_DEVICES].clone() + "card 0000:c4:00.0 amdgpu 536870912\n";
    mixed.files.insert(keys::GPU_DEVICES.into(), dev);
    mixed.files.remove(keys::DRM_SCANNED);
    procs.payload = crate::raw::RawPayload::LinuxFiles(mixed);
    let p = decode_full(&procs, None).processes.unwrap();
    let x = p.iter().find(|p| p.id.pid == 3300).unwrap();
    assert!(x.mem.gpu.value.is_none(), "{:?}", x.mem.gpu);
    // A process NVML reports keeps its (NVIDIA) value.
    assert_eq!(
        p.iter().find(|p| p.id.pid == 2900).unwrap().mem.gpu.value,
        Some(20 * GIB)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn live_system_decodes() {
    let mut host = LinuxHostSource::default();
    let mut procs = LinuxProcSource::default();
    let h1 = host.read(Duration::from_millis(200)).unwrap();
    let p1 = procs.read(Duration::from_millis(400)).unwrap();
    pause();
    let h2 = host.read(Duration::from_millis(200)).unwrap();
    let p2 = procs.read(Duration::from_millis(400)).unwrap();
    let mut s = Snapshot::default();
    for (x, prev) in [(&h1, None), (&p1, None), (&h2, Some(&h1)), (&p2, Some(&p1))] {
        decode_full(x, prev).apply(&mut s);
    }
    assert!(s.memory.total.value.unwrap() > 0);
    assert!(s.memory.available.is_available());
    assert!(!s.processes.is_empty());
    let me = s
        .processes
        .iter()
        .find(|p| p.id.pid == std::process::id())
        .unwrap();
    assert!(me.mem.footprint_or_pss.is_available());
    assert!(me.disk_io.read_bytes.is_available());
    assert!(s.oom.killers.contains(&OomKiller::Kernel));
    // Our start time matches signal::process_start_time_ms (identity used before any signal).
    assert_eq!(
        crate::signal::process_start_time_ms(me.id.pid),
        Some(me.id.start_time)
    );
}

#[test]
fn enrich_is_idempotent_and_procs_keep_load_average() {
    // The hook in decode::linux::decode_files and decode_full may both run enrich: same result.
    let dir = materialize();
    let mut host = host_source(dir.path());
    let mut procs = proc_source(dir.path());
    let h = host.read(Duration::from_millis(500)).unwrap();
    let p = procs.read(Duration::from_millis(500)).unwrap();
    for raw in [&h, &p] {
        let once = decode_full(raw, None);
        let mut twice = once.clone();
        enrich(&raw.source, files(raw), None, None, &mut twice);
        assert_eq!(once, twice, "{}", raw.source);
    }
    let cpu = decode_full(&p, None).cpu.unwrap();
    assert_eq!(
        cpu.load_avg_1.value,
        Some(11.52),
        "procs samples must not wipe load averages"
    );
    let th = decode_full(&h, None).thermal.unwrap();
    assert!(th
        .temps
        .iter()
        .any(|t| t.name == "gpu0 AMD Radeon RX 7700S" && t.celsius == 78.0));
}

#[test]
fn drivetemp_is_never_polled() {
    let dir = materialize();
    let d = dir.path().join("sys/class/hwmon/hwmon9");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("name"), "drivetemp\n").unwrap();
    std::fs::write(d.join("temp1_input"), "33000\n").unwrap();
    let mut host = host_source(dir.path());
    let h = host.read(Duration::from_millis(500)).unwrap();
    assert!(!files(&h).files.keys().any(|k| k.contains("hwmon9")));
}

/// SPEC §14 budget check on a 700-process tree (`cargo test -p oomtop-collect perf_ -- --ignored --nocapture`).
/// Measures file-read + decode cost; on a real procfs reads are cheaper than on a disk-backed temp dir.
#[test]
#[ignore]
fn perf_700_processes() {
    let dir = materialize();
    let root = dir.path();
    let template = std::fs::read_to_string(root.join("proc/2400/status")).unwrap();
    for pid in 10_000..10_700u32 {
        let d = root.join(format!("proc/{pid}"));
        std::fs::create_dir_all(d.join("fd")).unwrap();
        std::fs::write(
            d.join("stat"),
            format!("{pid} (worker) S 1 {pid} {pid} 0 -1 0 0 0 0 0 10 10 0 0 20 0 4 0 5000 1000000 5000"),
        )
        .unwrap();
        std::fs::write(d.join("statm"), "250000 5000 1000 10 0 4000 0\n").unwrap();
        std::fs::write(d.join("status"), template.replace("2400", &pid.to_string())).unwrap();
        std::fs::write(d.join("cmdline"), "worker\0--x\0").unwrap();
        std::fs::write(d.join("cgroup"), "0::/user.slice\n").unwrap();
        std::fs::write(d.join("oom_score"), "100\n").unwrap();
        std::fs::write(d.join("io"), "read_bytes: 1\nwrite_bytes: 2\n").unwrap();
        std::fs::write(
            d.join("smaps_rollup"),
            "Rss: 20000 kB\nPss: 15000 kB\nSwapPss: 0 kB\n",
        )
        .unwrap();
        std::fs::write(d.join("environ"), "HOME=/home/dev\0").unwrap();
    }
    let mut procs = proc_source(root);
    let mut host = host_source(root);
    let budget = Duration::from_millis(200);
    let mut prev: Option<RawSample> = None;
    for i in 0..4 {
        let t = std::time::Instant::now();
        let s = procs.read(budget).unwrap();
        let read = t.elapsed();
        let t = std::time::Instant::now();
        let part = decode_full(&s, prev.as_ref());
        let dec = t.elapsed();
        let smaps = files(&s)
            .files
            .keys()
            .filter(|k| k.ends_with("/smaps_rollup"))
            .count();
        println!(
            "procs sample {i}: read {read:?} (budget {budget:?}, truncated {}), decode {dec:?}, {} procs, {smaps} smaps reads",
            files(&s).truncated,
            part.processes.map(|p| p.len()).unwrap_or(0)
        );
        prev = Some(s);
        pause();
    }
    for i in 0..3 {
        let t = std::time::Instant::now();
        let h = host.read(Duration::from_millis(50)).unwrap();
        let read = t.elapsed();
        let t = std::time::Instant::now();
        let _ = decode_full(&h, None);
        println!(
            "host sample {i}: read {read:?} (budget 50ms), decode {:?}",
            t.elapsed()
        );
    }
}

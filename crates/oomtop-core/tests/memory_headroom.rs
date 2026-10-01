//! Golden tests for headroom (SPEC §8.1) on the M5 Air fixture numbers and a Linux/NVIDIA box.

mod memory_common;

use memory_common::*;
use oomtop_core::can_fit::reclaim_candidates;
use oomtop_core::headroom::*;
use oomtop_core::units::{GIB, MIB};
use oomtop_core::*;

fn settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    s.set_snapshot_path("memory_snapshots");
    s.set_prepend_module_to_snapshot(false);
    s
}

#[test]
fn m5_air_as_captured() {
    // available_now from the fixture's raw vm_statistics64 counters.
    let pages = MacVmPages {
        free_count: 64_893,
        speculative_count: 52_052,
        purgeable_count: 3_335,
        external_page_count: 404_144,
    };
    let avail = macos_available_now(&pages, PAGE);
    let s = m5_air(avail.value.unwrap());
    let h = compute(&s, &HeadroomConfig::default());
    // 8 % of 24 GiB beats the 1.5 GiB floor.
    assert_eq!(h.safety_margin, (24.0 * GIB as f64 * 0.08) as u64);
    // memorystatus (77 %) is looser than vm_statistics64 → not used, but noted.
    assert_eq!(h.available_now.value, avail.value);
    assert!(h.notes.iter().any(|n| n.contains("memorystatus")));
    // Reclaimable = the two idle JVM daemons (compressed share at the host compressor ratio).
    let cands = reclaim_candidates(&s);
    let ids: Vec<&str> = cands.iter().map(|c| c.group_id.as_str()).collect();
    assert_eq!(ids, ["daemon:gradle", "daemon:kotlin"]);
    let sum: u64 = cands.iter().map(|c| c.gain).sum();
    assert_eq!(h.reclaimable.value, Some(sum));
    assert_eq!(h.reclaimable.quality, Quality::Estimate);
    // Gains stay within the footprint and above the resident part (acceptance #2 needs ≤ 15 % error).
    assert!(cands[0].gain > (3578 - 459) * MIB && cands[0].gain < 3578 * MIB);
    // GPU: unified, Metal budget estimated at 2/3 of RAM (iogpu.wired_limit_mb = 0).
    assert_eq!(h.gpu[0].budget.value, Some(16 * GIB));
    assert_eq!(h.gpu[0].budget.quality, Quality::Estimate);
    settings().bind(|| {
        insta::assert_json_snapshot!("headroom_m5_air", h);
        insta::assert_json_snapshot!("headroom_m5_air_candidates", cands);
    });
}

#[test]
fn m5_air_under_pressure() {
    let mut s = m5_air(3 * GIB);
    s.memory.pressure = Measured::exact(PressureLevel::Warn, "kern.memorystatus_vm_pressure_level");
    // The kernel itself sees less: memorystatus 11 % of 24 GiB = 2.64 GiB < 3 GiB.
    s.memory.memorystatus_level = Measured::exact(11, "kern.memorystatus_level");
    let h = compute(&s, &HeadroomConfig::default());
    assert!(h.margin_boosted);
    assert_eq!(h.available_now.value, Some(memorystatus_bytes(11, 24 * GIB)));
    assert!(h.headroom.unwrap() < 0);
    settings().bind(|| {
        insta::assert_json_snapshot!("headroom_m5_air_pressure", h);
    });
}

#[test]
fn linux_cgroup_and_discrete_gpu() {
    let mut s = linux_box(40 * GIB);
    // Running inside a 16 GiB container with 12 GiB used: the collector caps MemAvailable.
    s.memory.available = linux_available_now(&s.memory.available, Some(16 * GIB), Some(12 * GIB));
    s.memory.own_cgroup_limit = Measured::exact(16 * GIB, "/sys/fs/cgroup/memory.max");
    let h = compute(&s, &HeadroomConfig::default());
    assert_eq!(h.available_now.value, Some(4 * GIB));
    // RAM margin: 8 % of 64 GiB.
    assert_eq!(h.safety_margin, (64.0 * GIB as f64 * 0.08) as u64);
    assert_eq!(h.gpu[0].free.value, Some(14 * GIB));
    assert_eq!(h.gpu[0].margin, discrete_gpu_margin(24 * GIB));
    assert_eq!(h.reclaimable_swap.value, Some(256 * MIB));
    settings().bind(|| {
        insta::assert_json_snapshot!("headroom_linux_cgroup_gpu", h);
    });
}

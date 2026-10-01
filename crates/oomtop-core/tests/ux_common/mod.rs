//! Shared fixtures for the UX acceptance tests: the motivating machine from SPEC §2 / UX §2
//! (MacBook Air M5, 24 GB unified memory, sd-server holding ~9.9 GB, two idle build daemons, Chrome,
//! four Claude Code sessions, the Claude desktop VM and an unrelated `clang`).
#![allow(dead_code)]

use oomtop_core::model::*;
use oomtop_core::units::GIB;

pub const GB: u64 = 1_000_000_000;
pub const T0_MS: u64 = 1_790_000_000_000;

pub const FP_SD: &str = "fp-sd-server";
pub const FP_CLAUDE: &str = "fp-claude-code";
pub const FP_CHROME: &str = "fp-chrome";
pub const FP_GRADLE: &str = "fp-gradle";
pub const FP_KOTLIN: &str = "fp-kotlin";
pub const FP_VM: &str = "fp-claude-vm";
pub const FP_CLANG: &str = "fp-clang";

fn totals(footprint: u64, cpu: f64, n: u32) -> GroupTotals {
    GroupTotals {
        footprint: Measured::exact(footprint, "Σ ri_phys_footprint"),
        resident: Measured::exact(footprint / 3, "Σ resident"),
        gpu: Measured::unavailable("gpu", "per-process GPU needs IOReport"),
        swapped: Measured::unavailable("swapped", "not per-process on macOS"),
        cpu_pct: Measured::exact(cpu, "Σ cpu"),
        process_count: n,
    }
}

fn proc(pid: u32, name: &str, footprint: u64, start_ms: u64) -> Process {
    Process {
        id: ProcId::new(pid, start_ms),
        ppid: Some(1),
        name: name.into(),
        exe: format!("/usr/bin/{name}"),
        mem: MemBreakdown {
            footprint_or_pss: Measured::exact(footprint, "ri_phys_footprint"),
            resident: Measured::exact(footprint / 3, "resident"),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn group(
    id: &str,
    kind: GroupKind,
    label: &str,
    fp: &str,
    root: ProcId,
    footprint: u64,
    cpu: f64,
    n: u32,
) -> Group {
    Group {
        id: id.into(),
        kind,
        label: label.into(),
        fingerprint: fp.into(),
        root: Some(root),
        members: vec![Member {
            id: root,
            confidence: Confidence::High,
            via: AttributionSignal::Rule,
        }],
        totals: totals(footprint, cpu, n),
        confidence: Confidence::High,
        ..Default::default()
    }
}

/// The motivating machine. `idle_daemons` adds the Gradle + Kotlin daemons (idle, reclaimable).
pub fn machine(idle_daemons: bool) -> Snapshot {
    let old = T0_MS - 6 * 3600 * 1000;
    let mut s = Snapshot {
        taken_at_ms: T0_MS,
        self_pid: Some(999),
        ..Default::default()
    };
    s.host = HostInfo {
        hostname: "air".into(),
        os: OsKind::Macos,
        os_version: "macOS 26.6".into(),
        arch: "aarch64".into(),
        model: Some("Mac17,3".into()),
        cpu_brand: Some("Apple M5".into()),
        cores_logical: 10,
        mem_total: 24 * GIB,
        unified_memory: true,
        fanless: Some(true),
        page_size: 16384,
        ..Default::default()
    };
    s.memory.total = Measured::exact(24 * GIB, "hw.memsize");
    s.memory.available = Measured::exact(11 * GB, "vm_statistics64");
    s.memory.pressure = Measured::exact(PressureLevel::Normal, "kern.memorystatus_vm_pressure_level");
    s.memory.swap_total = Measured::exact(7 * GB, "vm.swapusage");
    s.memory.swap_used = Measured::exact(2_600_000_000, "vm.swapusage");
    // What a live macOS snapshot carries: jetsam is the killer, and swap can grow until the swap volume is
    // full (a nearly full disk here, so the ceiling is 7 GB — with ample disk no ETA is reachable).
    s.oom = oomtop_core::forecast::oom_setup(&oomtop_core::forecast::OomInputs {
        os: OsKind::Macos,
        ..Default::default()
    });
    s.memory.swap_limit = Measured::estimate(7 * GB, "vm.swapusage used + statfs /System/Volumes/VM free");
    s.memory.swap_out_per_min = Measured::exact(0, "Δ vm_statistics64.swapouts");
    s.memory.swap_in_per_min = Measured::exact(0, "Δ vm_statistics64.swapins");
    s.thermal.throttle_factor = Measured::unavailable("throttle", "idle");
    s.thermal.pressure = Measured::exact(ThermalPressure::Nominal, "notify thermal");
    s.thermal.low_power_mode = Measured::exact(false, "NSProcessInfo");
    s.thermal.on_battery = Measured::exact(false, "IOPowerSources");
    s.thermal.battery_pct = Measured::exact(80.0, "IOPowerSources");
    s.thermal.trip_point_hit = Measured::unavailable("trip", "not on macOS");

    let add = |s: &mut Snapshot, p: Process, g: Group| {
        s.processes.push(p);
        s.groups.push(g);
    };
    let sd = proc(4101, "sd-server", 9_900_000_000, old);
    let sd_id = sd.id;
    add(
        &mut s,
        sd,
        group(
            "model:sd-server",
            GroupKind::ModelServer,
            "sd-server",
            FP_SD,
            sd_id,
            9_900_000_000,
            0.3,
            1,
        ),
    );
    s.model_servers.push(ModelServer {
        id: "sdcpp:7861".into(),
        kind: ModelServerKind::SdCpp,
        endpoint: Some("http://127.0.0.1:7861".into()),
        pids: vec![sd_id],
        group_id: Some("model:sd-server".into()),
        busy: Measured::exact(false, "sd-server /health"),
        ..Default::default()
    });
    let chrome = proc(500, "Google Chrome", 2_800_000_000, old);
    let chrome_id = chrome.id;
    add(
        &mut s,
        chrome,
        group(
            "app:google-chrome",
            GroupKind::App,
            "Google Chrome",
            FP_CHROME,
            chrome_id,
            2_800_000_000,
            12.0,
            14,
        ),
    );
    for i in 0..4u32 {
        let p = proc(7000 + i, "claude", 350_000_000, old + i as u64 * 1000);
        let id = p.id;
        add(
            &mut s,
            p,
            group(
                &format!("agent:{:012x}", 0xc1a0de00u64 + i as u64),
                GroupKind::AgentSession,
                "Claude Code",
                FP_CLAUDE,
                id,
                350_000_000,
                2.0,
                3,
            ),
        );
    }
    let vm = proc(640, "com.apple.Virtualization.VirtualMachine", 1_500_000_000, old);
    let vm_id = vm.id;
    let mut vmg = group(
        "sandbox:claude-vm",
        GroupKind::Sandbox,
        "Claude desktop VM",
        FP_VM,
        vm_id,
        1_500_000_000,
        1.0,
        1,
    );
    vmg.lower_bound = true;
    add(&mut s, vm, vmg);
    let clang = proc(8800, "clang", 180_000_000, T0_MS - 60_000);
    let clang_id = clang.id;
    add(
        &mut s,
        clang,
        group(
            "other:clang",
            GroupKind::Other,
            "clang",
            FP_CLANG,
            clang_id,
            180_000_000,
            0.0,
            1,
        ),
    );
    if idle_daemons {
        for (pid, label, fp, fpr, idle_s) in [
            (3001u32, "GradleDaemon", FP_GRADLE, 2_900_000_000u64, 13_200u64),
            (3002, "KotlinCompileDaemon", FP_KOTLIN, 3_000_000_000, 23_700),
        ] {
            let p = proc(pid, label, fpr, old);
            let id = p.id;
            let mut g = group(
                &format!("daemon:{}", label.to_lowercase()),
                GroupKind::BuildDaemon,
                label,
                fp,
                id,
                fpr,
                0.0,
                1,
            );
            g.idle = true;
            g.idle_for_s = Some(idle_s);
            g.reclaim_gain = Measured::estimate(fpr, "resident+compressed share");
            add(&mut s, p, g);
        }
    }
    // Groups are sorted by footprint desc (attribution contract).
    s.groups.sort_by(|a, b| {
        b.totals
            .footprint
            .value
            .cmp(&a.totals.footprint.value)
            .then(a.id.cmp(&b.id))
    });
    s
}

/// Frame `i` of a replay (5 s cadence).
pub fn at(mut s: Snapshot, i: u64) -> Snapshot {
    s.taken_at_ms = T0_MS + i * 5_000;
    s
}

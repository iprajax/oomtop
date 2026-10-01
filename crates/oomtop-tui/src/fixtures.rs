//! Deterministic snapshots for tests, snapshot renders and the render-time bench. Modelled on the motivating
//! session (SPEC §1): a 24 GB fanless M5 Air with a Metal-backed `sd-server` (9.9 GB), idle Gradle + Kotlin
//! daemons (~3 GB each), Chrome, the Claude desktop VM (footprint is a lower bound), four Claude Code sessions,
//! an orphaned headless Chrome and oomtop itself. Pure data — no I/O.

use oomtop_core::history::{History, HistoryPoint, ProcPoint};
use oomtop_core::units::{GIB, MIB};
use oomtop_core::{
    Accelerator, AttributionSignal, ClusterFreq, Confidence, Device, GpuVendor, Group, GroupKind,
    GroupTotals, HostInfo, JobProgress, LoadedModel, Markers, Measured, Member, ModelServer, ModelServerKind,
    OomKiller, OsKind, PressureLevel, ProcId, ProcState, Process, Sandbox, SandboxKind, Snapshot,
    SourceStatus, ThermalPressure, Victim,
};

/// Timestamp of the fixture snapshot (2026-09-29 10:00:00 UTC).
pub const T0: u64 = 1_790_676_000_000;
/// oomtop's own pid in the fixture.
pub const SELF_PID: u32 = 9999;
/// uid of the user in the fixture.
pub const UID: u32 = 501;

struct P<'a> {
    pid: u32,
    ppid: u32,
    name: &'a str,
    exe: &'a str,
    argv: &'a [&'a str],
    footprint: u64,
    resident: u64,
    cpu: f64,
    idle_s: Option<u64>,
}

fn process(p: P) -> Process {
    let non_res = p.footprint.saturating_sub(p.resident);
    Process {
        id: ProcId::new(p.pid, T0 - 3_600_000 - p.pid as u64),
        ppid: Some(p.ppid),
        responsible_pid: None,
        name: p.name.into(),
        exe: p.exe.into(),
        cmdline: p.argv.iter().map(|s| s.to_string()).collect(),
        cwd: None,
        uid: Some(UID),
        user: Some("user".into()),
        cpu_pct: Measured::exact(p.cpu, "proc_pid_rusage.cpu_time Δ"),
        mem: oomtop_core::MemBreakdown {
            resident: Measured::exact(p.resident, "proc_pid_rusage.ri_resident_size"),
            footprint_or_pss: Measured::exact(p.footprint, "proc_pid_rusage.ri_phys_footprint"),
            gpu: Measured::unavailable("macos.gpu_split", "inside footprint (SPEC §19 Q3)"),
            compressed: Measured::unavailable("task_info", "needs root"),
            swapped: Measured::unavailable("macos", "not attributable per process"),
            non_resident_est: Measured::estimate(non_res, "footprint − resident"),
        },
        state: if p.cpu > 1.0 {
            ProcState::Running
        } else {
            ProcState::Sleeping
        },
        threads: Some(8),
        idle_for_s: match p.idle_s {
            Some(s) => Measured::exact(s, "lineage journal"),
            None => Measured::exact(0, "cpu activity"),
        },
        ..Default::default()
    }
}

struct G<'a> {
    id: &'a str,
    kind: GroupKind,
    label: &'a str,
    fp: &'a str,
    members: Vec<u32>,
    idle_s: Option<u64>,
}

fn group(s: &Snapshot, g: G) -> Group {
    let procs: Vec<&Process> = g
        .members
        .iter()
        .filter_map(|pid| s.process_by_pid(*pid))
        .collect();
    let footprint: u64 = procs.iter().filter_map(|p| p.mem.footprint_or_pss.value).sum();
    let resident: u64 = procs.iter().filter_map(|p| p.mem.resident.value).sum();
    let cpu: f64 = procs.iter().filter_map(|p| p.cpu_pct.value).sum();
    let root = procs.first().map(|p| p.id);
    Group {
        id: g.id.into(),
        kind: g.kind,
        label: g.label.into(),
        fingerprint: g.fp.into(),
        root,
        members: procs
            .iter()
            .enumerate()
            .map(|(i, p)| Member {
                id: p.id,
                confidence: if i == 0 {
                    Confidence::High
                } else {
                    Confidence::Medium
                },
                via: if i == 0 {
                    AttributionSignal::Rule
                } else {
                    AttributionSignal::Ancestry
                },
            })
            .collect(),
        totals: GroupTotals {
            footprint: Measured::exact(footprint, "Σ footprint"),
            resident: Measured::exact(resident, "Σ resident"),
            gpu: Measured::unavailable("macos.gpu_split", "inside footprint"),
            swapped: Measured::unavailable("macos", "not attributable per process"),
            cpu_pct: Measured::exact(cpu, "Σ cpu"),
            process_count: procs.len() as u32,
        },
        reclaim_gain: Measured::estimate(
            resident + (footprint - resident.min(footprint)) / 3,
            "resident+compressed share",
        ),
        swap_gain: Measured::estimate(footprint.saturating_sub(resident) / 3, "non-resident est."),
        confidence: Confidence::High,
        idle: g.idle_s.is_some(),
        idle_for_s: g.idle_s,
        matched_by: Some(format!("builtin:{}", g.kind.alias())),
        ..Default::default()
    }
}

/// The motivating machine under memory pressure (Pressure mode on the first sample).
pub fn motivating() -> Snapshot {
    let mut s = Snapshot {
        taken_at_ms: T0,
        self_pid: Some(SELF_PID),
        ..Default::default()
    };
    s.host = HostInfo {
        hostname: "air".into(),
        os: OsKind::Macos,
        os_version: "macOS 26.6".into(),
        arch: "aarch64".into(),
        // What the collector really reports (`hw.model`); the header shows "MacBook Air M5".
        model: Some("Mac17,3".into()),
        cpu_brand: Some("Apple M5".into()),
        cores_logical: 10,
        cores_performance: Some(4),
        cores_efficiency: Some(6),
        mem_total: 24 * GIB,
        unified_memory: true,
        fanless: Some(true),
        page_size: 16384,
        boot_time_ms: Some(T0 - 5_052_184_000), // up 58 days, 11:23:04
    };
    let m = &mut s.memory;
    m.total = Measured::exact(24 * GIB, "hw.memsize");
    // Swap grows on demand on macOS; this machine's disk leaves room for ~7 GiB of it (the ceiling jetsam
    // cannot pass), so 6.1G used is near full.
    m.swap_limit = Measured::estimate(7 * GIB, "vm.swapusage used + statfs /System/Volumes/VM free");
    m.available = Measured::exact(
        3 * GIB + 100 * MIB,
        "vm_statistics64 free+speculative+purgeable+external",
    );
    m.free = Measured::exact(100 * MIB, "vm_statistics64.free_count");
    m.cached = Measured::exact(3 * GIB + 800 * MIB, "vm_statistics64.external_page_count");
    // Metal allocations on Apple Silicon are wired pages: 3.0 GB kernel/other + 9.9 GB GPU.
    m.wired = Measured::exact(12 * GIB + 900 * MIB, "vm_statistics64.wire_count");
    m.compressed = Measured::exact(GIB + 600 * MIB, "vm_statistics64.compressor_page_count");
    m.compressed_logical = Measured::exact(
        4 * GIB + 800 * MIB,
        "vm_statistics64.total_uncompressed_pages_in_compressor",
    );
    m.app = Measured::exact(5 * GIB + 600 * MIB, "vm_statistics64 internal − purgeable");
    m.swap_used = Measured::exact(6 * GIB + 100 * MIB, "vm.swapusage");
    m.swap_total = Measured::exact(7 * GIB, "vm.swapusage");
    m.swap_in_per_min = Measured::exact(60 * MIB, "vm_statistics64.swapins Δ");
    m.swap_out_per_min = Measured::exact(420 * MIB, "vm_statistics64.swapouts Δ");
    m.pressure = Measured::exact(PressureLevel::Warn, "kern.memorystatus_vm_pressure_level");
    m.memorystatus_level = Measured::exact(13, "kern.memorystatus_level");
    m.psi = Measured::unavailable("psi", "Linux only");
    m.own_cgroup_limit = Measured::unavailable("cgroup", "Linux only");
    s.cpu.total_pct = Measured::exact(18.0, "host_processor_info Δ");
    s.cpu.load_avg_1 = Measured::exact(3.2, "vm.loadavg");
    s.cpu.load_avg_5 = Measured::exact(2.74, "vm.loadavg");
    s.cpu.load_avg_15 = Measured::exact(2.41, "vm.loadavg");
    // htop-style meters: 6 efficiency + 4 performance cores (the collector reports the kinds per logical CPU).
    s.cpu.per_core_pct = vec![21.5, 16.0, 6.7, 2.0, 1.3, 0.7, 42.4, 78.0, 93.1, 11.8];
    s.cpu.core_kinds = [
        [oomtop_core::CoreKind::Efficiency; 6].as_slice(),
        &[oomtop_core::CoreKind::Performance; 4],
    ]
    .concat();
    s.cpu.tasks = oomtop_core::TaskCounts {
        processes: 761,
        threads: Some(3_412),
        running: 3,
    };
    s.oom.killer = OomKiller::Jetsam;
    s.oom.killers = vec![OomKiller::Jetsam];
    s.oom.likely_victim = Some(Victim {
        id: ProcId::new(800, T0 - 3_600_800),
        name: "Google Chrome".into(),
        group_id: Some("app:google-chrome".into()),
        reason: "largest background app (heuristic)".into(),
        heuristic: true,
    });
    s.accelerators = vec![Accelerator {
        id: "gpu0".into(),
        vendor: GpuVendor::Apple,
        name: "Apple M5 GPU".into(),
        unified: true,
        util_pct: Measured::exact(58.0, "IOReport GPU residency"),
        mem_used: Measured::exact(9 * GIB + 900 * MIB, "IOAccelerator in-use system memory"),
        mem_total: Measured::exact(24 * GIB, "unified"),
        gpu_budget: Measured::exact(16 * GIB, "recommendedMaxWorkingSetSize"),
        power_w: Measured::exact(4.1, "IOReport energy"),
        temp_c: Measured::unavailable("smc", "not collected"),
        clock_mhz: Measured::exact(950.0, "IOReport GPU frequency"),
        max_clock_mhz: Measured::exact(1400.0, "IOReport GPU frequency table"),
        throttle_reasons: Vec::new(),
    }];
    let t = &mut s.thermal;
    t.pressure = Measured::exact(ThermalPressure::Moderate, "com.apple.system.thermalpressurelevel");
    t.throttle_factor = Measured::exact(0.72, "IOReport cluster residency/frequency");
    t.low_power_mode = Measured::exact(false, "NSProcessInfo.isLowPowerModeEnabled");
    t.on_battery = Measured::exact(true, "IOPSCopyPowerSourcesInfo");
    t.battery_pct = Measured::exact(29.0, "IOPSCopyPowerSourcesInfo");
    t.adapter_watts = Measured::unavailable("iokit", "on battery");
    t.package_power_w = Measured::exact(11.8, "IOReport energy");
    t.clusters = vec![
        ClusterFreq {
            name: "P-cluster".into(),
            cur_mhz: 3100.0,
            max_mhz: 4400.0,
            active_pct: 71.0,
        },
        ClusterFreq {
            name: "E-cluster".into(),
            cur_mhz: 2000.0,
            max_mhz: 2900.0,
            active_pct: 40.0,
        },
    ];
    t.trip_point_hit = Measured::unavailable("thermal_zone", "Linux only");

    let procs = vec![
        P {
            pid: 4200,
            ppid: 700,
            name: "python3",
            exe: "/opt/homebrew/bin/python3",
            argv: &["python3", "studio.py"],
            footprint: 180 * MIB,
            resident: 150 * MIB,
            cpu: 0.4,
            idle_s: None,
        },
        P {
            pid: 4242,
            ppid: 4200,
            name: "sd-server",
            exe: "/Users/user/rnd/qwen-image-studio/bin/sd-server",
            argv: &["sd-server", "--listen-port", "7861", "--diffusion-model", "qwen-image-Q4_K.gguf"],
            footprint: 9 * GIB + 700 * MIB,
            resident: GIB + 100 * MIB,
            cpu: 85.0,
            idle_s: None,
        },
        P {
            pid: 5100,
            ppid: 1,
            name: "java",
            exe: "/opt/homebrew/opt/openjdk/bin/java",
            argv: &["java", "-Xmx4g", "org.gradle.launcher.daemon.bootstrap.GradleDaemon", "9.8.0"],
            footprint: 2 * GIB + 900 * MIB,
            resident: 500 * MIB,
            cpu: 0.0,
            idle_s: Some(13_200),
        },
        P {
            pid: 5200,
            ppid: 1,
            name: "java",
            exe: "/opt/homebrew/opt/openjdk/bin/java",
            argv: &["java", "org.jetbrains.kotlin.daemon.KotlinCompileDaemon"],
            footprint: 3 * GIB,
            resident: 450 * MIB,
            cpu: 0.0,
            idle_s: Some(23_700),
        },
        P {
            pid: 800,
            ppid: 1,
            name: "Google Chrome",
            exe: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            argv: &["Google Chrome"],
            footprint: 620 * MIB,
            resident: 400 * MIB,
            cpu: 3.0,
            idle_s: None,
        },
        P {
            pid: 1300,
            ppid: 1,
            name: "Claude",
            exe: "/Applications/Claude.app/Contents/MacOS/Claude",
            argv: &["Claude"],
            footprint: 420 * MIB,
            resident: 300 * MIB,
            cpu: 1.0,
            idle_s: None,
        },
        P {
            pid: 1350,
            ppid: 1,
            name: "com.apple.Virtualization.VirtualMachine",
            exe: "/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine",
            argv: &["com.apple.Virtualization.VirtualMachine"],
            footprint: GIB + 500 * MIB,
            resident: 900 * MIB,
            cpu: 2.0,
            idle_s: None,
        },
        P {
            pid: 1500,
            ppid: 1,
            name: "ChatGPT",
            exe: "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT",
            argv: &["ChatGPT"],
            footprint: 400 * MIB,
            resident: 300 * MIB,
            cpu: 0.8,
            idle_s: None,
        },
        P {
            pid: 300,
            ppid: 1,
            name: "WindowServer",
            exe: "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer",
            argv: &["WindowServer", "-daemon"],
            footprint: 410 * MIB,
            resident: 300 * MIB,
            cpu: 6.0,
            idle_s: None,
        },
        P {
            pid: 6100,
            ppid: 1,
            name: "chrome-headless-shell",
            exe: "/Users/user/.cache/puppeteer/chrome-headless-shell",
            argv: &["chrome-headless-shell", "--remote-debugging-port=9222", "--api-key=sk-live-123456"],
            footprint: 450 * MIB,
            resident: 380 * MIB,
            cpu: 0.0,
            idle_s: Some(2_400),
        },
        P {
            pid: 7100,
            ppid: 1,
            name: "clang",
            exe: "/usr/bin/clang",
            argv: &["clang", "-c", "main.c"],
            footprint: 120 * MIB,
            resident: 110 * MIB,
            cpu: 30.0,
            idle_s: None,
        },
        P {
            pid: SELF_PID,
            ppid: 2000,
            name: "oomtop",
            exe: "/opt/homebrew/bin/oomtop",
            argv: &["oomtop"],
            footprint: 22 * MIB,
            resident: 20 * MIB,
            cpu: 0.6,
            idle_s: None,
        },
    ];
    s.processes = procs.into_iter().map(process).collect();
    // Chrome helpers (13) and ChatGPT/Codex helpers (5).
    for i in 0..13u32 {
        s.processes.push(process(P {
            pid: 810 + i,
            ppid: 800,
            name: "Google Chrome Helper (Renderer)",
            exe: "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Helper (Renderer).app",
            argv: &["Google Chrome Helper (Renderer)", "--type=renderer"],
            footprint: 170 * MIB + (i as u64 % 4) * 7 * MIB,
            resident: 120 * MIB,
            cpu: if i == 0 { 2.0 } else { 0.1 },
            idle_s: None,
        }));
    }
    for i in 0..5u32 {
        s.processes.push(process(P {
            pid: 1510 + i,
            ppid: 1500,
            name: if i == 0 { "codex" } else { "ChatGPT Helper" },
            exe: "/Applications/ChatGPT.app/Contents/Frameworks/ChatGPT Helper.app",
            argv: &["ChatGPT Helper"],
            footprint: 125 * MIB,
            resident: 90 * MIB,
            cpu: 0.2,
            idle_s: None,
        }));
    }
    for i in 0..4u32 {
        let base = 2000 + i * 10;
        s.processes.push(process(P {
            pid: base,
            ppid: 1900,
            name: "claude",
            exe: "/Users/user/.local/bin/claude",
            argv: &["claude"],
            footprint: 260 * MIB + i as u64 * 11 * MIB,
            resident: 200 * MIB,
            cpu: if i == 0 { 4.0 } else { 0.3 },
            idle_s: None,
        }));
        s.processes.push(process(P {
            pid: base + 1,
            ppid: base,
            name: "node",
            exe: "/opt/homebrew/bin/node",
            argv: &["node", "mcp-server.js"],
            footprint: 90 * MIB,
            resident: 70 * MIB,
            cpu: 0.1,
            idle_s: None,
        }));
    }
    for p in s.processes.iter_mut() {
        if p.name == "claude" || p.name == "node" {
            p.markers = Markers {
                session_id: Some(format!("{:012x}", (p.id.pid / 10) as u64 * 0x9e37_79b9)),
                agent: Some("claude-code".into()),
                keys: vec!["CLAUDECODE".into(), "CLAUDE_CODE_SESSION_ID".into()],
            };
        }
        if p.id.pid == 1350 {
            p.responsible_pid = Some(1300);
        }
        if p.id.pid == 4242 {
            p.model_files = vec!["/Users/user/models/qwen-image-Q4_K.gguf".into()];
            p.cwd = Some("/Users/user/rnd/qwen-image-studio".into());
        }
    }

    let chrome: Vec<u32> = std::iter::once(800).chain(810..823).collect();
    let chatgpt: Vec<u32> = std::iter::once(1500).chain(1510..1515).collect();
    let specs = vec![
        G {
            id: "model:sd-server",
            kind: GroupKind::ModelServer,
            label: "sd-server · qwen-image-studio",
            fp: "fp-sd-server",
            members: vec![4242, 4200],
            idle_s: None,
        },
        G {
            id: "daemon:gradle",
            kind: GroupKind::BuildDaemon,
            label: "GradleDaemon 9.8",
            fp: "fp-gradle",
            members: vec![5100],
            idle_s: Some(13_200),
        },
        G {
            id: "daemon:kotlin",
            kind: GroupKind::BuildDaemon,
            label: "KotlinCompileDaemon",
            fp: "fp-kotlin",
            members: vec![5200],
            idle_s: Some(23_700),
        },
        G {
            id: "app:google-chrome",
            kind: GroupKind::App,
            label: "Google Chrome",
            fp: "fp-chrome",
            members: chrome,
            idle_s: None,
        },
        G {
            id: "sandbox:claude-vm",
            kind: GroupKind::Sandbox,
            label: "Claude desktop VM",
            fp: "fp-claude-vm",
            members: vec![1350],
            idle_s: None,
        },
        G {
            id: "app:claude",
            kind: GroupKind::App,
            label: "Claude",
            fp: "fp-claude-app",
            members: vec![1300],
            idle_s: None,
        },
        G {
            id: "app:chatgpt",
            kind: GroupKind::App,
            label: "ChatGPT / Codex",
            fp: "fp-chatgpt",
            members: chatgpt,
            idle_s: None,
        },
        G {
            id: "agent:3f2a9c01a1b2",
            kind: GroupKind::AgentSession,
            label: "Claude Code",
            fp: "fp-claude-code",
            members: vec![2000, 2001],
            idle_s: None,
        },
        G {
            id: "agent:7c11d0e4b5a6",
            kind: GroupKind::AgentSession,
            label: "Claude Code",
            fp: "fp-claude-code",
            members: vec![2010, 2011],
            idle_s: None,
        },
        G {
            id: "agent:a90be2f3c4d5",
            kind: GroupKind::AgentSession,
            label: "Claude Code",
            fp: "fp-claude-code",
            members: vec![2020, 2021],
            idle_s: None,
        },
        G {
            id: "agent:d4e5f6a7b8c9",
            kind: GroupKind::AgentSession,
            label: "Claude Code",
            fp: "fp-claude-code",
            members: vec![2030, 2031],
            idle_s: None,
        },
        G {
            id: "other:chrome-headless-shell",
            kind: GroupKind::Other,
            label: "chrome-headless-shell",
            fp: "fp-headless",
            members: vec![6100],
            idle_s: Some(2_400),
        },
        G {
            id: "other:clang",
            kind: GroupKind::Other,
            label: "clang",
            fp: "fp-clang",
            members: vec![7100],
            idle_s: None,
        },
        G {
            id: "system:windowserver",
            kind: GroupKind::System,
            label: "WindowServer",
            fp: "fp-windowserver",
            members: vec![300],
            idle_s: None,
        },
        G {
            id: "self:oomtop",
            kind: GroupKind::Other,
            label: "oomtop",
            fp: "fp-oomtop",
            members: vec![SELF_PID],
            idle_s: None,
        },
    ];
    let mut groups: Vec<Group> = specs.into_iter().map(|g| group(&s, g)).collect();
    for g in groups.iter_mut() {
        match g.id.as_str() {
            "sandbox:claude-vm" => {
                g.lower_bound = true;
                g.configured_mem = Some(4 * GIB);
                g.owner_group = Some("app:claude".into());
                g.matched_by = Some("responsible pid → Claude.app".into());
            }
            "other:chrome-headless-shell" => {
                g.orphan = true;
                g.owner_group = Some("agent:7c11d0e4b5a6".into());
                g.matched_by = Some("lineage: spawned by Claude Code".into());
            }
            "system:windowserver" => g.protected = true,
            "self:oomtop" => g.is_self = true,
            _ => {}
        }
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g.totals.footprint.value.unwrap_or(0)));
    s.groups = groups;
    s.model_servers = vec![ModelServer {
        id: "sdcpp:7861".into(),
        kind: ModelServerKind::SdCpp,
        endpoint: Some("http://127.0.0.1:7861".into()),
        pids: vec![ProcId::new(4242, T0 - 3_600_000 - 4242)],
        group_id: Some("model:sd-server".into()),
        models: vec![LoadedModel {
            name: "qwen-image-Q4_K".into(),
            file: Some("/Users/user/models/qwen-image-Q4_K.gguf".into()),
            weights_bytes: Measured::exact(8 * GIB + 200 * MIB, "file size"),
            kv_bytes: Measured::unavailable("sdcpp", "diffusion model"),
            device: Device::Gpu,
        }],
        tok_s: Measured::unavailable("sdcpp", "diffusion model"),
        s_per_step: Measured::exact(8.1, "/sdcpp/v1/jobs"),
        queue: Measured::exact(1, "/sdcpp/v1/jobs"),
        busy: Measured::exact(true, "/sdcpp/v1/jobs"),
        progress: Some(JobProgress {
            done: 3,
            total: 6,
            label: "generating".into(),
        }),
        status: SourceStatus::Available,
    }];
    s.sandboxes = vec![Sandbox {
        id: "vz:claudevm".into(),
        kind: SandboxKind::Vm,
        runtime: "virtualization.framework".into(),
        label: "Claude desktop VM".into(),
        host_pids: vec![ProcId::new(1350, T0 - 3_600_000 - 1350)],
        configured_mem: Measured::exact(4 * GIB, "claudevm.bundle config"),
        guest_mem: Measured::unavailable("vz", "no guest agent"),
        limits: Default::default(),
        started_by_group: Some("app:claude".into()),
        footprint_lower_bound: true,
    }];
    s.source_status
        .insert("macos.host".into(), SourceStatus::Available);
    s.source_status
        .insert("macos.procs".into(), SourceStatus::Available);
    s.source_status.insert(
        "macos.gpu_split".into(),
        SourceStatus::Unavailable("per-process Metal split not available (SPEC §19 Q3)".into()),
    );
    s.source_status.insert(
        "adapter.sdcpp".into(),
        SourceStatus::Partial("no /metrics endpoint".into()),
    );
    s
}

/// A calm machine: plenty of memory, nothing throttled or busy.
pub fn calm() -> Snapshot {
    let mut s = motivating();
    s.memory.available = Measured::exact(14 * GIB, "vm_statistics64");
    s.memory.free = Measured::exact(9 * GIB, "vm_statistics64.free_count");
    s.memory.app = Measured::exact(6 * GIB, "vm_statistics64");
    s.memory.swap_used = Measured::exact(0, "vm.swapusage");
    s.memory.swap_out_per_min = Measured::exact(0, "vm_statistics64.swapouts Δ");
    s.memory.pressure = Measured::exact(PressureLevel::Normal, "kern.memorystatus_vm_pressure_level");
    s.thermal.pressure = Measured::exact(ThermalPressure::Nominal, "com.apple.system.thermalpressurelevel");
    s.thermal.throttle_factor = Measured::unavailable("ioreport", "idle");
    s.groups.retain(|g| !g.orphan && g.kind != GroupKind::BuildDaemon);
    for m in s.model_servers.iter_mut() {
        m.busy = Measured::exact(false, "/sdcpp/v1/jobs");
        m.progress = None;
    }
    s
}

/// `n` processes (≥ the motivating set) in ~n/6 groups: the render-time bench input (SPEC §14: ~700).
pub fn large(n: usize) -> Snapshot {
    let mut s = motivating();
    let mut pid = 20_000u32;
    let mut gi = 0usize;
    while s.processes.len() < n {
        let members = 1 + gi % 11;
        let label = format!("helper-app-{gi:03}");
        let mut ids = Vec::new();
        for k in 0..members {
            if s.processes.len() >= n {
                break;
            }
            let fp = (5 + (gi * 37 + k * 13) % 400) as u64 * MIB;
            let mut p = process(P {
                pid,
                ppid: if k == 0 { 1 } else { pid - k as u32 },
                name: "helper",
                exe: "/usr/local/bin/helper",
                argv: &["helper", "--worker", "--port", "8080"],
                footprint: fp,
                resident: fp * 3 / 4,
                cpu: ((gi + k) % 7) as f64 * 0.3,
                idle_s: None,
            });
            p.name = format!("{label}-w{k}");
            ids.push(p.id);
            s.processes.push(p);
            pid += 1;
        }
        let procs: Vec<&Process> = ids.iter().filter_map(|id| s.process(*id)).collect();
        let footprint: u64 = procs.iter().filter_map(|p| p.mem.footprint_or_pss.value).sum();
        let resident: u64 = procs.iter().filter_map(|p| p.mem.resident.value).sum();
        let cpu: f64 = procs.iter().filter_map(|p| p.cpu_pct.value).sum();
        let kind = [
            GroupKind::App,
            GroupKind::Other,
            GroupKind::BuildDaemon,
            GroupKind::System,
        ][gi % 4];
        s.groups.push(Group {
            id: format!("{}:{label}", kind.alias()),
            kind,
            label: label.clone(),
            fingerprint: format!("fp-{label}"),
            root: ids.first().copied(),
            members: ids
                .iter()
                .map(|id| Member {
                    id: *id,
                    confidence: Confidence::Medium,
                    via: AttributionSignal::Ancestry,
                })
                .collect(),
            totals: GroupTotals {
                footprint: Measured::exact(footprint, "Σ footprint"),
                resident: Measured::exact(resident, "Σ resident"),
                cpu_pct: Measured::exact(cpu, "Σ cpu"),
                process_count: ids.len() as u32,
                ..Default::default()
            },
            reclaim_gain: Measured::estimate(resident, "resident"),
            idle: gi.is_multiple_of(5),
            idle_for_s: gi.is_multiple_of(5).then_some(3600 + gi as u64 * 60),
            ..Default::default()
        });
        gi += 1;
    }
    s.groups
        .sort_by_key(|g| std::cmp::Reverse(g.totals.footprint.value.unwrap_or(0)));
    s
}

/// A history ending at `s` with swap growing and slightly varying group footprints (for sparklines/trends).
pub fn history_for(s: &Snapshot, points: usize) -> History {
    let mut h = History::default();
    for k in (0..points).rev() {
        let t = s.taken_at_ms.saturating_sub(k as u64 * 2000);
        let wobble = |i: usize| -> f64 { 1.0 - ((k * 7 + i * 3) % 10) as f64 * 0.02 - k as f64 * 0.004 };
        h.push(HistoryPoint {
            t_ms: t,
            available: s.memory.available.value.map(|v| v + k as u64 * 40 * MIB),
            swap_used: s
                .memory
                .swap_used
                .value
                .map(|v| v.saturating_sub(k as u64 * 14 * MIB)),
            swap_total: s.memory.swap_total.value,
            swap_limit: s.memory.swap_limit.value,
            group_keys: Vec::new(),
            psi_some_avg10: None,
            cpu_total_pct: s.cpu.total_pct.value.map(|c| c * wobble(0)),
            groups: s
                .groups
                .iter()
                .enumerate()
                .filter_map(|(i, g)| {
                    Some((
                        g.id.clone(),
                        (g.totals.footprint.value? as f64 * wobble(i)) as u64,
                    ))
                })
                .collect(),
            procs: s
                .processes
                .iter()
                .take(200)
                .enumerate()
                .map(|(i, p)| ProcPoint {
                    pid: p.id.pid,
                    start_time: p.id.start_time,
                    footprint_kib: ((p.mem.footprint_or_pss.value.unwrap_or(0) / 1024) as f64 * wobble(i))
                        as u32,
                    cpu_permille: (p.cpu_pct.value.unwrap_or(0.0) * 10.0) as u16,
                })
                .collect(),
        });
    }
    h
}

/// Protect context matching the fixture (oomtop's pid/uid, its shell as ancestor).
pub fn protect() -> oomtop_core::actions::ProtectContext {
    oomtop_core::actions::ProtectContext {
        self_pid: Some(SELF_PID),
        self_uid: Some(UID),
        ancestor_pids: vec![1900],
        protected_names: Vec::new(),
        caller_group: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_are_consistent() {
        let s = motivating();
        for g in &s.groups {
            for m in &g.members {
                assert!(s.process(m.id).is_some(), "{} member {:?}", g.id, m.id);
            }
        }
        assert_eq!(s.group("model:sd-server").unwrap().totals.process_count, 2);
        assert_eq!(s.group("app:google-chrome").unwrap().totals.process_count, 14);
        let big = large(700);
        assert_eq!(big.processes.len(), 700);
        assert!(big.groups.len() > 100);
        let h = history_for(&s, 30);
        assert_eq!(h.len(), 30);
    }
}

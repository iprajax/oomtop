//! Shared builders for the memory golden tests (headroom, can_fit, forecast, model estimates).
//!
//! The macOS snapshot mirrors `fixtures/macos/m5-air-agents.json` (MacBook Air M5, 24 GiB, 16 KiB pages):
//! compressor 180 230 pages physical / 821 961 pages logical, swap 3.68 GB of 5 GiB, memorystatus 77 %,
//! idle Gradle (`java`, footprint 3578M, 459M compressed) and Kotlin (`java`, 2808M, 156M compressed)
//! daemons, the Claude desktop VM, Chrome, several `claude` sessions. Only `available` varies per scenario.
#![allow(dead_code)]

use oomtop_core::units::{GIB, MIB};
use oomtop_core::*;

pub const PAGE: u64 = 16_384;

pub fn proc_(pid: u32, name: &str, footprint: u64, non_resident: u64, cpu: f64) -> Process {
    let mut p = Process {
        id: ProcId::new(pid, 1_790_000_000_000 + pid as u64),
        ppid: Some(1),
        name: name.into(),
        exe: format!("/usr/bin/{name}"),
        ..Default::default()
    };
    p.mem.footprint_or_pss = Measured::exact(footprint, "proc_pid_rusage.ri_phys_footprint");
    p.mem.resident = Measured::exact(footprint - non_resident, "proc_pid_rusage.ri_resident_size");
    p.mem.non_resident_est = Measured::estimate(non_resident, "max(0, footprint − resident)");
    p.mem.compressed = Measured::unavailable("task_info", "needs root");
    p.mem.swapped = Measured::unavailable("macos", "not attributable per process");
    p.cpu_pct = Measured::exact(cpu, "proc_pid_rusage");
    p
}

pub fn group(id: &str, kind: GroupKind, label: &str, members: &[&Process]) -> Group {
    let fp: u64 = members
        .iter()
        .map(|p| p.mem.footprint_or_pss.value.unwrap_or(0))
        .sum();
    let cpu: f64 = members.iter().map(|p| p.cpu_pct.value.unwrap_or(0.0)).sum();
    Group {
        id: id.into(),
        kind,
        label: label.into(),
        root: members.first().map(|p| p.id),
        members: members
            .iter()
            .map(|p| Member {
                id: p.id,
                confidence: Confidence::High,
                via: AttributionSignal::Rule,
            })
            .collect(),
        totals: GroupTotals {
            footprint: Measured::exact(fp, "Σ footprint"),
            resident: Measured::unavailable("t", "unused"),
            gpu: Measured::unavailable("gpu", "inside footprint on macOS"),
            swapped: Measured::unavailable("swap", "not attributable"),
            cpu_pct: Measured::exact(cpu, "Σ cpu"),
            process_count: members.len() as u32,
        },
        // Left unavailable on purpose: reclaim_candidates must compute it.
        reclaim_gain: Measured::unavailable("reclaim_gain", "not computed"),
        swap_gain: Measured::unavailable("swap_gain", "not attributable"),
        ..Default::default()
    }
}

/// The M5 Air as captured, with `available` overridden.
pub fn m5_air(available: u64) -> Snapshot {
    let mut s = Snapshot {
        taken_at_ms: 1_790_709_769_433,
        self_pid: Some(6470),
        ..Default::default()
    };
    s.host.os = OsKind::Macos;
    s.host.hostname = "<host>".into();
    s.host.model = Some("Mac17,3".into());
    s.host.mem_total = 24 * GIB;
    s.host.unified_memory = true;
    s.host.fanless = Some(true);
    s.host.page_size = PAGE;
    let m = &mut s.memory;
    m.total = Measured::exact(24 * GIB, "sysctl hw.memsize");
    m.available = Measured::exact(available, "vm_statistics64");
    m.compressed = Measured::exact(180_230 * PAGE, "vm_statistics64.compressor_page_count");
    m.compressed_logical = Measured::exact(
        821_961 * PAGE,
        "vm_statistics64.total_uncompressed_pages_in_compressor",
    );
    m.swap_used = Measured::exact(3_684_761_600, "sysctl vm.swapusage");
    m.swap_total = Measured::exact(5 * GIB, "sysctl vm.swapusage");
    m.swap_out_per_min = Measured::exact(0, "vm_statistics64.swapouts Δ");
    m.pressure = Measured::exact(PressureLevel::Normal, "kern.memorystatus_vm_pressure_level");
    m.memorystatus_level = Measured::exact(77, "kern.memorystatus_level");
    m.own_cgroup_limit = Measured::unavailable("cgroup", "Linux only");

    let gradle = proc_(78921, "java", 3578 * MIB, 459 * MIB, 0.1);
    let kotlin = proc_(88143, "java", 2808 * MIB, 156 * MIB, 0.2);
    let vm = proc_(
        88489,
        "com.apple.Virtualization.VirtualMachine",
        1520 * MIB,
        0,
        2.0,
    );
    let claude_app = proc_(88440, "Claude", 315 * MIB, 50 * MIB, 1.0);
    let chrome = proc_(71050, "Google Chrome", 245 * MIB, 40 * MIB, 3.0);
    let chrome_r = proc_(41842, "Google Chrome Helper (Renderer)", 453 * MIB, 60 * MIB, 5.0);
    let cc1 = proc_(92759, "claude", 365 * MIB, 45 * MIB, 4.0);
    let cc2 = proc_(93459, "claude", 284 * MIB, 31 * MIB, 0.5);
    let ws = proc_(403, "WindowServer", 559 * MIB, 100 * MIB, 8.0);
    let me = proc_(6470, "oomtop", 20 * MIB, 0, 1.0);

    let mut g_gradle = group(
        "daemon:gradle",
        GroupKind::BuildDaemon,
        "Gradle daemon",
        &[&gradle],
    );
    g_gradle.idle = true;
    g_gradle.idle_for_s = Some(5 * 3600);
    let mut g_kotlin = group(
        "daemon:kotlin",
        GroupKind::BuildDaemon,
        "Kotlin compile daemon",
        &[&kotlin],
    );
    g_kotlin.idle = true;
    g_kotlin.idle_for_s = Some(3 * 3600 + 40 * 60);
    let mut g_claude = group("app:claude", GroupKind::App, "Claude", &[&claude_app, &vm]);
    g_claude.lower_bound = true;
    let g_chrome = group(
        "app:google-chrome",
        GroupKind::App,
        "Google Chrome",
        &[&chrome, &chrome_r],
    );
    let g_cc1 = group(
        "agent:3f2a9c01aa10",
        GroupKind::AgentSession,
        "claude · oomtop",
        &[&cc1],
    );
    let mut g_cc2 = group(
        "agent:77b1c0de0042",
        GroupKind::AgentSession,
        "claude · rnd",
        &[&cc2, &me],
    );
    g_cc2.is_self = false;
    let mut g_sys = group("system", GroupKind::System, "System", &[&ws]);
    g_sys.protected = true;

    s.processes = vec![gradle, kotlin, vm, claude_app, chrome, chrome_r, cc1, cc2, ws, me];
    s.groups = vec![g_gradle, g_kotlin, g_claude, g_chrome, g_cc1, g_cc2, g_sys];
    s.accelerators = vec![Accelerator {
        id: "gpu0".into(),
        vendor: GpuVendor::Apple,
        name: "Apple M5 GPU (10-core)".into(),
        unified: true,
        mem_used: Measured::exact(300 * MIB, "IOAccelerator PerformanceStatistics"),
        mem_total: Measured::exact(24 * GIB, "unified"),
        // iogpu.wired_limit_mb = 0 → OS default; recommendedMaxWorkingSetSize not read yet.
        gpu_budget: Measured::unavailable("iogpu.wired_limit_mb", "0 = OS default (not readable yet)"),
        ..Default::default()
    }];
    s
}

/// A Linux workstation with a discrete NVIDIA GPU, running inside a memory-limited cgroup.
pub fn linux_box(available: u64) -> Snapshot {
    let mut s = Snapshot {
        taken_at_ms: 1_790_709_000_000,
        ..Default::default()
    };
    s.host.os = OsKind::Linux;
    s.host.mem_total = 64 * GIB;
    let m = &mut s.memory;
    m.total = Measured::exact(64 * GIB, "/proc/meminfo:MemTotal");
    m.available = Measured::exact(available, "/proc/meminfo:MemAvailable");
    m.swap_used = Measured::exact(GIB, "/proc/meminfo");
    m.swap_total = Measured::exact(8 * GIB, "/proc/meminfo");
    m.pressure = Measured::exact(PressureLevel::Normal, "/proc/pressure/memory");
    m.own_cgroup_limit = Measured::unavailable("cgroup", "memory.max = max");
    let mut ollama = proc_(4242, "ollama", 2 * GIB, 0, 0.0);
    ollama.mem.swapped = Measured::exact(256 * MIB, "smaps_rollup:SwapPss");
    ollama.mem.gpu = Measured::exact(9 * GIB, "nvml");
    ollama.mem.non_resident_est = Measured::unavailable("non_resident_est", "macOS only");
    let mut g = group("model:ollama", GroupKind::ModelServer, "Ollama", &[&ollama]);
    g.idle = true;
    g.totals.gpu = Measured::exact(9 * GIB, "nvml");
    s.processes = vec![ollama];
    s.groups = vec![g];
    s.accelerators = vec![Accelerator {
        id: "gpu0".into(),
        vendor: GpuVendor::Nvidia,
        name: "NVIDIA GeForce RTX 4090".into(),
        unified: false,
        mem_total: Measured::exact(24 * GIB, "nvml"),
        mem_used: Measured::exact(10 * GIB, "nvml"),
        gpu_budget: Measured::unavailable("gpu_budget", "discrete"),
        ..Default::default()
    }];
    s
}

fn push_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u64).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

fn push_u32(b: &mut Vec<u8>, k: &str, v: u32) {
    push_str(b, k);
    b.extend_from_slice(&4u32.to_le_bytes());
    b.extend_from_slice(&v.to_le_bytes());
}

/// A GGUF v3 header with the metadata of Qwen3-VL-8B-Instruct (the studio's text encoder / a typical 8B
/// GQA model): 36 layers, 32 heads, 8 KV heads, 4096 embedding, key/value length 128, 256k context, plus a
/// skipped tokenizer array and a large skipped int array (token types).
pub fn qwen3_8b_gguf() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(b"GGUF");
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&399u64.to_le_bytes());
    b.extend_from_slice(&12u64.to_le_bytes());
    push_str(&mut b, "general.architecture");
    b.extend_from_slice(&8u32.to_le_bytes());
    push_str(&mut b, "qwen3vl");
    push_str(&mut b, "general.name");
    b.extend_from_slice(&8u32.to_le_bytes());
    push_str(&mut b, "Qwen3-VL-8B-Instruct");
    push_u32(&mut b, "general.file_type", 15);
    push_u32(&mut b, "qwen3vl.block_count", 36);
    push_u32(&mut b, "qwen3vl.attention.head_count", 32);
    push_u32(&mut b, "qwen3vl.attention.head_count_kv", 8);
    push_u32(&mut b, "qwen3vl.embedding_length", 4096);
    push_u32(&mut b, "qwen3vl.attention.key_length", 128);
    push_u32(&mut b, "qwen3vl.attention.value_length", 128);
    push_u32(&mut b, "qwen3vl.context_length", 262_144);
    push_str(&mut b, "tokenizer.ggml.tokens");
    b.extend_from_slice(&9u32.to_le_bytes());
    b.extend_from_slice(&8u32.to_le_bytes());
    b.extend_from_slice(&4u64.to_le_bytes());
    for t in ["<|im_start|>", "<|im_end|>", "hello", "world"] {
        push_str(&mut b, t);
    }
    push_str(&mut b, "tokenizer.ggml.token_type");
    b.extend_from_slice(&9u32.to_le_bytes());
    b.extend_from_slice(&5u32.to_le_bytes());
    b.extend_from_slice(&5000u64.to_le_bytes());
    b.extend(std::iter::repeat_n(0u8, 5000 * 4));
    b
}

/// Deterministic noise in [-1, 1] (64-bit LCG; no floating-point libm calls, so goldens are stable).
pub struct Lcg(pub u64);

impl Lcg {
    pub fn next_unit(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

//! Sandboxes / VMs / containers (SPEC §11). Host processes are classified from the snapshot (pure); configured
//! VM sizes come from argv (QEMU `-m`, Cloud Hypervisor `--memory`, vfkit/krunkit `--memory`), the
//! Firecracker API socket or config file, and Lima/Colima instance YAML. Container engines (Docker, Podman,
//! OrbStack, Colima) are in [`crate::docker`].
//!
//! Host-side cost is always shown; Virtualization.framework guests are marked as footprint lower bounds
//! (SPEC §5, §19 Q2). App-owned VMs keep their app as `started_by_group`.
//!
//! Seatbelt: `sandbox-exec` execs into its command, so sandboxed agent tools are found by checking agent
//! session members with `sandbox_check` ([`detect_seatbelt`], live probes only).

use crate::env::HostEnv;
use crate::http::unix_get;
use crate::models::{arg_value, basename, merge_status};
use oomtop_core::{
    GroupKind, Measured, ProcId, Process, Sandbox, SandboxKind, SandboxLimits, Snapshot, SourceStatus,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;

/// Wall-clock budget for all live sandbox enrichment in one probe (sockets + small config files).
pub const LIVE_BUDGET: Duration = Duration::from_millis(300);

/// True for the macOS Virtualization.framework VM XPC service (name may be truncated by the kernel).
pub fn is_virtualization_vm(p: &Process) -> bool {
    basename(&p.exe).starts_with("com.apple.Virtualization.VirtualMachine")
        || p.name.starts_with("com.apple.Virtualization.Virtu")
}

/// (kind, runtime, footprint lower bound) for a sandbox host process.
pub fn classify(p: &Process) -> Option<(SandboxKind, &'static str, bool)> {
    if is_virtualization_vm(p) {
        return Some((SandboxKind::Vm, "virtualization.framework", true));
    }
    let exe = if p.exe.is_empty() {
        p.cmdline.first().map(|a| basename(a)).unwrap_or("")
    } else {
        basename(&p.exe)
    };
    let has = |w: &str| p.cmdline.iter().any(|a| a == w);
    Some(match exe {
        "firecracker" => (SandboxKind::MicroVm, "firecracker", false),
        "cloud-hypervisor" => (SandboxKind::MicroVm, "cloud-hypervisor", false),
        "qemu-kvm" => (SandboxKind::Vm, "qemu", false),
        "QEMULauncher" => (SandboxKind::Vm, "utm", false),
        "vfkit" => (SandboxKind::Vm, "vfkit", false),
        "krunkit" => (SandboxKind::Vm, "krunkit", false),
        "limactl" if has("hostagent") => (SandboxKind::Vm, "lima", false),
        "VBoxHeadless" | "VirtualBoxVM" => (SandboxKind::Vm, "virtualbox", false),
        "prl_vm_app" => (SandboxKind::Vm, "parallels", false),
        "vmware-vmx" => (SandboxKind::Vm, "vmware", false),
        "OrbStack Helper" if has("vmgr") => (SandboxKind::Vm, "orbstack", false),
        "conmon" => (SandboxKind::Container, "podman", false),
        "bwrap" => (SandboxKind::ProcessSandbox, "bubblewrap", false),
        "firejail" => (SandboxKind::ProcessSandbox, "firejail", false),
        "nsjail" => (SandboxKind::ProcessSandbox, "nsjail", false),
        "sandbox-exec" => (SandboxKind::ProcessSandbox, "seatbelt", false),
        e if e.starts_with("qemu-system-") && has("-avd") => (SandboxKind::Vm, "android-emulator", false),
        e if e.starts_with("qemu-system-") => (SandboxKind::Vm, "qemu", false),
        e if e.starts_with("containerd-shim") => (SandboxKind::Container, "containerd", false),
        e if e == "runsc" || e.starts_with("runsc-") => (SandboxKind::ProcessSandbox, "gvisor", false),
        _ => return None,
    })
}

/// Parses a size with a binary suffix (`K`, `M`, `G`, `T`, optional `iB`/`B`); bare numbers use `default_unit`.
pub fn parse_size(s: &str, default_unit: u64) -> Option<u64> {
    let s = s.trim().trim_matches('"');
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mult = match unit
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches("ib")
        .trim_end_matches('b')
    {
        "" => default_unit,
        "k" => 1024,
        "m" => MIB,
        "g" => 1024 * MIB,
        "t" => 1024 * 1024 * MIB,
        _ => return None,
    };
    (n >= 0.0).then_some((n * mult as f64) as u64)
}

/// Value of `key=` inside a comma-separated option list (`size=4G,slots=2`), or the leading bare value.
fn opt_value<'a>(list: &'a str, key: &str) -> Option<&'a str> {
    for part in list.split(',') {
        if let Some(v) = part.strip_prefix(key).and_then(|r| r.strip_prefix('=')) {
            return Some(v);
        }
    }
    list.split(',').next().filter(|f| !f.contains('='))
}

/// Configured memory / vCPUs / name from a VM monitor's argv (pure).
pub fn vm_config_from_argv(runtime: &str, argv: &[String]) -> (Option<u64>, Option<f64>, Option<String>) {
    match runtime {
        "qemu" | "utm" => {
            // No `-m`: unknown (QEMU's 128 MiB default is rarely what a wrapper configures).
            let mem = arg_value(argv, &["-m"])
                .and_then(|v| opt_value(v, "size"))
                .and_then(|v| parse_size(v, MIB));
            let cpus = arg_value(argv, &["-smp"])
                .and_then(|v| opt_value(v, "cpus"))
                .and_then(|v| v.parse::<f64>().ok());
            let name = arg_value(argv, &["-name"])
                .and_then(|v| opt_value(v, "guest"))
                .map(str::to_string);
            (mem, cpus, name)
        }
        "cloud-hypervisor" => {
            let mem = arg_value(argv, &["--memory"])
                .and_then(|v| opt_value(v, "size"))
                .and_then(|v| parse_size(v, 1))
                .or(Some(512 * MIB));
            let cpus = arg_value(argv, &["--cpus"])
                .and_then(|v| opt_value(v, "boot"))
                .and_then(|v| v.parse::<f64>().ok())
                .or(Some(1.0));
            (mem, cpus, None)
        }
        "vfkit" | "krunkit" => (
            arg_value(argv, &["--memory"]).and_then(|v| parse_size(v, MIB)),
            arg_value(argv, &["--cpus"]).and_then(|v| v.parse::<f64>().ok()),
            None,
        ),
        "firecracker" => (None, None, arg_value(argv, &["--id"]).map(str::to_string)),
        "android-emulator" => (
            arg_value(argv, &["-memory"]).and_then(|v| parse_size(v, MIB)),
            arg_value(argv, &["-cores"]).and_then(|v| v.parse::<f64>().ok()),
            arg_value(argv, &["-avd", "@"]).map(str::to_string),
        ),
        "lima" => (None, None, argv.last().filter(|a| !a.starts_with('-')).cloned()),
        _ => (None, None, None),
    }
}

fn runtime_title(runtime: &str) -> &'static str {
    match runtime {
        "qemu" => "QEMU",
        "android-emulator" => "Android emulator",
        "utm" => "UTM",
        "cloud-hypervisor" => "Cloud Hypervisor",
        "firecracker" => "Firecracker",
        "vfkit" | "krunkit" => "Podman machine",
        "lima" => "Lima",
        "virtualbox" => "VirtualBox",
        "parallels" => "Parallels",
        "vmware" => "VMware",
        "orbstack" => "OrbStack",
        "containerd" | "podman" => "container",
        _ => "sandbox",
    }
}

fn container_id_from_argv(runtime: &str, argv: &[String]) -> Option<String> {
    let v = match runtime {
        "containerd" => arg_value(argv, &["-id", "--id"]),
        "podman" => arg_value(argv, &["-c", "--cid"]),
        _ => None,
    }?;
    Some(v.chars().take(12).collect())
}

fn owner_of(s: &Snapshot, id: ProcId) -> Option<String> {
    let g = s.group_of(id)?;
    if g.kind == GroupKind::Sandbox {
        g.owner_group.clone()
    } else {
        Some(g.id.clone())
    }
}

/// Detects sandboxes among the snapshot's processes (pure: argv and process tree only).
pub fn detect_sandboxes(s: &Snapshot) -> Vec<Sandbox> {
    let by_pid: HashMap<u32, usize> = s
        .processes
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id.pid, i))
        .collect();
    let classes: Vec<Option<(SandboxKind, &'static str, bool)>> = s.processes.iter().map(classify).collect();
    let mut out: Vec<Sandbox> = Vec::new();
    let mut vms: Vec<usize> = Vec::new();
    for (i, p) in s.processes.iter().enumerate() {
        let Some((kind, runtime, lower_bound)) = classes[i] else {
            continue;
        };
        if runtime == "virtualization.framework" {
            vms.push(i);
            continue;
        }
        let (mem, cpus, name) = vm_config_from_argv(runtime, &p.cmdline);
        let label = match (kind, &name, container_id_from_argv(runtime, &p.cmdline)) {
            (SandboxKind::Container, _, Some(id)) => format!("container {id}"),
            (_, Some(n), _) => format!("{} {n}", runtime_title(runtime)),
            (SandboxKind::ProcessSandbox, _, _) => format!("{} ({})", runtime, p.name),
            _ => format!("{} VM", runtime_title(runtime)),
        };
        out.push(Sandbox {
            id: format!("{runtime}:{}", p.id.pid),
            kind,
            runtime: runtime.to_string(),
            label,
            host_pids: vec![p.id],
            configured_mem: match mem {
                Some(m) => Measured::exact(m, format!("{runtime} argv")),
                None => Measured::unavailable(runtime, "not in argv"),
            },
            guest_mem: Measured::unavailable(runtime, "no guest agent (SPEC §19 Q5)"),
            limits: SandboxLimits {
                mem_max: if kind == SandboxKind::Container { None } else { mem },
                cpus,
            },
            started_by_group: owner_of(s, p.id),
            footprint_lower_bound: lower_bound,
        });
    }
    // Virtualization.framework VMs: fold into the sandbox of the monitor that launched them (lima, vfkit…),
    // otherwise their own entry owned by the responsible app.
    for i in vms {
        let p = &s.processes[i];
        let monitor = [p.responsible_pid, p.ppid]
            .into_iter()
            .flatten()
            .filter(|&pid| pid > 1 && pid != p.id.pid)
            .find_map(|pid| {
                let j = *by_pid.get(&pid)?;
                out.iter()
                    .position(|sb| sb.host_pids.contains(&s.processes[j].id))
            });
        if let Some(k) = monitor {
            out[k].host_pids.push(p.id);
            out[k].footprint_lower_bound = true;
            continue;
        }
        let group = s.group_of(p.id);
        let label = match group {
            Some(g) if g.kind != GroupKind::Sandbox => format!("{} VM", g.label),
            _ => "Virtualization VM".to_string(),
        };
        out.push(Sandbox {
            id: format!("virtualization.framework:{}", p.id.pid),
            kind: SandboxKind::Vm,
            runtime: "virtualization.framework".into(),
            label,
            host_pids: vec![p.id],
            configured_mem: Measured::unavailable(
                "virtualization.framework",
                "configured size not exposed (SPEC §19 Q2)",
            ),
            guest_mem: Measured::unavailable("virtualization.framework", "no guest agent (SPEC §19 Q5)"),
            limits: SandboxLimits::default(),
            started_by_group: owner_of(s, p.id),
            footprint_lower_bound: true,
        });
    }
    out
}

// ---------------------------------------------------------------------------------------------------------
// Seatbelt markers (macOS)
// ---------------------------------------------------------------------------------------------------------

/// At most this many processes are checked for a Seatbelt sandbox per probe.
pub const MAX_SEATBELT_CHECKS: usize = 256;

/// Seatbelt sandboxes inside agent sessions (SPEC §11 "Seatbelt markers"). `sandbox-exec` applies the
/// profile and then `exec`s the command, so a sandboxed tool (e.g. an agent's shell) never shows up as
/// `sandbox-exec`: its members are checked with `check` (macOS [`seatbelt_check`]) instead. One entry per
/// sandboxed subtree (a sandboxed process whose parent is not), owned by the agent session.
///
/// Only non-root members of `agent_session` groups owned by `uid` are checked (App Store apps are all
/// sandboxed; listing them would be noise). Processes already in `existing` sandboxes are skipped.
pub fn detect_seatbelt(
    s: &Snapshot,
    existing: &[Sandbox],
    uid: Option<u32>,
    mut check: impl FnMut(ProcId) -> Option<bool>,
) -> Vec<Sandbox> {
    let known: HashSet<ProcId> = existing
        .iter()
        .flat_map(|sb| sb.host_pids.iter().copied())
        .collect();
    let mut owner: HashMap<ProcId, &str> = HashMap::new();
    let mut checked = 0usize;
    'groups: for g in s.groups.iter().filter(|g| g.kind == GroupKind::AgentSession) {
        for m in &g.members {
            if g.root == Some(m.id) || known.contains(&m.id) {
                continue;
            }
            let Some(p) = s.process(m.id) else { continue };
            if uid.is_some() && p.uid.is_some() && p.uid != uid {
                continue;
            }
            if checked >= MAX_SEATBELT_CHECKS {
                break 'groups;
            }
            checked += 1;
            if check(m.id) == Some(true) {
                owner.insert(m.id, g.id.as_str());
            }
        }
    }
    // Walk each sandboxed process up to its topmost sandboxed ancestor (same group).
    let parent = |id: ProcId| -> Option<ProcId> {
        let pp = s.process(id)?.ppid.filter(|&pp| pp > 1 && pp != id.pid)?;
        s.process_by_pid(pp).map(|q| q.id)
    };
    let mut trees: BTreeMap<ProcId, Vec<ProcId>> = BTreeMap::new();
    for (&id, &g) in &owner {
        let mut root = id;
        for _ in 0..64 {
            match parent(root) {
                Some(pp) if owner.get(&pp) == Some(&g) => root = pp,
                _ => break,
            }
        }
        trees.entry(root).or_default().push(id);
    }
    trees
        .into_iter()
        .filter_map(|(root, mut members)| {
            let p = s.process(root)?;
            members.sort_by_key(|m| (m != &root, m.pid));
            Some(Sandbox {
                id: format!("seatbelt:{}", root.pid),
                kind: SandboxKind::ProcessSandbox,
                runtime: "seatbelt".into(),
                label: format!("seatbelt ({})", p.name),
                host_pids: members,
                configured_mem: Measured::unavailable("seatbelt", "Seatbelt limits access, not memory"),
                guest_mem: Measured::unavailable("seatbelt", "not a VM"),
                limits: SandboxLimits::default(),
                started_by_group: owner.get(&root).map(|g| g.to_string()),
                footprint_lower_bound: false,
            })
        })
        .collect()
}

/// Whether a live process runs under a Seatbelt profile (`sandbox_check(pid, NULL, 0)`, the check behind
/// Activity Monitor's "Sandbox" column). `sandbox_check` also answers 1 for a pid that no longer exists, so a
/// positive answer counts only if the pid still has `id`'s start time afterwards. `None` = unknown.
#[cfg(target_os = "macos")]
pub fn seatbelt_check(id: ProcId) -> Option<bool> {
    extern "C" {
        // libsystem_sandbox (re-exported by libSystem); not in the public headers.
        fn sandbox_check(
            pid: libc::pid_t,
            operation: *const libc::c_char,
            filter: libc::c_int,
            ...
        ) -> libc::c_int;
    }
    let pid = libc::pid_t::try_from(id.pid).ok().filter(|&p| p > 0)?;
    // SAFETY: NULL operation + SANDBOX_FILTER_NONE (0) takes no further arguments.
    let r = unsafe { sandbox_check(pid, std::ptr::null(), 0) };
    match r {
        0 => Some(false),
        1 => (mac_start_time_ms(id.pid)? == id.start_time).then_some(true),
        _ => None,
    }
}

#[cfg(not(target_os = "macos"))]
pub fn seatbelt_check(_id: ProcId) -> Option<bool> {
    None
}

/// Start time of a live process (ms since epoch, the `ProcId` convention), same-user processes only.
#[cfg(target_os = "macos")]
pub(crate) fn mac_start_time_ms(pid: u32) -> Option<u64> {
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_bsdinfo is plain old data; zeroed is valid and `size` matches the buffer.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let n = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut libc::proc_bsdinfo as *mut libc::c_void,
            size,
        )
    };
    (n == size).then(|| info.pbi_start_tvsec * 1000 + info.pbi_start_tvusec / 1000)
}

// ---------------------------------------------------------------------------------------------------------
// Live enrichment (local files and unix sockets only)
// ---------------------------------------------------------------------------------------------------------

/// `(mem, vcpus)` from a Firecracker `/machine-config` response or config file (`machine-config` object).
pub fn parse_firecracker_machine(v: &serde_json::Value) -> (Option<u64>, Option<f64>) {
    let m = v.get("machine-config").unwrap_or(v);
    (
        m.get("mem_size_mib").and_then(|x| x.as_u64()).map(|x| x * MIB),
        m.get("vcpu_count").and_then(|x| x.as_f64()),
    )
}

/// `(mem, cpus)` from a Lima `lima.yaml` (`memory: "4GiB"`, `cpus: 4`) or Colima `colima.yaml`
/// (`memory: 2` GiB, `cpu: 2`). Top-level keys only; no YAML library needed.
pub fn parse_vm_yaml(text: &str) -> (Option<u64>, Option<f64>) {
    let mut mem = None;
    let mut cpus = None;
    for line in text.lines() {
        if line.starts_with([' ', '\t', '#', '-']) {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.split('#').next().unwrap_or("").trim().trim_matches(['"', '\'']);
        if v.is_empty() || v == "null" {
            continue;
        }
        match k.trim() {
            "memory" => mem = parse_size(v, 1024 * MIB),
            "cpus" | "cpu" => cpus = v.parse::<f64>().ok(),
            _ => {}
        }
    }
    (mem, cpus)
}

fn firecracker_sockets(p: &Process) -> Vec<PathBuf> {
    if p.cmdline.iter().any(|a| a == "--no-api") {
        return Vec::new();
    }
    let sock = arg_value(&p.cmdline, &["--api-sock"]).unwrap_or("/run/firecracker.socket");
    let mut v = Vec::new();
    if sock.starts_with('/') {
        v.push(PathBuf::from(sock));
        // Under the jailer the socket lives inside the chroot.
        v.push(
            Path::new("/proc")
                .join(p.id.pid.to_string())
                .join("root")
                .join(sock.trim_start_matches('/')),
        );
    } else if let Some(cwd) = &p.cwd {
        v.push(Path::new(cwd).join(sock));
    }
    v
}

fn read_small(path: &Path) -> Option<String> {
    let m = std::fs::metadata(path).ok()?;
    (m.is_file() && m.len() <= 1024 * 1024)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
}

/// `(mem, cpus)` from an Android AVD `config.ini` (`hw.ramSize=4096M` or MiB, `hw.cpu.ncore=2`).
pub fn parse_avd_config(text: &str) -> (Option<u64>, Option<f64>) {
    let mut mem = None;
    let mut cpus = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "hw.ramSize" => mem = parse_size(v.trim(), MIB),
            "hw.cpu.ncore" => cpus = v.trim().parse::<f64>().ok(),
            _ => {}
        }
    }
    (mem, cpus)
}

/// Lima instance directory of a `limactl hostagent` process (from `--pidfile <dir>/ha.pid` or the name).
fn lima_dir(p: &Process, env: &HostEnv) -> Option<PathBuf> {
    if let Some(pf) = arg_value(&p.cmdline, &["--pidfile"]) {
        return Path::new(pf).parent().map(Path::to_path_buf);
    }
    let name = p.cmdline.last()?;
    env.home_join(".lima").map(|h| h.join(name))
}

pub(crate) fn enrich_live(
    sandboxes: &mut [Sandbox],
    s: &Snapshot,
    env: &HostEnv,
    timeout: Duration,
    status: &mut BTreeMap<String, SourceStatus>,
) {
    enrich_live_within(sandboxes, s, env, timeout, LIVE_BUDGET, status)
}

/// [`enrich_live`] with an explicit overall budget; sandboxes left when it runs out keep their argv data.
pub(crate) fn enrich_live_within(
    sandboxes: &mut [Sandbox],
    s: &Snapshot,
    env: &HostEnv,
    timeout: Duration,
    budget: Duration,
    status: &mut BTreeMap<String, SourceStatus>,
) {
    let end = Instant::now() + budget;
    for sb in sandboxes.iter_mut() {
        let Some(p) = sb.host_pids.first().and_then(|id| s.process(*id)) else {
            continue;
        };
        let left = end.saturating_duration_since(Instant::now()).min(timeout);
        if left.is_zero() {
            if sb.runtime == "firecracker" {
                merge_status(
                    status,
                    "adapter.firecracker".into(),
                    &SourceStatus::Partial("probe budget exceeded".into()),
                );
            }
            continue;
        }
        match sb.runtime.as_str() {
            "firecracker" => {
                let socks = firecracker_sockets(p);
                let mut st = if socks.is_empty() {
                    SourceStatus::Partial("started with --no-api".into())
                } else {
                    SourceStatus::Partial("API socket not reachable".into())
                };
                for sock in socks {
                    let left = end.saturating_duration_since(Instant::now()).min(timeout);
                    if left.is_zero() {
                        break;
                    }
                    if let Ok(r) = unix_get(&sock, "/machine-config", left) {
                        if let Ok(v) = r.json() {
                            let (mem, cpus) = parse_firecracker_machine(&v);
                            if let Some(m) = mem {
                                sb.configured_mem = Measured::exact(m, "firecracker /machine-config");
                                sb.limits.mem_max = Some(m);
                            }
                            sb.limits.cpus = cpus.or(sb.limits.cpus);
                            st = SourceStatus::Available;
                            break;
                        }
                    }
                }
                if st != SourceStatus::Available {
                    let cfg =
                        arg_value(&p.cmdline, &["--config-file"]).and_then(|c| read_small(Path::new(c)));
                    if let Some(v) = cfg.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
                        let (mem, cpus) = parse_firecracker_machine(&v);
                        if let Some(m) = mem {
                            sb.configured_mem = Measured::exact(m, "firecracker --config-file");
                            sb.limits.mem_max = Some(m);
                        }
                        sb.limits.cpus = cpus.or(sb.limits.cpus);
                        st = SourceStatus::Partial("config file only (API socket not reachable)".into());
                    }
                }
                merge_status(status, "adapter.firecracker".into(), &st);
            }
            "android-emulator" if !sb.configured_mem.is_available() => {
                let name =
                    arg_value(&p.cmdline, &["-avd"]).filter(|n| !n.contains('/') && !n.starts_with('.'));
                let cfg = name
                    .zip(env.android_avd_home.as_ref())
                    .and_then(|(n, home)| read_small(&home.join(format!("{n}.avd")).join("config.ini")));
                if let Some(text) = cfg {
                    let (mem, cpus) = parse_avd_config(&text);
                    if let Some(m) = mem {
                        sb.configured_mem = Measured::exact(m, "AVD config.ini hw.ramSize");
                        sb.limits.mem_max = Some(m);
                    }
                    sb.limits.cpus = cpus.or(sb.limits.cpus);
                }
            }
            "lima" => {
                if let Some(text) = lima_dir(p, env).and_then(|d| read_small(&d.join("lima.yaml"))) {
                    let (mem, cpus) = parse_vm_yaml(&text);
                    if let Some(m) = mem {
                        sb.configured_mem = Measured::exact(m, "lima.yaml memory");
                        sb.limits.mem_max = Some(m);
                    }
                    sb.limits.cpus = cpus.or(sb.limits.cpus);
                }
                // Colima wraps Lima: `colima` / `colima-<profile>` instances.
                if let Some(name) = p.cmdline.last().filter(|n| n.starts_with("colima")) {
                    let profile = name.strip_prefix("colima-").unwrap_or("default");
                    sb.label = format!("Colima {profile}");
                    sb.runtime = "colima".into();
                    if !sb.configured_mem.is_available() {
                        if let Some(text) = env
                            .home_join(".colima")
                            .and_then(|c| read_small(&c.join(profile).join("colima.yaml")))
                        {
                            let (mem, cpus) = parse_vm_yaml(&text);
                            if let Some(m) = mem {
                                sb.configured_mem = Measured::exact(m, "colima.yaml memory");
                                sb.limits.mem_max = Some(m);
                            }
                            sb.limits.cpus = cpus.or(sb.limits.cpus);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::{Group, Member};

    fn proc(pid: u32, ppid: u32, exe: &str, argv: &[&str]) -> Process {
        Process {
            id: ProcId::new(pid, 1),
            ppid: Some(ppid),
            name: basename(exe).chars().take(31).collect(),
            exe: exe.into(),
            cmdline: argv.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    const VM_EXE: &str = "/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine";

    #[test]
    fn virtualization_vm_is_lower_bound_and_owned_by_app() {
        let id = ProcId::new(900, 1);
        let mut s = Snapshot::default();
        let mut vm = proc(900, 1, VM_EXE, &[VM_EXE]);
        vm.name = "com.apple.Virtualization.Virtua".into(); // truncated like the real kernel name
        s.processes.push(vm);
        s.groups.push(Group {
            id: "app:claude".into(),
            kind: GroupKind::App,
            label: "Claude".into(),
            members: vec![Member {
                id,
                ..Default::default()
            }],
            ..Default::default()
        });
        let sb = detect_sandboxes(&s);
        assert_eq!(sb.len(), 1);
        assert_eq!(sb[0].kind, SandboxKind::Vm);
        assert!(sb[0].footprint_lower_bound);
        assert_eq!(sb[0].started_by_group.as_deref(), Some("app:claude"));
        assert_eq!(sb[0].label, "Claude VM");
        assert!(sb[0].configured_mem.value.is_none(), "unknown is never zero");
        let mut s2 = s.clone();
        crate::apply(
            &mut s2,
            crate::Probe {
                sandboxes: sb,
                ..Default::default()
            },
        );
        assert!(s2.groups[0].lower_bound);
    }

    #[test]
    fn seatbelt_trees_inside_agent_sessions() {
        let mut s = Snapshot {
            processes: vec![
                proc(10, 1, "/Users/u/.local/bin/codex", &["codex"]),
                proc(20, 10, "/bin/bash", &["bash", "-c", "make"]),
                proc(21, 20, "/usr/bin/make", &["make"]),
                proc(30, 10, "/usr/bin/git", &["git", "status"]),
                proc(40, 1, "/Applications/Notes.app/Contents/MacOS/Notes", &[]),
                proc(
                    50,
                    10,
                    "/usr/bin/sandbox-exec",
                    &["sandbox-exec", "-p", "x", "sh"],
                ),
            ],
            ..Default::default()
        };
        let member = |pid: u32| Member {
            id: ProcId::new(pid, 1),
            ..Default::default()
        };
        s.groups = vec![
            Group {
                id: "agent:abc".into(),
                kind: GroupKind::AgentSession,
                root: Some(ProcId::new(10, 1)),
                members: vec![member(10), member(20), member(21), member(30), member(50)],
                ..Default::default()
            },
            Group {
                id: "app:notes".into(),
                kind: GroupKind::App,
                root: Some(ProcId::new(40, 1)),
                members: vec![member(40)],
                ..Default::default()
            },
        ];
        let sandboxed: HashSet<u32> = [10, 20, 21, 40, 50].into_iter().collect();
        let existing = detect_sandboxes(&s);
        assert_eq!(existing.len(), 1, "the sandbox-exec process itself");
        let mut asked = Vec::new();
        let sb = detect_seatbelt(&s, &existing, None, |id| {
            asked.push(id.pid);
            Some(sandboxed.contains(&id.pid))
        });
        asked.sort_unstable();
        assert_eq!(
            asked,
            vec![20, 21, 30],
            "agent members only: not the root, apps, or known sandboxes"
        );
        assert_eq!(sb.len(), 1, "{sb:#?}");
        assert_eq!(sb[0].label, "seatbelt (bash)");
        assert_eq!(sb[0].kind, SandboxKind::ProcessSandbox);
        assert_eq!(sb[0].host_pids, vec![ProcId::new(20, 1), ProcId::new(21, 1)]);
        assert_eq!(sb[0].started_by_group.as_deref(), Some("agent:abc"));
        assert!(sb[0].configured_mem.value.is_none());
        // Unknown answers and other users' processes are skipped.
        assert!(detect_seatbelt(&s, &existing, None, |_| None).is_empty());
        s.processes[1].uid = Some(0);
        s.processes[2].uid = Some(0);
        assert!(detect_seatbelt(&s, &existing, Some(501), |id| Some(sandboxed.contains(&id.pid))).is_empty());
    }

    /// Spawns our own `sandbox-exec … sleep` child and finds it through `sandbox_check`.
    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_check_on_own_child() {
        let mut child = std::process::Command::new("/usr/bin/sandbox-exec")
            .args(["-p", "(version 1)(allow default)", "/bin/sleep", "10"])
            .spawn()
            .unwrap();
        let pid = child.id();
        // Wait until sandbox-exec has exec'd into sleep (the profile is applied before the exec).
        let mut id = None;
        for _ in 0..100 {
            let st = mac_start_time_ms(pid).unwrap();
            if seatbelt_check(ProcId::new(pid, st)) == Some(true) {
                id = Some(ProcId::new(pid, st));
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let me = ProcId::new(std::process::id(), mac_start_time_ms(std::process::id()).unwrap());
        let result = (|| {
            let id = id.ok_or("child never reported sandboxed")?;
            if seatbelt_check(me) != Some(false) {
                return Err("test process reported sandboxed");
            }
            if seatbelt_check(ProcId::new(pid, id.start_time + 5_000)).is_some() {
                return Err("wrong start time must be unknown (pid reuse guard)");
            }
            Ok(())
        })();
        child.kill().unwrap();
        child.wait().unwrap();
        result.unwrap();
        // Gone process: `sandbox_check` says 1, the start-time check turns it into unknown.
        assert_eq!(seatbelt_check(ProcId::new(pid, id.unwrap().start_time)), None);
    }

    #[test]
    fn argv_configs() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            vm_config_from_argv(
                "qemu",
                &v(&[
                    "qemu-system-aarch64",
                    "-m",
                    "4G",
                    "-smp",
                    "cpus=4,sockets=1",
                    "-name",
                    "guest=dev,debug-threads=on"
                ])
            ),
            (Some(4096 * MIB), Some(4.0), Some("dev".into()))
        );
        assert_eq!(
            vm_config_from_argv("qemu", &v(&["qemu-system-x86_64", "-m", "2048"])).0,
            Some(2048 * MIB)
        );
        assert_eq!(
            vm_config_from_argv(
                "qemu",
                &v(&["qemu-system-x86_64", "-m", "size=1G,slots=2,maxmem=4G"])
            )
            .0,
            Some(1024 * MIB)
        );
        assert_eq!(
            vm_config_from_argv("qemu", &v(&["qemu-system-x86_64"])).0,
            None,
            "unknown, not QEMU's default"
        );
        assert_eq!(
            parse_avd_config("hw.cpu.ncore=2\nhw.ramSize=4096M\n"),
            (Some(4096 * MIB), Some(2.0))
        );
        assert_eq!(parse_avd_config("hw.ramSize=1536\n").0, Some(1536 * MIB));
        assert_eq!(
            vm_config_from_argv(
                "cloud-hypervisor",
                &v(&["cloud-hypervisor", "--memory", "size=1024M", "--cpus", "boot=2"])
            ),
            (Some(1024 * MIB), Some(2.0), None)
        );
        assert_eq!(
            vm_config_from_argv("vfkit", &v(&["vfkit", "--cpus", "5", "--memory", "2048"])),
            (Some(2048 * MIB), Some(5.0), None)
        );
        assert_eq!(parse_size("4GiB", 1), Some(4096 * MIB));
        assert_eq!(parse_size("1.5G", 1), Some(1536 * MIB));
        assert_eq!(parse_size("12X", 1), None);
    }

    #[test]
    fn detects_monitors_and_folds_vz_vms() {
        let s = Snapshot {
            processes: vec![
                proc(
                    100,
                    1,
                    "/opt/homebrew/bin/limactl",
                    &[
                        "limactl",
                        "hostagent",
                        "--pidfile",
                        "/Users/u/.colima/_lima/colima/ha.pid",
                        "colima",
                    ],
                ),
                {
                    let mut vm = proc(101, 1, VM_EXE, &[VM_EXE]);
                    vm.responsible_pid = Some(100);
                    vm
                },
                proc(
                    200,
                    1,
                    "/usr/bin/qemu-system-x86_64",
                    &["qemu-system-x86_64", "-m", "8G", "-name", "win11"],
                ),
                proc(
                    300,
                    1,
                    "/usr/bin/containerd-shim-runc-v2",
                    &[
                        "containerd-shim-runc-v2",
                        "-namespace",
                        "moby",
                        "-id",
                        "0123456789abcdef0123",
                        "-address",
                        "/run/containerd/containerd.sock",
                    ],
                ),
                proc(
                    400,
                    1,
                    "/usr/bin/firecracker",
                    &["firecracker", "--api-sock", "/tmp/fc.sock", "--id", "vm7"],
                ),
                proc(500, 1, "/usr/bin/bwrap", &["bwrap", "--unshare-all", "sh"]),
                proc(600, 1, "/opt/homebrew/bin/limactl", &["limactl", "list"]),
            ],
            ..Default::default()
        };
        let sb = detect_sandboxes(&s);
        let by = |rt: &str| {
            sb.iter()
                .find(|x| x.runtime == rt)
                .unwrap_or_else(|| panic!("{rt}: {sb:#?}"))
        };
        let lima = by("lima");
        assert_eq!(lima.host_pids.len(), 2, "VZ guest folded into its Lima monitor");
        assert!(lima.footprint_lower_bound);
        assert_eq!(lima.label, "Lima colima");
        let q = by("qemu");
        assert_eq!(q.configured_mem.value, Some(8192 * MIB));
        assert_eq!(q.label, "QEMU win11");
        assert_eq!(by("containerd").label, "container 0123456789ab");
        assert_eq!(by("containerd").kind, SandboxKind::Container);
        assert_eq!(by("firecracker").label, "Firecracker vm7");
        assert_eq!(by("bubblewrap").kind, SandboxKind::ProcessSandbox);
        assert_eq!(sb.len(), 5, "limactl list is not a VM");
    }

    #[test]
    fn yaml_and_firecracker_config() {
        let lima = "vmType: vz\ncpus: 4\nmemory: \"6GiB\"\nmounts:\n  - location: \"~\"\n    memory: 1\n";
        assert_eq!(parse_vm_yaml(lima), (Some(6 * 1024 * MIB), Some(4.0)));
        let colima = "# colima\ncpu: 2\nmemory: 2 # GiB\ndisk: 60\n";
        assert_eq!(parse_vm_yaml(colima), (Some(2 * 1024 * MIB), Some(2.0)));
        assert_eq!(parse_vm_yaml("memory: null\n"), (None, None));
        let fc = serde_json::json!({"machine-config": {"vcpu_count": 2, "mem_size_mib": 1024, "smt": false}});
        assert_eq!(parse_firecracker_machine(&fc), (Some(1024 * MIB), Some(2.0)));
        let api = serde_json::json!({"vcpu_count": 1, "mem_size_mib": 256});
        assert_eq!(parse_firecracker_machine(&api), (Some(256 * MIB), Some(1.0)));
    }

    /// Unresponsive API sockets on many microVMs cannot add up past the budget.
    #[cfg(unix)]
    #[test]
    fn live_enrichment_is_budgeted() {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let mut listeners = Vec::new();
        let mut procs = Vec::new();
        for i in 0..6u32 {
            let sock = dir.path().join(format!("fc{i}.sock"));
            listeners.push(UnixListener::bind(&sock).unwrap()); // accepts (backlog), never answers
            procs.push(proc(
                400 + i,
                1,
                "/usr/bin/firecracker",
                &["firecracker", "--api-sock", sock.to_str().unwrap()],
            ));
        }
        procs.push(proc(
            500,
            1,
            "/usr/bin/firecracker",
            &["firecracker", "--no-api", "--config-file", "/nope.json"],
        ));
        let s = Snapshot {
            processes: procs,
            ..Default::default()
        };
        let mut sb = detect_sandboxes(&s);
        assert_eq!(sb.len(), 7);
        let mut status = BTreeMap::new();
        let t = Instant::now();
        enrich_live_within(
            &mut sb,
            &s,
            &HostEnv::default(),
            Duration::from_millis(200),
            Duration::from_millis(250),
            &mut status,
        );
        assert!(t.elapsed() < Duration::from_millis(800), "{:?}", t.elapsed());
        assert!(matches!(&status["adapter.firecracker"], SourceStatus::Partial(_)));
        assert!(sb.iter().all(|x| x.configured_mem.value.is_none()));
        assert!(
            firecracker_sockets(&s.processes[6]).is_empty(),
            "--no-api has no socket"
        );
        drop(listeners);
    }

    #[cfg(unix)]
    #[test]
    fn firecracker_api_and_lima_yaml_live() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("fc.sock");
        let server = tiny_http::Server::http_unix(&sock).unwrap();
        let h = std::thread::spawn(move || {
            let req = server.recv().unwrap();
            assert_eq!(req.url(), "/machine-config");
            req.respond(tiny_http::Response::from_string(
                r#"{"vcpu_count":2,"mem_size_mib":2048,"smt":false}"#,
            ))
            .unwrap();
        });
        let avd = dir.path().join("avd/pixel.avd");
        std::fs::create_dir_all(&avd).unwrap();
        std::fs::write(avd.join("config.ini"), "hw.ramSize=4096M\nhw.cpu.ncore=2\n").unwrap();
        let lima_dir = dir.path().join("lima/dev");
        std::fs::create_dir_all(&lima_dir).unwrap();
        std::fs::write(lima_dir.join("lima.yaml"), "cpus: 3\nmemory: 5GiB\n").unwrap();
        let s = Snapshot {
            processes: vec![
                proc(
                    400,
                    1,
                    "/usr/bin/firecracker",
                    &["firecracker", "--api-sock", sock.to_str().unwrap()],
                ),
                proc(
                    100,
                    1,
                    "/opt/homebrew/bin/limactl",
                    &[
                        "limactl",
                        "hostagent",
                        "--pidfile",
                        lima_dir.join("ha.pid").to_str().unwrap(),
                        "dev",
                    ],
                ),
                proc(
                    500,
                    1,
                    "/sdk/emulator/qemu/darwin-aarch64/qemu-system-aarch64-headless",
                    &["qemu-system-aarch64-headless", "-avd", "pixel", "-no-window"],
                ),
            ],
            ..Default::default()
        };
        let mut sb = detect_sandboxes(&s);
        let mut status = BTreeMap::new();
        let env = HostEnv {
            android_avd_home: Some(dir.path().join("avd")),
            ..Default::default()
        };
        enrich_live(&mut sb, &s, &env, Duration::from_secs(2), &mut status);
        h.join().unwrap();
        let a = sb.iter().find(|x| x.runtime == "android-emulator").unwrap();
        assert_eq!(a.label, "Android emulator pixel");
        assert_eq!(a.configured_mem.value, Some(4096 * MIB));
        assert_eq!(a.limits.cpus, Some(2.0));
        let fc = sb.iter().find(|x| x.runtime == "firecracker").unwrap();
        assert_eq!(fc.configured_mem.value, Some(2048 * MIB));
        assert_eq!(fc.limits.cpus, Some(2.0));
        assert_eq!(status.get("adapter.firecracker"), Some(&SourceStatus::Available));
        let l = sb.iter().find(|x| x.runtime == "lima").unwrap();
        assert_eq!(l.configured_mem.value, Some(5 * 1024 * MIB));
        assert_eq!(l.limits.cpus, Some(3.0));
    }
}

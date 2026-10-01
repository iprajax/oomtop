//! Container engines over the Docker Engine API on a unix socket (SPEC §11): Docker (Linux), Docker Desktop,
//! OrbStack, Colima, Rancher Desktop and Podman (compat API). An engine is contacted only when one of its
//! processes exists; nothing but local sockets is used.
//!
//! Per engine: `GET /info` (VM memory on macOS = the VM's configured size), `GET /containers/json` (running
//! containers), and per container, within a time budget and rotating across refreshes,
//! `GET /containers/{id}/stats?stream=false&one-shot=true` (working set = usage − inactive file, like
//! `docker stats`) and `GET /containers/{id}/json` (limits, host pid).

use crate::env::HostEnv;
use crate::http::unix_get;
use crate::AdapterError;
use oomtop_core::{GroupKind, Measured, ProcId, Sandbox, SandboxKind, SandboxLimits, Snapshot};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// An engine socket worth probing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSocket {
    /// docker | docker-desktop | orbstack | colima | rancher-desktop | podman
    pub runtime: &'static str,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EngineInfo {
    pub mem_total: Option<u64>,
    pub ncpu: Option<f64>,
    pub operating_system: Option<String>,
    pub name: Option<String>,
    pub server_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContainerSummary {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContainerStats {
    pub usage: Option<u64>,
    /// `usage − inactive_file` (cgroup v2) / `usage − total_inactive_file|cache` (v1).
    pub working_set: Option<u64>,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ContainerInspect {
    /// Host pid of the container's init (meaningful only for native engines).
    pub pid: Option<u32>,
    /// `HostConfig.Memory` (> 0 only).
    pub mem_limit: Option<u64>,
    pub cpus: Option<f64>,
}

fn s(v: &serde_json::Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string)
}

pub fn parse_info(v: &serde_json::Value) -> EngineInfo {
    EngineInfo {
        mem_total: v.get("MemTotal").and_then(|x| x.as_u64()).filter(|&m| m > 0),
        ncpu: v.get("NCPU").and_then(|x| x.as_f64()),
        operating_system: s(v, "OperatingSystem"),
        name: s(v, "Name"),
        server_version: s(v, "ServerVersion"),
    }
}

/// Container ids are interpolated into request paths, so only plain ids (hex for Docker / Podman) are used.
pub fn valid_container_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub fn parse_containers(v: &serde_json::Value) -> Vec<ContainerSummary> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let id = s(c, "Id").filter(|id| valid_container_id(id))?;
                    let name = c
                        .get("Names")
                        .and_then(|n| n.as_array())
                        .and_then(|n| n.first())
                        .and_then(|n| n.as_str())
                        .map(|n| n.trim_start_matches('/').to_string())
                        .unwrap_or_else(|| id.chars().take(12).collect());
                    let labels = c
                        .get("Labels")
                        .and_then(|l| l.as_object())
                        .map(|m| {
                            m.iter()
                                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(ContainerSummary {
                        id,
                        name,
                        image: s(c, "Image").unwrap_or_default(),
                        state: s(c, "State").unwrap_or_default(),
                        labels,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_stats(v: &serde_json::Value) -> ContainerStats {
    let ms = v.get("memory_stats");
    let usage = ms.and_then(|m| m.get("usage")).and_then(|x| x.as_u64());
    let st = ms.and_then(|m| m.get("stats"));
    let inactive = st.and_then(|st| {
        ["inactive_file", "total_inactive_file", "cache"]
            .iter()
            .find_map(|k| st.get(*k).and_then(|x| x.as_u64()))
    });
    ContainerStats {
        usage,
        working_set: usage.map(|u| u.saturating_sub(inactive.unwrap_or(0))),
        limit: ms.and_then(|m| m.get("limit")).and_then(|x| x.as_u64()),
    }
}

pub fn parse_inspect(v: &serde_json::Value) -> ContainerInspect {
    let hc = v.get("HostConfig");
    let nano = hc
        .and_then(|h| h.get("NanoCpus"))
        .and_then(|x| x.as_u64())
        .filter(|&n| n > 0);
    let quota = hc
        .and_then(|h| h.get("CpuQuota"))
        .and_then(|x| x.as_i64())
        .filter(|&q| q > 0);
    let period = hc
        .and_then(|h| h.get("CpuPeriod"))
        .and_then(|x| x.as_i64())
        .filter(|&p| p > 0);
    ContainerInspect {
        pid: v
            .get("State")
            .and_then(|st| st.get("Pid"))
            .and_then(|x| x.as_u64())
            .filter(|&p| p > 0)
            .map(|p| p as u32),
        mem_limit: hc
            .and_then(|h| h.get("Memory"))
            .and_then(|x| x.as_u64())
            .filter(|&m| m > 0),
        cpus: nano
            .map(|n| n as f64 / 1e9)
            .or_else(|| quota.zip(period).map(|(q, p)| q as f64 / p as f64)),
    }
}

fn exe_contains(s: &Snapshot, needle: &str) -> bool {
    s.processes.iter().any(|p| p.exe.contains(needle))
}

fn name_is(s: &Snapshot, names: &[&str]) -> bool {
    s.processes.iter().any(|p| {
        let base = p.exe.rsplit('/').next().unwrap_or(&p.exe);
        names.contains(&p.name.as_str()) || names.contains(&base)
    })
}

fn sockets_in(dir: &Path, depth: usize, file: &str, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten().take(64) {
            let p = e.path();
            if depth > 0 && p.is_dir() {
                sockets_in(&p, depth - 1, file, out);
            } else if p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == file || (file.starts_with('*') && n.ends_with(&file[1..])))
            {
                out.push(p);
            }
        }
    }
}

#[cfg(unix)]
fn is_socket(p: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::metadata(p)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_socket(_p: &Path) -> bool {
    false
}

/// Engine sockets for the runtimes whose processes are present (existing sockets only, de-duplicated by
/// canonical path; the most specific runtime wins, e.g. OrbStack's `/var/run/docker.sock` symlink).
pub fn engine_sockets(s: &Snapshot, env: &HostEnv) -> Vec<EngineSocket> {
    let mut cands: Vec<(&'static str, PathBuf)> = Vec::new();
    let home = |r: &str| env.home_join(r);
    if exe_contains(s, "/OrbStack.app/") || name_is(s, &["OrbStack", "OrbStack Helper"]) {
        cands.extend(home(".orbstack/run/docker.sock").map(|p| ("orbstack", p)));
    }
    if exe_contains(s, "/Docker.app/") || s.processes.iter().any(|p| p.name.starts_with("com.docker.")) {
        cands.extend(home(".docker/run/docker.sock").map(|p| ("docker-desktop", p)));
        cands.push(("docker-desktop", PathBuf::from("/var/run/docker.sock")));
    }
    if exe_contains(s, "/Rancher Desktop.app/") {
        cands.extend(home(".rd/docker.sock").map(|p| ("rancher-desktop", p)));
    }
    let colima = name_is(s, &["colima"])
        || s.processes
            .iter()
            .any(|p| p.name == "limactl" && p.cmdline.last().is_some_and(|a| a.starts_with("colima")));
    if colima {
        if let Some(c) = home(".colima") {
            let mut v = Vec::new();
            sockets_in(&c, 1, "docker.sock", &mut v);
            cands.extend(v.into_iter().map(|p| ("colima", p)));
        }
    }
    if name_is(
        s,
        &[
            "podman",
            "conmon",
            "gvproxy",
            "vfkit",
            "krunkit",
            "podman-mac-helper",
        ],
    ) {
        if let Some(x) = &env.xdg_runtime_dir {
            cands.push(("podman", x.join("podman/podman.sock")));
        }
        if let Some(uid) = env.uid {
            cands.push((
                "podman",
                PathBuf::from(format!("/run/user/{uid}/podman/podman.sock")),
            ));
        }
        cands.push(("podman", PathBuf::from("/run/podman/podman.sock")));
        if let Some(m) = home(".local/share/containers/podman/machine") {
            let mut v = Vec::new();
            sockets_in(&m, 1, "podman.sock", &mut v);
            cands.extend(v.into_iter().map(|p| ("podman", p)));
        }
        if let Some(t) = &env.tmpdir {
            let mut v = Vec::new();
            sockets_in(&t.join("podman"), 0, "*-api.sock", &mut v);
            cands.extend(v.into_iter().map(|p| ("podman", p)));
        }
    }
    if name_is(s, &["dockerd", "rootlesskit", "containerd"])
        || (env.docker_host.is_some() && name_is(s, &["dockerd", "com.docker.backend"]))
    {
        cands.extend(env.docker_host.clone().map(|p| ("docker", p)));
        if let Some(x) = &env.xdg_runtime_dir {
            cands.push(("docker", x.join("docker.sock")));
        }
        cands.push(("docker", PathBuf::from("/var/run/docker.sock")));
        cands.push(("docker", PathBuf::from("/run/docker.sock")));
    }
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut out = Vec::new();
    for (runtime, p) in cands {
        if !is_socket(&p) {
            continue;
        }
        let canon = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
        if seen.contains(&canon) {
            continue;
        }
        seen.push(canon);
        out.push(EngineSocket { runtime, path: p });
    }
    out
}

/// Result of probing one engine.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EngineProbe {
    pub runtime: String,
    pub socket: String,
    pub info: EngineInfo,
    /// Engine runs containers directly on this host (Linux dockerd/podman), not inside a VM.
    pub native: bool,
    pub containers: Vec<Sandbox>,
    /// Containers whose stats were skipped for the time budget (read on a later refresh).
    pub stats_skipped: usize,
    /// Why `/info` failed, when it did (VM size / CPU annotations are then missing).
    pub info_error: Option<String>,
}

fn get_json(sock: &Path, path: &str, timeout: Duration) -> Result<serde_json::Value, AdapterError> {
    unix_get(sock, path, timeout)?.json()
}

/// Probes one engine within `budget` (per-call timeout `timeout`). `rotation` offsets which containers get
/// stats first so every container is refreshed over successive calls.
pub fn probe_engine(
    ep: &EngineSocket,
    s: &Snapshot,
    env: &HostEnv,
    timeout: Duration,
    budget: Duration,
    max_stats: usize,
    rotation: usize,
) -> Result<EngineProbe, AdapterError> {
    let end = Instant::now() + budget;
    let left = || end.saturating_duration_since(Instant::now()).min(timeout);
    let info = get_json(&ep.path, "/info", left()).map(|v| parse_info(&v));
    // A failed /info only loses VM size / CPU annotations, so the probe goes on — but the reason is kept
    // (it used to be dropped by `.ok()`, which made a once-in-hundreds test failure undiagnosable).
    let (info, info_error) = match info {
        Ok(i) => (Some(i), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let list = get_json(&ep.path, "/containers/json", left().max(Duration::from_millis(1)))?;
    let info = info.unwrap_or_default();
    let desktop = info
        .operating_system
        .as_deref()
        .is_some_and(|o| o.contains("Docker Desktop"));
    let runtime: &str = if desktop && ep.runtime == "docker" {
        "docker-desktop"
    } else {
        ep.runtime
    };
    let native = !env.is_macos && !desktop;
    let containers = parse_containers(&list);
    let n = containers.len();
    let mut out = EngineProbe {
        runtime: runtime.to_string(),
        socket: ep.path.display().to_string(),
        info,
        native,
        info_error,
        ..Default::default()
    };
    let app_group = |label: &str| {
        s.groups
            .iter()
            .find(|g| g.kind == GroupKind::App && g.label == label)
            .map(|g| g.id.clone())
    };
    let vm_owner = match runtime {
        "docker-desktop" => app_group("Docker"),
        "orbstack" => app_group("OrbStack"),
        "rancher-desktop" => app_group("Rancher Desktop"),
        _ => None,
    };
    let mut done = 0usize;
    for k in 0..n {
        let c = &containers[(k + rotation) % n.max(1)];
        let mut sb = Sandbox {
            id: format!("{runtime}:{}", c.id.chars().take(12).collect::<String>()),
            kind: SandboxKind::Container,
            runtime: runtime.to_string(),
            label: c.name.clone(),
            host_pids: Vec::new(),
            configured_mem: Measured::unavailable(runtime, "stats not read yet"),
            guest_mem: Measured::unavailable(runtime, "stats not read yet"),
            limits: SandboxLimits::default(),
            started_by_group: vm_owner.clone(),
            footprint_lower_bound: false,
        };
        if done < max_stats && Instant::now() < end {
            done += 1;
            let id = &c.id;
            if let Ok(v) = get_json(
                &ep.path,
                &format!("/containers/{id}/stats?stream=false&one-shot=true"),
                left(),
            ) {
                let st = parse_stats(&v);
                if let Some(ws) = st.working_set {
                    sb.guest_mem = Measured::exact(ws, format!("{runtime} stats (usage − inactive file)"));
                }
            }
            if let Ok(v) = get_json(&ep.path, &format!("/containers/{id}/json"), left()) {
                let ins = parse_inspect(&v);
                sb.limits = SandboxLimits {
                    mem_max: ins.mem_limit,
                    cpus: ins.cpus,
                };
                sb.configured_mem = match ins.mem_limit {
                    Some(m) => Measured::exact(m, format!("{runtime} HostConfig.Memory")),
                    None => Measured::unavailable(runtime, "no memory limit"),
                };
                if native {
                    if let Some(pid) = ins.pid {
                        if let Some(p) = s.process_by_pid(pid) {
                            sb.host_pids.push(p.id);
                        }
                    }
                }
            }
        } else {
            out.stats_skipped += 1;
            sb.guest_mem = Measured::unavailable(runtime, "stats budget exceeded; read on next refresh");
        }
        if native {
            // Every host process in the container's cgroup.
            for p in &s.processes {
                if p.cgroup.as_deref().is_some_and(|cg| cg.contains(&c.id)) && !sb.host_pids.contains(&p.id) {
                    sb.host_pids.push(p.id);
                }
            }
            sb.started_by_group = sb.host_pids.first().and_then(|id| s.group_of(*id)).and_then(|g| {
                if g.kind == GroupKind::Sandbox {
                    g.owner_group.clone()
                } else {
                    Some(g.id.clone())
                }
            });
        }
        out.containers.push(sb);
    }
    out.containers.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(out)
}

/// Merges engine results into the process-level sandboxes: containers replace the shim entries whose
/// child they are (native engines), and the engine's VM entry (macOS runtimes) gets its configured memory.
pub fn merge_engine(sandboxes: &mut Vec<Sandbox>, eng: &EngineProbe, s: &Snapshot) {
    if eng.native {
        let mut drop = Vec::new();
        for c in &eng.containers {
            for id in c.host_pids.clone() {
                let Some(ppid) = s.process(id).and_then(|p| p.ppid) else {
                    continue;
                };
                if let Some(k) = sandboxes.iter().position(|sb| {
                    sb.kind == SandboxKind::Container && sb.host_pids.iter().any(|h| h.pid == ppid)
                }) {
                    drop.push(k);
                }
            }
        }
        drop.sort_unstable();
        drop.dedup();
        let mut shim_pids: Vec<(usize, Vec<ProcId>)> = Vec::new();
        for (ci, c) in eng.containers.iter().enumerate() {
            let parents: Vec<ProcId> = c
                .host_pids
                .iter()
                .filter_map(|id| s.process(*id).and_then(|p| p.ppid))
                .filter_map(|pp| s.process_by_pid(pp).map(|p| p.id))
                .filter(|pid| drop.iter().any(|&k| sandboxes[k].host_pids.contains(pid)))
                .collect();
            shim_pids.push((ci, parents));
        }
        for k in drop.into_iter().rev() {
            sandboxes.remove(k);
        }
        for (ci, extra) in shim_pids {
            let mut c = eng.containers[ci].clone();
            for e in extra {
                if !c.host_pids.contains(&e) {
                    c.host_pids.insert(0, e);
                }
            }
            sandboxes.push(c);
        }
        return;
    }
    // VM-hosted engine: annotate the VM sandbox, then list containers (host cost is the VM's).
    let working: Vec<u64> = eng.containers.iter().filter_map(|c| c.guest_mem.value).collect();
    let vm = sandboxes.iter_mut().find(|sb| {
        sb.kind == SandboxKind::Vm
            && match eng.runtime.as_str() {
                "docker-desktop" => {
                    sb.started_by_group.as_deref().is_some_and(|g| g == "app:docker")
                        || sb.label.starts_with("Docker")
                }
                "orbstack" => sb.runtime == "orbstack" || sb.label.starts_with("OrbStack"),
                "rancher-desktop" => sb.label.starts_with("Rancher Desktop"),
                "colima" => sb.runtime == "colima" || (sb.runtime == "lima" && sb.label.contains("colima")),
                "podman" => matches!(sb.runtime.as_str(), "vfkit" | "krunkit"),
                _ => false,
            }
    });
    let mem = eng.info.mem_total;
    match vm {
        Some(v) => {
            if let (Some(m), false) = (mem, v.configured_mem.is_available()) {
                v.configured_mem = Measured::exact(m, format!("{} /info MemTotal", eng.runtime));
                v.limits.mem_max = Some(m);
            }
            v.limits.cpus = v.limits.cpus.or(eng.info.ncpu);
            if !working.is_empty() {
                v.guest_mem = Measured::estimate(working.iter().sum(), "Σ container memory (engine stats)");
            }
        }
        None => sandboxes.push(Sandbox {
            id: format!("{}:vm", eng.runtime),
            kind: SandboxKind::Vm,
            runtime: eng.runtime.clone(),
            label: format!("{} VM", title(&eng.runtime)),
            host_pids: Vec::new(),
            configured_mem: mem
                .map(|m| Measured::exact(m, format!("{} /info MemTotal", eng.runtime)))
                .unwrap_or_else(|| Measured::unavailable(eng.runtime.clone(), "not reported")),
            guest_mem: if working.is_empty() {
                Measured::unavailable(eng.runtime.clone(), "no running containers")
            } else {
                Measured::estimate(working.iter().sum(), "Σ container memory (engine stats)")
            },
            limits: SandboxLimits {
                mem_max: mem,
                cpus: eng.info.ncpu,
            },
            started_by_group: eng.containers.first().and_then(|c| c.started_by_group.clone()),
            footprint_lower_bound: false,
        }),
    }
    sandboxes.extend(eng.containers.iter().cloned());
}

fn title(runtime: &str) -> &'static str {
    match runtime {
        "docker-desktop" => "Docker Desktop",
        "orbstack" => "OrbStack",
        "colima" => "Colima",
        "rancher-desktop" => "Rancher Desktop",
        "podman" => "Podman machine",
        _ => "Docker",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::{Group, Member, Process};

    const CONTAINERS: &str = r#"[
      {"Id":"aaaa1111bbbb2222cccc3333dddd4444eeee5555ffff6666aaaa1111bbbb2222","Names":["/web"],"Image":"nginx:1.27","State":"running","Labels":{"com.docker.compose.project":"shop"}},
      {"Id":"bbbb1111bbbb2222cccc3333dddd4444eeee5555ffff6666aaaa1111bbbb2222","Names":["/db"],"Image":"postgres:17","State":"running","Labels":{}}
    ]"#;
    const STATS_V2: &str = r#"{"memory_stats":{"usage":209715200,"limit":8589934592,"stats":{"inactive_file":52428800,"anon":150000000}}}"#;
    const STATS_V1: &str =
        r#"{"memory_stats":{"usage":100,"limit":1000,"stats":{"total_inactive_file":30}}}"#;
    const INSPECT: &str = r#"{"State":{"Pid":4321,"Status":"running"},"HostConfig":{"Memory":536870912,"NanoCpus":1500000000}}"#;
    const INSPECT_NOLIMIT: &str =
        r#"{"State":{"Pid":0},"HostConfig":{"Memory":0,"CpuQuota":50000,"CpuPeriod":100000}}"#;
    const INFO: &str = r#"{"MemTotal":8217473024,"NCPU":6,"OperatingSystem":"Docker Desktop","Name":"docker-desktop","ServerVersion":"28.3.2"}"#;

    fn v(t: &str) -> serde_json::Value {
        serde_json::from_str(t).unwrap()
    }

    #[test]
    fn parsers() {
        let cs = parse_containers(&v(CONTAINERS));
        assert_eq!(cs.len(), 2);
        // Ids that would change the request path are dropped.
        let bad = parse_containers(&v(
            r#"[{"Id":"../../info?x="},{"Id":"a b"},{"Id":""},{"Id":"abc/../x"},{"Id":"ok123"}]"#,
        ));
        assert_eq!(
            bad.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["ok123"]
        );
        assert_eq!(cs[0].name, "web");
        assert_eq!(cs[0].labels["com.docker.compose.project"], "shop");
        let st = parse_stats(&v(STATS_V2));
        assert_eq!(st.working_set, Some(209715200 - 52428800));
        assert_eq!(parse_stats(&v(STATS_V1)).working_set, Some(70));
        assert_eq!(parse_stats(&v("{}")), ContainerStats::default());
        let i = parse_inspect(&v(INSPECT));
        assert_eq!(
            (i.pid, i.mem_limit, i.cpus),
            (Some(4321), Some(536870912), Some(1.5))
        );
        let i = parse_inspect(&v(INSPECT_NOLIMIT));
        assert_eq!((i.pid, i.mem_limit, i.cpus), (None, None, Some(0.5)));
        let info = parse_info(&v(INFO));
        assert_eq!(info.mem_total, Some(8217473024));
        assert_eq!(info.ncpu, Some(6.0));
    }

    /// Serves the Docker API paths used by `probe_engine` on a unix socket.
    #[cfg(unix)]
    fn mock_engine(sock: &Path, info: &'static str, requests: usize) -> std::thread::JoinHandle<Vec<String>> {
        let server = tiny_http::Server::http_unix(sock).unwrap();
        std::thread::spawn(move || {
            let mut urls = Vec::new();
            for _ in 0..requests {
                let Ok(Some(req)) = server.recv_timeout(Duration::from_secs(5)) else {
                    break;
                };
                let url = req.url().to_string();
                let body = if url == "/info" {
                    info
                } else if url.starts_with("/containers/json") {
                    CONTAINERS
                } else if url.contains("/stats") {
                    STATS_V2
                } else {
                    INSPECT
                };
                urls.push(url);
                req.respond(tiny_http::Response::from_string(body)).unwrap();
            }
            urls
        })
    }

    #[cfg(unix)]
    #[test]
    fn desktop_engine_annotates_app_vm() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("docker.sock");
        let h = mock_engine(&sock, INFO, 6);
        let vm_id = ProcId::new(900, 1);
        let mut s = Snapshot::default();
        s.processes.push(Process {
            id: vm_id,
            name: "com.apple.Virtualization.Virtua".into(),
            exe: "/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine".into(),
            ..Default::default()
        });
        s.groups.push(Group {
            id: "app:docker".into(),
            kind: GroupKind::App,
            label: "Docker".into(),
            members: vec![Member {
                id: vm_id,
                ..Default::default()
            }],
            ..Default::default()
        });
        let env = HostEnv {
            is_macos: true,
            ..Default::default()
        };
        let ep = EngineSocket {
            runtime: "docker-desktop",
            path: sock.clone(),
        };
        // Generous bounds: this test checks the annotations, not timeouts (a stalled scheduler on a
        // loaded CI box must not turn into a missing /info).
        let eng = probe_engine(
            &ep,
            &s,
            &env,
            Duration::from_secs(20),
            Duration::from_secs(60),
            8,
            0,
        )
        .unwrap();
        let urls = h.join().unwrap();
        assert_eq!(urls[0], "/info");
        assert_eq!(eng.info_error, None, "/info failed");
        assert!(
            urls.iter()
                .any(|u| u.contains("stats?stream=false&one-shot=true")),
            "{urls:?}"
        );
        assert!(!eng.native);
        assert_eq!(eng.containers.len(), 2);
        let web = eng.containers.iter().find(|c| c.label == "web").unwrap();
        assert_eq!(web.guest_mem.value, Some(209715200 - 52428800));
        assert_eq!(web.configured_mem.value, Some(536870912));
        assert_eq!(web.limits.cpus, Some(1.5));
        assert!(web.host_pids.is_empty(), "containers run inside the VM on macOS");
        assert_eq!(web.started_by_group.as_deref(), Some("app:docker"));

        let mut sandboxes = crate::sandboxes::detect_sandboxes(&s);
        assert_eq!(sandboxes[0].label, "Docker VM");
        merge_engine(&mut sandboxes, &eng, &s);
        let vm = &sandboxes[0];
        assert_eq!(vm.configured_mem.value, Some(8217473024));
        assert_eq!(vm.limits.cpus, Some(6.0));
        assert_eq!(vm.guest_mem.value, Some(2 * (209715200 - 52428800)));
        assert_eq!(sandboxes.len(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn native_engine_replaces_shims_and_budget_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("docker.sock");
        // max_stats = 1: one container gets stats+inspect, the other is skipped.
        let h = mock_engine(
            &sock,
            r#"{"MemTotal":33000000000,"NCPU":16,"OperatingSystem":"Ubuntu 24.04"}"#,
            4,
        );
        let shim = ProcId::new(4000, 1);
        let init = ProcId::new(4321, 2);
        let mut s = Snapshot::default();
        s.processes.push(Process {
            id: shim,
            ppid: Some(1),
            name: "containerd-shim-runc-v2".into(),
            exe: "/usr/bin/containerd-shim-runc-v2".into(),
            cmdline: vec![
                "containerd-shim-runc-v2".into(),
                "-id".into(),
                "aaaa1111bbbb".into(),
            ],
            ..Default::default()
        });
        s.processes.push(Process {
            id: init,
            ppid: Some(4000),
            name: "nginx".into(),
            exe: "/usr/sbin/nginx".into(),
            cgroup: Some(
                "/system.slice/docker-aaaa1111bbbb2222cccc3333dddd4444eeee5555ffff6666aaaa1111bbbb2222.scope"
                    .into(),
            ),
            ..Default::default()
        });
        let env = HostEnv::default();
        let ep = EngineSocket {
            runtime: "docker",
            path: sock.clone(),
        };
        let eng = probe_engine(
            &ep,
            &s,
            &env,
            Duration::from_secs(2),
            Duration::from_secs(5),
            1,
            0,
        )
        .unwrap();
        h.join().unwrap();
        assert!(eng.native);
        assert_eq!(eng.stats_skipped, 1);
        let web = eng.containers.iter().find(|c| c.label == "web").unwrap();
        assert_eq!(web.host_pids, vec![init]);
        let db = eng.containers.iter().find(|c| c.label == "db").unwrap();
        assert!(db.guest_mem.value.is_none());
        assert!(db
            .guest_mem
            .unavailable_reason()
            .unwrap()
            .contains("next refresh"));
        let mut sandboxes = crate::sandboxes::detect_sandboxes(&s);
        assert_eq!(sandboxes.len(), 1, "the shim");
        merge_engine(&mut sandboxes, &eng, &s);
        let web = sandboxes.iter().find(|c| c.label == "web").unwrap();
        assert_eq!(web.host_pids, vec![shim, init], "shim folded into its container");
        assert!(
            sandboxes.iter().all(|c| !c.label.starts_with("container ")),
            "{sandboxes:#?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sockets_only_when_runtime_process_exists() {
        use std::os::unix::net::UnixListener;
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".orbstack/run")).unwrap();
        let sock = home.path().join(".orbstack/run/docker.sock");
        let _l = UnixListener::bind(&sock).unwrap();
        let env = HostEnv {
            home: Some(home.path().to_path_buf()),
            ..Default::default()
        };
        let mut s = Snapshot::default();
        assert!(
            engine_sockets(&s, &env).is_empty(),
            "no OrbStack process → no probe"
        );
        s.processes.push(Process {
            id: ProcId::new(5, 1),
            name: "OrbStack Helper".into(),
            exe: "/Applications/OrbStack.app/Contents/Frameworks/OrbStack Helper.app/Contents/MacOS/OrbStack Helper".into(),
            ..Default::default()
        });
        let eps = engine_sockets(&s, &env);
        assert_eq!(
            eps,
            vec![EngineSocket {
                runtime: "orbstack",
                path: sock
            }]
        );
    }
}

//! Machine profile (UX §6): hardware facts, detected roles and the adaptations they imply. Built from a
//! snapshot (pure) plus a bounded scan of the configured model folders (headers are never read), stored in
//! `oomtop-state` and refreshed at most daily. Roles are sticky for [`ROLE_TTL_MS`] so a model server that
//! isn't running right now doesn't erase the "local LLM" role.

use oomtop_core::{GroupKind, ModelServerKind, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// A role not observed for this long is dropped.
pub const ROLE_TTL_MS: u64 = 30 * 86_400_000;
/// Bounded model-folder scan: entries visited at most.
pub const SCAN_MAX_ENTRIES: usize = 4000;
const SCAN_MAX_DEPTH: usize = 3;
/// Wall-time budget for the scan (it runs at most once a day, but must not delay the first frame much).
pub const SCAN_BUDGET: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Model servers (Ollama, llama.cpp, LM Studio, vLLM, MLX) or gguf/safetensors files.
    LocalLlm,
    /// Diffusion servers (sd.cpp, ComfyUI-style).
    Diffusion,
    /// Agent CLIs (Claude Code, Codex, …).
    AgentHeavy,
    /// Gradle / Kotlin / JVM daemons.
    JvmAndroid,
    /// node / tsserver / browsers.
    WebDev,
    /// Docker / OrbStack / Podman / VMs.
    ContainerUser,
}

impl Role {
    pub fn describe(self) -> &'static str {
        match self {
            Role::LocalLlm => "local LLM (model servers / model files)",
            Role::Diffusion => "image diffusion",
            Role::AgentHeavy => "agent-heavy (agent CLIs)",
            Role::JvmAndroid => "JVM / Android dev (Gradle, Kotlin daemons)",
            Role::WebDev => "web dev (node, tsserver, browsers)",
            Role::ContainerUser => "containers / VMs",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GpuInfo {
    pub vendor: String,
    pub name: String,
    pub unified: bool,
    pub vram: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Hardware {
    pub model: Option<String>,
    pub cpu: Option<String>,
    pub arch: String,
    pub os: String,
    pub cores_logical: u32,
    pub cores_performance: Option<u32>,
    pub cores_efficiency: Option<u32>,
    pub ram: u64,
    pub unified_memory: bool,
    pub fanless: Option<bool>,
    pub has_battery: bool,
    pub gpus: Vec<GpuInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Adaptations {
    /// Show a GPU column (a usable GPU with memory or utilization data).
    pub gpu_column: bool,
    /// Thermal-prone (fanless or unified-memory laptop): throttle insights enabled, larger margin advised.
    pub thermal_prone: bool,
    /// Suggested extra safety margin over the default, in percent of RAM (applied only if the user opts in).
    pub suggested_margin_pct: f64,
    /// Insight generators worth enabling for this machine.
    pub insights: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MachineProfile {
    pub version: u32,
    pub hardware: Hardware,
    /// Role → last observed (ms).
    pub roles: BTreeMap<Role, u64>,
    pub adaptations: Adaptations,
    /// Model files found in the configured folders (count only; names are not stored).
    pub model_files: u64,
    pub updated_ms: u64,
}

fn name_has(s: &Snapshot, needles: &[&str]) -> bool {
    s.processes.iter().any(|p| {
        let n = p.name.to_ascii_lowercase();
        needles.iter().any(|x| n == *x || n.starts_with(x))
    })
}

/// Roles visible in this snapshot.
pub fn roles_in(s: &Snapshot, model_files: u64) -> Vec<Role> {
    let mut r = Vec::new();
    let llm = s.model_servers.iter().any(|m| m.kind != ModelServerKind::SdCpp) || model_files > 0;
    if llm {
        r.push(Role::LocalLlm);
    }
    if s.model_servers.iter().any(|m| m.kind == ModelServerKind::SdCpp)
        || name_has(s, &["sd-server", "comfyui"])
    {
        r.push(Role::Diffusion);
    }
    if s.groups.iter().any(|g| g.kind == GroupKind::AgentSession) {
        r.push(Role::AgentHeavy);
    }
    let jvm = s.groups.iter().any(|g| {
        g.kind == GroupKind::BuildDaemon && {
            let l = g.label.to_ascii_lowercase();
            l.contains("gradle") || l.contains("kotlin") || l.contains("java")
        }
    });
    if jvm {
        r.push(Role::JvmAndroid);
    }
    if name_has(s, &["node", "tsserver", "bun", "deno", "vite"]) {
        r.push(Role::WebDev);
    }
    if !s.sandboxes.is_empty() || name_has(s, &["dockerd", "com.docker", "orbstack", "podman", "containerd"])
    {
        r.push(Role::ContainerUser);
    }
    r
}

/// Hardware facts from a snapshot.
pub fn hardware(s: &Snapshot) -> Hardware {
    let h = &s.host;
    Hardware {
        model: h.model.clone(),
        cpu: h.cpu_brand.clone(),
        arch: h.arch.clone(),
        os: h.os_version.clone(),
        cores_logical: h.cores_logical,
        cores_performance: h.cores_performance,
        cores_efficiency: h.cores_efficiency,
        ram: h.mem_total,
        unified_memory: h.unified_memory,
        fanless: h.fanless,
        has_battery: s.thermal.battery_pct.is_available(),
        gpus: s
            .accelerators
            .iter()
            .map(|a| GpuInfo {
                vendor: format!("{:?}", a.vendor).to_ascii_lowercase(),
                name: a.name.clone(),
                unified: a.unified,
                vram: if a.unified { None } else { a.mem_total.value },
            })
            .collect(),
    }
}

/// Builds (or refreshes) the profile. `prev` roles are kept until they expire.
pub fn build(s: &Snapshot, model_files: u64, prev: Option<&MachineProfile>, now_ms: u64) -> MachineProfile {
    let hw = hardware(s);
    let mut roles: BTreeMap<Role, u64> = prev
        .map(|p| {
            p.roles
                .iter()
                .filter(|(_, t)| now_ms.saturating_sub(**t) < ROLE_TTL_MS)
                .map(|(r, t)| (*r, *t))
                .collect()
        })
        .unwrap_or_default();
    for r in roles_in(s, model_files) {
        roles.insert(r, now_ms);
    }
    let gpu_column = s
        .accelerators
        .iter()
        .any(|a| a.mem_used.is_available() || a.util_pct.is_available());
    let thermal_prone = hw.fanless == Some(true) || (hw.unified_memory && hw.has_battery);
    let mut insights = vec!["headroom".to_string(), "reclaim".to_string()];
    if thermal_prone {
        insights.push("throttle".into());
    }
    if roles.contains_key(&Role::LocalLlm) || roles.contains_key(&Role::Diffusion) {
        insights.push("model_fit".into());
    }
    if roles.contains_key(&Role::AgentHeavy) {
        insights.push("agent_sessions".into());
    }
    if roles.contains_key(&Role::JvmAndroid) {
        insights.push("idle_build_daemons".into());
    }
    MachineProfile {
        version: 1,
        adaptations: Adaptations {
            gpu_column,
            thermal_prone,
            suggested_margin_pct: if thermal_prone && hw.unified_memory {
                4.0
            } else {
                0.0
            },
            insights,
        },
        hardware: hw,
        roles,
        model_files,
        updated_ms: now_ms,
    }
}

/// Counts `.gguf` / `.safetensors` files under the folders (depth ≤ 3, ≤ [`SCAN_MAX_ENTRIES`] entries, at most
/// [`SCAN_BUDGET`] of wall time; symlinks not followed; unreadable dirs skipped). Only the count is kept.
pub fn count_model_files(folders: &[std::path::PathBuf]) -> u64 {
    struct Walk {
        seen: usize,
        count: u64,
        deadline: std::time::Instant,
    }
    impl Walk {
        fn exhausted(&self) -> bool {
            self.seen >= SCAN_MAX_ENTRIES || std::time::Instant::now() >= self.deadline
        }
        fn dir(&mut self, dir: &Path, depth: usize) {
            if depth > SCAN_MAX_DEPTH || self.exhausted() {
                return;
            }
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                self.seen += 1;
                if self.exhausted() {
                    return;
                }
                let Ok(ft) = e.file_type() else { continue };
                let p = e.path();
                if ft.is_dir() {
                    self.dir(&p, depth + 1);
                } else if ft.is_file()
                    && p.extension().is_some_and(|x| {
                        let x = x.to_ascii_lowercase();
                        x == "gguf" || x == "safetensors"
                    })
                {
                    self.count += 1;
                }
            }
        }
    }
    let mut w = Walk {
        seen: 0,
        count: 0,
        deadline: std::time::Instant::now() + SCAN_BUDGET,
    };
    for f in folders {
        w.dir(f, 0);
    }
    w.count
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::{Group, ModelServer, Process};

    fn snap() -> Snapshot {
        let mut s = Snapshot::default();
        s.host.mem_total = 24 << 30;
        s.host.unified_memory = true;
        s.host.fanless = Some(true);
        s.groups.push(Group {
            id: "daemon:gradle".into(),
            kind: GroupKind::BuildDaemon,
            label: "GradleDaemon".into(),
            ..Default::default()
        });
        s.groups.push(Group {
            id: "agent:1".into(),
            kind: GroupKind::AgentSession,
            label: "Claude Code".into(),
            ..Default::default()
        });
        s.model_servers.push(ModelServer {
            id: "sd".into(),
            kind: ModelServerKind::SdCpp,
            ..Default::default()
        });
        s.processes.push(Process {
            name: "node".into(),
            ..Default::default()
        });
        s
    }

    #[test]
    fn roles_and_adaptations() {
        let p = build(&snap(), 0, None, 1000);
        let roles: Vec<Role> = p.roles.keys().copied().collect();
        assert_eq!(
            roles,
            vec![Role::Diffusion, Role::AgentHeavy, Role::JvmAndroid, Role::WebDev]
        );
        assert!(p.adaptations.thermal_prone);
        assert!(!p.adaptations.gpu_column);
        assert!(p.adaptations.insights.contains(&"throttle".to_string()));
        assert!(p.adaptations.insights.contains(&"model_fit".to_string()));
    }

    #[test]
    fn roles_are_sticky_until_they_expire() {
        let first = build(&snap(), 3, None, 1000);
        assert!(first.roles.contains_key(&Role::LocalLlm));
        let later = build(&Snapshot::default(), 0, Some(&first), 1000 + 86_400_000);
        assert!(later.roles.contains_key(&Role::LocalLlm), "kept for a day");
        let much_later = build(&Snapshot::default(), 0, Some(&first), 1000 + ROLE_TTL_MS);
        assert!(much_later.roles.is_empty());
    }

    #[test]
    fn model_file_scan_is_bounded_and_counts() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("a/b")).unwrap();
        std::fs::write(d.path().join("a/m.gguf"), b"x").unwrap();
        std::fs::write(d.path().join("a/b/w.safetensors"), b"x").unwrap();
        std::fs::write(d.path().join("a/b/readme.md"), b"x").unwrap();
        assert_eq!(count_model_files(&[d.path().to_path_buf()]), 2);
        assert_eq!(count_model_files(&[d.path().join("missing")]), 0);
    }

    #[test]
    fn serde_round_trip() {
        let p = build(&snap(), 1, None, 5);
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["roles"]["agent_heavy"], 5);
        let back: MachineProfile = serde_json::from_value(v).unwrap();
        assert_eq!(back, p);
    }
}

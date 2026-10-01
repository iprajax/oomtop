//! # oomtop-adapters
//!
//! Local model-server & sandbox adapters (SPEC §10, §11). Adapters probe **127.0.0.1 and local unix sockets
//! only**, only when a matching process exists (no port scanning), with 200 ms timeouts, no proxies and no
//! redirects. Gentle actions (Ollama unload, `lms unload`) come before signals (SPEC §13) and run only when
//! a caller executes them after explicit confirmation.
//!
//! | module | what |
//! |---|---|
//! | [`models`] | Ollama, llama.cpp, sd.cpp, vLLM, LM Studio, MLX, generic weight users; gentle actions; stop guard |
//! | [`sandboxes`] | VMs, microVMs, containers, process sandboxes; configured VM sizes |
//! | [`docker`] | Docker Engine API (Docker, Docker Desktop, OrbStack, Colima, Rancher Desktop, Podman) |
//! | [`weights`] | weight files open or mapped by a process (macOS `proc_pidinfo`, Linux `/proc`) |
//! | [`model_files`] | load estimates from headers (never the weights) |
//! | [`model_index`] | model files on disk: sizes, last used, duplicates, which server has them loaded |
//! | [`prober`] | caches and budgets; [`probe`] uses a shared default [`Prober`] |

pub mod docker;
pub mod env;
pub mod http;
pub mod model_files;
pub mod model_index;
pub mod models;
pub mod prober;
pub mod prom;
pub mod sandboxes;
pub mod weights;

pub use env::HostEnv;
pub use http::ensure_loopback;
pub use model_files::estimate_model_file;
pub use model_index::{IndexOptions, ModelIndex};
pub use models::{detect_model_servers, execute_gentle, gentle_actions, stop_guard, GentleAction, StopGuard};
pub use prober::{Prober, ProberConfig};
pub use sandboxes::detect_sandboxes;

use oomtop_core::{ModelServer, ProcId, Sandbox, Snapshot, SourceStatus};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use thiserror::Error;

/// Default HTTP timeout for adapter endpoints.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeOptions {
    pub timeout: Duration,
    /// When false, only process/argv-level detection runs: no HTTP, no sockets, no file reads (replay,
    /// tests and `--offline`).
    pub http: bool,
    /// Per-kind port overrides from config `[adapters]` (e.g. "ollama" → 11434). Host is always 127.0.0.1.
    pub ports: BTreeMap<String, u16>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        ProbeOptions {
            timeout: DEFAULT_TIMEOUT,
            http: true,
            ports: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AdapterError {
    #[error("refusing non-loopback endpoint {0}")]
    NotLoopback(String),
    #[error("http: {0}")]
    Http(String),
    #[error("HTTP status {0}")]
    Status(u16),
    #[error("timed out")]
    Timeout,
    #[error("{0}")]
    Io(String),
    #[error("unexpected response: {0}")]
    Decode(String),
    #[error("not supported for this server")]
    Unsupported,
}

/// Result of one probe.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Probe {
    pub model_servers: Vec<ModelServer>,
    pub sandboxes: Vec<Sandbox>,
    /// "adapter.<kind>" → status.
    pub status: BTreeMap<String, SourceStatus>,
    /// Weight files found open or mapped per process (written to `Process::model_files` by [`apply`]).
    pub process_model_files: BTreeMap<ProcId, Vec<String>>,
}

fn shared() -> &'static Mutex<Prober> {
    static P: OnceLock<Mutex<Prober>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Prober::default()))
}

/// Probes model servers and sandboxes for the processes in `snapshot` (groups should already be attributed
/// so `group_id` / `started_by_group` can be filled). Uses a process-wide [`Prober`] (caches, budgets).
pub fn probe(snapshot: &Snapshot, opts: &ProbeOptions) -> Probe {
    let mut p = shared().lock().unwrap_or_else(|e| e.into_inner());
    p.probe(snapshot, opts)
}

/// Writes probe results into the snapshot: model servers, sandboxes, per-process weight files, source
/// status; groups holding VM processes become lower bounds and get the sandbox's configured memory.
pub fn apply(snapshot: &mut Snapshot, probe: Probe) {
    for sb in &probe.sandboxes {
        let limit = sb.configured_mem.value;
        if !sb.footprint_lower_bound && limit.is_none() {
            continue;
        }
        for g in &mut snapshot.groups {
            if g.members.iter().any(|m| sb.host_pids.contains(&m.id)) {
                if sb.footprint_lower_bound {
                    g.lower_bound = true;
                }
                if g.configured_mem.is_none() {
                    g.configured_mem = limit;
                }
            }
        }
    }
    for (id, files) in &probe.process_model_files {
        if let Some(p) = snapshot.processes.iter_mut().find(|p| p.id == *id) {
            for f in files {
                if !p.model_files.contains(f) {
                    p.model_files.push(f.clone());
                }
            }
        }
    }
    snapshot.model_servers = probe.model_servers;
    snapshot.sandboxes = probe.sandboxes;
    snapshot.source_status.extend(probe.status);
}

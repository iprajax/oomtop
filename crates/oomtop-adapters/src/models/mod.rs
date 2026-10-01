//! Model-server discovery (SPEC §10): process + argv first, then the server's own loopback API.
//!
//! | kind | process | API (127.0.0.1 only) |
//! |---|---|---|
//! | Ollama | `ollama serve` / `ollama runner` | `/api/ps` (+ manifests → weight blob), `/api/tags` |
//! | llama.cpp | `llama-server`, llamafile | `/props`, `/slots`, `/metrics` |
//! | sd.cpp | `sd-server` | `/sdcpp/v1/capabilities`, job list when exposed |
//! | vLLM | `vllm serve`, `vllm.entrypoints.*` | Prometheus `/metrics`, `/v1/models` |
//! | LM Studio | the app (`LM Studio`) | `/api/v0/models` |
//! | MLX | `mlx_lm.server` | process-level (argv + HF cache) |
//! | Generic | anything with weight files open/mapped | process-level |
//!
//! HTTP runs only when `ProbeOptions::http` is set; offline (replay / `--offline`) no local I/O happens at all
//! (a replayed snapshot comes from another machine). Unreachable endpoints never fail the probe: the server
//! is still listed with `status` saying why.

pub mod llama_cpp;
pub mod lm_studio;
pub mod ollama;
pub mod sd_cpp;
pub mod vllm;

pub use ollama::{parse_ollama_ps, parse_ollama_tags};

use crate::env::HostEnv;
use crate::model_index::hf_repo_weight_bytes;
use crate::prober::Caches;
use crate::{AdapterError, ProbeOptions};
use oomtop_core::{
    Device, LoadedModel, Measured, ModelServer, ModelServerKind, ProcId, Process, Snapshot, SourceStatus,
};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

/// A server whose processes use more CPU than this (% of one core, summed) is assumed busy when its API
/// does not say (heuristic, `Estimate`).
pub const BUSY_CPU_PCT: f64 = 10.0;

pub(crate) fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

fn exe_name(p: &Process) -> &str {
    if p.exe.is_empty() {
        p.cmdline.first().map(|a| basename(a)).unwrap_or("")
    } else {
        basename(&p.exe)
    }
}

/// Kind of model server a process is, if any. `ollama run/pull/…` clients and `vllm chat` are not servers.
pub fn classify(p: &Process) -> Option<ModelServerKind> {
    let exe = exe_name(p);
    let argv1 = p.cmdline.get(1).map(String::as_str).unwrap_or("");
    let joined = p.cmdline.join(" ");
    match exe {
        "ollama" => return matches!(argv1, "serve" | "runner").then_some(ModelServerKind::Ollama),
        "ollama_llama_server" => return Some(ModelServerKind::Ollama),
        "llama-server" | "llama-cpp-server" | "llamafile" => return Some(ModelServerKind::LlamaCpp),
        "sd-server" => return Some(ModelServerKind::SdCpp),
        "LM Studio" | "lm-studio" | "lmstudio" => return Some(ModelServerKind::LmStudio),
        "mlx_lm.server" | "mlx_vlm.server" => return Some(ModelServerKind::Mlx),
        "vllm" => return (argv1 == "serve").then_some(ModelServerKind::Vllm),
        _ => {}
    }
    if exe.ends_with(".llamafile") {
        return Some(ModelServerKind::LlamaCpp);
    }
    if joined.contains("vllm.entrypoints") || has_word_pair(&p.cmdline, "vllm", "serve") {
        return Some(ModelServerKind::Vllm);
    }
    if ["mlx_lm.server", "mlx_lm/server", "mlx_vlm.server"]
        .iter()
        .any(|m| joined.contains(m))
        || has_word_pair(&p.cmdline, "mlx_lm", "server")
    {
        return Some(ModelServerKind::Mlx);
    }
    if joined.contains("llama_cpp.server")
        || matches!(
            exe,
            "koboldcpp" | "local-ai" | "whisper-server" | "sd-cli" | "invokeai-web"
        )
        || p.cmdline
            .iter()
            .skip(1)
            .any(|a| a.ends_with("/ComfyUI/main.py") || a.ends_with("koboldcpp.py"))
    {
        return Some(ModelServerKind::Generic);
    }
    if !p.model_files.is_empty() {
        return Some(ModelServerKind::Generic);
    }
    None
}

fn has_word_pair(argv: &[String], a: &str, b: &str) -> bool {
    argv.windows(2).any(|w| basename(&w[0]) == a && w[1] == b)
}

pub fn kind_key(k: ModelServerKind) -> &'static str {
    match k {
        ModelServerKind::Ollama => "ollama",
        ModelServerKind::LlamaCpp => "llama_cpp",
        ModelServerKind::SdCpp => "sd_cpp",
        ModelServerKind::Vllm => "vllm",
        ModelServerKind::LmStudio => "lm_studio",
        ModelServerKind::Mlx => "mlx",
        ModelServerKind::Generic => "generic",
    }
}

/// Default API port of each server kind.
pub fn default_port(k: ModelServerKind) -> Option<u16> {
    match k {
        ModelServerKind::Ollama => Some(11434),
        ModelServerKind::LlamaCpp => Some(8080),
        ModelServerKind::SdCpp => Some(1234),
        ModelServerKind::Vllm => Some(8000),
        ModelServerKind::LmStudio => Some(1234),
        ModelServerKind::Mlx => Some(8080),
        ModelServerKind::Generic => None,
    }
}

/// Value following `flag` (or `flag=value`) in argv.
pub fn arg_value<'a>(argv: &'a [String], flags: &[&str]) -> Option<&'a str> {
    for (i, a) in argv.iter().enumerate() {
        for f in flags {
            if a == f {
                return argv.get(i + 1).map(String::as_str);
            }
            if let Some(v) = a.strip_prefix(f).and_then(|r| r.strip_prefix('=')) {
                return Some(v);
            }
        }
    }
    None
}

fn port_flags(k: ModelServerKind) -> &'static [&'static str] {
    match k {
        ModelServerKind::SdCpp => &["--listen-port", "--port"],
        _ => &["--port"],
    }
}

fn host_flags(k: ModelServerKind) -> &'static [&'static str] {
    match k {
        ModelServerKind::SdCpp => &["--listen-ip", "--host"],
        _ => &["--host"],
    }
}

/// Why a server cannot be reached on 127.0.0.1, from its `--host` flag (`None` = reachable).
fn host_problem(p: &Process, k: ModelServerKind) -> Option<String> {
    let host = arg_value(&p.cmdline, host_flags(k))?;
    if host.ends_with(".sock") || host.starts_with('/') {
        return Some(format!("listens on unix socket {host}"));
    }
    match host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
        Ok(ip) if ip.is_loopback() || ip.is_unspecified() => None,
        Ok(ip) => Some(format!("listens on {ip} only; oomtop probes loopback only")),
        Err(_) if host == "localhost" => None,
        Err(_) => Some(format!("listens on {host}; oomtop probes loopback only")),
    }
}

/// The server's loopback base URL, if it can be probed. A server bound to the IPv6 loopback only
/// (`--host ::1`) is probed on `[::1]`; everything else on 127.0.0.1.
pub fn endpoint_of(p: &Process, k: ModelServerKind, opts: &ProbeOptions) -> Option<String> {
    if host_problem(p, k).is_some() {
        return None;
    }
    let v6 = arg_value(&p.cmdline, host_flags(k))
        .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip.is_ipv6() && ip.is_loopback());
    let host = if v6 { "[::1]" } else { "127.0.0.1" };
    arg_value(&p.cmdline, port_flags(k))
        .and_then(|v| v.parse::<u16>().ok())
        .or_else(|| opts.ports.get(kind_key(k)).copied())
        .or_else(|| default_port(k))
        .map(|port| format!("http://{host}:{port}"))
}

/// Flags that name weight files (llama.cpp, sd.cpp components, generic loaders).
pub const MODEL_FILE_FLAGS: &[&str] = &[
    "-m",
    "--model",
    "-md",
    "--model-draft",
    "--diffusion-model",
    "--vae",
    "--taesd",
    "--clip_l",
    "--clip_g",
    "--clip_vision",
    "--t5xxl",
    "--llm",
    "--llm_vision",
    "--qwen2vl",
    "--qwen2vl_vision",
    "--mmproj",
    "--control-net",
    "--upscale-model",
    "--photo-maker",
    "--lora",
];

fn is_weight_arg(v: &str) -> bool {
    [".gguf", ".safetensors", ".bin", ".ckpt", ".pt", ".pth", ".ggml"]
        .iter()
        .any(|e| v.to_ascii_lowercase().ends_with(e))
}

/// Weight files named on the command line (llama.cpp `-m`, sd.cpp component flags, generic `--model`).
pub fn argv_model_files(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, a) in argv.iter().enumerate() {
        let v = if MODEL_FILE_FLAGS.contains(&a.as_str()) {
            argv.get(i + 1).cloned()
        } else {
            MODEL_FILE_FLAGS.iter().find_map(|f| {
                a.strip_prefix(f)
                    .and_then(|r| r.strip_prefix('='))
                    .map(str::to_string)
            })
        };
        if let Some(v) = v.filter(|v| is_weight_arg(v)) {
            if !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

/// The model a server loads, by repo id or directory (`vllm serve X`, `mlx_lm.server --model X`): `--model`
/// / `-m`, else the positional argument after `serve`. Redacted (argv may carry credentials in URLs).
pub fn argv_model_source(argv: &[String]) -> Option<String> {
    let v = arg_value(argv, &["--model", "-m"])
        .map(str::to_string)
        .or_else(|| {
            let i = argv.iter().position(|a| a == "serve")?;
            argv.get(i + 1).filter(|v| !v.starts_with('-')).cloned()
        })?;
    Some(oomtop_core::redact::redact_text(&v)).filter(|v| !v.is_empty())
}

/// Display name of the served model: the model source, else `--served-model-name`.
pub fn argv_model_name(argv: &[String]) -> Option<String> {
    argv_model_source(argv).or_else(|| {
        arg_value(argv, &["--served-model-name"])
            .map(oomtop_core::redact::redact_text)
            .filter(|v| !v.is_empty())
    })
}

fn resolve(path: &str, cwd: Option<&str>) -> String {
    if path.starts_with('/') {
        return path.to_string();
    }
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME") {
            return Path::new(&h).join(stripped).display().to_string();
        }
    }
    match cwd {
        Some(c) => Path::new(c).join(path).display().to_string(),
        None => path.to_string(),
    }
}

pub(crate) fn file_size(path: &str, live: bool) -> Measured<u64> {
    if !live {
        return Measured::unavailable("file size", "not probed (offline)");
    }
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => Measured::exact(m.len(), "file size"),
        Ok(_) => Measured::unavailable("file size", "not a regular file"),
        Err(e) => Measured::unavailable("file size", e.kind().to_string()),
    }
}

pub(crate) fn file_model(path: &str, live: bool) -> LoadedModel {
    LoadedModel {
        name: basename(path).to_string(),
        file: Some(path.to_string()),
        weights_bytes: file_size(path, live),
        kv_bytes: Measured::unavailable("argv", "unknown until the server reports it"),
        device: Device::Unknown,
    }
}

/// Busy by CPU activity of the server's processes (heuristic, `Estimate`).
pub fn cpu_busy(s: &Snapshot, ms: &ModelServer) -> Measured<bool> {
    let from_group = ms
        .group_id
        .as_deref()
        .and_then(|g| s.group(g))
        .and_then(|g| g.totals.cpu_pct.value);
    let from_pids = || {
        let v: Vec<f64> = ms
            .pids
            .iter()
            .filter_map(|id| s.process(*id))
            .filter_map(|p| p.cpu_pct.value)
            .collect();
        (!v.is_empty()).then(|| v.iter().sum::<f64>())
    };
    match from_group.or_else(from_pids) {
        Some(cpu) => Measured::estimate(
            cpu >= BUSY_CPU_PCT,
            format!("cpu {cpu:.0}% (heuristic ≥ {BUSY_CPU_PCT:.0}%)"),
        ),
        None => Measured::unavailable("cpu heuristic", "cpu % not sampled yet"),
    }
}

fn pid_index(s: &Snapshot) -> HashMap<u32, usize> {
    s.processes
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id.pid, i))
        .collect()
}

/// Topmost ancestor (within 16 hops, including `i`) classified with the same kind.
fn server_root(
    s: &Snapshot,
    by_pid: &HashMap<u32, usize>,
    kinds: &[Option<ModelServerKind>],
    i: usize,
) -> usize {
    let kind = kinds[i];
    let mut root = i;
    let mut cur = s.processes[i].ppid;
    for _ in 0..16 {
        let Some(pi) = cur.filter(|&pp| pp > 1).and_then(|pp| by_pid.get(&pp).copied()) else {
            break;
        };
        if pi == root || s.processes[pi].id.start_time > s.processes[i].id.start_time {
            break;
        }
        if kinds[pi] == kind {
            root = pi;
        }
        cur = s.processes[pi].ppid;
    }
    root
}

/// Nearest ancestor (within 16 hops) that is any classified server.
fn classified_ancestor(
    s: &Snapshot,
    by_pid: &HashMap<u32, usize>,
    kinds: &[Option<ModelServerKind>],
    i: usize,
) -> Option<usize> {
    let mut cur = s.processes[i].ppid;
    for _ in 0..16 {
        let pi = cur.filter(|&pp| pp > 1).and_then(|pp| by_pid.get(&pp).copied())?;
        if pi == i {
            return None;
        }
        if kinds[pi].is_some() {
            return Some(pi);
        }
        cur = s.processes[pi].ppid;
    }
    None
}

/// Merges a per-server status into the per-kind `adapter.<kind>` status.
pub(crate) fn merge_status(status: &mut BTreeMap<String, SourceStatus>, key: String, new: &SourceStatus) {
    let merged = match (status.get(&key), new) {
        (None, n) => n.clone(),
        (Some(SourceStatus::Available), SourceStatus::Available) => SourceStatus::Available,
        (Some(SourceStatus::Unavailable(a)), SourceStatus::Unavailable(_)) => {
            SourceStatus::Unavailable(a.clone())
        }
        (Some(SourceStatus::Partial(a)), _) => SourceStatus::Partial(a.clone()),
        (Some(_), SourceStatus::Partial(b) | SourceStatus::Unavailable(b)) => {
            SourceStatus::Partial(b.clone())
        }
        (Some(SourceStatus::Unavailable(a)), SourceStatus::Available) => SourceStatus::Partial(a.clone()),
    };
    status.insert(key, merged);
}

/// Detects model servers among the snapshot's processes (one entry per server root process), probing their
/// loopback APIs when `opts.http` is set. Stateless variant of [`crate::Prober::probe`] (no caches).
pub fn detect_model_servers(
    s: &Snapshot,
    opts: &ProbeOptions,
    status: &mut BTreeMap<String, SourceStatus>,
) -> Vec<ModelServer> {
    let env = if opts.http {
        HostEnv::from_env()
    } else {
        HostEnv::default()
    };
    detect(s, opts, &BTreeMap::new(), &mut Caches::default(), &env, status)
}

pub(crate) fn detect(
    s: &Snapshot,
    opts: &ProbeOptions,
    scanned: &BTreeMap<ProcId, Vec<String>>,
    caches: &mut Caches,
    env: &HostEnv,
    status: &mut BTreeMap<String, SourceStatus>,
) -> Vec<ModelServer> {
    let live = opts.http;
    let by_pid = pid_index(s);
    let files_of = |p: &Process| -> Vec<String> {
        let mut v = p.model_files.clone();
        if let Some(f) = scanned.get(&p.id) {
            v.extend(f.iter().cloned());
        }
        v
    };
    let kinds: Vec<Option<ModelServerKind>> = s
        .processes
        .iter()
        .map(|p| classify(p).or_else(|| (!files_of(p).is_empty()).then_some(ModelServerKind::Generic)))
        .collect();

    // Group classified processes under their server root; generic weight users under a real server join it.
    let mut members: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..s.processes.len() {
        let Some(kind) = kinds[i] else { continue };
        if kind == ModelServerKind::Generic && classify(&s.processes[i]).is_none() {
            if let Some(a) = classified_ancestor(s, &by_pid, &kinds, i) {
                let root = server_root(s, &by_pid, &kinds, a);
                members.entry(root).or_default().push(i);
                continue;
            }
        }
        let root = server_root(s, &by_pid, &kinds, i);
        members.entry(root).or_default().push(i);
    }

    let mut out = Vec::new();
    for (root, mut idxs) in members {
        idxs.sort_unstable();
        idxs.dedup();
        let p = &s.processes[root];
        let Some(kind) = kinds[root] else { continue };
        let endpoint = endpoint_of(p, kind, opts);
        let mut files: Vec<String> = Vec::new();
        for &i in &idxs {
            let q = &s.processes[i];
            for f in argv_model_files(&q.cmdline) {
                files.push(resolve(&f, q.cwd.as_deref()));
            }
            files.extend(files_of(q));
        }
        dedup_files(&mut files, live);
        let mut ms = ModelServer {
            id: format!("{}:{}", kind_key(kind), p.id.pid),
            kind,
            endpoint: endpoint.clone(),
            pids: idxs.iter().map(|&i| s.processes[i].id).collect(),
            group_id: s.group_of(p.id).map(|g| g.id.clone()),
            models: files.iter().map(|f| file_model(f, live)).collect(),
            tok_s: Measured::unavailable("adapter", "not reported"),
            s_per_step: Measured::unavailable("adapter", "not reported"),
            queue: Measured::unavailable("adapter", "not reported"),
            busy: Measured::unavailable("adapter", "not reported"),
            progress: None,
            status: SourceStatus::Partial(if live {
                "process-level only".into()
            } else {
                "offline: process-level only".into()
            }),
        };
        if let Some(why) = host_problem(p, kind) {
            ms.status = SourceStatus::Partial(why);
        } else if live {
            if let Some(ep) = &endpoint {
                let r = match kind {
                    ModelServerKind::Ollama => ollama::enrich(&mut ms, ep, opts.timeout, env),
                    ModelServerKind::LlamaCpp => llama_cpp::enrich(&mut ms, p, ep, opts.timeout, caches),
                    ModelServerKind::SdCpp => sd_cpp::enrich(&mut ms, ep, opts.timeout),
                    ModelServerKind::Vllm => vllm::enrich(&mut ms, p, ep, opts.timeout, caches, env),
                    ModelServerKind::LmStudio => lm_studio::enrich(&mut ms, ep, opts.timeout),
                    ModelServerKind::Mlx | ModelServerKind::Generic => Ok(()),
                };
                if let Err(e) = r {
                    ms.status = match (kind, &e) {
                        (ModelServerKind::LmStudio, AdapterError::Http(_) | AdapterError::Io(_)) => {
                            SourceStatus::Partial(format!(
                                "local server not running ({e}); `lms server start`"
                            ))
                        }
                        _ => SourceStatus::Unavailable(e.to_string()),
                    };
                }
            }
            if kind == ModelServerKind::Mlx {
                mlx_models(&mut ms, p, env);
            }
        }
        if ms.busy.value.is_none() {
            let h = cpu_busy(s, &ms);
            if h.value.is_some() {
                ms.busy = h;
            }
        }
        merge_status(status, format!("adapter.{}", kind_key(kind)), &ms.status);
        out.push(ms);
    }
    out
}

/// Sorts, de-duplicates (by canonical path when live) and keeps argv order stable enough for display.
fn dedup_files(files: &mut Vec<String>, live: bool) {
    let mut seen: Vec<String> = Vec::new();
    files.retain(|f| {
        let key = if live {
            std::fs::canonicalize(f)
                .map(|c| c.display().to_string())
                .unwrap_or_else(|_| f.clone())
        } else {
            f.clone()
        };
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

/// MLX: the model is a repo id or a directory; its size comes from the directory or the HF cache.
fn mlx_models(ms: &mut ModelServer, p: &Process, env: &HostEnv) {
    let Some(name) = argv_model_source(&p.cmdline) else {
        return;
    };
    if ms.models.iter().any(|m| m.file.as_deref() == Some(name.as_str())) {
        return;
    }
    let (weights, file) = source_weights(&name, p, env);
    ms.models.push(LoadedModel {
        name,
        file,
        weights_bytes: weights,
        kv_bytes: Measured::unavailable("mlx", "not reported"),
        device: Device::Gpu,
    });
}

/// On-disk weight size of a model named by directory or HF repo id (`Qwen/Qwen3-8B`), plus the directory
/// when it is local. Exact for the files; what the server holds may differ (dtype casts, quantization).
pub(crate) fn source_weights(name: &str, p: &Process, env: &HostEnv) -> (Measured<u64>, Option<String>) {
    let local = resolve(name, p.cwd.as_deref());
    if Path::new(&local).is_dir() {
        let w = crate::model_index::dir_weight_bytes(Path::new(&local))
            .map(|b| Measured::exact(b, "Σ weight files in model dir"))
            .unwrap_or_else(|| Measured::unavailable("model dir", "no weight files"));
        return (w, Some(local));
    }
    let w = match hf_repo_weight_bytes(env, name) {
        Some(b) => Measured::exact(b, "Σ weight files in HF cache"),
        None => Measured::unavailable("HF cache", "repo not in local cache"),
    };
    (w, None)
}

// ---------------------------------------------------------------------------------------------------------
// Gentle actions & stop guard
// ---------------------------------------------------------------------------------------------------------

/// A graceful action that frees memory without stopping the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GentleAction {
    /// `POST /api/generate {"model": m, "keep_alive": 0}`.
    OllamaUnload { endpoint: String, model: String },
    /// `lms unload <model>` (LM Studio CLI).
    LmsUnload { model: String },
}

impl GentleAction {
    pub fn describe(&self) -> String {
        match self {
            GentleAction::OllamaUnload { model, .. } => format!("ollama: unload {model}"),
            GentleAction::LmsUnload { model } => format!("lms unload {model}"),
        }
    }
}

/// Graceful actions available for a server. Only models the server's own API reported are offered: when the
/// API did not answer, `models` holds argv / mapped-file guesses (blob hashes, file names) that are not
/// names the server would accept.
pub fn gentle_actions(ms: &ModelServer) -> Vec<GentleAction> {
    if ms.status != SourceStatus::Available {
        return Vec::new();
    }
    match (ms.kind, &ms.endpoint) {
        (ModelServerKind::Ollama, Some(ep)) => ms
            .models
            .iter()
            .filter(|m| !m.name.is_empty())
            .map(|m| GentleAction::OllamaUnload {
                endpoint: ep.clone(),
                model: m.name.clone(),
            })
            .collect(),
        (ModelServerKind::LmStudio, _) => ms
            .models
            .iter()
            .filter(|m| lm_studio::valid_model_id(&m.name))
            .map(|m| GentleAction::LmsUnload {
                model: m.name.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Executes a gentle action (only after explicit user confirmation; the caller owns that).
pub fn execute_gentle(a: &GentleAction, timeout: Duration) -> Result<(), AdapterError> {
    match a {
        GentleAction::OllamaUnload { endpoint, model } => ollama::unload(endpoint, model, timeout),
        GentleAction::LmsUnload { model } => lm_studio::lms_unload(model, timeout),
    }
}

/// Whether stopping a server now would interrupt work (SPEC §10: sd.cpp refuses while a job runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopGuard {
    /// Nothing known to be running.
    Allowed,
    /// A request/job is running: refuse unless the user explicitly confirms stopping anyway.
    Busy(String),
    /// Unknown (API unreachable / not reported): say so in the confirmation.
    Unknown(String),
}

/// Stop guard for a server, from its `busy` / `queue` / `progress` fields.
pub fn stop_guard(ms: &ModelServer) -> StopGuard {
    let what = match ms.kind {
        ModelServerKind::SdCpp => "a generation job",
        _ => "a request",
    };
    match ms.busy.value {
        Some(true) => {
            let detail = ms
                .progress
                .as_ref()
                .filter(|p| p.total > 0)
                .map(|p| format!(" ({}/{} {})", p.done, p.total, p.label))
                .unwrap_or_default();
            StopGuard::Busy(format!("{what} is running{detail} [{}]", ms.busy.source))
        }
        Some(false) => match ms.queue.value {
            Some(q) if q > 0 => StopGuard::Busy(format!("{q} queued [{}]", ms.queue.source)),
            // "Idle" from a CPU heuristic is not confirmation: a GPU-bound job (Metal, CUDA) can run with
            // an idle CPU thread waiting on the device.
            _ if ms.busy.quality != oomtop_core::Quality::Exact => StopGuard::Unknown(format!(
                "{what} may be running: the server does not report job state, and {} is only a heuristic",
                ms.busy.source
            )),
            _ => StopGuard::Allowed,
        },
        None => StopGuard::Unknown(
            ms.busy
                .unavailable_reason()
                .unwrap_or("busy state not reported")
                .to_string(),
        ),
    }
}

#[cfg(test)]
mod tests;

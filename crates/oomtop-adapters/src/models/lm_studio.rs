//! LM Studio: `/api/v0/models` (REST beta; `state: loaded | not-loaded`) and `lms unload <id>`.

use crate::http::get_json;
use crate::AdapterError;
use oomtop_core::{Device, LoadedModel, Measured, ModelServer, SourceStatus};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One entry of `/api/v0/models`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LmsModel {
    pub id: String,
    pub state: String,
    pub kind: Option<String>,
    pub arch: Option<String>,
    pub quantization: Option<String>,
    pub compatibility: Option<String>,
    pub loaded_context_length: Option<u64>,
    pub max_context_length: Option<u64>,
}

impl LmsModel {
    pub fn loaded(&self) -> bool {
        self.state == "loaded"
    }
}

pub fn parse_models(v: &serde_json::Value) -> Vec<LmsModel> {
    let s = |m: &serde_json::Value, k: &str| m.get(k).and_then(|x| x.as_str()).map(str::to_string);
    v.get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .map(|m| LmsModel {
                    id: s(m, "id").unwrap_or_default(),
                    state: s(m, "state").unwrap_or_default(),
                    kind: s(m, "type"),
                    arch: s(m, "arch"),
                    quantization: s(m, "quantization"),
                    compatibility: s(m, "compatibility_type"),
                    loaded_context_length: m.get("loaded_context_length").and_then(|x| x.as_u64()),
                    max_context_length: m.get("max_context_length").and_then(|x| x.as_u64()),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn enrich(ms: &mut ModelServer, ep: &str, timeout: Duration) -> Result<(), AdapterError> {
    let v = get_json(&format!("{ep}/api/v0/models"), timeout)?;
    let loaded: Vec<LmsModel> = parse_models(&v).into_iter().filter(|m| m.loaded()).collect();
    // Weight files seen open/mapped by LM Studio's processes: attributable only when one model is loaded.
    let mapped: Vec<LoadedModel> = std::mem::take(&mut ms.models);
    let mapped_total: Option<u64> = (!mapped.is_empty())
        .then(|| mapped.iter().filter_map(|m| m.weights_bytes.value).sum::<u64>())
        .filter(|&b| b > 0);
    let single = loaded.len() == 1;
    ms.models = loaded
        .into_iter()
        .map(|m| LoadedModel {
            weights_bytes: match (single, mapped_total) {
                (true, Some(b)) => Measured::estimate(b, "Σ weight files mapped by LM Studio"),
                _ => Measured::unavailable("lm studio /api/v0/models", "not reported"),
            },
            kv_bytes: Measured::unavailable(
                "lm studio /api/v0/models",
                m.loaded_context_length
                    .map(|c| format!("not reported (ctx {c})"))
                    .unwrap_or_else(|| "not reported".into()),
            ),
            file: if single && mapped.len() == 1 {
                mapped[0].file.clone()
            } else {
                None
            },
            device: match m.compatibility.as_deref() {
                Some("mlx") => Device::Gpu,
                _ => Device::Unknown,
            },
            name: m.id,
        })
        .collect();
    ms.queue = Measured::unavailable("lm studio", "not reported");
    ms.status = SourceStatus::Available;
    Ok(())
}

/// Model ids passed to `lms` must not look like flags or contain control characters.
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty() && !id.starts_with('-') && id.len() <= 512 && !id.chars().any(|c| c.is_control())
}

/// Locates the `lms` CLI: `$PATH`, then LM Studio's own bin dirs.
pub fn find_lms() -> Option<PathBuf> {
    find_lms_in(std::env::var_os("PATH"), std::env::var_os("HOME"))
}

/// [`find_lms`] over explicit `PATH` / `HOME` values. Relative `PATH` entries (`.`, empty) are skipped so a
/// stray `./lms` in the working directory is never executed.
pub fn find_lms_in(path: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = path
        .map(|p| {
            std::env::split_paths(&p)
                .filter(|d| d.is_absolute())
                .map(|d| d.join("lms"))
                .collect()
        })
        .unwrap_or_default();
    if let Some(h) = home.map(PathBuf::from).filter(|h| h.is_absolute()) {
        cands.push(h.join(".lmstudio/bin/lms"));
        cands.push(h.join(".cache/lm-studio/bin/lms"));
    }
    cands.into_iter().find(|c| c.is_file())
}

/// Runs `lms unload <id>` (at least 5 s; LM Studio needs a moment). The child is ours and is killed on
/// timeout. No shell is involved.
pub(crate) fn lms_unload(model: &str, timeout: Duration) -> Result<(), AdapterError> {
    if !valid_model_id(model) {
        return Err(AdapterError::Decode(format!("refusing model id {model:?}")));
    }
    let lms = find_lms().ok_or_else(|| {
        AdapterError::Io("`lms` CLI not found (LM Studio → Developer → install lms)".into())
    })?;
    run_with_timeout(
        Command::new(lms).arg("unload").arg(model),
        timeout.max(Duration::from_secs(5)),
    )
}

pub(crate) fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<(), AdapterError> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AdapterError::Io(e.to_string()))?;
    let end = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    use std::io::Read;
                    let _ = e.by_ref().take(4096).read_to_string(&mut err);
                }
                return Err(AdapterError::Io(format!("exit {st}: {}", err.trim())));
            }
            Ok(None) if Instant::now() >= end => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AdapterError::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(AdapterError::Io(e.to_string())),
        }
    }
}

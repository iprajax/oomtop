//! Ollama: `/api/ps` (loaded models), `/api/tags` (installed models), gentle unload (`keep_alive: 0`).
//!
//! `/api/ps` `size` is the model's whole memory (weights + KV cache + compute graph). When the weights blob
//! can be found through the local manifest, `weights_bytes` is the blob size (exact) and `kv_bytes` is the
//! rest (estimate); otherwise `weights_bytes` is the reported total, marked as an estimate.

use crate::env::HostEnv;
use crate::http::{get_json, post_json};
use crate::AdapterError;
use oomtop_core::{Device, LoadedModel, Measured, ModelServer, SourceStatus};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One entry of `/api/ps`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PsModel {
    pub name: String,
    pub model: String,
    pub size: Option<u64>,
    pub size_vram: Option<u64>,
    pub digest: Option<String>,
    pub context_length: Option<u64>,
    pub expires_at: Option<String>,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub quantization_level: Option<String>,
}

/// One entry of `/api/tags` (installed models).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TagModel {
    pub name: String,
    pub size: Option<u64>,
    pub digest: Option<String>,
    pub modified_at: Option<String>,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub quantization_level: Option<String>,
}

fn s(v: &serde_json::Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string)
}

fn details(v: &serde_json::Value, k: &str) -> Option<String> {
    v.get("details").and_then(|d| s(d, k))
}

/// Parses `/api/ps` in full.
pub fn parse_ps(v: &serde_json::Value) -> Vec<PsModel> {
    v.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .map(|m| PsModel {
                    name: s(m, "name").or_else(|| s(m, "model")).unwrap_or_default(),
                    model: s(m, "model").unwrap_or_default(),
                    size: m.get("size").and_then(|x| x.as_u64()),
                    size_vram: m.get("size_vram").and_then(|x| x.as_u64()),
                    digest: s(m, "digest"),
                    context_length: m.get("context_length").and_then(|x| x.as_u64()),
                    expires_at: s(m, "expires_at"),
                    family: details(m, "family"),
                    parameter_size: details(m, "parameter_size"),
                    quantization_level: details(m, "quantization_level"),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn device(size: Option<u64>, vram: Option<u64>) -> Device {
    match (size, vram) {
        (Some(s), Some(v)) if v >= s && s > 0 => Device::Gpu,
        (Some(_), Some(0)) => Device::Cpu,
        (Some(_), Some(_)) => Device::Mixed,
        _ => Device::Unknown,
    }
}

/// Parses an Ollama `/api/ps` response into loaded models (no file lookup; see module docs).
pub fn parse_ollama_ps(v: &serde_json::Value) -> Vec<LoadedModel> {
    parse_ps(v)
        .into_iter()
        .map(|m| LoadedModel {
            weights_bytes: m
                .size
                .map(|s| Measured::estimate(s, "ollama /api/ps size (weights + KV + graph)"))
                .unwrap_or_else(|| Measured::unavailable("ollama /api/ps", "size missing")),
            kv_bytes: Measured::unavailable("ollama /api/ps", "included in size"),
            device: device(m.size, m.size_vram),
            name: m.name,
            file: None,
        })
        .collect()
}

/// Parses `/api/tags` (installed models).
pub fn parse_ollama_tags(v: &serde_json::Value) -> Vec<TagModel> {
    v.get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .map(|m| TagModel {
                    name: s(m, "name").or_else(|| s(m, "model")).unwrap_or_default(),
                    size: m.get("size").and_then(|x| x.as_u64()),
                    digest: s(m, "digest"),
                    modified_at: s(m, "modified_at"),
                    family: details(m, "family"),
                    parameter_size: details(m, "parameter_size"),
                    quantization_level: details(m, "quantization_level"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Manifest path of a model name (`llama3:8b`, `user/model`, `hf.co/org/repo:Q4_K_M`) under a models dir.
pub fn manifest_path(models_dir: &Path, name: &str) -> Option<PathBuf> {
    let (repo, tag) = match name.rsplit_once(':') {
        Some((r, t)) if !t.contains('/') => (r, t),
        _ => (name, "latest"),
    };
    let parts: Vec<&str> = repo.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() || parts.iter().any(|p| *p == ".." || *p == ".") || tag.contains("..") {
        return None;
    }
    let (registry, rest): (&str, Vec<&str>) = match parts.len() {
        1 => ("registry.ollama.ai", vec!["library", parts[0]]),
        2 => ("registry.ollama.ai", parts.clone()),
        _ => (parts[0], parts[1..].to_vec()),
    };
    let mut p = models_dir.join("manifests").join(registry);
    for r in rest {
        p = p.join(r);
    }
    Some(p.join(tag))
}

/// Digest (`sha256:…`) of the weights layer in a manifest.
pub fn manifest_model_digest(manifest: &serde_json::Value) -> Option<String> {
    manifest
        .get("layers")?
        .as_array()?
        .iter()
        .find(|l| l.get("mediaType").and_then(|m| m.as_str()) == Some("application/vnd.ollama.image.model"))
        .and_then(|l| s(l, "digest"))
}

/// Blob file of a digest (`sha256:abc` → `blobs/sha256-abc`).
pub fn blob_path(models_dir: &Path, digest: &str) -> Option<PathBuf> {
    let hex = digest
        .strip_prefix("sha256:")
        .or_else(|| digest.strip_prefix("sha256-"))?;
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| models_dir.join("blobs").join(format!("sha256-{hex}")))
}

/// The weights blob of an installed model, searching the candidate model stores.
pub fn model_blob(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    for d in dirs {
        let Some(mp) = manifest_path(d, name) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&mp) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some(b) = manifest_model_digest(&v).and_then(|dg| blob_path(d, &dg)) {
            if b.is_file() {
                return Some(b);
            }
        }
    }
    None
}

pub(crate) fn enrich(
    ms: &mut ModelServer,
    ep: &str,
    timeout: Duration,
    env: &HostEnv,
) -> Result<(), AdapterError> {
    let v = get_json(&format!("{ep}/api/ps"), timeout)?;
    let dirs = env.ollama_dirs();
    ms.models = parse_ps(&v)
        .into_iter()
        .map(|m| {
            let blob = model_blob(&dirs, &m.name);
            let blob_size = blob
                .as_ref()
                .and_then(|b| std::fs::metadata(b).ok())
                .map(|md| md.len());
            let (weights, kv) = match (blob_size, m.size) {
                (Some(w), Some(total)) => (
                    Measured::exact(w, "ollama weights blob size"),
                    Measured::estimate(
                        total.saturating_sub(w),
                        "ollama /api/ps size − weights (KV + graph)",
                    ),
                ),
                (Some(w), None) => (
                    Measured::exact(w, "ollama weights blob size"),
                    Measured::unavailable("ollama /api/ps", "size missing"),
                ),
                (None, Some(total)) => (
                    Measured::estimate(total, "ollama /api/ps size (weights + KV + graph)"),
                    Measured::unavailable("ollama /api/ps", "included in size"),
                ),
                (None, None) => (
                    Measured::unavailable("ollama /api/ps", "size missing"),
                    Measured::unavailable("ollama /api/ps", "size missing"),
                ),
            };
            LoadedModel {
                device: device(m.size, m.size_vram),
                file: blob.map(|b| b.display().to_string()),
                name: m.name,
                weights_bytes: weights,
                kv_bytes: kv,
            }
        })
        .collect();
    ms.queue = Measured::unavailable("ollama", "not reported");
    ms.status = SourceStatus::Available;
    Ok(())
}

/// Installed models from a running server (`/api/tags`).
pub fn installed(ep: &str, timeout: Duration) -> Result<Vec<TagModel>, AdapterError> {
    Ok(parse_ollama_tags(&get_json(&format!("{ep}/api/tags"), timeout)?))
}

/// Unloads a model without stopping the server.
pub(crate) fn unload(endpoint: &str, model: &str, timeout: Duration) -> Result<(), AdapterError> {
    if model.is_empty() {
        return Err(AdapterError::Decode("empty model name".into()));
    }
    post_json(
        &format!("{endpoint}/api/generate"),
        &serde_json::json!({ "model": model, "keep_alive": 0 }),
        timeout,
    )
    .map(|_| ())
}

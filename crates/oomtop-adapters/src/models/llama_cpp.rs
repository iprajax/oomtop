//! llama.cpp `llama-server` (and llamafile): `/props` (model path, context, slots), `/slots` (busy slots;
//! disabled with `--no-slots`), `/metrics` (only with `--metrics`). Endpoints needing an API key answer 401;
//! oomtop never reads or sends keys.

use super::{arg_value, basename, file_size};
use crate::http::get;
use crate::prober::Caches;
use crate::prom::Metrics;
use crate::AdapterError;
use oomtop_core::model_estimate::{estimate_llm, kv_type_bytes, KvParams};
use oomtop_core::{Device, LoadedModel, Measured, ModelServer, Process, SourceStatus};
use std::time::Duration;

/// Fields of `/props` that matter here.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Props {
    pub model_path: Option<String>,
    /// Per-slot context (`default_generation_settings.n_ctx`).
    pub n_ctx: Option<u64>,
    pub total_slots: Option<u64>,
}

pub fn parse_props(v: &serde_json::Value) -> Props {
    let dgs = v.get("default_generation_settings");
    Props {
        model_path: v
            .get("model_path")
            .and_then(|x| x.as_str())
            .or_else(|| dgs.and_then(|d| d.get("model")).and_then(|x| x.as_str()))
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        n_ctx: dgs
            .and_then(|d| {
                d.get("n_ctx")
                    .or_else(|| d.get("params").and_then(|p| p.get("n_ctx")))
            })
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("n_ctx").and_then(|x| x.as_u64())),
        total_slots: v.get("total_slots").and_then(|x| x.as_u64()),
    }
}

/// `(processing, total)` slots from `/slots` (`is_processing`, or the older numeric `state` ≠ 0).
pub fn parse_slots(v: &serde_json::Value) -> Option<(u32, u32)> {
    let arr = v.as_array()?;
    let busy = arr
        .iter()
        .filter(|s| {
            s.get("is_processing")
                .and_then(|x| x.as_bool())
                .or_else(|| s.get("state").and_then(|x| x.as_u64()).map(|st| st != 0))
                .unwrap_or(false)
        })
        .count();
    Some((busy as u32, arr.len() as u32))
}

fn kv_bytes_per_elem(p: &Process) -> f64 {
    let k = arg_value(&p.cmdline, &["--cache-type-k", "-ctk"])
        .and_then(kv_type_bytes)
        .unwrap_or(2.0);
    let v = arg_value(&p.cmdline, &["--cache-type-v", "-ctv"])
        .and_then(kv_type_bytes)
        .unwrap_or(2.0);
    (k + v) / 2.0
}

/// `-ngl` → where the weights live.
pub fn device_from_argv(argv: &[String]) -> Device {
    match arg_value(argv, &["-ngl", "--n-gpu-layers", "--gpu-layers"]).and_then(|v| v.parse::<i64>().ok()) {
        Some(0) => Device::Cpu,
        Some(n) if !(0..99).contains(&n) => Device::Gpu,
        Some(_) => Device::Mixed,
        None => Device::Unknown,
    }
}

/// Total context the server allocated KV for: `-c` when given, else per-slot `n_ctx` × slots.
fn total_ctx(p: &Process, props: &Props) -> Option<u64> {
    arg_value(&p.cmdline, &["-c", "--ctx-size"])
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&c| c > 0)
        .or_else(|| props.n_ctx.map(|c| c * props.total_slots.unwrap_or(1).max(1)))
}

/// KV-cache bytes from the GGUF header (cached per file, context and KV type).
pub(crate) fn kv_estimate(caches: &mut Caches, path: &str, ctx: u64, per_elem: f64) -> Option<u64> {
    let key = (path.to_string(), ctx, (per_elem * 1e4) as u64);
    if let Some(v) = caches.kv.get(&key) {
        return *v;
    }
    let est = crate::model_files::read_gguf_meta(std::path::Path::new(path))
        .ok()
        .map(|(size, meta)| {
            estimate_llm(
                size,
                meta.as_ref(),
                &KvParams {
                    ctx: Some(ctx),
                    kv_type_bytes: per_elem,
                    n_parallel: 1,
                },
            )
        });
    let kv = est.filter(|e| !e.fallback).map(|e| e.kv);
    caches.kv.insert(key, kv);
    kv
}

/// Generation speed since the previous probe: Δ`tokens_predicted_total` / Δ`tokens_predicted_seconds_total`
/// (tokens per second of generation time; 0 when nothing was generated). `None` on the first sample or
/// after a counter reset, when the caller falls back to the lifetime average.
fn current_tok_s(m: &Metrics, p: &Process, caches: &mut Caches) -> Option<Measured<f64>> {
    let tokens = m.sum("llamacpp:tokens_predicted_total")?;
    let secs = m.sum("llamacpp:tokens_predicted_seconds_total")?;
    let prev = caches.llama_prev.insert(p.id, (tokens, secs));
    let (t0, s0) = prev?;
    let (dt, ds) = (tokens - t0, secs - s0);
    if dt < 0.0 || ds < 0.0 {
        return None; // server restarted under the same pid
    }
    Some(if dt == 0.0 {
        Measured::estimate(0.0, "llama.cpp /metrics: nothing generated since last sample")
    } else if ds > 0.0 {
        Measured::estimate(
            dt / ds,
            "Δ llamacpp:tokens_predicted_total / Δ generation seconds",
        )
    } else {
        return None;
    })
}

pub(crate) fn enrich(
    ms: &mut ModelServer,
    p: &Process,
    ep: &str,
    timeout: Duration,
    caches: &mut Caches,
) -> Result<(), AdapterError> {
    let props_r = get(&format!("{ep}/props"), timeout)?;
    if props_r.status == 401 || props_r.status == 403 {
        ms.status = SourceStatus::Partial("API key required; oomtop sends none".into());
        return Ok(());
    }
    let props = parse_props(&props_r.json()?);
    let mut missing: Vec<&str> = Vec::new();

    if let Some(path) = &props.model_path {
        let ctx = total_ctx(p, &props);
        let kv = ctx.and_then(|c| kv_estimate(caches, path, c, kv_bytes_per_elem(p)));
        let model = LoadedModel {
            name: basename(path).to_string(),
            file: Some(path.clone()),
            weights_bytes: file_size(path, true),
            kv_bytes: match (kv, ctx) {
                (Some(b), Some(c)) => Measured::estimate(b, format!("GGUF header × ctx {c}")),
                (None, Some(_)) => Measured::unavailable("llama.cpp", "GGUF header unreadable"),
                _ => Measured::unavailable("llama.cpp", "context unknown"),
            },
            device: device_from_argv(&p.cmdline),
        };
        // Replace the argv entry for the same file, keep other argv files (draft model, mmproj).
        ms.models.retain(|m| {
            m.file.as_deref() != Some(path.as_str())
                && m.file.as_deref().map(basename) != Some(basename(path))
        });
        ms.models.insert(0, model);
    }

    match get(&format!("{ep}/slots"), timeout) {
        Ok(r) if r.is_success() => match r.json().ok().as_ref().and_then(parse_slots) {
            Some((busy, total)) => {
                ms.busy = Measured::exact(busy > 0, format!("llama.cpp /slots ({busy}/{total} processing)"));
            }
            None => missing.push("/slots"),
        },
        _ => missing.push("/slots"),
    }

    match get(&format!("{ep}/metrics"), timeout) {
        Ok(r) if r.is_success() => {
            let m = Metrics::parse(&r.body);
            ms.tok_s = current_tok_s(&m, p, caches)
                .or_else(|| {
                    m.sum("llamacpp:predicted_tokens_seconds").map(|t| {
                        Measured::estimate(t, "llama.cpp /metrics predicted_tokens_seconds (average)")
                    })
                })
                .unwrap_or_else(|| Measured::unavailable("llama.cpp /metrics", "not reported"));
            if let Some(q) = m.sum("llamacpp:requests_deferred") {
                ms.queue = Measured::exact(q.max(0.0) as u32, "llama.cpp /metrics requests_deferred");
            }
            if ms.busy.value.is_none() {
                if let Some(n) = m.sum("llamacpp:requests_processing") {
                    ms.busy = Measured::exact(n > 0.0, "llama.cpp /metrics requests_processing");
                }
            }
        }
        _ => {
            missing.push("/metrics (start with --metrics)");
            ms.queue = Measured::unavailable("llama.cpp", "start llama-server with --metrics");
        }
    }
    ms.status = if missing.is_empty() {
        SourceStatus::Available
    } else {
        SourceStatus::Partial(format!("no {}", missing.join(", ")))
    };
    Ok(())
}

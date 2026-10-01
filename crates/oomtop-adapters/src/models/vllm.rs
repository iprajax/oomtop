//! vLLM: Prometheus `/metrics` (running / waiting requests, KV-cache usage, generation throughput) and
//! `/v1/models` for model names when metrics carry none.

use super::{argv_model_name, argv_model_source, source_weights};
use crate::env::HostEnv;
use crate::http::get;
use crate::prober::Caches;
use crate::prom::Metrics;
use crate::AdapterError;
use oomtop_core::{Device, LoadedModel, Measured, ModelServer, Process, SourceStatus};
use std::time::{Duration, Instant};

/// Values read from one metrics page.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VllmStats {
    pub running: Option<f64>,
    pub waiting: Option<f64>,
    /// 0..1.
    pub kv_cache_usage: Option<f64>,
    /// Cumulative generated tokens (counter).
    pub generation_tokens_total: Option<f64>,
    /// Older gauge (vLLM v0).
    pub avg_generation_tps: Option<f64>,
    pub models: Vec<String>,
}

pub fn parse_metrics(text: &str) -> VllmStats {
    let m = Metrics::parse(text);
    VllmStats {
        running: m.sum("vllm:num_requests_running"),
        waiting: m.sum("vllm:num_requests_waiting"),
        kv_cache_usage: m
            .max("vllm:kv_cache_usage_perc")
            .or_else(|| m.max("vllm:gpu_cache_usage_perc")),
        generation_tokens_total: m.sum_any(&["vllm:generation_tokens_total", "vllm:generation_tokens"]),
        avg_generation_tps: m.sum("vllm:avg_generation_throughput_toks_per_s"),
        models: m.label_values(
            &[
                "vllm:num_requests_running",
                "vllm:num_requests_waiting",
                "vllm:generation_tokens_total",
                "vllm:kv_cache_usage_perc",
                "vllm:gpu_cache_usage_perc",
            ],
            "model_name",
        ),
    }
}

/// Model ids from an OpenAI-style `/v1/models` response.
pub fn parse_models(v: &serde_json::Value) -> Vec<String> {
    v.get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|x| x.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn enrich(
    ms: &mut ModelServer,
    p: &Process,
    ep: &str,
    timeout: Duration,
    caches: &mut Caches,
    env: &HostEnv,
) -> Result<(), AdapterError> {
    let r = get(&format!("{ep}/metrics"), timeout)?;
    if !r.is_success() {
        return Err(AdapterError::Status(r.status));
    }
    let st = parse_metrics(&r.body);
    let now = Instant::now();
    if let Some(total) = st.generation_tokens_total {
        if let Some((t0, v0)) = caches.vllm_prev.get(&p.id) {
            let dt = now.duration_since(*t0).as_secs_f64();
            if dt > 0.2 && total >= *v0 {
                ms.tok_s = Measured::estimate((total - v0) / dt, "Δ vllm:generation_tokens_total");
            }
        }
        caches.vllm_prev.insert(p.id, (now, total));
    }
    if ms.tok_s.value.is_none() {
        if let Some(t) = st.avg_generation_tps {
            ms.tok_s = Measured::estimate(t, "vllm:avg_generation_throughput_toks_per_s");
        }
    }
    if let Some(r) = st.running {
        ms.busy = Measured::exact(r > 0.0, "vllm:num_requests_running");
    }
    if let Some(w) = st.waiting {
        ms.queue = Measured::exact(w.max(0.0) as u32, "vllm:num_requests_waiting");
    }
    let mut names = st.models.clone();
    if names.is_empty() {
        if let Ok(v) = get(&format!("{ep}/v1/models"), timeout).and_then(|r| r.json()) {
            names = parse_models(&v);
        }
    }
    if names.is_empty() {
        names.extend(argv_model_name(&p.cmdline));
    }
    let kv_note = st
        .kv_cache_usage
        .map(|u| format!("KV cache {:.0}% used (vLLM preallocates KV)", u * 100.0))
        .unwrap_or_else(|| "not reported".into());
    // vLLM serves one model per process: its weights are the on-disk size of the `--model` source (local
    // dir or HF cache). An estimate of what the GPU holds (the server may cast or quantize on load).
    let single = names.len() == 1;
    let source = argv_model_source(&p.cmdline);
    ms.models = names
        .into_iter()
        .map(|name| {
            let (weights, file) = match (&source, single) {
                (Some(src), true) => {
                    let (w, f) = source_weights(src, p, env);
                    (w.into_estimate(), f)
                }
                _ => (Measured::unavailable("vllm", "not reported"), None),
            };
            LoadedModel {
                name,
                file,
                weights_bytes: weights,
                kv_bytes: Measured::unavailable("vllm /metrics", kv_note.clone()),
                device: Device::Gpu,
            }
        })
        .collect();
    ms.status = SourceStatus::Available;
    Ok(())
}

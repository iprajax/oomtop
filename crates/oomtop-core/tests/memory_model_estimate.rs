//! Golden tests for model-load estimates (SPEC §8.2): GGUF header parsing, KV formula, safetensors,
//! diffusion component sums, and the 1.2× fallback.
//!
//! Set `OOMTOP_TEST_GGUF=/path/a.gguf:/path/b.gguf` to also parse real files (first 32 MiB only).

mod memory_common;

use memory_common::*;
use oomtop_core::model_estimate::*;
use oomtop_core::units::{format_bytes_short, GIB};
use serde_json::json;
use std::io::Read;

fn settings() -> insta::Settings {
    let mut s = insta::Settings::clone_current();
    s.set_snapshot_path("memory_snapshots");
    s.set_prepend_module_to_snapshot(false);
    s
}

fn summary(e: &ModelEstimate) -> serde_json::Value {
    json!({
        "weights": format_bytes_short(e.weights),
        "kv": format_bytes_short(e.kv),
        "overhead": format_bytes_short(e.overhead),
        "need": format_bytes_short(e.need),
        "need_bytes": e.need,
        "fallback": e.fallback,
        "ctx": e.ctx,
        "note": e.note,
    })
}

#[test]
fn estimates_golden() {
    let meta = parse_gguf_header(&qwen3_8b_gguf()).unwrap();
    assert!(!meta.truncated);
    assert_eq!(meta.name(), Some("Qwen3-VL-8B-Instruct"));
    assert_eq!(meta.file_type(), Some(15));
    assert_eq!(meta.n_layers(), Some(36));
    assert_eq!(meta.n_kv_heads(), Some(8));
    assert_eq!(meta.head_dim(), Some(128));
    assert_eq!(meta.context_length(), Some(262_144));
    // The 5000-entry token_type array is skipped, not kept.
    assert_eq!(
        meta.kv.get("tokenizer.ggml.token_type"),
        Some(&GgufValue::Array { len: 5000 })
    );

    let file = 5_027_784_800;
    let default_ctx = estimate_llm(file, Some(&meta), &KvParams::default());
    // 2 × 36 × 8 × 128 × 8192 × 2 B = 1.125 GiB
    assert_eq!(default_ctx.kv, 9 * GIB / 8);
    let long = estimate_llm(
        file,
        Some(&meta),
        &KvParams {
            ctx: Some(32_768),
            kv_type_bytes: kv_type_bytes("q8_0").unwrap(),
            n_parallel: 4,
        },
    );
    let truncated = parse_gguf_header(&qwen3_8b_gguf()[..200]).unwrap();
    assert!(truncated.truncated);
    let from_truncated = estimate_llm(file, Some(&truncated), &KvParams::default());

    let hf = r#"{"architectures":["LlamaForCausalLM"],"hidden_size":8192,"num_attention_heads":64,"num_hidden_layers":80,"num_key_value_heads":8,"max_position_embeddings":131072}"#;
    let index = r#"{"metadata":{"total_size":141107412992},"weight_map":{"lm_head.weight":"model-00030-of-00030.safetensors"}}"#;
    let weights = parse_safetensors_index(index).unwrap().weights(|_| None).unwrap();
    let llama70b = estimate_safetensors_llm(weights, Some(hf), &KvParams::default());

    let studio = estimate_diffusion(
        &[
            4_604_558_112,
            675_509_688,
            5_027_784_800,
            1_159_029_824,
            679_604_800,
        ],
        448,
        608,
    );
    let studio_1k = estimate_diffusion(
        &[
            4_604_558_112,
            675_509_688,
            5_027_784_800,
            1_159_029_824,
            679_604_800,
        ],
        1024,
        1024,
    );

    settings().bind(|| {
        insta::assert_yaml_snapshot!(
            "model_estimates",
            json!({
                "qwen3_8b_q4km_default_ctx": summary(&default_ctx),
                "qwen3_8b_q4km_32k_q8kv_x4": summary(&long),
                "gguf_truncated_before_dims": summary(&from_truncated),
                "llama_70b_bf16_safetensors": summary(&llama70b),
                "unknown_12g_blob": summary(&estimate_unknown(12 * GIB)),
                "qwen_image_studio_448x608": summary(&studio),
                "qwen_image_studio_1024x1024": summary(&studio_1k),
            })
        )
    });
}

#[test]
fn real_gguf_files_when_provided() {
    let Ok(paths) = std::env::var("OOMTOP_TEST_GGUF") else {
        return;
    };
    for p in paths.split(':').filter(|p| !p.is_empty()) {
        let f = std::fs::File::open(p).unwrap_or_else(|e| panic!("{p}: {e}"));
        let size = f.metadata().unwrap().len();
        let mut buf = Vec::new();
        f.take(32 * 1024 * 1024).read_to_end(&mut buf).unwrap();
        let meta = parse_gguf_header(&buf).unwrap_or_else(|e| panic!("{p}: {e}"));
        let e = estimate_llm(size, Some(&meta), &KvParams::default());
        eprintln!(
            "{p}: v{} arch={:?} layers={:?} kv_heads={:?} head_dim={:?} ctx={:?} truncated={} → weights {} kv {} overhead {} need {} fallback={}",
            meta.version,
            meta.architecture(),
            meta.n_layers(),
            meta.n_kv_heads(),
            meta.head_dim(),
            meta.context_length(),
            meta.truncated,
            format_bytes_short(e.weights),
            format_bytes_short(e.kv),
            format_bytes_short(e.overhead),
            format_bytes_short(e.need),
            e.fallback
        );
    }
}

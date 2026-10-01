//! `oomtop models` (SPEC §10 "Model files on disk"): the weight files in the well-known model stores
//! (Hugging Face cache, Ollama, LM Studio, llama.cpp cache) plus `models.folders`, with size, last use,
//! duplicates and which running server or process has each one loaded. Reads directory entries and, for
//! same-size duplicate candidates only, the first and last MiB of a file — never whole weights.

use super::{json_pretty, out, Ctx};
use anyhow::Result;
use oomtop_adapters::model_index::{mark_loaded, scan, IndexOptions, ModelFormat, ModelIndex};
use oomtop_adapters::HostEnv;
use oomtop_core::provider::SnapshotProvider;
use oomtop_core::units::format_duration;

/// Builds the index for this machine (default stores + `models.folders`).
pub fn build_index(folders: &[String], hash_duplicates: bool) -> ModelIndex {
    let env = HostEnv::from_env();
    let mut opts = IndexOptions::new(&env, folders);
    opts.hash_duplicates = hash_duplicates;
    scan(&opts, &env)
}

/// The index as the TUI's "On disk" rows.
pub fn disk_models(idx: &ModelIndex) -> Vec<oomtop_tui::DiskModel> {
    idx.files
        .iter()
        .map(|f| oomtop_tui::DiskModel {
            name: f.name.clone(),
            path: f.path.clone(),
            real_path: f.real_path.clone(),
            size: f.size,
            format: format_word(f.format).to_string(),
            last_used_ms: f.accessed_ms.or(f.modified_ms),
            duplicate: f.duplicate_set.is_some(),
        })
        .collect()
}

/// `$HOME/…` → `~/…` in every path of the export (the home directory names the user).
fn tilde_paths(idx: &mut ModelIndex) {
    let Some(home) = std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned()) else {
        return;
    };
    if home.is_empty() || home == "/" {
        return;
    }
    let t = |p: &mut String| {
        if let Some(rest) = p.strip_prefix(&home) {
            *p = format!("~{rest}");
        }
    };
    idx.folders.iter_mut().for_each(t);
    for f in &mut idx.files {
        t(&mut f.path);
        t(&mut f.real_path);
    }
    for d in &mut idx.duplicates {
        d.paths.iter_mut().for_each(t);
    }
}

fn format_word(f: ModelFormat) -> &'static str {
    match f {
        ModelFormat::Gguf => "gguf",
        ModelFormat::Safetensors => "safetensors",
        ModelFormat::OllamaBlob => "ollama",
        ModelFormat::Pytorch => "pytorch",
        ModelFormat::Onnx => "onnx",
        ModelFormat::Numpy => "numpy",
        ModelFormat::Other => "other",
    }
}

pub fn run(ctx: &Ctx, json: bool, no_hash: bool) -> Result<i32> {
    let mut idx = build_index(&ctx.config().models.folders, !no_hash);
    // Which files are loaded right now (model servers' own reports + mapped/open weight files).
    if !ctx.replaying() {
        let mut e = ctx.engine(false)?;
        let s = e.snapshot();
        mark_loaded(&mut idx, &s);
    }
    if json {
        tilde_paths(&mut idx);
        out(&json_pretty(&idx)?);
        return Ok(0);
    }
    if idx.files.is_empty() {
        out(&format!(
            "No model files found in {}.\n(add folders with `oomtop config set models.folders '[\"~/models\"]'`)\n",
            if idx.folders.is_empty() {
                "the default stores".to_string()
            } else {
                idx.folders.join(", ")
            }
        ));
        return Ok(0);
    }
    let now_ms = oomtop_collect::now_ms();
    out(&format!(
        "{} model files, {} on disk{}\n",
        idx.files.len(),
        ctx.fmt(idx.total_bytes),
        if idx.duplicate_bytes > 0 {
            format!(" · duplicates waste {}", ctx.fmt(idx.duplicate_bytes))
        } else {
            String::new()
        }
    ));
    for f in &idx.files {
        let state = if !f.loaded_by.is_empty() {
            format!("loaded by {}", f.loaded_by.join(", "))
        } else {
            match f.accessed_ms.or(f.modified_ms) {
                Some(t) if t <= now_ms => {
                    format!("used {} ago", format_duration((now_ms - t) / 1000))
                }
                _ => String::new(),
            }
        };
        let dup = f
            .duplicate_set
            .map(|i| format!(" · duplicate #{}", i + 1))
            .unwrap_or_default();
        out(&format!(
            "  {:>10}  {:<11}  {}  {}{}\n",
            ctx.fmt(f.size),
            format_word(f.format),
            f.name,
            state,
            dup
        ));
    }
    for (i, d) in idx.duplicates.iter().enumerate() {
        out(&format!(
            "duplicate #{}: {} copies of {} — keeping one frees {}\n",
            i + 1,
            d.paths.len(),
            ctx.fmt(d.size),
            ctx.fmt(d.reclaimable)
        ));
    }
    if idx.truncated {
        out("note: scan stopped early (entry or time budget); results are partial\n");
    }
    for e in &idx.errors {
        out(&format!("note: {e}\n"));
    }
    Ok(0)
}

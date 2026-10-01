//! Model files on disk (SPEC §8.2, §10): load estimates from headers only — never the weights.
//! `.gguf` (and extension-less Ollama blobs with GGUF magic) → header parse (first 32 MiB) + KV estimate;
//! model directories → Σ weight file sizes with dims from `config.json` (HF / MLX layout) or the 1.2×
//! fallback; a directory holding exactly one GGUF uses that file's header.

use crate::AdapterError;
use oomtop_core::model_estimate::{
    estimate_llm, estimate_safetensors_llm, estimate_unknown, parse_gguf_header, GgufMeta, KvParams,
    ModelEstimate,
};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Header bytes read from a GGUF file.
pub const GGUF_HEADER_BYTES: u64 = 32 * 1024 * 1024;

fn io(p: &Path, e: std::io::Error) -> AdapterError {
    AdapterError::Io(format!("{}: {e}", p.display()))
}

fn is_gguf(path: &Path) -> bool {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("gguf")) {
        return true;
    }
    // Ollama blobs are GGUF without an extension; sniff the magic.
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map(|_| &magic == b"GGUF")
        .unwrap_or(false)
}

/// File size plus the parsed GGUF header (`None` when the file is not GGUF or the header is unusable).
pub fn read_gguf_meta(path: &Path) -> Result<(u64, Option<GgufMeta>), AdapterError> {
    let meta = std::fs::metadata(path).map_err(|e| io(path, e))?;
    if !meta.is_file() {
        return Err(AdapterError::Decode(format!("{}: not a file", path.display())));
    }
    if !is_gguf(path) {
        return Ok((meta.len(), None));
    }
    let mut f = std::fs::File::open(path).map_err(|e| io(path, e))?;
    let mut buf = Vec::new();
    f.by_ref()
        .take(GGUF_HEADER_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| io(path, e))?;
    Ok((meta.len(), parse_gguf_header(&buf).ok()))
}

fn weight_files_in(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, u64)>) {
    if depth > 6 || out.len() > 4096 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if md.is_dir() {
            weight_files_in(&p, depth + 1, out);
        } else if md.is_file()
            && p.extension().is_some_and(|x| {
                let x = x.to_ascii_lowercase();
                x == "safetensors"
                    || x == "gguf"
                    || x == "bin"
                    || x == "pt"
                    || x == "pth"
                    || x == "ckpt"
                    || x == "npz"
            })
        {
            out.push((p, md.len()));
        }
    }
}

/// Estimates what loading `path` (a file or a model directory) needs.
pub fn estimate_model_file(path: &Path, kv: &KvParams) -> Result<ModelEstimate, AdapterError> {
    let meta = std::fs::metadata(path).map_err(|e| io(path, e))?;
    if meta.is_dir() {
        let mut files = Vec::new();
        weight_files_in(path, 0, &mut files);
        let ggufs: Vec<&(PathBuf, u64)> = files
            .iter()
            .filter(|(p, _)| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("gguf")))
            .collect();
        if ggufs.len() == 1 {
            return estimate_model_file(&ggufs[0].0, kv);
        }
        let total: u64 = files.iter().map(|(_, s)| s).sum();
        if total == 0 {
            return Err(AdapterError::Decode(format!(
                "{}: no weight files",
                path.display()
            )));
        }
        // HF / MLX layout: dims from config.json when present (small file, read whole).
        let config = path.join("config.json");
        let cfg_text = std::fs::metadata(&config)
            .ok()
            .filter(|m| m.is_file() && m.len() <= 1024 * 1024)
            .and_then(|_| std::fs::read_to_string(&config).ok());
        return Ok(match cfg_text {
            Some(c) => estimate_safetensors_llm(total, Some(&c), kv),
            None => estimate_unknown(total),
        });
    }
    let (size, gguf) = read_gguf_meta(path)?;
    Ok(match gguf {
        Some(m) => estimate_llm(size, Some(&m), kv),
        None => estimate_unknown(size),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal GGUF v3 header with the llama dims the estimator needs.
    pub(crate) fn tiny_gguf() -> Vec<u8> {
        fn kv_u32(out: &mut Vec<u8>, k: &str, v: u32) {
            out.extend_from_slice(&(k.len() as u64).to_le_bytes());
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(&4u32.to_le_bytes()); // UINT32
            out.extend_from_slice(&v.to_le_bytes());
        }
        let mut b = b"GGUF".to_vec();
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes()); // tensors
        b.extend_from_slice(&6u64.to_le_bytes()); // kv count
        let arch = "general.architecture";
        b.extend_from_slice(&(arch.len() as u64).to_le_bytes());
        b.extend_from_slice(arch.as_bytes());
        b.extend_from_slice(&8u32.to_le_bytes()); // STRING
        b.extend_from_slice(&5u64.to_le_bytes());
        b.extend_from_slice(b"llama");
        kv_u32(&mut b, "llama.block_count", 32);
        kv_u32(&mut b, "llama.attention.head_count", 32);
        kv_u32(&mut b, "llama.attention.head_count_kv", 8);
        kv_u32(&mut b, "llama.embedding_length", 4096);
        kv_u32(&mut b, "llama.context_length", 8192);
        b
    }

    #[test]
    fn files_and_dirs() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("m.safetensors");
        std::fs::write(&f, vec![0u8; 1000]).unwrap();
        let e = estimate_model_file(&f, &KvParams::default()).unwrap();
        assert_eq!(e.need, 1200);
        assert!(e.fallback);
        let e = estimate_model_file(d.path(), &KvParams::default()).unwrap();
        assert_eq!(e.weights, 1000);
        let g = d.path().join("bad.gguf");
        std::fs::write(&g, b"not a gguf").unwrap();
        assert!(estimate_model_file(&g, &KvParams::default()).unwrap().fallback);
        assert!(estimate_model_file(&d.path().join("missing.gguf"), &KvParams::default()).is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(estimate_model_file(empty.path(), &KvParams::default()).is_err());
    }

    #[test]
    fn gguf_header_and_blob_sniffing() {
        let d = tempfile::tempdir().unwrap();
        let mut bytes = tiny_gguf();
        bytes.resize(bytes.len() + 4096, 0);
        let g = d.path().join("llama.gguf");
        std::fs::write(&g, &bytes).unwrap();
        let kv = KvParams {
            ctx: Some(4096),
            ..Default::default()
        };
        let e = estimate_model_file(&g, &kv).unwrap();
        assert!(!e.fallback, "{e:?}");
        // 2 × 32 layers × 8 kv heads × 128 dim × 4096 ctx × 2 bytes = 512 MiB
        assert_eq!(e.kv, 512 * 1024 * 1024);
        // Ollama blob: no extension, GGUF magic.
        let blob = d.path().join("sha256-0000");
        std::fs::write(&blob, &bytes).unwrap();
        let (_, m) = read_gguf_meta(&blob).unwrap();
        assert_eq!(m.unwrap().n_layers(), Some(32));
        // A directory with one GGUF uses its header.
        let sub = tempfile::tempdir().unwrap();
        std::fs::write(sub.path().join("only.gguf"), &bytes).unwrap();
        assert!(!estimate_model_file(sub.path(), &kv).unwrap().fallback);
    }
}

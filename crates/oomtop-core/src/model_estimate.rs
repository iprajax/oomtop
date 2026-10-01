//! Model-load estimate (SPEC §8.2): pure parsers for GGUF headers, safetensors headers/indexes and
//! Hugging Face `config.json`, and the estimate formulas.
//!
//! ```text
//! need     = weights + kv + overhead
//! weights  = Σ tensor file sizes (already quantized — no quant multiplier)
//! kv       = 2 × n_layers × n_kv_heads × head_dim × ctx × bytes(kv_type) × n_parallel
//!            (generalized: Σ_layers kv_heads(layer) × (key_length + value_length) × ctx × bytes × n_parallel,
//!             which equals the formula above for uniform layers with key_length = value_length = head_dim)
//! overhead = max(512 MiB, 5 % of weights)          # compute buffers
//! ```
//! Diffusion (sd.cpp): Σ component files (diffusion model, text encoders, VAE, LoRAs) + activations by
//! resolution from [`DIFFUSION_CALIBRATION`]. Without metadata: 1.2 × file size, flagged `fallback`.
//!
//! Callers read only headers (e.g. the first 32 MiB of a GGUF, the 8-byte-prefixed JSON header of a
//! safetensors file, the `*.safetensors.index.json`) and pass file sizes separately; weights are never read.
//! The GGUF parser skips arrays (tokenizer vocabularies) without allocating them, except small integer
//! arrays (per-layer head counts), which are kept.

use crate::units::MIB;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// Integer arrays up to this length are kept (per-layer head counts); longer ones are skipped.
pub const MAX_KEPT_INT_ARRAY: u64 = 4096;
/// Sanity bound on `block_count`: real models have < 1000 layers; a larger value is a corrupt header and
/// must not drive allocations or the KV estimate.
pub const MAX_LAYERS: u64 = 65_536;
/// Default context when neither the caller nor the model says: 4096. A model's own context length is capped
/// at [`DEFAULT_CTX_CAP`] (servers rarely allocate the full training context by default).
pub const DEFAULT_CTX: u64 = 4096;
pub const DEFAULT_CTX_CAP: u64 = 8192;
/// Fallback multiplier without metadata.
pub const FALLBACK_FACTOR: f64 = 1.2;
/// safetensors headers are at most 100 MB by format definition.
pub const SAFETENSORS_MAX_HEADER: u64 = 100_000_000;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum GgufError {
    #[error("not a GGUF file (bad magic)")]
    BadMagic,
    #[error("unsupported GGUF version {0}")]
    Version(u32),
    #[error("header truncated at byte {0}")]
    Truncated(usize),
    #[error("invalid value type {0}")]
    BadType(u32),
    #[error("invalid utf-8 in key or string")]
    Utf8,
}

/// Scalar metadata value (arrays are summarized by length; small integer arrays are kept).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GgufValue {
    Int(i64),
    UInt(u64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// Array of `len` items (contents skipped).
    Array {
        len: u64,
    },
    /// Small integer array (e.g. per-layer `attention.head_count_kv`).
    IntArray(Vec<i64>),
}

impl GgufValue {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            GgufValue::UInt(v) => Some(*v),
            GgufValue::Int(v) if *v >= 0 => Some(*v as u64),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            GgufValue::Str(s) => Some(s),
            _ => None,
        }
    }
    /// Scalar → one-element list; integer array → its values (negative entries rejected).
    pub fn as_u64_list(&self) -> Option<Vec<u64>> {
        match self {
            GgufValue::IntArray(v) => v.iter().map(|x| u64::try_from(*x).ok()).collect(),
            other => other.as_u64().map(|x| vec![x]),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct GgufMeta {
    pub version: u32,
    pub tensor_count: u64,
    pub kv: BTreeMap<String, GgufValue>,
    /// true when the buffer ended before all key/values were read.
    pub truncated: bool,
}

impl GgufMeta {
    pub fn architecture(&self) -> Option<&str> {
        self.kv.get("general.architecture").and_then(|v| v.as_str())
    }
    pub fn name(&self) -> Option<&str> {
        self.kv.get("general.name").and_then(|v| v.as_str())
    }
    /// `general.file_type` (llama.cpp `LLAMA_FTYPE_*`, e.g. 15 = Q4_K_M).
    pub fn file_type(&self) -> Option<u64> {
        self.kv.get("general.file_type").and_then(|v| v.as_u64())
    }
    fn arch_value(&self, suffix: &str) -> Option<&GgufValue> {
        let arch = self.architecture()?;
        self.kv.get(&format!("{arch}.{suffix}"))
    }
    fn arch_u64(&self, suffix: &str) -> Option<u64> {
        self.arch_value(suffix).and_then(|v| v.as_u64())
    }
    /// Max over layers when the value is a per-layer array.
    fn arch_u64_max(&self, suffix: &str) -> Option<u64> {
        self.arch_value(suffix)?.as_u64_list()?.into_iter().max()
    }
    /// `<arch>.block_count` (values above [`MAX_LAYERS`] are treated as corrupt → `None`).
    pub fn n_layers(&self) -> Option<u64> {
        self.arch_u64("block_count").filter(|l| *l <= MAX_LAYERS)
    }
    pub fn n_heads(&self) -> Option<u64> {
        self.arch_u64_max("attention.head_count")
    }
    /// KV heads (max over layers for per-layer arrays); defaults to `n_heads` (no GQA).
    pub fn n_kv_heads(&self) -> Option<u64> {
        self.arch_u64_max("attention.head_count_kv")
            .or_else(|| self.n_heads())
    }
    /// Per-layer KV heads, expanded to `n_layers` entries (`n_layers` ≤ [`MAX_LAYERS`]).
    pub fn kv_heads_per_layer(&self) -> Option<Vec<u64>> {
        let layers = self.n_layers()?;
        let v = self.kv_heads_list()?;
        match v.len() {
            1 => Some(vec![v[0]; layers as usize]),
            n if n as u64 == layers => Some(v),
            _ => None,
        }
    }
    fn kv_heads_list(&self) -> Option<Vec<u64>> {
        self.arch_value("attention.head_count_kv")
            .or_else(|| self.arch_value("attention.head_count"))?
            .as_u64_list()
    }
    /// Σ over layers of KV heads, without expanding a scalar into a per-layer vector.
    pub fn kv_heads_sum(&self) -> Option<u64> {
        let layers = self.n_layers()?;
        let v = self.kv_heads_list()?;
        match v.len() {
            1 => Some(v[0].saturating_mul(layers)),
            n if n as u64 == layers => Some(v.iter().fold(0u64, |a, b| a.saturating_add(*b))),
            _ => None,
        }
    }
    pub fn embedding_length(&self) -> Option<u64> {
        self.arch_u64("embedding_length")
    }
    pub fn context_length(&self) -> Option<u64> {
        self.arch_u64("context_length")
    }
    /// `key_length` if present, else embedding_length / n_heads.
    pub fn head_dim(&self) -> Option<u64> {
        self.arch_u64("attention.key_length").or_else(|| {
            let e = self.embedding_length()?;
            let h = self.n_heads()?;
            (h > 0).then(|| e / h)
        })
    }
    /// `value_length` if present, else [`GgufMeta::head_dim`].
    pub fn value_dim(&self) -> Option<u64> {
        self.arch_u64("attention.value_length")
            .or_else(|| self.head_dim())
    }
    pub fn expert_count(&self) -> Option<u64> {
        self.arch_u64("expert_count")
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], GgufError> {
        let end = self.pos.checked_add(n).ok_or(GgufError::Truncated(self.pos))?;
        if end > self.buf.len() {
            return Err(GgufError::Truncated(self.pos));
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> Result<[u8; N], GgufError> {
        let s = self.take(N)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }
    fn u32(&mut self) -> Result<u32, GgufError> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    fn u64(&mut self) -> Result<u64, GgufError> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    fn len(&mut self) -> Result<usize, GgufError> {
        let at = self.pos;
        usize::try_from(self.u64()?).map_err(|_| GgufError::Truncated(at))
    }
    fn string(&mut self) -> Result<String, GgufError> {
        let len = self.len()?;
        let b = self.take(len)?;
        String::from_utf8(b.to_vec()).map_err(|_| GgufError::Utf8)
    }
    /// String values (names, licenses, chat templates) may hold invalid UTF-8 in the wild; that must not
    /// discard the whole header, so values are decoded lossily (keys stay strict).
    fn string_lossy(&mut self) -> Result<String, GgufError> {
        let len = self.len()?;
        let b = self.take(len)?;
        Ok(String::from_utf8_lossy(b).into_owned())
    }
    fn skip_string(&mut self) -> Result<(), GgufError> {
        let len = self.len()?;
        self.take(len).map(|_| ())
    }
    fn scalar_size(ty: u32) -> Option<usize> {
        match ty {
            0 | 1 | 7 => Some(1),
            2 | 3 => Some(2),
            4..=6 => Some(4),
            10..=12 => Some(8),
            _ => None,
        }
    }
    fn int_scalar(&mut self, ty: u32) -> Result<Option<i64>, GgufError> {
        Ok(match ty {
            0 => Some(self.take(1)?[0] as i64),
            1 => Some(self.take(1)?[0] as i8 as i64),
            2 => Some(u16::from_le_bytes(self.arr()?) as i64),
            3 => Some(i16::from_le_bytes(self.arr()?) as i64),
            4 => Some(self.u32()? as i64),
            5 => Some(self.u32()? as i32 as i64),
            10 => Some(i64::try_from(self.u64()?).unwrap_or(i64::MAX)),
            11 => Some(self.u64()? as i64),
            _ => None,
        })
    }
    fn value(&mut self, ty: u32, depth: u32) -> Result<GgufValue, GgufError> {
        Ok(match ty {
            0 => GgufValue::UInt(self.take(1)?[0] as u64),
            1 => GgufValue::Int(self.take(1)?[0] as i8 as i64),
            2 => GgufValue::UInt(u16::from_le_bytes(self.arr()?) as u64),
            3 => GgufValue::Int(i16::from_le_bytes(self.arr()?) as i64),
            4 => GgufValue::UInt(self.u32()? as u64),
            5 => GgufValue::Int(self.u32()? as i32 as i64),
            6 => GgufValue::Float(f32::from_bits(self.u32()?) as f64),
            7 => GgufValue::Bool(self.take(1)?[0] != 0),
            8 => GgufValue::Str(self.string_lossy()?),
            9 => {
                if depth > 8 {
                    return Err(GgufError::BadType(9));
                }
                let item_ty = self.u32()?;
                let len = self.u64()?;
                let is_int = matches!(item_ty, 0..=5 | 10 | 11);
                if is_int && len <= MAX_KEPT_INT_ARRAY {
                    let mut v = Vec::with_capacity(len as usize);
                    for _ in 0..len {
                        if let Some(x) = self.int_scalar(item_ty)? {
                            v.push(x);
                        }
                    }
                    return Ok(GgufValue::IntArray(v));
                }
                if let Some(sz) = Self::scalar_size(item_ty) {
                    let total = usize::try_from(len)
                        .ok()
                        .and_then(|l| l.checked_mul(sz))
                        .ok_or(GgufError::Truncated(self.pos))?;
                    self.take(total)?;
                } else if item_ty == 8 {
                    for _ in 0..len {
                        self.skip_string()?;
                    }
                } else if item_ty == 9 {
                    // nested arrays: parse and discard
                    for _ in 0..len {
                        self.value(9, depth + 1)?;
                    }
                } else {
                    return Err(GgufError::BadType(item_ty));
                }
                GgufValue::Array { len }
            }
            10 => GgufValue::UInt(self.u64()?),
            11 => GgufValue::Int(self.u64()? as i64),
            12 => GgufValue::Float(f64::from_bits(self.u64()?)),
            other => return Err(GgufError::BadType(other)),
        })
    }
}

/// Parses the GGUF header (magic, version 2/3, tensor count, metadata key/values).
/// A buffer that ends mid-metadata returns what was read with `truncated = true`.
pub fn parse_gguf_header(buf: &[u8]) -> Result<GgufMeta, GgufError> {
    let mut c = Cursor { buf, pos: 0 };
    if c.take(4).map_err(|_| GgufError::BadMagic)? != b"GGUF" {
        return Err(GgufError::BadMagic);
    }
    let version = c.u32()?;
    if !(2..=3).contains(&version) {
        return Err(GgufError::Version(version));
    }
    let tensor_count = c.u64()?;
    let kv_count = c.u64()?;
    let mut meta = GgufMeta {
        version,
        tensor_count,
        ..Default::default()
    };
    for _ in 0..kv_count {
        let key = match c.string() {
            Ok(k) => k,
            Err(GgufError::Truncated(_)) => {
                meta.truncated = true;
                break;
            }
            Err(e) => return Err(e),
        };
        let ty = match c.u32() {
            Ok(t) => t,
            Err(_) => {
                meta.truncated = true;
                break;
            }
        };
        match c.value(ty, 0) {
            Ok(v) => {
                meta.kv.insert(key, v);
            }
            Err(GgufError::Truncated(_)) => {
                meta.truncated = true;
                break;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(meta)
}

// -----------------------------------------------------------------------------------------------------
// safetensors
// -----------------------------------------------------------------------------------------------------

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SafetensorsError {
    /// The buffer holds fewer bytes than the header needs; read `needed` bytes and retry.
    #[error("header truncated: need {needed} bytes")]
    Truncated { needed: u64 },
    #[error("header too large ({0} bytes)")]
    TooLarge(u64),
    #[error("invalid header JSON: {0}")]
    Json(String),
}

/// Summary of a safetensors file header.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetensorsHeader {
    /// JSON header length (the file's data starts at `8 + header_len`).
    pub header_len: u64,
    pub n_tensors: u64,
    /// Σ tensor byte ranges (`data_offsets`).
    pub tensor_bytes: u64,
    /// Tensor count per dtype, e.g. {"BF16": 291}.
    pub dtypes: BTreeMap<String, u64>,
    /// `__metadata__` string map.
    pub metadata: BTreeMap<String, String>,
}

/// Bytes per element for a safetensors dtype.
pub fn safetensors_dtype_bytes(dtype: &str) -> Option<f64> {
    Some(match dtype {
        "F64" | "I64" | "U64" => 8.0,
        "F32" | "I32" | "U32" => 4.0,
        "F16" | "BF16" | "I16" | "U16" => 2.0,
        "F8_E4M3" | "F8_E5M2" | "F8_E8M0" | "I8" | "U8" | "BOOL" => 1.0,
        "F4" | "F6_E2M3" | "F6_E3M2" => 0.5,
        _ => return None,
    })
}

/// Parses the header of a `.safetensors` file (8-byte little-endian length + JSON). Only the header bytes
/// are needed; `SafetensorsError::Truncated` says how many to read.
pub fn parse_safetensors_header(buf: &[u8]) -> Result<SafetensorsHeader, SafetensorsError> {
    if buf.len() < 8 {
        return Err(SafetensorsError::Truncated { needed: 8 });
    }
    let mut lenb = [0u8; 8];
    lenb.copy_from_slice(&buf[..8]);
    let n = u64::from_le_bytes(lenb);
    if n > SAFETENSORS_MAX_HEADER {
        return Err(SafetensorsError::TooLarge(n));
    }
    let end = 8 + n as usize;
    if buf.len() < end {
        return Err(SafetensorsError::Truncated { needed: end as u64 });
    }
    let v: serde_json::Value =
        serde_json::from_slice(&buf[8..end]).map_err(|e| SafetensorsError::Json(e.to_string()))?;
    let obj = v
        .as_object()
        .ok_or_else(|| SafetensorsError::Json("header is not an object".into()))?;
    let mut h = SafetensorsHeader {
        header_len: n,
        ..Default::default()
    };
    for (name, t) in obj {
        if name == "__metadata__" {
            if let Some(m) = t.as_object() {
                for (k, val) in m {
                    if let Some(s) = val.as_str() {
                        h.metadata.insert(k.clone(), s.to_string());
                    }
                }
            }
            continue;
        }
        let dtype = t.get("dtype").and_then(|d| d.as_str()).unwrap_or("?");
        let bytes = match t.get("data_offsets").and_then(|o| o.as_array()).map(|o| {
            (
                o.first().and_then(|x| x.as_u64()),
                o.get(1).and_then(|x| x.as_u64()),
            )
        }) {
            Some((Some(a), Some(b))) if b >= a => b - a,
            _ => {
                // Fall back to dtype × shape.
                // Checked: a malformed shape must not overflow (debug builds panic on overflow).
                let elems: Option<u64> = t.get("shape").and_then(|s| s.as_array()).and_then(|s| {
                    s.iter()
                        .try_fold(1u64, |acc, x| acc.checked_mul(x.as_u64().unwrap_or(0)))
                });
                match (elems, safetensors_dtype_bytes(dtype)) {
                    (Some(e), Some(b)) => (e as f64 * b).ceil() as u64,
                    _ => 0,
                }
            }
        };
        h.n_tensors += 1;
        h.tensor_bytes = h.tensor_bytes.saturating_add(bytes);
        *h.dtypes.entry(dtype.to_string()).or_insert(0) += 1;
    }
    Ok(h)
}

/// `model.safetensors.index.json` summary.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetensorsIndex {
    /// `metadata.total_size` (Σ tensor bytes), when present.
    pub total_size: Option<u64>,
    /// Unique shard file names from `weight_map`, sorted.
    pub shards: Vec<String>,
    pub n_tensors: u64,
}

impl SafetensorsIndex {
    /// Weight bytes: `total_size` if present, else Σ shard sizes from `shard_size` (all must be known).
    pub fn weights(&self, shard_size: impl Fn(&str) -> Option<u64>) -> Option<u64> {
        if let Some(t) = self.total_size.filter(|t| *t > 0) {
            return Some(t);
        }
        if self.shards.is_empty() {
            return None;
        }
        self.shards
            .iter()
            .try_fold(0u64, |acc, s| Some(acc.saturating_add(shard_size(s)?)))
    }
}

/// Parses a sharded safetensors index (`*.safetensors.index.json`).
pub fn parse_safetensors_index(json: &str) -> Result<SafetensorsIndex, SafetensorsError> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| SafetensorsError::Json(e.to_string()))?;
    let total_size = v.get("metadata").and_then(|m| m.get("total_size")).and_then(|t| {
        t.as_u64()
            .or_else(|| t.as_f64().map(|f| f as u64))
            .or_else(|| t.as_str()?.parse().ok())
    });
    let map = v
        .get("weight_map")
        .and_then(|m| m.as_object())
        .ok_or_else(|| SafetensorsError::Json("weight_map missing".into()))?;
    let mut shards: Vec<String> = map
        .values()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect();
    shards.sort();
    shards.dedup();
    Ok(SafetensorsIndex {
        total_size,
        shards,
        n_tensors: map.len() as u64,
    })
}

// -----------------------------------------------------------------------------------------------------
// Dimensions & estimates
// -----------------------------------------------------------------------------------------------------

/// Transformer dimensions needed for the KV cache.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmDims {
    pub n_layers: u64,
    /// Σ over layers of KV heads (= n_layers × n_kv_heads for uniform models).
    pub kv_heads_sum: u64,
    /// Max KV heads of a layer (for display).
    pub n_kv_heads: u64,
    pub key_length: u64,
    pub value_length: u64,
    pub context_length: Option<u64>,
}

impl LlmDims {
    /// From GGUF metadata (`<arch>.block_count`, `attention.head_count[_kv]`, `embedding_length`,
    /// `attention.key_length/value_length`, `context_length`).
    pub fn from_gguf(m: &GgufMeta) -> Option<LlmDims> {
        let n_layers = m.n_layers().filter(|l| *l > 0)?;
        let kv_heads_sum = m.kv_heads_sum()?;
        let key_length = m.head_dim().filter(|d| *d > 0)?;
        let value_length = m.value_dim().filter(|d| *d > 0).unwrap_or(key_length);
        if kv_heads_sum == 0 {
            return None;
        }
        Some(LlmDims {
            n_layers,
            kv_heads_sum,
            n_kv_heads: m.kv_heads_list()?.into_iter().max().unwrap_or(0),
            key_length,
            value_length,
            context_length: m.context_length(),
        })
    }

    /// From a Hugging Face `config.json` (also looks inside `text_config` for multimodal models).
    pub fn from_hf_config(json: &str) -> Option<LlmDims> {
        let root: serde_json::Value = serde_json::from_str(json).ok()?;
        let cfg = root
            .get("text_config")
            .filter(|t| t.get("num_hidden_layers").is_some())
            .unwrap_or(&root);
        let get = |k: &str| cfg.get(k).and_then(|v| v.as_u64());
        let n_layers = get("num_hidden_layers")
            .or_else(|| get("n_layer"))
            .filter(|l| *l > 0 && *l <= MAX_LAYERS)?;
        let heads = get("num_attention_heads")
            .or_else(|| get("n_head"))
            .filter(|h| *h > 0)?;
        let kv = get("num_key_value_heads").unwrap_or(heads);
        let hidden = get("hidden_size").or_else(|| get("n_embd"));
        let head_dim = get("head_dim")
            .or_else(|| hidden.map(|h| h / heads))
            .filter(|d| *d > 0)?;
        Some(LlmDims {
            n_layers,
            kv_heads_sum: n_layers.saturating_mul(kv),
            n_kv_heads: kv,
            key_length: head_dim,
            value_length: head_dim,
            context_length: get("max_position_embeddings").or_else(|| get("n_positions")),
        })
    }

    /// KV cache bytes for `ctx` tokens.
    pub fn kv_bytes(&self, ctx: u64, kv_type_bytes: f64, n_parallel: u64) -> u64 {
        let v = self.kv_heads_sum as f64
            * (self.key_length as f64 + self.value_length as f64)
            * ctx as f64
            * kv_type_bytes.max(0.0)
            * n_parallel.max(1) as f64;
        if v.is_finite() && v < u64::MAX as f64 {
            v as u64
        } else {
            u64::MAX
        }
    }
}

/// Estimate breakdown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelEstimate {
    pub weights: u64,
    pub kv: u64,
    pub overhead: u64,
    pub need: u64,
    /// true when metadata was missing and the 1.2× file-size fallback was used.
    pub fallback: bool,
    pub ctx: Option<u64>,
    pub note: String,
}

/// Parameters for the KV cache estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct KvParams {
    /// Context length; `None` = model's context_length capped at 8192 (4096 if unknown).
    pub ctx: Option<u64>,
    /// Bytes per KV element (f16 = 2.0, q8_0 ≈ 1.0625, q4_0 ≈ 0.5625), see [`kv_type_bytes`].
    pub kv_type_bytes: f64,
    pub n_parallel: u64,
}

impl Default for KvParams {
    fn default() -> Self {
        KvParams {
            ctx: None,
            kv_type_bytes: 2.0,
            n_parallel: 1,
        }
    }
}

/// Bytes per element for a llama.cpp KV cache type (`--cache-type-k/v`, Ollama `OLLAMA_KV_CACHE_TYPE`).
pub fn kv_type_bytes(name: &str) -> Option<f64> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "f32" => 4.0,
        "f16" | "bf16" => 2.0,
        "q8_0" => 34.0 / 32.0,
        "q5_1" => 24.0 / 32.0,
        "q5_0" => 22.0 / 32.0,
        "q4_1" => 20.0 / 32.0,
        "q4_0" | "iq4_nl" => 18.0 / 32.0,
        _ => return None,
    })
}

pub fn overhead_for(weights: u64) -> u64 {
    (512 * MIB).max(weights / 20)
}

fn resolve_ctx(kv: &KvParams, model_ctx: Option<u64>) -> u64 {
    kv.ctx.filter(|c| *c > 0).unwrap_or_else(|| {
        model_ctx
            .filter(|c| *c > 0)
            .unwrap_or(DEFAULT_CTX)
            .min(DEFAULT_CTX_CAP)
    })
}

/// LLM estimate from known dimensions; falls back to 1.2 × weights without them.
pub fn estimate_llm_dims(weights: u64, dims: Option<&LlmDims>, kv: &KvParams) -> ModelEstimate {
    let Some(d) = dims else {
        return estimate_unknown(weights);
    };
    let ctx = resolve_ctx(kv, d.context_length);
    let kv_bytes = d.kv_bytes(ctx, kv.kv_type_bytes, kv.n_parallel);
    let overhead = overhead_for(weights);
    let head = if d.key_length == d.value_length {
        format!("{} dim", d.key_length)
    } else {
        format!("{}+{} dim", d.key_length, d.value_length)
    };
    ModelEstimate {
        weights,
        kv: kv_bytes,
        overhead,
        need: weights.saturating_add(kv_bytes).saturating_add(overhead),
        fallback: false,
        ctx: Some(ctx),
        note: format!(
            "{} layers × {} kv heads × {head} × ctx {ctx}{}",
            d.n_layers,
            d.n_kv_heads,
            if kv.n_parallel > 1 {
                format!(" × {} parallel", kv.n_parallel)
            } else {
                String::new()
            }
        ),
    }
}

/// LLM estimate from a GGUF header; falls back to 1.2 × file size without usable metadata.
pub fn estimate_llm(file_size: u64, meta: Option<&GgufMeta>, kv: &KvParams) -> ModelEstimate {
    let dims = meta.and_then(LlmDims::from_gguf);
    estimate_llm_dims(file_size, dims.as_ref(), kv)
}

/// safetensors LLM: weights from the index/header sum, dims from `config.json` when available.
pub fn estimate_safetensors_llm(weights: u64, hf_config: Option<&str>, kv: &KvParams) -> ModelEstimate {
    let dims = hf_config.and_then(LlmDims::from_hf_config);
    estimate_llm_dims(weights, dims.as_ref(), kv)
}

/// Without metadata: 1.2 × file size, `estimate`.
pub fn estimate_unknown(file_size: u64) -> ModelEstimate {
    let need = (file_size as f64 * FALLBACK_FACTOR) as u64;
    ModelEstimate {
        weights: file_size,
        kv: 0,
        overhead: need.saturating_sub(file_size),
        need,
        fallback: true,
        ctx: None,
        note: "no metadata: 1.2 × file size".into(),
    }
}

/// One calibration point for diffusion activations (sd.cpp compute buffers from the engine log).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiffusionCalibration {
    pub model: &'static str,
    pub width: u32,
    pub height: u32,
    /// Σ compute buffers (diffusion + VAE + text encoder), bytes.
    pub compute_bytes: u64,
}

/// Calibrated on qwen-image-studio (Qwen-Image 2.1 on sd.cpp/Metal, M5 Air, 2026-09-29): a 256×256 job
/// logged compute buffers of 62.95 MiB (diffusion) + 542.32 MiB (VAE) + 15.34 MiB (text encoder).
/// Activations are modelled as linear in pixels (conv/flash-attention activations are), which extrapolates
/// conservatively for larger images.
pub const DIFFUSION_CALIBRATION: &[DiffusionCalibration] = &[DiffusionCalibration {
    model: "qwen-image-2.1 (sd.cpp, Metal)",
    width: 256,
    height: 256,
    compute_bytes: 650_756_751, // (62.95 + 542.32 + 15.34) MiB
}];

/// Activation estimate for a `w × h` job (floor 512 MiB).
pub fn diffusion_activations(width: u32, height: u32) -> u64 {
    let pixels = width as u128 * height as u128;
    let act = DIFFUSION_CALIBRATION
        .iter()
        .filter(|c| c.width > 0 && c.height > 0)
        .map(|c| c.compute_bytes as u128 * pixels / (c.width as u128 * c.height as u128))
        .max()
        .unwrap_or(0);
    u64::try_from(act).unwrap_or(u64::MAX).max(512 * MIB)
}

/// Diffusion (sd.cpp): Σ component files (diffusion model, text encoders, VAE, LoRAs merged at load) +
/// activations for the resolution (SPEC §8.2).
pub fn estimate_diffusion(component_sizes: &[u64], width: u32, height: u32) -> ModelEstimate {
    let weights: u64 = component_sizes.iter().fold(0u64, |a, b| a.saturating_add(*b));
    let act = diffusion_activations(width, height);
    ModelEstimate {
        weights,
        kv: 0,
        overhead: act,
        need: weights.saturating_add(act),
        fallback: false,
        ctx: None,
        note: format!(
            "{} components + activations for {width}×{height} (calibrated: {})",
            component_sizes.len(),
            DIFFUSION_CALIBRATION
                .iter()
                .map(|c| c.model)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::units::GIB;

    fn push_str(b: &mut Vec<u8>, s: &str) {
        b.extend_from_slice(&(s.len() as u64).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
    }
    fn push_kv_u32(b: &mut Vec<u8>, k: &str, v: u32) {
        push_str(b, k);
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(&v.to_le_bytes());
    }

    /// Builds a small synthetic GGUF v3 header.
    pub(crate) fn synthetic_gguf() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&291u64.to_le_bytes()); // tensors
        b.extend_from_slice(&7u64.to_le_bytes()); // kv count
        push_str(&mut b, "general.architecture");
        b.extend_from_slice(&8u32.to_le_bytes());
        push_str(&mut b, "llama");
        push_kv_u32(&mut b, "llama.block_count", 32);
        push_kv_u32(&mut b, "llama.attention.head_count", 32);
        push_kv_u32(&mut b, "llama.attention.head_count_kv", 8);
        push_kv_u32(&mut b, "llama.embedding_length", 4096);
        push_kv_u32(&mut b, "llama.context_length", 131072);
        // tokenizer array of strings
        push_str(&mut b, "tokenizer.ggml.tokens");
        b.extend_from_slice(&9u32.to_le_bytes());
        b.extend_from_slice(&8u32.to_le_bytes());
        b.extend_from_slice(&3u64.to_le_bytes());
        for t in ["<s>", "</s>", "hi"] {
            push_str(&mut b, t);
        }
        b
    }

    #[test]
    fn parses_header() {
        let buf = synthetic_gguf();
        let m = parse_gguf_header(&buf).unwrap();
        assert_eq!(m.version, 3);
        assert_eq!(m.tensor_count, 291);
        assert_eq!(m.architecture(), Some("llama"));
        assert_eq!(m.n_layers(), Some(32));
        assert_eq!(m.n_kv_heads(), Some(8));
        assert_eq!(m.head_dim(), Some(128));
        assert_eq!(m.context_length(), Some(131072));
        assert_eq!(
            m.kv.get("tokenizer.ggml.tokens"),
            Some(&GgufValue::Array { len: 3 })
        );
        assert!(!m.truncated);
    }

    #[test]
    fn gguf_v2_parses_like_v3() {
        let mut buf = synthetic_gguf();
        buf[4..8].copy_from_slice(&2u32.to_le_bytes());
        let m = parse_gguf_header(&buf).unwrap();
        assert_eq!(m.version, 2);
        assert_eq!(m.n_layers(), Some(32));
        // Big-endian files read as a huge little-endian version and are rejected, not misparsed.
        buf[4..8].copy_from_slice(&3u32.to_be_bytes());
        assert_eq!(
            parse_gguf_header(&buf),
            Err(GgufError::Version(3u32.swap_bytes()))
        );
    }

    #[test]
    fn truncated_and_bad_magic() {
        let buf = synthetic_gguf();
        let m = parse_gguf_header(&buf[..60]).unwrap();
        assert!(m.truncated);
        for n in 0..buf.len() {
            // Never panics on any prefix.
            let _ = parse_gguf_header(&buf[..n]);
        }
        assert_eq!(parse_gguf_header(b"GGML...."), Err(GgufError::BadMagic));
        assert_eq!(parse_gguf_header(b""), Err(GgufError::BadMagic));
        let mut v1 = b"GGUF".to_vec();
        v1.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(parse_gguf_header(&v1), Err(GgufError::Version(1)));
    }

    #[test]
    fn llm_estimate() {
        let m = parse_gguf_header(&synthetic_gguf()).unwrap();
        let e = estimate_llm(8 * GIB, Some(&m), &KvParams::default());
        // 2 × 32 × 8 × 128 × 8192 × 2 = 1 GiB
        assert_eq!(e.kv, GIB);
        // 5 % of 8 GiB = 410 MiB < 512 MiB floor
        assert_eq!(e.overhead, 512 * MIB);
        assert_eq!(overhead_for(20 * GIB), GIB);
        assert_eq!(e.need, e.weights + e.kv + e.overhead);
        let f = estimate_llm(1000, None, &KvParams::default());
        assert!(f.fallback);
        assert_eq!(f.need, 1200);
        let q8 = estimate_llm(
            8 * GIB,
            Some(&m),
            &KvParams {
                ctx: Some(4096),
                kv_type_bytes: kv_type_bytes("q8_0").unwrap(),
                n_parallel: 2,
            },
        );
        assert_eq!(q8.kv, (GIB as f64 / 2.0 * 1.0625 / 2.0 * 2.0) as u64);
    }

    #[test]
    fn per_layer_kv_heads() {
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&5u64.to_le_bytes());
        push_str(&mut b, "general.architecture");
        b.extend_from_slice(&8u32.to_le_bytes());
        push_str(&mut b, "hybrid");
        push_kv_u32(&mut b, "hybrid.block_count", 4);
        push_kv_u32(&mut b, "hybrid.attention.head_count", 16);
        push_kv_u32(&mut b, "hybrid.embedding_length", 2048);
        // per-layer kv heads as i32 array: [4, 0, 4, 0] (recurrent layers have no KV)
        push_str(&mut b, "hybrid.attention.head_count_kv");
        b.extend_from_slice(&9u32.to_le_bytes());
        b.extend_from_slice(&5u32.to_le_bytes());
        b.extend_from_slice(&4u64.to_le_bytes());
        for v in [4i32, 0, 4, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        let m = parse_gguf_header(&b).unwrap();
        assert_eq!(m.kv_heads_per_layer(), Some(vec![4, 0, 4, 0]));
        let d = LlmDims::from_gguf(&m).unwrap();
        assert_eq!(d.kv_heads_sum, 8);
        assert_eq!(d.key_length, 128);
        assert_eq!(d.kv_bytes(1000, 2.0, 1), 8 * 256 * 1000 * 2);
    }

    #[test]
    fn safetensors_header_and_index() {
        let json = br#"{"__metadata__":{"format":"pt"},"a":{"dtype":"BF16","shape":[4,8],"data_offsets":[0,64]},"b":{"dtype":"F32","shape":[10],"data_offsets":[64,104]}}"#;
        let mut buf = (json.len() as u64).to_le_bytes().to_vec();
        buf.extend_from_slice(json);
        let h = parse_safetensors_header(&buf).unwrap();
        assert_eq!(h.n_tensors, 2);
        assert_eq!(h.tensor_bytes, 104);
        assert_eq!(h.dtypes.get("BF16"), Some(&1));
        assert_eq!(h.metadata.get("format").map(String::as_str), Some("pt"));
        assert_eq!(
            parse_safetensors_header(&buf[..20]),
            Err(SafetensorsError::Truncated {
                needed: buf.len() as u64
            })
        );
        assert!(parse_safetensors_header(&[1, 2]).is_err());
        let mut huge = u64::MAX.to_le_bytes().to_vec();
        huge.push(b'{');
        assert!(matches!(
            parse_safetensors_header(&huge),
            Err(SafetensorsError::TooLarge(_))
        ));

        let idx = r#"{"metadata":{"total_size":16060522496},"weight_map":{"a":"model-00001-of-00002.safetensors","b":"model-00002-of-00002.safetensors","c":"model-00001-of-00002.safetensors"}}"#;
        let i = parse_safetensors_index(idx).unwrap();
        assert_eq!(i.total_size, Some(16_060_522_496));
        assert_eq!(i.shards.len(), 2);
        assert_eq!(i.n_tensors, 3);
        assert_eq!(i.weights(|_| None), Some(16_060_522_496));
        let no_total = r#"{"weight_map":{"a":"s1","b":"s2"}}"#;
        let i = parse_safetensors_index(no_total).unwrap();
        assert_eq!(i.weights(|s| Some(if s == "s1" { 10 } else { 20 })), Some(30));
        assert_eq!(i.weights(|s| (s == "s1").then_some(10)), None);
        assert!(parse_safetensors_index("{}").is_err());
    }

    #[test]
    fn hf_config_dims() {
        let cfg = r#"{"num_hidden_layers":32,"num_attention_heads":32,"num_key_value_heads":8,"hidden_size":4096,"max_position_embeddings":131072}"#;
        let d = LlmDims::from_hf_config(cfg).unwrap();
        assert_eq!(d.kv_heads_sum, 256);
        assert_eq!(d.key_length, 128);
        let e = estimate_safetensors_llm(16 * GIB, Some(cfg), &KvParams::default());
        assert_eq!(e.kv, GIB);
        assert!(!e.fallback);
        let mm = r#"{"text_config":{"num_hidden_layers":36,"num_attention_heads":32,"num_key_value_heads":8,"head_dim":128}}"#;
        assert_eq!(LlmDims::from_hf_config(mm).unwrap().n_layers, 36);
        assert!(estimate_safetensors_llm(100, Some("not json"), &KvParams::default()).fallback);
    }

    fn header(arch: &str, kvs: &[(&str, u32)], extra: impl FnOnce(&mut Vec<u8>) -> u64) -> Vec<u8> {
        let mut body = Vec::new();
        push_str(&mut body, "general.architecture");
        body.extend_from_slice(&8u32.to_le_bytes());
        push_str(&mut body, arch);
        for (k, v) in kvs {
            push_kv_u32(&mut body, k, *v);
        }
        let n_extra = extra(&mut body);
        let mut b = Vec::new();
        b.extend_from_slice(b"GGUF");
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&(1 + kvs.len() as u64 + n_extra).to_le_bytes());
        b.extend_from_slice(&body);
        b
    }

    #[test]
    fn corrupt_block_count_does_not_allocate_or_overflow() {
        // block_count = u32::MAX used to allocate a 32 GiB per-layer vector.
        let b = header(
            "x",
            &[
                ("x.block_count", u32::MAX),
                ("x.attention.head_count", u32::MAX),
                ("x.attention.head_count_kv", u32::MAX),
                ("x.embedding_length", u32::MAX),
            ],
            |_| 0,
        );
        let m = parse_gguf_header(&b).unwrap();
        assert_eq!(m.n_layers(), None);
        assert_eq!(m.kv_heads_per_layer(), None);
        let e = estimate_llm(GIB, Some(&m), &KvParams::default());
        assert!(e.fallback);
        // Large-but-plausible dims saturate instead of overflowing.
        let b = header(
            "y",
            &[
                ("y.block_count", 60_000),
                ("y.attention.head_count", 1),
                ("y.attention.head_count_kv", u32::MAX),
                ("y.attention.key_length", u32::MAX),
            ],
            |_| 0,
        );
        let m = parse_gguf_header(&b).unwrap();
        let d = LlmDims::from_gguf(&m).unwrap();
        assert_eq!(d.kv_heads_sum, 60_000 * u32::MAX as u64);
        assert_eq!(d.kv_bytes(u64::MAX, 4.0, u64::MAX), u64::MAX);
        let dims = LlmDims {
            n_layers: 1,
            kv_heads_sum: 1,
            n_kv_heads: 1,
            key_length: u64::MAX,
            value_length: u64::MAX,
            context_length: None,
        };
        assert_eq!(dims.kv_bytes(1, 1.0, 1), u64::MAX);
        let hf = r#"{"num_hidden_layers":4000,"num_attention_heads":1,"num_key_value_heads":18446744073709551615,"head_dim":1}"#;
        assert_eq!(LlmDims::from_hf_config(hf).unwrap().kv_heads_sum, u64::MAX);
    }

    #[test]
    fn invalid_utf8_value_keeps_the_header() {
        let b = header("llama", &[("llama.block_count", 2)], |b| {
            push_str(b, "general.name");
            b.extend_from_slice(&8u32.to_le_bytes());
            b.extend_from_slice(&3u64.to_le_bytes());
            b.extend_from_slice(&[b'a', 0xff, b'b']);
            1
        });
        let m = parse_gguf_header(&b).unwrap();
        assert_eq!(m.n_layers(), Some(2));
        assert_eq!(m.name(), Some("a\u{fffd}b"));
    }

    #[test]
    fn malformed_safetensors_never_panics() {
        let json = br#"{"a":{"dtype":"F32","shape":[4294967296,4294967296,4294967296]},"b":{"dtype":"F16","shape":[3,5]},"c":{"dtype":"F32","data_offsets":[10,2]}}"#;
        let mut buf = (json.len() as u64).to_le_bytes().to_vec();
        buf.extend_from_slice(json);
        let h = parse_safetensors_header(&buf).unwrap();
        assert_eq!(h.n_tensors, 3);
        assert_eq!(h.tensor_bytes, 30);
        let idx = SafetensorsIndex {
            total_size: None,
            shards: vec!["a".into(), "b".into()],
            n_tensors: 2,
        };
        assert_eq!(idx.weights(|_| Some(u64::MAX)), Some(u64::MAX));
    }

    #[test]
    fn kv_types() {
        assert_eq!(kv_type_bytes("F16"), Some(2.0));
        assert_eq!(kv_type_bytes("q4_0"), Some(0.5625));
        assert_eq!(kv_type_bytes("q2_k"), None);
    }

    #[test]
    fn diffusion_calibration_point() {
        // The calibration job itself reproduces the logged compute buffers.
        assert_eq!(diffusion_activations(256, 256), 650_756_751);
        // Tiny jobs hit the 512 MiB floor; activations grow with pixels.
        assert_eq!(diffusion_activations(64, 64), 512 * MIB);
        assert!(diffusion_activations(1024, 1024) > diffusion_activations(512, 512));
        let e = estimate_diffusion(&[GIB, 2 * GIB], 256, 256);
        assert_eq!(e.need, 3 * GIB + 650_756_751);
    }
}

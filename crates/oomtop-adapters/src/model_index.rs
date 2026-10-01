//! Model files index (SPEC §10 "Model files on disk"): weight files under `~/.cache/huggingface`,
//! `~/.ollama/models`, the LM Studio models dir, llama.cpp's download cache and user folders, with size,
//! last-used time, duplicates (same size + SHA-256 of the first and last MiB) and which server has each
//! loaded. Only directory entries, `stat` and at most 2 MiB per duplicate candidate are read.

use crate::env::HostEnv;
use oomtop_core::Snapshot;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Bytes hashed at each end of a duplicate candidate.
pub const HASH_WINDOW: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct IndexOptions {
    pub folders: Vec<PathBuf>,
    pub max_depth: usize,
    /// Directory entries visited before the scan stops (`truncated`).
    pub max_entries: usize,
    /// Smaller files are ignored (configs, tokenizers).
    pub min_size: u64,
    pub hash_duplicates: bool,
    /// Wall-clock budget for the whole scan.
    pub budget: Option<Duration>,
}

impl IndexOptions {
    /// Default folders plus user folders (`~` expanded), e.g. config `models.folders`.
    pub fn new(env: &HostEnv, user_folders: &[String]) -> IndexOptions {
        let mut folders = default_folders(env);
        for f in user_folders {
            let p = match f.strip_prefix("~/").or((f == "~").then_some("")) {
                Some(rest) => match &env.home {
                    Some(h) => h.join(rest),
                    None => continue,
                },
                None => PathBuf::from(f),
            };
            if !folders.contains(&p) {
                folders.push(p);
            }
        }
        IndexOptions {
            folders,
            ..IndexOptions::default()
        }
    }
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            folders: Vec::new(),
            max_depth: 12,
            max_entries: 200_000,
            min_size: 8 * 1024 * 1024,
            hash_duplicates: true,
            budget: Some(Duration::from_secs(10)),
        }
    }
}

/// Well-known model stores on this machine (existing or not; missing folders are skipped by the scan).
pub fn default_folders(env: &HostEnv) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    v.extend(env.hf_hub_cache.clone());
    v.extend(env.ollama_dirs());
    v.extend(env.lm_studio_dirs());
    v.extend(env.home_join(if env.is_macos {
        "Library/Caches/llama.cpp"
    } else {
        ".cache/llama.cpp"
    }));
    let mut seen = HashSet::new();
    v.retain(|p| seen.insert(p.clone()));
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelFormat {
    Gguf,
    Safetensors,
    /// Ollama content-addressed blob (GGUF inside).
    OllamaBlob,
    Pytorch,
    Onnx,
    Numpy,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    HuggingFace,
    Ollama,
    LmStudio,
    LlamaCpp,
    #[default]
    Folder,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelFile {
    /// Path as found (HF snapshot symlink, Ollama blob, …).
    pub path: String,
    /// Canonical path (symlinks resolved); identity for duplicates and "loaded".
    pub real_path: String,
    /// Display name: `org/repo/file` (HF), `model:tag` (Ollama), `publisher/repo/file` (LM Studio).
    pub name: String,
    pub size: u64,
    pub format: ModelFormat,
    pub source: ModelSource,
    pub modified_ms: Option<u64>,
    /// Last access (atime; coarse on `relatime`/`noatime` mounts).
    pub accessed_ms: Option<u64>,
    /// Partial hash, computed for duplicate candidates only.
    pub hash: Option<String>,
    /// Model-server ids (or `pid:<n>`) that have this file loaded.
    pub loaded_by: Vec<String>,
    /// Index into [`ModelIndex::duplicates`].
    pub duplicate_set: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DuplicateSet {
    pub size: u64,
    pub hash: String,
    pub paths: Vec<String>,
    /// Bytes freed by keeping one copy.
    pub reclaimable: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ModelIndex {
    pub folders: Vec<String>,
    /// Largest first.
    pub files: Vec<ModelFile>,
    pub duplicates: Vec<DuplicateSet>,
    pub total_bytes: u64,
    pub duplicate_bytes: u64,
    /// The scan hit `max_entries` or the time budget.
    pub truncated: bool,
    pub errors: Vec<String>,
}

fn ms_of(t: std::io::Result<std::time::SystemTime>) -> Option<u64> {
    t.ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

fn format_of(file: &str, in_ollama_blobs: bool) -> Option<ModelFormat> {
    if in_ollama_blobs {
        return file.starts_with("sha256-").then_some(ModelFormat::OllamaBlob);
    }
    let ext = file.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "gguf" | "ggml" => ModelFormat::Gguf,
        "safetensors" => ModelFormat::Safetensors,
        "bin" | "pt" | "pth" | "ckpt" => ModelFormat::Pytorch,
        "onnx" => ModelFormat::Onnx,
        "npz" => ModelFormat::Numpy,
        _ => return None,
    })
}

fn source_of(root: &Path, env: &HostEnv) -> ModelSource {
    let r = root.display().to_string();
    if env.hf_hub_cache.as_deref() == Some(root) || r.contains("huggingface") {
        ModelSource::HuggingFace
    } else if env.ollama_dirs().iter().any(|d| d == root) || r.contains(".ollama") || r.contains("/ollama/") {
        ModelSource::Ollama
    } else if r.contains("lmstudio") || r.contains("lm-studio") {
        ModelSource::LmStudio
    } else if r.contains("llama.cpp") {
        ModelSource::LlamaCpp
    } else {
        ModelSource::Folder
    }
}

/// `org/repo/rel` from `…/models--org--repo/snapshots/<rev>/rel`.
pub fn hf_display_name(path: &Path) -> Option<String> {
    let comps: Vec<&str> = path.iter().filter_map(|c| c.to_str()).collect();
    let i = comps.iter().position(|c| c.starts_with("models--"))?;
    let repo = comps[i].trim_start_matches("models--").replace("--", "/");
    let rel = comps.get(i + 3..).filter(|r| !r.is_empty())?.join("/");
    (comps.get(i + 1) == Some(&"snapshots")).then(|| format!("{repo}/{rel}"))
}

/// Ollama blob file name (`sha256-<hex>`) → `model:tag`, from the manifests under `models_dir`.
pub fn ollama_blob_names(models_dir: &Path) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let root = models_dir.join("manifests");
    let mut stack = vec![(root.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push((p, depth + 1));
                continue;
            }
            let Ok(meta) = e.metadata() else { continue };
            if meta.len() > 256 * 1024 {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let Some(digest) = crate::models::ollama::manifest_model_digest(&v) else {
                continue;
            };
            let Ok(rel) = p.strip_prefix(&root) else { continue };
            let parts: Vec<&str> = rel.iter().filter_map(|c| c.to_str()).collect();
            // <registry>/<namespace…>/<model>/<tag>; the default registry and `library` are implied.
            let Some((tag, repo)) = parts.split_last().filter(|(_, r)| r.len() >= 2) else {
                continue;
            };
            let repo = match repo {
                ["registry.ollama.ai", "library", model] => model.to_string(),
                ["registry.ollama.ai", rest @ ..] => rest.join("/"),
                other => other.join("/"),
            };
            let name = format!("{repo}:{tag}");
            let file = digest.replacen(':', "-", 1);
            out.entry(file).or_insert(name);
        }
    }
    out
}

/// SHA-256 over the size and the first/last [`HASH_WINDOW`] bytes (hex, 32 chars).
pub fn partial_hash(path: &Path, size: u64) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    h.update(size.to_le_bytes());
    let mut buf = vec![0u8; HASH_WINDOW.min(size) as usize];
    f.read_exact(&mut buf)?;
    h.update(&buf);
    if size > HASH_WINDOW {
        let tail = HASH_WINDOW.min(size - HASH_WINDOW);
        f.seek(SeekFrom::Start(size - tail))?;
        let mut t = vec![0u8; tail as usize];
        f.read_exact(&mut t)?;
        h.update(&t);
    }
    Ok(h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn inode(m: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino())
}

#[cfg(not(unix))]
fn inode(_m: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

/// Scans the folders. Missing folders are skipped silently; unreadable ones are reported in `errors`.
pub fn scan(opts: &IndexOptions, env: &HostEnv) -> ModelIndex {
    let start = Instant::now();
    let mut idx = ModelIndex {
        folders: opts.folders.iter().map(|f| f.display().to_string()).collect(),
        ..Default::default()
    };
    let mut seen_inodes: HashSet<(u64, u64)> = HashSet::new();
    let mut entries = 0usize;
    'roots: for root in &opts.folders {
        if !root.is_dir() {
            continue;
        }
        let source = source_of(root, env);
        let blob_names = if source == ModelSource::Ollama {
            ollama_blob_names(root)
        } else {
            HashMap::new()
        };
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let rd = match std::fs::read_dir(&dir) {
                Ok(rd) => rd,
                Err(e) => {
                    idx.errors.push(format!("{}: {e}", dir.display()));
                    continue;
                }
            };
            let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let in_ollama_blobs = source == ModelSource::Ollama && dir_name == "blobs";
            for e in rd.flatten() {
                entries += 1;
                if entries > opts.max_entries || opts.budget.is_some_and(|b| start.elapsed() > b) {
                    idx.truncated = true;
                    break 'roots;
                }
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    let skip = name.starts_with('.')
                        || name == "manifests"
                        || (name == "blobs" && dir_name.starts_with("models--"))
                        || depth + 1 > opts.max_depth;
                    if !skip {
                        stack.push((p, depth + 1));
                    }
                    continue;
                }
                // Files and file symlinks (HF snapshots); directory symlinks are not followed.
                let Some(format) = format_of(&name, in_ollama_blobs) else {
                    continue;
                };
                let Ok(meta) = std::fs::metadata(&p) else { continue };
                if !meta.is_file() || meta.len() < opts.min_size || !seen_inodes.insert(inode(&meta)) {
                    continue;
                }
                let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
                let display = match source {
                    ModelSource::HuggingFace => hf_display_name(&p),
                    ModelSource::Ollama => blob_names.get(&name).cloned(),
                    _ => None,
                }
                .unwrap_or_else(|| {
                    p.strip_prefix(root)
                        .map(|r| r.display().to_string())
                        .unwrap_or_else(|_| name.clone())
                });
                idx.files.push(ModelFile {
                    path: p.display().to_string(),
                    real_path: real.display().to_string(),
                    name: display,
                    size: meta.len(),
                    format,
                    source,
                    modified_ms: ms_of(meta.modified()),
                    accessed_ms: ms_of(meta.accessed()),
                    ..Default::default()
                });
            }
        }
    }
    idx.files
        .sort_by(|a, b| b.size.cmp(&a.size).then(a.path.cmp(&b.path)));
    idx.total_bytes = idx.files.iter().map(|f| f.size).sum();
    if opts.hash_duplicates {
        find_duplicates(&mut idx);
    }
    idx
}

fn find_duplicates(idx: &mut ModelIndex) {
    let mut by_size: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, f) in idx.files.iter().enumerate() {
        by_size.entry(f.size).or_default().push(i);
    }
    let mut by_hash: BTreeMap<(u64, String), Vec<usize>> = BTreeMap::new();
    for (size, is) in by_size.into_iter().filter(|(_, v)| v.len() > 1) {
        for i in is {
            match partial_hash(Path::new(&idx.files[i].real_path), size) {
                Ok(h) => {
                    idx.files[i].hash = Some(h.clone());
                    by_hash.entry((size, h)).or_default().push(i);
                }
                Err(e) => idx.errors.push(format!("{}: {e}", idx.files[i].path)),
            }
        }
    }
    for ((size, hash), is) in by_hash.into_iter().filter(|(_, v)| v.len() > 1) {
        let set = idx.duplicates.len();
        for &i in &is {
            idx.files[i].duplicate_set = Some(set);
        }
        let reclaimable = size * (is.len() as u64 - 1);
        idx.duplicate_bytes += reclaimable;
        idx.duplicates.push(DuplicateSet {
            size,
            hash,
            paths: is.iter().map(|&i| idx.files[i].path.clone()).collect(),
            reclaimable,
        });
    }
    idx.duplicates.sort_by_key(|d| std::cmp::Reverse(d.reclaimable));
    // Keep duplicate_set indices consistent with the sorted order.
    let order: HashMap<String, usize> = idx
        .duplicates
        .iter()
        .enumerate()
        .map(|(i, d)| (d.hash.clone(), i))
        .collect();
    for f in &mut idx.files {
        if f.duplicate_set.is_some() {
            f.duplicate_set = f.hash.as_ref().and_then(|h| order.get(h).copied());
        }
    }
}

/// Marks files that a model server (or any process) in `s` has loaded.
pub fn mark_loaded(idx: &mut ModelIndex, s: &Snapshot) {
    let canon = |p: &str| {
        std::fs::canonicalize(p)
            .map(|c| c.display().to_string())
            .unwrap_or_else(|_| p.to_string())
    };
    let mut loaded: HashMap<String, Vec<String>> = HashMap::new();
    for ms in &s.model_servers {
        for m in &ms.models {
            if let Some(f) = &m.file {
                loaded.entry(canon(f)).or_default().push(ms.id.clone());
            }
        }
    }
    for p in &s.processes {
        for f in &p.model_files {
            let owner = s
                .model_servers
                .iter()
                .find(|ms| ms.pids.contains(&p.id))
                .map(|ms| ms.id.clone())
                .unwrap_or_else(|| format!("pid:{}", p.id.pid));
            let v = loaded.entry(canon(f)).or_default();
            if !v.contains(&owner) {
                v.push(owner);
            }
        }
    }
    for f in &mut idx.files {
        f.loaded_by = loaded.get(&f.real_path).cloned().unwrap_or_default();
    }
}

/// Σ sizes of weight files directly or recursively under `dir` (`None` if there are none).
pub fn dir_weight_bytes(dir: &Path) -> Option<u64> {
    let mut total = 0u64;
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut n = 0;
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            n += 1;
            if n > 10_000 {
                break;
            }
            let p = e.path();
            let Ok(md) = std::fs::metadata(&p) else { continue };
            if md.is_dir() {
                if depth < 4 {
                    stack.push((p, depth + 1));
                }
            } else if format_of(&e.file_name().to_string_lossy(), false).is_some() {
                total += md.len();
            }
        }
    }
    (total > 0).then_some(total)
}

/// The HF cache directory of a repo id (`org/name` → `<hub>/models--org--name`).
pub fn hf_repo_dir(env: &HostEnv, repo: &str) -> Option<PathBuf> {
    let hub = env.hf_hub_cache.as_ref()?;
    let parts: Vec<&str> = repo.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|p| p.is_empty() || *p == ".." || *p == ".") {
        return None;
    }
    let d = hub.join(format!("models--{}--{}", parts[0], parts[1]));
    d.is_dir().then_some(d)
}

/// Σ weight-file sizes of a repo's current snapshot (`refs/main`, else the newest snapshot).
pub fn hf_repo_weight_bytes(env: &HostEnv, repo: &str) -> Option<u64> {
    let d = hf_repo_dir(env, repo)?;
    let snaps = d.join("snapshots");
    let rev = std::fs::read_to_string(d.join("refs/main"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|r| !r.is_empty() && !r.contains('/') && snaps.join(r).is_dir())
        .or_else(|| {
            std::fs::read_dir(&snaps)
                .ok()?
                .flatten()
                .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.file_name())))
                .max()
                .map(|(_, n)| n.to_string_lossy().to_string())
        })?;
    dir_weight_bytes(&snaps.join(rev))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::{LoadedModel, ModelServer, ProcId, Process};

    fn sparse(p: &Path, len: u64) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let f = std::fs::File::create(p).unwrap();
        f.set_len(len).unwrap();
    }

    fn write_at(p: &Path, off: u64, data: &[u8]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().write(true).open(p).unwrap();
        f.seek(SeekFrom::Start(off)).unwrap();
        f.write_all(data).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn indexes_hf_ollama_lmstudio_and_finds_duplicates() {
        const M: u64 = 1024 * 1024;
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let env = HostEnv {
            home: Some(h.to_path_buf()),
            hf_hub_cache: Some(h.join(".cache/huggingface/hub")),
            ollama_models: Some(h.join(".ollama/models")),
            ..Default::default()
        };
        // HF: blob + snapshot symlink (counted once, named by repo/file).
        let repo = h.join(".cache/huggingface/hub/models--Qwen--Qwen3-8B");
        sparse(&repo.join("blobs/aaaa"), 40 * M);
        std::fs::create_dir_all(repo.join("snapshots/rev1")).unwrap();
        std::os::unix::fs::symlink(
            repo.join("blobs/aaaa"),
            repo.join("snapshots/rev1/model.safetensors"),
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("refs")).unwrap();
        std::fs::write(repo.join("refs/main"), "rev1\n").unwrap();
        std::fs::write(repo.join("snapshots/rev1/config.json"), "{}").unwrap();
        // Ollama: manifest → blob.
        let om = h.join(".ollama/models");
        let hex = "ab".repeat(32);
        sparse(&om.join(format!("blobs/sha256-{hex}")), 30 * M);
        sparse(&om.join(format!("blobs/sha256-{}", "cd".repeat(32))), 1024); // license layer, too small
        std::fs::create_dir_all(om.join("manifests/registry.ollama.ai/library/llama3")).unwrap();
        std::fs::write(
            om.join("manifests/registry.ollama.ai/library/llama3/8b"),
            format!(r#"{{"layers":[{{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:{hex}"}}]}}"#),
        )
        .unwrap();
        // LM Studio: same GGUF content as a user folder copy → duplicate.
        let lms = h.join(".lmstudio/models/lmstudio-community/Qwen3-GGUF/qwen3-q4.gguf");
        sparse(&lms, 20 * M);
        write_at(&lms, 0, b"GGUFsame-head");
        let user = h.join("Models/copy-of-qwen3.gguf");
        sparse(&user, 20 * M);
        write_at(&user, 0, b"GGUFsame-head");
        // Same size, different content → not a duplicate.
        let other = h.join("Models/different.gguf");
        sparse(&other, 20 * M);
        write_at(&other, 20 * M - 4, b"tail");
        // Hard link: same inode, counted once, never a duplicate of itself.
        std::fs::hard_link(&user, h.join("Models/hardlink.gguf")).unwrap();
        // Small / irrelevant files.
        std::fs::write(h.join("Models/notes.txt"), "x").unwrap();
        sparse(&h.join("Models/tiny.gguf"), 1024);

        let opts = IndexOptions::new(&env, &["~/Models".to_string()]);
        let idx = scan(&opts, &env);
        assert!(idx.errors.is_empty(), "{:?}", idx.errors);
        let names: Vec<&str> = idx.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(idx.files.len(), 5, "{names:?}");
        assert_eq!(names[0], "Qwen/Qwen3-8B/model.safetensors");
        assert_eq!(idx.files[0].source, ModelSource::HuggingFace);
        let ol = idx
            .files
            .iter()
            .find(|f| f.format == ModelFormat::OllamaBlob)
            .unwrap();
        assert_eq!(ol.name, "llama3:8b");
        assert_eq!(ol.source, ModelSource::Ollama);
        assert!(names.contains(&"lmstudio-community/Qwen3-GGUF/qwen3-q4.gguf"));
        assert_eq!(idx.duplicates.len(), 1, "{:?}", idx.duplicates);
        assert_eq!(idx.duplicates[0].paths.len(), 2);
        assert_eq!(idx.duplicates[0].reclaimable, 20 * M);
        assert_eq!(idx.duplicate_bytes, 20 * M);
        assert_eq!(idx.total_bytes, (40 + 30 + 20 + 20 + 20) * M);
        assert!(idx.files.iter().all(|f| f.modified_ms.is_some()));
        let dup_ids: Vec<Option<usize>> = idx.files.iter().map(|f| f.duplicate_set).collect();
        assert_eq!(dup_ids.iter().filter(|d| **d == Some(0)).count(), 2);

        // HF helpers used by the MLX adapter.
        assert_eq!(hf_repo_weight_bytes(&env, "Qwen/Qwen3-8B"), Some(40 * M));
        assert_eq!(hf_repo_weight_bytes(&env, "nope/nope"), None);
        assert_eq!(hf_repo_weight_bytes(&env, "../etc"), None);

        // "Loaded" marking by canonical path.
        let mut s = Snapshot::default();
        s.model_servers.push(ModelServer {
            id: "ollama:10".into(),
            pids: vec![ProcId::new(10, 1)],
            models: vec![LoadedModel {
                file: Some(om.join(format!("blobs/sha256-{hex}")).display().to_string()),
                ..Default::default()
            }],
            ..Default::default()
        });
        s.processes.push(Process {
            id: ProcId::new(99, 1),
            model_files: vec![repo
                .join("snapshots/rev1/model.safetensors")
                .display()
                .to_string()],
            ..Default::default()
        });
        let mut idx = idx;
        mark_loaded(&mut idx, &s);
        assert_eq!(idx.files[0].loaded_by, vec!["pid:99".to_string()]);
        let ol = idx
            .files
            .iter()
            .find(|f| f.format == ModelFormat::OllamaBlob)
            .unwrap();
        assert_eq!(ol.loaded_by, vec!["ollama:10".to_string()]);
        // Round-trips as JSON for `oomtop` frontends.
        let j = serde_json::to_string(&idx).unwrap();
        let back: ModelIndex = serde_json::from_str(&j).unwrap();
        assert_eq!(back, idx);
    }

    #[test]
    fn user_folders_expand_tilde() {
        let env = HostEnv {
            home: Some(PathBuf::from("/h")),
            ..Default::default()
        };
        let o = IndexOptions::new(&env, &["~".into(), "~/Models".into(), "/abs".into()]);
        for want in ["/h", "/h/Models", "/abs"] {
            assert!(
                o.folders.contains(&PathBuf::from(want)),
                "{want}: {:?}",
                o.folders
            );
        }
        assert!(!o.folders.contains(&PathBuf::from("~")));
        let o = IndexOptions::new(&HostEnv::default(), &["~/Models".into()]);
        assert!(
            o.folders
                .iter()
                .all(|f| f.is_absolute() && !f.ends_with("Models")),
            "no $HOME → tilde folders are skipped, not taken literally: {:?}",
            o.folders
        );
    }

    #[test]
    fn budget_and_limits_truncate() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(d.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        let idx = scan(
            &IndexOptions {
                folders: vec![d.path().to_path_buf(), PathBuf::from("/nonexistent/models")],
                max_entries: 5,
                ..Default::default()
            },
            &HostEnv::default(),
        );
        assert!(idx.truncated);
        assert!(idx.files.is_empty());
    }

    #[test]
    fn partial_hash_small_and_large() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        std::fs::write(&a, b"hello").unwrap();
        let h1 = partial_hash(&a, 5).unwrap();
        assert_eq!(h1.len(), 32);
        let b = d.path().join("b");
        std::fs::write(&b, b"hellp").unwrap();
        assert_ne!(h1, partial_hash(&b, 5).unwrap());
    }

    #[test]
    fn hf_names() {
        assert_eq!(
            hf_display_name(Path::new(
                "/h/hub/models--mlx-community--Qwen3-4B-4bit/snapshots/abc/sub/model.safetensors"
            )),
            Some("mlx-community/Qwen3-4B-4bit/sub/model.safetensors".into())
        );
        assert_eq!(hf_display_name(Path::new("/h/hub/models--a--b/blobs/x")), None);
    }
}

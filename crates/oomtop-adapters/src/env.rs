//! Where local runtimes keep their sockets and model folders. Read from **oomtop's own** environment only
//! (other processes' environments are never read here; SPEC §13), injectable for tests.

use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostEnv {
    pub home: Option<PathBuf>,
    /// `$XDG_RUNTIME_DIR` (Linux rootless Docker/Podman sockets).
    pub xdg_runtime_dir: Option<PathBuf>,
    /// `$TMPDIR` (Podman machine API sockets on macOS).
    pub tmpdir: Option<PathBuf>,
    /// `$DOCKER_HOST` when it is a `unix://` URL (tcp endpoints are ignored: loopback-only policy).
    pub docker_host: Option<PathBuf>,
    /// `$HF_HUB_CACHE`, else `$HF_HOME/hub`, else `~/.cache/huggingface/hub`.
    pub hf_hub_cache: Option<PathBuf>,
    /// `$OLLAMA_MODELS`, else `~/.ollama/models`.
    pub ollama_models: Option<PathBuf>,
    pub uid: Option<u32>,
    pub is_macos: bool,
    /// `$ANDROID_AVD_HOME`, else `$ANDROID_USER_HOME/avd`, else `~/.android/avd`.
    pub android_avd_home: Option<PathBuf>,
}

fn var(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

impl HostEnv {
    pub fn from_env() -> HostEnv {
        let home = var("HOME");
        let docker_host = std::env::var("DOCKER_HOST")
            .ok()
            .and_then(|h| h.strip_prefix("unix://").map(PathBuf::from));
        let hf_hub_cache = var("HF_HUB_CACHE")
            .or_else(|| var("HF_HOME").map(|h| h.join("hub")))
            .or_else(|| home.as_ref().map(|h| h.join(".cache/huggingface/hub")));
        let ollama_models = var("OLLAMA_MODELS").or_else(|| home.as_ref().map(|h| h.join(".ollama/models")));
        #[cfg(unix)]
        let uid = Some(unsafe { libc::getuid() });
        #[cfg(not(unix))]
        let uid = None;
        let android_avd_home = var("ANDROID_AVD_HOME")
            .or_else(|| var("ANDROID_USER_HOME").map(|h| h.join("avd")))
            .or_else(|| home.as_ref().map(|h| h.join(".android/avd")));
        HostEnv {
            android_avd_home,
            home,
            xdg_runtime_dir: var("XDG_RUNTIME_DIR"),
            tmpdir: var("TMPDIR"),
            docker_host,
            hf_hub_cache,
            ollama_models,
            uid,
            is_macos: cfg!(target_os = "macos"),
        }
    }

    /// A home-relative path, if `$HOME` is known.
    pub fn home_join(&self, rel: &str) -> Option<PathBuf> {
        self.home.as_ref().map(|h| h.join(rel))
    }

    /// Candidate Ollama model stores (user default, `$OLLAMA_MODELS`, Linux service installs).
    pub fn ollama_dirs(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.ollama_models.iter().cloned().collect();
        v.extend(self.home_join(".ollama/models"));
        v.push(PathBuf::from("/usr/share/ollama/.ollama/models"));
        v.push(PathBuf::from("/var/lib/ollama/models"));
        v.dedup();
        v
    }

    /// Candidate LM Studio model folders.
    pub fn lm_studio_dirs(&self) -> Vec<PathBuf> {
        [".lmstudio/models", ".cache/lm-studio/models"]
            .iter()
            .filter_map(|r| self.home_join(r))
            .collect()
    }
}

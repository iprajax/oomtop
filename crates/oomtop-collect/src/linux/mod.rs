//! Linux Sources (SPEC §6 `linux/`): procfs, smaps_rollup (rotating), PSI, vmstat, cgroup v2, cpufreq,
//! thermal zones, hwmon, powercap (RAPL), power_supply, NVML (dlopen), DRM fdinfo, amdgpu `gpu_metrics`,
//! systemd-oomd / earlyoom configuration.
//!
//! **I/O only here** ([`host`], [`procs`], [`nvml`]); everything that interprets data is pure
//! ([`parse`], [`gpu`], [`oom`], [`smaps`], [`decode`]) and runs on any OS, so fixtures recorded on Linux
//! replay in macOS CI and vice versa. Readers are plain `std::fs` over configurable roots
//! ([`LinuxRoots`]), so tests point them at a fake tree (`fixtures/linux/tree-*`).
//!
//! # `LinuxFilesRaw::files` key conventions
//! - `meminfo`, `1234/stat`, … — paths relative to `/proc` (`exe`/`cwd`/`fd` hold readlink targets).
//! - `/sys/…`, `/etc/…`, `/usr/lib/…` — absolute paths for everything outside `/proc`. Binary files are
//!   stored as `hex:…` ([`parse::to_hex`]); a key ending in `/` holds a directory listing.
//! - `@…` — synthetic records produced by the Source (never file contents): `@nvml`, `@nvml/procs`,
//!   `@status/<sub-source>`, `@oom/scan`, `@oom/daemons`, `@oom/kills`, `@users`, `@gpu/devices`,
//!   `@drm/scanned`, `<pid>/@smaps_cache`, `<pid>/@model_files`.
//!
//! `environ` is never stored: allowlisted keys become [`oomtop_core::Markers`] (hashed values) and every
//! other pair is dropped without being copied.

pub mod decode;
pub mod gpu;
pub mod host;
pub mod nvml;
pub mod oom;
pub mod parse;
pub mod procs;
pub mod smaps;

#[cfg(test)]
mod tests;

pub use decode::{decode_full, enrich, replay_full};
pub use host::LinuxHostSource;
pub use procs::LinuxProcSource;

use crate::raw::LinuxFilesRaw;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// smaps_rollup top-N re-read every 5 s (SPEC §6.2 rotation: it takes the mmap lock).
pub const SMAPS_PER_SAMPLE: usize = 64;

/// Synthetic key names (see the module docs).
pub mod keys {
    pub const NVML: &str = "@nvml";
    pub const NVML_PROCS: &str = "@nvml/procs";
    pub const OOM_SCAN: &str = "@oom/scan";
    pub const OOM_DAEMONS: &str = "@oom/daemons";
    pub const OOM_KILLS: &str = "@oom/kills";
    pub const USERS: &str = "@users";
    pub const GPU_DEVICES: &str = "@gpu/devices";
    pub const DRM_SCANNED: &str = "@drm/scanned";
    pub const SMAPS_CACHE: &str = "@smaps_cache";
    pub const MODEL_FILES: &str = "@model_files";
    /// `@status/<name>` = `unavailable: reason` | `partial: reason`.
    pub fn status(name: &str) -> String {
        format!("@status/{name}")
    }
}

/// `sysconf(_SC_CLK_TCK)`, 100 when unknown.
pub fn clk_tck() -> u64 {
    // SAFETY: sysconf is always safe to call.
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 {
        v as u64
    } else {
        100
    }
}

/// `sysconf(_SC_PAGESIZE)`, 4096 when unknown.
pub fn page_size() -> u64 {
    // SAFETY: sysconf is always safe to call.
    let v = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if v > 0 {
        v as u64
    } else {
        4096
    }
}

/// Where the Sources read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxRoots {
    /// procfs root (`/proc`).
    pub proc: PathBuf,
    /// Filesystem root for `/sys`, `/etc`, `/usr/lib`, `/run`; `None` disables those reads.
    pub fs: Option<PathBuf>,
}

impl Default for LinuxRoots {
    fn default() -> Self {
        LinuxRoots::system()
    }
}

impl LinuxRoots {
    /// The live system: `/proc` and `/`.
    pub fn system() -> Self {
        LinuxRoots {
            proc: PathBuf::from("/proc"),
            fs: Some(PathBuf::from("/")),
        }
    }

    /// A fake tree: `<root>/proc`, `<root>/sys`, `<root>/etc`, …
    pub fn under(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        LinuxRoots {
            proc: root.join("proc"),
            fs: Some(root.to_path_buf()),
        }
    }

    /// Back-compat with `Source::new(proc_root)`: a root named `proc` implies its parent as the
    /// filesystem root (`/proc` → `/`); any other directory is a bare procfs without `/sys`.
    pub fn from_proc_root(proc: impl Into<PathBuf>) -> Self {
        let proc = proc.into();
        let fs = if proc.file_name().and_then(|n| n.to_str()) == Some("proc") {
            proc.parent().map(|p| {
                if p.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    p.to_path_buf()
                }
            })
        } else {
            None
        };
        LinuxRoots { proc, fs }
    }

    /// True when reading the live system (NVML and uid checks only make sense there).
    pub fn is_system(&self) -> bool {
        self.proc == Path::new("/proc")
    }

    pub fn proc_path(&self, rel: &str) -> PathBuf {
        self.proc.join(rel)
    }

    /// Maps an absolute path (`/sys/…`) under the filesystem root.
    pub fn fs_path(&self, abs: &str) -> Option<PathBuf> {
        let rel = abs.trim_start_matches('/');
        if rel.split('/').any(|c| c == "..") {
            return None;
        }
        self.fs.as_ref().map(|r| r.join(rel))
    }
}

/// Human reason for an I/O error, stable across platforms ("permission denied", "not found").
pub fn io_reason(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => "permission denied".into(),
        std::io::ErrorKind::NotFound => "not found".into(),
        _ => match e.raw_os_error() {
            Some(libc::ENODATA) => "no data".into(),
            Some(libc::EINVAL) => "invalid argument".into(),
            Some(libc::EIO) => "i/o error".into(),
            Some(libc::ESRCH) => "no such process".into(),
            _ => e.to_string(),
        },
    }
}

/// Upper bound for one text read (maps of huge processes, fdinfo): larger files are cut.
pub const MAX_READ_BYTES: u64 = 4 << 20;

/// Reads a file as (lossy) UTF-8, at most [`MAX_READ_BYTES`].
pub fn read_text(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    f.take(MAX_READ_BYTES).read_to_end(&mut buf)?;
    Ok(match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

/// Sorted names in a directory; empty when unreadable.
pub fn list_dir(path: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(path)
        .map(|rd| {
            rd.filter_map(|e| e.ok()?.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Natural sort key for names like `thermal_zone10` (so zone10 follows zone9).
pub fn natural_key(s: &str) -> (String, u64) {
    let digits: String = s
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let n = digits.parse().unwrap_or(0);
    (s[..s.len() - digits.len()].to_string(), n)
}

/// Budget-aware reader that records file contents into a [`LinuxFilesRaw`] under the key conventions.
pub(crate) struct Reader<'a> {
    pub raw: &'a mut LinuxFilesRaw,
    pub roots: &'a LinuxRoots,
    start: Instant,
    budget: Duration,
}

impl<'a> Reader<'a> {
    pub fn new(raw: &'a mut LinuxFilesRaw, roots: &'a LinuxRoots, start: Instant, budget: Duration) -> Self {
        Reader {
            raw,
            roots,
            start,
            budget,
        }
    }

    /// True once `frac` of the budget is spent.
    pub fn spent(&self, frac: f64) -> bool {
        self.start.elapsed().as_secs_f64() >= self.budget.as_secs_f64() * frac
    }

    /// `/proc/<rel>`; failures are recorded in `missing` (callers prune per-pid noise).
    pub fn proc(&mut self, rel: &str) -> Option<String> {
        match read_text(&self.roots.proc_path(rel)) {
            Ok(s) => {
                self.raw.files.insert(rel.to_string(), s.clone());
                Some(s)
            }
            Err(e) => {
                self.raw.missing.insert(rel.to_string(), io_reason(&e));
                None
            }
        }
    }

    /// `/proc/<rel>` without recording a failure.
    pub fn proc_quiet(&mut self, rel: &str) -> Option<String> {
        let s = read_text(&self.roots.proc_path(rel)).ok()?;
        self.raw.files.insert(rel.to_string(), s.clone());
        Some(s)
    }

    /// readlink `/proc/<rel>` stored as the target.
    pub fn proc_link(&mut self, rel: &str) -> Option<String> {
        let t = std::fs::read_link(self.roots.proc_path(rel)).ok()?;
        let t = t.to_string_lossy().into_owned();
        self.raw.files.insert(rel.to_string(), t.clone());
        Some(t)
    }

    /// An absolute path outside `/proc`. Only "permission denied" is recorded as missing (that is the
    /// actionable "needs root" case); absent files are normal on most hosts.
    pub fn abs(&mut self, abs: &str) -> Option<String> {
        let path = self.roots.fs_path(abs)?;
        match read_text(&path) {
            Ok(s) => {
                self.raw.files.insert(abs.to_string(), s.clone());
                Some(s)
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    self.raw.missing.insert(abs.to_string(), io_reason(&e));
                }
                None
            }
        }
    }

    /// A binary file stored as `hex:…`.
    pub fn abs_hex(&mut self, abs: &str) -> Option<()> {
        let path = self.roots.fs_path(abs)?;
        match std::fs::read(&path) {
            Ok(b) if !b.is_empty() && b.len() <= 64 * 1024 => {
                self.raw.files.insert(abs.to_string(), parse::to_hex(&b));
                Some(())
            }
            Ok(_) => None,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    self.raw.missing.insert(abs.to_string(), io_reason(&e));
                }
                None
            }
        }
    }

    /// Directory listing of an absolute path (names), recorded under `<abs>/`.
    pub fn abs_list(&mut self, abs: &str, record: bool) -> Vec<String> {
        let Some(path) = self.roots.fs_path(abs) else {
            return Vec::new();
        };
        let names = list_dir(&path);
        if record && !names.is_empty() {
            let key = format!("{}/", abs.trim_end_matches('/'));
            self.raw.files.insert(key, names.join("\n") + "\n");
        }
        names
    }

    /// Whether an absolute path exists under the filesystem root.
    pub fn abs_exists(&self, abs: &str) -> bool {
        self.roots.fs_path(abs).map(|p| p.exists()).unwrap_or(false)
    }
}

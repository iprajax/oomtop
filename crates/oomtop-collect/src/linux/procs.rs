//! `linux.procs` Source: per-process `/proc/<pid>/*` (SPEC §5, §6.2, §7).
//!
//! Per sample, in budget order:
//! 1. basics for every pid: `stat`, `statm`, `status`, `cmdline`, `cgroup`, `oom_score`, `io` (readable
//!    processes only), `exe`/`cwd` links, allowlisted environ markers (cached per `(pid, starttime)`);
//! 2. `smaps_rollup` for the processes the [`SmapsScheduler`] picks (top 64 by RSS every 5 s, the rest
//!    round-robin within 60 s); cached values of the others go to `<pid>/@smaps_cache`;
//! 3. an incremental fd scan (≤ every [`FD_RESCAN_MS`] per process): DRM client fds (`/dev/dri/*`) and open
//!    model files; then the `fdinfo` of every known DRM fd (runtime-detected `drm-*` keys);
//! 4. `maps` of large processes (≤ every [`MAPS_RESCAN_MS`]) for mmapped model files;
//! 5. NVML per-process GPU memory when the NVIDIA driver is loaded.
//!
//! Other users' processes only get the world-readable files (no environ/io/smaps/fd), unless running as
//! root. `environ` is converted to markers and never stored.

use super::parse::{is_model_file, model_files_from_maps};
use super::smaps::{cache_to_text, ProcKey, SmapsScheduler};
use super::{clk_tck, keys, list_dir, nvml, page_size, read_text, LinuxRoots, Reader, SMAPS_PER_SAMPLE};
use crate::decode::linux::{parse_kv_kb, parse_stat};
use crate::raw::{names, LinuxFilesRaw, RawPayload, RawSample};
use crate::{now_ms, Source, SourceError};
use oomtop_core::redact::{default_allowlist, markers_from_env};
use oomtop_core::Markers;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A process's fds are re-scanned at most this often.
pub const FD_RESCAN_MS: u64 = 10_000;
/// `maps` of a large process is re-read at most this often.
pub const MAPS_RESCAN_MS: u64 = 30_000;
/// Processes at least this large (RSS) get a `maps` scan for mmapped model files.
pub const MAPS_MIN_RSS: u64 = 256 << 20;
const MAPS_PER_SAMPLE: usize = 4;
const FD_SCANS_PER_SAMPLE: usize = 64;
const MAX_FDS: usize = 4096;
const MAX_DRM_FDS: usize = 16;
const PRESENCE_EVERY_MS: u64 = 60_000;

/// Which processes' private files may be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// root, or a fixture tree.
    All,
    /// Only processes of this uid.
    Uid(u32),
}

impl Access {
    fn allows(self, uid: Option<u32>) -> bool {
        match self {
            Access::All => true,
            Access::Uid(u) => uid == Some(u),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct FdResult {
    scanned_ms: u64,
    /// The fd directory was readable (otherwise "not scanned", never "no GPU clients").
    ok: bool,
    drm_fds: Vec<u32>,
    model_files: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Presence {
    /// (pci, driver, vram_total) per DRM card.
    cards: Vec<(String, String, Option<u64>)>,
    nvidia: bool,
}

/// Per-process Source.
#[derive(Debug, Clone)]
pub struct LinuxProcSource {
    roots: LinuxRoots,
    allowlist: Vec<String>,
    access: Access,
    nvml: bool,
    markers: HashMap<ProcKey, Markers>,
    smaps: SmapsScheduler,
    fds: HashMap<ProcKey, FdResult>,
    maps: HashMap<ProcKey, (u64, Vec<String>)>,
    users: Option<(u64, BTreeMap<u32, String>)>,
    presence: Option<(u64, Presence)>,
    clk_tck: u64,
    page_size: u64,
}

impl Default for LinuxProcSource {
    fn default() -> Self {
        Self::new("/proc", default_allowlist())
    }
}

fn default_access(roots: &LinuxRoots) -> Access {
    if roots.is_system() {
        // SAFETY: geteuid never fails.
        let euid = unsafe { libc::geteuid() };
        if euid == 0 {
            Access::All
        } else {
            Access::Uid(euid)
        }
    } else {
        Access::All
    }
}

impl LinuxProcSource {
    /// `root` = procfs root (`/proc`); a root named `proc` also enables `/sys`/`/etc` reads under its parent.
    pub fn new(root: impl Into<PathBuf>, allowlist: Vec<String>) -> Self {
        Self::with_roots(LinuxRoots::from_proc_root(root), allowlist)
    }

    pub fn with_roots(roots: LinuxRoots, allowlist: Vec<String>) -> Self {
        LinuxProcSource {
            access: default_access(&roots),
            nvml: roots.is_system(),
            roots,
            allowlist,
            markers: HashMap::new(),
            smaps: SmapsScheduler::new(SMAPS_PER_SAMPLE),
            fds: HashMap::new(),
            maps: HashMap::new(),
            users: None,
            presence: None,
            clk_tck: clk_tck(),
            page_size: page_size(),
        }
    }

    /// Restricts private reads to one uid (`None` = all, as root).
    pub fn with_access_uid(mut self, uid: Option<u32>) -> Self {
        self.access = uid.map_or(Access::All, Access::Uid);
        self
    }

    pub fn with_nvml(mut self, on: bool) -> Self {
        self.nvml = on;
        self
    }

    /// Overrides CLK_TCK / page size (fixture trees recorded on other machines).
    pub fn with_units(mut self, clk_tck: u64, page_size: u64) -> Self {
        self.clk_tck = clk_tck.max(1);
        self.page_size = page_size.max(1);
        self
    }

    pub fn access(&self) -> Access {
        self.access
    }

    fn markers_for(&mut self, key: ProcKey) -> Markers {
        if let Some(m) = self.markers.get(&key) {
            return m.clone();
        }
        let m = match read_bounded(&self.roots.proc_path(&format!("{}/environ", key.0))) {
            Ok(bytes) => {
                let pairs = bytes
                    .split(|b| *b == 0)
                    .filter_map(|kv| std::str::from_utf8(kv).ok()?.split_once('='));
                markers_from_env(pairs, &self.allowlist)
            }
            Err(_) => Markers::default(),
        };
        self.markers.insert(key, m.clone());
        m
    }

    /// Allowlisted markers of `jobs`. New processes' environ reads honor the budget (cached markers are
    /// free); the rest are read on the next sample instead of blowing the budget on a cold start with
    /// thousands of processes, and the sample says so (`@status/markers`).
    fn collect_markers(
        &mut self,
        jobs: Vec<ProcKey>,
        over_budget: impl Fn() -> bool,
        raw: &mut LinuxFilesRaw,
    ) {
        let mut deferred = 0usize;
        for key in jobs {
            if !self.markers.contains_key(&key) && over_budget() {
                deferred += 1;
                continue;
            }
            let m = self.markers_for(key);
            if !m.keys.is_empty() {
                raw.markers.insert(key.0, m);
            }
        }
        if deferred > 0 {
            raw.files.insert(
                keys::status("markers"),
                format!("partial: time budget; session markers of {deferred} processes deferred"),
            );
        }
    }

    fn presence(&mut self, now: u64) -> Presence {
        if let Some((at, p)) = &self.presence {
            if now.saturating_sub(*at) < PRESENCE_EVERY_MS && now >= *at {
                return p.clone();
            }
        }
        let mut cards = Vec::new();
        if let Some(base) = self.roots.fs_path("/sys/class/drm") {
            for n in list_dir(&base) {
                let is_card = n
                    .strip_prefix("card")
                    .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()));
                if !is_card {
                    continue;
                }
                let dev = base.join(&n).join("device");
                let uevent = read_text(&dev.join("uevent"))
                    .map(|t| super::parse::parse_env_lines(&t))
                    .unwrap_or_default();
                let vram = read_text(&dev.join("mem_info_vram_total"))
                    .ok()
                    .and_then(|t| super::parse::parse_u64(&t));
                cards.push((
                    uevent.get("PCI_SLOT_NAME").cloned().unwrap_or_else(|| "-".into()),
                    uevent.get("DRIVER").cloned().unwrap_or_else(|| "-".into()),
                    vram,
                ));
            }
        }
        let nvidia = self.roots.proc_path("driver/nvidia/version").exists();
        let p = Presence { cards, nvidia };
        self.presence = Some((now, p.clone()));
        p
    }

    fn users(&mut self, now: u64) -> BTreeMap<u32, String> {
        if let Some((at, u)) = &self.users {
            if now.saturating_sub(*at) < PRESENCE_EVERY_MS && now >= *at {
                return u.clone();
            }
        }
        let u = self
            .roots
            .fs_path("/etc/passwd")
            .and_then(|p| read_text(&p).ok())
            .map(|t| super::parse::parse_passwd(&t))
            .unwrap_or_default();
        self.users = Some((now, u.clone()));
        u
    }

    /// readdir + readlink of `/proc/<pid>/fd`: DRM client fds and open model files.
    fn scan_fds(&self, pid: u32, now: u64) -> Option<FdResult> {
        let dir = self.roots.proc_path(&format!("{pid}/fd"));
        let rd = std::fs::read_dir(&dir).ok()?;
        let mut res = FdResult {
            scanned_ms: now,
            ok: true,
            ..Default::default()
        };
        for e in rd.take(MAX_FDS).flatten() {
            let Some(fd) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(t) = std::fs::read_link(e.path()) else {
                continue;
            };
            let t = t.to_string_lossy();
            if t.starts_with("/dev/dri/") {
                if res.drm_fds.len() < MAX_DRM_FDS {
                    res.drm_fds.push(fd);
                }
            } else if t.starts_with('/') && is_model_file(&t) {
                res.model_files.push(t.into_owned());
            }
        }
        res.drm_fds.sort_unstable();
        res.model_files.sort();
        res.model_files.dedup();
        Some(res)
    }
}

/// Reads at most [`MAX_READ_BYTES`](super::MAX_READ_BYTES) (environ is bounded by ARG_MAX, but a process can
/// grow its environment block with `prctl(PR_SET_MM_ENV_*)`).
fn read_bounded(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)?
        .take(super::MAX_READ_BYTES)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

struct Basic {
    key: ProcKey,
    rss: u64,
    uid: Option<u32>,
    /// Private files (environ, io, smaps, fd, maps) are readable: the kernel's ptrace-read check needs
    /// the real, effective and saved uid of the target to all be ours (or root).
    readable: bool,
}

/// `Uid:` line of `/proc/<pid>/status` → (real, effective, saved, fs).
fn status_uids(status: &str) -> Option<[u32; 4]> {
    let l = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    let v: Vec<u32> = l.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    (v.len() == 4).then(|| [v[0], v[1], v[2], v[3]])
}

impl Source for LinuxProcSource {
    fn name(&self) -> &'static str {
        names::LINUX_PROCS
    }

    fn read(&mut self, budget: Duration) -> Result<RawSample, SourceError> {
        let start = Instant::now();
        let now = now_ms();
        let mut raw = LinuxFilesRaw {
            clk_tck: self.clk_tck,
            page_size: self.page_size,
            ..Default::default()
        };
        let roots = self.roots.clone();
        let access = self.access;
        let entries = std::fs::read_dir(&roots.proc)
            .map_err(|e| SourceError::Unavailable(format!("{}: {e}", roots.proc.display())))?;
        let mut pids: Vec<u32> = entries
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .collect();
        pids.sort_unstable();

        // 1. Basics.
        let mut basics: Vec<Basic> = Vec::with_capacity(pids.len());
        let mut marker_jobs: Vec<ProcKey> = Vec::new();
        {
            let mut r = Reader::new(&mut raw, &roots, start, budget);
            r.proc("stat");
            // decode_files rebuilds HostCpu from any sample with `stat`: keep load averages in it too.
            r.proc_quiet("loadavg");
            // Hybrid x86 (Intel P/E) core lists for the per-core meters; absent elsewhere (kinds unknown).
            r.abs("/sys/devices/cpu_core/cpus");
            r.abs("/sys/devices/cpu_atom/cpus");
            for pid in pids.iter().copied() {
                if r.spent(1.0) {
                    r.raw.truncated = true;
                    break;
                }
                let Some(stat) = r.proc(&format!("{pid}/stat")) else {
                    continue;
                };
                let Some(st) = parse_stat(&stat) else {
                    continue;
                };
                let statm = r.proc(&format!("{pid}/statm"));
                let status = r.proc(&format!("{pid}/status"));
                for f in ["cmdline", "cgroup", "oom_score"] {
                    r.proc(&format!("{pid}/{f}"));
                }
                let uids = status.as_deref().and_then(status_uids);
                let uid = uids.map(|u| u[0]);
                let readable = match access {
                    Access::All => true,
                    Access::Uid(_) => uids.is_some_and(|u| u[..3].iter().all(|x| access.allows(Some(*x)))),
                };
                if readable {
                    r.proc_quiet(&format!("{pid}/io"));
                }
                for link in ["exe", "cwd"] {
                    r.proc_link(&format!("{pid}/{link}"));
                }
                let rss = statm
                    .as_deref()
                    .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .map(|p| p * self.page_size)
                    .unwrap_or(0);
                let key = (pid, st.starttime);
                if readable {
                    marker_jobs.push(key);
                }
                basics.push(Basic {
                    key,
                    rss,
                    uid,
                    readable,
                });
            }
        }
        self.collect_markers(marker_jobs, || start.elapsed() >= budget, &mut raw);
        let readable: Vec<&Basic> = basics.iter().filter(|b| b.readable).collect();
        let mut r = Reader::new(&mut raw, &roots, start, budget);

        // 2. smaps_rollup rotation.
        let cands: Vec<(ProcKey, u64)> = readable
            .iter()
            .filter(|b| b.rss > 0)
            .map(|b| (b.key, b.rss))
            .collect();
        let mut read_now: HashSet<ProcKey> = HashSet::new();
        for key in self.smaps.plan(now, &cands) {
            if r.spent(0.85) {
                break;
            }
            let rel = format!("{}/smaps_rollup", key.0);
            let vals = r.proc_quiet(&rel).and_then(|t| {
                let kv = parse_kv_kb(&t);
                Some((
                    *kv.get("Pss")?,
                    kv.get("SwapPss").copied(),
                    kv.get("Rss").copied().unwrap_or(0),
                ))
            });
            if vals.is_none() {
                r.raw.files.remove(&rel);
            } else {
                read_now.insert(key);
            }
            self.smaps.record(key, now, vals);
        }
        for (key, _) in &cands {
            if read_now.contains(key) {
                continue;
            }
            if let Some(c) = self.smaps.cached(key) {
                r.raw
                    .files
                    .insert(format!("{}/{}", key.0, keys::SMAPS_CACHE), cache_to_text(&c, now));
            }
        }

        // 3. fd scan (DRM clients + open model files), oldest first.
        let presence = self.presence(now);
        let mut due: Vec<(u64, ProcKey)> = readable
            .iter()
            .filter_map(|b| {
                let age = self
                    .fds
                    .get(&b.key)
                    .map(|f| now.saturating_sub(f.scanned_ms))
                    .unwrap_or(u64::MAX);
                (age >= FD_RESCAN_MS).then_some((age, b.key))
            })
            .collect();
        due.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for (_, key) in due.into_iter().take(FD_SCANS_PER_SAMPLE) {
            if r.spent(0.85) {
                break;
            }
            match self.scan_fds(key.0, now) {
                Some(res) => {
                    self.fds.insert(key, res);
                }
                None => {
                    // Unreadable (exited / not ours): remember the attempt, but report it as not scanned.
                    self.fds.insert(
                        key,
                        FdResult {
                            scanned_ms: now,
                            ..Default::default()
                        },
                    );
                }
            }
        }
        let mut drm_scanned = Vec::new();
        for b in &readable {
            let Some(res) = self.fds.get(&b.key).filter(|f| f.ok) else {
                continue;
            };
            drm_scanned.push(format!("{}:{}", b.key.0, b.key.1));
            for fd in &res.drm_fds {
                if r.spent(0.95) {
                    break;
                }
                r.proc_quiet(&format!("{}/fdinfo/{fd}", b.key.0));
            }
        }
        if !presence.cards.is_empty() || presence.nvidia {
            let mut text = format!(
                "drm {}\nnvidia {}\n",
                presence.cards.len(),
                u8::from(presence.nvidia)
            );
            for (pci, driver, vram) in &presence.cards {
                let v = vram.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
                text.push_str(&format!("card {pci} {driver} {v}\n"));
            }
            r.raw.files.insert(keys::GPU_DEVICES.into(), text);
            if !drm_scanned.is_empty() {
                r.raw
                    .files
                    .insert(keys::DRM_SCANNED.into(), drm_scanned.join(" ") + "\n");
            }
        }

        // 4. maps of large processes (mmapped model weights).
        let mut maps_due: Vec<(u64, ProcKey)> = readable
            .iter()
            .filter(|b| b.rss >= MAPS_MIN_RSS)
            .filter_map(|b| {
                let age = self
                    .maps
                    .get(&b.key)
                    .map(|(t, _)| now.saturating_sub(*t))
                    .unwrap_or(u64::MAX);
                (age >= MAPS_RESCAN_MS).then_some((age, b.key))
            })
            .collect();
        maps_due.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for (_, key) in maps_due.into_iter().take(MAPS_PER_SAMPLE) {
            if r.spent(0.85) {
                break;
            }
            // Parsed in flight: maps can be megabytes; only the model paths are kept.
            let files = read_text(&roots.proc_path(&format!("{}/maps", key.0)))
                .map(|t| model_files_from_maps(&t))
                .unwrap_or_default();
            self.maps.insert(key, (now, files));
        }
        for b in &readable {
            let mut files: BTreeSet<String> = BTreeSet::new();
            if let Some(f) = self.fds.get(&b.key) {
                files.extend(f.model_files.iter().cloned());
            }
            if let Some((_, f)) = self.maps.get(&b.key) {
                files.extend(f.iter().cloned());
            }
            if !files.is_empty() {
                r.raw.files.insert(
                    format!("{}/{}", b.key.0, keys::MODEL_FILES),
                    files.into_iter().collect::<Vec<_>>().join("\n") + "\n",
                );
            }
        }

        // 5. NVML per-process GPU memory.
        if presence.nvidia {
            if self.nvml {
                match nvml::query_procs(now) {
                    Ok(p) => {
                        r.raw
                            .files
                            .insert(keys::NVML_PROCS.into(), nvml::procs_to_text(&p));
                    }
                    Err(e) => {
                        r.raw
                            .files
                            .insert(keys::status("nvml"), format!("unavailable: {e}"));
                    }
                }
            } else {
                r.raw.files.insert(
                    keys::status("nvml"),
                    "unavailable: NVML disabled for this root".into(),
                );
            }
        }

        // Users of the processes seen (uid → name only for those uids).
        let uids: BTreeSet<u32> = basics.iter().filter_map(|b| b.uid).collect();
        let users = self.users(now);
        let text: String = uids
            .iter()
            .filter_map(|u| users.get(u).map(|n| format!("{u} {n}\n")))
            .collect();
        if !text.is_empty() {
            raw.files.insert(keys::USERS.into(), text);
        }

        // Forget exited processes (only after a complete listing).
        if !raw.truncated {
            let alive: HashSet<ProcKey> = basics.iter().map(|b| b.key).collect();
            self.markers.retain(|k, _| alive.contains(k));
            self.smaps.retain(&alive);
            self.fds.retain(|k, _| alive.contains(k));
            self.maps.retain(|k, _| alive.contains(k));
        }
        // Per-pid read failures are normal (processes exit, other users' files): don't report them.
        raw.missing.retain(|k, _| !k.contains('/'));
        Ok(RawSample {
            source: names::LINUX_PROCS.into(),
            taken_at_ms: now,
            read_us: start.elapsed().as_micros() as u64,
            payload: RawPayload::LinuxFiles(raw),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_reads_are_deferred_when_over_budget_and_cached_ones_still_emitted() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("proc/5");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("environ"),
            "CLAUDECODE=1\0CLAUDE_CODE_SESSION_ID=abc\0SECRET_TOKEN=tok-x\0",
        )
        .unwrap();
        let mut src = LinuxProcSource::with_roots(LinuxRoots::under(dir.path()), default_allowlist());
        let key = (5, 100);

        let mut raw = LinuxFilesRaw::default();
        src.collect_markers(vec![key], || true, &mut raw);
        assert!(raw.markers.is_empty());
        assert!(raw.files[&keys::status("markers")].starts_with("partial:"));

        let mut raw = LinuxFilesRaw::default();
        src.collect_markers(vec![key], || false, &mut raw);
        assert_eq!(raw.markers[&5].keys, vec!["CLAUDECODE", "CLAUDE_CODE_SESSION_ID"]);
        assert!(!format!("{raw:?}").contains("tok-x"));
        assert!(!format!("{raw:?}").contains("abc\""));

        // Cached: emitted even when the budget is gone, no deferral reported.
        let mut raw = LinuxFilesRaw::default();
        src.collect_markers(vec![key], || true, &mut raw);
        assert!(raw.markers.contains_key(&5));
        assert!(!raw.files.contains_key(&keys::status("markers")));
    }

    #[test]
    fn environ_reads_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big");
        std::fs::write(&p, vec![b'a'; (super::super::MAX_READ_BYTES + 10) as usize]).unwrap();
        assert_eq!(
            read_bounded(&p).unwrap().len() as u64,
            super::super::MAX_READ_BYTES
        );
        assert!(read_bounded(&dir.path().join("missing")).is_err());
    }
}

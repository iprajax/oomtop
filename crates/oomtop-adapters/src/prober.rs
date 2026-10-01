//! Stateful probing with caches and budgets (SPEC §14: adapters must not blow the sampling budget).
//!
//! - weight-file scans (open files / maps) are cached per `ProcId` (servers 10 s, other large processes 30 s)
//!   and bounded per probe in count and time;
//! - GGUF-header KV estimates are cached per (file, ctx, KV type);
//! - vLLM token counters are kept to derive tok/s;
//! - container engines refresh every 10 s within a time budget, rotating which containers get stats.
//!
//! [`crate::probe`] uses one process-wide `Prober`, so callers get caching without holding state.

use crate::docker::{engine_sockets, merge_engine, probe_engine, EngineProbe};
use crate::env::HostEnv;
use crate::models::{self, classify, merge_status};
use crate::sandboxes::{detect_sandboxes, detect_seatbelt, enrich_live, seatbelt_check};
use crate::weights::scan_process;
use crate::{Probe, ProbeOptions};
use oomtop_core::{ProcId, Snapshot, SourceStatus};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct ProberConfig {
    /// Re-scan a model server's open/mapped files after this long.
    pub server_scan_ttl: Duration,
    /// Re-scan other large processes after this long.
    pub scan_ttl: Duration,
    /// Time budget for weight scans per probe.
    pub scan_budget: Duration,
    /// At most this many (uncached) processes are scanned per probe.
    pub max_scans: usize,
    /// Processes below this footprint are not scanned unless they are model servers.
    pub scan_min_footprint: u64,
    pub engine_refresh: Duration,
    /// Time budget per engine refresh (all engines).
    pub engine_budget: Duration,
    /// Containers that get stats per engine refresh.
    pub engine_max_stats: usize,
}

impl Default for ProberConfig {
    fn default() -> Self {
        ProberConfig {
            server_scan_ttl: Duration::from_secs(10),
            scan_ttl: Duration::from_secs(30),
            scan_budget: Duration::from_millis(25),
            max_scans: 16,
            scan_min_footprint: 512 * 1024 * 1024,
            engine_refresh: Duration::from_secs(10),
            // Engines refresh every 10 s; keep a refresh near the 200 ms adapter timeout so it never stalls a
            // frame for long (containers without fresh stats rotate into the next refresh).
            engine_budget: Duration::from_millis(300),
            engine_max_stats: 16,
        }
    }
}

/// Caches shared by the model-server adapters.
#[derive(Debug, Default)]
pub struct Caches {
    pub(crate) kv: HashMap<(String, u64, u64), Option<u64>>,
    pub(crate) vllm_prev: HashMap<ProcId, (Instant, f64)>,
    /// llama.cpp `(tokens_predicted_total, tokens_predicted_seconds_total)` of the previous probe.
    pub(crate) llama_prev: HashMap<ProcId, (f64, f64)>,
}

#[derive(Debug)]
struct Scan {
    at: Instant,
    files: Vec<String>,
}

#[derive(Debug, Default)]
struct EngineCache {
    at: Option<Instant>,
    engines: Vec<EngineProbe>,
    status: BTreeMap<String, SourceStatus>,
    rotation: usize,
}

/// Probes model servers and sandboxes with caching. One per sampling loop.
#[derive(Debug)]
pub struct Prober {
    cfg: ProberConfig,
    env: HostEnv,
    caches: Caches,
    scans: HashMap<ProcId, Scan>,
    engines: EngineCache,
    /// Seatbelt state per process (fixed after exec for tools; only definite answers are cached).
    seatbelt: HashMap<ProcId, bool>,
}

impl Default for Prober {
    fn default() -> Self {
        Prober::new(ProberConfig::default(), HostEnv::from_env())
    }
}

impl Prober {
    pub fn new(cfg: ProberConfig, env: HostEnv) -> Prober {
        Prober {
            cfg,
            env,
            caches: Caches::default(),
            scans: HashMap::new(),
            engines: EngineCache::default(),
            seatbelt: HashMap::new(),
        }
    }

    pub fn env(&self) -> &HostEnv {
        &self.env
    }

    /// One probe of `snapshot` (groups should already be attributed so `group_id` / `started_by_group`
    /// can be filled). With `opts.http == false` nothing outside the snapshot is read.
    pub fn probe(&mut self, s: &Snapshot, opts: &ProbeOptions) -> Probe {
        let mut status = BTreeMap::new();
        let files = if opts.http {
            self.scan_weights(s)
        } else {
            BTreeMap::new()
        };
        let model_servers = models::detect(s, opts, &files, &mut self.caches, &self.env, &mut status);
        let mut sandboxes = detect_sandboxes(s);
        if opts.http {
            enrich_live(&mut sandboxes, s, &self.env, opts.timeout, &mut status);
            self.engines(s, opts, &mut sandboxes, &mut status);
            let cache = &mut self.seatbelt;
            let seatbelt = detect_seatbelt(s, &sandboxes, self.env.uid, |id| {
                if let Some(&v) = cache.get(&id) {
                    return Some(v);
                }
                let v = seatbelt_check(id);
                if let Some(v) = v {
                    cache.insert(id, v);
                }
                v
            });
            sandboxes.extend(seatbelt);
        }
        self.gc(s);
        Probe {
            model_servers,
            sandboxes,
            status,
            process_model_files: files,
        }
    }

    fn scan_weights(&mut self, s: &Snapshot) -> BTreeMap<ProcId, Vec<String>> {
        let now = Instant::now();
        let start = now;
        let servers: HashSet<ProcId> = s
            .processes
            .iter()
            .filter(|p| classify(p).is_some())
            .map(|p| p.id)
            .collect();
        // Children of servers (runners, workers) are scanned like servers.
        let server_pids: HashSet<u32> = servers.iter().map(|id| id.pid).collect();
        let mut cands: Vec<(bool, u64, ProcId)> = s
            .processes
            .iter()
            .filter(|p| self.env.uid.is_none() || p.uid.is_none() || p.uid == self.env.uid)
            .filter_map(|p| {
                let is_server = servers.contains(&p.id) || p.ppid.is_some_and(|pp| server_pids.contains(&pp));
                let fp = p.mem.footprint_or_pss.value.or(p.mem.resident.value).unwrap_or(0);
                (is_server || fp >= self.cfg.scan_min_footprint).then_some((is_server, fp, p.id))
            })
            .collect();
        cands.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let mut scanned = 0usize;
        let mut out = BTreeMap::new();
        for (is_server, _, id) in cands {
            let ttl = if is_server {
                self.cfg.server_scan_ttl
            } else {
                self.cfg.scan_ttl
            };
            let fresh = self
                .scans
                .get(&id)
                .is_some_and(|c| now.duration_since(c.at) < ttl);
            if !fresh {
                let left = self.cfg.scan_budget.saturating_sub(start.elapsed());
                if scanned >= self.cfg.max_scans || left.is_zero() {
                    // Keep a stale result rather than nothing.
                    if let Some(c) = self.scans.get(&id).filter(|c| !c.files.is_empty()) {
                        out.insert(id, c.files.clone());
                    }
                    continue;
                }
                scanned += 1;
                let files = scan_process(id.pid, left).unwrap_or_default();
                self.scans.insert(
                    id,
                    Scan {
                        at: Instant::now(),
                        files,
                    },
                );
            }
            if let Some(c) = self.scans.get(&id).filter(|c| !c.files.is_empty()) {
                out.insert(id, c.files.clone());
            }
        }
        out
    }

    fn engines(
        &mut self,
        s: &Snapshot,
        opts: &ProbeOptions,
        sandboxes: &mut Vec<oomtop_core::Sandbox>,
        status: &mut BTreeMap<String, SourceStatus>,
    ) {
        let due = self
            .engines
            .at
            .is_none_or(|t| t.elapsed() >= self.cfg.engine_refresh);
        if due {
            let sockets = engine_sockets(s, &self.env);
            let start = Instant::now();
            let mut engines = Vec::new();
            let mut st = BTreeMap::new();
            for ep in sockets {
                let left = self.cfg.engine_budget.saturating_sub(start.elapsed());
                let key = format!("adapter.{}", ep.runtime);
                if left.is_zero() {
                    merge_status(
                        &mut st,
                        key,
                        &SourceStatus::Partial("engine budget exceeded".into()),
                    );
                    continue;
                }
                match probe_engine(
                    &ep,
                    s,
                    &self.env,
                    opts.timeout,
                    left,
                    self.cfg.engine_max_stats,
                    self.engines.rotation,
                ) {
                    Ok(e) => {
                        let s2 = if e.stats_skipped > 0 {
                            SourceStatus::Partial(format!(
                                "{} containers without fresh stats",
                                e.stats_skipped
                            ))
                        } else {
                            SourceStatus::Available
                        };
                        merge_status(&mut st, format!("adapter.{}", e.runtime), &s2);
                        engines.push(e);
                    }
                    Err(err) => merge_status(&mut st, key, &SourceStatus::Unavailable(err.to_string())),
                }
            }
            self.engines = EngineCache {
                at: Some(Instant::now()),
                engines,
                status: st,
                rotation: self.engines.rotation.wrapping_add(self.cfg.engine_max_stats),
            };
        }
        let alive: HashSet<ProcId> = s.processes.iter().map(|p| p.id).collect();
        for e in &self.engines.engines {
            let mut e = e.clone();
            for c in &mut e.containers {
                c.host_pids.retain(|id| alive.contains(id));
            }
            merge_engine(sandboxes, &e, s);
        }
        status.extend(self.engines.status.clone());
    }

    fn gc(&mut self, s: &Snapshot) {
        let alive: HashSet<ProcId> = s.processes.iter().map(|p| p.id).collect();
        self.scans.retain(|id, _| alive.contains(id));
        self.seatbelt.retain(|id, _| alive.contains(id));
        self.caches.vllm_prev.retain(|id, _| alive.contains(id));
        self.caches.llama_prev.retain(|id, _| alive.contains(id));
        if self.caches.kv.len() > 256 {
            self.caches.kv.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::{Measured, MemBreakdown, Process};

    #[test]
    fn offline_probe_reads_nothing_outside_the_snapshot() {
        let mut pr = Prober::new(ProberConfig::default(), HostEnv::default());
        let s = Snapshot {
            processes: vec![Process {
                id: ProcId::new(std::process::id(), 1),
                exe: "/x/llama-server".into(),
                cmdline: vec![
                    "llama-server".into(),
                    "-m".into(),
                    "/definitely/not/here.gguf".into(),
                ],
                mem: MemBreakdown {
                    footprint_or_pss: Measured::exact(8 << 30, "t"),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = pr.probe(
            &s,
            &ProbeOptions {
                http: false,
                ..Default::default()
            },
        );
        assert!(p.process_model_files.is_empty());
        assert_eq!(p.model_servers.len(), 1);
        assert_eq!(
            p.model_servers[0].models[0].weights_bytes.unavailable_reason(),
            Some("not probed (offline)")
        );
        assert!(pr.scans.is_empty());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn weight_scans_are_cached_and_bounded() {
        use std::os::unix::io::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.gguf");
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        f.set_len(crate::weights::MIN_WEIGHT_FILE_BYTES).unwrap();
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED);
        let me = ProcId::new(std::process::id(), 1);
        let s = Snapshot {
            processes: vec![Process {
                id: me,
                exe: "/usr/bin/python3".into(),
                cmdline: vec!["python3".into(), "app.py".into()],
                uid: None,
                mem: MemBreakdown {
                    footprint_or_pss: Measured::exact(1 << 30, "t"),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut pr = Prober::new(ProberConfig::default(), HostEnv::default());
        let opts = ProbeOptions {
            timeout: Duration::from_millis(100),
            ..Default::default()
        };
        let p1 = pr.probe(&s, &opts);
        let canon = std::fs::canonicalize(&path).unwrap().display().to_string();
        // (Parallel tests in this process may have other weight files open too.)
        assert!(
            p1.process_model_files
                .get(&me)
                .is_some_and(|f| f.contains(&canon)),
            "{p1:?}"
        );
        // Weight files make it a Generic model server.
        assert_eq!(p1.model_servers.len(), 1);
        assert_eq!(p1.model_servers[0].kind, oomtop_core::ModelServerKind::Generic);
        assert!(p1.model_servers[0]
            .models
            .iter()
            .any(|m| m.file.as_deref() == Some(canon.as_str())));
        let at = pr.scans[&me].at;
        let _p2 = pr.probe(&s, &opts);
        assert_eq!(pr.scans[&me].at, at, "second probe uses the cache");
        // Gone process → cache entry dropped.
        pr.probe(&Snapshot::default(), &opts);
        assert!(pr.scans.is_empty());
        unsafe { libc::munmap(ptr, 4096) };
    }
}

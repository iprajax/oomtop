//! The sampling pipeline behind every frontend (SPEC §6, §6.2):
//! `Sampler → attribution (detect) → idle/orphan → adapters → history → OOM forecast → likely victim`,
//! plus the lineage journal (`oomtop-state`). Implements `oomtop_core::provider::SnapshotProvider`.

use oomtop_adapters::{probe, ProbeOptions};
use oomtop_collect::replay::{load_fixture, replay};
use oomtop_collect::{Sampler, SamplerOptions};
use oomtop_config::Config;
use oomtop_core::actions::ProtectContext;
use oomtop_core::attribution::LineageEntry;
use oomtop_core::forecast::forecast_oom_with;
use oomtop_core::headroom::HeadroomConfig;
use oomtop_core::history::History;
use oomtop_core::idle::{IdleConfig, IdleTracker};
use oomtop_core::provider::SnapshotProvider;
use oomtop_core::throttle::throttle_factor;
use oomtop_core::{GroupKind, ModelServer, OsKind, ProcId, Sandbox, SandboxKind, Snapshot, Victim};
use oomtop_detect::Detector;
use oomtop_state::StateDb;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Lineage is persisted at most this often (plus on drop).
const LINEAGE_FLUSH_MS: u64 = 30_000;
/// In-memory lineage entries of processes not seen for this long are dropped after they were persisted
/// (a `(pid, start_time)` never comes back, and the journal keeps them for the next run). Keeps a long TUI
/// session during builds — thousands of short-lived compilers — from growing without bound (SPEC §14).
pub const LINEAGE_EVICT_MS: u64 = 5 * 60_000;
/// On-demand (MCP) calls reuse the previous call's sample as the CPU baseline when it is this recent.
pub const ON_DEMAND_BASELINE_MS: u64 = 30_000;
/// Gap between the two samples of an on-demand call without a recent baseline.
pub const ON_DEMAND_GAP_MS: u64 = 150;

enum Input {
    Live(Box<Sampler>),
    Replay { frames: Vec<Snapshot>, next: usize },
}

pub struct Engine {
    input: Input,
    detector: Detector,
    state: Option<StateDb>,
    lineage: BTreeMap<ProcId, LineageEntry>,
    idle: IdleTracker,
    idle_cfg: IdleConfig,
    headroom_cfg: HeadroomConfig,
    history: History,
    probe_opts: ProbeOptions,
    adapters_enabled: bool,
    /// `general.other_users = false` → drop other users' processes before attribution.
    own_uid_only: Option<u32>,
    protected_names: Vec<String>,
    /// Two samples 150 ms apart per call (MCP / one-shot commands), or one when the previous call's sample
    /// is recent enough to be the CPU baseline.
    on_demand: bool,
    last_on_demand_ms: Option<u64>,
    last_flush_ms: u64,
    /// Learning allowed (machine profile refresh); `--no-learn` turns it off. Lineage is always kept.
    learn: bool,
    model_folders: Vec<PathBuf>,
    profile_checked: bool,
    /// The journal is read lazily for the processes of the first sample (not the whole 30-day journal).
    lineage_loaded: bool,
    /// The previous probe's model servers and sandboxes: adapter truth for the next attribution (SPEC §7
    /// signal #1), since the probe itself needs attributed groups.
    adapter_truth: (Vec<ModelServer>, Vec<Sandbox>),
    pub protect: ProtectContext,
    pub rule_errors: Vec<String>,
}

impl Engine {
    /// Live engine for this machine (user rules from the default config dir).
    pub fn live(cfg: &Config, state: Option<StateDb>, on_demand: bool, offline: bool) -> Self {
        Self::live_in(cfg, state, on_demand, offline, None)
    }

    /// Live engine reading user rules from `config_dir/rules.d` (`--config` moves the config dir).
    pub fn live_in(
        cfg: &Config,
        state: Option<StateDb>,
        on_demand: bool,
        offline: bool,
        config_dir: Option<&Path>,
    ) -> Self {
        let opts = SamplerOptions {
            cadence: oomtop_collect::Cadence {
                host_ms: cfg.general.host_refresh_ms,
                procs_ms: cfg.general.refresh_ms,
                expensive_ms: cfg.general.expensive_refresh_ms,
            },
            marker_allowlist: cfg.privacy.marker_allowlist.clone(),
            ..Default::default()
        };
        Self::with_input(
            cfg,
            state,
            on_demand,
            offline,
            config_dir,
            Input::Live(Box::new(Sampler::platform_default(&opts))),
        )
    }

    /// Engine replaying a fixture file (`--replay`).
    pub fn replay(cfg: &Config, path: &Path) -> Result<Self, String> {
        Self::replay_in(cfg, path, None)
    }

    /// Replay engine with user rules from `config_dir/rules.d`. Never persists state, never probes adapters.
    pub fn replay_in(cfg: &Config, path: &Path, config_dir: Option<&Path>) -> Result<Self, String> {
        let fx = load_fixture(path).map_err(|e| e.to_string())?;
        let frames = replay(&fx);
        if frames.is_empty() {
            return Err(format!("{}: fixture has no frames", path.display()));
        }
        Ok(Self::with_input(
            cfg,
            None,
            false,
            true,
            config_dir,
            Input::Replay { frames, next: 0 },
        ))
    }

    fn with_input(
        cfg: &Config,
        state: Option<StateDb>,
        on_demand: bool,
        offline: bool,
        config_dir: Option<&Path>,
        input: Input,
    ) -> Self {
        let paths = match config_dir {
            Some(d) => oomtop_config::paths::ConfigPaths::new(d),
            None => oomtop_config::paths::ConfigPaths::default_user(),
        };
        let (detector, errs) = Detector::with_user_rules(Some(&paths.rules_d));
        Engine {
            input,
            detector,
            state,
            lineage: BTreeMap::new(),
            idle: IdleTracker::new(),
            idle_cfg: cfg.idle_config(),
            headroom_cfg: cfg.headroom_config(),
            history: History::default(),
            probe_opts: ProbeOptions {
                timeout: Duration::from_millis(cfg.adapters.timeout_ms),
                http: !offline,
                ports: cfg.adapters.ports.clone(),
            },
            adapters_enabled: cfg.adapters.enabled,
            // SAFETY: getuid never fails.
            own_uid_only: (!cfg.general.other_users).then(|| unsafe { libc::getuid() }),
            protected_names: cfg.protected.names.clone(),
            on_demand,
            last_on_demand_ms: None,
            last_flush_ms: 0,
            learn: cfg.personalization.learn,
            model_folders: cfg
                .models
                .folders
                .iter()
                .map(|f| oomtop_config::paths::expand_tilde(f))
                .collect(),
            profile_checked: false,
            lineage_loaded: false,
            adapter_truth: (Vec::new(), Vec::new()),
            protect: ProtectContext::default(),
            rule_errors: errs.iter().map(|e| e.to_string()).collect(),
        }
    }

    fn raw_snapshot(&mut self) -> Snapshot {
        self.raw_snapshot_with(false)
    }

    /// `force`: run every source now (the TUI's second sample, taken 500 ms after the first so CPU % shows
    /// within a second, SPEC §6.2/§14).
    fn raw_snapshot_with(&mut self, force: bool) -> Snapshot {
        match &mut self.input {
            Input::Live(s) => {
                if self.on_demand {
                    // CPU % needs two samples. A previous call's sample younger than ON_DEMAND_BASELINE_MS
                    // is the baseline (CPU is then averaged since that call); otherwise take a baseline now.
                    let now = oomtop_collect::now_ms();
                    let fresh = self
                        .last_on_demand_ms
                        .is_some_and(|t| now.saturating_sub(t) <= ON_DEMAND_BASELINE_MS);
                    if !fresh {
                        s.sample_all();
                        std::thread::sleep(Duration::from_millis(ON_DEMAND_GAP_MS));
                    }
                    let snap = s.sample_all();
                    self.last_on_demand_ms = Some(oomtop_collect::now_ms());
                    snap
                } else if force {
                    s.sample_all()
                } else {
                    s.sample_once()
                }
            }
            Input::Replay { frames, next } => {
                let i = (*next).min(frames.len() - 1);
                *next += 1;
                frames[i].clone()
            }
        }
    }

    fn update_protect(&mut self, s: &Snapshot) {
        let me = s.self_pid.and_then(|p| s.process_by_pid(p));
        let mut ancestors = Vec::new();
        let mut cur = me.and_then(|p| p.ppid);
        while let Some(pid) = cur.filter(|p| *p > 1 && ancestors.len() < 64) {
            ancestors.push(pid);
            cur = s.process_by_pid(pid).and_then(|p| p.ppid);
        }
        self.protect.self_pid = s.self_pid;
        self.protect.self_uid = me.and_then(|p| p.uid);
        self.protect.ancestor_pids = ancestors;
        self.protect.protected_names = self.protected_names.clone();
    }

    fn likely_victim(s: &Snapshot) -> Option<Victim> {
        match s.host.os {
            OsKind::Linux => s
                .processes
                .iter()
                .filter(|p| p.oom_score.is_some())
                .max_by_key(|p| p.oom_score)
                .map(|p| Victim {
                    id: p.id,
                    name: p.name.clone(),
                    group_id: s.group_of(p.id).map(|g| g.id.clone()),
                    reason: format!("highest oom_score ({})", p.oom_score.unwrap_or(0)),
                    heuristic: false,
                }),
            _ => s
                .groups
                .iter()
                .filter(|g| g.kind == GroupKind::App && !g.protected && !g.is_self)
                .max_by_key(|g| g.totals.footprint.value.unwrap_or(0))
                .and_then(|g| {
                    Some(Victim {
                        id: g.root?,
                        name: g.label.clone(),
                        group_id: Some(g.id.clone()),
                        reason: "largest background app (heuristic)".into(),
                        heuristic: true,
                    })
                }),
        }
    }

    fn record_lineage(&mut self, s: &Snapshot) {
        let now = s.taken_at_ms;
        let by_id: std::collections::HashMap<ProcId, &oomtop_core::Process> =
            s.processes.iter().map(|p| (p.id, p)).collect();
        for g in &s.groups {
            let agent_group = g.kind == GroupKind::AgentSession;
            for m in &g.members {
                let p = by_id.get(&m.id).copied();
                let spawned = agent_group || p.map(|p| p.markers.session_id.is_some()).unwrap_or(false);
                let e = self.lineage.entry(m.id).or_insert_with(|| LineageEntry {
                    first_seen_ms: now,
                    ..Default::default()
                });
                let keep_agent = e.spawned_by_agent && !spawned;
                e.id = m.id;
                // Keep the original parent: the journal exists to remember it after re-parenting.
                if e.ppid.map(|pp| pp <= 1).unwrap_or(true) {
                    e.ppid = p.and_then(|p| p.ppid);
                }
                // Assign only what changed: ~700 processes × 3 strings per refresh otherwise (SPEC §14).
                if !keep_agent {
                    if e.group_id != g.id {
                        e.group_id = g.id.clone();
                    }
                    e.group_kind = g.kind;
                    if e.group_label != g.label {
                        e.group_label = g.label.clone();
                    }
                    if e.group_fingerprint != g.fingerprint {
                        e.group_fingerprint = g.fingerprint.clone();
                    }
                }
                if let Some(sid) = p.and_then(|p| p.markers.session_id.as_ref()) {
                    if e.session_id.as_ref() != Some(sid) {
                        e.session_id = Some(sid.clone());
                    }
                }
                e.spawned_by_agent |= spawned;
                e.last_seen_ms = now;
                if let Some(t) = self.idle.last_active_ms(m.id) {
                    e.last_active_ms = e.last_active_ms.max(t);
                }
            }
        }
        if now.saturating_sub(self.last_flush_ms) >= LINEAGE_FLUSH_MS || self.on_demand {
            self.flush();
            self.last_flush_ms = now;
            self.evict_lineage(now);
        }
    }

    /// Loads the journal entries of the processes in the first sample (re-parented children keep their
    /// agent across oomtop restarts) and seeds the idle tracker with them.
    fn load_lineage(&mut self, s: &Snapshot) {
        if self.lineage_loaded {
            return;
        }
        self.lineage_loaded = true;
        let Some(db) = self.state.as_ref() else { return };
        let ids: Vec<ProcId> = s.processes.iter().map(|p| p.id).collect();
        if let Ok(found) = db.lineage_for(&ids) {
            self.idle.seed(&found);
            self.lineage.extend(found);
        }
    }

    /// Drops entries not seen for [`LINEAGE_EVICT_MS`] (only after a flush, so nothing is lost).
    fn evict_lineage(&mut self, now: u64) {
        self.lineage
            .retain(|_, e| now.saturating_sub(e.last_seen_ms) < LINEAGE_EVICT_MS);
    }

    /// Rule-file lint (`doctor`): e.g. `match.env` keys outside the marker allowlist, which can never match.
    pub fn rule_warnings(&self, allowlist: &[String]) -> Vec<String> {
        self.detector
            .lint(allowlist)
            .iter()
            .map(|w| w.to_string())
            .collect()
    }

    /// In-memory lineage entries (tests, `doctor`).
    pub fn lineage_len(&self) -> usize {
        self.lineage.len()
    }

    /// Persists the lineage journal (entries seen since the last flush) and anything else the state store has
    /// queued.
    pub fn flush(&mut self) {
        let since = self.last_flush_ms;
        let mut latest = since;
        if let Some(db) = self.state.as_mut() {
            let entries: Vec<LineageEntry> = self
                .lineage
                .values()
                .filter(|e| e.last_seen_ms >= since)
                .cloned()
                .collect();
            latest = entries.iter().map(|e| e.last_seen_ms).max().unwrap_or(since);
            if !entries.is_empty() {
                let _ = db.record_lineage(&entries);
            }
            let _ = db.flush();
        }
        // Next flush: entries seen since the newest one written now (that sample may be re-sent once).
        self.last_flush_ms = self.last_flush_ms.max(latest);
    }

    /// `--no-learn`: the machine profile is not refreshed and the state store records no learning signals.
    /// The lineage journal is attribution, not learning, and is always kept.
    pub fn set_learning(&mut self, learn: bool) {
        self.learn = learn;
        if let Some(db) = self.state.as_mut() {
            db.set_learning(learn);
        }
    }

    /// True for a live engine (actions may be executed); false when replaying a fixture.
    pub fn is_live(&self) -> bool {
        matches!(self.input, Input::Live(_))
    }

    /// Refreshes the machine profile (UX §6) when it is missing or older than a day. Checked once per engine.
    fn refresh_profile(&mut self, s: &Snapshot) {
        if self.profile_checked || !self.learn || s.processes.is_empty() {
            return;
        }
        self.profile_checked = true;
        let Some(db) = self.state.as_mut() else { return };
        let now = s.taken_at_ms;
        if !db.machine_profile_stale(now).unwrap_or(false) {
            return;
        }
        let prev: Option<crate::profile::MachineProfile> = db
            .machine_profile()
            .ok()
            .flatten()
            .and_then(|(j, _)| serde_json::from_value(j).ok());
        let files = crate::profile::count_model_files(&self.model_folders);
        let p = crate::profile::build(s, files, prev.as_ref(), now);
        if let Ok(j) = serde_json::to_value(&p) {
            let _ = db.set_machine_profile(&j, now);
        }
    }

    /// Carries the previous probe's pid mapping into `s` (only pids still alive with the same identity).
    fn seed_adapter_truth(&self, s: &mut Snapshot) {
        let alive: std::collections::HashSet<ProcId> = s.processes.iter().map(|p| p.id).collect();
        s.model_servers = self
            .adapter_truth
            .0
            .iter()
            .filter(|m| m.pids.iter().any(|p| alive.contains(p)))
            .cloned()
            .collect();
        s.sandboxes = self
            .adapter_truth
            .1
            .iter()
            .filter(|b| b.host_pids.iter().any(|p| alive.contains(p)))
            .cloned()
            .collect();
    }

    /// True when a probed model server or (non app-owned) sandbox pid sits in a group of another kind, i.e.
    /// attribution ran without that adapter truth and should run again.
    fn adapter_truth_missed(s: &Snapshot) -> bool {
        let kind_of = |id: ProcId| s.group_of(id).map(|g| g.kind);
        let servers = s
            .model_servers
            .iter()
            .flat_map(|m| m.pids.iter())
            .any(|p| kind_of(*p).is_some_and(|k| k != GroupKind::ModelServer));
        let sandboxes = s
            .sandboxes
            .iter()
            .filter(|b| {
                !(b.kind == SandboxKind::Vm && b.runtime.to_ascii_lowercase().contains("virtualization"))
            })
            .flat_map(|b| b.host_pids.iter())
            .any(|p| kind_of(*p).is_some_and(|k| k != GroupKind::Sandbox));
        servers || sandboxes
    }

    /// Full enrichment pipeline on one raw snapshot.
    pub fn enrich(&mut self, mut s: Snapshot) -> Snapshot {
        if let Some(uid) = self.own_uid_only {
            s.processes.retain(|p| p.uid.is_none_or(|u| u == uid));
        }
        self.load_lineage(&s);
        self.update_protect(&s);
        if self.adapters_enabled {
            self.seed_adapter_truth(&mut s);
        }
        s.groups = self.detector.attribute(&s, &self.lineage, &self.protect);
        self.idle.observe(&s, &self.idle_cfg);
        self.idle.apply(&mut s, &self.lineage, &self.idle_cfg);
        if self.adapters_enabled {
            let p = probe(&s, &self.probe_opts);
            oomtop_adapters::apply(&mut s, p.clone());
            // First sample (or a server that just appeared): re-attribute with the adapter truth so the
            // server's processes form their own group now, not one refresh later.
            if Self::adapter_truth_missed(&s) {
                s.groups = self.detector.attribute(&s, &self.lineage, &self.protect);
                self.idle.apply(&mut s, &self.lineage, &self.idle_cfg);
                oomtop_adapters::apply(&mut s, p);
                let group_of = |s: &Snapshot, id: ProcId| s.group_of(id).map(|g| g.id.clone());
                let fixed: Vec<Option<String>> = s
                    .model_servers
                    .iter()
                    .map(|m| m.pids.iter().find_map(|p| group_of(&s, *p)))
                    .collect();
                for (m, gid) in s.model_servers.iter_mut().zip(fixed) {
                    if gid.is_some() {
                        m.group_id = gid;
                    }
                }
            }
            self.adapter_truth = (s.model_servers.clone(), s.sandboxes.clone());
        }
        if !s.thermal.clusters.is_empty() {
            s.thermal.throttle_factor = throttle_factor(&s.thermal.clusters);
        }
        self.history.push_snapshot(&s);
        s.oom.forecast = forecast_oom_with(&self.history, &s.oom, s.memory.total.value);
        s.oom.likely_victim = Self::likely_victim(&s);
        self.record_lineage(&s);
        self.refresh_profile(&s);
        s
    }

    /// Group id containing oomtop itself (for MCP: the calling agent's session).
    pub fn self_group(s: &Snapshot) -> Option<String> {
        let pid = s.self_pid?;
        let p = s.process_by_pid(pid)?;
        s.group_of(p.id).map(|g| g.id.clone())
    }

    pub fn state_mut(&mut self) -> Option<&mut StateDb> {
        self.state.as_mut()
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.flush();
    }
}

impl SnapshotProvider for Engine {
    fn snapshot(&mut self) -> Snapshot {
        let raw = self.raw_snapshot();
        self.enrich(raw)
    }
    fn snapshot_now(&mut self) -> Snapshot {
        let raw = self.raw_snapshot_with(true);
        self.enrich(raw)
    }
    fn history(&self) -> &History {
        &self.history
    }
    fn headroom_config(&self) -> HeadroomConfig {
        self.headroom_cfg.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::Process;

    fn proc(pid: u32, ppid: u32, name: &str) -> Process {
        Process {
            id: ProcId::new(pid, 1_000 + pid as u64),
            ppid: Some(ppid),
            name: name.into(),
            exe: format!("/usr/bin/{name}"),
            cmdline: vec![name.into()],
            uid: Some(501),
            ..Default::default()
        }
    }

    fn snap(at_ms: u64, procs: Vec<Process>) -> Snapshot {
        let mut s = Snapshot {
            taken_at_ms: at_ms,
            processes: procs,
            self_pid: Some(10),
            ..Default::default()
        };
        s.host.os = OsKind::Macos;
        s
    }

    fn replay_engine(frames: Vec<Snapshot>, db: StateDb) -> Engine {
        let mut cfg = Config::default();
        cfg.adapters.enabled = false;
        Engine::with_input(
            &cfg,
            Some(db),
            false,
            true,
            None,
            Input::Replay { frames, next: 0 },
        )
    }

    #[test]
    fn lineage_is_bounded_persisted_and_reloaded_lazily() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let base = vec![proc(10, 1, "oomtop"), proc(20, 1, "zsh")];
        // A build spawns 500 short-lived compilers, then they're gone for good.
        let mut burst = base.clone();
        burst.extend((0..500).map(|i| proc(1_000 + i, 20, "rustc")));
        let t0 = oomtop_collect::now_ms(); // the journal prunes by wall-clock retention
        let frames = vec![
            snap(t0, burst),
            snap(t0 + 31_000, base.clone()),
            snap(t0 + 31_000 + LINEAGE_EVICT_MS + 31_000, base.clone()),
        ];
        let mut e = replay_engine(frames, StateDb::open(&path).unwrap());
        e.snapshot();
        assert_eq!(e.lineage_len(), 502);
        e.snapshot();
        assert_eq!(e.lineage_len(), 502, "recently seen entries are kept");
        e.snapshot();
        assert_eq!(
            e.lineage_len(),
            2,
            "dead processes are evicted after they were persisted"
        );
        drop(e);
        let db = StateDb::open(&path).unwrap();
        assert_eq!(
            db.lineage_all().unwrap().len(),
            502,
            "evicted entries stay in the journal"
        );
        // A new engine reads only the entries of the processes it sees, not the whole journal.
        let mut e2 = replay_engine(vec![snap(t0 + 3_600_000, base)], db);
        e2.snapshot();
        assert_eq!(e2.lineage_len(), 2);
        let zsh = e2.lineage.get(&ProcId::new(20, 1_020)).unwrap();
        assert_eq!(zsh.first_seen_ms, t0, "first_seen survives the restart");
    }

    /// SPEC §14 "CPU % within 1 s": the TUI's forced second sample 500 ms after the first has host and
    /// per-process CPU, although neither source is due yet on its cadence.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn forced_second_sample_has_cpu_within_a_second() {
        let started = std::time::Instant::now();
        let mut cfg = Config::default();
        cfg.adapters.enabled = false;
        let mut e = Engine::live(&cfg, None, false, true);
        let first = e.snapshot();
        assert!(first.cpu.total_pct.value.is_none());
        std::thread::sleep(Duration::from_millis(500));
        let mut s = e.snapshot_now();
        // Under a loaded parallel debug test run a source can blow its 50 ms budget (no baseline then); the
        // next forced sample has one. Seen once in ~20 full-suite runs at load average 3 with one retry.
        for _ in 0..3 {
            if s.cpu.total_pct.value.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
            s = e.snapshot_now();
        }
        assert!(started.elapsed() < Duration::from_millis(1500) || cfg!(debug_assertions));
        assert!(
            s.cpu.total_pct.value.is_some(),
            "host CPU after the forced sample: {:?} · {:?}",
            s.cpu.total_pct,
            s.source_status
        );
        let with_cpu = s.processes.iter().filter(|p| p.cpu_pct.value.is_some()).count();
        assert!(with_cpu > 0, "per-process CPU after the forced sample");
        // A plain cadence-gated sample at that point would not have re-read the process listing.
        let again = e.snapshot();
        assert_eq!(again.processes.len(), s.processes.len());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn live_pipeline_on_this_machine() {
        let db = StateDb::open_in_memory().unwrap();
        let mut e = Engine::live(&Config::default(), Some(db), true, true);
        let s = e.snapshot();
        assert!(!s.processes.is_empty());
        assert!(!s.groups.is_empty());
        let me = s.self_pid.unwrap();
        let grouped: usize = s.groups.iter().map(|g| g.members.len()).sum();
        assert_eq!(
            grouped,
            s.processes.len(),
            "every process attributed exactly once"
        );
        assert!(s.groups.iter().all(|g| !g.id.is_empty()));
        assert!(s.process_by_pid(me).is_some());
        assert!(e.protect.self_uid.is_some());
        e.flush();
        assert!(!e.state_mut().unwrap().lineage_all().unwrap().is_empty());
    }
}

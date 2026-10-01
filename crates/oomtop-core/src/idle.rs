//! Idle and orphan detection (SPEC §7).
//!
//! - **Idle:** CPU < 0.5 % of one core and no disk I/O (rate or cumulative-byte growth) for ≥ 30 min
//!   (configurable). A group is idle when *every* member is.
//! - **Orphan:** a group whose root was spawned by an agent session or tool (session marker, lineage journal,
//!   or an agent-owned `owner_group`), that is now **detached** (its parent exited / it was re-parented to
//!   launchd/init/systemd, and the group that spawned it is gone), and that is older than 10 min
//!   (configurable). Attribution makes such processes their own roots so they can be stopped individually.
//! - Without journal data, `idle_for` is "≥ observed window" (quality `Estimate`); with journal data it is
//!   "since last activity" (also `Estimate`: activity is sampled).
//! - A process whose CPU usage was never measurable (other users' processes, a first sample without a
//!   delta) is never called idle: `idle_for` is `unavailable("no CPU data")`, so its group is not idle.

use crate::attribution::{LineageEntry, SELF_GROUP_ID};
use crate::model::{Group, GroupKind, Measured, ProcId, Process, Quality, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IdleConfig {
    /// Per-core CPU % below which a process counts as inactive (default 0.5).
    pub cpu_pct_threshold: f64,
    /// Idle after this many seconds of inactivity (default 1800).
    pub idle_after_s: u64,
    /// Orphans must be at least this old (default 600).
    pub orphan_age_s: u64,
}

impl Default for IdleConfig {
    fn default() -> Self {
        IdleConfig {
            cpu_pct_threshold: 0.5,
            idle_after_s: 1800,
            orphan_age_s: 600,
        }
    }
}

/// Source label for idle times measured from observed activity.
pub const SOURCE_LAST_ACTIVE: &str = "since last activity (sampled)";
/// Source label for idle times that are only a lower bound.
pub const SOURCE_OBSERVED_WINDOW: &str = "≥ observed window";

/// Tracks last-activity times across samples (pure state; persisted via the lineage journal).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IdleTracker {
    /// When this tracker first observed the machine.
    pub observed_since_ms: Option<u64>,
    last_active: HashMap<ProcId, u64>,
    first_seen: HashMap<ProcId, u64>,
    /// Cumulative (read, write) bytes at the last observation, for I/O detection without rates.
    last_io: HashMap<ProcId, (Option<u64>, Option<u64>)>,
    /// Processes with at least one measurable CPU sample (or journal activity).
    cpu_known: HashSet<ProcId>,
}

fn grew(prev: Option<u64>, now: Option<u64>) -> bool {
    matches!((prev, now), (Some(a), Some(b)) if b > a)
}

impl IdleTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds last-active / first-seen times from the lineage journal (keeps the later activity time).
    pub fn seed(&mut self, lineage: &BTreeMap<ProcId, LineageEntry>) {
        for (id, e) in lineage {
            if e.last_active_ms > 0 {
                let v = self.last_active.entry(*id).or_insert(e.last_active_ms);
                *v = (*v).max(e.last_active_ms);
                self.cpu_known.insert(*id);
            }
            if e.first_seen_ms > 0 {
                let v = self.first_seen.entry(*id).or_insert(e.first_seen_ms);
                *v = (*v).min(e.first_seen_ms);
            }
        }
    }

    /// True if the process shows CPU or disk activity in this sample.
    fn active(&self, p: &Process, cfg: &IdleConfig) -> bool {
        let cpu = p
            .cpu_pct
            .value
            .filter(|_| p.cpu_pct.quality.is_available())
            .map(|c| c >= cfg.cpu_pct_threshold)
            .unwrap_or(false);
        let rate =
            p.disk_io.read_rate.value.unwrap_or(0.0) > 0.0 || p.disk_io.write_rate.value.unwrap_or(0.0) > 0.0;
        let bytes = self
            .last_io
            .get(&p.id)
            .map(|(r, w)| grew(*r, p.disk_io.read_bytes.value) || grew(*w, p.disk_io.write_bytes.value))
            .unwrap_or(false);
        cpu || rate || bytes
    }

    /// Records activity from a snapshot and forgets processes that disappeared.
    pub fn observe(&mut self, s: &Snapshot, cfg: &IdleConfig) {
        let now = s.taken_at_ms;
        self.observed_since_ms.get_or_insert(now);
        let mut alive = HashSet::with_capacity(s.processes.len());
        for p in &s.processes {
            alive.insert(p.id);
            let start = p.id.start_time;
            // a process cannot have been seen before it started
            let seen = self.first_seen.entry(p.id).or_insert(now);
            if start > 0 && *seen < start {
                *seen = start;
            }
            if p.cpu_pct.value.is_some() && p.cpu_pct.quality.is_available() {
                self.cpu_known.insert(p.id);
            }
            if self.active(p, cfg) {
                self.last_active.insert(p.id, now);
            }
            self.last_io
                .insert(p.id, (p.disk_io.read_bytes.value, p.disk_io.write_bytes.value));
        }
        self.last_active.retain(|id, _| alive.contains(id));
        self.first_seen.retain(|id, _| alive.contains(id));
        self.last_io.retain(|id, _| alive.contains(id));
        self.cpu_known.retain(|id| alive.contains(id));
    }

    pub fn last_active_ms(&self, id: ProcId) -> Option<u64> {
        self.last_active.get(&id).copied()
    }

    /// Idle duration: from the last observed activity; otherwise a lower bound since first seen.
    pub fn idle_for(&self, id: ProcId, now_ms: u64) -> Measured<u64> {
        if !self.cpu_known.contains(&id) {
            let seen = self.first_seen.contains_key(&id) || self.last_active.contains_key(&id);
            return Measured::unavailable(
                "idle tracker",
                if seen { "no CPU data" } else { "not observed yet" },
            );
        }
        if let Some(t) = self.last_active.get(&id) {
            return Measured::estimate(now_ms.saturating_sub(*t) / 1000, SOURCE_LAST_ACTIVE);
        }
        match self.first_seen.get(&id).copied().or(self.observed_since_ms) {
            Some(t) => Measured {
                value: Some(now_ms.saturating_sub(t) / 1000),
                source: SOURCE_OBSERVED_WINDOW.into(),
                quality: Quality::Estimate,
            },
            None => Measured::unavailable("idle tracker", "not observed yet"),
        }
    }

    /// Fills `Process::idle_for_s` and `Group::{idle, idle_for_s, orphan}`.
    pub fn apply(&self, s: &mut Snapshot, lineage: &BTreeMap<ProcId, LineageEntry>, cfg: &IdleConfig) {
        let now = s.taken_at_ms;
        for p in &mut s.processes {
            p.idle_for_s = self.idle_for(p.id, now);
        }
        let ctx = OrphanCtx {
            now_ms: now,
            procs: s.processes.iter().map(|p| (p.id, p)).collect(),
            by_pid: newest_by_pid(&s.processes),
            kinds: s.groups.iter().map(|g| (g.id.as_str(), g.kind)).collect(),
            lineage,
            cfg,
        };
        let procs = &ctx.procs;
        let mut updates: Vec<(Option<u64>, bool, bool)> = Vec::with_capacity(s.groups.len());
        for g in &s.groups {
            let idle_times: Vec<u64> = g
                .members
                .iter()
                .filter_map(|m| procs.get(&m.id))
                .filter_map(|p| p.idle_for_s.value)
                .collect();
            let all_known = !idle_times.is_empty() && idle_times.len() == g.members.len();
            let min_idle = idle_times.iter().copied().min().filter(|_| all_known);
            let idle = min_idle.map(|m| m >= cfg.idle_after_s).unwrap_or(false);
            let orphan = ctx.is_orphan(g);
            updates.push((min_idle, idle, orphan));
        }
        for (g, (idle_for, idle, orphan)) in s.groups.iter_mut().zip(updates) {
            g.idle_for_s = idle_for;
            g.idle = idle;
            // re-decided here (attribution's flag is preliminary: it cannot apply the age rule)
            g.orphan = orphan;
        }
    }
}

/// pid → the newest process with that pid (a listing may briefly hold an exited and a reused entry).
fn newest_by_pid(ps: &[Process]) -> HashMap<u32, &Process> {
    let mut out: HashMap<u32, &Process> = HashMap::with_capacity(ps.len());
    for p in ps {
        match out.get(&p.id.pid) {
            Some(q) if q.id.start_time >= p.id.start_time => {}
            _ => {
                out.insert(p.id.pid, p);
            }
        }
    }
    out
}

/// Everything the orphan rule looks at besides the group itself.
struct OrphanCtx<'a> {
    now_ms: u64,
    procs: HashMap<ProcId, &'a Process>,
    by_pid: HashMap<u32, &'a Process>,
    /// Live group ids and their kinds.
    kinds: HashMap<&'a str, GroupKind>,
    lineage: &'a BTreeMap<ProcId, LineageEntry>,
    cfg: &'a IdleConfig,
}

impl OrphanCtx<'_> {
    fn is_agent_group_id(&self, id: &str) -> bool {
        self.kinds
            .get(id)
            .map(|k| *k == GroupKind::AgentSession)
            .unwrap_or(false)
            || id.starts_with("agent:")
    }

    fn is_orphan(&self, g: &Group) -> bool {
        if g.protected || g.is_self || g.id == SELF_GROUP_ID || g.kind == GroupKind::System {
            return false;
        }
        let root = g.root.and_then(|r| self.procs.get(&r).copied()).or_else(|| {
            g.members
                .iter()
                .filter_map(|m| self.procs.get(&m.id).copied())
                .min_by_key(|p| p.id.start_time)
        });
        let Some(root) = root else {
            return false;
        };
        // spawned by an agent session or tool?
        let marker = root.markers.session_id.is_some() || root.markers.agent.is_some();
        let journal = self
            .lineage
            .get(&root.id)
            .map(|e| e.spawned_by_agent)
            .unwrap_or(false);
        let owned_by_agent = g
            .owner_group
            .as_deref()
            .map(|o| self.is_agent_group_id(o))
            .unwrap_or(false);
        if !(marker || journal || owned_by_agent) {
            return false;
        }
        // detached: parent gone / re-parented to a reaper, and the spawning group is gone
        let parent_gone = match root.ppid {
            None | Some(0) | Some(1) => true,
            Some(pp) => match self.by_pid.get(&pp) {
                None => true,
                Some(parent) => {
                    parent.id.start_time > root.id.start_time
                        || matches!(parent.name.as_str(), "launchd" | "systemd" | "init")
                }
            },
        };
        let owner_gone = g
            .owner_group
            .as_deref()
            .map(|o| o == g.id || !self.kinds.contains_key(o))
            .unwrap_or(true);
        if !(parent_gone && owner_gone) {
            return false;
        }
        // age from the start time; else from the journal's first sighting (0 = unknown → not old enough)
        let since = if root.id.start_time > 0 {
            Some(root.id.start_time)
        } else {
            self.lineage
                .get(&root.id)
                .map(|e| e.first_seen_ms)
                .filter(|&t| t > 0)
        };
        let age_s = since.map(|t| self.now_ms.saturating_sub(t) / 1000).unwrap_or(0);
        age_s >= self.cfg.orphan_age_s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Group, Member};

    fn group(id: &str, kind: GroupKind, root: ProcId) -> Group {
        Group {
            id: id.into(),
            kind,
            root: Some(root),
            members: vec![Member {
                id: root,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn idle_needs_the_whole_window() {
        let cfg = IdleConfig {
            idle_after_s: 60,
            ..Default::default()
        };
        let id = ProcId::new(7, 1);
        let mut s = Snapshot {
            taken_at_ms: 1_000_000,
            ..Default::default()
        };
        s.processes.push(Process {
            id,
            ppid: Some(50),
            cpu_pct: Measured::exact(5.0, "t"),
            ..Default::default()
        });
        s.groups.push(group("other:x:7", GroupKind::Other, id));
        let mut t = IdleTracker::new();
        t.observe(&s, &cfg);
        s.taken_at_ms += 30_000;
        s.processes[0].cpu_pct = Measured::exact(0.1, "t");
        t.observe(&s, &cfg);
        t.apply(&mut s, &BTreeMap::new(), &cfg);
        assert_eq!(s.processes[0].idle_for_s.value, Some(30));
        assert_eq!(s.processes[0].idle_for_s.source, SOURCE_LAST_ACTIVE);
        assert!(!s.groups[0].idle);
        s.taken_at_ms += 60_000;
        t.observe(&s, &cfg);
        t.apply(&mut s, &BTreeMap::new(), &cfg);
        assert!(s.groups[0].idle);
        assert_eq!(s.groups[0].idle_for_s, Some(90));
        // disk activity (cumulative bytes growing) resets idleness
        s.processes[0].disk_io.write_bytes = Measured::exact(10, "t");
        t.observe(&s, &cfg);
        s.taken_at_ms += 1000;
        s.processes[0].disk_io.write_bytes = Measured::exact(20, "t");
        t.observe(&s, &cfg);
        t.apply(&mut s, &BTreeMap::new(), &cfg);
        assert_eq!(s.processes[0].idle_for_s.value, Some(0));
        assert!(!s.groups[0].idle);
    }

    #[test]
    fn without_history_idle_is_a_lower_bound() {
        let cfg = IdleConfig::default();
        let id = ProcId::new(9, 0);
        let mut s = Snapshot {
            taken_at_ms: 5_000,
            ..Default::default()
        };
        s.processes.push(Process {
            id,
            cpu_pct: Measured::exact(0.1, "t"),
            ..Default::default()
        });
        let mut t = IdleTracker::new();
        t.observe(&s, &cfg);
        s.taken_at_ms = 65_000;
        t.observe(&s, &cfg);
        let m = t.idle_for(id, s.taken_at_ms);
        assert_eq!(m.value, Some(60));
        assert_eq!(m.quality, Quality::Estimate);
        assert_eq!(m.source, SOURCE_OBSERVED_WINDOW);
        // seeded journal extends the window
        let mut lineage = BTreeMap::new();
        lineage.insert(
            id,
            LineageEntry {
                id,
                first_seen_ms: 1,
                ..Default::default()
            },
        );
        let mut t2 = IdleTracker::new();
        t2.seed(&lineage);
        t2.observe(&s, &cfg);
        assert_eq!(t2.idle_for(id, s.taken_at_ms).value, Some(64));
    }

    #[test]
    fn unmeasurable_cpu_is_never_idle() {
        let cfg = IdleConfig {
            idle_after_s: 60,
            ..Default::default()
        };
        let id = ProcId::new(11, 1);
        let mut s = Snapshot {
            taken_at_ms: 1_000_000,
            ..Default::default()
        };
        s.processes.push(Process {
            id,
            cpu_pct: Measured::unavailable("proc_pid_rusage", "other user"),
            ..Default::default()
        });
        s.groups.push(group("other:x:11", GroupKind::Other, id));
        let mut t = IdleTracker::new();
        assert_eq!(
            t.idle_for(id, s.taken_at_ms).unavailable_reason(),
            Some("not observed yet")
        );
        t.observe(&s, &cfg);
        s.taken_at_ms += 3_600_000;
        t.observe(&s, &cfg);
        t.apply(&mut s, &BTreeMap::new(), &cfg);
        assert_eq!(
            s.processes[0].idle_for_s.unavailable_reason(),
            Some("no CPU data")
        );
        assert!(!s.groups[0].idle, "an hour without CPU data is not an idle hour");
        assert_eq!(s.groups[0].idle_for_s, None);
        // once CPU becomes measurable the lower bound starts counting
        s.processes[0].cpu_pct = Measured::exact(0.0, "t");
        t.observe(&s, &cfg);
        assert!(t.idle_for(id, s.taken_at_ms).value.is_some());
    }

    #[test]
    fn unknown_age_is_not_old() {
        // start time unknown (0) and a journal entry without first_seen: age is unknown → not an orphan
        let cfg = IdleConfig::default();
        let now = 10_000_000u64;
        let id = ProcId::new(7, 0);
        let mut s = Snapshot {
            taken_at_ms: now,
            ..Default::default()
        };
        let mut p = Process {
            id,
            ppid: Some(1),
            ..Default::default()
        };
        p.markers.session_id = Some("abc".into());
        s.processes.push(p);
        s.groups.push(group("other:next:7", GroupKind::Other, id));
        let mut lineage = BTreeMap::new();
        lineage.insert(
            id,
            LineageEntry {
                id,
                spawned_by_agent: true,
                ..Default::default()
            },
        );
        IdleTracker::new().apply(&mut s, &lineage, &cfg);
        assert!(!s.groups[0].orphan);
        // with a journal sighting an hour ago it is
        lineage.get_mut(&id).unwrap().first_seen_ms = now - 3_600_000;
        IdleTracker::new().apply(&mut s, &lineage, &cfg);
        assert!(s.groups[0].orphan);
    }

    #[test]
    fn orphans_need_agent_origin_detachment_and_age() {
        let cfg = IdleConfig::default();
        let now = 10_000_000u64;
        let id = ProcId::new(7, now - 3_600_000);
        let mut s = Snapshot {
            taken_at_ms: now,
            ..Default::default()
        };
        let mut p = Process {
            id,
            ppid: Some(1),
            ..Default::default()
        };
        p.markers.session_id = Some("abc".into());
        s.processes.push(p);
        let mut g = group("other:next:7", GroupKind::Other, id);
        g.owner_group = Some("agent:abc".into());
        s.groups.push(g);
        let t = IdleTracker::new();
        t.apply(&mut s, &BTreeMap::new(), &cfg);
        assert!(s.groups[0].orphan, "session gone + marker + re-parented + old");

        // while the owning session is alive it is not an orphan
        let mut s2 = s.clone();
        s2.groups[0].orphan = false;
        s2.groups.push(Group {
            id: "agent:abc".into(),
            kind: GroupKind::AgentSession,
            ..Default::default()
        });
        t.apply(&mut s2, &BTreeMap::new(), &cfg);
        assert!(!s2.groups[0].orphan);

        // too young: a preliminary flag from attribution is cleared
        let mut s3 = s.clone();
        assert!(s3.groups[0].orphan);
        s3.processes[0].id.start_time = now - 60_000;
        s3.groups[0].root = Some(s3.processes[0].id);
        s3.groups[0].members[0].id = s3.processes[0].id;
        t.apply(&mut s3, &BTreeMap::new(), &cfg);
        assert!(!s3.groups[0].orphan);

        // not agent-spawned: a user daemon re-parented to launchd is not an orphan
        let mut s4 = s.clone();
        s4.groups[0].orphan = false;
        s4.groups[0].owner_group = None;
        s4.processes[0].markers = Default::default();
        t.apply(&mut s4, &BTreeMap::new(), &cfg);
        assert!(!s4.groups[0].orphan);
        // …unless the journal says an agent spawned it
        let mut lineage = BTreeMap::new();
        lineage.insert(
            id,
            LineageEntry {
                id,
                spawned_by_agent: true,
                ..Default::default()
            },
        );
        t.apply(&mut s4, &lineage, &cfg);
        assert!(s4.groups[0].orphan);
    }
}

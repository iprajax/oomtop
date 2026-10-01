//! In-memory history ring buffer (SPEC §6.2): last 10 minutes, compact, for sparklines, trends and the
//! OOM forecast. Pure data structure; the sampler owner pushes one point per host sample.

use crate::model::Snapshot;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Default retention: 10 minutes.
pub const DEFAULT_RETENTION_MS: u64 = 10 * 60 * 1000;
/// Per-process entries kept per point (top N by footprint). 50 × 24 B per point keeps the 10-minute ring
/// small (SPEC §14 RSS budget); only the biggest processes matter for trends.
pub const TOP_PROCS: usize = 50;

/// Stable 64-bit key of a group id (FNV-1a). History stores keys, not a `String` per group per point: a
/// 10-minute ring of ~400 groups would otherwise hold ~120k small heap strings (SPEC §14).
pub fn group_key(id: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Compact per-process sample: pid, footprint in KiB, CPU in permille of one core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProcPoint {
    pub pid: u32,
    pub start_time: u64,
    pub footprint_kib: u32,
    pub cpu_permille: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HistoryPoint {
    pub t_ms: u64,
    pub available: Option<u64>,
    pub swap_used: Option<u64>,
    pub swap_total: Option<u64>,
    /// Swap ceiling where swap grows on demand (macOS, [`crate::model::HostMemory::swap_limit`]).
    pub swap_limit: Option<u64>,
    pub psi_some_avg10: Option<f64>,
    pub cpu_total_pct: Option<f64>,
    /// Per group: (group id, footprint bytes). Accepted on [`History::push`] and converted into
    /// [`HistoryPoint::group_keys`] (then cleared); read group trends with [`History::group_series`].
    pub groups: Vec<(String, u64)>,
    /// Per group: ([`group_key`] of the id, footprint bytes), sorted by key.
    pub group_keys: Vec<(u64, u64)>,
    pub procs: Vec<ProcPoint>,
}

impl HistoryPoint {
    pub fn from_snapshot(s: &Snapshot) -> Self {
        let mut procs: Vec<ProcPoint> = s
            .processes
            .iter()
            .map(|p| ProcPoint {
                pid: p.id.pid,
                start_time: p.id.start_time,
                footprint_kib: (p.mem.footprint_or_pss.value.unwrap_or(0) / 1024).min(u32::MAX as u64) as u32,
                cpu_permille: (p.cpu_pct.value.unwrap_or(0.0) * 10.0).clamp(0.0, u16::MAX as f64) as u16,
            })
            .collect();
        procs.sort_by_key(|p| std::cmp::Reverse(p.footprint_kib));
        procs.truncate(TOP_PROCS);
        HistoryPoint {
            t_ms: s.taken_at_ms,
            available: s.memory.available.value,
            swap_used: s.memory.swap_used.value,
            swap_total: s.memory.swap_total.value,
            swap_limit: s.memory.swap_limit.value,
            psi_some_avg10: s.memory.psi.value.map(|p| p.some_avg10),
            cpu_total_pct: s.cpu.total_pct.value,
            groups: Vec::new(),
            group_keys: sorted_keys(
                s.groups
                    .iter()
                    .filter_map(|g| Some((group_key(&g.id), g.totals.footprint.value?)))
                    .collect(),
            ),
            procs,
        }
    }
}

fn sorted_keys(mut v: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    v.sort_unstable_by_key(|(k, _)| *k);
    v.dedup_by_key(|(k, _)| *k);
    v.shrink_to_fit();
    v
}

/// Time-bounded ring buffer of [`HistoryPoint`]s, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct History {
    pub retention_ms: u64,
    points: VecDeque<HistoryPoint>,
}

impl Default for History {
    fn default() -> Self {
        History::new(DEFAULT_RETENTION_MS)
    }
}

impl History {
    pub fn new(retention_ms: u64) -> Self {
        History {
            retention_ms,
            points: VecDeque::new(),
        }
    }

    /// Appends a point (out-of-order points are dropped) and evicts points older than the retention.
    pub fn push(&mut self, mut p: HistoryPoint) {
        if let Some(last) = self.points.back() {
            if p.t_ms < last.t_ms {
                return;
            }
        }
        if !p.groups.is_empty() {
            let mut keys = std::mem::take(&mut p.group_keys);
            keys.extend(p.groups.drain(..).map(|(id, v)| (group_key(&id), v)));
            p.group_keys = sorted_keys(keys);
            p.groups = Vec::new();
        }
        p.procs.truncate(TOP_PROCS);
        p.procs.shrink_to_fit();
        let cutoff = p.t_ms.saturating_sub(self.retention_ms);
        self.points.push_back(p);
        while self.points.front().map(|f| f.t_ms < cutoff).unwrap_or(false) {
            self.points.pop_front();
        }
    }

    pub fn push_snapshot(&mut self, s: &Snapshot) {
        self.push(HistoryPoint::from_snapshot(s));
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &HistoryPoint> {
        self.points.iter()
    }

    pub fn last(&self) -> Option<&HistoryPoint> {
        self.points.back()
    }

    /// Points within the last `window_ms` of the newest point.
    pub fn window(&self, window_ms: u64) -> Vec<&HistoryPoint> {
        let Some(last) = self.points.back() else {
            return Vec::new();
        };
        let cutoff = last.t_ms.saturating_sub(window_ms);
        self.points.iter().filter(|p| p.t_ms >= cutoff).collect()
    }

    /// `(t_seconds_relative_to_first, value)` series for a field.
    pub fn series(&self, window_ms: u64, f: impl Fn(&HistoryPoint) -> Option<f64>) -> Vec<(f64, f64)> {
        let w = self.window(window_ms);
        let Some(t0) = w.first().map(|p| p.t_ms) else {
            return Vec::new();
        };
        w.iter()
            .filter_map(|p| Some(((p.t_ms - t0) as f64 / 1000.0, f(p)?)))
            .collect()
    }

    /// Footprint series for one group (for sparklines).
    pub fn group_series(&self, group_id: &str) -> Vec<u64> {
        let key = group_key(group_id);
        self.points
            .iter()
            .map(|p| {
                p.group_keys
                    .binary_search_by_key(&key, |(k, _)| *k)
                    .map(|i| p.group_keys[i].1)
                    .unwrap_or(0)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_old_points() {
        let mut h = History::new(10_000);
        for t in 0..30 {
            h.push(HistoryPoint {
                t_ms: t * 1000,
                available: Some(t),
                ..Default::default()
            });
        }
        assert_eq!(h.len(), 11);
        assert_eq!(h.iter().next().unwrap().t_ms, 19_000);
        let s = h.series(5_000, |p| p.available.map(|v| v as f64));
        assert_eq!(s.len(), 6);
        assert_eq!(s[0], (0.0, 24.0));
    }

    #[test]
    fn group_series_from_ids_or_snapshots() {
        let mut h = History::default();
        for t in 0..5u64 {
            h.push(HistoryPoint {
                t_ms: t * 1000,
                groups: vec![("model:sd".into(), 10 + t), ("app:chrome".into(), 5)],
                ..Default::default()
            });
        }
        assert_eq!(h.group_series("model:sd"), vec![10, 11, 12, 13, 14]);
        assert_eq!(h.group_series("app:chrome"), vec![5; 5]);
        assert_eq!(h.group_series("nope"), vec![0; 5]);
        assert!(h.iter().all(|p| p.groups.is_empty() && p.group_keys.len() == 2));
    }

    #[test]
    fn a_full_ring_stays_small() {
        // 10 min at 2 s with 400 groups and 700 processes: keys + top-50 procs only.
        let mut s = Snapshot::default();
        for i in 0..400u32 {
            s.groups.push(crate::model::Group {
                id: format!("other:proc-{i}"),
                totals: crate::model::GroupTotals {
                    footprint: crate::model::Measured::exact(i as u64 * 1000, "t"),
                    ..Default::default()
                },
                ..Default::default()
            });
        }
        for i in 0..700u32 {
            s.processes.push(crate::model::Process {
                id: crate::model::ProcId::new(i, 1),
                ..Default::default()
            });
        }
        let mut h = History::default();
        for t in 0..300u64 {
            s.taken_at_ms = t * 2000;
            h.push_snapshot(&s);
        }
        let bytes: usize = h
            .iter()
            .map(|p| p.group_keys.capacity() * 16 + p.procs.capacity() * std::mem::size_of::<ProcPoint>())
            .sum();
        assert!(bytes < 3 * 1024 * 1024, "{bytes} bytes");
    }
}

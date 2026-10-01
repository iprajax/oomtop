//! Timeline (UX §7 "Timeline"): a 10-minute ring buffer of compact frames — host numbers plus the ranked list
//! as it was served — with insight markers ("swap +2 GB", "throttle start", "Gradle started"). Scrubbing picks
//! a frame and the Timeline view shows the ranked list as it was. Pure data; fed by `App::update`.
//!
//! Insight generators (UX §5.3) compare consecutive snapshots; each insight becomes a marker, and the ones
//! about "your things" or with high severity also become a toast (max one per 30 s, never steals focus).

use oomtop_core::{GroupKind, PressureLevel, Snapshot};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Retention of the timeline (matches the core history ring buffer).
pub const RETENTION_MS: u64 = 10 * 60 * 1000;
/// Rows kept per frame (the ranked list as served).
pub const ROWS_PER_FRAME: usize = 30;
/// Minimum gap between toasts.
pub const TOAST_GAP_MS: u64 = 30_000;
/// A new group bigger than this is an insight.
pub const LARGE_NEW_GROUP: u64 = 1 << 30;
/// Swap growth over the window that is an insight.
pub const SWAP_STEP: u64 = 1 << 30;
/// A group that turns idle and reclaimable with at least this estimated gain is an "idle cost" insight.
pub const IDLE_COST: u64 = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Crit,
}

/// One insight at a point in time.
#[derive(Debug, Clone, PartialEq)]
pub struct Marker {
    pub t_ms: u64,
    pub text: String,
    pub severity: Severity,
    /// Fingerprint of the entity the insight is about, if any (for "your things" toasts).
    pub fingerprint: Option<String>,
}

/// One served row, frozen in time. `id` and `label` are shared between frames ([`Timeline::intern`]): 10 min
/// of frames repeat the same few dozen names, and one `String` pair per row per frame was ~1.5 MB of the
/// TUI's resident memory at the plateau (SPEC §14).
#[derive(Debug, Clone, PartialEq)]
pub struct FrameRow {
    pub id: Arc<str>,
    pub label: Arc<str>,
    pub kind: GroupKind,
    pub footprint: Option<u64>,
    pub reclaimable: bool,
}

/// A compact frame.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Frame {
    pub t_ms: u64,
    pub available: Option<u64>,
    pub total: Option<u64>,
    pub swap_used: Option<u64>,
    pub swap_total: Option<u64>,
    pub cpu_pct: Option<f64>,
    pub pressure: Option<PressureLevel>,
    pub mode: &'static str,
    pub rows: Vec<FrameRow>,
}

#[derive(Debug, Clone, Default)]
pub struct Timeline {
    pub frames: VecDeque<Frame>,
    /// Names shared by the frames' rows; pruned when frames age out.
    names: HashSet<Arc<str>>,
    pub markers: VecDeque<Marker>,
    /// Scrub position (index into `frames`); `None` = live.
    pub scrub: Option<usize>,
    last_toast_ms: Option<u64>,
    swap_anchor: Option<(u64, u64)>,
}

impl Timeline {
    pub fn push(&mut self, f: Frame) {
        if let Some(last) = self.frames.back() {
            if f.t_ms < last.t_ms {
                return;
            }
        }
        let cutoff = f.t_ms.saturating_sub(RETENTION_MS);
        self.frames.push_back(f);
        let mut dropped = 0;
        while self.frames.front().map(|x| x.t_ms < cutoff).unwrap_or(false) {
            self.frames.pop_front();
            dropped += 1;
        }
        while self.markers.front().map(|m| m.t_ms < cutoff).unwrap_or(false) {
            self.markers.pop_front();
        }
        if let Some(s) = self.scrub.as_mut() {
            *s = s.saturating_sub(dropped);
        }
        if dropped > 0 && self.names.len() > 4 * ROWS_PER_FRAME {
            // Only this set still holds names no frame uses any more.
            self.names.retain(|n| Arc::strong_count(n) > 1);
        }
    }

    /// The shared copy of `s` for a frame row.
    pub fn intern(&mut self, s: &str) -> Arc<str> {
        if let Some(n) = self.names.get(s) {
            return n.clone();
        }
        let n: Arc<str> = Arc::from(s);
        self.names.insert(n.clone());
        n
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// The frame being looked at (scrubbed or latest).
    pub fn current(&self) -> Option<&Frame> {
        match self.scrub {
            Some(i) => self.frames.get(i),
            None => self.frames.back(),
        }
    }

    pub fn scrub_by(&mut self, delta: isize) {
        if self.frames.is_empty() {
            return;
        }
        let last = self.frames.len() - 1;
        let cur = self.scrub.unwrap_or(last) as isize;
        let next = (cur + delta).clamp(0, last as isize) as usize;
        self.scrub = if next == last { None } else { Some(next) };
    }

    pub fn scrub_to_fraction(&mut self, frac: f64) {
        if self.frames.is_empty() {
            return;
        }
        let last = self.frames.len() - 1;
        let i = ((frac.clamp(0.0, 1.0)) * last as f64).round() as usize;
        self.scrub = if i >= last { None } else { Some(i) };
    }

    pub fn oldest(&mut self) {
        if !self.frames.is_empty() {
            self.scrub = Some(0);
            if self.frames.len() == 1 {
                self.scrub = None;
            }
        }
    }

    pub fn live(&mut self) {
        self.scrub = None;
    }

    /// Markers between two frames (inclusive of `from`, exclusive of `to`).
    pub fn markers_between(&self, from_ms: u64, to_ms: u64) -> impl Iterator<Item = &Marker> {
        self.markers
            .iter()
            .filter(move |m| m.t_ms >= from_ms && m.t_ms < to_ms)
    }

    /// Adds a marker; returns true if it should also be shown as a toast (rate-limited).
    pub fn mark(&mut self, m: Marker, about_your_things: bool) -> bool {
        let toast = (m.severity >= Severity::Warn || about_your_things)
            && self
                .last_toast_ms
                .map(|t| m.t_ms.saturating_sub(t) >= TOAST_GAP_MS)
                .unwrap_or(true);
        if toast {
            self.last_toast_ms = Some(m.t_ms);
        }
        self.markers.push_back(m);
        toast
    }

    /// Insight generators: compares `prev` → `cur` (UX §5.3). Returned markers are not yet recorded.
    pub fn insights(
        &mut self,
        prev: Option<&Snapshot>,
        cur: &Snapshot,
        mode_changed_to: Option<&str>,
    ) -> Vec<Marker> {
        let mut out = Vec::new();
        let t = cur.taken_at_ms;
        let mk = |text: String, severity: Severity, fp: Option<String>| Marker {
            t_ms: t,
            text,
            severity,
            fingerprint: fp,
        };
        if let Some(m) = mode_changed_to {
            let (text, sev) = match m {
                "throttle" => ("throttle start".to_string(), Severity::Warn),
                "pressure" => ("memory pressure".to_string(), Severity::Warn),
                "working" => ("work started".to_string(), Severity::Info),
                "leftovers" => ("leftovers found".to_string(), Severity::Info),
                _ => ("back to calm".to_string(), Severity::Info),
            };
            out.push(mk(text, sev, None));
        }
        // Swap growth: anchored at the last marker (or first sight); every +1 GiB is an insight.
        if let Some(used) = cur.memory.swap_used.value {
            match self.swap_anchor {
                None => self.swap_anchor = Some((t, used)),
                Some((_, base)) if used >= base.saturating_add(SWAP_STEP) => {
                    let gb = (used - base) as f64 / (1u64 << 30) as f64;
                    out.push(mk(format!("swap +{gb:.1}G"), Severity::Warn, None));
                    self.swap_anchor = Some((t, used));
                }
                Some((_, base)) if used < base => self.swap_anchor = Some((t, used)),
                _ => {}
            }
        }
        let Some(prev) = prev else {
            return out;
        };
        if cur.oom.forecast.is_some() && prev.oom.forecast.is_none() {
            let eta = cur.oom.forecast.as_ref().map(|f| f.eta_s).unwrap_or(0);
            out.push(mk(
                format!("OOM forecast ~{}", oomtop_core::units::format_duration(eta)),
                Severity::Crit,
                None,
            ));
        }
        let prev_groups: HashMap<&str, &oomtop_core::Group> =
            prev.groups.iter().map(|g| (g.id.as_str(), g)).collect();
        for g in &cur.groups {
            match prev_groups.get(g.id.as_str()) {
                None => {
                    let fp = g.totals.footprint.value.unwrap_or(0);
                    if fp >= LARGE_NEW_GROUP
                        || matches!(g.kind, GroupKind::ModelServer | GroupKind::BuildDaemon)
                    {
                        out.push(mk(
                            format!("{} started", g.label),
                            Severity::Info,
                            Some(g.fingerprint.clone()),
                        ));
                    }
                }
                Some(p) => {
                    if g.orphan && !p.orphan {
                        out.push(mk(
                            format!("orphan: {}", g.label),
                            Severity::Info,
                            Some(g.fingerprint.clone()),
                        ));
                    }
                    // Idle cost above threshold (UX §5.3): it just became reclaimable and holds ≥ 1 GiB.
                    let gain = g.reclaim_gain.value.unwrap_or(0);
                    if gain >= IDLE_COST
                        && g.idle
                        && !p.idle
                        && oomtop_core::headroom::is_reclaim_candidate(g)
                    {
                        out.push(mk(
                            format!(
                                "{} idle — could free {}",
                                g.label,
                                oomtop_core::units::format_bytes_short(gain)
                            ),
                            Severity::Info,
                            Some(g.fingerprint.clone()),
                        ));
                    }
                }
            }
        }
        let cur_ids: std::collections::HashSet<&str> = cur.groups.iter().map(|g| g.id.as_str()).collect();
        for p in &prev.groups {
            if !cur_ids.contains(p.id.as_str())
                && (p.totals.footprint.value.unwrap_or(0) >= LARGE_NEW_GROUP
                    || matches!(p.kind, GroupKind::ModelServer | GroupKind::BuildDaemon))
            {
                out.push(mk(
                    format!("{} ended", p.label),
                    Severity::Info,
                    Some(p.fingerprint.clone()),
                ));
            }
        }
        for m in &cur.model_servers {
            let was = prev
                .model_servers
                .iter()
                .find(|x| x.id == m.id)
                .and_then(|x| x.busy.value);
            let label = m
                .models
                .first()
                .map(|x| x.name.clone())
                .unwrap_or_else(|| m.id.clone());
            let fp = m
                .group_id
                .as_ref()
                .and_then(|id| cur.group(id))
                .map(|g| g.fingerprint.clone());
            match (was, m.busy.value) {
                (Some(true), Some(false)) => out.push(mk(format!("{label} finished"), Severity::Info, fp)),
                (Some(false), Some(true)) | (None, Some(true)) => {
                    out.push(mk(format!("{label} generating"), Severity::Info, fp))
                }
                _ => {}
            }
            if m.status
                != prev
                    .model_servers
                    .iter()
                    .find(|x| x.id == m.id)
                    .map(|x| x.status.clone())
                    .unwrap_or_default()
            {
                if let oomtop_core::SourceStatus::Unavailable(r) = &m.status {
                    out.push(mk(format!("{label} failed: {r}"), Severity::Warn, None));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::Measured;

    fn frame(t: u64) -> Frame {
        Frame {
            t_ms: t,
            ..Default::default()
        }
    }

    #[test]
    fn ring_buffer_and_scrub() {
        let mut tl = Timeline::default();
        for i in 0..400u64 {
            tl.push(frame(i * 2000));
        }
        assert_eq!(tl.len(), 301, "10 minutes at 2 s");
        assert!(tl.scrub.is_none());
        tl.scrub_by(-10);
        assert_eq!(tl.scrub, Some(290));
        tl.push(frame(400 * 2000));
        assert_eq!(tl.scrub, Some(289), "scrub follows eviction");
        tl.scrub_by(1000);
        assert!(tl.scrub.is_none(), "scrubbing past the end returns to live");
        tl.scrub_to_fraction(0.0);
        assert_eq!(tl.scrub, Some(0));
        tl.live();
        assert_eq!(tl.current().unwrap().t_ms, 800_000);
    }

    #[test]
    fn toasts_are_rate_limited() {
        let mut tl = Timeline::default();
        let m = |t, s| Marker {
            t_ms: t,
            text: "x".into(),
            severity: s,
            fingerprint: None,
        };
        assert!(tl.mark(m(0, Severity::Warn), false));
        assert!(!tl.mark(m(10_000, Severity::Crit), false));
        assert!(
            !tl.mark(m(40_000, Severity::Info), false),
            "info about others never toasts"
        );
        assert!(tl.mark(m(41_000, Severity::Info), true), "your things toast");
    }

    #[test]
    fn swap_and_group_insights() {
        let mut tl = Timeline::default();
        let mut a = Snapshot {
            taken_at_ms: 1000,
            ..Default::default()
        };
        a.memory.swap_used = Measured::exact(1 << 30, "t");
        assert!(tl.insights(None, &a, None).is_empty());
        let mut b = a.clone();
        b.taken_at_ms = 3000;
        b.memory.swap_used = Measured::exact(3 << 29, "t");
        assert!(tl.insights(Some(&a), &b, None).is_empty());
        let mut c = b.clone();
        c.taken_at_ms = 5000;
        c.memory.swap_used = Measured::exact(2 << 30, "t");
        c.groups.push(oomtop_core::Group {
            id: "daemon:gradle".into(),
            label: "GradleDaemon".into(),
            kind: GroupKind::BuildDaemon,
            ..Default::default()
        });
        let ins = tl.insights(Some(&b), &c, Some("throttle"));
        let texts: Vec<&str> = ins.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["throttle start", "swap +1.0G", "GradleDaemon started"]);
    }

    #[test]
    fn idle_cost_insight_fires_once_when_a_group_turns_idle() {
        let mut tl = Timeline::default();
        let busy = oomtop_core::Group {
            id: "daemon:gradle".into(),
            label: "GradleDaemon".into(),
            kind: GroupKind::BuildDaemon,
            reclaim_gain: Measured::estimate(3 << 30, "t"),
            ..Default::default()
        };
        let mut a = Snapshot {
            taken_at_ms: 1000,
            ..Default::default()
        };
        a.groups.push(busy.clone());
        let mut b = a.clone();
        b.taken_at_ms = 3000;
        b.groups[0].idle = true;
        b.groups[0].idle_for_s = Some(1800);
        let texts: Vec<String> = tl
            .insights(Some(&a), &b, None)
            .into_iter()
            .map(|m| m.text)
            .collect();
        assert_eq!(texts, ["GradleDaemon idle — could free 3.0G"]);
        let mut c = b.clone();
        c.taken_at_ms = 5000;
        assert!(
            tl.insights(Some(&b), &c, None).is_empty(),
            "only on the transition"
        );
        // Small idle groups are not worth an insight.
        let mut small = b.clone();
        small.groups[0].reclaim_gain = Measured::estimate(100 << 20, "t");
        assert!(tl.insights(Some(&a), &small, None).is_empty());
    }
}

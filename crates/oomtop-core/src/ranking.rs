//! Ranking (UX §5.4) and frecency (UX §5.1).
//!
//! ```text
//! score = w_s · salience        # share of memory/GPU/CPU weighted by mode; + anomaly z-score of change rate
//!       + w_a · affinity         # frecency of this entity for this user (cold-start prior when unknown)
//!       + w_x · actionability    # reclaimable bytes / idle cost
//!       + w_q · query_match      # when a query is active, dominates
//!       − w_n · noise            # muted, "show less", system-noise priors
//!       + novelty                # small, decaying boost for entities the user hasn't met yet
//!       − diversity              # MMR-style penalty for repeats of the same cluster
//! ```
//!
//! Defaults `w_s=1.0, w_a=0.6, w_x=0.5, w_q=3.0, w_n=1.0` ([`RankWeights`]). Modes re-weight salience
//! features (Pressure boosts memory and idle cost, Throttle boosts CPU/GPU, Working boosts busy servers).
//!
//! **Stability** ([`RankState`]): a row moves only after it has beaten the row above it by `margin` for
//! [`MOVE_AFTER_REFRESHES`] consecutive refreshes; the selected row keeps its index; a moved row carries a
//! one-refresh `moved` marker (rendered as `›`, no animation frames). At a 2 s refresh a row can therefore
//! climb at most once every 4 s, and flip-flopping scores never move anything.
//!
//! **Frecency**: `Σ w(event) · 0.5^(age / half_life)`, half-life 7 days by default ([`DEFAULT_HALF_LIFE_S`]).

use crate::headline::format_amount;
use crate::headroom::is_reclaim_candidate;
use crate::history::History;
use crate::model::{Group, GroupKind, Snapshot};
use crate::modes::Mode;
use crate::query::{eval, EntityView, Understanding};
use crate::units::{format_duration, UnitSystem};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Default frecency half-life: 7 days (UX §5.1, configurable as `personalization.half_life_days`).
pub const DEFAULT_HALF_LIFE_S: f64 = 7.0 * 86_400.0;
/// Default MMR diversity penalty per earlier pick of the same cluster.
pub const DEFAULT_DIVERSITY_LAMBDA: f64 = 0.15;
/// Default score margin a row must beat the row above by before it may move.
pub const DEFAULT_MOVE_MARGIN: f64 = 0.05;
/// Refreshes required before a row may move up.
pub const MOVE_AFTER_REFRESHES: u32 = 2;
/// Maximum novelty boost for an entity the user has never interacted with.
pub const NOVELTY_MAX: f64 = 0.15;
/// Novelty halves every 30 minutes of the entity's age.
pub const NOVELTY_HALF_LIFE_S: f64 = 1_800.0;
/// Affinity given to pinned entities.
pub const PINNED_AFFINITY: f64 = 1.0;
/// Weight of the same-mode context affinity added to the plain affinity.
pub const CONTEXT_AFFINITY_WEIGHT: f64 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RankWeights {
    pub salience: f64,
    pub affinity: f64,
    pub actionability: f64,
    pub query: f64,
    pub noise: f64,
}

impl Default for RankWeights {
    fn default() -> Self {
        RankWeights {
            salience: 1.0,
            affinity: 0.6,
            actionability: 0.5,
            query: 3.0,
            noise: 1.0,
        }
    }
}

/// Features of one candidate, each roughly 0..1.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Candidate {
    pub id: String,
    pub salience: f64,
    pub affinity: f64,
    pub actionability: f64,
    pub query_match: f64,
    pub noise: f64,
    /// Decaying boost for new entities (exploration).
    pub novelty: f64,
    /// Diversity key (e.g. kind + label); items sharing a key are penalized after the first.
    pub cluster: Option<String>,
    /// Cold-start prior (machine profile), used in place of affinity for entities without history;
    /// weighted like affinity.
    pub prior: f64,
}

/// Per-feature contributions (for "why ranked here"). `noise` and `diversity` are ≤ 0.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScoreParts {
    pub salience: f64,
    pub affinity: f64,
    pub actionability: f64,
    pub query: f64,
    pub noise: f64,
    pub novelty: f64,
    pub total: f64,
    /// MMR penalty applied by [`rank`] (≤ 0).
    pub diversity: f64,
    /// Cold-start prior contribution (weighted by `w_a`).
    pub prior: f64,
}

fn finite(x: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}

/// Weighted contributions and their total (before diversity).
pub fn score(c: &Candidate, w: &RankWeights) -> ScoreParts {
    let p = ScoreParts {
        salience: finite(w.salience * c.salience),
        affinity: finite(w.affinity * c.affinity),
        actionability: finite(w.actionability * c.actionability),
        query: finite(w.query * c.query_match),
        noise: finite(-w.noise * c.noise),
        novelty: finite(c.novelty),
        total: 0.0,
        diversity: 0.0,
        prior: finite(w.affinity * c.prior),
    };
    ScoreParts {
        total: p.salience + p.affinity + p.actionability + p.query + p.noise + p.novelty + p.prior,
        ..p
    }
}

/// Frecency: `Σ w(event) · 0.5^(age / half_life)` (zoxide/atuin style). `events` = (age_s, weight).
/// Future events (negative age) count at full weight; non-finite inputs are ignored.
pub fn frecency(events: &[(f64, f64)], half_life_s: f64) -> f64 {
    if half_life_s.is_nan() || half_life_s <= 0.0 || half_life_s.is_infinite() {
        return 0.0;
    }
    events
        .iter()
        .filter(|(age, w)| age.is_finite() && w.is_finite())
        .map(|(age, w)| w * 0.5f64.powf(age.max(0.0) / half_life_s))
        .sum()
}

/// Maps a raw frecency score to 0..1 affinity (`1 − e^(−f/5)`; f = 5 → 0.63, f = 15 → 0.95).
pub fn affinity_from_frecency(f: f64) -> f64 {
    if !f.is_finite() {
        return 0.0;
    }
    1.0 - (-f.max(0.0) / 5.0).exp()
}

/// Novelty boost for an entity of age `age_s` that the user has affinity `affinity` (0..1) with:
/// `NOVELTY_MAX · 0.5^(age / 30 min) · (1 − affinity)`.
pub fn novelty_boost(age_s: f64, affinity: f64) -> f64 {
    if !age_s.is_finite() {
        return 0.0;
    }
    NOVELTY_MAX * 0.5f64.powf(age_s.max(0.0) / NOVELTY_HALF_LIFE_S) * (1.0 - affinity.clamp(0.0, 1.0))
}

/// Anomaly of the latest change in a footprint series (0..1): z-score of the last delta against the earlier
/// deltas, counting growth only. Needs ≥ 6 points; the noise floor is 1 % of the mean level or 1 MiB.
pub fn anomaly_score(series: &[u64]) -> f64 {
    if series.len() < 6 {
        return 0.0;
    }
    let deltas: Vec<f64> = series.windows(2).map(|w| w[1] as f64 - w[0] as f64).collect();
    let Some((last, earlier)) = deltas.split_last() else {
        return 0.0;
    };
    let n = earlier.len() as f64;
    let mean = earlier.iter().sum::<f64>() / n;
    let var = earlier.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / n;
    let level = series.iter().map(|v| *v as f64).sum::<f64>() / series.len() as f64;
    let floor = (level * 0.01).max(1024.0 * 1024.0);
    let z = (last - mean) / var.sqrt().max(floor);
    if *last <= 0.0 {
        return 0.0;
    }
    ((z - 2.0) / 4.0).clamp(0.0, 1.0)
}

/// Cold-start priors from the machine profile (UX §5.4, §6): model servers and agent CLIs start with a
/// boost on a machine where they are detected; everything else uses salience only.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColdStartPriors {
    pub model_server: f64,
    pub agent: f64,
}

impl Default for ColdStartPriors {
    fn default() -> Self {
        ColdStartPriors {
            model_server: 0.3,
            agent: 0.25,
        }
    }
}

impl ColdStartPriors {
    /// Priors for the roles detected in this snapshot (`None` when neither role is present).
    pub fn detect(s: &Snapshot) -> Option<ColdStartPriors> {
        let d = ColdStartPriors::default();
        let models = !s.model_servers.is_empty() || s.groups.iter().any(|g| g.kind == GroupKind::ModelServer);
        let agents = s.groups.iter().any(|g| g.kind == GroupKind::AgentSession);
        (models || agents).then_some(ColdStartPriors {
            model_server: if models { d.model_server } else { 0.0 },
            agent: if agents { d.agent } else { 0.0 },
        })
    }

    pub fn for_kind(&self, kind: GroupKind) -> f64 {
        match kind {
            GroupKind::ModelServer => self.model_server,
            GroupKind::AgentSession => self.agent,
            _ => 0.0,
        }
    }
}

/// Everything the ranker can use beyond the snapshot. All maps are keyed by entity fingerprint except
/// `query` (keyed by group id, see [`query_scores`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct RankContext<'a> {
    pub mode: Mode,
    /// Affinity 0..1 (use [`affinity_from_frecency`]).
    pub affinity: Option<&'a HashMap<String, f64>>,
    pub muted: Option<&'a HashSet<String>>,
    /// "Show less like this" (`-`): half the noise of a mute.
    pub dismissed: Option<&'a HashSet<String>>,
    pub pinned: Option<&'a HashSet<String>>,
    /// First time the profile saw each entity (ms); falls back to the root process start time.
    pub first_seen_ms: Option<&'a HashMap<String, u64>>,
    /// Ring buffer for the anomaly feature.
    pub history: Option<&'a History>,
    /// Query match 0..1 per group id.
    pub query: Option<&'a HashMap<String, f64>>,
    /// Cold-start priors; `None` = salience only for unknown entities.
    pub priors: Option<ColdStartPriors>,
    /// Context match (UX §5.1 context counters, §5.4 "+ context match: same mode"): affinity 0..1 of each
    /// entity *in the current mode* (e.g. "opened during Working mode"), keyed by fingerprint. Adds up to
    /// [`CONTEXT_AFFINITY_WEIGHT`] on top of the plain affinity (capped at 1).
    pub mode_affinity: Option<&'a HashMap<String, f64>>,
}

fn mem_total(s: &Snapshot) -> f64 {
    s.memory.total.value.unwrap_or(s.host.mem_total).max(1) as f64
}

fn mode_weights(mode: Mode) -> (f64, f64, f64) {
    // (memory, gpu, cpu)
    match mode {
        Mode::Pressure => (2.0, 1.0, 0.3),
        Mode::Throttle => (0.6, 1.2, 1.8),
        Mode::Working => (1.0, 1.5, 1.0),
        Mode::Leftovers | Mode::Calm => (1.0, 1.0, 0.7),
    }
}

/// Salience of a group: memory share (weighted by mode), GPU share, CPU share (of the whole machine).
/// Unavailable values contribute nothing (they are unknown, not zero, but ranking must still order them).
pub fn group_salience(g: &Group, s: &Snapshot, mode: Mode) -> f64 {
    let total = mem_total(s);
    let mem = g.totals.footprint.value.unwrap_or(0) as f64 / total;
    let gpu_total = s
        .accelerators
        .iter()
        .filter_map(|a| a.gpu_budget.value.or(a.mem_total.value))
        .max()
        .map(|b| b as f64)
        .unwrap_or(total)
        .max(1.0);
    let gpu = g.totals.gpu.value.unwrap_or(0) as f64 / gpu_total;
    let cores = s.host.cores_logical.max(1) as f64;
    let cpu = g.totals.cpu_pct.value.unwrap_or(0.0).max(0.0) / (100.0 * cores);
    let (wm, wg, wc) = mode_weights(mode);
    let mut sal = (wm * mem + wg * gpu + wc * cpu) * 2.0;
    if mode == Mode::Working
        && s.model_servers
            .iter()
            .any(|m| m.group_id.as_deref() == Some(g.id.as_str()) && m.busy.value == Some(true))
    {
        sal += 0.5;
    }
    finite(sal).min(1.5)
}

/// Actionability: reclaimable bytes relative to RAM (idle cost), boosted in Pressure/Leftovers mode.
pub fn group_actionability(g: &Group, s: &Snapshot, mode: Mode) -> f64 {
    if !is_reclaim_candidate(g) {
        return 0.0;
    }
    let gain = g.reclaim_gain.value.unwrap_or(0) as f64 / mem_total(s);
    let boost = if matches!(mode, Mode::Pressure | Mode::Leftovers) {
        2.0
    } else {
        1.0
    };
    finite(gain * 4.0 * boost).min(1.0)
}

/// System-noise prior: system groups and oomtop itself sink a little.
pub fn group_noise(g: &Group) -> f64 {
    let mut n = 0.0;
    if g.kind == GroupKind::System {
        n += 0.3;
    }
    if g.is_self {
        n += 0.5;
    }
    n
}

/// Builds candidates for all groups (affinity 0..1 and mutes from the caller's profile maps, keyed by
/// fingerprint). No history, query, novelty-by-profile or priors: see [`group_candidates_with`].
pub fn group_candidates(
    s: &Snapshot,
    mode: Mode,
    affinity: &HashMap<String, f64>,
    muted: &HashSet<String>,
) -> Vec<Candidate> {
    group_candidates_with(
        s,
        &RankContext {
            mode,
            affinity: Some(affinity),
            muted: Some(muted),
            ..Default::default()
        },
    )
}

/// Diversity cluster of a group: same kind and label (e.g. four "Claude Code" sessions).
pub fn group_cluster(g: &Group) -> String {
    format!("{}:{}", g.kind.alias(), g.label.to_lowercase())
}

/// Builds candidates for all groups with the full context (UX §5.4).
pub fn group_candidates_with(s: &Snapshot, ctx: &RankContext<'_>) -> Vec<Candidate> {
    s.groups
        .iter()
        .map(|g| {
            let fp = g.fingerprint.as_str();
            let has_fp = !fp.is_empty();
            let pinned = has_fp && ctx.pinned.map(|p| p.contains(fp)).unwrap_or(false);
            let get = |m: Option<&HashMap<String, f64>>| {
                if has_fp {
                    m.and_then(|a| a.get(fp)).copied().map(finite)
                } else {
                    None
                }
            };
            let in_mode = get(ctx.mode_affinity);
            // Any learned signal (overall or in this mode) replaces the cold-start prior.
            let learned = get(ctx.affinity).or(in_mode.map(|_| 0.0));
            let affinity = if pinned {
                PINNED_AFFINITY
            } else {
                (learned.unwrap_or(0.0).clamp(0.0, 1.0)
                    + CONTEXT_AFFINITY_WEIGHT * in_mode.unwrap_or(0.0).clamp(0.0, 1.0))
                .min(1.0)
            };
            let prior = match (learned, pinned, ctx.priors) {
                (None, false, Some(p)) => p.for_kind(g.kind),
                _ => 0.0,
            };
            let mut salience = group_salience(g, s, ctx.mode);
            if let Some(h) = ctx.history {
                salience = (salience + 0.5 * anomaly_score(&h.group_series(&g.id))).min(1.5);
            }
            let muted = has_fp && ctx.muted.map(|m| m.contains(fp)).unwrap_or(false);
            let dismissed = has_fp && ctx.dismissed.map(|d| d.contains(fp)).unwrap_or(false);
            let noise = group_noise(g) + if muted { 1.0 } else { 0.0 } + if dismissed { 0.5 } else { 0.0 };
            let first_seen = ctx
                .first_seen_ms
                .and_then(|m| m.get(fp))
                .copied()
                .or_else(|| g.root.map(|r| r.start_time).filter(|t| *t > 0));
            let novelty = match first_seen {
                Some(t) if !muted && s.taken_at_ms > 0 => {
                    novelty_boost(s.taken_at_ms.saturating_sub(t) as f64 / 1000.0, affinity)
                }
                _ => 0.0,
            };
            Candidate {
                id: g.id.clone(),
                salience,
                affinity,
                actionability: group_actionability(g, s, ctx.mode),
                query_match: ctx.query.and_then(|q| q.get(&g.id)).copied().unwrap_or(0.0),
                noise,
                novelty,
                cluster: Some(group_cluster(g)),
                prior,
            }
        })
        .collect()
}

/// Query match per group id (0..1) for an understood query: 1.0 when the filter matches, plus linked
/// targets scaled relative to the best link (a `pid:N` target credits the group containing that pid).
pub fn query_scores(u: &Understanding, s: &Snapshot) -> HashMap<String, f64> {
    let mut out: HashMap<String, f64> = HashMap::new();
    if let Some(f) = &u.filter {
        for g in &s.groups {
            if eval(f, &EntityView::from_group(g, s)) {
                out.insert(g.id.clone(), 1.0);
            }
        }
    }
    let best = u.targets.first().map(|t| t.1).filter(|b| *b > 0.0);
    if let Some(best) = best {
        for (id, sc) in &u.targets {
            let gid = if let Some(pid) = id.strip_prefix("pid:").and_then(|p| p.parse::<u32>().ok()) {
                s.processes
                    .iter()
                    .find(|p| p.id.pid == pid)
                    .and_then(|p| s.group_of(p.id))
                    .map(|g| g.id.clone())
            } else {
                s.group(id).map(|g| g.id.clone())
            };
            if let Some(gid) = gid {
                let v = (sc / best).clamp(0.0, 1.0);
                // Groups sharing the entity fingerprint (e.g. four Claude Code sessions) are one entity.
                let fp = s.group(&gid).map(|g| g.fingerprint.clone()).unwrap_or_default();
                for g in &s.groups {
                    if g.id == gid || (!fp.is_empty() && g.fingerprint == fp) {
                        let e = out.entry(g.id.clone()).or_insert(0.0);
                        *e = e.max(v);
                    }
                }
            }
        }
    }
    out
}

/// Scores candidates and orders them greedily with an MMR-style diversity penalty: each pick of a cluster
/// costs later members of that cluster `λ` more (`λ = diversity_lambda`, 0 disables). Ties by id.
pub fn rank(cands: &[Candidate], w: &RankWeights, diversity_lambda: f64) -> Vec<(String, ScoreParts)> {
    let lambda = finite(diversity_lambda).max(0.0);
    // Bucket by cluster (candidates without a cluster are singletons), each bucket sorted worst-first so
    // `pop()`/`last()` is its best remaining member.
    type Scored = (String, ScoreParts);
    let mut buckets: Vec<Vec<Scored>> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for c in cands {
        let parts = score(c, w);
        match &c.cluster {
            Some(k) => {
                let i = *index.entry(k.clone()).or_insert_with(|| {
                    buckets.push(Vec::new());
                    buckets.len() - 1
                });
                buckets[i].push((c.id.clone(), parts));
            }
            None => buckets.push(vec![(c.id.clone(), parts)]),
        }
    }
    let cmp = |a: &Scored, b: &Scored| {
        b.1.total
            .partial_cmp(&a.1.total)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    };
    for v in &mut buckets {
        v.sort_by(cmp);
        v.reverse();
    }
    let mut picked: Vec<u32> = vec![0; buckets.len()];
    let mut out = Vec::with_capacity(cands.len());
    loop {
        let mut best: Option<(usize, f64, &str)> = None;
        for (bi, v) in buckets.iter().enumerate() {
            if let Some((id, p)) = v.last() {
                let eff = p.total - lambda * picked[bi] as f64;
                let better = match best {
                    None => true,
                    Some((_, oeff, oid)) => eff > oeff || (eff == oeff && id.as_str() < oid),
                };
                if better {
                    best = Some((bi, eff, id.as_str()));
                }
            }
        }
        let Some((bi, _, _)) = best else { break };
        let Some((id, mut p)) = buckets[bi].pop() else {
            break;
        };
        p.diversity = -lambda * picked[bi] as f64;
        p.total += p.diversity;
        picked[bi] += 1;
        out.push((id, p));
    }
    out
}

/// One row as served, with a one-refresh movement marker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankedRow {
    pub id: String,
    pub score: f64,
    /// Moved up this refresh (render `›` for one refresh).
    pub moved: bool,
}

/// Stability state carried between refreshes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RankState {
    order: Vec<String>,
    pending: HashMap<String, u32>,
    /// Rows that appeared on the last refresh.
    new_ids: HashSet<String>,
}

fn sane(x: f64) -> f64 {
    if x.is_nan() {
        f64::MIN
    } else {
        x
    }
}

impl RankState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The order served last refresh.
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// Rows that appeared on the last refresh (not counting the very first one).
    pub fn new_ids(&self) -> &HashSet<String> {
        &self.new_ids
    }

    /// Forgets the order (next [`apply`](Self::apply) sorts from scratch), e.g. after a view change.
    pub fn reset(&mut self) {
        self.order.clear();
        self.pending.clear();
        self.new_ids.clear();
    }

    /// Applies hysteresis to this refresh's scores:
    /// - first call: plain score order (ties by id);
    /// - rows that disappeared are dropped; new rows are inserted at their score position (see
    ///   [`new_ids`](Self::new_ids));
    /// - a row climbs only after beating the row directly above it by `margin` for
    ///   [`MOVE_AFTER_REFRESHES`] consecutive refreshes; it then jumps above every row it beats by `margin`
    ///   (not past a row that moved this refresh), and its counter restarts;
    /// - `selected` keeps its index; rows arrange around it.
    pub fn apply(&mut self, scored: &[(String, f64)], selected: Option<&str>, margin: f64) -> Vec<RankedRow> {
        let margin = if margin.is_finite() { margin.max(0.0) } else { 0.0 };
        let mut scores: HashMap<&str, f64> = HashMap::with_capacity(scored.len());
        let mut ids: Vec<&str> = Vec::with_capacity(scored.len());
        for (id, s) in scored {
            if !scores.contains_key(id.as_str()) {
                scores.insert(id.as_str(), sane(*s));
                ids.push(id.as_str());
            }
        }
        let sc = |id: &str| scores.get(id).copied().unwrap_or(f64::MIN);
        let by_score = |a: &&str, b: &&str| {
            sc(b)
                .partial_cmp(&sc(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.cmp(b))
        };
        let prev: Vec<String> = self
            .order
            .iter()
            .filter(|id| scores.contains_key(id.as_str()))
            .cloned()
            .collect();
        if prev.is_empty() {
            let mut order = ids.clone();
            order.sort_by(by_score);
            self.pending.clear();
            self.new_ids.clear();
            self.order = order.iter().map(|s| s.to_string()).collect();
            return order
                .into_iter()
                .map(|id| RankedRow {
                    id: id.to_string(),
                    score: sc(id),
                    moved: false,
                })
                .collect();
        }
        let prev_set: HashSet<&str> = prev.iter().map(String::as_str).collect();
        let sel_idx = selected.and_then(|s| prev.iter().position(|x| x == s));
        let sel_id = sel_idx.map(|i| prev[i].clone());
        let mut work: Vec<String> = prev
            .iter()
            .filter(|x| Some(*x) != sel_id.as_ref())
            .cloned()
            .collect();
        // New rows at their score position.
        let mut new_ids: Vec<&str> = ids.iter().copied().filter(|id| !prev_set.contains(id)).collect();
        new_ids.sort_by(by_score);
        let new_set: HashSet<&str> = new_ids.iter().copied().collect();
        for id in &new_ids {
            let s = sc(id);
            let pos = work.iter().position(|w| sc(w) < s).unwrap_or(work.len());
            work.insert(pos, id.to_string());
        }
        // Streaks: consecutive refreshes beating the row directly above by the margin.
        for i in 0..work.len() {
            let id = &work[i];
            if new_set.contains(id.as_str()) {
                self.pending.remove(id);
                continue;
            }
            let beats = i > 0 && sc(id) > sc(&work[i - 1]) + margin;
            if beats {
                *self.pending.entry(id.clone()).or_insert(0) += 1;
            } else {
                self.pending.remove(id);
            }
        }
        // Moves, top to bottom.
        let mut moved: HashSet<String> = HashSet::new();
        let mut i = 1;
        while i < work.len() {
            let ready = self
                .pending
                .get(&work[i])
                .map(|n| *n >= MOVE_AFTER_REFRESHES)
                .unwrap_or(false);
            if ready {
                let s = sc(&work[i]);
                let mut j = i;
                while j > 0 && s > sc(&work[j - 1]) + margin && !moved.contains(&work[j - 1]) {
                    j -= 1;
                }
                if j < i {
                    let r = work.remove(i);
                    self.pending.remove(&r);
                    moved.insert(r.clone());
                    work.insert(j, r);
                }
            }
            i += 1;
        }
        if let (Some(idx), Some(id)) = (sel_idx, sel_id) {
            self.pending.remove(&id);
            let idx = idx.min(work.len());
            work.insert(idx, id);
        }
        self.pending.retain(|id, _| scores.contains_key(id.as_str()));
        self.new_ids = new_set.iter().map(|s| s.to_string()).collect();
        self.order = work;
        self.order
            .iter()
            .map(|id| RankedRow {
                id: id.clone(),
                score: sc(id),
                moved: moved.contains(id),
            })
            .collect()
    }
}

/// English ordinal: 1st, 2nd, 3rd, 4th, 11th, 12th, 13th, 21st, …
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// Facts that turn score parts into plain words ("holds 2.8 GB", "you opened it 9× this week").
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RankFacts {
    pub footprint: Option<u64>,
    pub gpu: Option<u64>,
    pub cpu_pct: Option<f64>,
    pub process_count: u32,
    /// Selections in the last 7 days (from the profile).
    pub opens_this_week: Option<u32>,
    /// Selections while the current mode was active (context counter), e.g. (Pressure, 4).
    pub opens_in_mode: Option<(Mode, u32)>,
    pub reclaim_gain: Option<u64>,
    pub idle_for_s: Option<u64>,
    pub kind: GroupKind,
    pub pinned: bool,
    pub muted: bool,
    pub dismissed: bool,
    /// The active query text, when any.
    pub query: Option<String>,
    pub units: UnitSystem,
}

impl RankFacts {
    pub fn from_group(g: &Group, opens_this_week: Option<u32>) -> Self {
        RankFacts {
            footprint: g.totals.footprint.value,
            gpu: g.totals.gpu.value,
            cpu_pct: g.totals.cpu_pct.value,
            process_count: g.totals.process_count.max(g.members.len() as u32),
            opens_this_week,
            reclaim_gain: if is_reclaim_candidate(g) {
                g.reclaim_gain.value
            } else {
                None
            },
            idle_for_s: if g.idle { g.idle_for_s } else { None },
            kind: g.kind,
            ..Default::default()
        }
    }
}

const EPS: f64 = 0.01;

fn contributions(parts: &ScoreParts) -> Vec<(&'static str, f64)> {
    let mut items: Vec<(&'static str, f64)> = vec![
        ("query", parts.query),
        ("salience", parts.salience),
        ("affinity", parts.affinity),
        ("prior", parts.prior),
        ("actionability", parts.actionability),
        ("novelty", parts.novelty),
        ("noise", parts.noise),
        ("diversity", parts.diversity),
    ];
    items.retain(|(_, v)| v.abs() > EPS);
    // Stable sort: equal magnitudes keep the declared order.
    items.sort_by(|a, b| {
        b.1.abs()
            .partial_cmp(&a.1.abs())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    items
}

fn phrase(key: &str, f: Option<&RankFacts>) -> String {
    let units = f.map(|f| f.units).unwrap_or_default();
    let words = match (key, f) {
        ("salience", Some(f)) => match (f.footprint, f.gpu, f.cpu_pct) {
            (_, Some(g), _) if g > 0 && Some(g) >= f.footprint => {
                format!("holds {} GPU memory", format_amount(g, units))
            }
            (Some(b), _, _) if b > 0 => format!("holds {}", format_amount(b, units)),
            (_, _, Some(c)) if c >= 1.0 => format!("uses {c:.0}% CPU"),
            _ => "large share of the machine".into(),
        },
        ("salience", None) => "large share of the machine".into(),
        ("affinity", Some(f)) if f.pinned => "pinned by you".into(),
        ("affinity", Some(f)) => {
            let week = f.opens_this_week.filter(|n| *n > 0);
            let ctx = f.opens_in_mode.filter(|(_, n)| *n > 0);
            match (week, ctx) {
                (Some(n), Some((m, k))) => {
                    format!("you opened it {n}× this week, {k}× in {} mode", m.as_str())
                }
                (Some(n), None) => format!("you opened it {n}× this week"),
                (None, Some((m, k))) => format!("you opened it {k}× in {} mode", m.as_str()),
                (None, None) => "you use it often".into(),
            }
        }
        ("affinity", None) => "you use it often".into(),
        ("prior", Some(f)) => match f.kind {
            GroupKind::ModelServer => "model servers start boosted on this machine".into(),
            GroupKind::AgentSession => "agent sessions start boosted on this machine".into(),
            _ => "machine-profile prior".into(),
        },
        ("prior", None) => "machine-profile prior".into(),
        ("actionability", Some(f)) => match (f.idle_for_s, f.reclaim_gain) {
            (Some(i), Some(g)) => format!(
                "idle {}, could free {}",
                format_duration(i),
                format_amount(g, units)
            ),
            (None, Some(g)) => format!("could free {}", format_amount(g, units)),
            _ => "reclaimable".into(),
        },
        ("actionability", None) => "reclaimable".into(),
        ("query", Some(f)) => match &f.query {
            Some(q) if !q.is_empty() => format!("matches \"{q}\""),
            _ => "matches your query".into(),
        },
        ("query", None) => "matches your query".into(),
        ("novelty", _) => "recently started".into(),
        ("noise", Some(f)) if f.muted => "muted".into(),
        ("noise", Some(f)) if f.dismissed => "you asked to see less of it".into(),
        ("noise", Some(f)) if f.kind == GroupKind::System => "system process".into(),
        ("noise", _) => "lowered as noise".into(),
        ("diversity", _) => "similar to a row above".into(),
        _ => key.to_string(),
    };
    format!("{words} ({key})")
}

fn explain_impl(position: usize, parts: &ScoreParts, facts: Option<&RankFacts>) -> String {
    let mut body: Vec<String> = contributions(parts)
        .iter()
        .take(3)
        .map(|(k, _)| phrase(k, facts))
        .collect();
    if let Some(f) = facts {
        if f.process_count > 1 {
            body.push(format!("{} processes grouped", f.process_count));
        }
    }
    if body.is_empty() {
        body.push("no strong signal (default order)".into());
    }
    format!("{}: {}.", ordinal(position.max(1)), body.join(" · "))
}

/// "Why ranked here" from score parts alone (top 3 contributions in plain words), e.g.
/// `"3rd: large share of the machine (salience) · you use it often (affinity)."`.
pub fn explain_rank(position: usize, parts: &ScoreParts) -> String {
    explain_impl(position, parts, None)
}

/// "Why ranked here" with facts, e.g.
/// `"3rd: holds 2.8 GB (salience) · you opened it 9× this week (affinity) · 14 processes grouped."`.
pub fn explain_rank_for(position: usize, parts: &ScoreParts, facts: &RankFacts) -> String {
    explain_impl(position, parts, Some(facts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Measured, ProcId};
    use crate::units::GIB;

    #[test]
    fn frecency_halves() {
        let f = frecency(&[(0.0, 1.0), (7.0 * 86400.0, 1.0)], DEFAULT_HALF_LIFE_S);
        assert!((f - 1.5).abs() < 1e-9);
        assert_eq!(frecency(&[(1.0, 1.0)], 0.0), 0.0);
        assert_eq!(frecency(&[(f64::NAN, 1.0), (0.0, 2.0)], 10.0), 2.0);
        assert!((affinity_from_frecency(5.0) - 0.632).abs() < 1e-3);
        assert_eq!(affinity_from_frecency(-3.0), 0.0);
    }

    #[test]
    fn stability() {
        let mut st = RankState::new();
        let r = st.apply(&[("a".into(), 3.0), ("b".into(), 2.0)], None, 0.1);
        assert_eq!(r.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        // b beats a: needs 2 refreshes.
        let s2 = [("a".to_string(), 1.0), ("b".to_string(), 2.0)];
        let r = st.apply(&s2, None, 0.1);
        assert_eq!(r[0].id, "a");
        let r = st.apply(&s2, None, 0.1);
        assert_eq!(r[0].id, "b");
        assert!(r[0].moved);
        let r = st.apply(&s2, None, 0.1);
        assert!(!r[0].moved, "marker lasts one refresh");
        // Selected keeps its index.
        let s3 = [("a".to_string(), 5.0), ("b".to_string(), 2.0)];
        st.apply(&s3, Some("b"), 0.1);
        let r = st.apply(&s3, Some("b"), 0.1);
        assert_eq!(r[0].id, "b");
        let r = st.apply(&s3, Some("a"), 0.1);
        assert_eq!(r[1].id, "a", "selected a stays at index 1");
        let r = st.apply(&s3, None, 0.1);
        assert_eq!(r[0].id, "b", "streak restarted");
        let r = st.apply(&s3, None, 0.1);
        assert_eq!(r[0].id, "a");
    }

    #[test]
    fn new_rows_insert_by_score_and_climbers_jump() {
        let mut st = RankState::new();
        st.apply(
            &[("a".into(), 5.0), ("b".into(), 4.0), ("c".into(), 3.0)],
            None,
            0.1,
        );
        let r = st.apply(
            &[
                ("a".into(), 5.0),
                ("b".into(), 4.0),
                ("c".into(), 3.0),
                ("n".into(), 4.5),
            ],
            None,
            0.1,
        );
        assert_eq!(
            r.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["a", "n", "b", "c"]
        );
        assert!(st.new_ids().contains("n"));
        // c jumps to the top after 2 refreshes.
        let s = [
            ("a".into(), 5.0),
            ("b".into(), 4.0),
            ("c".into(), 9.0),
            ("n".into(), 4.5),
        ];
        st.apply(&s, None, 0.1);
        let r = st.apply(&s, None, 0.1);
        assert_eq!(r[0].id, "c");
        assert!(r[0].moved);
        // Removal.
        let r = st.apply(&[("a".into(), 5.0), ("n".into(), 4.5)], None, 0.1);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn diversity_penalizes_repeats() {
        let c = |id: &str, s: f64, cl: &str| Candidate {
            id: id.into(),
            salience: s,
            cluster: Some(cl.into()),
            ..Default::default()
        };
        let r = rank(
            &[
                c("x1", 1.0, "x"),
                c("x2", 0.95, "x"),
                c("y", 0.9, "y"),
                c("x3", 0.94, "x"),
            ],
            &RankWeights::default(),
            0.2,
        );
        assert_eq!(
            r.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["x1", "y", "x2", "x3"]
        );
        assert!((r[2].1.diversity + 0.2).abs() < 1e-9);
        assert!((r[3].1.diversity + 0.4).abs() < 1e-9);
        assert_eq!(
            explain_rank(3, &r[2].1),
            "3rd: large share of the machine (salience) · similar to a row above (diversity)."
        );
        // λ = 0: plain score order.
        let r = rank(
            &[c("x1", 1.0, "x"), c("x2", 0.95, "x"), c("y", 0.9, "y")],
            &RankWeights::default(),
            0.0,
        );
        assert_eq!(
            r.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["x1", "x2", "y"]
        );
    }

    #[test]
    fn novelty_and_priors() {
        assert!((novelty_boost(0.0, 0.0) - NOVELTY_MAX).abs() < 1e-12);
        assert!((novelty_boost(NOVELTY_HALF_LIFE_S, 0.0) - NOVELTY_MAX / 2.0).abs() < 1e-12);
        assert_eq!(novelty_boost(0.0, 1.0), 0.0);
        let mut s = Snapshot {
            taken_at_ms: 10_000_000,
            ..Default::default()
        };
        s.memory.total = Measured::exact(24 * GIB, "t");
        s.groups.push(Group {
            id: "model:sd".into(),
            kind: GroupKind::ModelServer,
            label: "sd-server".into(),
            fingerprint: "fp-sd".into(),
            root: Some(ProcId::new(10, 10_000_000)),
            ..Default::default()
        });
        s.groups.push(Group {
            id: "app:x".into(),
            kind: GroupKind::App,
            label: "X".into(),
            fingerprint: "fp-x".into(),
            ..Default::default()
        });
        let priors = ColdStartPriors::detect(&s).unwrap();
        assert_eq!(priors.agent, 0.0);
        let c = group_candidates_with(
            &s,
            &RankContext {
                priors: Some(priors),
                ..Default::default()
            },
        );
        assert!((c[0].prior - 0.3).abs() < 1e-12);
        assert!((c[0].novelty - NOVELTY_MAX).abs() < 1e-12, "just started");
        assert_eq!(c[1].prior, 0.0);
        // Learned affinity replaces the prior.
        let aff: HashMap<String, f64> = [("fp-sd".to_string(), 0.2)].into();
        let c = group_candidates_with(
            &s,
            &RankContext {
                priors: Some(priors),
                affinity: Some(&aff),
                ..Default::default()
            },
        );
        assert_eq!(c[0].prior, 0.0);
        assert_eq!(c[0].affinity, 0.2);
    }

    #[test]
    fn anomaly() {
        let flat: Vec<u64> = vec![GIB; 10];
        assert_eq!(anomaly_score(&flat), 0.0);
        let mut jump = flat.clone();
        jump.push(3 * GIB);
        assert!(anomaly_score(&jump) > 0.9);
        let mut drop = flat;
        drop.push(0);
        assert_eq!(anomaly_score(&drop), 0.0);
        assert_eq!(anomaly_score(&[1, 2]), 0.0);
    }

    #[test]
    fn explanations_with_facts() {
        let parts = ScoreParts {
            salience: 0.23,
            affinity: 0.5,
            total: 0.73,
            ..Default::default()
        };
        let facts = RankFacts {
            footprint: Some(2_800_000_000),
            process_count: 14,
            opens_this_week: Some(9),
            units: UnitSystem::Si,
            ..Default::default()
        };
        assert_eq!(
            explain_rank_for(3, &parts, &facts),
            "3rd: you opened it 9× this week (affinity) · holds 2.8 GB (salience) · 14 processes grouped."
        );
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(22), "22nd");
        assert_eq!(
            explain_rank(1, &ScoreParts::default()),
            "1st: no strong signal (default order)."
        );
    }
}

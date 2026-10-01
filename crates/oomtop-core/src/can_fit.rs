//! `can_fit(need)` → yes | yes_after_reclaim(list, gain) | no(shortfall), plus CLI exit codes (SPEC §8.2).
//!
//! Advisory only: two agents can both get `yes` (SPEC §19 Q1). Suspend never appears in plans (SPEC §13).
//!
//! Decision rules (conservative):
//! - Host RAM: `need.bytes ≤ headroom` (headroom already subtracts the safety margin). On unified memory a
//!   GPU-resident need is host RAM too, so the host need is `max(bytes, gpu_bytes)`.
//! - GPU (SPEC §8.1): a GPU-resident need must also fit `budget − in use − margin` of one device (unified:
//!   the Metal working set; discrete: the device with the most free VRAM). Unknown GPU state is noted and
//!   not treated as a pass *or* a fail of the host check.
//! - Reclaim: greedy over candidates, largest gain first, until both shortfalls are covered; then a prune
//!   pass drops candidates that turned out unnecessary (smallest first) so the plan stops as little as
//!   possible. Candidates are only groups oomtop may suggest ([`crate::headroom::is_reclaim_candidate`]).
//! - Missing host data → `no` with `error` set; [`answer_exit_code`] maps that to exit code 1.

use crate::headroom::{
    compute, group_gpu_gain, group_reclaim_gain, group_swap_gain, is_reclaim_candidate, GpuHeadroom,
    Headroom, HeadroomConfig,
};
use crate::model::{GroupKind, Snapshot};
use crate::model_estimate::ModelEstimate;
use crate::units::{format_bytes, UnitSystem};
use serde::{Deserialize, Serialize};

/// `oomtop headroom` exit codes.
pub const EXIT_YES: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_YES_AFTER_RECLAIM: i32 = 3;
pub const EXIT_NO: i32 = 4;

/// Answers stay valid for this long.
pub const VALID_FOR_S: u64 = 10;

/// What must fit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Need {
    /// Host RAM needed, bytes.
    pub bytes: u64,
    /// Bytes that must also fit the GPU budget (GPU-resident model), if any.
    pub gpu_bytes: Option<u64>,
    /// Free-text description, e.g. "model.gguf (weights 12.1G + kv 0.5G + overhead 0.6G)".
    pub label: Option<String>,
}

impl Need {
    /// Plain byte need ("--need 13G").
    pub fn bytes(bytes: u64) -> Self {
        Need {
            bytes,
            gpu_bytes: None,
            label: None,
        }
    }

    /// Need from a model estimate. `gpu_resident` = the model is offloaded to the GPU (Metal/CUDA), so the
    /// whole `need` must also fit the GPU budget.
    pub fn from_estimate(est: &ModelEstimate, gpu_resident: bool, label: impl Into<String>) -> Self {
        Need {
            bytes: est.need,
            gpu_bytes: gpu_resident.then_some(est.need),
            label: Some(format!(
                "{} (weights {} + kv {} + overhead {}{})",
                label.into(),
                fmt(est.weights),
                fmt(est.kv),
                fmt(est.overhead),
                if est.fallback {
                    ", estimated 1.2× file size"
                } else {
                    ""
                }
            )),
        }
    }
}

/// A group that could be stopped to make room.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ReclaimCandidate {
    pub group_id: String,
    pub label: String,
    pub kind: GroupKind,
    /// Estimated RAM freed.
    pub gain: u64,
    pub idle_for_s: Option<u64>,
    pub orphan: bool,
    /// Estimated swap freed (separate from RAM, SPEC §8.1).
    pub swap_gain: Option<u64>,
    /// Estimated GPU memory freed, when known (GPU-resident model servers).
    pub gpu_gain: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "answer")]
pub enum Fit {
    Yes,
    YesAfterReclaim {
        reclaim: Vec<ReclaimCandidate>,
        gain: u64,
    },
    No {
        shortfall: u64,
    },
}

impl Default for Fit {
    fn default() -> Self {
        Fit::No { shortfall: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CanFitAnswer {
    pub fit: Fit,
    pub need: Need,
    /// Host headroom used for the decision (bytes, may be negative).
    pub headroom: Option<i64>,
    /// GPU budget free on the chosen accelerator (after its margin), if a GPU need was given.
    pub gpu_free: Option<u64>,
    pub as_of_ms: u64,
    pub valid_for_s: u64,
    /// One-line human explanation.
    pub reason: String,
    /// `as_of_ms + valid_for_s × 1000`.
    pub expires_at_ms: u64,
    /// Host RAM need used for the decision (`max(bytes, gpu_bytes)` on unified memory).
    pub host_need: u64,
    /// Accelerator checked for a GPU need.
    pub gpu_device: Option<String>,
    /// GPU part of the shortfall (a `no` caused by the GPU budget).
    pub gpu_shortfall: Option<u64>,
    /// Whether the GPU budget could be checked (false when a GPU need was given but GPU data is missing).
    pub gpu_checked: bool,
    /// Set when the answer could not be computed from measurements (exit code 1).
    pub error: Option<String>,
    /// Caveats (advisory nature, unchecked GPU, estimated inputs).
    pub notes: Vec<String>,
    /// Host RAM still missing after reclaiming every candidate that helps (a `no`; 0 otherwise).
    pub host_shortfall: u64,
    /// Host RAM all helpful candidates would free together (a `no`; the chosen gain for yes-after-reclaim).
    pub reclaimable: u64,
}

/// Reclaim candidates sorted by gain (largest first). Groups without a stored gain get one computed.
pub fn reclaim_candidates(snapshot: &Snapshot) -> Vec<ReclaimCandidate> {
    let mut v: Vec<ReclaimCandidate> = snapshot
        .groups
        .iter()
        .filter(|g| is_reclaim_candidate(g))
        .filter_map(|g| {
            let gain = g
                .reclaim_gain
                .value
                .filter(|_| g.reclaim_gain.quality.is_available())
                .or_else(|| group_reclaim_gain(g, snapshot).value)?;
            let swap_gain = g
                .swap_gain
                .value
                .filter(|_| g.swap_gain.quality.is_available())
                .or_else(|| group_swap_gain(g, snapshot).value);
            Some(ReclaimCandidate {
                group_id: g.id.clone(),
                label: g.label.clone(),
                kind: g.kind,
                gain,
                idle_for_s: g.idle_for_s,
                orphan: g.orphan,
                swap_gain,
                gpu_gain: group_gpu_gain(g, snapshot),
            })
        })
        .filter(|c| c.gain > 0 || c.gpu_gain.unwrap_or(0) > 0)
        .collect();
    v.sort_by(|a, b| b.gain.cmp(&a.gain).then_with(|| a.group_id.cmp(&b.group_id)));
    v
}

/// Picks the accelerator a GPU need is checked against: the unified GPU if any, else the discrete device
/// with the most headroom. Devices without usable data are skipped.
fn pick_gpu(gpus: &[GpuHeadroom]) -> Option<&GpuHeadroom> {
    let usable = || gpus.iter().filter(|g| g.headroom.is_some());
    usable()
        .find(|g| g.unified)
        .or_else(|| usable().max_by_key(|g| g.headroom.unwrap_or(i64::MIN)))
}

/// Amounts in the stored `reason`: the default display units (IEC, one decimal — what the CLI prints with
/// the default config). Frontends re-render the reason in their configured units with [`reason_with`].
fn fmt(b: u64) -> String {
    format_bytes(b, UnitSystem::Iec, 1)
}

/// The one-line explanation of an answer, with amounts rendered by `fmt` — so a frontend that shows sizes
/// in the user's units (`format.memory_units`) says the whole sentence in those units. `can_fit` stores
/// the IEC rendering in [`CanFitAnswer::reason`].
pub fn reason_with(a: &CanFitAnswer, fmt: &dyn Fn(u64) -> String) -> String {
    if a.error.is_some() {
        return "available memory is unavailable on this host".into();
    }
    match &a.fit {
        Fit::Yes => {
            if a.need.gpu_bytes.is_some() && a.gpu_device.is_some() {
                "fits in current headroom and GPU budget".into()
            } else {
                "fits in current headroom".into()
            }
        }
        Fit::YesAfterReclaim { reclaim, gain } => {
            let n = reclaim.len();
            let names: Vec<String> = reclaim.iter().map(display).collect();
            format!(
                "fits after stopping {n} group{} ({}) freeing ≈{}",
                if n == 1 { "" } else { "s" },
                names.join(", "),
                fmt(*gain)
            )
        }
        Fit::No { .. } => {
            let gpu_left = a.gpu_shortfall.unwrap_or(0);
            if gpu_left > a.host_shortfall {
                format!(
                    "exceeds GPU budget on {} by {}",
                    a.gpu_device.as_deref().unwrap_or("gpu"),
                    fmt(gpu_left)
                )
            } else if a.reclaimable > 0 {
                format!(
                    "short by {} even after reclaiming ≈{}",
                    fmt(a.host_shortfall),
                    fmt(a.reclaimable)
                )
            } else {
                format!("short by {} and nothing to reclaim", fmt(a.host_shortfall))
            }
        }
    }
}

/// `need − room`, clamped at 0, without wrapping (a need above `i64::MAX` must never look like it fits).
fn shortfall(need: u64, room: i64) -> u64 {
    (need as i128 - room as i128).clamp(0, u64::MAX as i128) as u64
}

/// Decides whether `need` fits now, after reclaiming candidates (greedy, largest gain first), or not at all.
/// Candidates are ordered by gain here, so callers may pass them in any order.
pub fn can_fit(need: &Need, headroom: &Headroom, candidates: &[ReclaimCandidate]) -> CanFitAnswer {
    let mut sorted: Vec<&ReclaimCandidate> = candidates.iter().collect();
    sorted.sort_by(|a, b| {
        b.gain
            .cmp(&a.gain)
            .then_with(|| b.gpu_gain.cmp(&a.gpu_gain))
            .then_with(|| a.group_id.cmp(&b.group_id))
    });
    let candidates = sorted;
    let gpu = need.gpu_bytes.and_then(|_| pick_gpu(&headroom.gpu));
    let unified = headroom.unified_memory
        || gpu.map(|g| g.unified).unwrap_or(false)
        || headroom.gpu.iter().any(|g| g.unified);
    let host_need = match need.gpu_bytes {
        Some(g) if unified => need.bytes.max(g),
        _ => need.bytes,
    };
    let mut base = CanFitAnswer {
        need: need.clone(),
        headroom: headroom.headroom,
        as_of_ms: headroom.as_of_ms,
        valid_for_s: VALID_FOR_S,
        expires_at_ms: headroom.as_of_ms.saturating_add(VALID_FOR_S * 1000),
        host_need,
        gpu_device: gpu.map(|g| g.accelerator_id.clone()),
        gpu_checked: need.gpu_bytes.is_none() || gpu.is_some(),
        ..Default::default()
    };
    base.notes
        .push("advisory: another process may take this memory before you load".into());
    if headroom.available_now.quality == crate::model::Quality::Estimate {
        base.notes.push("available memory is an estimate".into());
    }
    if need.gpu_bytes.is_some() && gpu.is_none() {
        base.notes
            .push("GPU budget unavailable on this host: only host RAM was checked".into());
    }
    let Some(room) = headroom.headroom else {
        return CanFitAnswer {
            fit: Fit::No { shortfall: host_need },
            reason: "available memory is unavailable on this host".into(),
            error: Some(
                headroom
                    .available_now
                    .unavailable_reason()
                    .unwrap_or("available memory unavailable")
                    .to_string(),
            ),
            ..base
        };
    };

    let host_short = shortfall(host_need, room);
    let gpu_room = gpu.and_then(|g| g.headroom);
    base.gpu_free = gpu_room.map(|r| r.max(0) as u64);
    let gpu_short = match (need.gpu_bytes, gpu_room) {
        (Some(n), Some(r)) => shortfall(n, r),
        _ => 0,
    };
    if host_short == 0 && gpu_short == 0 {
        return CanFitAnswer {
            fit: Fit::Yes,
            reason: if need.gpu_bytes.is_some() && gpu.is_some() {
                "fits in current headroom and GPU budget".into()
            } else {
                "fits in current headroom".into()
            },
            ..base
        };
    }

    // Greedy selection, largest host gain first. A GPU shortfall is only covered by known GPU gains
    // (stopping a CPU-only daemon frees RAM but not GPU working set, even on unified memory).
    let gpu_gain_of = |c: &ReclaimCandidate| -> u64 { c.gpu_gain.unwrap_or(0) };
    let covered = |chosen: &[&ReclaimCandidate]| -> (bool, u64, u64) {
        let host: u64 = chosen.iter().map(|c| c.gain).fold(0, u64::saturating_add);
        let g: u64 = chosen.iter().map(|c| gpu_gain_of(c)).fold(0, u64::saturating_add);
        (host >= host_short && g >= gpu_short, host, g)
    };
    let mut chosen: Vec<&ReclaimCandidate> = Vec::new();
    for &c in &candidates {
        let (ok, host, g) = covered(&chosen);
        if ok {
            break;
        }
        let helps_host = host < host_short && c.gain > 0;
        let helps_gpu = g < gpu_short && gpu_gain_of(c) > 0;
        if helps_host || helps_gpu {
            chosen.push(c);
        }
    }
    let (ok, _, _) = covered(&chosen);
    if ok && !chosen.is_empty() {
        // Prune: drop the smallest candidates that are not needed.
        let mut i = chosen.len();
        while i > 0 {
            i -= 1;
            let mut trial = chosen.clone();
            trial.remove(i);
            if covered(&trial).0 {
                chosen = trial;
            }
        }
        let gain: u64 = chosen.iter().map(|c| c.gain).fold(0, u64::saturating_add);
        let mut a = CanFitAnswer {
            fit: Fit::YesAfterReclaim {
                reclaim: chosen.into_iter().cloned().collect(),
                gain,
            },
            reclaimable: gain,
            ..base
        };
        a.reason = reason_with(&a, &fmt);
        return a;
    }

    // Not enough even after reclaiming everything that helps.
    let (_, host_all, gpu_all) = covered(&candidates);
    let host_left = host_short.saturating_sub(host_all);
    let gpu_left = gpu_short.saturating_sub(gpu_all);
    let shortfall = host_left.max(gpu_left);
    let mut a = CanFitAnswer {
        fit: Fit::No { shortfall },
        gpu_shortfall: (gpu_left > 0).then_some(gpu_left),
        host_shortfall: host_left,
        reclaimable: host_all,
        ..base
    };
    a.reason = reason_with(&a, &fmt);
    a
}

fn display(c: &ReclaimCandidate) -> String {
    if c.label.is_empty() {
        c.group_id.clone()
    } else {
        c.label.clone()
    }
}

/// Convenience: headroom + candidates + decision in one call. `exclude_group` removes the caller's own
/// group (e.g. the agent session asking over MCP, or oomtop itself) from the reclaim candidates.
pub fn can_fit_snapshot(
    need: &Need,
    snapshot: &Snapshot,
    cfg: &HeadroomConfig,
    exclude_group: Option<&str>,
) -> CanFitAnswer {
    let h = compute(snapshot, cfg);
    let mut cands = reclaim_candidates(snapshot);
    if let Some(ex) = exclude_group {
        cands.retain(|c| c.group_id != ex);
    }
    can_fit(need, &h, &cands)
}

/// CLI exit code for an answer: 0 yes · 3 yes after reclaim · 4 no.
pub fn exit_code(fit: &Fit) -> i32 {
    match fit {
        Fit::Yes => EXIT_YES,
        Fit::YesAfterReclaim { .. } => EXIT_YES_AFTER_RECLAIM,
        Fit::No { .. } => EXIT_NO,
    }
}

/// Exit code for a full answer: like [`exit_code`], but 1 when the answer could not be measured.
pub fn answer_exit_code(a: &CanFitAnswer) -> i32 {
    if a.error.is_some() {
        EXIT_ERROR
    } else {
        exit_code(&a.fit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Measured;
    use crate::units::GIB;

    fn hr(room: i64) -> Headroom {
        Headroom {
            available_now: Measured::exact(room.max(0) as u64 + 2 * GIB, "test"),
            headroom: Some(room),
            ..Default::default()
        }
    }

    fn cand(id: &str, gain: u64) -> ReclaimCandidate {
        ReclaimCandidate {
            group_id: id.into(),
            gain,
            kind: GroupKind::BuildDaemon,
            ..Default::default()
        }
    }

    #[test]
    fn yes_after_reclaim_and_no() {
        let need = Need::bytes(13 * GIB);
        let a = can_fit(&need, &hr(20 * GIB as i64), &[]);
        assert_eq!(exit_code(&a.fit), EXIT_YES);
        let cands = [cand("gradle", 3 * GIB), cand("kotlin", 3 * GIB)];
        let a = can_fit(&need, &hr(9 * GIB as i64), &cands);
        assert_eq!(exit_code(&a.fit), EXIT_YES_AFTER_RECLAIM);
        match a.fit {
            Fit::YesAfterReclaim { reclaim, gain } => {
                assert_eq!(reclaim.len(), 2);
                assert_eq!(gain, 6 * GIB);
            }
            _ => panic!("expected yes_after_reclaim"),
        }
        let a = can_fit(&need, &hr(2 * GIB as i64), &cands);
        assert_eq!(a.fit, Fit::No { shortfall: 5 * GIB });
        assert_eq!(a.valid_for_s, VALID_FOR_S);
        assert_eq!(answer_exit_code(&a), EXIT_NO);
    }

    #[test]
    fn prune_keeps_the_plan_minimal() {
        // Need 1 GiB more: greedy takes the 5 GiB one first and stops.
        let cands = [cand("big", 5 * GIB), cand("small", 2 * GIB)];
        let a = can_fit(&Need::bytes(10 * GIB), &hr(9 * GIB as i64), &cands);
        match a.fit {
            Fit::YesAfterReclaim { reclaim, gain } => {
                assert_eq!(reclaim.len(), 1);
                assert_eq!(gain, 5 * GIB);
            }
            other => panic!("{other:?}"),
        }
        // Need 6 GiB more with 5 + 2 + 1: greedy takes 5 + 2; prune can't drop either.
        let cands = [cand("a", 5 * GIB), cand("b", 2 * GIB), cand("c", GIB)];
        let a = can_fit(&Need::bytes(15 * GIB), &hr(9 * GIB as i64), &cands);
        match a.fit {
            Fit::YesAfterReclaim { reclaim, gain } => {
                assert_eq!(reclaim.len(), 2);
                assert_eq!(gain, 7 * GIB);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn negative_headroom_and_zero_need() {
        let a = can_fit(&Need::bytes(0), &hr(-(GIB as i64)), &[]);
        assert_eq!(a.fit, Fit::No { shortfall: GIB });
        let a = can_fit(&Need::bytes(0), &hr(0), &[]);
        assert_eq!(a.fit, Fit::Yes);
    }

    #[test]
    fn unavailable_is_an_error() {
        let a = can_fit(&Need::bytes(GIB), &Headroom::default(), &[]);
        assert!(matches!(a.fit, Fit::No { .. }));
        assert!(a.error.is_some());
        assert_eq!(answer_exit_code(&a), EXIT_ERROR);
    }

    fn gpu(id: &str, unified: bool, free: u64, margin: u64) -> GpuHeadroom {
        GpuHeadroom {
            accelerator_id: id.into(),
            unified,
            free: Measured::exact(free, "t"),
            margin,
            headroom: Some(free as i64 - margin as i64),
            ..Default::default()
        }
    }

    #[test]
    fn gpu_budget_checks() {
        // Unified: host need becomes max(bytes, gpu_bytes).
        let mut h = hr(12 * GIB as i64);
        h.gpu.push(gpu("gpu0", true, 14 * GIB, 0));
        let need = Need {
            bytes: 2 * GIB,
            gpu_bytes: Some(13 * GIB),
            label: None,
        };
        let a = can_fit(&need, &h, &[]);
        assert_eq!(a.host_need, 13 * GIB);
        assert_eq!(a.fit, Fit::No { shortfall: GIB });
        // Discrete: VRAM short, host fine; reclaim a GPU-resident server.
        let mut h = hr(40 * GIB as i64);
        h.gpu.push(gpu("gpu0", false, 6 * GIB, GIB));
        h.gpu.push(gpu("gpu1", false, 10 * GIB, GIB));
        let need = Need {
            bytes: 12 * GIB,
            gpu_bytes: Some(12 * GIB),
            label: None,
        };
        let a = can_fit(&need, &h, &[]);
        assert_eq!(a.gpu_device.as_deref(), Some("gpu1"));
        assert_eq!(a.fit, Fit::No { shortfall: 3 * GIB });
        assert_eq!(a.gpu_shortfall, Some(3 * GIB));
        let mut ollama = cand("model:ollama", 2 * GIB);
        ollama.kind = GroupKind::ModelServer;
        ollama.gpu_gain = Some(8 * GIB);
        let a = can_fit(&need, &h, &[cand("gradle", 3 * GIB), ollama]);
        match &a.fit {
            Fit::YesAfterReclaim { reclaim, .. } => {
                assert_eq!(reclaim.len(), 1);
                assert_eq!(reclaim[0].group_id, "model:ollama");
            }
            other => panic!("{other:?}"),
        }
        // GPU need without GPU data: host-only check, noted.
        let a = can_fit(&need, &hr(40 * GIB as i64), &[]);
        assert_eq!(a.fit, Fit::Yes);
        assert!(!a.gpu_checked);
    }

    #[test]
    fn huge_need_never_fits() {
        // 16 EiB-ish need: `as i64` used to wrap negative and answer "yes".
        let a = can_fit(&Need::bytes(u64::MAX - 1), &hr(20 * GIB as i64), &[]);
        assert!(matches!(a.fit, Fit::No { .. }), "{:?}", a.fit);
        assert_eq!(answer_exit_code(&a), EXIT_NO);
        let a = can_fit(&Need::bytes(1 << 63), &hr(i64::MAX), &[]);
        assert_eq!(a.fit, Fit::No { shortfall: 1 });
        let big = [cand("a", u64::MAX), cand("b", u64::MAX)];
        let a = can_fit(&Need::bytes(u64::MAX), &hr(0), &big);
        assert!(matches!(a.fit, Fit::YesAfterReclaim { .. }));
    }

    #[test]
    fn candidate_order_does_not_matter() {
        // Smallest first on input: greedy still takes the largest (one group instead of three).
        let cands = [cand("s1", GIB), cand("s2", GIB), cand("big", 5 * GIB)];
        let a = can_fit(&Need::bytes(12 * GIB), &hr(9 * GIB as i64), &cands);
        match a.fit {
            Fit::YesAfterReclaim { reclaim, gain } => {
                assert_eq!(reclaim.len(), 1);
                assert_eq!(reclaim[0].group_id, "big");
                assert_eq!(gain, 5 * GIB);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unified_host_without_gpu_data_counts_gpu_need_as_ram() {
        let mut h = hr(8 * GIB as i64);
        h.unified_memory = true;
        let need = Need {
            bytes: GIB,
            gpu_bytes: Some(10 * GIB),
            label: None,
        };
        let a = can_fit(&need, &h, &[]);
        assert_eq!(a.host_need, 10 * GIB);
        assert_eq!(a.fit, Fit::No { shortfall: 2 * GIB });
        assert!(!a.gpu_checked);
    }

    #[test]
    fn need_from_estimate() {
        let e = ModelEstimate {
            weights: 8 * GIB,
            kv: GIB,
            overhead: GIB / 2,
            need: 8 * GIB + GIB + GIB / 2,
            ..Default::default()
        };
        let n = Need::from_estimate(&e, true, "m.gguf");
        assert_eq!(n.gpu_bytes, Some(e.need));
        assert_eq!(
            n.label.as_deref(),
            Some("m.gguf (weights 8.0 GiB + kv 1.0 GiB + overhead 512.0 MiB)")
        );
    }

    #[test]
    fn reasons_use_one_unit_system_and_can_be_re_rendered() {
        let h = hr(5 * GIB as i64);
        let cands = [cand("gradle", 3 * GIB)];
        let a = can_fit(&Need::bytes(13 * GIB), &h, &cands);
        assert_eq!(a.host_shortfall, 5 * GIB);
        assert_eq!(a.reclaimable, 3 * GIB);
        assert_eq!(a.reason, "short by 5.0 GiB even after reclaiming ≈3.0 GiB");
        let si = |b: u64| crate::units::format_bytes(b, UnitSystem::Si, 1);
        assert_eq!(
            reason_with(&a, &si),
            "short by 5.4 GB even after reclaiming ≈3.2 GB"
        );
        let a = can_fit(&Need::bytes(7 * GIB), &h, &cands);
        assert_eq!(a.reason, reason_with(&a, &fmt));
        assert!(a.reason.contains("≈3.0 GiB"), "{}", a.reason);
        let a = can_fit(&Need::bytes(13 * GIB), &h, &[]);
        assert_eq!(a.reason, "short by 8.0 GiB and nothing to reclaim");
    }
}

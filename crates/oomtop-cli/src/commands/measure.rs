//! `oomtop headroom` (SPEC §8.1–8.2) and `oomtop why` (SPEC §9).

use super::{json_pretty, out, usage, Ctx};
use crate::engine::Engine;
use crate::{HeadroomArgs, Offload};
use anyhow::{anyhow, Result};
use oomtop_core::can_fit::{answer_exit_code, can_fit, reclaim_candidates, Fit, Need, EXIT_ERROR, EXIT_YES};
use oomtop_core::headroom::{compute, Headroom};
use oomtop_core::model_estimate::{kv_type_bytes, KvParams, ModelEstimate};
use oomtop_core::provider::SnapshotProvider;
use oomtop_core::units::format_duration;
use oomtop_core::why::explain;
use oomtop_core::{PressureLevel, Snapshot, SourceStatus};

/// GPU-resident decision for `--model` (see [`Offload`]).
fn gpu_resident(offload: Offload, s: &Snapshot) -> bool {
    match offload {
        Offload::Gpu => true,
        Offload::Cpu => false,
        Offload::Auto => {
            s.host.unified_memory
                && s.accelerators
                    .iter()
                    .any(|a| a.unified && a.gpu_budget.is_available())
        }
    }
}

fn estimate_line(ctx: &Ctx, est: &ModelEstimate) -> String {
    let mut s = format!(
        "Estimate: {} = weights {} + KV {} + overhead {}",
        ctx.fmt(est.need),
        ctx.fmt(est.weights),
        ctx.fmt(est.kv),
        ctx.fmt(est.overhead)
    );
    if let Some(c) = est.ctx {
        s.push_str(&format!(" (ctx {c})"));
    }
    if est.fallback {
        s.push_str(" — no metadata, 1.2× file size");
    }
    if !est.note.is_empty() {
        s.push_str(&format!(" — {}", est.note));
    }
    s.push('\n');
    s
}

fn pressure_word(p: Option<PressureLevel>) -> &'static str {
    match p {
        Some(PressureLevel::Normal) => "normal",
        Some(PressureLevel::Warn) => "warn",
        Some(PressureLevel::Critical) => "critical",
        None => "unknown",
    }
}

fn headroom_text(ctx: &Ctx, s: &Snapshot, h: &Headroom) -> String {
    let mut o = String::new();
    match h.headroom {
        Some(x) if x >= 0 => o.push_str(&format!(
            "{} headroom — {} available minus {} safety margin.\n",
            ctx.fmt(x as u64),
            ctx.fmt_opt(h.available_now.value),
            ctx.fmt(h.safety_margin)
        )),
        Some(x) => o.push_str(&format!(
            "Tight: {} below the safety margin ({} available, margin {}).\n",
            ctx.fmt(x.unsigned_abs()),
            ctx.fmt_opt(h.available_now.value),
            ctx.fmt(h.safety_margin)
        )),
        None => o.push_str(&format!(
            "Headroom unavailable: {}.\n",
            h.available_now
                .unavailable_reason()
                .unwrap_or("available memory could not be measured")
        )),
    }
    let reclaim = match h.reclaimable.value {
        Some(r) if r > 0 => format!(" · reclaimable ≈{} (oomtop reclaim --dry-run)", ctx.fmt(r)),
        _ => String::new(),
    };
    o.push_str(&format!(
        "Pressure {} · swap {}{}\n",
        pressure_word(h.pressure),
        if h.swap_growing { "growing" } else { "steady" },
        reclaim
    ));
    for g in &h.gpu {
        match (g.free.value, g.budget.value) {
            (Some(f), Some(b)) => o.push_str(&format!(
                "GPU {}: {} free of {} {}\n",
                g.name,
                ctx.fmt(f),
                ctx.fmt(b),
                if g.unified { "Metal budget" } else { "VRAM" }
            )),
            _ => o.push_str(&format!(
                "GPU {}: memory unavailable{}\n",
                g.name,
                g.free
                    .unavailable_reason()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            )),
        }
    }
    if let Some(f) = &s.oom.forecast {
        o.push_str(&format!(
            "Forecast: {} in ~{} at {}/min (confidence {:.2})\n",
            match f.target {
                oomtop_core::ForecastTarget::SwapExhaustion => "swap full",
                oomtop_core::ForecastTarget::AvailableExhaustion => "memory exhausted",
                oomtop_core::ForecastTarget::KillerThreshold => "OOM-killer threshold",
            },
            format_duration(f.eta_s),
            ctx.fmt(f.rate_per_min.max(0.0) as u64),
            f.confidence
        ));
    }
    for n in &h.notes {
        o.push_str(&format!("note: {n}\n"));
    }
    o
}

pub fn headroom(ctx: &Ctx, a: HeadroomArgs) -> Result<i32> {
    let estimate = match &a.model {
        Some(p) => {
            let kv_bytes = kv_type_bytes(&a.kv_type).ok_or_else(|| {
                anyhow::Error::new(super::UsageError(format!(
                    "--kv-type {:?}: expected f32, f16, bf16, q8_0, q5_1, q5_0, q4_1, q4_0 or iq4_nl",
                    a.kv_type
                )))
            })?;
            if a.parallel == 0 {
                return usage("--parallel must be at least 1");
            }
            let kv = KvParams {
                ctx: a.ctx,
                kv_type_bytes: kv_bytes,
                n_parallel: a.parallel,
            };
            Some(
                oomtop_adapters::estimate_model_file(p, &kv)
                    .map_err(|e| anyhow!("--model {}: {e}", p.display()))?,
            )
        }
        None => None,
    };
    let mut e = ctx.engine(true)?;
    let s = e.snapshot();
    let h = compute(&s, &e.headroom_config());

    let need = match (&estimate, a.need, a.gpu) {
        (Some(est), _, gpu) => {
            let label = a
                .model
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "model".into());
            let mut n = Need::from_estimate(est, gpu_resident(a.offload, &s), label);
            if gpu.is_some() {
                n.gpu_bytes = gpu;
            }
            Some(n)
        }
        (None, Some(bytes), gpu) => Some(Need {
            bytes,
            gpu_bytes: gpu,
            label: Some(ctx.fmt(bytes)),
        }),
        (None, None, Some(gpu)) => Some(Need {
            bytes: 0,
            gpu_bytes: Some(gpu),
            label: Some(format!("{} GPU", ctx.fmt(gpu))),
        }),
        (None, None, None) => None,
    };

    let Some(need) = need else {
        if a.json {
            out(&json_pretty(&serde_json::json!({
                "headroom": h,
                "forecast": s.oom.forecast,
                "likely_victim": s.oom.likely_victim,
            }))?);
        } else {
            out(&headroom_text(ctx, &s, &h));
        }
        return Ok(if h.headroom.is_some() {
            EXIT_YES
        } else {
            EXIT_ERROR
        });
    };

    let mut cands = reclaim_candidates(&s);
    if let Some(me) = Engine::self_group(&s) {
        cands.retain(|c| c.group_id != me);
    }
    let ans = can_fit(&need, &h, &cands);
    let code = answer_exit_code(&ans);
    if a.json {
        let mut v = serde_json::to_value(&ans)?;
        if let (Some(est), Some(obj)) = (&estimate, v.as_object_mut()) {
            obj.insert("estimate".into(), serde_json::to_value(est)?);
        }
        out(&json_pretty(&v)?);
        return Ok(code);
    }
    if let Some(est) = &estimate {
        out(&estimate_line(ctx, est));
    }
    let what = need.label.clone().unwrap_or_else(|| ctx.fmt(need.bytes));
    if let Some(err) = &ans.error {
        out(&format!("Can't tell: {err}.\n"));
        return Ok(code);
    }
    match &ans.fit {
        Fit::Yes => out(&format!(
            "Yes: {what} fits — {} headroom.\n",
            ctx.fmt(ans.headroom.unwrap_or(0).max(0) as u64)
        )),
        Fit::YesAfterReclaim { reclaim, gain } => {
            let list: Vec<String> = reclaim
                .iter()
                .map(|c| {
                    let word = if s.group(&c.group_id).is_some_and(|g| g.idle) {
                        "idle"
                    } else {
                        "quiet"
                    };
                    let idle = c
                        .idle_for_s
                        .filter(|s| *s > 0)
                        .map(|secs| format!(", {word} {}", format_duration(secs)))
                        .unwrap_or_default();
                    format!("{} (≈{}{idle})", c.label, ctx.fmt(c.gain))
                })
                .collect();
            out(&format!(
                "Yes, after reclaim: stopping {} frees ≈{}, then {what} fits.\n",
                list.join(" and "),
                ctx.fmt(*gain)
            ));
            let ids: Vec<&str> = reclaim.iter().map(|c| c.group_id.as_str()).collect();
            out(&format!("Run: oomtop reclaim --groups {}\n", ids.join(",")));
        }
        Fit::No { shortfall } => {
            if ans.reason.is_empty() {
                out(&format!("No: {what} is short by {}.\n", ctx.fmt(*shortfall)));
            } else {
                // The core's reason re-rendered in the configured units, so the sentence never mixes them.
                let reason = oomtop_core::can_fit::reason_with(&ans, &|b| ctx.fmt(b));
                out(&format!("No: {what} doesn't fit — {reason}.\n"));
            }
        }
    }
    for n in &ans.notes {
        out(&format!("note: {n}\n"));
    }
    out(&format!("(answer valid for ~{} s)\n", ans.valid_for_s));
    Ok(code)
}

pub fn why(ctx: &Ctx, json: bool) -> Result<i32> {
    let mut e = ctx.engine(true)?;
    let mut s = e.snapshot();
    // Swap in/out rates need two samples (a replay's first frame has none): take one more so a swap storm
    // shows up next to throttling instead of being silently absent (SPEC §9).
    if s.memory.swap_in_per_min.value.is_none() && s.memory.swap_out_per_min.value.is_none() {
        let next = e.snapshot();
        if next.taken_at_ms > s.taken_at_ms {
            s = next;
        }
    }
    let causes = explain(&s, e.history());
    if json {
        out(&json_pretty(&causes)?);
        return Ok(0);
    }
    if causes.is_empty() {
        out("Nothing looks wrong: no memory pressure, swap storm or throttling detected.\n");
    }
    for (i, c) in causes.iter().enumerate() {
        out(&format!("{}. {}\n", i + 1, c.title));
        for ev in &c.evidence {
            out(&format!("   · {ev}\n"));
        }
        if let Some(f) = &c.fix {
            out(&format!("   fix: {f}\n"));
        }
    }
    let unavailable: Vec<String> = s
        .source_status
        .iter()
        .filter(|(_, st)| matches!(st, SourceStatus::Unavailable(_)))
        .map(|(k, _)| k.clone())
        .collect();
    if !unavailable.is_empty() {
        out(&format!(
            "(not measured: {} — see `oomtop doctor`)\n",
            unavailable.join(", ")
        ));
    }
    Ok(0)
}

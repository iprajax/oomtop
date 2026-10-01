//! `oomtop reclaim` (SPEC §7, §8.1, §13): idle build daemons, orphans and idle model servers.
//!
//! Safety rules, in order:
//! 1. Only reclaim candidates (`can_fit::reclaim_candidates`), never oomtop's own group, never a group whose
//!    root is protected or owned by another user (`actions::is_protected_process`), group roots only.
//! 2. Model servers with an adapter action are unloaded gently (no signal) before anything else.
//! 3. Nothing happens without confirmation: an interactive `[y/N]`, or `--yes` for exactly the listed plan.
//! 4. SIGTERM first. SIGKILL only for groups still alive after `--wait`, and only after a second,
//!    interactive confirmation — `--yes` never escalates.
//! 5. `--replay` never signals anything (the fixture's pids are not this machine's processes).

use super::{confirm, json_pretty, out, Ctx};
use crate::actuator::SignalActuator;
use crate::engine::Engine;
use crate::ReclaimArgs;
use anyhow::{bail, Result};
use oomtop_adapters::{stop_guard, StopGuard};
use oomtop_core::actions::{
    is_protected_process, plan_group, ActionKind, ActionOutcome, ActionPlan, Actuator,
};
use oomtop_core::can_fit::{reclaim_candidates, ReclaimCandidate, EXIT_ERROR};
use oomtop_core::provider::SnapshotProvider;
use oomtop_core::units::format_duration;
use oomtop_core::Snapshot;
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize)]
struct PlannedRow {
    #[serde(flatten)]
    candidate: ReclaimCandidate,
    /// "sigterm" | "graceful"
    action: &'static str,
    /// Human description (e.g. "ollama: unload llama3:8b").
    detail: String,
    /// The group is idle by the SPEC §7 rule (not just quiet for a while).
    idle: bool,
}

#[derive(Debug, Clone, Serialize)]
struct Skipped {
    group_id: String,
    reason: String,
}

/// Plans the reclaim: returns the confirmed-able plan, the rows shown to the user, and what was skipped.
fn plan(
    s: &Snapshot,
    engine: &Engine,
    actuator: &SignalActuator,
    only: &[String],
) -> (ActionPlan, Vec<PlannedRow>, Vec<Skipped>) {
    let me = Engine::self_group(s);
    let mut cands = reclaim_candidates(s);
    cands.retain(|c| Some(&c.group_id) != me.as_ref());
    let mut skipped = Vec::new();
    if !only.is_empty() {
        for id in only {
            if !cands.iter().any(|c| &c.group_id == id) {
                let reason = match s.group(id) {
                    None => "no such group in this snapshot".to_string(),
                    Some(_) if Some(id) == me.as_ref() => "oomtop's own group".to_string(),
                    Some(_) => {
                        "not a reclaim candidate (not idle, orphaned or an idle model server)".to_string()
                    }
                };
                skipped.push(Skipped {
                    group_id: id.clone(),
                    reason,
                });
            }
        }
        cands.retain(|c| only.contains(&c.group_id));
    }
    let mut plan = ActionPlan::default();
    let mut rows = Vec::new();
    for c in cands {
        let Some(g) = s.group(&c.group_id) else { continue };
        let gentle = actuator.gentle_for(&g.id);
        let kind = if gentle.is_empty() {
            ActionKind::Terminate
        } else {
            ActionKind::Graceful
        };
        let mut t = match plan_group(g, kind, &engine.protect) {
            Ok(t) => t,
            Err(r) => {
                skipped.push(Skipped {
                    group_id: c.group_id.clone(),
                    reason: r.to_string(),
                });
                continue;
            }
        };
        // Other users' processes and protected names/ancestors are never targets (SPEC §13).
        if let Some(root) = s.process(t.root) {
            if is_protected_process(root, s, &engine.protect) {
                skipped.push(Skipped {
                    group_id: c.group_id.clone(),
                    reason: format!(
                        "root {} (pid {}) is protected or owned by another user",
                        root.name, root.id.pid
                    ),
                });
                continue;
            }
        } else {
            skipped.push(Skipped {
                group_id: c.group_id.clone(),
                reason: "root process not in this snapshot".into(),
            });
            continue;
        }
        // SPEC §10: never interrupt a running job silently. A busy server is skipped unless it was named
        // with --groups; an unconfirmed idle state is said out loud in the plan.
        let mut guard_note = String::new();
        if kind == ActionKind::Terminate {
            if let Some(ms) = s
                .model_servers
                .iter()
                .find(|m| m.group_id.as_deref() == Some(g.id.as_str()))
            {
                match stop_guard(ms) {
                    StopGuard::Allowed => {}
                    StopGuard::Busy(why) if !only.contains(&g.id) => {
                        skipped.push(Skipped {
                            group_id: c.group_id.clone(),
                            reason: format!("busy ({why}); name it with --groups to stop it anyway"),
                        });
                        continue;
                    }
                    StopGuard::Busy(why) => guard_note = format!(" — interrupts {why}"),
                    StopGuard::Unknown(why) => guard_note = format!(" — idle not confirmed ({why})"),
                }
            }
        }
        let detail = if kind == ActionKind::Graceful {
            let d: Vec<String> = gentle.iter().map(|a| a.describe()).collect();
            let d = d.join("; ");
            t.graceful = Some(d.clone());
            d
        } else {
            format!("SIGTERM pid {}{guard_note}", t.root.pid)
        };
        rows.push(PlannedRow {
            candidate: c,
            action: if kind == ActionKind::Graceful {
                "graceful"
            } else {
                "sigterm"
            },
            detail,
            idle: g.idle,
        });
        plan.targets.push(t);
    }
    (plan, rows, skipped)
}

fn print_plan(ctx: &Ctx, rows: &[PlannedRow], skipped: &[Skipped]) {
    if rows.is_empty() {
        out("Nothing to reclaim: no idle build daemons, orphans or idle model servers.\n");
    } else {
        out("Reclaim candidates (RAM freed is an estimate):\n");
        for r in rows {
            let c = &r.candidate;
            let mut tags = Vec::new();
            if let Some(s) = c.idle_for_s.filter(|s| *s > 0) {
                // "idle" only when the group meets the idle rule (SPEC §7: 30 min below the CPU threshold);
                // a daemon that has merely been quiet for a while says so.
                let word = if r.idle { "idle" } else { "quiet" };
                tags.push(format!("{word} {}", format_duration(s)));
            }
            if c.orphan {
                tags.push("orphan".into());
            }
            if let Some(sw) = c.swap_gain.filter(|s| *s > 0) {
                tags.push(format!("+{} swap", ctx.fmt(sw)));
            }
            let tags = if tags.is_empty() {
                String::new()
            } else {
                format!(" · {}", tags.join(" · "))
            };
            out(&format!(
                "  {:<28} {:<7} ≈{:>10}  {}{tags}\n      {}\n",
                truncate(&c.label, 28),
                c.kind.alias(),
                ctx.fmt(c.gain),
                r.detail,
                c.group_id
            ));
        }
        let total: u64 = rows.iter().map(|r| r.candidate.gain).sum();
        out(&format!("Total ≈{} RAM\n", ctx.fmt(total)));
    }
    for s in skipped {
        out(&format!("  skipped {}: {}\n", s.group_id, s.reason));
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// Waits until every target's root is gone (identity-checked) or the deadline passes; returns survivors.
fn wait_for_exit(plan: &ActionPlan, wait: Duration) -> Vec<usize> {
    let deadline = Instant::now() + wait;
    loop {
        let alive: Vec<usize> = plan
            .targets
            .iter()
            .enumerate()
            .filter(|(_, t)| t.kind == ActionKind::Terminate && oomtop_collect::signal::is_alive(t.root))
            .map(|(i, _)| i)
            .collect();
        if alive.is_empty() || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn run(ctx: &Ctx, a: ReclaimArgs) -> Result<i32> {
    let mut e = ctx.engine(true)?;
    let s = e.snapshot();
    let mut actuator = SignalActuator::new();
    actuator.register_gentle(&s);
    let (mut plan, rows, skipped) = plan(&s, &e, &actuator, &a.groups);
    let total: u64 = rows.iter().map(|r| r.candidate.gain).sum();

    if a.dry_run || plan.targets.is_empty() {
        if a.json {
            out(&json_pretty(&serde_json::json!({
                "dry_run": a.dry_run,
                "candidates": rows,
                "skipped": skipped,
                "total_gain": total,
                "as_of_ms": s.taken_at_ms,
            }))?);
        } else {
            print_plan(ctx, &rows, &skipped);
        }
        return Ok(0);
    }
    if !a.json {
        print_plan(ctx, &rows, &skipped);
    }
    if !a.yes {
        let q = format!(
            "Stop {} group(s) (gentle unload / SIGTERM), freeing ≈{}?",
            plan.targets.len(),
            ctx.fmt(total)
        );
        match confirm(&q)? {
            Some(true) => {}
            Some(false) => {
                out("Cancelled; nothing was stopped.\n");
                return Ok(0);
            }
            None => bail!(
                "refusing to stop processes without confirmation: stdin is not a terminal (review with --dry-run, then pass --yes)"
            ),
        }
    }
    if !e.is_live() {
        out(
            "Replay mode: plan confirmed but nothing executed (fixture processes are not on this machine).\n",
        );
        return Ok(0);
    }
    plan.confirmed = true;
    let before = s.memory.available.value;
    let mut outcomes: Vec<ActionOutcome> = actuator.execute(&plan);
    for o in &outcomes {
        if !a.json {
            out(&format!(
                "  {}: {}{}\n",
                o.target.label,
                if o.ok { "" } else { "failed: " },
                o.message
            ));
        }
    }
    // Only targets that were actually signalled can survive.
    let mut sent = plan.clone();
    sent.targets = outcomes
        .iter()
        .filter(|o| o.ok)
        .map(|o| o.target.clone())
        .collect();
    let survivors = wait_for_exit(&sent, a.wait);
    if !survivors.is_empty() {
        let names: Vec<String> = survivors.iter().map(|i| sent.targets[*i].label.clone()).collect();
        out(&format!(
            "Still running after {}: {}\n",
            format_duration(a.wait.as_secs().max(1)),
            names.join(", ")
        ));
        // Second, explicit confirmation — never implied by --yes.
        let q = format!("Force-stop {} group(s) with SIGKILL?", survivors.len());
        match confirm(&q)? {
            Some(true) => {
                let kill = ActionPlan {
                    targets: survivors
                        .iter()
                        .map(|i| {
                            let mut t = sent.targets[*i].clone();
                            t.kind = ActionKind::Kill;
                            t
                        })
                        .collect(),
                    confirmed: true,
                    confirmed_kill: true,
                };
                let ko = actuator.execute(&kill);
                for o in &ko {
                    if !a.json {
                        out(&format!("  {}: {}\n", o.target.label, o.message));
                    }
                }
                outcomes.extend(ko);
            }
            _ => out("Left running (SIGKILL needs an interactive second confirmation).\n"),
        }
    }
    // Measure what was actually freed.
    std::thread::sleep(Duration::from_millis(500));
    let after = e.snapshot().memory.available.value;
    let freed = match (before, after) {
        (Some(b), Some(a)) => Some(a.saturating_sub(b)),
        _ => None,
    };
    let ok = outcomes.iter().all(|o| o.ok);
    if a.json {
        out(&json_pretty(&serde_json::json!({
            "dry_run": false,
            "outcomes": outcomes,
            "skipped": skipped,
            "estimated_gain": total,
            "measured_gain": freed,
        }))?);
    } else {
        out(&format!(
            "Freed {} (estimated ≈{}); memory settles over a few seconds.\n",
            ctx.fmt_opt(freed),
            ctx.fmt(total)
        ));
    }
    Ok(if ok { 0 } else { EXIT_ERROR })
}

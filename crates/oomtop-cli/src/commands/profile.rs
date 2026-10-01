//! `oomtop profile show | export | reset | stats` (UX §5.6, §6). Everything is local SQLite under
//! `$XDG_STATE_HOME/oomtop/`; nothing is sent anywhere.

use super::{confirm, json_pretty, out, Ctx};
use crate::profile::MachineProfile;
use crate::ProfileCmd;
use anyhow::{bail, Context, Result};
use oomtop_core::units::format_duration;

pub fn run(ctx: &Ctx, cmd: ProfileCmd) -> Result<i32> {
    let mut db = oomtop_state::StateDb::open_default().context("opening the state database")?;
    let now = oomtop_collect::now_ms();
    let hl = ctx.config().personalization.half_life_days * 86400.0;
    match cmd {
        ProfileCmd::Show { json } => {
            let ents = db.entities()?;
            let fr = db.frecency(now, hl)?;
            let machine = db.machine_profile()?;
            let mp: Option<MachineProfile> = machine
                .as_ref()
                .and_then(|(j, _)| serde_json::from_value(j.clone()).ok());
            let mut rows: Vec<_> = ents
                .iter()
                .map(|e| (fr.get(&e.fingerprint).copied().unwrap_or(0.0), e))
                .collect();
            rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            if json {
                out(&json_pretty(&serde_json::json!({
                    "path": db.path(),
                    "learning": !ctx.no_learn,
                    "machine_profile": machine.map(|(j, _)| j),
                    "entities": rows.iter().map(|(score, e)| serde_json::json!({
                        "fingerprint": e.fingerprint,
                        "name": e.name(),
                        "score": score,
                        "pinned": e.pinned,
                        "muted": e.is_muted(now),
                    })).collect::<Vec<_>>(),
                }))?);
                return Ok(0);
            }
            out(&format!(
                "profile {}{}\n",
                db.path().map(|p| p.display().to_string()).unwrap_or_default(),
                if ctx.no_learn {
                    " · learning off (--no-learn / personalization.learn = false)"
                } else {
                    ""
                }
            ));
            match &mp {
                Some(p) => {
                    let hw = &p.hardware;
                    out(&format!(
                        "\nMachine   {} · {} · {} RAM{}{}{}\n",
                        hw.model.clone().unwrap_or_else(|| hw.arch.clone()),
                        hw.cpu.clone().unwrap_or_default(),
                        ctx.fmt(hw.ram),
                        if hw.unified_memory { " unified" } else { "" },
                        if hw.fanless == Some(true) {
                            " · fanless"
                        } else {
                            ""
                        },
                        if hw.has_battery { " · battery" } else { "" }
                    ));
                    for g in &hw.gpus {
                        out(&format!(
                            "GPU       {} ({}{})\n",
                            g.name,
                            g.vendor,
                            g.vram
                                .map(|v| format!(", {} VRAM", ctx.fmt(v)))
                                .unwrap_or_default()
                        ));
                    }
                    if p.roles.is_empty() {
                        out("Roles     none detected yet\n");
                    }
                    for (r, seen) in &p.roles {
                        out(&format!(
                            "Role      {:<45} seen {} ago\n",
                            r.describe(),
                            format_duration(now.saturating_sub(*seen) / 1000)
                        ));
                    }
                    out(&format!(
                        "Adapts    gpu column {} · thermal-prone {} · insights: {}\n",
                        if p.adaptations.gpu_column { "on" } else { "off" },
                        if p.adaptations.thermal_prone { "yes" } else { "no" },
                        p.adaptations.insights.join(", ")
                    ));
                    if p.model_files > 0 {
                        out(&format!(
                            "Models    {} model files in models.folders\n",
                            p.model_files
                        ));
                    }
                    out(&format!(
                        "Updated   {} ago (refreshed daily)\n",
                        format_duration(now.saturating_sub(p.updated_ms) / 1000)
                    ));
                }
                None => out("\nMachine   not profiled yet (built on the next `oomtop` run)\n"),
            }
            out(&format!("\nEntities  {} remembered\n", ents.len()));
            for (score, e) in rows.iter().take(30) {
                let mut tags = Vec::new();
                if e.pinned {
                    tags.push("pinned");
                }
                if e.is_muted(now) {
                    tags.push("muted");
                }
                if e.alias.is_some() {
                    tags.push("renamed");
                }
                out(&format!("  {score:>7.2}  {:<40} {}\n", e.name(), tags.join(" ")));
            }
            Ok(0)
        }
        ProfileCmd::Export => {
            out(&json_pretty(&db.export_profile(now, hl)?)?);
            Ok(0)
        }
        ProfileCmd::Reset { yes } => {
            if !yes {
                match confirm("Forget pins, mutes, renames, learned ranking and the machine profile?")? {
                    Some(true) => {}
                    Some(false) => {
                        out("Cancelled.\n");
                        return Ok(0);
                    }
                    None => {
                        bail!("refusing to reset without confirmation (stdin is not a terminal; pass --yes)")
                    }
                }
            }
            db.reset_profile()?;
            db.flush()?;
            out("profile reset: ranking is back to salience-only defaults (lineage journal kept)\n");
            Ok(0)
        }
        ProfileCmd::Stats { json } => {
            let s = db.ux_stats(now.saturating_sub(oomtop_state::RETENTION_MS))?;
            if json {
                out(&json_pretty(&s)?);
                return Ok(0);
            }
            let pct = |v: Option<f64>| {
                v.map(|x| format!("{:.0}%", x * 100.0))
                    .unwrap_or_else(|| "n/a".into())
            };
            out(&format!(
                "last 30 days: {} impressions · {} selections\n\
                 Hit@3 (target in top 3 without typing)  {}\n\
                 mean keystrokes to target               {}\n\
                 query reformulation rate                {}\n\
                 dismiss / mute rate                     {}\n",
                s.impressions,
                s.selections,
                pct(s.hit_at_3),
                s.mean_keystrokes
                    .map(|k| format!("{k:.1}"))
                    .unwrap_or_else(|| "n/a".into()),
                pct(s.reformulation_rate),
                pct(s.dismiss_rate)
            ));
            Ok(0)
        }
    }
}

//! Headline sentence (UX §9): template-based, deterministic, one line, with the one best action key.
//!
//! ```text
//! [mode summary] — [key number]. [top owner + amount]; [best action + gain].
//! "Tight on memory — 1.2 GB headroom. sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB."
//! "Running at 45% speed — thermal pressure heavy, on battery (29%). Plug in or pause generation."
//! "Swap full in ~6 min at this rate — sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB."
//! "All good — 11 GB free. Your things: sd-server idle, 4 Claude Code sessions."
//! ```
//!
//! An OOM forecast is the top insight (UX §5.5) and takes the headline in every mode. Amounts use one decimal
//! below 10 units and none above ("9.9 GB", "11 GB"), in SI units unless the input
//! asks for IEC (`units = "iec"` → "GiB"). Unavailable values are never rendered as zero: the template
//! drops the clause or says "unknown".

use crate::can_fit::reclaim_candidates;
use crate::headroom::Headroom;
use crate::model::{ForecastTarget, GroupKind, OomKiller, Snapshot, ThermalPressure};
use crate::modes::Mode;
use crate::throttle::THROTTLED_BELOW;
use crate::units::{format_duration, UnitSystem};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Everything the templates need (built by [`input_from`] or by the TUI with personalization).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HeadlineInput {
    pub mode: Mode,
    pub headroom: Option<i64>,
    /// Available memory ("11 GB free").
    pub free: Option<u64>,
    pub top_owner: Option<(String, u64)>,
    /// (description e.g. "2 idle build daemons", gain bytes).
    pub best_action: Option<(String, u64)>,
    pub throttle_factor: Option<f64>,
    pub thermal: Option<ThermalPressure>,
    pub on_battery_pct: Option<f64>,
    pub low_power_mode: bool,
    pub forecast_eta_s: Option<u64>,
    pub forecast_target: Option<ForecastTarget>,
    /// Short chips, e.g. "sd-server idle", "4 Claude Code sessions" (see [`your_things_chips`]).
    pub your_things: Vec<String>,
    /// What is working, e.g. "sd-server generating 3/6 · 8.1 s/step, ~24s left".
    pub working: Option<String>,
    pub orphans: u32,
    /// "iec" → GiB; anything else (or `None`) → GB.
    pub units: Option<String>,
    /// Which killer acts at a `KillerThreshold` forecast.
    pub forecast_killer: Option<OomKiller>,
    /// A model is generating (throttle advice says "pause generation").
    pub generating: bool,
    /// Linux thermal trip point hit.
    pub trip_point_hit: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Headline {
    pub text: String,
    /// The one best action key (e.g. 'r' reclaim), if any.
    pub action_key: Option<char>,
    pub mode: Mode,
}

/// Human amount: one decimal below 10 units, none above ("9.9 GB", "11 GB", "512 MB").
pub fn format_amount(bytes: u64, system: UnitSystem) -> String {
    let (base, units): (f64, [&str; 5]) = match system {
        UnitSystem::Iec => (1024.0, ["B", "KiB", "MiB", "GiB", "TiB"]),
        UnitSystem::Si => (1000.0, ["B", "KB", "MB", "GB", "TB"]),
    };
    let mut v = bytes as f64;
    let mut i = 0;
    // Promote when the *rounded* value reaches the next unit (999.96 MB → "1.0 GB", not "1000 MB").
    while (v >= base || (i > 0 && v.round() >= base)) && i < units.len() - 1 {
        v /= base;
        i += 1;
    }
    if i == 0 {
        return format!("{bytes} B");
    }
    let one = (v * 10.0).round() / 10.0;
    if one >= 10.0 {
        format!("{:.0} {}", v, units[i])
    } else {
        format!("{one:.1} {}", units[i])
    }
}

/// ETA wording: "under a minute", "~6 min", "~1 h 5 min".
pub fn format_eta(secs: u64) -> String {
    if secs < 60 {
        return "under a minute".into();
    }
    let mins = (secs + 30) / 60;
    if mins < 60 {
        format!("~{mins} min")
    } else {
        let (h, m) = (mins / 60, mins % 60);
        if m == 0 {
            format!("~{h} h")
        } else {
            format!("~{h} h {m} min")
        }
    }
}

fn kind_noun(kind: GroupKind, n: usize) -> String {
    let base = match kind {
        GroupKind::BuildDaemon => "build daemon",
        GroupKind::ModelServer => "model server",
        GroupKind::AgentSession => "agent session",
        GroupKind::Sandbox => "sandbox",
        GroupKind::App => "app",
        GroupKind::System => "system process",
        GroupKind::Other => "process group",
    };
    match (n, kind) {
        (1, _) => base.to_string(),
        (_, GroupKind::Sandbox) => "sandboxes".into(),
        (_, GroupKind::System) => "system processes".into(),
        _ => format!("{base}s"),
    }
}

/// Describes a reclaim set: "2 idle build daemons", "idle GradleDaemon", "3 leftovers", "2 idle groups".
fn describe_reclaim(items: &[(String, GroupKind, bool, bool)]) -> String {
    // (label, kind, idle, orphan)
    let n = items.len();
    if n == 0 {
        return "nothing".into();
    }
    let all_idle = items.iter().all(|i| i.2);
    let all_orphan = items.iter().all(|i| i.3);
    if n == 1 {
        let (label, _, idle, orphan) = &items[0];
        return if *orphan {
            format!("orphaned {label}")
        } else if *idle {
            format!("idle {label}")
        } else {
            label.clone()
        };
    }
    let kind = items[0].1;
    let same_kind = items.iter().all(|i| i.1 == kind);
    let adj = if all_orphan {
        "orphaned "
    } else if all_idle {
        "idle "
    } else {
        ""
    };
    if same_kind {
        format!("{n} {adj}{}", kind_noun(kind, n))
    } else if items.iter().all(|i| i.2 || i.3) && !all_idle && !all_orphan {
        format!("{n} leftovers")
    } else {
        format!("{n} {adj}groups")
    }
}

fn working_from(s: &Snapshot) -> Option<(String, bool)> {
    if let Some(m) = s.model_servers.iter().find(|m| m.busy.value == Some(true)) {
        let name = m
            .group_id
            .as_deref()
            .and_then(|g| s.group(g))
            .map(|g| g.label.clone())
            // A model name can be a path; the headline only ever shows its basename.
            .or_else(|| {
                m.models
                    .first()
                    .map(|x| x.name.rsplit(['/', '\\']).next().unwrap_or("").to_string())
                    .filter(|n| !n.is_empty())
            })
            .unwrap_or_else(|| m.id.clone());
        let mut text = name;
        let mut remaining_s = None;
        match &m.progress {
            Some(p) if p.total > 0 => {
                let label = if p.label.is_empty() {
                    "generating"
                } else {
                    p.label.as_str()
                };
                text.push_str(&format!(" {label} {}/{}", p.done.min(p.total), p.total));
                if let Some(sps) = m.s_per_step.value.filter(|x| x.is_finite() && *x > 0.0) {
                    text.push_str(&format!(" · {sps:.1} s/step"));
                    remaining_s = Some((p.total.saturating_sub(p.done)) as f64 * sps);
                }
            }
            _ => {
                text.push_str(" generating");
                if let Some(t) = m.tok_s.value.filter(|x| x.is_finite() && *x > 0.0) {
                    text.push_str(&format!(" · {t:.0} tok/s"));
                } else if let Some(sps) = m.s_per_step.value.filter(|x| x.is_finite() && *x > 0.0) {
                    text.push_str(&format!(" · {sps:.1} s/step"));
                }
            }
        }
        if let Some(r) = remaining_s.filter(|r| *r >= 1.0) {
            text.push_str(&format!(", ~{} left", format_duration(r.round() as u64)));
        }
        return Some((text, true));
    }
    s.groups
        .iter()
        .filter(|g| {
            g.kind == GroupKind::BuildDaemon
                && !g.idle
                && g.totals.cpu_pct.value.unwrap_or(0.0) >= crate::modes::WORKING_CPU_PCT
        })
        .max_by(|a, b| {
            a.totals
                .cpu_pct
                .value
                .unwrap_or(0.0)
                .partial_cmp(&b.totals.cpu_pct.value.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|g| (format!("{} building", g.label), false))
}

/// Builds the input from a snapshot + headroom + current mode (no personalization; the caller adds
/// `your_things` and may override `units`).
pub fn input_from(s: &Snapshot, h: &Headroom, mode: Mode) -> HeadlineInput {
    let cands = reclaim_candidates(s);
    let cand_ids: Vec<&str> = cands.iter().map(|c| c.group_id.as_str()).collect();
    let top = s
        .groups
        .iter()
        .filter(|g| g.kind != GroupKind::System && !g.is_self && !cand_ids.contains(&g.id.as_str()))
        .filter_map(|g| Some((g, g.totals.footprint.value?)))
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.id.cmp(&a.0.id)))
        .map(|(g, b)| (g.label.clone(), b));
    let best_action = if cands.is_empty() {
        None
    } else {
        let items: Vec<(String, GroupKind, bool, bool)> = cands
            .iter()
            .map(|c| {
                let g = s.group(&c.group_id);
                (
                    c.label.clone(),
                    c.kind,
                    g.map(|g| g.idle).unwrap_or(false),
                    c.orphan,
                )
            })
            .collect();
        let gain: u64 = cands.iter().map(|c| c.gain).sum();
        Some((describe_reclaim(&items), gain))
    };
    let working = working_from(s);
    HeadlineInput {
        mode,
        headroom: h.headroom,
        // The same resolved "available now" headroom uses (own-cgroup cap, the tighter macOS
        // memorystatus view), so the first sentence never contradicts the header below it.
        free: h.available_now.value.or(s.memory.available.value),
        top_owner: top,
        best_action,
        throttle_factor: s.thermal.throttle_factor.value,
        thermal: s.thermal.pressure.value,
        on_battery_pct: if s.thermal.on_battery.value == Some(true) {
            s.thermal.battery_pct.value
        } else {
            None
        },
        low_power_mode: s.thermal.low_power_mode.value == Some(true),
        forecast_eta_s: s.oom.forecast.as_ref().map(|f| f.eta_s),
        forecast_target: s.oom.forecast.as_ref().map(|f| f.target),
        forecast_killer: s.oom.forecast.as_ref().and_then(|f| f.killer),
        your_things: Vec::new(),
        generating: working.as_ref().map(|w| w.1).unwrap_or(false),
        working: working.map(|w| w.0),
        orphans: s
            .groups
            .iter()
            .filter(|g| g.orphan && !g.protected && !g.is_self)
            .count() as u32,
        units: None,
        trip_point_hit: s.thermal.trip_point_hit.value == Some(true),
    }
}

/// "Your things" chips for the Calm headline and the strip: for each fingerprint (in order, max 4) the
/// running groups with it — "sd-server idle", "sd-server generating", "4 Claude Code sessions",
/// "Google Chrome".
pub fn your_things_chips(s: &Snapshot, fingerprints: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for fp in fingerprints {
        if out.len() >= 4 {
            break;
        }
        if fp.is_empty() {
            continue;
        }
        let groups: Vec<&crate::model::Group> = s.groups.iter().filter(|g| &g.fingerprint == fp).collect();
        let Some(first) = groups.first() else { continue };
        let chip = if groups.len() > 1 {
            match first.kind {
                GroupKind::AgentSession => format!("{} {} sessions", groups.len(), first.label),
                _ => format!("{} ×{}", first.label, groups.len()),
            }
        } else {
            let server = s
                .model_servers
                .iter()
                .find(|m| m.group_id.as_deref() == Some(first.id.as_str()));
            match server.and_then(|m| m.busy.value) {
                Some(true) => format!("{} generating", first.label),
                Some(false) => format!("{} idle", first.label),
                None if first.idle => format!("{} idle", first.label),
                None => first.label.clone(),
            }
        };
        if !out.contains(&chip) {
            out.push(chip);
        }
    }
    out
}

/// Chips from affinity: pinned first, then the top-affinity running entities (max 4 total).
pub fn your_things_from_profile(
    s: &Snapshot,
    pinned: &[String],
    affinity_by_fp: &HashMap<String, f64>,
) -> Vec<String> {
    let mut fps: Vec<String> = pinned.to_vec();
    let mut ranked: Vec<(&String, f64)> = affinity_by_fp
        .iter()
        .filter(|(fp, a)| {
            **a > 0.05 && !pinned.contains(fp) && s.groups.iter().any(|g| &g.fingerprint == *fp)
        })
        .map(|(fp, a)| (fp, *a))
        .collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(b.0))
    });
    fps.extend(ranked.into_iter().take(3).map(|(fp, _)| fp.clone()));
    your_things_chips(s, &fps)
}

fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

fn thermal_word(t: ThermalPressure) -> &'static str {
    match t {
        ThermalPressure::Nominal => "nominal",
        ThermalPressure::Moderate => "moderate",
        ThermalPressure::Heavy => "heavy",
        ThermalPressure::Trapping => "trapping",
        ThermalPressure::Sleeping => "sleeping",
    }
}

fn killer_name(k: Option<OomKiller>) -> &'static str {
    match k {
        Some(OomKiller::SystemdOomd) => "systemd-oomd",
        Some(OomKiller::Earlyoom) => "earlyoom",
        Some(OomKiller::Jetsam) => "jetsam",
        Some(OomKiller::Kernel) | None => "OOM killer",
    }
}

/// Renders the headline.
pub fn render(i: &HeadlineInput) -> Headline {
    let units = match i.units.as_deref().map(str::to_ascii_lowercase).as_deref() {
        Some("iec") => UnitSystem::Iec,
        _ => UnitSystem::Si,
    };
    let amt = |b: u64| format_amount(b, units);
    let room = match i.headroom {
        Some(h) if h >= 0 => Some(format!("{} headroom", amt(h as u64))),
        Some(h) => Some(format!("{} into the safety margin", amt(h.unsigned_abs()))),
        None => None,
    };
    let owner = i
        .top_owner
        .as_ref()
        .map(|(label, b)| format!("{label} holds {}", amt(*b)));
    let action = i
        .best_action
        .as_ref()
        .filter(|(_, gain)| *gain > 0)
        .map(|(desc, gain)| format!("{desc} could free {}", amt(*gain)));
    let clauses = |with_owner: bool| -> Option<String> {
        let parts: Vec<String> = [with_owner.then(|| owner.clone()).flatten(), action.clone()]
            .into_iter()
            .flatten()
            .collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    };

    let mut key = None;
    // UX §5.5: the headline is the top insight, else the mode summary. An OOM forecast (already gated on
    // ≥ 2 min of consistent data, SPEC §8.3) is the top insight in every mode: "Swap full in ~6 min" is never
    // hidden behind "All good" or "Working" while the Pressure latch waits out its 10 s.
    let template = if i.forecast_eta_s.is_some() {
        Mode::Pressure
    } else {
        i.mode
    };
    let text = match template {
        Mode::Pressure => {
            if action.is_some() {
                key = Some('r');
            }
            if let Some(eta) = i.forecast_eta_s {
                let lead = match i.forecast_target {
                    Some(ForecastTarget::SwapExhaustion) | None => "Swap full".to_string(),
                    Some(ForecastTarget::AvailableExhaustion) => "Out of memory".to_string(),
                    Some(ForecastTarget::KillerThreshold) => {
                        format!("{} threshold", killer_name(i.forecast_killer))
                    }
                };
                let tail = clauses(true)
                    .or_else(|| room.clone())
                    .map(|t| format!(" — {t}"))
                    .unwrap_or_default();
                format!("{lead} in {} at this rate{tail}.", format_eta(eta))
            } else {
                let r = room.clone().unwrap_or_else(|| "headroom unknown".into());
                // Entity labels keep their case ("sd-server"); only generated wording is capitalized.
                match clauses(true) {
                    Some(t) if owner.is_some() => format!("Tight on memory — {r}. {t}."),
                    Some(t) => format!("Tight on memory — {r}. {}.", cap_first(&t)),
                    None => format!("Tight on memory — {r}."),
                }
            }
        }
        Mode::Throttle => {
            let heavy = i.thermal.filter(|t| *t >= ThermalPressure::Heavy);
            // The factor leads only when it actually shows a slowdown; "Running at 97% speed" under a
            // thermal / Low Power Mode trigger would read as a contradiction.
            let factor = i
                .throttle_factor
                .filter(|f| f.is_finite() && *f >= 0.0 && *f < THROTTLED_BELOW);
            let (lead, lead_is) = match (factor, heavy) {
                (Some(f), _) => (format!("Running at {:.0}% speed", f * 100.0), "factor"),
                (None, Some(t)) => (format!("Thermal pressure {}", thermal_word(t)), "thermal"),
                (None, None) if i.low_power_mode => ("Low Power Mode on".to_string(), "lpm"),
                (None, None) if i.trip_point_hit => ("Thermal trip point hit".to_string(), "trip"),
                (None, None) => ("Throttling".to_string(), "none"),
            };
            let mut causes = Vec::new();
            if let Some(t) = i.thermal.filter(|t| *t >= ThermalPressure::Moderate) {
                if lead_is != "thermal" {
                    causes.push(format!("thermal pressure {}", thermal_word(t)));
                }
            }
            if i.trip_point_hit && lead_is != "trip" {
                causes.push("thermal trip point hit".into());
            }
            if i.low_power_mode && lead_is != "lpm" {
                causes.push("Low Power Mode on".into());
            }
            let battery = i
                .on_battery_pct
                .filter(|b| b.is_finite())
                .map(|b| b.clamp(0.0, 100.0));
            if let Some(b) = battery {
                causes.push(format!("on battery ({b:.0}%)"));
            }
            let task = if i.generating { "generation" } else { "heavy work" };
            let advice = if battery.is_some() {
                format!("Plug in or pause {task}.")
            } else if lead_is == "lpm" {
                format!("Turn it off or pause {task}.")
            } else if i.low_power_mode {
                format!("Turn off Low Power Mode or pause {task}.")
            } else {
                format!("Pause {task} or let it cool.")
            };
            if causes.is_empty() {
                format!("{lead}. {advice}")
            } else {
                format!("{lead} — {}. {advice}", causes.join(", "))
            }
        }
        Mode::Working => {
            let w = i.working.clone().unwrap_or_else(|| "a job is running".into());
            match &room {
                Some(r) => format!("Working — {w}. {}.", cap_first(r)),
                None => format!("Working — {w}."),
            }
        }
        Mode::Leftovers => {
            if action.is_some() {
                key = Some('r');
            }
            let n = i.orphans;
            let lead = if n > 0 {
                format!("{n} orphan{} left behind", if n == 1 { "" } else { "s" })
            } else {
                "Idle leftovers".to_string()
            };
            match (clauses(false), i.free) {
                (Some(t), _) => format!("{lead} — {t}."),
                (None, Some(f)) => format!("{lead} — {} free.", amt(f)),
                (None, None) => format!("{lead}."),
            }
        }
        Mode::Calm => {
            let head = match i.free {
                Some(f) => format!("All good — {} free.", amt(f)),
                None => "All good — no pressure signals.".to_string(),
            };
            if i.your_things.is_empty() {
                head
            } else {
                format!("{head} Your things: {}.", i.your_things.join(", "))
            }
        }
    };
    Headline {
        text,
        action_key: key,
        mode: i.mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1_000_000_000;

    #[test]
    fn ux_examples() {
        let base = HeadlineInput {
            top_owner: Some(("sd-server".into(), 9_900_000_000)),
            best_action: Some(("2 idle build daemons".into(), 5_900_000_000)),
            ..Default::default()
        };
        let h = render(&HeadlineInput {
            mode: Mode::Pressure,
            headroom: Some(1_200_000_000),
            ..base.clone()
        });
        assert_eq!(
            h.text,
            "Tight on memory — 1.2 GB headroom. sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB."
        );
        assert_eq!(h.action_key, Some('r'));
        let h = render(&HeadlineInput {
            mode: Mode::Pressure,
            forecast_eta_s: Some(360),
            ..base.clone()
        });
        assert_eq!(
            h.text,
            "Swap full in ~6 min at this rate — sd-server holds 9.9 GB; 2 idle build daemons could free 5.9 GB."
        );
        let h = render(&HeadlineInput {
            mode: Mode::Throttle,
            throttle_factor: Some(0.45),
            thermal: Some(ThermalPressure::Heavy),
            on_battery_pct: Some(29.0),
            generating: true,
            ..Default::default()
        });
        assert_eq!(
            h.text,
            "Running at 45% speed — thermal pressure heavy, on battery (29%). Plug in or pause generation."
        );
        assert_eq!(h.action_key, None);
        let h = render(&HeadlineInput {
            mode: Mode::Calm,
            free: Some(11 * GB),
            your_things: vec!["sd-server idle".into(), "4 Claude Code sessions".into()],
            ..Default::default()
        });
        assert_eq!(
            h.text,
            "All good — 11 GB free. Your things: sd-server idle, 4 Claude Code sessions."
        );
    }

    #[test]
    fn amounts_and_eta() {
        assert_eq!(format_amount(9_900_000_000, UnitSystem::Si), "9.9 GB");
        assert_eq!(format_amount(9_960_000_000, UnitSystem::Si), "10 GB");
        assert_eq!(format_amount(11_000_000_000, UnitSystem::Si), "11 GB");
        assert_eq!(format_amount(512_000_000, UnitSystem::Si), "512 MB");
        assert_eq!(format_amount(3 * 1024 * 1024 * 1024, UnitSystem::Iec), "3.0 GiB");
        assert_eq!(format_amount(12, UnitSystem::Si), "12 B");
        assert_eq!(format_eta(30), "under a minute");
        assert_eq!(format_eta(360), "~6 min");
        assert_eq!(format_eta(389), "~6 min");
        assert_eq!(format_eta(3900), "~1 h 5 min");
        assert_eq!(format_eta(7200), "~2 h");
    }

    #[test]
    fn reclaim_descriptions() {
        let d = |v: &[(&str, GroupKind, bool, bool)]| {
            describe_reclaim(
                &v.iter()
                    .map(|(l, k, i, o)| (l.to_string(), *k, *i, *o))
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            d(&[
                ("GradleDaemon", GroupKind::BuildDaemon, true, false),
                ("Kotlin", GroupKind::BuildDaemon, true, false)
            ]),
            "2 idle build daemons"
        );
        assert_eq!(
            d(&[("GradleDaemon", GroupKind::BuildDaemon, true, false)]),
            "idle GradleDaemon"
        );
        assert_eq!(d(&[("chrome", GroupKind::Other, false, true)]), "orphaned chrome");
        assert_eq!(
            d(&[
                ("a", GroupKind::BuildDaemon, true, false),
                ("b", GroupKind::Other, false, true)
            ]),
            "2 leftovers"
        );
        assert_eq!(
            d(&[
                ("a", GroupKind::BuildDaemon, true, false),
                ("b", GroupKind::ModelServer, true, false)
            ]),
            "2 idle groups"
        );
        assert_eq!(
            d(&[
                ("a", GroupKind::Sandbox, false, true),
                ("b", GroupKind::Sandbox, false, true)
            ]),
            "2 orphaned sandboxes"
        );
    }

    #[test]
    fn other_templates() {
        let h = render(&HeadlineInput {
            mode: Mode::Pressure,
            headroom: Some(-800_000_000),
            ..Default::default()
        });
        assert_eq!(h.text, "Tight on memory — 800 MB into the safety margin.");
        assert_eq!(h.action_key, None);
        let h = render(&HeadlineInput {
            mode: Mode::Pressure,
            forecast_eta_s: Some(600),
            forecast_target: Some(ForecastTarget::KillerThreshold),
            forecast_killer: Some(OomKiller::SystemdOomd),
            headroom: Some(2 * GB as i64),
            ..Default::default()
        });
        assert_eq!(
            h.text,
            "systemd-oomd threshold in ~10 min at this rate — 2.0 GB headroom."
        );
        let h = render(&HeadlineInput {
            mode: Mode::Pressure,
            ..Default::default()
        });
        assert_eq!(h.text, "Tight on memory — headroom unknown.");
        let h = render(&HeadlineInput {
            mode: Mode::Throttle,
            low_power_mode: true,
            ..Default::default()
        });
        assert_eq!(h.text, "Low Power Mode on. Turn it off or pause heavy work.");
        let h = render(&HeadlineInput {
            mode: Mode::Working,
            working: Some("sd-server generating 3/6 · 8.1 s/step, ~24s left".into()),
            headroom: Some(5_100_000_000),
            ..Default::default()
        });
        assert_eq!(
            h.text,
            "Working — sd-server generating 3/6 · 8.1 s/step, ~24s left. 5.1 GB headroom."
        );
        let h = render(&HeadlineInput {
            mode: Mode::Leftovers,
            orphans: 2,
            best_action: Some(("2 orphaned process groups".into(), 1_300_000_000)),
            ..Default::default()
        });
        assert_eq!(
            h.text,
            "2 orphans left behind — 2 orphaned process groups could free 1.3 GB."
        );
        assert_eq!(h.action_key, Some('r'));
        let h = render(&HeadlineInput {
            mode: Mode::Calm,
            units: Some("iec".into()),
            free: Some(11 * 1024 * 1024 * 1024),
            ..Default::default()
        });
        assert_eq!(h.text, "All good — 11 GiB free.");
        assert_eq!(
            render(&HeadlineInput::default()).text,
            "All good — no pressure signals."
        );
    }
}

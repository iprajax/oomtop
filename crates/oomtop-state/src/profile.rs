//! `oomtop profile show` (UX §6): what oomtop has learned on this machine, in plain words.

use crate::{Result, StateDb, UxStats, LOG_CAP_BYTES, RETENTION_MS};
use serde::Serialize;

/// An entity with its current affinity score.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityScore {
    pub fingerprint: String,
    /// Alias if renamed, else the derived display name.
    pub name: String,
    pub score: f64,
}

/// Summary for `oomtop profile show`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProfileSummary {
    pub path: Option<String>,
    pub schema_version: u32,
    pub learning: bool,
    pub file_bytes: u64,
    pub log_bytes: u64,
    pub log_cap_bytes: u64,
    pub retention_days: u64,
    pub entities: usize,
    pub pinned: Vec<EntityScore>,
    pub muted: Vec<EntityScore>,
    /// Highest affinity first (top 10).
    pub top: Vec<EntityScore>,
    pub renamed: Vec<(String, String)>,
    pub events: u64,
    pub impressions: u64,
    pub queries: u64,
    pub top_queries: Vec<String>,
    pub lineage_rows: u64,
    pub oldest_event_ms: Option<u64>,
    pub machine_profile: Option<serde_json::Value>,
    pub machine_profile_updated_ms: Option<u64>,
    pub stats: UxStats,
}

pub(crate) fn summarize(db: &StateDb, now_ms: u64, half_life_s: f64) -> Result<ProfileSummary> {
    let ents = db.entities()?;
    let fr = db.frecency(now_ms, half_life_s)?;
    let score_of = |fp: &str| fr.get(fp).copied().unwrap_or(0.0);
    let to_score = |e: &crate::EntityRecord| EntityScore {
        fingerprint: e.fingerprint.clone(),
        name: e.name().to_string(),
        score: score_of(&e.fingerprint),
    };
    let mut top: Vec<EntityScore> = ents.iter().map(to_score).filter(|e| e.score > 0.0).collect();
    top.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.name.cmp(&b.name))
    });
    top.truncate(10);
    let (events, impressions, queries, lineage_rows) = db.counts()?;
    let mp = db.machine_profile()?;
    Ok(ProfileSummary {
        path: db.path().map(|p| p.display().to_string()),
        schema_version: db.schema_version()?,
        learning: db.learning(),
        file_bytes: db.file_bytes(),
        log_bytes: db.log_bytes()?,
        log_cap_bytes: LOG_CAP_BYTES,
        retention_days: RETENTION_MS / 86_400_000,
        entities: ents.len(),
        pinned: ents.iter().filter(|e| e.pinned).map(to_score).collect(),
        muted: ents.iter().filter(|e| e.is_muted(now_ms)).map(to_score).collect(),
        top,
        renamed: ents
            .iter()
            .filter_map(|e| e.alias.clone().map(|a| (e.display_name.clone(), a)))
            .collect(),
        events,
        impressions,
        queries,
        top_queries: db
            .top_queries(5, now_ms, half_life_s)?
            .into_iter()
            .map(|(q, _)| q)
            .collect(),
        lineage_rows,
        oldest_event_ms: db.oldest_event_ms()?,
        machine_profile_updated_ms: mp.as_ref().map(|(_, t)| *t),
        machine_profile: mp.map(|(j, _)| j),
        stats: db.ux_stats(now_ms.saturating_sub(RETENTION_MS))?,
    })
}

fn kib(b: u64) -> String {
    if b >= 1024 * 1024 {
        format!("{:.1} MiB", b as f64 / 1048576.0)
    } else {
        format!("{:.0} KiB", (b as f64 / 1024.0).ceil())
    }
}

fn pct(v: Option<f64>) -> String {
    v.map(|x| format!("{:.0}%", x * 100.0))
        .unwrap_or_else(|| "–".into())
}

impl ProfileSummary {
    /// Plain-text rendering (screen-reader friendly: one fact per line).
    pub fn render_text(&self, now_ms: u64) -> String {
        let mut o = String::new();
        let line = |o: &mut String, s: String| {
            o.push_str(&s);
            o.push('\n');
        };
        line(
            &mut o,
            format!(
                "Profile: {}",
                self.path.clone().unwrap_or_else(|| "(in memory)".into())
            ),
        );
        line(
            &mut o,
            format!(
                "Learning: {}",
                if self.learning {
                    "on (local only)"
                } else {
                    "off (--no-learn / personalization.learn = false)"
                }
            ),
        );
        line(
            &mut o,
            format!(
                "Log: {} of {} cap, kept {} days · {} events · {} impressions · {} queries",
                kib(self.log_bytes),
                kib(self.log_cap_bytes),
                self.retention_days,
                self.events,
                self.impressions,
                self.queries
            ),
        );
        if let Some(t) = self.oldest_event_ms {
            let days = now_ms.saturating_sub(t) / 86_400_000;
            line(&mut o, format!("Learning since: {days} day(s) ago"));
        }
        line(
            &mut o,
            format!(
                "Lineage journal: {} process record(s) (kept on reset)",
                self.lineage_rows
            ),
        );
        let list = |o: &mut String, title: &str, v: &[EntityScore], scores: bool| {
            if v.is_empty() {
                line(o, format!("{title}: none"));
                return;
            }
            line(o, format!("{title}:"));
            for e in v {
                if scores {
                    line(o, format!("  {} (affinity {:.2})", e.name, e.score));
                } else {
                    line(o, format!("  {}", e.name));
                }
            }
        };
        list(&mut o, "Pinned", &self.pinned, false);
        list(&mut o, "Muted", &self.muted, false);
        list(&mut o, "Most used", &self.top, true);
        if !self.renamed.is_empty() {
            line(&mut o, "Renamed:".into());
            for (from, to) in &self.renamed {
                line(&mut o, format!("  {from} → {to}"));
            }
        }
        if !self.top_queries.is_empty() {
            line(
                &mut o,
                format!("Frequent queries: {}", self.top_queries.join(" · ")),
            );
        }
        line(
            &mut o,
            format!(
                "Ranking quality (30 d): Hit@3 {} · keystrokes to target {} · reformulation {} · dismiss {}",
                pct(self.stats.hit_at_3),
                self.stats
                    .mean_keystrokes
                    .map(|k| format!("{k:.1}"))
                    .unwrap_or_else(|| "–".into()),
                pct(self.stats.reformulation_rate),
                pct(self.stats.dismiss_rate)
            ),
        );
        match &self.machine_profile {
            Some(mp) => line(
                &mut o,
                format!(
                    "Machine profile: {}",
                    serde_json::to_string(mp).unwrap_or_default()
                ),
            ),
            None => line(&mut o, "Machine profile: not built yet".into()),
        }
        line(
            &mut o,
            "Reset with `oomtop profile reset` (ranking returns to salience-only defaults).".into(),
        );
        o
    }
}

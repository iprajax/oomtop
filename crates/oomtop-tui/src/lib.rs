//! # oomtop-tui
//!
//! ratatui + crossterm frontend (SPEC §12.1, UX §2–§12). Never touches OS APIs for monitoring: it gets
//! snapshots from a [`SnapshotProvider`] and executes confirmed plans through an [`Actuator`] (both built by
//! `oomtop-cli`). Personalization is read from / written to `oomtop-state`; the settings screen reads the config
//! layers and writes them through `oomtop-config` (comment-preserving).
//!
//! `--plain`, `TERM=dumb` or a non-TTY stdout print a linear, screen-reader-friendly summary instead.
//!
//! Frame pipeline: provider → [`app::App::update`] (headroom, mode, ranking, headline, timeline) →
//! [`render::draw`] inside a synchronized-output bracket (DEC 2026), so frames never tear.

pub mod app;
pub mod caps;
pub mod columns;
pub mod fixtures;
pub mod format;
pub mod layout;
pub mod render;
pub mod replies;
pub mod settings;
pub mod style;
pub mod timeline;

pub use app::DiskModel;
use app::{App, Effect, MouseInput, MouseKind};
use oomtop_config::keymap::{load_keymap, Keymap, ACTIONS};
use oomtop_config::layered::{
    find_line, leaves, load_layered, load_layered_cached, LayerCache, LoadOptions, Loaded,
};
use oomtop_config::model::AppearanceMode;
use oomtop_config::model::{Background, Glyphs as GlyphMode};
use oomtop_config::paths::{short_hostname, ConfigPaths};
use oomtop_config::theme::{
    check, list_themes, load_theme, load_theme_for, none_theme, terminal_theme, Theme, Variant,
};
use oomtop_config::Config;
use oomtop_core::actions::{ActionKind, Actuator, ProtectContext};
use oomtop_core::headroom::Headroom;
use oomtop_core::provider::SnapshotProvider;
use oomtop_core::units::{format_bytes_short, format_duration};
use oomtop_core::{Quality, Snapshot};
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::Rect;
use settings::{SaveLayer, SettingsCmd, SettingsModel, ThemeChoice, DROPIN_NAME};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use style::{ColorDepth, Palette};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TuiError {
    #[error("terminal: {0}")]
    Io(#[from] std::io::Error),
}

/// Everything the CLI hands the TUI (frozen contract, docs/CONTRACTS.md).
pub struct TuiOptions {
    pub provider: Box<dyn SnapshotProvider>,
    pub actuator: Option<Box<dyn Actuator>>,
    pub config: Config,
    pub theme: Theme,
    pub keymap: Keymap,
    pub state: Option<oomtop_state::StateDb>,
    pub protect: ProtectContext,
    /// Force `--plain` output.
    pub plain: bool,
    /// ASCII glyphs (`--ascii`).
    pub ascii: bool,
    /// Disable learning (`--no-learn`).
    pub no_learn: bool,
}

/// Optional extras (additive to the frozen [`TuiOptions`]): the loaded config layers for the settings screen
/// and live reload. Without them, [`run`] loads the layers itself and treats values that differ from
/// `TuiOptions::config` as CLI flags (re-applied on every reload).
pub struct TuiExtras {
    /// The layered config as the CLI loaded it (origins for the settings screen).
    pub loaded: Option<Loaded>,
    /// How to reload on file changes (defaults to the user environment).
    pub load_options: Option<LoadOptions>,
    /// Config paths (defaults to the XDG user paths).
    pub paths: Option<ConfigPaths>,
    /// Watch config files and apply edits live (UX §12.2).
    pub watch: bool,
    /// Terminal background from the CLI's OSC 11 probe, for `appearance = "auto"` when themes are re-loaded
    /// (live reload, settings preview). Without it the variant of `TuiOptions::theme` or `$COLORFGBG` is used.
    pub background: Option<(u8, u8, u8)>,
    /// Model files on disk, delivered once the CLI's (background) index scan finishes.
    pub disk_models: Option<std::sync::mpsc::Receiver<Vec<DiskModel>>>,
    /// Kitty keyboard support from the CLI's terminal probe (`CSI ? u`, answered or not within its 200 ms
    /// cap). `Some` skips crossterm's own query, which waits up to 2 s on terminals that never answer DA1
    /// (first frame < 300 ms, SPEC §14). `None`: no probe ran; crossterm asks.
    pub kitty_keyboard: Option<bool>,
    /// The CLI's probe gave up before the terminal answered (no DA1 within its cap): the answers may still
    /// arrive — high-latency SSH, a slow terminal — and are dropped instead of read as keys
    /// ([`replies::LateReplyFilter`], UX §12.10).
    pub late_replies: bool,
}

impl Default for TuiExtras {
    fn default() -> Self {
        TuiExtras {
            loaded: None,
            load_options: None,
            paths: None,
            watch: true,
            background: None,
            disk_models: None,
            kitty_keyboard: None,
            late_replies: false,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Plain output (UX §8 accessibility)
// ---------------------------------------------------------------------------------------------------------

fn b(v: Option<u64>) -> String {
    v.map(format_bytes_short).unwrap_or_else(|| "unavailable".into())
}

fn why_unavailable<T>(m: &oomtop_core::Measured<T>) -> String {
    match &m.quality {
        Quality::Unavailable(r) if !r.is_empty() => format!(" ({r})"),
        _ => String::new(),
    }
}

/// Linear summary for `--plain`, `TERM=dumb` and pipes (UX §8 accessibility): full sentences, no glyphs,
/// no tables, every number with its unit, unavailable values said as such (never zero).
pub fn plain_summary(s: &Snapshot, h: &Headroom, headline: &str) -> String {
    let mut out = String::new();
    let host = if s.host.hostname.is_empty() {
        "this machine"
    } else {
        &s.host.hostname
    };
    out.push_str(&format!(
        "oomtop — {host}{}.\n",
        s.host
            .model
            .as_ref()
            .map(|_| format!(", {}", oomtop_core::machine::display_name(&s.host)))
            .unwrap_or_default()
    ));
    out.push_str(&format!("{headline}\n"));
    let m = &s.memory;
    out.push_str(&format!(
        "Memory: total {}, available {}{}, apps {}, compressed {}, wired {}, free {}.\n",
        b(m.total.value),
        b(m.available.value),
        why_unavailable(&m.available),
        b(m.app.value),
        b(m.compressed.value),
        b(m.wired.value),
        b(m.free.value)
    ));
    let pressure = match m.pressure.value {
        Some(oomtop_core::PressureLevel::Critical) => "critical",
        Some(oomtop_core::PressureLevel::Warn) => "warning",
        Some(oomtop_core::PressureLevel::Normal) => "normal",
        None => "unavailable",
    };
    out.push_str(&format!("Memory pressure: {pressure}.\n"));
    let rate = match (m.swap_out_per_min.value, m.swap_in_per_min.value) {
        (Some(o), Some(i)) if o > i => format!(", growing {} per minute", format_bytes_short(o - i)),
        (Some(o), Some(i)) if i > o => format!(", shrinking {} per minute", format_bytes_short(i - o)),
        _ => String::new(),
    };
    out.push_str(&format!(
        "Swap: {} of {} used{rate}.\n",
        b(m.swap_used.value),
        b(m.swap_total.value)
    ));
    match h.headroom {
        Some(x) if x >= 0 => out.push_str(&format!(
            "Headroom: {} after a {} safety margin.\n",
            format_bytes_short(x as u64),
            format_bytes_short(h.safety_margin)
        )),
        Some(x) => out.push_str(&format!(
            "Headroom: {} below the safety margin.\n",
            format_bytes_short(x.unsigned_abs())
        )),
        None => out.push_str("Headroom: unavailable.\n"),
    }
    match &s.oom.forecast {
        Some(f) => out.push_str(&format!(
            "Forecast: out of memory in about {} at the current rate.\n",
            format_duration(f.eta_s)
        )),
        None => out.push_str("Forecast: stable.\n"),
    }
    for a in &s.accelerators {
        let mut parts = Vec::new();
        match a.util_pct.value {
            Some(u) => parts.push(format!("{u:.0}% busy")),
            None => parts.push(format!("utilization unavailable{}", why_unavailable(&a.util_pct))),
        }
        if let Some(used) = a.mem_used.value {
            parts.push(format!("{} in use", format_bytes_short(used)));
        }
        if let Some(g) = a.gpu_budget.value {
            parts.push(format!("budget {}", format_bytes_short(g)));
        }
        out.push_str(&format!(
            "GPU {}: {}.\n",
            if a.name.is_empty() { &a.id } else { &a.name },
            parts.join(", ")
        ));
    }
    let t = &s.thermal;
    let mut th = Vec::new();
    if let Some(p) = t.pressure.value {
        th.push(format!("thermal pressure {}", format!("{p:?}").to_lowercase()));
    }
    if let Some(f) = t.throttle_factor.value {
        th.push(oomtop_core::throttle::speed_label(f));
    }
    if t.low_power_mode.value == Some(true) {
        th.push("Low Power Mode on".into());
    }
    if t.on_battery.value == Some(true) {
        th.push(format!(
            "on battery{}",
            t.battery_pct
                .value
                .map(|b| format!(" at {b:.0}%"))
                .unwrap_or_default()
        ));
    }
    if !th.is_empty() {
        out.push_str(&format!("Thermal: {}.\n", th.join(", ")));
    }
    out.push_str("Top groups:\n");
    let mut label_count: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for g in &s.groups {
        *label_count.entry(g.label.as_str()).or_insert(0) += 1;
    }
    for (i, g) in s.groups.iter().take(10).enumerate() {
        let mut flags = Vec::new();
        if g.idle {
            flags.push(format!(
                "idle {}",
                g.idle_for_s.map(format_duration).unwrap_or_default()
            ));
        }
        if g.orphan {
            flags.push("orphan".into());
        }
        if g.lower_bound {
            flags.push("lower bound".into());
        }
        if oomtop_core::headroom::is_reclaim_candidate(g) {
            flags.push("reclaimable".into());
        }
        if g.protected {
            flags.push("protected".into());
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" ({})", flags.join(", "))
        };
        let label = if label_count.get(g.label.as_str()).copied().unwrap_or(0) > 1 {
            format!("{} {}", g.label, app::disambiguator(g))
        } else {
            g.label.clone()
        };
        let n = g.totals.process_count;
        out.push_str(&format!(
            "{}. {label} [{}] {}, {n} process{}{flags}\n",
            i + 1,
            g.kind.alias(),
            b(g.totals.footprint.value),
            if n == 1 { "" } else { "es" }
        ));
    }
    let cands = oomtop_core::can_fit::reclaim_candidates(s);
    if !cands.is_empty() {
        let gain: u64 = cands.iter().map(|c| c.gain).sum();
        out.push_str(&format!(
            "Reclaim: stopping {} could free about {}. Run oomtop reclaim to confirm.\n",
            cands
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            format_bytes_short(gain)
        ));
    }
    for m in &s.model_servers {
        out.push_str(&format!(
            "Model server {:?}{}: {}{}.\n",
            m.kind,
            m.endpoint
                .as_ref()
                .map(|e| format!(" at {e}"))
                .unwrap_or_default(),
            if m.models.is_empty() {
                "no models loaded".to_string()
            } else {
                m.models
                    .iter()
                    .map(|x| x.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            match (&m.progress, m.busy.value) {
                (Some(p), _) => format!(", {} {} of {}", p.label, p.done, p.total),
                (None, Some(true)) => ", busy".into(),
                _ => String::new(),
            }
        ));
    }
    for sb in &s.sandboxes {
        out.push_str(&format!(
            "Sandbox {} ({}){}{}.\n",
            sb.label,
            sb.runtime,
            sb.configured_mem
                .value
                .map(|c| format!(", configured {}", format_bytes_short(c)))
                .unwrap_or_default(),
            if sb.footprint_lower_bound {
                ", host footprint is a lower bound"
            } else {
                ""
            }
        ));
    }
    sanitize_plain(&out)
}

/// Process names, labels and model names are untrusted: control characters (ESC, BEL, C1 CSI…) are replaced
/// so `--plain` output cannot carry terminal escape sequences (clipboard writes, screen rewrites).
fn sanitize_plain(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\n' { '?' } else { c })
        .collect()
}

// ---------------------------------------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------------------------------------

/// Key event → keymap key string ("q", "ctrl-k", "up", "F9", …).
pub fn key_string(code: KeyCode, mods: KeyModifiers) -> Option<String> {
    let base = match code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::F(n) => format!("F{n}"),
        _ => return None,
    };
    // Canonical form of `oomtop_config::keymap::normalize_key`: ctrl- then alt-; shifted letters are upper
    // case; ctrl letters lower case.
    let mut out = String::new();
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    if ctrl {
        out.push_str("ctrl-");
    }
    if mods.contains(KeyModifiers::ALT) {
        out.push_str("alt-");
    }
    if ctrl && base.chars().count() == 1 {
        out.push_str(&base.to_lowercase());
    } else {
        out.push_str(&base);
    }
    Some(out)
}

/// Human description per keymap action (the `?` help lists keys from the active keymap).
pub fn action_description(action: &str) -> &'static str {
    match action {
        "move:up" => "move up",
        "move:down" => "move down",
        "move:top" => "top (timeline: oldest)",
        "move:bottom" => "bottom (timeline: live)",
        "move:page-up" => "page up (timeline: -1 min)",
        "move:page-down" => "page down (timeline: +1 min)",
        "expand" => "expand members / open details (timeline: scrub later)",
        "collapse" => "collapse / close (timeline: scrub earlier)",
        "view:home" => "Home: your things + ranked groups",
        "view:processes" => "Processes",
        "view:models" => "Models",
        "view:sandboxes" => "Sandboxes",
        "view:reclaim" => "Reclaim candidates",
        "view:timeline" => "Timeline (last 10 min)",
        "view:next" => "next view",
        "filter" => "filter (mem>2G kind:daemon …)",
        "command" => "command (:reclaim :headroom 13G :why …)",
        "palette" => "palette: search, filters, commands",
        "stop" => "stop group (SIGTERM; again for SIGKILL) — confirms",
        "suspend" => "suspend / resume (CPU relief only) — confirms",
        "pin" => "pin to Your things",
        "mute" => "mute for 30 days (ranked lower)",
        "less" => "show less like this",
        "rename" => "rename (usable in queries)",
        "undo" => "undo pin / mute / rename / less",
        "pin-mode" => "pin / unpin the situation mode",
        "why" => "why is this ranked here?",
        "watch" => "watch: focus mode",
        "compare" => "compare two entities",
        "settings" => "settings",
        "help" => "help",
        "quit" => "quit (closes overlays first)",
        "sort" => "cycle sort",
        "sort-by" => "sort by… (pick the sort column, like htop F6)",
        "tree" => "tree: process tree (Processes) / expand-collapse all groups (Home)",
        "open:cwd" => "open cwd / model file",
        "open:model" => "open the model file",
        "open:log" => "open the log file",
        "view:prev" => "previous view",
        "refresh" => "sample now",
        "headline-action" => "the headline's best action (shown as [key])",
        _ => "",
    }
}

/// `(keys, description)` pairs for the help overlay, from the active keymap.
pub fn help_pairs(km: &Keymap) -> Vec<(String, String)> {
    ACTIONS
        .iter()
        .filter_map(|a| {
            let keys = km.keys_for(a);
            if keys.is_empty() {
                return None;
            }
            let mut uniq: Vec<&str> = Vec::new();
            for k in keys {
                if !uniq.contains(&k) {
                    uniq.push(k);
                }
            }
            Some((uniq.join(" / "), action_description(a).to_string()))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------------------
// Session: config, theme, glyphs, keymap, learning
// ---------------------------------------------------------------------------------------------------------

/// Palette for a config + theme + detected capabilities (NO_COLOR → `none`, attributes only).
pub fn build_palette(cfg: &Config, theme: &Theme, caps: &caps::TermCaps) -> Palette {
    let (theme, depth) = if caps.no_color || theme.name == "none" {
        (none_theme(), ColorDepth::None)
    } else {
        (theme.clone(), caps.depth)
    };
    Palette::new(theme, depth, cfg.appearance.background == Background::Theme)
}

/// ASCII-only version of a text for `--ascii` plain output: dashes, separators and math signs get their
/// usual ASCII spellings (`—` → `-`, `·` → `|`, `≈` → `~`, `≥` → `>=`, `…` → `...`); anything else
/// non-ASCII falls back to the single-cell TUI mapping.
pub fn ascii_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
            continue;
        }
        match c {
            '≥' => out.push_str(">="),
            '≤' => out.push_str("<="),
            '…' => out.push_str("..."),
            '→' => out.push_str("->"),
            '←' => out.push_str("<-"),
            '²' => out.push('2'),
            _ => out.push_str(render::ascii_char(c.encode_utf8(&mut buf))),
        }
    }
    out
}

/// Glyph set: ASCII with `--ascii`, `glyphs = "ascii"`, or a non-UTF-8 locale.
pub fn build_glyphs(cfg: &Config, ascii_flag: bool, caps: &caps::TermCaps) -> render::Glyphs {
    if ascii_flag || cfg.appearance.glyphs == GlyphMode::Ascii || !caps.unicode {
        render::ASCII
    } else {
        render::UNICODE
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Converts a config into its dotted leaves (TOML literals).
fn config_leaves(cfg: &Config) -> Vec<(String, String)> {
    match toml::Value::try_from(cfg) {
        Ok(toml::Value::Table(t)) => leaves(&t).into_iter().map(|(k, v)| (k, v.to_string())).collect(),
        _ => Vec::new(),
    }
}

/// Keys whose value in `wanted` differs from `loaded` (CLI flags when the CLI did not pass its layers).
fn flag_overrides(wanted: &Config, loaded: &Config) -> Vec<(String, String)> {
    let have: std::collections::HashMap<String, String> = config_leaves(loaded).into_iter().collect();
    config_leaves(wanted)
        .into_iter()
        .filter(|(k, v)| have.get(k) != Some(v))
        .collect()
}

struct Learning {
    events: Vec<(String, oomtop_state::EventKind, u64, Option<String>)>,
    queries: Vec<(String, u64)>,
    impressions: Vec<oomtop_state::Impression>,
    last_flush: Instant,
}

impl Learning {
    fn new() -> Self {
        Learning {
            events: Vec::new(),
            queries: Vec::new(),
            impressions: Vec::new(),
            last_flush: Instant::now(),
        }
    }
}

/// Writes queued learning signals (batched every 30 s and on exit, UX §10) and refreshes affinity.
fn flush_learning(db: &mut oomtop_state::StateDb, l: &mut Learning, app: &mut App, half_life_days: f64) {
    let now = now_ms();
    for (fp, kind, at, mode) in l.events.drain(..) {
        let _ = db.record_event(&fp, kind, at, mode.as_deref());
    }
    for (q, at) in l.queries.drain(..) {
        let _ = db.record_query(&q, at);
    }
    for imp in l.impressions.drain(..) {
        let _ = db.log_impression(&imp);
    }
    for g in &app.snapshot.groups {
        if !g.fingerprint.is_empty() {
            let _ = db.upsert_entity(&g.fingerprint, &g.label, now);
        }
    }
    let hl = half_life_days.max(0.1) * 86_400.0;
    if let (Ok(f), Ok(e)) = (db.frecency(now, hl), db.entities()) {
        let q = db
            .top_queries(8, now, hl)
            .map(|v| v.into_iter().map(|(q, _)| q).collect())
            .unwrap_or_default();
        app.set_profile(f, &e, q);
    }
    l.last_flush = Instant::now();
}

/// Loads a layout by name (`""` / `"adaptive"` = built-in → `None`), with file:line errors.
pub fn load_active_layout(name: &str, dir: &Path) -> Result<Option<columns::ActiveLayout>, String> {
    if name.is_empty() || name == "adaptive" {
        return Ok(None);
    }
    let layout = oomtop_config::layout::load_layout(name, dir).map_err(|errs| {
        errs.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ") + " (kept the current layout)"
    })?;
    columns::ActiveLayout::new(layout)
        .map(Some)
        .map_err(|e| format!("layout {name:?}: {e} (kept the current layout)"))
}

/// Line of a setting in one config file: the key itself, or the inline table / array value that holds it.
/// A `[table]` header is not "the setting": an unset key of a table that exists in the file is not set there
/// (UX §12.8 "overridden elsewhere" must be accurate).
fn setting_line(text: &str, lines: &std::collections::BTreeMap<String, usize>, key: &str) -> Option<usize> {
    if let Some(l) = lines.get(key) {
        return Some(*l);
    }
    let mut k = key;
    while let Some((parent, _)) = k.rsplit_once('.') {
        if let Some(l) = lines.get(parent) {
            let is_header = text
                .lines()
                .nth(l.saturating_sub(1))
                .is_some_and(|t| t.trim_start().starts_with('['));
            return (!is_header).then_some(*l);
        }
        k = parent;
    }
    None
}

/// Builds the settings model from the session's config layers.
pub fn settings_model(
    loaded: &Loaded,
    flag_keys: &[String],
    paths: &ConfigPaths,
    main_config: &Path,
    hostname: &str,
) -> SettingsModel {
    // Per file: its key → line map, parsed once.
    let texts: Vec<(PathBuf, String, std::collections::BTreeMap<String, usize>)> = loaded
        .files
        .iter()
        .filter_map(|p| {
            std::fs::read_to_string(p).ok().map(|t| {
                let lines = oomtop_config::layered::key_lines(&t);
                (p.clone(), t, lines)
            })
        })
        .collect();
    let entries: Vec<(String, String, String, Vec<String>)> = loaded
        .effective()
        .into_iter()
        .map(|e| {
            let origin = if flag_keys.contains(&e.key) {
                "flag".to_string()
            } else {
                e.origin.to_string()
            };
            let also_in: Vec<String> = texts
                .iter()
                .filter_map(|(p, t, lines)| {
                    let line = setting_line(t, lines, &e.key)?;
                    let name = p.file_name()?.to_string_lossy().into_owned();
                    let label = format!("{name}:{line}");
                    (label != origin).then_some(label)
                })
                .collect();
            (e.key, e.value, origin, also_in)
        })
        .collect();
    let themes: Vec<ThemeChoice> = list_themes(Some(&paths.themes))
        .into_iter()
        .map(|name| {
            let badge = match load_theme(&name, Some(&paths.themes)) {
                Ok(t) => {
                    let n = check(&t).len();
                    if n == 0 {
                        "ok".to_string()
                    } else {
                        format!("{n} issue{}", if n == 1 { "" } else { "s" })
                    }
                }
                Err(_) => "error".into(),
            };
            ThemeChoice { name, badge }
        })
        .collect();
    let mut m = SettingsModel::from_entries(entries, themes);
    m.set_layouts(oomtop_config::layout::list_layouts(&paths.layouts));
    m.layer_paths = vec![
        (SaveLayer::User, main_config.to_path_buf()),
        (SaveLayer::Host, paths.host_file(hostname)),
        (SaveLayer::Dropin, paths.config_d.join(DROPIN_NAME)),
    ];
    m
}

/// Keys just saved to `written` whose effective value still comes from somewhere else (a later file, env or a
/// flag), as "KEY is set in ORIGIN" lines.
fn overridden_after_save(
    check: &Loaded,
    written: &Path,
    changes: &[(String, String)],
    flags: &[(String, String)],
) -> Vec<String> {
    changes
        .iter()
        .filter_map(|(k, _)| {
            if flags.iter().any(|(f, _)| f == k) {
                return Some(format!("{k} is set by a command-line flag"));
            }
            match check.origin(k) {
                oomtop_config::Origin::File { path, line } if path != written => Some(format!(
                    "{k} is overridden in {}{}",
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.display().to_string()),
                    line.map(|l| format!(":{l}")).unwrap_or_default()
                )),
                oomtop_config::Origin::Env(var) => Some(format!("{k} is overridden by ${var}")),
                _ => None,
            }
        })
        .collect()
}

/// Opens a cwd or model file with the OS opener (never through a shell). Paths must be absolute, so nothing
/// can be taken for an option; the child is reaped in the background.
fn open_path(path: &str) -> String {
    if !Path::new(path).is_absolute() {
        return format!("not opening {path:?}: not an absolute path");
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    match std::process::Command::new(opener)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            // Reap it so no zombie is left behind for the rest of the session.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            format!("opened {path}")
        }
        Err(e) => format!("{opener}: {e}"),
    }
}

/// Saves settings changes to one layer file with comment-preserving edits. Returns the file name.
pub fn save_settings(
    path: &Path,
    changes: &[(String, String)],
) -> Result<String, oomtop_config::ConfigError> {
    for (k, v) in changes {
        oomtop_config::write::set_value(path, k, v)?;
    }
    Ok(path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string()))
}

struct Session {
    opts: TuiOptions,
    caps: caps::TermCaps,
    palette: Palette,
    glyphs: render::Glyphs,
    keymap: Keymap,
    loaded: Loaded,
    saved: Loaded,
    flags: Vec<(String, String)>,
    load_options: LoadOptions,
    paths: ConfigPaths,
    main_config: PathBuf,
    hostname: String,
    learning: Learning,
    refresh: Duration,
    next_refresh: Instant,
    /// The next refresh samples every source now (`SnapshotProvider::snapshot_now`).
    force_next: bool,
    /// Last good version of every config file: an invalid edit keeps the previous values (UX §11 #9).
    cache: LayerCache,
    /// Layout chosen at runtime (`:layout`, a saved view); overrides `layout.name` until changed.
    layout_override: Option<String>,
    /// Name of the layout currently loaded ("" / "adaptive" = built-in).
    layout_loaded: Option<String>,
    /// The terminal reported focus loss: sample less often (UX §12.10).
    unfocused: bool,
    /// Light/dark variant `appearance = "auto"` resolves to on this terminal.
    auto_variant: Variant,
}

/// The variant `appearance = "auto"` means on this terminal: the OSC 11 background when the CLI probed it,
/// else the variant the CLI loaded the starting theme in, else `$COLORFGBG`, else dark (UX §12.4).
pub fn auto_variant(background: Option<(u8, u8, u8)>, cli_theme: &Theme, colorfgbg: Option<&str>) -> Variant {
    background
        .map(Variant::for_background)
        .or_else(|| cli_theme.appearance.as_deref().and_then(Variant::parse))
        .or_else(|| colorfgbg.and_then(Variant::from_colorfgbg))
        .unwrap_or_default()
}

/// The variant a config asks for.
pub fn wanted_variant(cfg: &Config, auto: Variant) -> Variant {
    match cfg.appearance.appearance {
        AppearanceMode::Dark => Variant::Dark,
        AppearanceMode::Light => Variant::Light,
        AppearanceMode::Auto => auto,
    }
}

/// Sampling slows down by this factor while the terminal is unfocused (history keeps coming, at a lower rate).
const UNFOCUSED_FACTOR: u32 = 4;

impl Session {
    /// Loads the active layout (runtime choice, else `layout.name`) when it changed or `force`; problems go to
    /// the status line and the previous layout stays.
    fn sync_layout(&mut self, app: &mut App, force: bool) {
        let name = self
            .layout_override
            .clone()
            .unwrap_or_else(|| self.loaded.config.layout.name.clone());
        if !force && self.layout_loaded.as_deref() == Some(name.as_str()) {
            return;
        }
        match load_active_layout(&name, &self.paths.layouts) {
            Ok(l) => {
                app.set_layout(l);
                self.layout_loaded = Some(name);
            }
            Err(e) => app.status = Some(e),
        }
    }

    /// Effective refresh interval (slower while the terminal is unfocused).
    fn interval(&self) -> Duration {
        if self.unfocused {
            self.refresh * UNFOCUSED_FACTOR
        } else {
            self.refresh
        }
    }

    fn config(&self) -> &Config {
        &self.loaded.config
    }

    fn rebuild_look(&mut self, app: &mut App) {
        let cfg = self.loaded.config.clone();
        self.caps = {
            let mut c = caps::detect(cfg.appearance.color);
            c.kitty_keyboard = self.caps.kitty_keyboard;
            c
        };
        let variant = wanted_variant(&cfg, self.auto_variant);
        let cli_variant = self
            .opts
            .theme
            .appearance
            .as_deref()
            .and_then(Variant::parse)
            .unwrap_or_default();
        // The CLI's theme is reused only while name and variant still match (appearance can be previewed).
        let theme = if cfg.appearance.theme == self.opts.theme.name
            && (variant == cli_variant || self.opts.theme.appearance.is_none())
        {
            self.opts.theme.clone()
        } else {
            match load_theme_for(&cfg.appearance.theme, Some(&self.paths.themes), variant) {
                Ok(t) => t,
                Err(e) => {
                    app.status = Some(format!("theme {:?}: {e} — using terminal", cfg.appearance.theme));
                    terminal_theme()
                }
            }
        };
        self.palette = build_palette(&cfg, &theme, &self.caps);
        self.glyphs = build_glyphs(&cfg, self.opts.ascii, &self.caps);
        if cfg.keys.preset != self.keymap.preset {
            let (km, _) = load_keymap(&cfg.keys.preset, Some(&self.paths.keymap));
            self.keymap = km;
            app.set_keymap(&self.keymap);
        }
        app.apply_config(&cfg);
        app.ascii = self.glyphs.ascii;
        self.refresh = Duration::from_millis(cfg.general.refresh_ms.max(250));
        self.sync_layout(app, false);
    }

    /// Live reload (UX §12.2): re-reads every layer; a file that became invalid keeps its last good version
    /// and the error is shown with file:line. Keymap and layout files are re-read too.
    fn reload(&mut self, app: &mut App) {
        let mut l = load_layered_cached(&self.load_options, &mut self.cache);
        for (k, v) in &self.flags {
            let _ = l.set_runtime(k, v);
        }
        let errs: Vec<String> = l.errors.iter().map(|e| e.to_string()).collect();
        self.loaded = l.clone();
        self.saved = l;
        let (km, kerrs) = load_keymap(&self.loaded.config.keys.preset, Some(&self.paths.keymap));
        // An invalid keymap.toml keeps the keys that worked (same rule as config files).
        if kerrs.is_empty() {
            self.keymap = km;
            app.set_keymap(&self.keymap);
        }
        app.status = None;
        self.rebuild_look(app);
        self.sync_layout(app, true);
        let mut all: Vec<String> = errs;
        all.extend(kerrs.iter().map(|e| format!("{e} (kept the previous keys)")));
        all.extend(app.status.take());
        app.status = Some(if all.is_empty() {
            "config reloaded".into()
        } else {
            all.join("; ")
        });
    }

    fn state_write(
        &mut self,
        f: impl FnOnce(&mut oomtop_state::StateDb) -> oomtop_state::Result<()>,
    ) -> Option<String> {
        if self.opts.no_learn {
            return Some("not saved (--no-learn)".into());
        }
        let db = self.opts.state.as_mut()?;
        f(db).err().map(|e| format!("profile: {e}"))
    }
}

// ---------------------------------------------------------------------------------------------------------
// Effects
// ---------------------------------------------------------------------------------------------------------

enum Flow {
    Continue,
    Quit,
    Editor(PathBuf, Option<usize>),
}

fn apply_effect(s: &mut Session, app: &mut App, eff: Effect) -> Flow {
    let now = now_ms();
    match eff {
        Effect::Quit => return Flow::Quit,
        Effect::Execute(plan) => {
            let outcomes = match s.opts.actuator.as_mut() {
                Some(a) => a.execute(&plan),
                None => oomtop_core::actions::NoopActuator.execute(&plan),
            };
            app.on_outcomes(&outcomes);
            for o in &outcomes {
                if o.ok && o.target.kind != ActionKind::Resume {
                    if let Some(g) = app.group(&o.target.group_id) {
                        s.learning.events.push((
                            g.fingerprint.clone(),
                            oomtop_state::EventKind::Action,
                            now,
                            Some(app.mode.as_str().to_string()),
                        ));
                    }
                }
            }
            // Re-measure soon: the freed-memory report needs a fresh sample (of every source).
            s.next_refresh = Instant::now() + Duration::from_millis(600);
            s.force_next = true;
        }
        Effect::Pin { fingerprint, pinned } => {
            if let Some(e) = s.state_write(|db| db.set_pinned(&fingerprint, pinned, now)) {
                app.status = Some(format!("{} — {e}", app.status.clone().unwrap_or_default()));
            }
        }
        Effect::Mute {
            fingerprint,
            until_ms,
        } => {
            if let Some(e) = s.state_write(|db| db.set_muted(&fingerprint, until_ms, now)) {
                app.status = Some(format!("{} — {e}", app.status.clone().unwrap_or_default()));
            }
        }
        Effect::Rename { fingerprint, alias } => {
            if let Some(e) = s.state_write(|db| {
                db.rename(&fingerprint, alias.as_deref(), now)?;
                db.record_event(&fingerprint, oomtop_state::EventKind::Action, now, None)
            }) {
                app.status = Some(format!("{} — {e}", app.status.clone().unwrap_or_default()));
            }
        }
        Effect::Less { fingerprint } => {
            s.learning.events.push((
                fingerprint,
                oomtop_state::EventKind::Less,
                now,
                Some(app.mode.as_str().to_string()),
            ));
        }
        Effect::Selected {
            fingerprint,
            searched,
        } => {
            let kind = if searched {
                oomtop_state::EventKind::SearchSelect
            } else {
                oomtop_state::EventKind::Select
            };
            let imp = app.take_impression(&fingerprint);
            s.learning.impressions.push(imp);
            s.learning
                .events
                .push((fingerprint, kind, now, Some(app.mode.as_str().to_string())));
        }
        Effect::Query(q) => s.learning.queries.push((q, now)),
        Effect::OpenSettings => {
            let flag_keys: Vec<String> = s.flags.iter().map(|(k, _)| k.clone()).collect();
            let model = settings_model(&s.loaded, &flag_keys, &s.paths, &s.main_config, &s.hostname);
            s.saved = s.loaded.clone();
            app.show_settings(model);
        }
        Effect::Settings(cmd) => match cmd {
            SettingsCmd::Preview(k, v) => match s.loaded.set_runtime(&k, &v) {
                Ok(()) => s.rebuild_look(app),
                Err(e) => {
                    // Invalid value: roll the row back so it is neither shown as effective nor saved.
                    if let Some(st) = app.settings.as_mut() {
                        st.reject(&k, &e.message);
                    }
                }
            },
            SettingsCmd::Revert => {
                s.loaded = s.saved.clone();
                s.rebuild_look(app);
                app.status = Some("settings reverted".into());
            }
            SettingsCmd::Apply => {
                s.saved = s.loaded.clone();
                app.status = Some("settings applied for this session (s in settings saves them)".into());
            }
            SettingsCmd::Save(layer, changes) => {
                let path = match layer {
                    SaveLayer::User => s.main_config.clone(),
                    SaveLayer::Host => s.paths.host_file(&s.hostname),
                    SaveLayer::Dropin => s.paths.config_d.join(DROPIN_NAME),
                };
                match save_settings(&path, &changes) {
                    Ok(name) => {
                        s.saved = s.loaded.clone();
                        // A higher layer (host file, drop-in, env, flag) may still win over the file just
                        // written: say so instead of letting the saved value silently not apply next start.
                        let check = load_layered(&s.load_options);
                        let shadowed = overridden_after_save(&check, &path, &changes, &s.flags);
                        if let Some(st) = app.settings.as_mut() {
                            st.saved(&name);
                            if !shadowed.is_empty() {
                                st.message = Some(format!(
                                    "saved to {name}, but {} — edit or remove it there",
                                    shadowed.join("; ")
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        if let Some(st) = app.settings.as_mut() {
                            st.message = Some(format!("not saved: {e}"));
                        }
                    }
                }
            }
            SettingsCmd::Edit(layer, key) => {
                let origin = s.loaded.origin(&key);
                let (path, line) = match origin {
                    oomtop_config::Origin::File { path, line } => (path, line),
                    _ => {
                        let p = match layer {
                            SaveLayer::User => s.main_config.clone(),
                            SaveLayer::Host => s.paths.host_file(&s.hostname),
                            SaveLayer::Dropin => s.paths.config_d.join(DROPIN_NAME),
                        };
                        let line = std::fs::read_to_string(&p).ok().and_then(|t| find_line(&t, &key));
                        (p, line)
                    }
                };
                return Flow::Editor(path, line);
            }
        },
        Effect::SaveAlias { name, query } => {
            let lit = format!("\"{}\"", query.replace('\\', "\\\\").replace('"', "\\\""));
            match oomtop_config::write::set_value(&s.main_config, &format!("aliases.{name}"), &lit) {
                Ok(()) => {
                    app.status = Some(format!(
                        "saved alias {name} = {query:?} to {}",
                        s.main_config.display()
                    ))
                }
                Err(e) => app.status = Some(format!("alias not saved: {e}")),
            }
        }
        Effect::Refresh => s.next_refresh = Instant::now(),
        Effect::Layout(name) => {
            s.layout_override = Some(name.clone());
            s.sync_layout(app, true);
            if s.layout_loaded.as_deref() == Some(name.as_str()) {
                let shown = if name.is_empty() {
                    "adaptive"
                } else {
                    name.as_str()
                };
                let st = app.status.take().map(|x| format!("{x} · ")).unwrap_or_default();
                app.status = Some(format!("{st}layout {shown}"));
            }
        }
        Effect::Open(path) => app.status = Some(open_path(&path)),
    }
    Flow::Continue
}

// ---------------------------------------------------------------------------------------------------------
// Terminal lifecycle
// ---------------------------------------------------------------------------------------------------------

/// Restores the modes oomtop enabled on top of ratatui's (mouse, keyboard flags, focus reports). Safe to call
/// more than once and from the panic hook.
fn restore_modes() {
    use ratatui::crossterm::{event as ev, execute};
    let mut out = std::io::stdout();
    let _ = execute!(
        out,
        ev::PopKeyboardEnhancementFlags,
        ev::DisableMouseCapture,
        ev::DisableFocusChange
    );
}

/// On panic, give the terminal back (mouse reporting off, keyboard flags popped, main screen) before the
/// message prints — ratatui's own hook only leaves the alternate screen and raw mode.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_modes();
            ratatui::restore();
            prev(info);
        }));
    });
}

fn enter_terminal(
    mouse: bool,
    caps: &mut caps::TermCaps,
    known_kitty: Option<bool>,
) -> std::io::Result<ratatui::DefaultTerminal> {
    use ratatui::crossterm::{event as ev, execute};
    let terminal = ratatui::try_init()?;
    install_panic_hook();
    let mut out = std::io::stdout();
    if mouse && caps.mouse {
        execute!(out, ev::EnableMouseCapture)?;
    }
    // Focus reports (ignored by terminals without them): sampling slows down while unfocused.
    let _ = execute!(out, ev::EnableFocusChange);
    caps.kitty_keyboard = match known_kitty {
        Some(k) => k,
        None => ratatui::crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false),
    };
    if caps.kitty_keyboard {
        execute!(
            out,
            ev::PushKeyboardEnhancementFlags(ev::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    Ok(terminal)
}

fn leave_terminal(mouse: bool, caps: &caps::TermCaps) {
    use ratatui::crossterm::{event as ev, execute};
    let mut out = std::io::stdout();
    if caps.kitty_keyboard {
        let _ = execute!(out, ev::PopKeyboardEnhancementFlags);
    }
    if mouse && caps.mouse {
        let _ = execute!(out, ev::DisableMouseCapture);
    }
    let _ = execute!(out, ev::DisableFocusChange);
    ratatui::restore();
}

fn draw_frame(term: &mut ratatui::DefaultTerminal, s: &Session, app: &App) -> std::io::Result<()> {
    use ratatui::crossterm::{queue, terminal as t};
    if s.caps.sync_output {
        queue!(term.backend_mut(), t::BeginSynchronizedUpdate)?;
    }
    let borders = s.config().appearance.borders;
    term.draw(|f| render::draw_with(f, app, &s.palette, &s.glyphs, borders))?;
    if s.caps.sync_output {
        queue!(term.backend_mut(), t::EndSynchronizedUpdate)?;
    }
    term.backend_mut().flush()
}

fn run_editor(path: &Path, line: Option<usize>) -> String {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".into());
    let mut parts = editor.split_whitespace();
    let Some(bin) = parts.next() else {
        return "no $EDITOR".into();
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut cmd = std::process::Command::new(bin);
    cmd.args(parts);
    if let Some(l) = line {
        cmd.arg(format!("+{l}"));
    }
    cmd.arg(path);
    match cmd.status() {
        Ok(st) if st.success() => format!("edited {}", path.display()),
        Ok(st) => format!("{bin} exited with {st}"),
        Err(e) => format!("{bin}: {e}"),
    }
}

/// The linear `--plain` summary when the full-screen TUI must not start: `--plain`, `TERM=dumb`, or stdout is
/// not a terminal (piped, redirected, a test harness). `None` → run the TUI. `stdout_is_tty` is passed in so
/// the decision is testable from any environment.
pub fn plain_fallback(
    opts: &TuiOptions,
    caps: &caps::TermCaps,
    app: &App,
    stdout_is_tty: bool,
) -> Option<String> {
    if !(opts.plain || caps.dumb || !stdout_is_tty) {
        return None;
    }
    let text = plain_summary(&app.snapshot, &app.headroom, &app.headline.text);
    // `--ascii` (or ASCII glyphs from config / locale) reaches the plain / screen-reader output too.
    Some(if build_glyphs(&opts.config, opts.ascii, caps).ascii {
        ascii_text(&text)
    } else {
        text
    })
}

/// Runs the TUI until the user quits (loads the config layers itself for settings and live reload).
pub fn run(opts: TuiOptions) -> Result<(), TuiError> {
    run_with(opts, TuiExtras::default())
}

/// Runs the TUI with the CLI's config layers (preferred: exact origins and flag handling on reload).
pub fn run_with(mut opts: TuiOptions, extras: TuiExtras) -> Result<(), TuiError> {
    let mut caps = caps::detect(opts.config.appearance.color);
    let mut app = App::new(&opts.config);
    app.set_protect(opts.protect.clone());
    app.set_keymap(&opts.keymap);
    let snap = opts.provider.snapshot();
    app.update(snap, opts.provider.history());
    if let Some(text) = plain_fallback(&opts, &caps, &app, std::io::stdout().is_terminal()) {
        print!("{text}");
        return Ok(());
    }
    let disk_rx = extras.disk_models;
    let paths = extras.paths.unwrap_or_else(ConfigPaths::default_user);
    let load_options = extras.load_options.unwrap_or_else(|| LoadOptions {
        config_path: std::env::var_os("OOMTOP_CONFIG").map(PathBuf::from),
        ..Default::default()
    });
    let (loaded, flags) = match extras.loaded {
        Some(l) => (l, Vec::new()),
        None => {
            let mut l = load_layered(&load_options);
            let flags = flag_overrides(&opts.config, &l.config);
            for (k, v) in &flags {
                let _ = l.set_runtime(k, v);
            }
            (l, flags)
        }
    };
    // Prime the last-good cache with what is valid now, so the first bad edit already falls back to it.
    let mut cache = LayerCache::new();
    let _ = load_layered_cached(&load_options, &mut cache);
    let main_config = load_options
        .config_path
        .clone()
        .unwrap_or_else(|| paths.config_toml.clone());
    let hostname = load_options.hostname.clone().unwrap_or_else(short_hostname);
    let mouse = opts.config.general.mouse;
    let palette = build_palette(&opts.config, &opts.theme, &caps);
    let glyphs = build_glyphs(&opts.config, opts.ascii, &caps);
    app.ascii = glyphs.ascii;
    let keymap = opts.keymap.clone();
    let refresh = Duration::from_millis(opts.config.general.refresh_ms.max(250));
    let mut learning = Learning::new();
    if let (Some(db), false) = (opts.state.as_ref(), opts.no_learn) {
        let hl = opts.config.personalization.half_life_days.max(0.1) * 86_400.0;
        let now = now_ms();
        if let (Ok(f), Ok(e)) = (db.frecency(now, hl), db.entities()) {
            let q = db
                .top_queries(8, now, hl)
                .map(|v| v.into_iter().map(|(q, _)| q).collect())
                .unwrap_or_default();
            app.set_profile(f, &e, q);
        }
        learning.last_flush = Instant::now();
    }
    // Live reload (UX §12.2): watch the config dir and the system layer.
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let _watcher = if extras.watch && opts.config.general.live_reload {
        // The config dir (or its nearest existing ancestor, so creating it is noticed), the directory of an
        // explicit --config file, and the system layer.
        let mut watch_paths = paths.watch_paths();
        let system = load_options
            .system_dir
            .clone()
            .unwrap_or_else(oomtop_config::paths::system_dir);
        for p in [main_config.parent().map(Path::to_path_buf), Some(system)]
            .into_iter()
            .flatten()
        {
            if p.exists() && !p.as_os_str().is_empty() && !watch_paths.contains(&p) {
                watch_paths.push(p);
            }
        }
        let mode = oomtop_config::watch::WatchMode::from_poll_ms(opts.config.general.watch_poll_ms);
        oomtop_config::watch::watch(watch_paths, mode, move || {
            let _ = tx.send(());
        })
        .ok()
    } else {
        None
    };

    let known_kitty = extras.kitty_keyboard;
    let mut term = enter_terminal(mouse, &mut caps, known_kitty)?;
    let mut s = Session {
        saved: loaded.clone(),
        loaded,
        opts,
        caps,
        palette,
        glyphs,
        keymap,
        flags,
        load_options,
        paths,
        main_config,
        hostname,
        learning,
        refresh,
        next_refresh: Instant::now() + refresh,
        force_next: false,
        cache,
        layout_override: None,
        layout_loaded: None,
        unfocused: false,
        auto_variant: Variant::Dark,
    };
    s.auto_variant = auto_variant(
        extras.background,
        &s.opts.theme,
        std::env::var("COLORFGBG").ok().as_deref(),
    );
    s.sync_layout(&mut app, true);
    let started = Instant::now();
    let mut replies = if extras.late_replies {
        replies::LateReplyFilter::new(Some(replies::ARM_WINDOW), started)
    } else {
        replies::LateReplyFilter::off()
    };
    let result = (|| -> Result<(), TuiError> {
        let size = term.size()?;
        app.area = Rect::new(0, 0, size.width, size.height);
        // CPU % needs a second sample: take it quickly after the first frame (SPEC §6.2), with every source
        // forced — a cadence-gated sample 500 ms in would skip the process listing (due every refresh_ms).
        s.next_refresh = Instant::now() + Duration::from_millis(500);
        s.force_next = true;
        let mut dirty = true;
        // A config change seen while the settings screen is open is applied when it closes.
        let mut reload_pending = false;
        loop {
            if dirty {
                draw_frame(&mut term, &s, &app)?;
                dirty = false;
            }
            let timeout = s.next_refresh.saturating_duration_since(Instant::now());
            // While a possible late probe answer is held, poll often so a user's key is never delayed long.
            let cap = if replies.holding() {
                replies::HOLD_MAX / 3
            } else {
                Duration::from_millis(250)
            };
            let events = if event::poll(timeout.min(cap))? {
                let ev = event::read()?;
                replies.push(ev, Instant::now())
            } else {
                replies.expire(Instant::now())
            };
            let mut quit = false;
            for ev in events {
                let eff = match ev {
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                            Some(Effect::Quit)
                        } else {
                            key_string(k.code, k.modifiers).and_then(|key| app.on_key(&key, &s.keymap))
                        }
                    }
                    Event::Mouse(m) => {
                        let kind = match m.kind {
                            MouseEventKind::Down(MouseButton::Left) => Some(MouseKind::Down),
                            MouseEventKind::Drag(MouseButton::Left) => Some(MouseKind::Drag),
                            MouseEventKind::ScrollUp => Some(MouseKind::ScrollUp),
                            MouseEventKind::ScrollDown => Some(MouseKind::ScrollDown),
                            _ => None,
                        };
                        kind.and_then(|kind| {
                            app.on_mouse(MouseInput {
                                kind,
                                col: m.column,
                                row: m.row,
                                at_ms: started.elapsed().as_millis() as u64,
                            })
                        })
                    }
                    Event::Resize(w, h) => {
                        app.area = Rect::new(0, 0, w, h);
                        None
                    }
                    Event::FocusLost => {
                        s.unfocused = true;
                        None
                    }
                    Event::FocusGained => {
                        if s.unfocused {
                            s.unfocused = false;
                            s.next_refresh = Instant::now();
                        }
                        None
                    }
                    _ => None,
                };
                dirty = true;
                if let Some(e) = eff {
                    match apply_effect(&mut s, &mut app, e) {
                        Flow::Quit => {
                            quit = true;
                            break;
                        }
                        Flow::Continue => {}
                        Flow::Editor(path, line) => {
                            leave_terminal(mouse, &s.caps);
                            let msg = run_editor(&path, line);
                            term = enter_terminal(mouse, &mut s.caps, known_kitty)?;
                            term.clear()?;
                            s.reload(&mut app);
                            if let Some(st) = app.settings.as_mut() {
                                st.message = Some(msg);
                            }
                            let flag_keys: Vec<String> = s.flags.iter().map(|(k, _)| k.clone()).collect();
                            if app.settings.is_some() {
                                let model = settings_model(
                                    &s.loaded,
                                    &flag_keys,
                                    &s.paths,
                                    &s.main_config,
                                    &s.hostname,
                                );
                                if let Some(st) = app.settings.as_mut() {
                                    st.model = model;
                                }
                            }
                        }
                    }
                }
            }
            if quit {
                break;
            }
            if rx.try_recv().is_ok() {
                while rx.try_recv().is_ok() {}
                reload_pending = true;
            }
            if let Some(Ok(models)) = disk_rx.as_ref().map(|r| r.try_recv()) {
                app.disk_models = models;
                dirty = true;
            }
            if reload_pending && app.settings.is_none() {
                reload_pending = false;
                s.reload(&mut app);
                dirty = true;
            }
            if Instant::now() >= s.next_refresh {
                let snap = if std::mem::take(&mut s.force_next) {
                    s.opts.provider.snapshot_now()
                } else {
                    s.opts.provider.snapshot()
                };
                app.update(snap, s.opts.provider.history());
                s.next_refresh = Instant::now() + s.interval();
                dirty = true;
            }
            if s.learning.last_flush.elapsed() >= Duration::from_secs(30) {
                let hl = s.config().personalization.half_life_days;
                if let (Some(db), false) = (s.opts.state.as_mut(), s.opts.no_learn) {
                    flush_learning(db, &mut s.learning, &mut app, hl);
                } else {
                    s.learning = Learning::new();
                }
            }
        }
        Ok(())
    })();
    leave_terminal(mouse, &s.caps);
    let hl = s.config().personalization.half_life_days;
    if let (Some(db), false) = (s.opts.state.as_mut(), s.opts.no_learn) {
        flush_learning(db, &mut s.learning, &mut app, hl);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_plain_output_is_ascii_only() {
        let s = fixtures::motivating();
        let h = oomtop_core::headroom::compute(&s, &Default::default());
        let text = ascii_text(&plain_summary(
            &s,
            &h,
            "Idle leftovers — 2 build daemons could free ≈5.3 GiB · ≥1.5G…",
        ));
        assert!(text.is_ascii(), "{text}");
        assert!(
            text.contains("Idle leftovers - 2 build daemons could free ~5.3 GiB | >=1.5G..."),
            "{text}"
        );
        assert!(text.starts_with("oomtop - air, MacBook Air M5."), "{text}");
    }

    #[test]
    fn plain_reads_linearly() {
        let s = fixtures::motivating();
        let h = oomtop_core::headroom::compute(&s, &Default::default());
        let text = plain_summary(&s, &h, "Tight on memory.");
        assert!(text.contains("Tight on memory."));
        assert!(
            text.contains("1. sd-server · qwen-image-studio [model] 9.9G, 2 processes"),
            "{text}"
        );
        assert!(
            text.contains("GradleDaemon 9.8 [daemon] 2.9G, 1 process (idle 3h40m, reclaimable)"),
            "{text}"
        );
        assert!(
            text.contains("Claude Code d4e5 [agent]"),
            "duplicate labels are disambiguated: {text}"
        );
        assert!(text.contains("Memory pressure: warning."));
        assert!(
            text.contains("Swap: 6.1G of 7.0G used, growing 360M per minute."),
            "{text}"
        );
        assert!(text.contains("host footprint is a lower bound"));
        assert!(
            !text.contains('█') && !text.contains('●'),
            "no glyphs in plain output"
        );
        let empty = plain_summary(&Snapshot::default(), &Headroom::default(), "x");
        assert!(empty.contains("available unavailable"), "{empty}");
        assert!(empty.contains("Headroom: unavailable."));
    }

    /// Not a TTY → the plain summary instead of the full-screen TUI. Hermetic: the TTY state is injected, so
    /// the test passes whether `cargo test` runs in a terminal, a pipe or CI.
    #[test]
    fn run_falls_back_to_plain_without_a_tty() {
        let opts = |plain: bool| TuiOptions {
            provider: Box::new(oomtop_core::provider::StaticProvider::new(fixtures::motivating())),
            actuator: None,
            config: Config::default(),
            theme: terminal_theme(),
            keymap: oomtop_config::keymap::preset("default").unwrap(),
            state: None,
            protect: fixtures::protect(),
            plain,
            ascii: false,
            no_learn: true,
        };
        let env = |term: &str| vec![("TERM".to_string(), term.to_string())];
        let caps = caps::detect_from(&env("xterm-256color"), oomtop_config::model::ColorMode::Auto);
        let o = opts(false);
        let mut app = App::new(&o.config);
        app.update(
            fixtures::motivating(),
            &fixtures::history_for(&fixtures::motivating(), 5),
        );
        let text = plain_fallback(&o, &caps, &app, false).expect("not a tty → plain");
        assert!(text.contains(&app.headline.text), "{text}");
        assert_eq!(plain_fallback(&o, &caps, &app, true), None, "a tty runs the TUI");
        assert!(
            plain_fallback(&opts(true), &caps, &app, true).is_some(),
            "--plain wins over a tty"
        );
        let dumb = caps::detect_from(&env("dumb"), oomtop_config::model::ColorMode::Auto);
        assert!(dumb.dumb);
        assert!(
            plain_fallback(&o, &dumb, &app, true).is_some(),
            "TERM=dumb → plain"
        );
    }

    #[test]
    fn keys() {
        assert_eq!(
            key_string(KeyCode::Char('k'), KeyModifiers::CONTROL).as_deref(),
            Some("ctrl-k")
        );
        assert_eq!(
            key_string(KeyCode::Char('K'), KeyModifiers::CONTROL | KeyModifiers::SHIFT).as_deref(),
            Some("ctrl-k")
        );
        assert_eq!(
            key_string(KeyCode::F(9), KeyModifiers::NONE).as_deref(),
            Some("F9")
        );
        assert_eq!(
            key_string(KeyCode::Char('G'), KeyModifiers::SHIFT).as_deref(),
            Some("G")
        );
        assert_eq!(
            key_string(KeyCode::Char(' '), KeyModifiers::NONE).as_deref(),
            Some("space")
        );
        assert_eq!(
            key_string(KeyCode::BackTab, KeyModifiers::SHIFT).as_deref(),
            Some("backtab")
        );
        assert_eq!(
            key_string(KeyCode::Char('x'), KeyModifiers::CONTROL | KeyModifiers::ALT).as_deref(),
            Some("ctrl-alt-x")
        );
        // Every key string we emit is already canonical for the keymap.
        for k in ["ctrl-k", "G", "F9", "space", "backtab", "alt-x", "pagedown", "?"] {
            assert_eq!(oomtop_config::keymap::normalize_key(k).as_deref(), Ok(k));
        }
        assert_eq!(key_string(KeyCode::Null, KeyModifiers::NONE), None);
    }

    #[test]
    fn help_lists_every_bound_action_with_a_description() {
        for p in oomtop_config::keymap::PRESETS {
            let km = oomtop_config::keymap::preset(p).unwrap();
            let pairs = help_pairs(&km);
            assert!(pairs.len() >= 30, "{p}");
            for (k, d) in &pairs {
                assert!(!d.is_empty(), "{p}: {k} has no description");
            }
        }
        let htop = help_pairs(&oomtop_config::keymap::preset("htop").unwrap());
        assert!(htop
            .iter()
            .any(|(k, d)| k.contains("F9") && d.starts_with("stop")));
    }

    #[test]
    fn flags_are_detected_as_overrides() {
        let base = Config::default();
        let mut wanted = base.clone();
        wanted.appearance.theme = "none".into();
        wanted.general.refresh_ms = 1000;
        let f = flag_overrides(&wanted, &base);
        assert_eq!(
            f,
            vec![
                ("appearance.theme".to_string(), "\"none\"".to_string()),
                ("general.refresh_ms".to_string(), "1000".to_string())
            ]
        );
    }

    #[test]
    fn palette_and_glyph_selection() {
        let cfg = Config::default();
        let env = |p: &[(&str, &str)]| -> Vec<(String, String)> {
            p.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
        };
        let nc = caps::detect_from(
            &env(&[("NO_COLOR", "1"), ("TERM", "xterm-256color")]),
            cfg.appearance.color,
        );
        let p = build_palette(&cfg, &terminal_theme(), &nc);
        assert_eq!(p.depth(), ColorDepth::None);
        assert_eq!(p.theme_name(), "none");
        let c = caps::detect_from(&env(&[("LANG", "C")]), cfg.appearance.color);
        assert!(
            build_glyphs(&cfg, false, &c).ascii,
            "non-UTF-8 locale → ASCII glyphs"
        );
        let c = caps::detect_from(&env(&[("LANG", "en_US.UTF-8")]), cfg.appearance.color);
        assert!(!build_glyphs(&cfg, false, &c).ascii);
        assert!(build_glyphs(&cfg, true, &c).ascii);
    }

    /// UX acceptance #10: saving from the settings screen preserves comments and key order.
    #[test]
    fn settings_save_preserves_comments_and_order() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        let original = "# my oomtop settings\n[appearance]\n# the look\ntheme = \"terminal\"  # default\ndensity = \"compact\"\n\n[general]\n# slow machine\nrefresh_ms = 3000\n";
        std::fs::write(&p, original).unwrap();
        let name = save_settings(
            &p,
            &[
                ("appearance.theme".into(), "\"none\"".into()),
                ("format.decimals".into(), "2".into()),
            ],
        )
        .unwrap();
        assert_eq!(name, "config.toml");
        let out = std::fs::read_to_string(&p).unwrap();
        insta::assert_snapshot!("settings_save_golden", out);
        assert!(save_settings(&p, &[("general.refresh_ms".into(), "\"fast\"".into())]).is_err());
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            out,
            "invalid values are not written"
        );
    }

    /// Records plans instead of signalling (tests never signal anything).
    struct Recorder(std::sync::Arc<std::sync::Mutex<Vec<oomtop_core::actions::ActionPlan>>>);

    impl Actuator for Recorder {
        fn execute(
            &mut self,
            plan: &oomtop_core::actions::ActionPlan,
        ) -> Vec<oomtop_core::actions::ActionOutcome> {
            self.0.lock().unwrap().push(plan.clone());
            plan.targets
                .iter()
                .map(|t| oomtop_core::actions::ActionOutcome {
                    target: t.clone(),
                    ok: true,
                    message: "sent SIGTERM".into(),
                    measured_gain: None,
                })
                .collect()
        }
    }

    fn test_session(dir: &Path, rec: Recorder, db: Option<oomtop_state::StateDb>) -> Session {
        let load_options = LoadOptions {
            config_dir: Some(dir.to_path_buf()),
            system_dir: Some(PathBuf::new()),
            hostname: Some("air".into()),
            env: Some(vec![]),
            ..Default::default()
        };
        let loaded = load_layered(&load_options);
        let paths = ConfigPaths::new(dir);
        let caps = caps::detect_from(
            &[("TERM".into(), "xterm-256color".into())],
            loaded.config.appearance.color,
        );
        let opts = TuiOptions {
            provider: Box::new(oomtop_core::provider::StaticProvider::new(fixtures::motivating())),
            actuator: Some(Box::new(rec)),
            config: loaded.config.clone(),
            theme: terminal_theme(),
            keymap: oomtop_config::keymap::preset("default").unwrap(),
            state: db,
            protect: fixtures::protect(),
            plain: false,
            ascii: false,
            no_learn: false,
        };
        Session {
            palette: build_palette(&loaded.config, &terminal_theme(), &caps),
            glyphs: build_glyphs(&loaded.config, false, &caps),
            keymap: opts.keymap.clone(),
            saved: loaded.clone(),
            main_config: paths.config_toml.clone(),
            loaded,
            opts,
            caps,
            flags: Vec::new(),
            load_options,
            paths,
            hostname: "air".into(),
            learning: Learning::new(),
            refresh: Duration::from_millis(2000),
            next_refresh: Instant::now(),
            force_next: false,
            cache: LayerCache::new(),
            layout_override: None,
            layout_loaded: None,
            unfocused: false,
            auto_variant: Variant::Dark,
        }
    }

    fn key(s: &mut Session, app: &mut App, k: &str) {
        if let Some(e) = app.on_key(k, &s.keymap.clone()) {
            let _ = apply_effect(s, app, e);
        }
    }

    /// Settings through the loop: preview re-themes live, `s` saves comment-preserving to config.toml,
    /// `esc` reverts the session; `:save` writes an alias; a confirmed stop reaches the actuator exactly once.
    #[test]
    fn session_effects_settings_alias_and_actions() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.toml");
        std::fs::write(&cfg, "# keep me\n[appearance]\ntheme = \"terminal\" # mine\n").unwrap();
        let plans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let db = oomtop_state::StateDb::open_in_memory().unwrap();
        let mut s = test_session(d.path(), Recorder(plans.clone()), Some(db));
        let mut app = App::new(s.config());
        app.set_protect(fixtures::protect());
        app.update(fixtures::motivating(), &oomtop_core::history::History::default());

        key(&mut s, &mut app, ",");
        let st = app.settings.as_mut().expect("settings open");
        let theme_row = st
            .model
            .rows
            .iter()
            .position(|r| r.key == "appearance.theme")
            .unwrap();
        st.selected = theme_row;
        assert_eq!(st.row().unwrap().origin, "config.toml:3");
        // cycle until the `none` theme is previewed
        for _ in 0..12 {
            if s.loaded.config.appearance.theme == "none" {
                break;
            }
            key(&mut s, &mut app, "right");
        }
        assert_eq!(s.loaded.config.appearance.theme, "none");
        assert_eq!(
            s.palette.theme_name(),
            "none",
            "live preview re-renders with the theme"
        );
        key(&mut s, &mut app, "s");
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert_eq!(text, "# keep me\n[appearance]\ntheme = \"none\" # mine\n");
        assert!(app.settings.as_ref().unwrap().pending.is_empty());
        // another preview, then esc reverts to the saved state and closes
        key(&mut s, &mut app, "down");
        key(&mut s, &mut app, "esc");
        assert!(app.settings.is_none());
        assert_eq!(s.loaded.config.appearance.theme, "none");

        // :save NAME persists the current filter as an alias
        app.set_filter(Some("kind:daemon".into()));
        key(&mut s, &mut app, ":");
        for c in ["s", "a", "v", "e", "space", "d", "m", "n", "enter"] {
            key(&mut s, &mut app, c);
        }
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(text.contains("[aliases]\ndmn = \"kind:daemon\""), "{text}");
        app.set_filter(None);

        // x → y: one plan, confirmed, SIGTERM; learning signal queued
        key(&mut s, &mut app, "g");
        let i = app
            .rows
            .iter()
            .position(|r| r.key == app::RowKey::Group("daemon:gradle".into()))
            .unwrap();
        for _ in 0..i {
            key(&mut s, &mut app, "j");
        }
        key(&mut s, &mut app, "x");
        assert!(
            plans.lock().unwrap().is_empty(),
            "nothing sent before confirmation"
        );
        key(&mut s, &mut app, "y");
        let sent = plans.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].confirmed && !sent[0].confirmed_kill);
        assert_eq!(sent[0].targets[0].kind, ActionKind::Terminate);
        assert_eq!(sent[0].targets[0].group_id, "daemon:gradle");
        assert_eq!(app.pending.len(), 1, "SIGTERM is followed up");
        assert!(s
            .learning
            .events
            .iter()
            .any(|(fp, k, _, _)| fp == "fp-gradle" && *k == oomtop_state::EventKind::Action));

        // pin persists immediately; selection + flush feed frecency back into ranking
        key(&mut s, &mut app, "p");
        key(&mut s, &mut app, "enter");
        let hl = s.config().personalization.half_life_days;
        if let Some(db) = s.opts.state.as_mut() {
            flush_learning(db, &mut s.learning, &mut app, hl);
            let e = db.entities().unwrap();
            assert!(e.iter().any(|x| x.fingerprint == "fp-gradle" && x.pinned));
            assert!(db.ux_stats(0).unwrap().impressions >= 1);
        }
        assert!(app.pinned.contains("fp-gradle"));
        assert!(app.affinity.get("fp-gradle").copied().unwrap_or(0.0) > 0.0);
    }

    #[test]
    fn settings_model_from_layers() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "[appearance]\ntheme = \"none\"\n").unwrap();
        std::fs::create_dir(d.path().join("config.d")).unwrap();
        std::fs::write(
            d.path().join("config.d/host-air.toml"),
            "[appearance]\ntheme = \"terminal\"\n",
        )
        .unwrap();
        let opts = LoadOptions {
            config_dir: Some(d.path().to_path_buf()),
            system_dir: Some(PathBuf::new()),
            hostname: Some("air".into()),
            env: Some(vec![]),
            ..Default::default()
        };
        let loaded = load_layered(&opts);
        let paths = ConfigPaths::new(d.path());
        let m = settings_model(&loaded, &[], &paths, &paths.config_toml, "air");
        let theme = m.rows.iter().find(|r| r.key == "appearance.theme").unwrap();
        assert_eq!(theme.value, "terminal");
        assert_eq!(theme.origin, "host-air.toml:2");
        assert_eq!(
            theme.also_in,
            vec!["config.toml:2".to_string()],
            "overridden elsewhere is shown"
        );
        assert!(m.themes.iter().any(|t| t.name == "terminal" && t.badge == "ok"));
        assert_eq!(m.layer_paths[1].1, d.path().join("config.d/host-air.toml"));
        // An unset sibling of a set key is a default, not "also in config.toml:<line of [appearance]>".
        let glyphs = m.rows.iter().find(|r| r.key == "appearance.glyphs").unwrap();
        assert!(glyphs.also_in.is_empty(), "{:?}", glyphs.also_in);
    }

    #[test]
    fn setting_line_ignores_table_headers_but_finds_inline_values() {
        let text = "[general]\nrefresh_ms = 2000\n[adapters]\nports = { ollama = 11434 }\n";
        let lines = oomtop_config::layered::key_lines(text);
        assert_eq!(setting_line(text, &lines, "general.refresh_ms"), Some(2));
        assert_eq!(setting_line(text, &lines, "general.mouse"), None);
        assert_eq!(setting_line(text, &lines, "adapters.ports.ollama"), Some(4));
        assert_eq!(setting_line(text, &lines, "adapters.enabled"), None);
    }

    /// UX acceptance #9 (in-process): an edit applies on reload; an invalid edit keeps the previous values and
    /// shows file:line.
    #[test]
    fn live_reload_applies_edits_and_keeps_last_good_on_error() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.toml");
        std::fs::write(&cfg, "[appearance]\ntheme = \"terminal\"\n").unwrap();
        let plans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut s = test_session(d.path(), Recorder(plans), None);
        let _ = load_layered_cached(&s.load_options, &mut s.cache);
        let mut app = App::new(s.config());
        app.update(fixtures::motivating(), &oomtop_core::history::History::default());

        std::fs::write(
            &cfg,
            "[appearance]\ntheme = \"none\"\n[general]\nrefresh_ms = 1000\n",
        )
        .unwrap();
        s.reload(&mut app);
        assert_eq!(s.config().appearance.theme, "none");
        assert_eq!(s.palette.theme_name(), "none", "re-themed live");
        assert_eq!(s.refresh, Duration::from_millis(1000));
        assert_eq!(app.status.as_deref(), Some("config reloaded"));

        std::fs::write(&cfg, "[appearance]\ntheme = 3\n").unwrap();
        s.reload(&mut app);
        assert_eq!(s.config().appearance.theme, "none", "last good version kept");
        assert_eq!(s.refresh, Duration::from_millis(1000));
        let st = app.status.clone().unwrap();
        assert!(st.contains("config.toml:2"), "file:line shown: {st}");

        // keymap.toml: a valid edit applies, an invalid one keeps the previous keys
        std::fs::write(d.path().join("keymap.toml"), "[global]\n\"K\" = \"stop\"\n").unwrap();
        s.reload(&mut app);
        assert_eq!(s.keymap.action("global", "K"), Some("stop"));
        std::fs::write(
            d.path().join("keymap.toml"),
            "[global]\n\"K\" = \"no-such-action\"\n",
        )
        .unwrap();
        s.reload(&mut app);
        assert_eq!(s.keymap.action("global", "K"), Some("stop"), "previous keys kept");
        assert!(
            app.status.as_deref().unwrap().contains("kept the previous keys"),
            "{:?}",
            app.status
        );
    }

    /// Saving to config.toml while a host file sets the same key: the save works and says it is shadowed.
    #[test]
    fn save_warns_when_a_higher_layer_overrides() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "# mine\n").unwrap();
        std::fs::create_dir(d.path().join("config.d")).unwrap();
        std::fs::write(
            d.path().join("config.d/host-air.toml"),
            "[appearance]\ndensity = \"compact\"\n",
        )
        .unwrap();
        let plans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut s = test_session(d.path(), Recorder(plans), None);
        let mut app = App::new(s.config());
        app.update(fixtures::motivating(), &oomtop_core::history::History::default());
        let _ = apply_effect(&mut s, &mut app, Effect::OpenSettings);
        let _ = apply_effect(
            &mut s,
            &mut app,
            Effect::Settings(SettingsCmd::Save(
                SaveLayer::User,
                vec![("appearance.density".into(), "\"comfortable\"".into())],
            )),
        );
        let text = std::fs::read_to_string(d.path().join("config.toml")).unwrap();
        assert!(text.contains("density = \"comfortable\""), "{text}");
        let msg = app.settings.as_ref().unwrap().message.clone().unwrap();
        assert!(msg.contains("overridden in host-air.toml:2"), "{msg}");
    }

    /// An invalid preview is rolled back in the settings screen (not shown as effective, not saved).
    #[test]
    fn invalid_preview_is_rejected_through_the_loop() {
        let d = tempfile::tempdir().unwrap();
        let plans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut s = test_session(d.path(), Recorder(plans), None);
        let mut app = App::new(s.config());
        app.update(fixtures::motivating(), &oomtop_core::history::History::default());
        key(&mut s, &mut app, ",");
        let st = app.settings.as_mut().unwrap();
        st.selected = st
            .model
            .rows
            .iter()
            .position(|r| r.key == "general.refresh_ms")
            .unwrap();
        for k in ["enter", "ctrl-u", "f", "a", "s", "t", "enter"] {
            key(&mut s, &mut app, k);
        }
        let st = app.settings.as_ref().unwrap();
        assert_eq!(st.row().unwrap().value, "2000");
        assert!(st.pending.is_empty());
        assert!(
            st.message.as_deref().unwrap().contains("not applied"),
            "{:?}",
            st.message
        );
        assert_eq!(s.config().general.refresh_ms, 2000);
    }

    /// `layout.name`, `:layout NAME` and bad layout files (UX §12.6).
    #[test]
    fn layouts_load_from_files_and_errors_keep_the_current_one() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("layouts")).unwrap();
        std::fs::write(
            d.path().join("layouts/llm-dev.toml"),
            "[columns.groups]\nshow = [\"name\", \"footprint\", \"mem_share\"]\nsort = \"footprint\"\n[[columns.custom]]\nid = \"mem_share\"\ntitle = \"MEM%\"\nexpr = \"footprint / host.mem.total * 100\"\nformat = \"{:.0}%\"\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("layouts/broken.toml"),
            "[columns.groups]\nshow = [\"nosuch\"]\n",
        )
        .unwrap();
        let plans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut s = test_session(d.path(), Recorder(plans), None);
        let mut app = App::new(s.config());
        app.update(fixtures::motivating(), &oomtop_core::history::History::default());
        let _ = apply_effect(&mut s, &mut app, Effect::Layout("llm-dev".into()));
        assert_eq!(app.layout.as_ref().map(|l| l.name.as_str()), Some("llm-dev"));
        assert_eq!(app.home_sort, app::HomeSort::Footprint);
        let _ = apply_effect(&mut s, &mut app, Effect::Layout("broken".into()));
        assert_eq!(
            app.layout.as_ref().map(|l| l.name.as_str()),
            Some("llm-dev"),
            "kept"
        );
        let st = app.status.clone().unwrap();
        assert!(
            st.contains("broken.toml") && st.contains("kept the current layout"),
            "{st}"
        );
        let _ = apply_effect(&mut s, &mut app, Effect::Layout("adaptive".into()));
        assert!(app.layout.is_none());
        assert!(load_active_layout("missing", &d.path().join("layouts")).is_err());
        assert_eq!(load_active_layout("", Path::new("/nonexistent")), Ok(None));
    }

    #[test]
    fn open_refuses_relative_paths() {
        assert!(open_path("-a Calculator").starts_with("not opening"));
        assert!(open_path("relative/dir").starts_with("not opening"));
    }

    /// Light/dark variants follow `appearance` on reload and preview (not only at start-up).
    #[test]
    fn theme_variant_follows_appearance() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "[appearance]\ntheme = \"mono\"\n").unwrap();
        let plans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut s = test_session(d.path(), Recorder(plans), None);
        let mut app = App::new(s.config());
        s.rebuild_look(&mut app);
        let dark = s.palette.theme().tokens["ui.selection"].bg.clone();
        assert!(s.loaded.set_runtime("appearance.appearance", "\"light\"").is_ok());
        s.rebuild_look(&mut app);
        let light = s.palette.theme().tokens["ui.selection"].bg.clone();
        assert_ne!(dark, light, "light variant loaded");
        assert_eq!(s.palette.theme().appearance.as_deref(), Some("light"));

        let t = terminal_theme();
        assert_eq!(auto_variant(Some((250, 250, 250)), &t, None), Variant::Light);
        assert_eq!(auto_variant(None, &t, Some("0;15")), Variant::Light);
        assert_eq!(auto_variant(None, &t, Some("15;0")), Variant::Dark);
        assert_eq!(auto_variant(None, &t, None), Variant::Dark);
        let mut light_cli = t.clone();
        light_cli.appearance = Some("light".into());
        assert_eq!(
            auto_variant(None, &light_cli, Some("15;0")),
            Variant::Light,
            "CLI probe wins"
        );
    }

    /// UX acceptance #9 through the file watcher the event loop uses: an edit to config.toml reaches the
    /// session and applies well within 1 s (poll watcher so the test does not depend on FSEvents/inotify).
    #[test]
    fn watcher_delivers_edits_within_a_second() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("config.toml");
        std::fs::write(&cfg, "[general]\nrefresh_ms = 2000\n").unwrap();
        let plans = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut s = test_session(d.path(), Recorder(plans), None);
        let mut app = App::new(s.config());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let _w = oomtop_config::watch::watch(
            s.paths.watch_paths(),
            oomtop_config::watch::WatchMode::Poll(Duration::from_millis(50)),
            move || {
                let _ = tx.send(());
            },
        )
        .expect("watcher");
        std::thread::sleep(Duration::from_millis(120)); // let the poller take its first scan
        let t0 = Instant::now();
        std::fs::write(&cfg, "[general]\nrefresh_ms = 1000\n").unwrap();
        rx.recv_timeout(Duration::from_secs(5)).expect("change event");
        s.reload(&mut app);
        let took = t0.elapsed();
        assert_eq!(s.config().general.refresh_ms, 1000);
        eprintln!("edit → applied in {took:?}");
        assert!(took < Duration::from_secs(1), "applied in {took:?}");
    }
}

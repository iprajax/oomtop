//! Themes (UX §12.3–12.4): semantic tokens → styles. Widgets ask for `mem.gpu` or `state.warn`, never a raw
//! color. The default `terminal` theme uses only the 16 ANSI colors + default fg/bg and **never paints a
//! background**; `none` is attributes only (used when `NO_COLOR` is set). No blue/purple in built-in themes.
//!
//! Theme file format (`themes/<name>.toml`):
//! ```toml
//! name = "night-ember"
//! inherits = "mono"            # only override what differs
//! appearance = "dark"
//! [ui]
//! accent = { fg = "#ffb547", bold = true }
//! "border.focus" = { bold = true }     # or nested: border.focus = { … }
//! [mem]
//! swap = { fg = "@state.warn", underline = true }
//! gpu = "#ff8a6b"                      # shorthand for { fg = "#ff8a6b" }
//! ```
//!
//! Built-in truecolor themes (`mono`, `ember`, `mint`, `sand`, `coral`, `high-contrast`, `colorblind`) ship a
//! dark and a light variant; `<name>` picks by the requested [`Variant`] (terminal background via OSC 11,
//! falling back to dark), `<name>-dark` / `<name>-light` pin one.
//!
//! **Contrast guard** (hex colors only — the `terminal` theme's colors are whatever the terminal defines):
//! text tokens need WCAG ≥ 4.5:1 against their background, state colors ≥ 3:1. [`load_theme`] nudges failing
//! colors in lightness; [`check`] reports them (`oomtop theme check`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;

/// Every semantic token (UX §12.3).
pub const TOKENS: &[&str] = &[
    "ui.surface",
    "ui.text",
    "ui.muted",
    "ui.faint",
    "ui.border",
    "ui.border.focus",
    "ui.accent",
    "ui.accent.text",
    "ui.selection",
    "ui.selection.text",
    "state.ok",
    "state.warn",
    "state.crit",
    "state.info",
    "state.stale",
    "mem.app",
    "mem.gpu",
    "mem.compressed",
    "mem.wired",
    "mem.cache",
    "mem.free",
    "mem.swap",
    "kind.agent",
    "kind.model",
    "kind.sandbox",
    "kind.daemon",
    "kind.app",
    "kind.system",
    "kind.other",
    "chart.spark",
    "chart.spark.peak",
    "chart.bar.fill",
    "chart.bar.track",
    "text.number",
    "text.unit",
    "text.label",
    "text.key",
    "text.link",
    "text.headline",
];

/// Token sections (UX §12.3).
pub const SECTIONS: &[&str] = &["ui", "state", "mem", "kind", "chart", "text"];

/// Built-in theme names (UX §12.4).
pub const BUILTIN_THEMES: &[&str] = &[
    "terminal",
    "none",
    "mono",
    "ember",
    "mint",
    "sand",
    "coral",
    "high-contrast",
    "colorblind",
];

/// Built-in themes with light and dark truecolor variants.
pub const TRUECOLOR_THEMES: &[&str] = &[
    "mono",
    "ember",
    "mint",
    "sand",
    "coral",
    "high-contrast",
    "colorblind",
];

/// Minimum WCAG contrast for text tokens (UX §12.4).
pub const TEXT_CONTRAST: f64 = 4.5;
/// Minimum WCAG contrast for state colors (UX §12.4).
pub const STATE_CONTRAST: f64 = 3.0;
/// Text tokens checked at [`TEXT_CONTRAST`].
pub const TEXT_TOKENS: &[&str] = &[
    "ui.text",
    "ui.muted",
    "ui.accent",
    "state.info",
    "text.number",
    "text.unit",
    "text.label",
    "text.key",
    "text.link",
    "text.headline",
    "ui.selection.text",
    "ui.accent.text",
];
/// State tokens checked at [`STATE_CONTRAST`].
pub const STATE_TOKENS: &[&str] = &["state.ok", "state.warn", "state.crit"];

const STYLE_KEYS: &[&str] = &["fg", "bg", "bold", "dim", "italic", "underline", "reverse"];

/// Which variant of a light/dark theme to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Variant {
    #[default]
    Dark,
    Light,
}

impl Variant {
    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Dark => "dark",
            Variant::Light => "light",
        }
    }
    pub fn parse(s: &str) -> Option<Variant> {
        match s {
            "dark" => Some(Variant::Dark),
            "light" => Some(Variant::Light),
            _ => None,
        }
    }
    /// Variant for a background color (relative luminance > 0.4 = light).
    pub fn for_background(rgb: (u8, u8, u8)) -> Variant {
        if luminance(rgb) > 0.4 {
            Variant::Light
        } else {
            Variant::Dark
        }
    }

    /// Variant from `$COLORFGBG` (`"15;0"`, `"0;default;15"`): the last field is the background's ANSI index;
    /// 7 and 9–15 are light, 0–6 and 8 dark. `None` when absent or not a number (`default`).
    pub fn from_colorfgbg(v: &str) -> Option<Variant> {
        let bg: u8 = v.rsplit(';').next()?.trim().parse().ok()?;
        Some(match bg {
            7 | 9..=15 => Variant::Light,
            0..=6 | 8 => Variant::Dark,
            // 256-color background: judge by its RGB
            i => xterm256_rgb(i).map(Variant::for_background)?,
        })
    }

    /// UX §12.4 `appearance`: `dark` / `light` pin the variant; `auto` uses the terminal background from an
    /// OSC 11 query when known, falling back to `$COLORFGBG`, then dark.
    pub fn resolve(
        mode: crate::model::AppearanceMode,
        osc11_background: Option<(u8, u8, u8)>,
        colorfgbg: Option<&str>,
    ) -> Variant {
        use crate::model::AppearanceMode;
        match mode {
            AppearanceMode::Dark => Variant::Dark,
            AppearanceMode::Light => Variant::Light,
            AppearanceMode::Auto => osc11_background
                .map(Variant::for_background)
                .or_else(|| colorfgbg.and_then(Variant::from_colorfgbg))
                .unwrap_or(Variant::Dark),
        }
    }
}

/// One token's style. Colors: ANSI index/name ("3", "yellow", "bright-black"), "default", "#rrggbb", or a
/// reference "@ui.accent".
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Style {
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Style {
    pub fn fg(c: &str) -> Self {
        Style {
            fg: Some(c.into()),
            ..Default::default()
        }
    }
    pub fn bg(c: &str) -> Self {
        Style {
            bg: Some(c.into()),
            ..Default::default()
        }
    }
    fn attrs(bold: bool, dim: bool, reverse: bool) -> Self {
        Style {
            bold,
            dim,
            reverse,
            ..Default::default()
        }
    }
    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
    fn underline(mut self) -> Self {
        self.underline = true;
        self
    }
    fn with_bg(mut self, c: &str) -> Self {
        self.bg = Some(c.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub inherits: Option<String>,
    /// "dark" | "light" | None (follows terminal).
    pub appearance: Option<String>,
    /// Token → style (unresolved; may contain "@ref" colors).
    pub tokens: BTreeMap<String, Style>,
}

impl Theme {
    /// True when any token uses a hex color (the contrast guard applies).
    pub fn has_hex_colors(&self) -> bool {
        self.tokens
            .values()
            .flat_map(|s| [&s.fg, &s.bg])
            .flatten()
            .any(|c| c.starts_with('#'))
    }

    /// A token's style (default style when missing).
    pub fn style(&self, token: &str) -> Style {
        self.tokens.get(token).cloned().unwrap_or_default()
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ThemeError {
    #[error("{0}: {1}")]
    Parse(String, String),
    #[error("theme {0:?} not found")]
    NotFound(String),
    #[error("inheritance cycle at {0:?}")]
    Cycle(String),
}

/// A problem reported by `oomtop theme check`.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeIssue {
    pub token: String,
    pub message: String,
}

const ANSI_NAMES: &[&str] = &[
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright-black",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
    "bright-white",
    "default",
];

/// Validates a color string.
pub fn valid_color(c: &str) -> bool {
    if ANSI_NAMES.contains(&c) {
        return true;
    }
    if let Some(r) = c.strip_prefix('@') {
        return TOKENS.contains(&r);
    }
    if let Some(hex) = c.strip_prefix('#') {
        return hex.len() == 6 && hex.chars().all(|x| x.is_ascii_hexdigit());
    }
    c.parse::<u8>().is_ok()
}

/// RGB of an xterm-256 palette index ≥ 16 (6×6×6 cube and grey ramp). 0–15 are terminal-defined.
pub fn xterm256_rgb(i: u8) -> Option<(u8, u8, u8)> {
    match i {
        0..=15 => None,
        16..=231 => {
            let n = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            Some((level(n / 36), level((n / 6) % 6), level(n % 6)))
        }
        _ => {
            let g = 8 + 10 * (i - 232);
            Some((g, g, g))
        }
    }
}

/// ANSI blue/magenta (by name or index), or a hex / xterm-256 color whose hue is blue/violet/purple.
pub fn is_blue_or_purple(c: &str) -> bool {
    if matches!(
        c,
        "blue" | "magenta" | "bright-blue" | "bright-magenta" | "4" | "5" | "12" | "13"
    ) {
        return true;
    }
    let rgb = hex_rgb(c).or_else(|| c.parse::<u8>().ok().and_then(xterm256_rgb));
    rgb.is_some_and(|rgb| {
        let (h, s, l) = rgb_to_hsl(rgb);
        (195.0..=335.0).contains(&h) && s >= 0.25 && l > 0.08 && l < 0.92
    })
}

fn flatten(
    prefix: &str,
    t: &toml::Table,
    out: &mut BTreeMap<String, Style>,
    file: &str,
) -> Result<(), ThemeError> {
    let mut own = toml::Table::new();
    for (k, v) in t {
        let key = format!("{prefix}.{k}");
        match v {
            toml::Value::Table(sub) if !STYLE_KEYS.contains(&k.as_str()) => {
                // nested token table or a style table
                if sub.keys().all(|x| STYLE_KEYS.contains(&x.as_str())) {
                    let s: Style = toml::Value::Table(sub.clone())
                        .try_into()
                        .map_err(|e| ThemeError::Parse(file.into(), format!("{key}: {e}")))?;
                    out.insert(key, s);
                } else {
                    flatten(&key, sub, out, file)?;
                }
            }
            toml::Value::String(s) if !STYLE_KEYS.contains(&k.as_str()) => {
                out.insert(key, Style::fg(s));
            }
            _ => {
                own.insert(k.clone(), v.clone());
            }
        }
    }
    if !own.is_empty() {
        let s: Style = toml::Value::Table(own)
            .try_into()
            .map_err(|e| ThemeError::Parse(file.into(), format!("{prefix}: {e}")))?;
        out.insert(prefix.to_string(), s);
    }
    Ok(())
}

/// Parses a theme file. Errors carry `file:line` when the TOML parser knows the position.
pub fn parse_theme(text: &str, file: &str) -> Result<Theme, ThemeError> {
    let t: toml::Table = toml::from_str(text).map_err(|e| {
        let loc = e
            .span()
            .map(|s| format!("{file}:{}", crate::layered::line_of(text, s.start)))
            .unwrap_or_else(|| file.to_string());
        ThemeError::Parse(loc, e.message().to_string())
    })?;
    let mut theme = Theme::default();
    for (k, v) in &t {
        match (k.as_str(), v) {
            ("name", toml::Value::String(s)) => theme.name = s.clone(),
            ("inherits", toml::Value::String(s)) => theme.inherits = Some(s.clone()),
            ("appearance", toml::Value::String(s)) => {
                if Variant::parse(s).is_none() {
                    return Err(ThemeError::Parse(
                        file.into(),
                        format!("appearance must be \"dark\" or \"light\", not {s:?}"),
                    ));
                }
                theme.appearance = Some(s.clone())
            }
            (sec, toml::Value::Table(tbl)) => flatten(sec, tbl, &mut theme.tokens, file)?,
            (other, _) => {
                return Err(ThemeError::Parse(
                    file.into(),
                    format!("unexpected key {other:?}"),
                ))
            }
        }
    }
    if theme.name.is_empty() {
        theme.name = Path::new(file)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    Ok(theme)
}

fn put(t: &mut BTreeMap<String, Style>, k: &str, s: Style) {
    t.insert(k.to_string(), s);
}

/// The default theme: ANSI 16 + default fg/bg, transparent; accent = bold default fg; ok/warn/crit =
/// green/yellow/red and used for state only (categories such as swap, GPU memory and model servers use
/// attributes); no blue/magenta.
pub fn terminal_theme() -> Theme {
    let mut t = BTreeMap::new();
    let s = &mut t;
    put(s, "ui.surface", Style::default());
    put(s, "ui.text", Style::fg("default"));
    put(s, "ui.muted", Style::attrs(false, true, false));
    put(s, "ui.faint", Style::fg("bright-black"));
    put(s, "ui.border", Style::fg("bright-black"));
    put(s, "ui.border.focus", Style::attrs(true, false, false));
    put(s, "ui.accent", Style::fg("default").bold());
    put(s, "ui.accent.text", Style::attrs(true, false, false));
    put(s, "ui.selection", Style::attrs(false, false, true));
    put(s, "ui.selection.text", Style::attrs(true, false, true));
    put(s, "state.ok", Style::fg("green"));
    put(s, "state.warn", Style::fg("yellow"));
    put(s, "state.crit", Style::fg("red").bold());
    put(s, "state.info", Style::fg("default"));
    put(s, "state.stale", Style::attrs(false, true, false));
    put(s, "mem.app", Style::fg("default"));
    // Category tokens never reuse the state colors (green/yellow/red mean ok/warn/crit only, UX §8): the bar
    // glyphs tell segments apart, attributes tell text apart.
    put(s, "mem.gpu", Style::fg("default").bold());
    put(s, "mem.compressed", Style::fg("bright-black"));
    // default fg (not ANSI white/black, which vanish on light/dark terminals respectively), dim + italic
    // so it stays distinct from `mem.cache` (dim) in 16-color and NO_COLOR alike
    put(
        s,
        "mem.wired",
        Style {
            fg: Some("default".into()),
            dim: true,
            italic: true,
            ..Default::default()
        },
    );
    put(s, "mem.cache", Style::attrs(false, true, false));
    put(s, "mem.free", Style::fg("default"));
    put(s, "mem.swap", Style::default().italic());
    put(s, "kind.agent", Style::attrs(true, false, false));
    put(s, "kind.model", Style::default().bold().italic());
    put(s, "kind.sandbox", Style::default().italic());
    put(s, "kind.daemon", Style::attrs(false, true, false));
    put(s, "kind.app", Style::default());
    put(s, "kind.system", Style::attrs(false, true, false));
    put(s, "kind.other", Style::default());
    put(s, "chart.spark", Style::fg("default"));
    put(s, "chart.spark.peak", Style::attrs(true, false, false));
    put(s, "chart.bar.fill", Style::fg("default"));
    put(s, "chart.bar.track", Style::fg("bright-black"));
    put(s, "text.number", Style::default());
    put(s, "text.unit", Style::attrs(false, true, false));
    put(s, "text.label", Style::attrs(false, true, false));
    put(s, "text.key", Style::attrs(true, false, false));
    put(s, "text.link", Style::default().underline());
    put(s, "text.headline", Style::attrs(true, false, false));
    Theme {
        name: "terminal".into(),
        inherits: None,
        appearance: None,
        tokens: t,
    }
}

/// Monochrome: attributes only.
pub fn none_theme() -> Theme {
    let mut t = terminal_theme();
    t.name = "none".into();
    for s in t.tokens.values_mut() {
        s.fg = None;
        s.bg = None;
    }
    t
}

/// Neutral base of a truecolor theme.
struct Neutral {
    surface: &'static str,
    text: &'static str,
    app: &'static str,
    muted: &'static str,
    faint: &'static str,
    border: &'static str,
    selection: &'static str,
    compressed: &'static str,
    wired: &'static str,
    ok: &'static str,
    warn: &'static str,
    crit: &'static str,
}

const DARK: Neutral = Neutral {
    surface: "#141416",
    text: "#e8e8ea",
    app: "#cfcfd3",
    muted: "#a3a3a8",
    faint: "#707076",
    border: "#34343a",
    selection: "#2b2b30",
    compressed: "#8c8c93",
    wired: "#74747a",
    ok: "#6cc47a",
    warn: "#e6b450",
    crit: "#ff6a5c",
};

const LIGHT: Neutral = Neutral {
    surface: "#fbfbfa",
    text: "#1b1b1d",
    app: "#2c2c30",
    muted: "#55555b",
    faint: "#8a8a90",
    border: "#d4d4d8",
    selection: "#e6e6e2",
    compressed: "#66666c",
    wired: "#7c7c82",
    ok: "#2f7d3b",
    warn: "#8f6200",
    crit: "#c4321f",
};

/// (accent, gpu/model color) for dark and light.
fn accent_of(name: &str) -> Option<[(&'static str, &'static str); 2]> {
    Some(match name {
        "mono" => [("#ffffff", "#cfc6b8"), ("#000000", "#6b6255")],
        "ember" => [("#ffb547", "#ff8a6b"), ("#a35200", "#b0452c")],
        "mint" => [("#5fd7a7", "#9fd8c4"), ("#0a7350", "#2e6b58")],
        "sand" => [("#d8c09a", "#e8d8bd"), ("#7a5a2a", "#5e4a2a")],
        "coral" => [("#ff7f6b", "#ffb3a6"), ("#b8412f", "#8f3322")],
        _ => return None,
    })
}

fn truecolor(name: &str, variant: Variant, n: &Neutral, accent: &str, gpu: &str) -> Theme {
    let mut t = BTreeMap::new();
    let s = &mut t;
    put(s, "ui.surface", Style::bg(n.surface));
    put(s, "ui.text", Style::fg(n.text));
    put(s, "ui.muted", Style::fg(n.muted));
    put(s, "ui.faint", Style::fg(n.faint));
    put(s, "ui.border", Style::fg(n.border));
    put(s, "ui.border.focus", Style::fg(accent));
    put(s, "ui.accent", Style::fg(accent).bold());
    put(s, "ui.accent.text", Style::fg(n.surface).with_bg(accent).bold());
    put(s, "ui.selection", Style::bg(n.selection));
    put(
        s,
        "ui.selection.text",
        Style::fg(n.text).with_bg(n.selection).bold(),
    );
    put(s, "state.ok", Style::fg(n.ok));
    put(s, "state.warn", Style::fg(n.warn));
    put(s, "state.crit", Style::fg(n.crit).bold());
    put(s, "state.info", Style::fg(n.text));
    put(s, "state.stale", Style::fg(n.faint));
    put(s, "mem.app", Style::fg(n.app));
    put(s, "mem.gpu", Style::fg(gpu));
    put(s, "mem.compressed", Style::fg(n.compressed));
    put(s, "mem.wired", Style::fg(n.wired));
    put(s, "mem.cache", Style::fg(n.faint));
    put(s, "mem.free", Style::fg(n.ok));
    put(s, "mem.swap", Style::fg(n.crit));
    put(s, "kind.agent", Style::fg(accent).bold());
    put(s, "kind.model", Style::fg(gpu));
    put(s, "kind.sandbox", Style::fg(n.muted).italic());
    put(s, "kind.daemon", Style::fg(n.muted));
    put(s, "kind.app", Style::fg(n.text));
    put(s, "kind.system", Style::fg(n.faint));
    put(s, "kind.other", Style::fg(n.muted));
    put(s, "chart.spark", Style::fg(n.muted));
    put(s, "chart.spark.peak", Style::fg(accent).bold());
    put(s, "chart.bar.fill", Style::fg(n.text));
    put(s, "chart.bar.track", Style::fg(n.border));
    put(s, "text.number", Style::fg(n.text));
    put(s, "text.unit", Style::fg(n.muted));
    put(s, "text.label", Style::fg(n.muted));
    put(s, "text.key", Style::fg(accent).bold());
    put(s, "text.link", Style::fg(accent).underline());
    put(s, "text.headline", Style::fg(n.text).bold());
    Theme {
        name: name.to_string(),
        inherits: None,
        appearance: Some(variant.as_str().into()),
        tokens: t,
    }
}

fn high_contrast(name: &str, v: Variant) -> Theme {
    // ≥ 7:1 everywhere, bold borders, no dim text.
    let (n, accent, gpu) = match v {
        Variant::Dark => (
            Neutral {
                surface: "#000000",
                text: "#ffffff",
                app: "#ffffff",
                muted: "#e6e6e6",
                faint: "#c8c8c8",
                border: "#ffffff",
                selection: "#333333",
                compressed: "#d0d0d0",
                wired: "#bdbdbd",
                ok: "#5ce65c",
                warn: "#ffd23f",
                crit: "#ff8c8c",
            },
            "#ffd23f",
            "#ffb000",
        ),
        Variant::Light => (
            Neutral {
                surface: "#ffffff",
                text: "#000000",
                app: "#000000",
                muted: "#1f1f1f",
                faint: "#333333",
                border: "#000000",
                selection: "#d6d6d6",
                compressed: "#2b2b2b",
                wired: "#3a3a3a",
                ok: "#005c00",
                warn: "#6a4a00",
                crit: "#9e0000",
            },
            "#6a3d00",
            "#7a4500",
        ),
    };
    let mut t = truecolor(name, v, &n, accent, gpu);
    for k in ["ui.border", "ui.border.focus", "chart.bar.track"] {
        if let Some(s) = t.tokens.get_mut(k) {
            s.bold = true;
        }
    }
    for s in t.tokens.values_mut() {
        s.dim = false;
    }
    t
}

fn colorblind(name: &str, v: Variant) -> Theme {
    // Okabe–Ito-derived ramp without blue/purple: bluish green / orange / vermillion, which stay distinct
    // under deuteranopia and protanopia; state is also carried by glyphs (●▲■) in every theme.
    let (mut n, accent, gpu) = match v {
        Variant::Dark => (DARK, "#f0e442", "#f5c26b"),
        Variant::Light => (LIGHT, "#6b5f00", "#7a4e00"),
    };
    match v {
        Variant::Dark => {
            n.ok = "#1fb58f";
            n.warn = "#e69f00";
            n.crit = "#ff6a2a";
        }
        Variant::Light => {
            n.ok = "#00795a";
            n.warn = "#8a5a00";
            n.crit = "#b34200";
        }
    }
    truecolor(name, v, &n, accent, gpu)
}

/// Splits `mono-light` into (`mono`, Some(Light)).
fn split_variant(name: &str) -> (&str, Option<Variant>) {
    for (suffix, v) in [("-dark", Variant::Dark), ("-light", Variant::Light)] {
        if let Some(base) = name.strip_suffix(suffix) {
            if TRUECOLOR_THEMES.contains(&base) {
                return (base, Some(v));
            }
        }
    }
    (name, None)
}

/// True for built-in names, including `-dark` / `-light` variants.
pub fn is_builtin_name(name: &str) -> bool {
    let (base, _) = split_variant(name);
    BUILTIN_THEMES.contains(&base)
}

/// A built-in theme in the requested variant (ignored for `terminal`/`none`, and when the name pins one).
pub fn builtin_variant(name: &str, variant: Variant) -> Option<Theme> {
    let (base, pinned) = split_variant(name);
    let v = pinned.unwrap_or(variant);
    let mut t = match base {
        "terminal" => return (name == "terminal").then(terminal_theme),
        "none" => return (name == "none").then(none_theme),
        "high-contrast" => high_contrast(base, v),
        "colorblind" => colorblind(base, v),
        other => {
            let [dark, light] = accent_of(other)?;
            let (n, (a, g)) = match v {
                Variant::Dark => (&DARK, dark),
                Variant::Light => (&LIGHT, light),
            };
            truecolor(base, v, n, a, g)
        }
    };
    t.name = name.to_string();
    Some(t)
}

/// A built-in theme (truecolor themes in their dark variant unless the name pins `-light`).
pub fn builtin_theme(name: &str) -> Option<Theme> {
    builtin_variant(name, Variant::Dark)
}

/// Theme names: built-ins plus `themes/*.toml` in `user_dir`.
pub fn list_themes(user_dir: Option<&Path>) -> Vec<String> {
    let mut v: Vec<String> = BUILTIN_THEMES.iter().map(|s| s.to_string()).collect();
    if let Some(Ok(rd)) = user_dir.map(std::fs::read_dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "toml") {
                if let Some(stem) = p.file_stem() {
                    v.push(stem.to_string_lossy().into_owned());
                }
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

fn load_one(
    name: &str,
    user_dir: Option<&Path>,
    variant: Variant,
    allow_user: bool,
) -> Result<(Theme, bool), ThemeError> {
    if !valid_theme_name(name) {
        return Err(ThemeError::Parse(
            name.to_string(),
            "theme names are file names in themes/: letters, digits, '-', '_', '.' (no '/', no leading '.')"
                .into(),
        ));
    }
    if allow_user {
        if let Some(dir) = user_dir {
            let p = dir.join(format!("{name}.toml"));
            if let Ok(text) = std::fs::read_to_string(&p) {
                let mut t = parse_theme(&text, &p.display().to_string())?;
                t.name = name.to_string();
                return Ok((t, true));
            }
        }
    }
    builtin_variant(name, variant)
        .map(|t| (t, false))
        .ok_or_else(|| ThemeError::NotFound(name.to_string()))
}

/// Loads without resolving references or applying the contrast guard; returns the merged theme.
pub fn load_theme_unguarded(
    name: &str,
    user_dir: Option<&Path>,
    variant: Variant,
) -> Result<Theme, ThemeError> {
    let (first, first_user) = load_one(name, user_dir, variant, true)?;
    let mut v = first
        .appearance
        .as_deref()
        .and_then(Variant::parse)
        .unwrap_or(variant);
    let mut chain = vec![first];
    let mut seen = vec![(name.to_string(), first_user)];
    while let Some(parent) = chain.last().and_then(|t| t.inherits.clone()) {
        let child = chain.last().map(|t| t.name.clone()).unwrap_or_default();
        // `themes/mono.toml` with `inherits = "mono"` overrides the built-in of the same name
        let allow_user = parent != child;
        if chain.len() > 8 || seen.contains(&(parent.clone(), allow_user && user_has(user_dir, &parent))) {
            return Err(ThemeError::Cycle(parent));
        }
        let (t, is_user) = load_one(&parent, user_dir, v, allow_user)?;
        seen.push((parent.clone(), is_user));
        if let Some(pv) = t.appearance.as_deref().and_then(Variant::parse) {
            v = pv;
        }
        chain.push(t);
    }
    let mut merged = if chain.iter().any(|t| t.name == "none") {
        none_theme()
    } else {
        terminal_theme()
    };
    let top = chain[0].clone();
    for t in chain.iter().rev() {
        for (k, s) in &t.tokens {
            merged.tokens.insert(k.clone(), s.clone());
        }
    }
    merged.name = top.name;
    merged.inherits = top.inherits;
    merged.appearance = chain.iter().find_map(|t| t.appearance.clone());
    Ok(merged)
}

/// A theme name is a file stem under `themes/` — never a path (`../x`, `/etc/x`).
pub fn valid_theme_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

fn user_has(user_dir: Option<&Path>, name: &str) -> bool {
    user_dir.is_some_and(|d| d.join(format!("{name}.toml")).is_file())
}

/// Loads a theme by name (user themes override built-ins), resolving `inherits` and `@ref` colors, with every
/// missing token filled from `terminal`, and the contrast guard applied (dark variant by default).
pub fn load_theme(name: &str, user_dir: Option<&Path>) -> Result<Theme, ThemeError> {
    load_theme_for(name, user_dir, Variant::Dark)
}

/// [`load_theme`] for a light or dark terminal background.
pub fn load_theme_for(name: &str, user_dir: Option<&Path>, variant: Variant) -> Result<Theme, ThemeError> {
    let mut t = load_theme_unguarded(name, user_dir, variant)?;
    resolve_refs(&mut t);
    guard(&mut t);
    Ok(t)
}

/// `oomtop theme check <name>`: issues of the theme as written (after inheritance and references), plus the
/// guarded theme that oomtop will actually use.
pub fn check_named(
    name: &str,
    user_dir: Option<&Path>,
    variant: Variant,
) -> Result<(Theme, Vec<ThemeIssue>), ThemeError> {
    let raw = load_theme_unguarded(name, user_dir, variant)?;
    // a user file named like a built-in (`themes/mono.toml`) is the user's theme: the no-blue rule is ours
    let builtin = is_builtin_name(name) && !user_has(user_dir, name);
    let mut issues = check_structure(&raw, builtin);
    let mut t = raw;
    resolve_refs(&mut t);
    issues.extend(check_contrast(&t));
    guard(&mut t);
    Ok((t, issues))
}

/// Resolves `@token` references (fg refs read the target's fg, bg refs its bg; chains up to 8 deep).
/// Unresolvable references become `None` (terminal default).
pub fn resolve_refs(t: &mut Theme) {
    let snapshot = t.tokens.clone();
    let resolve = |c: &Option<String>, want_bg: bool| -> Option<String> {
        let mut cur = c.clone();
        for _ in 0..8 {
            match cur.as_deref() {
                Some(r) if r.starts_with('@') => {
                    let target = snapshot.get(&r[1..]);
                    cur = target.and_then(|s| if want_bg { s.bg.clone() } else { s.fg.clone() });
                }
                _ => return cur,
            }
        }
        None
    };
    for s in t.tokens.values_mut() {
        s.fg = resolve(&s.fg, false);
        s.bg = resolve(&s.bg, true);
    }
}

/// Checks a theme: unknown tokens, invalid colors and references, painted backgrounds in `terminal`,
/// blue/purple in built-ins, and WCAG contrast for hex colors (text ≥ 4.5:1, state ≥ 3:1).
pub fn check(t: &Theme) -> Vec<ThemeIssue> {
    let mut issues = check_structure(t, is_builtin_name(&t.name));
    let mut resolved = t.clone();
    resolve_refs(&mut resolved);
    issues.extend(check_contrast(&resolved));
    issues
}

/// The file-level part of [`check`] (what `oomtop config validate` reports for `themes/*.toml`): unknown
/// tokens, invalid colors and references, and — for shipped themes only (`builtin`) — blue/purple and a
/// painted background in `terminal`. Contrast is left out: it is auto-nudged at load and reported by
/// `oomtop theme check` (UX §12.4).
pub fn check_structure(t: &Theme, builtin: bool) -> Vec<ThemeIssue> {
    let mut issues = Vec::new();
    for (k, s) in &t.tokens {
        if !TOKENS.contains(&k.as_str()) {
            issues.push(ThemeIssue {
                token: k.clone(),
                message: match crate::layered::closest(k, TOKENS.iter().copied()) {
                    Some(c) => format!("unknown token — did you mean {c}?"),
                    None => "unknown token".into(),
                },
            });
        }
        for c in [&s.fg, &s.bg].into_iter().flatten() {
            if !valid_color(c) {
                issues.push(ThemeIssue {
                    token: k.clone(),
                    message: if c.starts_with('@') {
                        format!("reference {c:?} names no token")
                    } else {
                        format!("invalid color {c:?} (ANSI name/index, \"default\", #rrggbb or @token)")
                    },
                });
            }
            if builtin && is_blue_or_purple(c) {
                issues.push(ThemeIssue {
                    token: k.clone(),
                    message: "blue/purple is not allowed in built-in themes".into(),
                });
            }
        }
        if builtin && t.name == "terminal" && s.bg.is_some() {
            issues.push(ThemeIssue {
                token: k.clone(),
                message: "the terminal theme never paints a background".into(),
            });
        }
    }
    issues
}

/// Background hex a token is drawn on: its own bg, else the surface.
fn bg_of(t: &Theme, token: &str) -> Option<(u8, u8, u8)> {
    let own = t
        .tokens
        .get(token)
        .and_then(|s| s.bg.as_deref())
        .and_then(hex_rgb);
    own.or_else(|| {
        t.tokens
            .get("ui.surface")
            .and_then(|s| s.bg.as_deref())
            .and_then(hex_rgb)
    })
}

fn contrast_pairs() -> impl Iterator<Item = (&'static str, f64)> {
    TEXT_TOKENS
        .iter()
        .map(|t| (*t, TEXT_CONTRAST))
        .chain(STATE_TOKENS.iter().map(|t| (*t, STATE_CONTRAST)))
}

fn check_contrast(t: &Theme) -> Vec<ThemeIssue> {
    let mut out = Vec::new();
    for (k, min) in contrast_pairs() {
        let Some(bg) = bg_of(t, k) else { continue };
        let Some(fg) = t.tokens.get(k).and_then(|s| s.fg.as_deref()).and_then(hex_rgb) else {
            continue;
        };
        let r = contrast(fg, bg);
        if r + 1e-9 < min {
            out.push(ThemeIssue {
                token: k.into(),
                message: format!("contrast {r:.1}:1 < {min}:1"),
            });
        }
    }
    out
}

/// Contrast guard: nudges failing hex foregrounds in lightness (hue kept) until they meet the minimum.
/// Returns what was changed. Resolve references first.
pub fn guard(t: &mut Theme) -> Vec<ThemeIssue> {
    let mut changes = Vec::new();
    for (k, min) in contrast_pairs() {
        let Some(bg) = bg_of(t, k) else { continue };
        let Some(fg_hex) = t.tokens.get(k).and_then(|s| s.fg.clone()) else {
            continue;
        };
        let Some(fg) = hex_rgb(&fg_hex) else { continue };
        let before = contrast(fg, bg);
        if before + 1e-9 >= min {
            continue;
        }
        let fixed = nudge(fg, bg, min);
        let hex = rgb_hex(fixed);
        if let Some(s) = t.tokens.get_mut(k) {
            s.fg = Some(hex.clone());
        }
        changes.push(ThemeIssue {
            token: k.into(),
            message: format!(
                "contrast {before:.1}:1 < {min}:1 — nudged {fg_hex} → {hex} ({:.1}:1)",
                contrast(fixed, bg)
            ),
        });
    }
    changes
}

/// Moves `fg` away from `bg` in HSL lightness until `contrast ≥ min` (falls back to black/white).
pub fn nudge(fg: (u8, u8, u8), bg: (u8, u8, u8), min: f64) -> (u8, u8, u8) {
    let (h, s, l) = rgb_to_hsl(fg);
    let lighter = luminance(bg) < 0.18;
    let mut lum = l;
    for _ in 0..100 {
        lum = if lighter {
            (lum + 0.01).min(1.0)
        } else {
            (lum - 0.01).max(0.0)
        };
        let c = hsl_to_rgb(h, s, lum);
        if contrast(c, bg) >= min {
            return c;
        }
    }
    if lighter {
        (255, 255, 255)
    } else {
        (0, 0, 0)
    }
}

pub fn hex_rgb(c: &str) -> Option<(u8, u8, u8)> {
    let h = c.strip_prefix('#')?;
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some((p(0)?, p(2)?, p(4)?))
}

pub fn rgb_hex((r, g, b): (u8, u8, u8)) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// WCAG relative luminance.
pub fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let ch = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
}

/// WCAG contrast ratio.
pub fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// RGB → (hue °, saturation 0..1, lightness 0..1).
pub fn rgb_to_hsl((r, g, b): (u8, u8, u8)) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let d = max - min;
    if d.abs() < 1e-12 {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if (max - r).abs() < 1e-12 {
        ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < 1e-12 {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } * 60.0;
    (h, s, l)
}

pub fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = (h / 60.0).rem_euclid(6.0);
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let to = |v: f64| ((v + m).clamp(0.0, 1.0) * 255.0).round() as u8;
    (to(r1), to(g1), to(b1))
}

fn style_inline(s: &Style) -> String {
    let mut parts = Vec::new();
    if let Some(c) = &s.fg {
        parts.push(format!("fg = {}", toml::Value::String(c.clone())));
    }
    if let Some(c) = &s.bg {
        parts.push(format!("bg = {}", toml::Value::String(c.clone())));
    }
    for (on, name) in [
        (s.bold, "bold"),
        (s.dim, "dim"),
        (s.italic, "italic"),
        (s.underline, "underline"),
        (s.reverse, "reverse"),
    ] {
        if on {
            parts.push(format!("{name} = true"));
        }
    }
    if parts.is_empty() {
        "{}".into()
    } else {
        format!("{{ {} }}", parts.join(", "))
    }
}

/// Serializes a theme as a theme file (`oomtop theme export`, `theme import` output). Round-trips through
/// [`parse_theme`].
pub fn theme_to_toml(t: &Theme) -> String {
    let mut out = String::new();
    out.push_str(&format!("name = {}\n", toml::Value::String(t.name.clone())));
    if let Some(i) = &t.inherits {
        out.push_str(&format!("inherits = {}\n", toml::Value::String(i.clone())));
    }
    if let Some(a) = &t.appearance {
        out.push_str(&format!("appearance = {}\n", toml::Value::String(a.clone())));
    }
    let mut by_section: BTreeMap<&str, Vec<(&str, &Style)>> = BTreeMap::new();
    for (k, s) in &t.tokens {
        let (sec, rest) = k.split_once('.').unwrap_or(("ui", k.as_str()));
        by_section.entry(sec).or_default().push((rest, s));
    }
    let order: Vec<&str> = SECTIONS
        .iter()
        .copied()
        .chain(by_section.keys().copied().filter(|s| !SECTIONS.contains(s)))
        .collect();
    for sec in order {
        let Some(entries) = by_section.get(sec) else {
            continue;
        };
        out.push_str(&format!("\n[{sec}]\n"));
        let width = entries.iter().map(|(k, _)| key_repr(k).len()).max().unwrap_or(0);
        for (k, s) in entries {
            let key = key_repr(k);
            out.push_str(&format!("{key:<width$} = {}\n", style_inline(s)));
        }
    }
    out
}

fn key_repr(k: &str) -> String {
    if k.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        k.to_string()
    } else {
        format!("\"{k}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_builtin_variants() -> Vec<Theme> {
        let mut v = vec![terminal_theme(), none_theme()];
        for n in TRUECOLOR_THEMES {
            for var in [Variant::Dark, Variant::Light] {
                v.push(builtin_variant(n, var).unwrap());
            }
        }
        v
    }

    #[test]
    fn terminal_theme_is_clean() {
        let t = terminal_theme();
        assert_eq!(t.tokens.len(), TOKENS.len(), "every token defined");
        assert!(check(&t).is_empty(), "{:?}", check(&t));
        assert!(
            t.tokens.values().all(|s| s.bg.is_none()),
            "never paints a background"
        );
        assert!(
            t.tokens
                .values()
                .flat_map(|s| [&s.fg, &s.bg])
                .flatten()
                .all(|c| ANSI_NAMES.contains(&c.as_str())),
            "ANSI 16 + default only"
        );
        let n = none_theme();
        assert!(n.tokens.values().all(|s| s.fg.is_none() && s.bg.is_none()));
        assert!(check(&n).is_empty());
    }

    #[test]
    fn builtins_are_complete_clean_and_not_blue() {
        for t in all_builtin_variants() {
            assert_eq!(t.tokens.len(), TOKENS.len(), "{}: every token", t.name);
            let issues = check(&t);
            assert!(issues.is_empty(), "{} ({:?}): {issues:?}", t.name, t.appearance);
            let mut g = t.clone();
            assert!(guard(&mut g).is_empty(), "{} needs no nudging", t.name);
            for c in t.tokens.values().flat_map(|s| [&s.fg, &s.bg]).flatten() {
                assert!(!is_blue_or_purple(c), "{}: {c}", t.name);
            }
        }
        for n in TRUECOLOR_THEMES {
            let d = builtin_variant(n, Variant::Dark).unwrap();
            let l = builtin_variant(n, Variant::Light).unwrap();
            assert_ne!(
                d.tokens["ui.surface"], l.tokens["ui.surface"],
                "{n} has two variants"
            );
            assert_eq!(d.appearance.as_deref(), Some("dark"));
            assert_eq!(l.appearance.as_deref(), Some("light"));
            assert_eq!(builtin_theme(&format!("{n}-light")).unwrap().tokens, l.tokens);
        }
    }

    #[test]
    fn high_contrast_is_seven_to_one() {
        for v in [Variant::Dark, Variant::Light] {
            let t = builtin_variant("high-contrast", v).unwrap();
            let bg = hex_rgb(t.tokens["ui.surface"].bg.as_deref().unwrap()).unwrap();
            for (k, s) in &t.tokens {
                assert!(!s.dim, "{k}: no dim text");
                if let (Some(fg), None) = (s.fg.as_deref().and_then(hex_rgb), &s.bg) {
                    assert!(contrast(fg, bg) >= 7.0, "{k} {:?}: {:.2}", v, contrast(fg, bg));
                }
            }
            assert!(t.tokens["ui.border"].bold);
        }
    }

    #[test]
    fn terminal_theme_works_on_light_and_dark_terminals() {
        // UX §11 test 8: the terminal's own palette decides; ANSI white/black disappear on one of the two
        let t = terminal_theme();
        for (k, s) in &t.tokens {
            for c in [&s.fg, &s.bg].into_iter().flatten() {
                assert!(
                    !matches!(c.as_str(), "white" | "black" | "bright-white" | "7" | "0" | "15"),
                    "{k} uses {c}, invisible on a light or dark terminal"
                );
            }
        }
        // wired and cache stay distinguishable without color (NO_COLOR / none)
        let n = none_theme();
        assert_ne!(n.tokens["mem.wired"], n.tokens["mem.cache"]);
    }

    #[test]
    fn theme_names_are_never_paths() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("themes")).unwrap();
        std::fs::write(d.path().join("evil.toml"), "[ui]\naccent = \"#ffb547\"\n").unwrap();
        for bad in ["../evil", "/etc/passwd", ".hidden", "", "a/b"] {
            assert!(!valid_theme_name(bad), "{bad:?}");
            assert!(
                matches!(
                    load_theme(bad, Some(&d.path().join("themes"))),
                    Err(ThemeError::Parse(..))
                ),
                "{bad:?}"
            );
        }
        for good in ["night-ember", "mono-light", "tokyo_night", "base16.gruvbox"] {
            assert!(valid_theme_name(good), "{good}");
        }
    }

    #[test]
    fn user_override_of_builtin_name_may_use_blue_and_check_has_no_duplicates() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("mono.toml"),
            "inherits = \"mono\"\n[ui]\naccent = \"#3b82f6\"\ntext = \"#303030\"\n",
        )
        .unwrap();
        let (_, issues) = check_named("mono", Some(d.path()), Variant::Dark).unwrap();
        assert!(
            !issues.iter().any(|i| i.message.contains("blue/purple")),
            "{issues:?}"
        );
        let contrast: Vec<_> = issues.iter().filter(|i| i.token == "ui.text").collect();
        assert_eq!(contrast.len(), 1, "reported once: {issues:?}");
        // the shipped theme under that name would be flagged
        let mut shipped = builtin_theme("mono").unwrap();
        shipped.tokens.get_mut("ui.accent").unwrap().fg = Some("#3b82f6".into());
        assert!(check(&shipped).iter().any(|i| i.message.contains("blue/purple")));
        // structure-only checks skip contrast
        let raw = parse_theme(
            "[ui]\nsurface = { bg = \"#ffffff\" }\ntext = \"#fefefe\"\n",
            "x.toml",
        )
        .unwrap();
        assert!(check_structure(&raw, false).is_empty());
        assert!(!check(&raw).is_empty());
    }

    #[test]
    fn appearance_auto_fallback_chain() {
        use crate::model::AppearanceMode as M;
        assert_eq!(Variant::from_colorfgbg("15;0"), Some(Variant::Dark));
        assert_eq!(Variant::from_colorfgbg("0;15"), Some(Variant::Light));
        assert_eq!(Variant::from_colorfgbg("0;default;7"), Some(Variant::Light));
        assert_eq!(Variant::from_colorfgbg("7;8"), Some(Variant::Dark));
        assert_eq!(Variant::from_colorfgbg("0;255"), Some(Variant::Light));
        assert_eq!(Variant::from_colorfgbg("15;default"), None);
        assert_eq!(Variant::from_colorfgbg(""), None);
        // OSC 11 wins, then COLORFGBG, then dark
        assert_eq!(
            Variant::resolve(M::Auto, Some((250, 250, 245)), Some("15;0")),
            Variant::Light
        );
        assert_eq!(Variant::resolve(M::Auto, None, Some("0;15")), Variant::Light);
        assert_eq!(Variant::resolve(M::Auto, None, Some("junk")), Variant::Dark);
        assert_eq!(Variant::resolve(M::Auto, None, None), Variant::Dark);
        assert_eq!(
            Variant::resolve(M::Dark, Some((255, 255, 255)), Some("0;15")),
            Variant::Dark
        );
        assert_eq!(Variant::resolve(M::Light, None, None), Variant::Light);
    }

    #[test]
    fn xterm256_blue_detection() {
        for c in ["21", "57", "63", "93", "99", "27"] {
            assert!(is_blue_or_purple(c), "{c}");
        }
        for c in ["214", "208", "244", "16", "231", "2", "3", "46", "196"] {
            assert!(!is_blue_or_purple(c), "{c}");
        }
        assert_eq!(xterm256_rgb(21), Some((0, 0, 255)));
        assert_eq!(xterm256_rgb(255), Some((238, 238, 238)));
        assert_eq!(xterm256_rgb(7), None);
    }

    #[test]
    fn blue_detection() {
        for c in ["blue", "13", "#3b82f6", "#7c3aed", "#a855f7", "#5b5bd6"] {
            assert!(is_blue_or_purple(c), "{c}");
        }
        for c in [
            "yellow", "#ffb547", "#5fd7a7", "#1fb58f", "#808080", "#ff6a5c", "#ffffff",
        ] {
            assert!(!is_blue_or_purple(c), "{c}");
        }
        let mut t = builtin_theme("ember").unwrap();
        t.tokens.get_mut("ui.accent").unwrap().fg = Some("#3b82f6".into());
        assert!(check(&t).iter().any(|i| i.message.contains("blue/purple")));
    }

    #[test]
    fn user_theme_inherits_and_resolves() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("night.toml"),
            "name = \"night\"\ninherits = \"terminal\"\nappearance = \"dark\"\n[ui]\naccent = { fg = \"#ffb547\", bold = true }\nsurface = { bg = \"#101010\" }\n\"border.focus\" = { bold = true }\n[mem]\nswap = { fg = \"@state.warn\", underline = true }\n",
        )
        .unwrap();
        let t = load_theme("night", Some(d.path())).unwrap();
        assert_eq!(t.tokens["ui.accent"].fg.as_deref(), Some("#ffb547"));
        assert_eq!(t.tokens["mem.swap"].fg.as_deref(), Some("yellow"));
        assert!(t.tokens["mem.swap"].underline);
        assert!(t.tokens["ui.border.focus"].bold);
        assert_eq!(t.tokens.len(), TOKENS.len());
        assert!(list_themes(Some(d.path())).contains(&"night".to_string()));
        assert!(matches!(
            load_theme("nope", Some(d.path())),
            Err(ThemeError::NotFound(_))
        ));
    }

    #[test]
    fn ux_example_night_ember_inherits_mono_variant() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("night-ember.toml"),
            r##"name = "night-ember"
inherits = "mono"            # only override what differs
appearance = "dark"

[ui]
accent     = { fg = "#ffb547", bold = true }
selection  = { bg = "#2a2a2e" }
border     = { fg = "#2b2b30" }

[mem]
gpu        = { fg = "#ff8a6b" }
swap       = { fg = "@state.warn", underline = true }
"##,
        )
        .unwrap();
        // requested light, but the theme pins dark → inherits mono's dark variant
        let t = load_theme_for("night-ember", Some(d.path()), Variant::Light).unwrap();
        assert_eq!(t.appearance.as_deref(), Some("dark"));
        assert_eq!(t.tokens["ui.surface"].bg.as_deref(), Some("#141416"));
        assert_eq!(t.tokens["mem.swap"].fg.as_deref(), Some("#e6b450"));
        assert_eq!(t.tokens["ui.selection"].bg.as_deref(), Some("#2a2a2e"));
        let (_, issues) = check_named("night-ember", Some(d.path()), Variant::Dark).unwrap();
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn user_override_of_builtin_name_and_cycles() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("mono.toml"),
            "inherits = \"mono\"\n[ui]\naccent = \"#ffb547\"\n",
        )
        .unwrap();
        let t = load_theme("mono", Some(d.path())).unwrap();
        assert_eq!(t.tokens["ui.accent"].fg.as_deref(), Some("#ffb547"));
        assert_eq!(t.tokens["ui.surface"].bg.as_deref(), Some("#141416"));
        std::fs::write(d.path().join("a.toml"), "inherits = \"b\"\n").unwrap();
        std::fs::write(d.path().join("b.toml"), "inherits = \"a\"\n").unwrap();
        assert!(matches!(
            load_theme("a", Some(d.path())),
            Err(ThemeError::Cycle(_))
        ));
    }

    #[test]
    fn contrast_guard_nudges_and_reports() {
        assert!((contrast((0, 0, 0), (255, 255, 255)) - 21.0).abs() < 0.01);
        let mut t = terminal_theme();
        t.name = "x".into();
        t.tokens.get_mut("ui.surface").unwrap().bg = Some("#ffffff".into());
        t.tokens.get_mut("ui.text").unwrap().fg = Some("#eeeeee".into());
        t.tokens.get_mut("state.warn").unwrap().fg = Some("#ffee88".into());
        assert!(check(&t).iter().any(|i| i.token == "ui.text"));
        let changes = guard(&mut t);
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert!(check(&t).is_empty(), "{:?}", check(&t));
        let warn = hex_rgb(t.tokens["state.warn"].fg.as_deref().unwrap()).unwrap();
        let (h0, _, _) = rgb_to_hsl((0xff, 0xee, 0x88));
        let (h1, _, _) = rgb_to_hsl(warn);
        assert!((h0 - h1).abs() < 3.0, "hue kept: {h0} vs {h1}");
        // ANSI-only themes have nothing to check
        let mut term = terminal_theme();
        assert!(guard(&mut term).is_empty());
    }

    #[test]
    fn check_flags_bad_tokens_and_refs() {
        let t = parse_theme(
            "[ui]\naccnt = \"#ffb547\"\ntext = \"@ui.nope\"\n[state]\nok = \"chartreuse\"\n",
            "x.toml",
        )
        .unwrap();
        let issues = check(&t);
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("did you mean ui.accent?")),
            "{issues:?}"
        );
        assert!(issues.iter().any(|i| i.message.contains("names no token")));
        assert!(issues.iter().any(|i| i.message.contains("invalid color")));
        let e = parse_theme("[ui\n", "bad.toml").unwrap_err();
        assert!(e.to_string().starts_with("bad.toml:1"), "{e}");
        assert!(parse_theme("appearance = \"dim\"\n", "x.toml").is_err());
    }

    #[test]
    fn toml_roundtrip() {
        for t in all_builtin_variants() {
            let text = theme_to_toml(&t);
            let back = parse_theme(&text, "x.toml").unwrap();
            assert_eq!(back.tokens, t.tokens, "{text}");
            assert_eq!(back.appearance, t.appearance);
        }
        insta::assert_snapshot!("ember_dark", theme_to_toml(&builtin_theme("ember").unwrap()));
    }

    #[test]
    fn hsl_roundtrip() {
        for c in [
            (255, 181, 71),
            (20, 20, 22),
            (95, 215, 167),
            (0, 0, 0),
            (255, 255, 255),
        ] {
            let (h, s, l) = rgb_to_hsl(c);
            let back = hsl_to_rgb(h, s, l);
            let d = |a: u8, b: u8| (a as i32 - b as i32).abs();
            assert!(
                d(c.0, back.0) <= 1 && d(c.1, back.1) <= 1 && d(c.2, back.2) <= 1,
                "{c:?} {back:?}"
            );
        }
    }
}

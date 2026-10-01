//! `oomtop theme import` (UX §12.4): converts terminal color schemes into oomtop themes, mapping semantic
//! tokens automatically, then applies the contrast guard so the result passes `oomtop theme check`
//! (UX §11 test 11).
//!
//! Supported formats (detected from the extension, then the content):
//! - **base16 / base24** YAML — legacy (`scheme:` + `base00: "282c34"`) and tinted-theming
//!   (`system:`, `name:`, `variant:`, `palette: { base00: "#282c34" }`)
//! - **iTerm2** `.itermcolors` (XML property list)
//! - **Ghostty** theme files (`palette = 0=#1d1f21`, `background = …`)
//! - **Kitty** `.conf` themes (`color0 #1d1f21`, `background …`)
//! - **Alacritty** TOML (`[colors.primary]`, `[colors.normal]`, `[colors.bright]`)
//!
//! Parsing is purely textual (no YAML/plist dependency): only the color keys above are read.

use crate::theme::{guard, hex_rgb, is_blue_or_purple, rgb_hex, Style, Theme, ThemeIssue, Variant};
use std::collections::BTreeMap;
use std::path::Path;
use thiserror::Error;

pub type Rgb = (u8, u8, u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportFormat {
    Base16,
    Iterm2,
    Ghostty,
    Kitty,
    Alacritty,
}

impl ImportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            ImportFormat::Base16 => "base16/base24",
            ImportFormat::Iterm2 => "iTerm2",
            ImportFormat::Ghostty => "Ghostty",
            ImportFormat::Kitty => "Kitty",
            ImportFormat::Alacritty => "Alacritty",
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ImportError {
    #[error("{0}: {1}")]
    Io(String, String),
    #[error("could not recognize the color scheme format of {0} (base16/base24 YAML, .itermcolors, Ghostty, Kitty or Alacritty)")]
    UnknownFormat(String),
    #[error("{0}: missing {1}")]
    Missing(String, String),
}

/// Colors read from a scheme.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ImportedPalette {
    pub name: String,
    pub background: Option<Rgb>,
    pub foreground: Option<Rgb>,
    /// ANSI 0–15.
    pub ansi: [Option<Rgb>; 16],
    pub selection_bg: Option<Rgb>,
    pub selection_fg: Option<Rgb>,
    /// base16 `base00`..`base0F` (+ base24 `base10`..`base17`), when the source is base16/base24.
    pub base: BTreeMap<String, Rgb>,
    pub variant: Option<Variant>,
}

/// Parses a color like `#1d1f21`, `1d1f21`, `0x1d1f21`, `'#1d1f21'`.
pub fn parse_color(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_matches(|c| c == '"' || c == '\'');
    let hex = s
        .strip_prefix('#')
        .or_else(|| s.strip_prefix("0x"))
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if hex.len() == 3 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let d: Vec<u8> = hex
            .chars()
            .filter_map(|c| c.to_digit(16).map(|v| (v * 17) as u8))
            .collect();
        return Some((d[0], d[1], d[2]));
    }
    hex_rgb(&format!("#{}", hex.get(..6)?)).filter(|_| hex.len() == 6)
}

fn strip_comment(line: &str) -> &str {
    // '#' starts a comment only when it is not the start of a color value
    let mut in_q = None;
    for (i, c) in line.char_indices() {
        match (c, in_q) {
            ('"' | '\'', None) => in_q = Some(c),
            (q, Some(open)) if q == open => in_q = None,
            ('#', None) => {
                let rest = &line[i + 1..];
                let is_color = rest.len() >= 3 && rest.chars().take(3).all(|c| c.is_ascii_hexdigit());
                let after_sep = line[..i].trim_end().ends_with([':', '=']) || line[..i].ends_with(' ');
                if !(is_color && after_sep) {
                    return &line[..i];
                }
            }
            _ => {}
        }
    }
    line
}

/// base16/base24 YAML (legacy and tinted-theming formats).
pub fn parse_base16(text: &str) -> Option<ImportedPalette> {
    let mut p = ImportedPalette::default();
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (
            k.trim().trim_matches('"'),
            v.trim().trim_matches(|c| c == '"' || c == '\''),
        );
        match k {
            "scheme" | "name" if !v.is_empty() => p.name = v.to_string(),
            "variant" => p.variant = Variant::parse(v),
            _ if k.len() == 6 && k.starts_with("base") => {
                if let Some(c) = parse_color(v) {
                    p.base.insert(k.to_ascii_lowercase(), c);
                }
            }
            _ => {}
        }
    }
    let b = |k: &str| p.base.get(k).copied();
    if b("base00").is_none() || b("base05").is_none() || p.base.len() < 16 {
        return None;
    }
    p.background = b("base00");
    p.foreground = b("base05");
    p.selection_bg = b("base02");
    p.selection_fg = b("base05");
    let base24 = p.base.len() >= 24;
    // tinted-theming ANSI mapping for base16 / base24
    let map: [&str; 16] = [
        "base00",
        "base08",
        "base0b",
        "base0a",
        "base0d",
        "base0e",
        "base0c",
        "base05",
        "base03",
        if base24 { "base12" } else { "base08" },
        if base24 { "base14" } else { "base0b" },
        if base24 { "base13" } else { "base0a" },
        if base24 { "base16" } else { "base0d" },
        if base24 { "base17" } else { "base0e" },
        if base24 { "base15" } else { "base0c" },
        "base07",
    ];
    for (i, k) in map.iter().enumerate() {
        p.ansi[i] = b(k);
    }
    Some(p)
}

fn lower_keys(p: &mut ImportedPalette) {
    let base = std::mem::take(&mut p.base);
    p.base = base
        .into_iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v))
        .collect();
}

/// iTerm2 `.itermcolors` (plist with `Ansi N Color` / `Background Color` dicts of float components).
pub fn parse_iterm2(text: &str) -> Option<ImportedPalette> {
    if !text.contains("<plist") && !text.contains("Ansi 0 Color") {
        return None;
    }
    let mut p = ImportedPalette::default();
    let mut rest = text;
    while let Some(i) = rest.find("<key>") {
        rest = &rest[i + 5..];
        let Some(end) = rest.find("</key>") else { break };
        let name = rest[..end].trim().to_string();
        rest = &rest[end + 6..];
        let after = rest.trim_start();
        if !after.starts_with("<dict>") {
            continue;
        }
        let Some(dend) = after.find("</dict>") else { break };
        let dict = &after[6..dend];
        let comp = |c: &str| -> Option<f64> {
            let k = format!("<key>{c} Component</key>");
            let at = dict.find(&k)? + k.len();
            let s = &dict[at..];
            let open = s.find('>')? + 1;
            let close = s.find("</")?;
            s.get(open..close)?.trim().parse::<f64>().ok()
        };
        if let (Some(r), Some(g), Some(b)) = (comp("Red"), comp("Green"), comp("Blue")) {
            let to = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            let c = (to(r), to(g), to(b));
            match name.as_str() {
                "Background Color" => p.background = Some(c),
                "Foreground Color" => p.foreground = Some(c),
                "Selection Color" => p.selection_bg = Some(c),
                "Selected Text Color" => p.selection_fg = Some(c),
                n => {
                    if let Some(idx) = n
                        .strip_prefix("Ansi ")
                        .and_then(|x| x.strip_suffix(" Color"))
                        .and_then(|x| x.parse::<usize>().ok())
                    {
                        if idx < 16 {
                            p.ansi[idx] = Some(c);
                        }
                    }
                }
            }
        }
        rest = &after[dend + 7..];
    }
    (p.background.is_some() || p.ansi.iter().any(Option::is_some)).then_some(p)
}

/// Ghostty (`key = value`, `palette = N=#rrggbb`).
pub fn parse_ghostty(text: &str) -> Option<ImportedPalette> {
    let mut p = ImportedPalette::default();
    let mut seen = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "palette" => {
                if let Some((i, c)) = v.split_once('=') {
                    if let (Ok(i), Some(c)) = (i.trim().parse::<usize>(), parse_color(c)) {
                        if i < 16 {
                            p.ansi[i] = Some(c);
                            seen = true;
                        }
                    }
                }
            }
            "background" => p.background = parse_color(v),
            "foreground" => p.foreground = parse_color(v),
            "selection-background" => p.selection_bg = parse_color(v),
            "selection-foreground" => p.selection_fg = parse_color(v),
            _ => {}
        }
    }
    (seen || p.background.is_some()).then_some(p)
}

/// Kitty (`key value`, `color0`..`color15`).
pub fn parse_kitty(text: &str) -> Option<ImportedPalette> {
    let mut p = ImportedPalette::default();
    let mut seen = false;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(name) = line.strip_prefix("## name:") {
            p.name = name.trim().to_string();
        }
        if line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(k), Some(v)) = (it.next(), it.next()) else {
            continue;
        };
        let c = parse_color(v);
        match k {
            "background" => p.background = c,
            "foreground" => p.foreground = c,
            "selection_background" => p.selection_bg = c,
            "selection_foreground" => p.selection_fg = c,
            k => {
                if let Some(i) = k.strip_prefix("color").and_then(|x| x.parse::<usize>().ok()) {
                    if i < 16 {
                        p.ansi[i] = c;
                        seen |= c.is_some();
                    }
                }
            }
        }
    }
    (seen || p.background.is_some()).then_some(p)
}

/// Alacritty TOML (`[colors.primary]`, `[colors.normal]`, `[colors.bright]`, `[colors.selection]`).
pub fn parse_alacritty(text: &str) -> Option<ImportedPalette> {
    let t: toml::Table = toml::from_str(text).ok()?;
    let colors = t.get("colors")?.as_table()?;
    let get = |sec: &str, key: &str| {
        colors
            .get(sec)
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
            .and_then(parse_color)
    };
    let mut p = ImportedPalette {
        background: get("primary", "background"),
        foreground: get("primary", "foreground"),
        selection_bg: get("selection", "background"),
        selection_fg: get("selection", "text"),
        ..Default::default()
    };
    let names = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    for (i, n) in names.iter().enumerate() {
        p.ansi[i] = get("normal", n);
        p.ansi[i + 8] = get("bright", n);
    }
    (p.background.is_some() || p.ansi.iter().any(Option::is_some)).then_some(p)
}

/// Detects the format from the file name, then the content.
pub fn detect_format(file_name: &str, text: &str) -> Option<ImportFormat> {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        return Some(ImportFormat::Base16);
    }
    if lower.ends_with(".itermcolors") || text.contains("<plist") {
        return Some(ImportFormat::Iterm2);
    }
    if (lower.ends_with(".toml") || text.contains("[colors")) && text.contains("colors") {
        return Some(ImportFormat::Alacritty);
    }
    if text.contains("base00") {
        return Some(ImportFormat::Base16);
    }
    if text
        .lines()
        .any(|l| l.trim_start().starts_with("palette") && l.contains('='))
    {
        return Some(ImportFormat::Ghostty);
    }
    if text
        .lines()
        .any(|l| l.trim_start().starts_with("color0 ") || l.trim_start().starts_with("color0\t"))
        || lower.ends_with(".conf")
    {
        return Some(ImportFormat::Kitty);
    }
    if text
        .lines()
        .any(|l| l.trim_start().starts_with("background") && l.contains('='))
    {
        return Some(ImportFormat::Ghostty);
    }
    None
}

/// Parses a scheme of a given (or detected) format.
pub fn parse_scheme(
    file_name: &str,
    text: &str,
    format: Option<ImportFormat>,
) -> Result<(ImportedPalette, ImportFormat), ImportError> {
    let fmt = format
        .or_else(|| detect_format(file_name, text))
        .ok_or_else(|| ImportError::UnknownFormat(file_name.into()))?;
    let parsed = match fmt {
        ImportFormat::Base16 => parse_base16(text).map(|mut p| {
            lower_keys(&mut p);
            p
        }),
        ImportFormat::Iterm2 => parse_iterm2(text),
        ImportFormat::Ghostty => parse_ghostty(text),
        ImportFormat::Kitty => parse_kitty(text),
        ImportFormat::Alacritty => parse_alacritty(text),
    };
    let mut p = parsed.ok_or_else(|| {
        ImportError::Missing(
            file_name.into(),
            match fmt {
                ImportFormat::Base16 => "base00–base0F".to_string(),
                _ => "background / palette colors".to_string(),
            },
        )
    })?;
    if p.name.is_empty() {
        p.name = Path::new(file_name)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "imported".into());
    }
    if p.background.is_none() {
        p.background = p.ansi[0];
    }
    if p.foreground.is_none() {
        p.foreground = p.ansi[7].or(p.ansi[15]);
    }
    if p.background.is_none() || p.foreground.is_none() {
        return Err(ImportError::Missing(
            file_name.into(),
            "background and foreground".into(),
        ));
    }
    Ok((p, fmt))
}

/// `"Tokyo Night Storm"` → `"tokyo-night-storm"`.
pub fn theme_slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() {
        "imported".into()
    } else {
        out
    }
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let m = |x: u8, y: u8| (x as f64 * (1.0 - t) + y as f64 * t).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// Maps a palette to semantic tokens (one warm accent — never blue/purple by default).
pub fn palette_to_theme(p: &ImportedPalette) -> Theme {
    let bg = p.background.unwrap_or((0, 0, 0));
    let fg = p.foreground.unwrap_or((255, 255, 255));
    let variant = p.variant.unwrap_or_else(|| Variant::for_background(bg));
    let base = |k: &str| p.base.get(k).copied();
    let ansi = |i: usize| p.ansi[i];
    let red = base("base08").or(ansi(1)).unwrap_or((0xd0, 0x40, 0x40));
    let green = base("base0b").or(ansi(2)).unwrap_or((0x40, 0xa0, 0x40));
    let yellow = base("base0a").or(ansi(3)).unwrap_or((0xc0, 0xa0, 0x30));
    let orange = base("base09").or(ansi(11)).unwrap_or(yellow);
    let cyan = base("base0c").or(ansi(6)).unwrap_or(green);
    let muted = base("base04").unwrap_or_else(|| mix(fg, bg, 0.35));
    let faint = base("base03").or(ansi(8)).unwrap_or_else(|| mix(fg, bg, 0.55));
    let border = base("base02").unwrap_or_else(|| mix(fg, bg, 0.75));
    let selection = p
        .selection_bg
        .or(base("base02"))
        .unwrap_or_else(|| mix(fg, bg, 0.8));
    let selection_fg = p.selection_fg.unwrap_or(fg);
    // one accent: the first warm, saturated color of the scheme
    let accent = [orange, yellow, red, green, cyan]
        .into_iter()
        .find(|c| !is_blue_or_purple(&rgb_hex(*c)) && crate::theme::rgb_to_hsl(*c).1 > 0.2)
        .unwrap_or(fg);
    let gpu = if accent == orange { yellow } else { orange };
    let h = rgb_hex;
    let mut t = BTreeMap::new();
    let mut put = |k: &str, s: Style| {
        t.insert(k.to_string(), s);
    };
    let fgs = |c: Rgb| Style::fg(&h(c));
    let bold = |mut s: Style| {
        s.bold = true;
        s
    };
    put("ui.surface", Style::bg(&h(bg)));
    put("ui.text", fgs(fg));
    put("ui.muted", fgs(muted));
    put("ui.faint", fgs(faint));
    put("ui.border", fgs(border));
    put("ui.border.focus", fgs(accent));
    put("ui.accent", bold(fgs(accent)));
    put(
        "ui.accent.text",
        Style {
            fg: Some(h(bg)),
            bg: Some(h(accent)),
            bold: true,
            ..Default::default()
        },
    );
    put("ui.selection", Style::bg(&h(selection)));
    put(
        "ui.selection.text",
        Style {
            fg: Some(h(selection_fg)),
            bg: Some(h(selection)),
            bold: true,
            ..Default::default()
        },
    );
    put("state.ok", fgs(green));
    put("state.warn", fgs(yellow));
    put("state.crit", bold(fgs(red)));
    put("state.info", fgs(fg));
    put("state.stale", fgs(faint));
    put("mem.app", fgs(fg));
    put("mem.gpu", fgs(gpu));
    put("mem.compressed", fgs(muted));
    put("mem.wired", fgs(faint));
    put("mem.cache", fgs(border));
    put("mem.free", fgs(green));
    put("mem.swap", fgs(red));
    put("kind.agent", bold(fgs(accent)));
    put("kind.model", fgs(gpu));
    put(
        "kind.sandbox",
        Style {
            fg: Some(h(cyan)),
            italic: true,
            ..Default::default()
        },
    );
    put("kind.daemon", fgs(muted));
    put("kind.app", fgs(fg));
    put("kind.system", fgs(faint));
    put("kind.other", fgs(muted));
    put("chart.spark", fgs(muted));
    put("chart.spark.peak", bold(fgs(accent)));
    put("chart.bar.fill", fgs(fg));
    put("chart.bar.track", fgs(border));
    put("text.number", fgs(fg));
    put("text.unit", fgs(muted));
    put("text.label", fgs(muted));
    put("text.key", bold(fgs(accent)));
    put(
        "text.link",
        Style {
            fg: Some(h(accent)),
            underline: true,
            ..Default::default()
        },
    );
    put("text.headline", bold(fgs(fg)));
    Theme {
        name: theme_slug(&p.name),
        inherits: None,
        appearance: Some(variant.as_str().into()),
        tokens: t,
    }
}

/// Result of an import: the theme (contrast-guarded) and what the guard changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub theme: Theme,
    pub format: ImportFormat,
    pub nudged: Vec<ThemeIssue>,
}

/// Imports scheme text.
pub fn import_text(
    file_name: &str,
    text: &str,
    format: Option<ImportFormat>,
) -> Result<Imported, ImportError> {
    let (p, format) = parse_scheme(file_name, text, format)?;
    let mut theme = palette_to_theme(&p);
    let nudged = guard(&mut theme);
    Ok(Imported {
        theme,
        format,
        nudged,
    })
}

/// Imports a scheme file.
pub fn import_file(path: &Path) -> Result<Imported, ImportError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ImportError::Io(path.display().to_string(), e.to_string()))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    import_text(&name, &text, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{check, parse_theme, theme_to_toml, TOKENS};

    pub(crate) const GRUVBOX_BASE16: &str = r##"# legacy base16 format
scheme: "Gruvbox dark, medium"
author: "Dawid Kurek (dawikur@gmail.com), morhetz (https://github.com/morhetz/gruvbox)"
base00: "282828" # ----
base01: "3c3836" # ---
base02: "504945" # --
base03: "665c54" # -
base04: "bdae93" # +
base05: "d5c4a1" # ++
base06: "ebdbb2" # +++
base07: "fbf1c7" # ++++
base08: "fb4934" # red
base09: "fe8019" # orange
base0A: "fabd2f" # yellow
base0B: "b8bb26" # green
base0C: "8ec07c" # aqua/cyan
base0D: "83a598" # blue
base0E: "d3869b" # purple
base0F: "d65d0e" # brown
"##;

    const TINTED_LIGHT: &str = r##"system: "base16"
name: "Solarized Light"
author: "Ethan Schoonover (modified by aramisgithub)"
variant: "light"
palette:
  base00: "#fdf6e3"
  base01: "#eee8d5"
  base02: "#93a1a1"
  base03: "#839496"
  base04: "#657b83"
  base05: "#586e75"
  base06: "#073642"
  base07: "#002b36"
  base08: "#dc322f"
  base09: "#cb4b16"
  base0A: "#b58900"
  base0B: "#859900"
  base0C: "#2aa198"
  base0D: "#268bd2"
  base0E: "#6c71c4"
  base0F: "#d33682"
"##;

    #[test]
    fn base16_import_passes_theme_check() {
        for (file, text) in [
            ("gruvbox.yaml", GRUVBOX_BASE16),
            ("solarized-light.yaml", TINTED_LIGHT),
        ] {
            let imp = import_text(file, text, None).unwrap();
            assert_eq!(imp.format, ImportFormat::Base16);
            assert_eq!(imp.theme.tokens.len(), TOKENS.len());
            let issues = check(&imp.theme);
            assert!(issues.is_empty(), "{file}: {issues:?}");
            // the file written to themes/ passes too
            let back = parse_theme(&theme_to_toml(&imp.theme), "x.toml").unwrap();
            assert!(check(&back).is_empty());
            assert!(!is_blue_or_purple(
                imp.theme.tokens["ui.accent"].fg.as_deref().unwrap()
            ));
        }
        let g = import_text("gruvbox.yaml", GRUVBOX_BASE16, None).unwrap().theme;
        assert_eq!(g.name, "gruvbox-dark-medium");
        assert_eq!(g.appearance.as_deref(), Some("dark"));
        assert_eq!(g.tokens["ui.accent"].fg.as_deref(), Some("#fe8019"));
        assert_eq!(g.tokens["ui.surface"].bg.as_deref(), Some("#282828"));
        let s = import_text("s.yaml", TINTED_LIGHT, None).unwrap();
        assert_eq!(s.theme.appearance.as_deref(), Some("light"));
        assert!(!s.nudged.is_empty(), "solarized light needs nudging for 4.5:1");
        insta::assert_snapshot!("import_gruvbox", theme_to_toml(&g));
    }

    #[test]
    fn base24_mapping() {
        let mut text = GRUVBOX_BASE16.to_string();
        for (i, c) in [
            "1d2021", "171919", "fb5944", "fd9029", "fbcd3f", "c8cb36", "9ed08c", "93b5a8",
        ]
        .iter()
        .enumerate()
        {
            text.push_str(&format!("base1{i}: \"{c}\"\n"));
        }
        let (p, _) = parse_scheme("x.yaml", &text, None).unwrap();
        assert_eq!(p.ansi[9], Some((0xfb, 0x59, 0x44)));
        assert_eq!(p.ansi[1], Some((0xfb, 0x49, 0x34)));
    }

    const ITERM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Ansi 0 Color</key>
	<dict>
		<key>Alpha Component</key><real>1</real>
		<key>Blue Component</key><real>0.0</real>
		<key>Color Space</key><string>sRGB</string>
		<key>Green Component</key><real>0.0</real>
		<key>Red Component</key><real>0.0</real>
	</dict>
	<key>Ansi 1 Color</key>
	<dict>
		<key>Blue Component</key><real>0.2</real>
		<key>Green Component</key><real>0.2</real>
		<key>Red Component</key><real>0.8</real>
	</dict>
	<key>Ansi 2 Color</key>
	<dict>
		<key>Blue Component</key><real>0.3</real>
		<key>Green Component</key><real>0.7</real>
		<key>Red Component</key><real>0.3</real>
	</dict>
	<key>Ansi 3 Color</key>
	<dict>
		<key>Blue Component</key><real>0.2</real>
		<key>Green Component</key><real>0.7</real>
		<key>Red Component</key><real>0.8</real>
	</dict>
	<key>Background Color</key>
	<dict>
		<key>Blue Component</key><real>0.1</real>
		<key>Green Component</key><real>0.1</real>
		<key>Red Component</key><real>0.1</real>
	</dict>
	<key>Foreground Color</key>
	<dict>
		<key>Blue Component</key><real>0.9</real>
		<key>Green Component</key><real>0.9</real>
		<key>Red Component</key><real>0.9</real>
	</dict>
</dict>
</plist>
"#;

    #[test]
    fn iterm_ghostty_kitty_alacritty() {
        let imp = import_text("Night.itermcolors", ITERM, None).unwrap();
        assert_eq!(imp.format, ImportFormat::Iterm2);
        assert_eq!(imp.theme.tokens["ui.surface"].bg.as_deref(), Some("#1a1a1a"));
        assert!(check(&imp.theme).is_empty(), "{:?}", check(&imp.theme));

        let ghostty = "# ghostty\npalette = 0=#1d1f21\npalette = 1=#cc6666\npalette = 2=#b5bd68\npalette = 3=#f0c674\npalette = 4=#81a2be\nbackground = 1d1f21\nforeground = c5c8c6\nselection-background = 373b41\n";
        let imp = import_text("tomorrow-night", ghostty, None).unwrap();
        assert_eq!(imp.format, ImportFormat::Ghostty);
        assert_eq!(imp.theme.tokens["ui.text"].fg.as_deref(), Some("#c5c8c6"));
        assert_eq!(imp.theme.tokens["ui.accent"].fg.as_deref(), Some("#f0c674"));
        assert!(check(&imp.theme).is_empty());

        let kitty = "## name: Tokyo Night\nforeground #c0caf5\nbackground #1a1b26\nselection_background #33467c\ncolor0 #15161e\ncolor1 #f7768e\ncolor2 #9ece6a\ncolor3 #e0af68\ncolor4 #7aa2f7\ncolor5 #bb9af7\n";
        let imp = import_text("tokyo_night.conf", kitty, None).unwrap();
        assert_eq!(imp.format, ImportFormat::Kitty);
        assert_eq!(imp.theme.name, "tokyo-night");
        assert!(check(&imp.theme).is_empty());
        assert!(!is_blue_or_purple(
            imp.theme.tokens["ui.accent"].fg.as_deref().unwrap()
        ));

        let alacritty = "[colors.primary]\nbackground = '#fafafa'\nforeground = '#383a42'\n[colors.normal]\nblack = '#383a42'\nred = '0xe45649'\ngreen = '#50a14f'\nyellow = '#c18401'\n[colors.bright]\nblack = '#a0a1a7'\n";
        let imp = import_text("one-light.toml", alacritty, None).unwrap();
        assert_eq!(imp.format, ImportFormat::Alacritty);
        assert_eq!(imp.theme.appearance.as_deref(), Some("light"));
        assert!(check(&imp.theme).is_empty(), "{:?}", check(&imp.theme));
    }

    #[test]
    fn errors_and_helpers() {
        assert!(matches!(
            import_text("x.txt", "hello", None),
            Err(ImportError::UnknownFormat(_))
        ));
        assert!(matches!(
            import_text("x.yaml", "scheme: x\nbase00: \"000000\"\n", None),
            Err(ImportError::Missing(..))
        ));
        assert_eq!(parse_color("0xFFB547"), Some((255, 181, 71)));
        assert_eq!(parse_color("'#fff'"), Some((255, 255, 255)));
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(theme_slug("Gruvbox dark, medium"), "gruvbox-dark-medium");
        assert_eq!(theme_slug("!!!"), "imported");
        assert_eq!(strip_comment("base00: \"282828\" # ----"), "base00: \"282828\" ");
        assert_eq!(strip_comment("base00: #282828 # c"), "base00: #282828 ");
    }
}

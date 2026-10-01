//! Semantic token → ratatui style (UX §12.3). Widgets never name raw colors; they ask for a token.
//! Degrades truecolor → 256 → 16 → monochrome (UX §12.1.5). With `ColorDepth::None` (NO_COLOR / `none` theme)
//! only attributes are emitted. The `terminal` theme never paints a background, whatever the config says.
//! Resolved styles are cached per token so the render hot path is a hash lookup.

use oomtop_config::theme::{Style as TokenStyle, Theme};
use ratatui::style::{Color, Modifier, Style};
use std::collections::HashMap;

/// Color depth actually used (from caps + config `appearance.color`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    Truecolor,
    Ansi256,
    Ansi16,
    None,
}

impl ColorDepth {
    pub fn as_str(self) -> &'static str {
        match self {
            ColorDepth::Truecolor => "truecolor",
            ColorDepth::Ansi256 => "256",
            ColorDepth::Ansi16 => "16",
            ColorDepth::None => "none",
        }
    }
}

/// Resolved theme ready for rendering.
#[derive(Debug, Clone)]
pub struct Palette {
    theme: Theme,
    depth: ColorDepth,
    /// Paint the theme surface (`appearance.background = "theme"`); the terminal theme never does.
    pub paint_background: bool,
    cache: HashMap<String, Style>,
}

pub fn parse_color(c: &str) -> Option<Color> {
    let named = match c {
        "default" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::Gray),
        "bright-black" => Some(Color::DarkGray),
        "bright-red" => Some(Color::LightRed),
        "bright-green" => Some(Color::LightGreen),
        "bright-yellow" => Some(Color::LightYellow),
        "bright-blue" => Some(Color::LightBlue),
        "bright-magenta" => Some(Color::LightMagenta),
        "bright-cyan" => Some(Color::LightCyan),
        "bright-white" => Some(Color::White),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    if let Some((r, g, b)) = oomtop_config::theme::hex_rgb(c) {
        return Some(Color::Rgb(r, g, b));
    }
    let i = c.parse::<u8>().ok()?;
    Some(match i {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::White,
        n => Color::Indexed(n),
    })
}

/// Nearest of the 16 ANSI colors for an RGB/indexed value (for 16-color terminals).
fn to_ansi16(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => {
            let bright = (r as u16 + g as u16 + b as u16) > 382;
            let (r, g, b) = (r > 127, g > 127, b > 127);
            match (r, g, b, bright) {
                (false, false, false, false) => Color::Black,
                (false, false, false, true) => Color::DarkGray,
                (true, false, false, _) => Color::Red,
                (false, true, false, _) => Color::Green,
                (true, true, false, _) => Color::Yellow,
                (false, false, true, _) => Color::Blue,
                (true, false, true, _) => Color::Magenta,
                (false, true, true, _) => Color::Cyan,
                (true, true, true, false) => Color::Gray,
                (true, true, true, true) => Color::White,
            }
        }
        Color::Indexed(i) if i >= 16 => {
            if (232..=255).contains(&i) {
                if i < 244 {
                    Color::DarkGray
                } else {
                    Color::Gray
                }
            } else {
                // 6×6×6 cube → rgb → nearest ANSI
                let n = i - 16;
                let lvl = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                to_ansi16(Color::Rgb(lvl(n / 36), lvl((n / 6) % 6), lvl(n % 6)))
            }
        }
        other => other,
    }
}

/// Nearest xterm-256 index for an RGB value.
fn to_ansi256(c: Color) -> Color {
    match c {
        Color::Rgb(r, g, b) => {
            let q = |v: u8| -> u8 {
                if v < 48 {
                    0
                } else if v < 115 {
                    1
                } else {
                    (v - 35) / 40
                }
            };
            let (qr, qg, qb) = (q(r), q(g), q(b));
            let cube = 16 + 36 * qr + 6 * qg + qb;
            // greyscale ramp when the color is (nearly) grey
            if r.abs_diff(g) < 10 && g.abs_diff(b) < 10 {
                let avg = (r as u16 + g as u16 + b as u16) / 3;
                if avg > 8 && avg < 238 {
                    return Color::Indexed(232 + ((avg - 8) / 10) as u8);
                }
            }
            Color::Indexed(cube)
        }
        other => other,
    }
}

impl Palette {
    pub fn new(theme: Theme, depth: ColorDepth, paint_background: bool) -> Self {
        let paint = paint_background && theme.name != "terminal";
        let mut p = Palette {
            theme,
            depth,
            paint_background: paint,
            cache: HashMap::new(),
        };
        let cache: HashMap<String, Style> = p
            .theme
            .tokens
            .iter()
            .map(|(k, t)| (k.clone(), p.convert(t)))
            .collect();
        p.cache = cache;
        p
    }

    pub fn theme_name(&self) -> &str {
        &self.theme.name
    }

    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    pub fn depth(&self) -> ColorDepth {
        self.depth
    }

    fn color(&self, c: &Option<String>) -> Option<Color> {
        let c = parse_color(c.as_deref()?)?;
        match self.depth {
            ColorDepth::None => None,
            ColorDepth::Ansi16 => Some(to_ansi16(c)),
            ColorDepth::Ansi256 => Some(to_ansi256(c)),
            ColorDepth::Truecolor => Some(c),
        }
    }

    /// Style for a semantic token (unknown tokens → unstyled).
    pub fn token(&self, name: &str) -> Style {
        self.cache.get(name).copied().unwrap_or_default()
    }

    /// Base style for the whole screen: the theme surface when painting is allowed, else nothing.
    pub fn surface(&self) -> Style {
        if self.paint_background {
            self.token("ui.surface")
        } else {
            Style::default()
        }
    }

    fn convert(&self, t: &TokenStyle) -> Style {
        let mut s = Style::default();
        if let Some(fg) = self.color(&t.fg) {
            s = s.fg(fg);
        }
        let mut bg_dropped = false;
        if t.bg.is_some() {
            match self.color(&t.bg).filter(|_| self.paint_background) {
                Some(bg) => s = s.bg(bg),
                None => bg_dropped = true,
            }
        }
        let mut m = Modifier::empty();
        // A token that is *only* a background (e.g. `ui.selection = { bg = "#2b2b30" }`) would vanish when the
        // background is not painted (transparent, NO_COLOR, `color = "none"`): fall back to reverse video so
        // the selection stays visible without painting anything.
        if bg_dropped && s.fg.is_none() {
            m |= Modifier::REVERSED;
        }
        if t.bold {
            m |= Modifier::BOLD;
        }
        if t.dim {
            m |= Modifier::DIM;
        }
        if t.italic {
            m |= Modifier::ITALIC;
        }
        if t.underline {
            m |= Modifier::UNDERLINED;
        }
        if t.reverse {
            m |= Modifier::REVERSED;
        }
        s.add_modifier(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_config::theme::terminal_theme;

    #[test]
    fn terminal_theme_never_paints_background() {
        let p = Palette::new(terminal_theme(), ColorDepth::Truecolor, true);
        for tok in oomtop_config::theme::TOKENS {
            assert_eq!(p.token(tok).bg, None, "{tok}");
        }
        assert_eq!(p.surface(), Style::default());
        assert_eq!(p.token("state.warn").fg, Some(Color::Yellow));
        let none = Palette::new(terminal_theme(), ColorDepth::None, false);
        assert_eq!(none.token("state.warn").fg, None);
        assert!(none
            .token("ui.selection")
            .add_modifier
            .contains(Modifier::REVERSED));
    }

    #[test]
    fn no_blue_or_purple_in_terminal_theme() {
        let p = Palette::new(terminal_theme(), ColorDepth::Ansi16, false);
        for tok in oomtop_config::theme::TOKENS {
            let fg = p.token(tok).fg;
            assert!(
                !matches!(
                    fg,
                    Some(Color::Blue | Color::Magenta | Color::LightBlue | Color::LightMagenta)
                ),
                "{tok}: {fg:?}"
            );
        }
    }

    #[test]
    fn colors_and_degradation() {
        assert_eq!(parse_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("208"), Some(Color::Indexed(208)));
        assert_eq!(parse_color("3"), Some(Color::Yellow));
        assert_eq!(parse_color("nope"), None);
        assert_eq!(to_ansi16(Color::Rgb(255, 181, 71)), Color::Yellow);
        assert_eq!(to_ansi16(Color::Indexed(208)), Color::Yellow);
        assert_eq!(to_ansi256(Color::Rgb(255, 0, 0)), Color::Indexed(196));
        assert_eq!(to_ansi256(Color::Rgb(128, 128, 128)), Color::Indexed(244));
        let mut t = terminal_theme();
        t.name = "ember".into();
        t.tokens.get_mut("ui.accent").unwrap().fg = Some("#ffb547".into());
        t.tokens.get_mut("ui.surface").unwrap().bg = Some("#101010".into());
        let p = Palette::new(t.clone(), ColorDepth::Ansi256, true);
        assert!(matches!(p.token("ui.accent").fg, Some(Color::Indexed(_))));
        assert!(
            p.surface().bg.is_some(),
            "non-terminal themes may paint when asked"
        );
        let p = Palette::new(t, ColorDepth::Ansi256, false);
        assert!(p.surface().bg.is_none());
    }

    /// Truecolor themes define the selection as a background only; with `background = "transparent"` (the
    /// default) or without colors it must still be visible, as reverse video.
    #[test]
    fn background_only_selection_falls_back_to_reverse() {
        let mono = oomtop_config::theme::builtin_theme("mono").expect("mono theme");
        assert!(mono.tokens["ui.selection"].bg.is_some() && mono.tokens["ui.selection"].fg.is_none());
        for (depth, paint) in [
            (ColorDepth::Truecolor, false),
            (ColorDepth::None, false),
            (ColorDepth::None, true),
        ] {
            let p = Palette::new(mono.clone(), depth, paint);
            let sel = p.token("ui.selection");
            assert_eq!(sel.bg, None, "{depth:?} paint={paint}");
            assert!(
                sel.add_modifier.contains(Modifier::REVERSED),
                "{depth:?} paint={paint}"
            );
        }
        let painted = Palette::new(mono, ColorDepth::Truecolor, true);
        let sel = painted.token("ui.selection");
        assert!(sel.bg.is_some());
        assert!(
            !sel.add_modifier.contains(Modifier::REVERSED),
            "painted themes keep their color"
        );
    }
}

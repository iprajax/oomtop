//! Minimal SGR rendering of theme styles for `oomtop theme preview` (the TUI uses ratatui instead).
//! Colors degrade truecolor → 256 → 16 → none exactly like the TUI (UX §12.1.5); `default` and missing
//! colors emit nothing, so the terminal's own foreground/background show through.

use oomtop_config::theme::{hex_rgb, Style};
use oomtop_tui::style::ColorDepth;

pub const RESET: &str = "\x1b[0m";

const NAMES: &[&str] = &[
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
];

/// xterm's approximation of the 16 ANSI colors (used only to downsample hex colors).
const ANSI16_RGB: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

fn dist((r1, g1, b1): (u8, u8, u8), (r2, g2, b2): (u8, u8, u8)) -> u32 {
    let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2) as u32;
    d(r1, r2) + d(g1, g2) + d(b1, b2)
}

/// Nearest xterm-256 index (6×6×6 cube or the 24-step gray ramp).
pub fn rgb_to_256((r, g, b): (u8, u8, u8)) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let idx = |v: u8| -> usize {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| (**l as i32 - v as i32).abs())
            .map(|(i, _)| i)
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (idx(r), idx(g), idx(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;
    let avg = ((r as u32 + g as u32 + b as u32) / 3) as u8;
    let gi_ = ((avg.saturating_sub(8)) as u32 / 10).min(23) as u8;
    let gray = 8 + 10 * gi_;
    if dist((gray, gray, gray), (r, g, b)) < dist(cube, (r, g, b)) {
        232 + gi_
    } else {
        cube_idx as u8
    }
}

/// Nearest of the 16 ANSI colors.
pub fn rgb_to_16(rgb: (u8, u8, u8)) -> u8 {
    ANSI16_RGB
        .iter()
        .enumerate()
        .min_by_key(|(_, c)| dist(**c, rgb))
        .map(|(i, _)| i as u8)
        .unwrap_or(7)
}

fn index_code(i: u8, bg: bool, depth: ColorDepth) -> Option<String> {
    match (i, depth) {
        (_, ColorDepth::None) => None,
        (0..=7, _) => Some(format!("{}", if bg { 40 } else { 30 } + i as u32)),
        (8..=15, _) => Some(format!("{}", if bg { 100 } else { 90 } + (i - 8) as u32)),
        (_, ColorDepth::Ansi16) => None,
        _ => Some(format!("{};5;{i}", if bg { 48 } else { 38 })),
    }
}

/// SGR parameter(s) for one color, or `None` (terminal default / unsupported at this depth).
pub fn color_code(c: &str, bg: bool, depth: ColorDepth) -> Option<String> {
    if depth == ColorDepth::None || c == "default" || c.starts_with('@') {
        return None;
    }
    if let Some(i) = NAMES.iter().position(|n| *n == c) {
        return index_code(i as u8, bg, depth);
    }
    if let Ok(i) = c.parse::<u8>() {
        return index_code(i, bg, depth);
    }
    let rgb = hex_rgb(c)?;
    match depth {
        ColorDepth::Truecolor => Some(format!(
            "{};2;{};{};{}",
            if bg { 48 } else { 38 },
            rgb.0,
            rgb.1,
            rgb.2
        )),
        ColorDepth::Ansi256 => index_code(rgb_to_256(rgb), bg, depth),
        ColorDepth::Ansi16 => index_code(rgb_to_16(rgb), bg, depth),
        ColorDepth::None => None,
    }
}

/// Full SGR start sequence for a style ("" when the style is plain). `paint_bg = false` drops backgrounds,
/// as the default `terminal` theme and `appearance.background = "terminal"` require.
pub fn sgr(style: &Style, depth: ColorDepth, paint_bg: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (on, code) in [
        (style.bold, "1"),
        (style.dim, "2"),
        (style.italic, "3"),
        (style.underline, "4"),
        (style.reverse, "7"),
    ] {
        if on {
            parts.push(code.into());
        }
    }
    if let Some(c) = style.fg.as_deref().and_then(|c| color_code(c, false, depth)) {
        parts.push(c);
    }
    if paint_bg {
        if let Some(c) = style.bg.as_deref().and_then(|c| color_code(c, true, depth)) {
            parts.push(c);
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", parts.join(";"))
    }
}

/// `text` wrapped in the style (no escape codes at all for a plain style).
pub fn paint(text: &str, style: &Style, depth: ColorDepth, paint_bg: bool) -> String {
    let s = sgr(style, depth, paint_bg);
    if s.is_empty() {
        text.to_string()
    } else {
        format!("{s}{text}{RESET}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_indices_and_hex() {
        assert_eq!(
            color_code("yellow", false, ColorDepth::Ansi16).as_deref(),
            Some("33")
        );
        assert_eq!(
            color_code("bright-black", true, ColorDepth::Ansi16).as_deref(),
            Some("100")
        );
        assert_eq!(
            color_code("208", false, ColorDepth::Ansi256).as_deref(),
            Some("38;5;208")
        );
        assert_eq!(color_code("208", false, ColorDepth::Ansi16), None);
        assert_eq!(
            color_code("#e07b39", false, ColorDepth::Truecolor).as_deref(),
            Some("38;2;224;123;57")
        );
        assert_eq!(color_code("default", false, ColorDepth::Truecolor), None);
        assert_eq!(color_code("yellow", false, ColorDepth::None), None);
    }

    #[test]
    fn downsampling() {
        assert_eq!(rgb_to_256((255, 0, 0)), 196);
        assert_eq!(rgb_to_256((128, 128, 128)), 244);
        assert_eq!(rgb_to_16((250, 250, 250)), 15);
        assert_eq!(rgb_to_16((200, 10, 10)), 1);
    }

    #[test]
    fn background_only_when_allowed() {
        let st = Style {
            fg: Some("green".into()),
            bg: Some("black".into()),
            bold: true,
            ..Default::default()
        };
        assert_eq!(sgr(&st, ColorDepth::Ansi16, false), "\x1b[1;32m");
        assert_eq!(sgr(&st, ColorDepth::Ansi16, true), "\x1b[1;32;40m");
        assert_eq!(paint("x", &Style::default(), ColorDepth::Truecolor, true), "x");
    }
}

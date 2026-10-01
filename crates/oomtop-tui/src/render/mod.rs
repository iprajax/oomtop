//! Rendering (UX §2, §5.5, §8). Every region is a slot filled from the served state in [`App`]:
//! header (htop-style meter block, headline, true-memory legend, swap/accelerator rows, tab row with badges),
//! body (view or focus mode), overlays (details, why, help, compare, palette), message line and the htop-style
//! function-key bar. Layout is column-count
//! driven: wide ≥ 140, standard 80–139, compact < 80. Meaning is never carried by color alone — every state
//! has a glyph and a word. No animation: changed values and moved rows are marked for one refresh.
//!
//! Widgets ask the [`Palette`] for semantic tokens only. Drawing goes straight to the frame buffer (no
//! intermediate widgets for rows) to keep the UX layer within its 5 ms/frame budget (SPEC §14).

mod header;
mod meters;
mod overlays;
mod views;
pub(crate) mod widgets;

use crate::app::{App, Overlay};
use crate::layout::{LayoutClass, Regions};
use crate::style::Palette;
use oomtop_config::model::Borders as BorderStyle;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Glyph set (UX §8): Unicode blocks by default, ASCII fallback with `--ascii`. No emoji anywhere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyphs {
    pub full: &'static str,
    pub mid: &'static str,
    pub light: &'static str,
    pub empty: &'static str,
    pub ok: &'static str,
    pub warn: &'static str,
    pub crit: &'static str,
    pub select: &'static str,
    pub moved: &'static str,
    pub sep: &'static str,
    /// Pinned marker in "Your things".
    pub pin: &'static str,
    /// Lower-bound prefix (VM footprints).
    pub lower: &'static str,
    /// Estimate prefix.
    pub approx: &'static str,
    /// File-cache segment of the memory bar (reclaimable without swapping).
    pub cache: &'static str,
    /// Horizontal rule.
    pub hline: &'static str,
    /// Tree branches for nested members.
    pub branch: &'static str,
    pub branch_last: &'static str,
    pub ellipsis: &'static str,
    /// Bar caps: `▕…▏`.
    pub cap_l: &'static str,
    pub cap_r: &'static str,
    /// Scrub cursor in the timeline.
    pub cursor: &'static str,
    /// Timeline marker tick.
    pub tick: &'static str,
    /// Sparkline levels, lowest first.
    pub spark: [&'static str; 8],
    /// Braille sparklines are allowed (false for ASCII).
    pub braille: bool,
    pub ascii: bool,
}

pub const UNICODE: Glyphs = Glyphs {
    full: "█",
    mid: "▓",
    light: "▒",
    empty: "░",
    ok: "●",
    warn: "▲",
    crit: "■",
    select: "▸",
    moved: "›",
    sep: "·",
    pin: "★",
    lower: "≥",
    approx: "≈",
    cache: "▁",
    hline: "─",
    branch: "├",
    branch_last: "└",
    ellipsis: "…",
    cap_l: "▕",
    cap_r: "▏",
    cursor: "▲",
    tick: "│",
    spark: ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"],
    braille: true,
    ascii: false,
};

pub const ASCII: Glyphs = Glyphs {
    full: "#",
    mid: "=",
    light: "-",
    empty: ".",
    ok: "o",
    warn: "!",
    crit: "X",
    select: ">",
    // Every ASCII glyph has one meaning: "moved" is not the cursor, "at least" is not "more than", and the
    // ellipsis is not "approximately".
    moved: "+",
    sep: "|",
    pin: "*",
    lower: ">=",
    approx: "~",
    cache: ":",
    hline: "-",
    branch: "|",
    branch_last: "`",
    ellipsis: "..",
    cap_l: "[",
    cap_r: "]",
    cursor: "^",
    tick: "|",
    spark: ["_", ".", ",", "-", "=", "+", "*", "#"],
    braille: false,
    ascii: true,
};

/// Truncates to a display width, appending "…" when cut, and pads to exactly `width`.
pub fn fit(s: &str, width: usize) -> String {
    fit_with(s, width, "…")
}

/// [`fit`] with a custom ellipsis (ASCII mode uses "..").
pub fn fit_with(s: &str, width: usize, ellipsis: &str) -> String {
    let w = s.width();
    if w <= width {
        let mut out = String::with_capacity(s.len() + width - w);
        out.push_str(s);
        out.extend(std::iter::repeat_n(' ', width - w));
        return out;
    }
    if width == 0 {
        return String::new();
    }
    let ew = ellipsis.width();
    if ew >= width {
        // No room for the ellipsis: plain truncation.
        let mut out = String::new();
        let mut cur = 0;
        for c in s.chars() {
            let cw = c.width().unwrap_or(0);
            if cur + cw > width {
                break;
            }
            out.push(c);
            cur += cw;
        }
        out.extend(std::iter::repeat_n(' ', width.saturating_sub(cur)));
        return out;
    }
    let mut out = String::new();
    let mut cur = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if cur + cw + ew > width {
            break;
        }
        out.push(c);
        cur += cw;
    }
    out.push_str(ellipsis);
    cur += ew;
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(cur)));
    out
}

/// Right-aligns to `width` (truncating from the left is never done for numbers; they are short).
pub fn rjust(s: &str, width: usize) -> String {
    let w = s.width();
    if w >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - w))
    }
}

/// Border symbols per `appearance.borders` (ASCII glyphs force `+-|`).
pub fn border_set(b: BorderStyle, g: &Glyphs) -> border::Set<'static> {
    if g.ascii {
        return border::Set {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            vertical_left: "|",
            vertical_right: "|",
            horizontal_top: "-",
            horizontal_bottom: "-",
        };
    }
    match b {
        BorderStyle::Rounded => border::ROUNDED,
        BorderStyle::Plain => border::PLAIN,
        BorderStyle::Thick => border::THICK,
        BorderStyle::Double => border::DOUBLE,
        BorderStyle::None => border::Set {
            top_left: " ",
            top_right: " ",
            bottom_left: " ",
            bottom_right: " ",
            vertical_left: " ",
            vertical_right: " ",
            horizontal_top: " ",
            horizontal_bottom: " ",
        },
    }
}

/// Rendering context shared by the sub-renderers.
pub(crate) struct Ctx<'a> {
    pub app: &'a App,
    pub p: &'a Palette,
    pub g: &'a Glyphs,
    pub class: LayoutClass,
    pub borders: BorderStyle,
}

impl Ctx<'_> {
    pub fn t(&self, token: &str) -> Style {
        self.p.token(token)
    }
    pub fn fit(&self, s: &str, w: usize) -> String {
        fit_with(s, w, self.g.ellipsis)
    }
}

/// Writes a line at `(area.x, y)` clipped to the area width.
pub(crate) fn put(buf: &mut Buffer, area: Rect, y: u16, line: &Line) {
    if y < area.y || y >= area.y + area.height {
        return;
    }
    buf.set_line(area.x, y, line, area.width);
}

/// A section rule: `─ Title ───── right ─`.
pub(crate) fn rule<'a>(c: &Ctx, title: &str, right: &str, width: usize) -> Line<'a> {
    let h = c.g.hline;
    let left = format!("{h} {title} ");
    let right = if right.is_empty() {
        String::new()
    } else {
        format!(" {right} {h}")
    };
    let fill = width.saturating_sub(left.width() + right.width());
    Line::from(vec![
        Span::styled(format!("{h} "), c.t("ui.border")),
        Span::styled(format!("{title} "), c.t("text.label")),
        Span::styled(h.repeat(fill), c.t("ui.border")),
        Span::styled(right, c.t("ui.muted")),
    ])
}

/// Draws one frame with the default border style.
pub fn draw(frame: &mut Frame, app: &App, p: &Palette, g: &Glyphs) {
    draw_with(frame, app, p, g, BorderStyle::Rounded)
}

/// Draws one frame. `borders` follows `appearance.borders` (overlays only; sections use rules).
pub fn draw_with(frame: &mut Frame, app: &App, p: &Palette, g: &Glyphs, borders: BorderStyle) {
    let area = frame.area();
    let buf = frame.buffer_mut();
    if p.paint_background {
        buf.set_style(area, p.surface());
    }
    let c = Ctx {
        app,
        p,
        g,
        class: LayoutClass::for_width(area.width),
        borders,
    };
    let mut regions = Regions::default();
    if area.width < 20 || area.height < 6 {
        let msg = if area.width >= 24 {
            "oomtop: window too small"
        } else {
            "too small"
        };
        put(
            buf,
            area,
            area.y,
            &Line::from(Span::styled(msg, c.t("state.warn"))),
        );
        *app.regions.borrow_mut() = regions;
        return;
    }
    if let Some(st) = &app.settings {
        overlays::settings_screen(&c, buf, area, st, &mut regions);
        if g.ascii {
            asciify(buf);
        }
        *app.regions.borrow_mut() = regions;
        return;
    }
    let header = header::render(&c, buf, area, &mut regions);
    let msg = views::message_line(&c, area.width as usize);
    let footer_h = 1u16 + u16::from(msg.is_some());
    let body = Rect {
        x: area.x,
        y: area.y + header,
        width: area.width,
        height: area.height.saturating_sub(header + footer_h),
    };
    if app.focus.is_some() {
        views::focus(&c, buf, body);
    } else {
        views::body(&c, buf, body, &mut regions);
    }
    if let Some((line, _)) = &msg {
        put(buf, area, area.y + area.height - 2, line);
    }
    views::footer(
        &c,
        buf,
        Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        &mut regions,
    );
    // Overlays last, over the body.
    match &app.overlay {
        Some(Overlay::Help) => overlays::help(&c, buf, body),
        Some(Overlay::Details) => overlays::details(&c, buf, body),
        Some(Overlay::WhyRank) => overlays::why_rank(&c, buf, body),
        Some(Overlay::WhySlow) => overlays::why_slow(&c, buf, body),
        Some(Overlay::Answer(title, lines)) => overlays::answer(&c, buf, body, title, lines),
        None => {}
    }
    if let Some((_, Some(_))) = &app.compare {
        overlays::compare(&c, buf, body);
    }
    if matches!(
        app.input,
        Some(crate::app::Input::Palette(_)) | Some(crate::app::Input::Command(_))
    ) {
        overlays::palette(&c, buf, body);
    }
    if g.ascii {
        asciify(buf);
    }
    *app.regions.borrow_mut() = regions;
}

/// ASCII fallback for text that comes from data or core templates (labels, headline dashes): `--ascii`
/// guarantees an ASCII-only screen.
pub fn ascii_char(s: &str) -> &'static str {
    match s {
        "—" | "–" | "─" | "━" | "═" | "‐" | "−" => "-",
        "·" | "│" | "┃" | "║" | "▏" | "▕" | "|" => "|",
        "…" | "≈" | "∼" => "~",
        "≥" | "→" | "›" | "»" | "▸" | "▶" => ">",
        "≤" | "←" | "‹" | "«" => "<",
        "×" => "x",
        "★" | "☆" => "*",
        "°" => "o",
        "█" | "▓" => "#",
        "▒" | "░" => "=",
        "▁" | "▂" | "▃" | "▄" => "_",
        "▅" | "▆" | "▇" => "=",
        "╭" | "╮" | "╰" | "╯" | "┌" | "┐" | "└" | "┘" | "├" | "┤" | "┬" | "┴" | "┼" => {
            "+"
        }
        "●" => "o",
        "▲" => "!",
        "■" => "X",
        _ => "?",
    }
}

fn asciify(buf: &mut Buffer) {
    for cell in buf.content.iter_mut() {
        let sym = cell.symbol();
        if !sym.is_ascii() {
            let rep = if sym.chars().all(|c| ('\u{2800}'..='\u{28FF}').contains(&c)) {
                "."
            } else {
                ascii_char(sym)
            };
            cell.set_symbol(rep);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_truncates_by_width() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdefgh", 5), "abcd…");
        assert_eq!(fit_with("abcdefgh", 5, "~"), "abcd~");
        assert_eq!(fit("", 0), "");
        assert_eq!(
            fit("日本語テキスト", 6),
            "日本… ",
            "wide chars never split; padded to width"
        );
        assert_eq!(fit("日本語テキスト", 6).width(), 6);
        assert_eq!(rjust("9.9G", 6), "  9.9G");
    }

    #[test]
    fn glyph_widths_are_single_cell() {
        for g in [UNICODE, ASCII] {
            for s in [
                g.full,
                g.mid,
                g.light,
                g.empty,
                g.ok,
                g.warn,
                g.crit,
                g.select,
                g.moved,
                g.sep,
                g.pin,
                g.approx,
                g.cache,
                g.hline,
                g.branch,
                g.branch_last,
                g.cap_l,
                g.cap_r,
                g.cursor,
                g.tick,
            ] {
                assert_eq!(s.width(), 1, "{s:?}");
            }
            for s in g.spark {
                assert_eq!(s.width(), 1, "{s:?}");
            }
        }
        for s in [ASCII.full, ASCII.ok, ASCII.select, ASCII.pin] {
            assert!(s.is_ascii());
        }
        // `lower` and `ellipsis` are only used as text prefixes / inside `fit_with`, never as a fixed cell.
        assert_eq!(UNICODE.lower.width(), 1);
        assert_eq!(UNICODE.ellipsis.width(), 1);
        assert!(ASCII.lower.is_ascii() && ASCII.ellipsis.is_ascii());
    }

    #[test]
    fn ascii_glyphs_have_one_meaning_each() {
        let g = ASCII;
        let marks = [
            ("select", g.select),
            ("moved", g.moved),
            ("pin", g.pin),
            ("lower", g.lower),
            ("approx", g.approx),
            ("ellipsis", g.ellipsis),
            ("warn", g.warn),
            ("crit", g.crit),
            ("ok", g.ok),
            ("cursor", g.cursor),
        ];
        for (i, (a, x)) in marks.iter().enumerate() {
            for (b, y) in &marks[i + 1..] {
                assert_ne!(x, y, "{a} and {b} share {x:?}");
            }
        }
        assert_eq!(fit_with("abcdefgh", 5, ".."), "abc..");
        assert_eq!(fit_with("abcdefgh", 1, ".."), "a");
    }
}

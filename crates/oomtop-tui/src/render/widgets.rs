//! Small text widgets: segmented bars, mini-bars, sparklines (braille / blocks / ASCII / none) and state
//! badges that always pair a glyph with a word (meaning never by color alone, UX §8).

use super::{Ctx, Glyphs};
use oomtop_config::model::Sparklines;
use ratatui::style::Style;
use ratatui::text::Span;

/// Splits `width` cells among segments proportionally to `values / total`; the remainder is the track.
/// Returns the cell count per segment (never exceeding `width` in total).
pub fn segment_cells(values: &[u64], total: u64, width: usize) -> Vec<usize> {
    let total = total.max(1) as f64;
    let mut out = Vec::with_capacity(values.len());
    let mut used = 0usize;
    let mut acc = 0f64;
    for v in values {
        acc += *v as f64 / total * width as f64;
        let upto = (acc.round() as usize).min(width);
        let n = upto.saturating_sub(used);
        out.push(n);
        used += n;
    }
    out
}

/// A segmented bar `▕███▓▓▒▒░░▏` as spans; `segs` = (cells, glyph, style).
pub fn bar_spans<'a>(c: &Ctx, segs: &[(usize, &str, Style)], width: usize) -> Vec<Span<'a>> {
    let mut spans = vec![Span::styled(c.g.cap_l.to_string(), c.t("ui.border"))];
    let mut used = 0;
    for (n, glyph, style) in segs {
        if *n == 0 {
            continue;
        }
        spans.push(Span::styled(glyph.repeat(*n), *style));
        used += n;
    }
    if used < width {
        spans.push(Span::styled(
            c.g.empty.repeat(width - used),
            c.t("chart.bar.track"),
        ));
    }
    spans.push(Span::styled(c.g.cap_r.to_string(), c.t("ui.border")));
    spans
}

/// A mini-bar for a 0..100 percentage.
pub fn mini_bar<'a>(c: &Ctx, pct: Option<f64>, width: usize, fill_token: &str) -> Vec<Span<'a>> {
    let n = pct
        .map(|p| ((p.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize)
        .unwrap_or(0);
    bar_spans(c, &[(n.min(width), c.g.full, c.t(fill_token))], width)
}

const BRAILLE_LEFT: [u32; 4] = [0x40, 0x04, 0x02, 0x01];
const BRAILLE_RIGHT: [u32; 4] = [0x80, 0x20, 0x10, 0x08];

/// Scales values to levels `0..=levels` (min..max when there is visible variation, else a flat line).
fn levels(values: &[f64], levels: usize) -> Vec<usize> {
    let max = values.iter().copied().fold(f64::MIN, f64::max);
    let min = values.iter().copied().fold(f64::MAX, f64::min);
    if values.is_empty() || max <= 0.0 {
        return vec![0; values.len()];
    }
    let span = max - min;
    if span <= max * 0.02 {
        return vec![(levels / 2).max(1); values.len()];
    }
    values
        .iter()
        .map(|v| 1 + (((v - min) / span) * (levels - 1) as f64).round() as usize)
        .collect()
}

/// Sparkline text of at most `width` cells for the newest values.
pub fn sparkline(values: &[f64], width: usize, style: Sparklines, g: &Glyphs) -> String {
    // A single sample is not a trend: draw nothing rather than a meaningless dot.
    if width == 0 || values.len() < 2 || style == Sparklines::None {
        return String::new();
    }
    let braille = style == Sparklines::Braille && g.braille;
    let per_cell = if braille { 2 } else { 1 };
    let take = (width * per_cell).min(values.len());
    let vals = &values[values.len() - take..];
    if braille {
        let lv = levels(vals, 4);
        let mut out = String::new();
        for pair in lv.chunks(2) {
            let mut bits = 0u32;
            for (i, l) in pair.iter().enumerate() {
                let dots = if i == 0 { &BRAILLE_LEFT } else { &BRAILLE_RIGHT };
                for d in dots.iter().take((*l).min(4)) {
                    bits |= d;
                }
            }
            out.push(char::from_u32(0x2800 + bits).unwrap_or(' '));
        }
        out
    } else {
        let lv = levels(vals, 8);
        lv.iter().map(|l| g.spark[l.saturating_sub(1).min(7)]).collect()
    }
}

pub fn sparkline_u64(values: &[u64], width: usize, style: Sparklines, g: &Glyphs) -> String {
    let v: Vec<f64> = values.iter().map(|x| *x as f64).collect();
    sparkline(&v, width, style, g)
}

/// Multi-row bar chart (focus mode): `rows` lines, oldest left; each column one value.
pub fn column_chart(values: &[f64], width: usize, rows: usize, g: &Glyphs) -> Vec<String> {
    let mut out = vec![String::new(); rows];
    if values.is_empty() || width == 0 || rows == 0 {
        return out;
    }
    let take = width.min(values.len());
    let vals = &values[values.len() - take..];
    let max = vals.iter().copied().fold(0.0f64, f64::max).max(1.0);
    let min = vals.iter().copied().fold(f64::MAX, f64::min);
    let base = if max - min > max * 0.05 { min * 0.9 } else { 0.0 };
    let eighths = rows * 8;
    for v in vals {
        let h = (((v - base) / (max - base).max(1e-9)) * eighths as f64)
            .round()
            .clamp(1.0, eighths as f64) as usize;
        for (r, line) in out.iter_mut().enumerate() {
            let row_from_bottom = rows - 1 - r;
            let lo = row_from_bottom * 8;
            let cell = if h >= lo + 8 {
                g.spark[7]
            } else if h > lo {
                g.spark[(h - lo - 1).min(7)]
            } else {
                " "
            };
            line.push_str(cell);
        }
    }
    out
}

/// (glyph, word, token) for a pressure level.
pub fn pressure_badge(
    g: &Glyphs,
    p: Option<oomtop_core::PressureLevel>,
) -> (&'static str, &'static str, &'static str) {
    use oomtop_core::PressureLevel as P;
    match p {
        Some(P::Critical) => (g.crit, "Critical", "state.crit"),
        Some(P::Warn) => (g.warn, "Pressure", "state.warn"),
        Some(P::Normal) => (g.ok, "Normal", "state.ok"),
        None => ("-", "pressure n/a", "state.stale"),
    }
}

/// (glyph, word, token) for thermal pressure.
pub fn thermal_badge(
    g: &Glyphs,
    t: Option<oomtop_core::ThermalPressure>,
) -> Option<(&'static str, &'static str, &'static str)> {
    use oomtop_core::ThermalPressure as T;
    Some(match t? {
        T::Nominal => return None,
        T::Moderate => (g.warn, "warm", "state.warn"),
        T::Heavy => (g.crit, "hot", "state.crit"),
        T::Trapping => (g.crit, "trapping", "state.crit"),
        T::Sleeping => (g.crit, "sleeping", "state.crit"),
    })
}

#[cfg(test)]
mod tests {
    use super::super::{ASCII, UNICODE};
    use super::*;

    #[test]
    fn segments_never_overflow() {
        assert_eq!(segment_cells(&[12, 6, 6], 24, 24), vec![12, 6, 6]);
        let s = segment_cells(&[20, 20, 20], 24, 24);
        assert_eq!(s.iter().sum::<usize>(), 24);
        assert_eq!(segment_cells(&[1, 1], 1000, 10), vec![0, 0]);
    }

    #[test]
    fn sparklines() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        assert_eq!(sparkline(&v, 8, Sparklines::Blocks, &UNICODE), "▁▂▃▄▅▆▇█");
        assert_eq!(sparkline(&v, 8, Sparklines::Blocks, &ASCII), "_.,-=+*#");
        assert_eq!(sparkline(&v, 4, Sparklines::Braille, &UNICODE).chars().count(), 4);
        assert_eq!(
            sparkline(&v[4..], 4, Sparklines::Braille, &ASCII),
            "_,+#",
            "ASCII never uses braille"
        );
        assert_eq!(sparkline(&v, 4, Sparklines::None, &UNICODE), "");
        assert_eq!(
            sparkline(&[5.0, 5.0, 5.0], 3, Sparklines::Blocks, &UNICODE),
            "▄▄▄"
        );
        let b = sparkline(&[0.0, 10.0], 1, Sparklines::Braille, &UNICODE);
        assert_eq!(b, "\u{28F8}", "left column 1 dot, right column full");
    }

    #[test]
    fn chart_rows() {
        let rows = column_chart(&[1.0, 2.0, 4.0, 8.0], 4, 2, &UNICODE);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].chars().count(), 4);
        assert!(rows[0].ends_with('█'));
    }
}

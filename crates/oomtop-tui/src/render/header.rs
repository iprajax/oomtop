//! Header (UX §2, SPEC §12.1): the htop-style meter block (per-core CPU, Mem/Swp/GPU, tasks/load/uptime — see
//! `meters`), the headline sentence with its one action key, the true-memory legend (or bar, without meters),
//! swap trend + forecast, accelerator/thermal row, and the tab row (views + state badges). Rows follow
//! `appearance.header`; widths and the terminal height decide how much fits.

use super::widgets::{bar_spans, mini_bar, pressure_badge, segment_cells, sparkline_u64, thermal_badge};
use super::{put, Ctx};
use crate::app::View;
use crate::layout::{HitTarget, LayoutClass, Regions};
use oomtop_core::throttle::THROTTLED_BELOW;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Renders the header into the top of `area`; returns the number of rows used.
///
/// Order (UX §2): the htop-style meter block, the headline sentence, the true-memory legend, swap/forecast,
/// accelerators/thermal, then the tab row (views left, state badges right — htop's "Main" tab row). Rows follow
/// `appearance.header`; widths and the terminal height decide how much fits.
pub(crate) fn render(c: &Ctx, buf: &mut Buffer, area: Rect, regions: &mut Regions) -> u16 {
    let mut y = area.y;
    let w = area.width as usize;
    let mut rows: Vec<&str> = if c.app.header_rows.is_empty() {
        DEFAULT_ROWS.to_vec()
    } else {
        c.app.header_rows.iter().map(String::as_str).collect()
    };
    // Situation modes change emphasis (UX §3): throttling moves temps/power/clock up, right after the headline.
    if c.app.mode == oomtop_core::modes::Mode::Throttle {
        if let Some(i) = rows.iter().position(|r| *r == "accelerators") {
            let r = rows.remove(i);
            let at = rows
                .iter()
                .position(|r| *r == "headline")
                .map(|h| h + 1)
                .unwrap_or(0);
            rows.insert(at, r);
        }
    }
    // A named layout decides whether the Home screen carries the headline and the meters (UX §12.6).
    let (show_headline, show_meters) = match (&c.app.layout, c.app.view) {
        (Some(l), View::Home) if c.app.focus.is_none() => l.header_parts(area.width),
        _ => (true, true),
    };
    let meters = show_meters && rows.contains(&"meters");
    // Short terminals keep the list usable: secondary rows go first (the meter block already has Mem/Swp/GPU).
    if area.height < SHORT_HEIGHT {
        rows.retain(|r| *r != "accelerators" && *r != "thermal");
    }
    // Compact memory row (avail + swap) repeats the Mem/Swp meters; short terminals drop the legend too.
    if meters && (area.height < VERY_SHORT_HEIGHT || c.class == LayoutClass::Compact) {
        rows.retain(|r| *r != "memory");
    }
    let max_rows = match c.class {
        LayoutClass::Compact => 4,
        _ => 6,
    };
    let throttle = c.app.mode == oomtop_core::modes::Mode::Throttle;
    // Temps/power/clock are shown in wide layouts, and in standard ones while throttling (UX §3).
    let meters_row = c.class == LayoutClass::Wide || (throttle && c.class == LayoutClass::Standard);
    let mut other = 0u16;
    // Always leave the tab row, the F-key bar and a few list rows.
    let limit = area.y + area.height.saturating_sub(5);
    for row in rows {
        if y >= limit {
            break;
        }
        if row == "meters" {
            if meters {
                y += super::meters::render(c, buf, area, y, regions);
            }
            continue;
        }
        if other >= max_rows {
            continue;
        }
        if (row == "headline" && !show_headline) || (row != "headline" && !show_meters) {
            continue;
        }
        let before = y;
        match row {
            "headline" => {
                let max = if area.height < VERY_SHORT_HEIGHT {
                    1
                } else {
                    usize::MAX
                };
                for l in headline_lines(c, w).into_iter().take(max) {
                    put(buf, area, y, &l);
                    y += 1;
                }
            }
            "memory" => {
                put(buf, area, y, &memory_line(c, w, !meters));
                regions.hits.push((
                    Rect::new(area.x, y, area.width, 1),
                    HitTarget::View(View::Processes),
                ));
                y += 1;
            }
            "swap" if c.class != LayoutClass::Compact => {
                let (line, gpu_x) = swap_line(c, w, meters);
                put(buf, area, y, &line);
                let gx = gpu_x.map(|x| x as u16).unwrap_or(area.width);
                regions.hits.push((
                    Rect::new(area.x, y, gx.min(area.width), 1),
                    HitTarget::View(View::Timeline),
                ));
                if gx < area.width {
                    regions.hits.push((
                        Rect::new(area.x + gx, y, area.width - gx, 1),
                        HitTarget::View(View::Models),
                    ));
                }
                y += 1;
            }
            "accelerators" if meters_row => {
                if let Some(l) = accel_line(c, w, rows_include(c, "thermal"), meters) {
                    put(buf, area, y, &l);
                    regions
                        .hits
                        .push((Rect::new(area.x, y, area.width, 1), HitTarget::View(View::Models)));
                    y += 1;
                }
            }
            // `thermal` without `accelerators` in `appearance.header`: its own row.
            "thermal" if meters_row && !rows_include(c, "accelerators") => {
                let spans = thermal_spans(c);
                if !spans.is_empty() {
                    let mut spans = spans;
                    while width_of(&spans) > w && spans.len() > 1 {
                        spans.pop();
                    }
                    put(buf, area, y, &Line::from(spans));
                    regions
                        .hits
                        .push((Rect::new(area.x, y, area.width, 1), HitTarget::WhySlow));
                    y += 1;
                }
            }
            _ => {}
        }
        if y > before {
            other += 1;
        }
    }
    tab_line(c, buf, area, y, regions);
    y += 1;
    y - area.y
}

/// Default `appearance.header` (config default too).
pub(crate) const DEFAULT_ROWS: &[&str] = &["meters", "headline", "memory", "swap", "accelerators", "thermal"];
/// Room the tab row keeps for the state badges before tab titles shrink to numbers.
const MIN_BADGE_ROOM: usize = 14;
/// Below this height the accelerator/thermal row is dropped.
const SHORT_HEIGHT: u16 = 28;
/// Below this height the memory legend goes too (Mem is in the meter block) and the headline gets one line.
const VERY_SHORT_HEIGHT: u16 = 22;

fn rows_include(c: &Ctx, name: &str) -> bool {
    c.app.header_rows.is_empty() || c.app.header_rows.iter().any(|r| r == name)
}

/// The tab row: `oomtop  [1 Home] 2 Processes …` on the left, machine + state badges on the right (dropped by
/// priority when the width runs out; the tabs always stay).
fn tab_line(c: &Ctx, buf: &mut Buffer, area: Rect, y: u16, regions: &mut Regions) {
    let app = c.app;
    let w = area.width as usize;
    // Full tab titles, unless they would push out the state badges (the pressure badge needs ~12 columns):
    // then non-current tabs show their number only, as in the compact layout.
    let full: usize = 8
        + View::ALL
            .iter()
            .enumerate()
            .map(|(i, v)| format!(" {} {}", i + 1, v.title()).width())
            .sum::<usize>()
        + 2;
    let short = c.class == LayoutClass::Compact || full + MIN_BADGE_ROOM > w;
    let mut spans: Vec<Span> = vec![Span::styled(" oomtop ".to_string(), c.t("ui.accent"))];
    let mut hits = Vec::new();
    let mut x = 8usize;
    for (i, v) in View::ALL.iter().enumerate() {
        let current = *v == app.view;
        let text = if short && !current {
            format!("{}", i + 1)
        } else {
            format!("{} {}", i + 1, v.title())
        };
        let text = if current { format!("[{text}]") } else { text };
        let wd = text.width();
        spans.push(Span::raw(" "));
        x += 1;
        hits.push((Rect::new(area.x + x as u16, y, wd as u16, 1), HitTarget::View(*v)));
        spans.push(Span::styled(
            text,
            if current {
                c.t("ui.accent")
            } else {
                c.t("text.key")
            },
        ));
        x += wd;
    }
    regions.hits.extend(hits);
    let tabs_w = width_of(&spans);
    let badges = badge_spans(c, w.saturating_sub(tabs_w + 2));
    let bw = width_of(&badges);
    // The machine name, when there is room between the tabs and the badges.
    let s = &app.snapshot;
    let total = s.memory.total.value.unwrap_or(s.host.mem_total);
    let machine = format!(
        "{} {} {}{}  ",
        oomtop_core::machine::display_name(&s.host),
        c.g.sep,
        app.fmt.bytes(total),
        if s.host.unified_memory { " unified" } else { "" }
    );
    let room = w.saturating_sub(tabs_w + bw);
    let machine = (c.class != LayoutClass::Compact && room >= machine.width() + 4).then_some(machine);
    let mw = machine.as_ref().map(|m| m.width()).unwrap_or(0);
    let pad = w.saturating_sub(tabs_w + bw + mw);
    spans.push(Span::raw(" ".repeat(pad)));
    if let Some(m) = machine {
        spans.push(Span::styled(m, c.t("ui.muted")));
    }
    let badge_x = area.x + (tabs_w + pad + mw) as u16;
    spans.extend(badges);
    put(buf, area, y, &Line::from(spans));
    if bw > 0 {
        regions
            .hits
            .push((Rect::new(badge_x, y, bw as u16, 1), HitTarget::WhySlow));
    }
}

/// State badges (mode, pressure, speed, thermal, Low Power, battery, limited sources) that fit in `room`,
/// lowest priority dropped first, display order kept.
fn badge_spans<'a>(c: &Ctx, room: usize) -> Vec<Span<'a>> {
    let s = &c.app.snapshot;
    // (priority: lower = kept longer, text, token) in display order.
    let mut badges: Vec<(u8, String, &str)> = Vec::new();
    let mode = c.app.mode;
    if mode != oomtop_core::modes::Mode::Calm || c.app.modes.is_pinned() {
        let pin = if c.app.modes.is_pinned() { " (pinned)" } else { "" };
        badges.push((5, format!("mode {}{pin}", mode.as_str()), "text.label"));
    }
    let (glyph, word, tok) = pressure_badge(c.g, s.memory.pressure.value);
    badges.push((0, format!("{glyph} {word}"), tok));
    if let Some(tf) = s.thermal.throttle_factor.value {
        if tf < 0.995 {
            let tok = if tf < THROTTLED_BELOW {
                "state.warn"
            } else {
                "text.number"
            };
            badges.push((1, format!("{:.0}% speed", tf * 100.0), tok));
        }
    }
    if let Some((g, word, tok)) = thermal_badge(c.g, s.thermal.pressure.value) {
        badges.push((2, format!("{g} {word}"), tok));
    }
    if s.thermal.low_power_mode.value == Some(true) {
        badges.push((3, "Low Power".into(), "state.warn"));
    }
    if s.thermal.on_battery.value == Some(true) {
        let pct = s
            .thermal
            .battery_pct
            .value
            .map(|b| format!(" {b:.0}%"))
            .unwrap_or_default();
        let tok = if s.thermal.battery_pct.value.map(|b| b < 20.0).unwrap_or(false) {
            "state.warn"
        } else {
            "text.number"
        };
        badges.push((4, format!("bat{pct}"), tok));
    }
    if c.class == LayoutClass::Wide {
        let limited = s
            .source_status
            .values()
            .filter(|v| !matches!(v, oomtop_core::SourceStatus::Available))
            .count();
        if limited > 0 {
            badges.push((
                6,
                format!("{limited} source{} limited", if limited == 1 { "" } else { "s" }),
                "state.stale",
            ));
        }
    }
    // Drop the lowest-priority badges until everything fits (display order is kept).
    let width = |b: &[(u8, String, &str)]| b.iter().map(|x| x.1.width() + 2).sum::<usize>();
    while !badges.is_empty() && width(&badges) > room {
        let worst = badges
            .iter()
            .enumerate()
            .max_by_key(|(_, b)| b.0)
            .map(|(i, _)| i)
            .unwrap_or(0);
        badges.remove(worst);
    }
    let mut right = Vec::new();
    for (_, text, tok) in badges {
        right.push(Span::styled(text, c.t(tok)));
        right.push(Span::raw("  "));
    }
    right
}

/// Headline sentence + `[key]`, word-wrapped (compact layouts allow three lines, others two).
pub(crate) fn headline_lines<'a>(c: &Ctx, w: usize) -> Vec<Line<'a>> {
    let h = &c.app.headline;
    // The key really bound to the headline action in the active keymap (hidden when unbound).
    let key = h
        .action_key
        .and_then(|_| c.app.key_for("headline-action"))
        .map(|k| format!("  [{k}]"))
        .unwrap_or_default();
    let avail = w.saturating_sub(1).max(8);
    let style = match h.mode {
        oomtop_core::modes::Mode::Pressure | oomtop_core::modes::Mode::Throttle => {
            c.t("text.headline").patch(c.t("state.warn"))
        }
        _ => c.t("text.headline"),
    };
    let max_lines = if c.class == LayoutClass::Compact { 3 } else { 2 };
    let mut lines: Vec<String> = vec![String::new()];
    for word in h.text.split(' ') {
        let cur = lines.last().map(|l| l.width()).unwrap_or(0);
        let need = if cur == 0 {
            word.width()
        } else {
            cur + 1 + word.width()
        };
        if need > avail && cur > 0 {
            lines.push(String::new());
        }
        let last = lines.last_mut().expect("at least one line");
        if !last.is_empty() {
            last.push(' ');
        }
        last.push_str(word);
    }
    if lines.len() > max_lines {
        let rest = lines.split_off(max_lines - 1).join(" ");
        lines.push(rest);
    }
    let n = lines.len();
    lines
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            if i + 1 == n {
                let room = avail.saturating_sub(key.width());
                let t = if text.width() > room {
                    c.fit(&text, room).trim_end().to_string()
                } else {
                    text
                };
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(t, style),
                    Span::styled(key.clone(), c.t("text.key")),
                ])
            } else {
                Line::from(vec![Span::raw(" "), Span::styled(text, style)])
            }
        })
        .collect()
}

/// Bytes of GPU memory that live in host RAM (unified accelerators only).
pub(crate) fn unified_gpu(c: &Ctx) -> Option<u64> {
    let v: Vec<u64> = c
        .app
        .snapshot
        .accelerators
        .iter()
        .filter(|a| a.unified)
        .filter_map(|a| a.mem_used.value)
        .collect();
    (!v.is_empty()).then(|| v.iter().sum())
}

/// True-memory bar segments (bytes): apps, gpu/metal, compressed, other wired, file cache.
/// On unified memory Metal allocations are wired pages, so they are carved out of wired (or out of apps when
/// the OS reports them there) — the bar never counts them twice and "free" is what is left.
pub(crate) fn memory_segments(s: &oomtop_core::Snapshot, gpu: Option<u64>) -> [u64; 5] {
    let m = &s.memory;
    let app = m.app.value.unwrap_or(0);
    let gpu = gpu.unwrap_or(0);
    let wired = m.wired.value.unwrap_or(0);
    let comp = m.compressed.value.unwrap_or(0);
    let cache = m.cached.value.unwrap_or(0);
    let (app_bar, wired_bar) = if wired >= gpu {
        (app, wired - gpu)
    } else {
        (app.saturating_sub(gpu - wired), 0)
    };
    [app_bar, gpu, comp, wired_bar, cache]
}

/// The true-memory row. With the meter block on screen (`with_bar = false`) it is the legend of the `Mem[…]`
/// bar — same characters, one number per segment; otherwise it carries its own segmented bar.
fn memory_line<'a>(c: &Ctx, w: usize, with_bar: bool) -> Line<'a> {
    let s = &c.app.snapshot;
    let m = &s.memory;
    let f = &c.app.fmt;
    let total = m.total.value.unwrap_or(s.host.mem_total);
    let gpu = unified_gpu(c);
    let seg = memory_segments(s, gpu);
    let bar_w = match c.class {
        LayoutClass::Wide => 40,
        LayoutClass::Standard => (w / 5).clamp(14, 28),
        LayoutClass::Compact => (w / 5).clamp(8, 14),
    };
    let cells = segment_cells(&seg, total, bar_w);
    let used: usize = cells.iter().sum();
    let g = c.g;
    let segs = [
        (cells[0], g.full, c.t("mem.app")),
        (cells[1], g.mid, c.t("mem.gpu")),
        (cells[2], g.empty, c.t("mem.compressed")),
        (cells[3], g.light, c.t("mem.wired")),
        (cells[4], g.cache, c.t("mem.cache")),
        (bar_w - used.min(bar_w), " ", c.t("mem.free")),
    ];
    let mut spans = vec![Span::styled(
        if with_bar { " MEM " } else { " MEM" }.to_string(),
        c.t("text.label"),
    )];
    if with_bar {
        spans.extend(bar_spans(c, &segs, bar_w));
    }
    // Legend glyphs repeat the bar in use: the block glyphs of the own bar, or the meter block's characters.
    let mc = super::meters::MEM_CHARS;
    let lg = |i: usize, own: &'static str| if with_bar { own } else { mc[i] };
    // Legend items: (priority — lower is kept longer, spans). Glyphs repeat the bar fill so every segment is
    // identifiable without color; whole items are dropped (never a label without its number).
    let item = |glyph: &str, label: &str, value: String, tok: &str| -> Vec<Span<'a>> {
        let mut v = vec![Span::raw(" ")];
        if !glyph.trim().is_empty() {
            v.push(Span::styled(glyph.to_string(), c.t(tok)));
        }
        v.push(Span::styled(format!("{label} "), c.t("text.label")));
        v.push(Span::styled(value, c.t("text.number")));
        v
    };
    let na = |v: Option<u64>| f.opt_bytes(v);
    let long = c.class == LayoutClass::Wide;
    let mut items: Vec<(u8, Vec<Span<'a>>)> = Vec::new();
    if c.class == LayoutClass::Compact {
        items.push((0, item("", "avail", na(m.available.value), "mem.free")));
        items.push((1, item("", "swap", na(m.swap_used.value), "mem.swap")));
    } else {
        items.push((
            0,
            item(lg(0, g.full), "apps", na(m.app.value.map(|_| seg[0])), "mem.app"),
        ));
        if gpu.is_some() {
            items.push((
                0,
                item(
                    lg(1, g.mid),
                    if long { "gpu/metal" } else { "gpu" },
                    na(gpu),
                    "mem.gpu",
                ),
            ));
        }
        items.push((
            2,
            item(
                lg(2, g.empty),
                if long { "compressed" } else { "cmp" },
                na(m.compressed.value),
                "mem.compressed",
            ),
        ));
        items.push((
            2,
            item(
                lg(3, g.light),
                "wired",
                na(m.wired.value.map(|_| seg[3])),
                "mem.wired",
            ),
        ));
        items.push((3, item(lg(4, g.cache), "cache", na(m.cached.value), "mem.cache")));
        items.push((1, item("", "free", na(m.free.value), "mem.free")));
        items.push((4, item("", "avail", na(m.available.value), "mem.free")));
    }
    let base = width_of(&spans);
    let total = |items: &[(u8, Vec<Span<'a>>)]| items.iter().map(|(_, s)| width_of(s)).sum::<usize>();
    while !items.is_empty() && base + total(&items) > w {
        let worst = items
            .iter()
            .enumerate()
            .max_by_key(|(i, (p, _))| (*p, *i))
            .map(|(i, _)| i)
            .unwrap_or(0);
        items.remove(worst);
    }
    for (_, s) in items {
        spans.extend(s);
    }
    Line::from(spans)
}

/// Swap use (% of its limit) marked as a warning / critical in the swap row.
const SWAP_WARN_PCT: f64 = 80.0;
const SWAP_CRIT_PCT: f64 = 95.0;

/// Swap row (+ GPU/CPU in standard layout). Returns the line and the x where the GPU part starts.
fn swap_line<'a>(c: &Ctx, w: usize, meters: bool) -> (Line<'a>, Option<usize>) {
    let s = &c.app.snapshot;
    let m = &s.memory;
    let f = &c.app.fmt;
    let mut spans = vec![
        Span::styled(" SWAP ".to_string(), c.t("text.label")),
        Span::styled(
            format!(
                "{} / {}",
                f.opt_bytes(m.swap_used.value),
                f.opt_bytes(m.swap_total.value)
            ),
            c.t("mem.swap"),
        ),
    ];
    // Swap is colored by state, not by category: a near-full swap gets a glyph and a word (UX §8). The limit
    // is swap_total for fixed-size swap, and the swap volume ceiling where swap grows on demand (macOS).
    let limit = if s.host.os == oomtop_core::OsKind::Macos {
        m.swap_limit.value
    } else {
        m.swap_total.value
    };
    if let (Some(u), Some(l)) = (m.swap_used.value, limit.filter(|l| *l > 0)) {
        let pct = u as f64 / l as f64 * 100.0;
        if pct >= SWAP_WARN_PCT {
            let (glyph, tok) = if pct >= SWAP_CRIT_PCT {
                (c.g.crit, "state.crit")
            } else {
                (c.g.warn, "state.warn")
            };
            spans.push(Span::styled(format!("  {glyph} {pct:.0}% full"), c.t(tok)));
        }
    }
    let spark_w = if c.class == LayoutClass::Wide { 10 } else { 6 };
    let spark = sparkline_u64(&c.app.swap_series, spark_w, c.app.sparklines, c.g);
    // Where the (decorative) sparkline sits: it is the first thing dropped when the row runs out of width.
    let mut spark_at = None;
    if !spark.is_empty() {
        spark_at = Some(spans.len());
        spans.push(Span::raw("  "));
        spans.push(Span::styled(spark, c.t("chart.spark")));
    }
    // Net swap growth needs both rates: an unavailable rate is not zero.
    let growth = match (m.swap_out_per_min.value, m.swap_in_per_min.value) {
        (Some(o), Some(i)) => o as i64 - i as i64,
        _ => 0,
    };
    if growth != 0 {
        let (tok, sign, glyph) = if growth > 0 {
            ("state.warn", "+", c.g.warn)
        } else {
            ("state.ok", "-", c.g.ok)
        };
        spans.push(Span::styled(
            format!("  {glyph} {sign}{}/min", f.bytes(growth.unsigned_abs())),
            c.t(tok),
        ));
    }
    if let Some(fc) = &s.oom.forecast {
        let what = match fc.target {
            oomtop_core::ForecastTarget::SwapExhaustion => "swap full",
            oomtop_core::ForecastTarget::AvailableExhaustion => "out of memory",
            oomtop_core::ForecastTarget::KillerThreshold => "OOM killer",
        };
        spans.push(Span::styled(
            format!("  {} {what} in ~{}", c.g.crit, f.duration(fc.eta_s)),
            c.t("state.crit"),
        ));
    } else if c.class == LayoutClass::Wide {
        spans.push(Span::styled("  forecast stable".to_string(), c.t("ui.muted")));
    }
    let h = &c.app.headroom;
    let (room, tok) = match h.headroom {
        Some(x) if x >= 0 => (format!("headroom {}", f.signed(x)), "text.number"),
        Some(x) => (format!("{} headroom {}", c.g.warn, f.signed(x)), "state.warn"),
        None => ("headroom n/a".to_string(), "state.stale"),
    };
    spans.push(Span::raw("   "));
    spans.push(Span::styled(room, c.t(tok)));
    if let Some(psi) = m.psi.value {
        spans.push(Span::styled(
            format!("   PSI mem {:.0}%", psi.some_avg10),
            c.t(if psi.some_avg10 > 20.0 {
                "state.warn"
            } else {
                "text.number"
            }),
        ));
    }
    let mut gpu_x = None;
    // Standard layout: meters collapse into this one line (while throttling they get their own row).
    // With the meter block on screen CPU/GPU are already there.
    if !meters && c.class == LayoutClass::Standard && c.app.mode != oomtop_core::modes::Mode::Throttle {
        let used = width_of(&spans);
        let mut extra: Vec<Span> = Vec::new();
        if let Some(a) = s.accelerators.first() {
            extra.push(Span::styled("   GPU ".to_string(), c.t("text.label")));
            extra.push(Span::styled(
                a.util_pct
                    .value
                    .map(|u| format!("{u:.0}%"))
                    .unwrap_or_else(|| "n/a".into()),
                c.t("text.number"),
            ));
        }
        extra.push(Span::styled("  CPU ".to_string(), c.t("text.label")));
        extra.push(Span::styled(
            s.cpu
                .total_pct
                .value
                .map(|u| format!("{u:.0}%"))
                .unwrap_or_else(|| "n/a".into()),
            c.t("text.number"),
        ));
        if used + width_of(&extra) > w {
            if let Some(i) = spark_at {
                spans.drain(i..i + 2);
            }
        }
        let used = width_of(&spans);
        if used + width_of(&extra) <= w {
            gpu_x = Some(used);
            spans.extend(extra);
        }
    }
    (Line::from(spans), gpu_x)
}

/// Package power, busy cluster clocks and the hottest sensor (empty when nothing is known).
fn thermal_spans<'a>(c: &Ctx) -> Vec<Span<'a>> {
    let t = &c.app.snapshot.thermal;
    let mut spans = Vec::new();
    if let Some(pw) = t.package_power_w.value {
        spans.push(Span::styled(format!("   package {pw:.1} W"), c.t("text.number")));
    }
    for cl in &t.clusters {
        if cl.max_mhz > 0.0 && cl.active_pct >= oomtop_core::throttle::BUSY_PCT {
            spans.push(Span::styled(
                format!("   {} {:.0}/{:.0} MHz", cl.name, cl.cur_mhz, cl.max_mhz),
                c.t("text.number"),
            ));
        }
    }
    if let Some(hot) = t.temps.iter().filter(|s| s.celsius.is_finite()).max_by(|a, b| {
        a.celsius
            .partial_cmp(&b.celsius)
            .unwrap_or(std::cmp::Ordering::Equal)
    }) {
        spans.push(Span::styled(
            format!("   {} {:.0}°C", hot.name, hot.celsius),
            c.t("text.number"),
        ));
    }
    spans
}

fn accel_line<'a>(c: &Ctx, w: usize, thermal: bool, meters: bool) -> Option<Line<'a>> {
    let s = &c.app.snapshot;
    let f = &c.app.fmt;
    let mut spans: Vec<Span> = Vec::new();
    for a in &s.accelerators {
        let name = if s.accelerators.len() == 1 {
            "GPU".to_string()
        } else {
            format!("GPU {}", a.id)
        };
        spans.push(Span::styled(format!(" {name} "), c.t("text.label")));
        // With the meter block the GPU bar is already there; this row keeps the numbers.
        if !meters {
            spans.extend(mini_bar(c, a.util_pct.value, 8, "chart.bar.fill"));
        }
        spans.push(Span::styled(
            format!(
                "{}{}",
                if meters { "" } else { " " },
                a.util_pct
                    .value
                    .map(|u| format!("{u:.0}%"))
                    .unwrap_or_else(|| "n/a".into())
            ),
            c.t("text.number"),
        ));
        if let Some(pw) = a.power_w.value {
            spans.push(Span::styled(format!("  {pw:.1} W"), c.t("text.number")));
        }
        if let Some(used) = a.mem_used.value {
            let cap = a.gpu_budget.value.or(a.mem_total.value);
            let label = if a.gpu_budget.value.is_some() {
                " of budget "
            } else {
                " / "
            };
            spans.push(Span::styled(
                format!(
                    "  {}{}",
                    f.bytes(used),
                    cap.map(|b| format!("{label}{}", f.bytes(b))).unwrap_or_default()
                ),
                c.t("mem.gpu"),
            ));
        }
        if !a.throttle_reasons.is_empty() {
            spans.push(Span::styled(
                format!("  {} {}", c.g.warn, a.throttle_reasons.join(", ")),
                c.t("state.warn"),
            ));
        }
        spans.push(Span::raw("  "));
    }
    // The meter block already shows CPU per core; the aggregate mini-bar is only for the meter-less header.
    if !meters {
        let cpu = s.cpu.total_pct.value;
        spans.push(Span::styled(" CPU ".to_string(), c.t("text.label")));
        spans.extend(mini_bar(c, cpu, 8, "chart.bar.fill"));
        spans.push(Span::styled(
            format!(
                " {}",
                cpu.map(|u| format!("{u:.0}%")).unwrap_or_else(|| "n/a".into())
            ),
            c.t("text.number"),
        ));
    }

    if thermal {
        spans.extend(thermal_spans(c));
    }
    if spans.is_empty() {
        return None;
    }
    // clip gracefully: drop trailing spans that don't fit
    while width_of(&spans) > w && spans.len() > 1 {
        spans.pop();
    }
    Some(Line::from(spans))
}

//! Overlays over the body (UX §5.5, §7, §9, §12.8): details (standard/compact layouts), "why is this here?",
//! "why is it slow?", command answers, compare-two, the palette / command suggestions, help, and the full-screen
//! settings view over the config files. Overlays never steal a confirmation: prompts stay inline.

use super::views::detail_lines;
use super::{border_set, put, rjust, rule, Ctx};
use crate::app::Input;
use crate::layout::Regions;
use crate::settings::{SaveLayer, Section, SettingsState};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};

type L = Line<'static>;

fn sp(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

/// Draws a bordered box centered horizontally near the top of `area`; returns the inner rect.
fn boxed(c: &Ctx, buf: &mut Buffer, area: Rect, title: &str, want_w: u16, want_h: u16) -> Rect {
    let w = want_w.min(area.width).max(10.min(area.width));
    let h = want_h.min(area.height).max(3.min(area.height));
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 3;
    let r = Rect::new(x, y, w, h);
    Clear.render(r, buf);
    if c.p.paint_background {
        buf.set_style(r, c.p.surface());
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border_set(c.borders, c.g))
        .border_style(c.t("ui.border.focus"))
        .title(Line::from(sp(format!(" {title} "), c.t("ui.accent"))));
    let inner = block.inner(r);
    block.render(r, buf);
    inner
}

fn paragraph(c: &Ctx, buf: &mut Buffer, r: Rect, lines: Vec<L>, scroll: u16) {
    let _ = c;
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .render(r, buf);
}

/// Drops the padding `detail_lines` adds for the pane, so wrapped overlay text has no blank rows.
fn unpad(mut l: L) -> L {
    if let Some(last) = l.spans.last_mut() {
        let t = last.content.trim_end().to_string();
        last.content = t.into();
    }
    l
}

pub(crate) fn details(c: &Ctx, buf: &mut Buffer, area: Rect) {
    let w = area.width.saturating_sub(4).min(150);
    let lines: Vec<L> = detail_lines(c, 400).into_iter().map(unpad).collect();
    let inner_w = w.saturating_sub(2).max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.width().div_ceil(inner_w).max(1)).sum();
    let h = (rows as u16 + 2).min(area.height);
    let inner = boxed(c, buf, area, "Details · esc close", w, h);
    paragraph(c, buf, inner, lines, 0);
}

pub(crate) fn why_rank(c: &Ctx, buf: &mut Buffer, area: Rect) {
    let lines: Vec<L> = c
        .app
        .why_lines()
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            Line::from(sp(
                l,
                if i == 0 {
                    c.t("text.headline")
                } else {
                    c.t("ui.text")
                },
            ))
        })
        .collect();
    let w = area.width.saturating_sub(4).min(100);
    let h = (lines.len() as u16 + 4).min(area.height);
    let inner = boxed(c, buf, area, "Why is this here? · i/esc close", w, h);
    let mut lines = lines;
    lines.push(Line::from(sp(
        "Ranking only changes order and emphasis — nothing is hidden. m mutes, - shows less, p pins.",
        c.t("ui.faint"),
    )));
    paragraph(c, buf, inner, lines, 0);
}

pub(crate) fn why_slow(c: &Ctx, buf: &mut Buffer, area: Rect) {
    let app = c.app;
    let mut lines: Vec<L> = Vec::new();
    if app.causes.is_empty() {
        lines.push(Line::from(sp(
            "Nothing points at a slowdown right now: no memory pressure, swap storm or throttling under load.",
            c.t("state.ok"),
        )));
    }
    for (i, cause) in app.causes.iter().enumerate() {
        let (g, tok) = if cause.score >= 0.7 {
            (c.g.crit, "state.crit")
        } else if cause.score >= 0.4 {
            (c.g.warn, "state.warn")
        } else {
            (c.g.ok, "state.info")
        };
        lines.push(Line::from(vec![
            sp(format!("{}. {g} ", i + 1), c.t(tok)),
            sp(cause.title.clone(), c.t("text.headline")),
            sp(format!("  (score {:.2})", cause.score), c.t("ui.faint")),
        ]));
        for e in &cause.evidence {
            lines.push(Line::from(sp(format!("     {} {e}", c.g.sep), c.t("ui.text"))));
        }
        if let Some(fix) = &cause.fix {
            lines.push(Line::from(sp(format!("     fix: {fix}"), c.t("state.ok"))));
        }
    }
    let w = area.width.saturating_sub(4).min(110);
    let h = (lines.len() as u16 + 3).min(area.height);
    let inner = boxed(c, buf, area, "Why is it slow? · esc close", w, h);
    paragraph(c, buf, inner, lines, 0);
}

pub(crate) fn answer(c: &Ctx, buf: &mut Buffer, area: Rect, title: &str, lines: &[String]) {
    let lines: Vec<L> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            Line::from(sp(
                l.clone(),
                if i == 0 {
                    c.t("text.headline")
                } else {
                    c.t("ui.text")
                },
            ))
        })
        .collect();
    let w = area.width.saturating_sub(4).min(100);
    let h = (lines.len() as u16 + 3).min(area.height);
    let inner = boxed(c, buf, area, &format!("{title} · esc close"), w, h);
    paragraph(c, buf, inner, lines, 0);
}

pub(crate) fn compare(c: &Ctx, buf: &mut Buffer, area: Rect) {
    let app = c.app;
    let f = &app.fmt;
    let Some((a, Some(b))) = &app.compare else { return };
    let (Some(ga), Some(gb)) = (app.group_for_key(a), app.group_for_key(b)) else {
        return;
    };
    let colw = 22usize;
    let w = (18 + colw * 2 + 14) as u16;
    let rows: Vec<(&str, String, String, Option<f64>)> = {
        let byt = |m: &oomtop_core::Measured<u64>| match m.value {
            Some(_) => f.measured_bytes(m, c.g.ascii),
            None => "n/a".to_string(),
        };
        let num = |m: &oomtop_core::Measured<u64>| m.value.map(|v| v as f64);
        vec![
            (
                "kind",
                ga.kind.alias().to_string(),
                gb.kind.alias().to_string(),
                None,
            ),
            (
                "footprint",
                byt(&ga.totals.footprint),
                byt(&gb.totals.footprint),
                num(&ga.totals.footprint)
                    .zip(num(&gb.totals.footprint))
                    .map(|(x, y)| y - x),
            ),
            (
                "resident",
                byt(&ga.totals.resident),
                byt(&gb.totals.resident),
                num(&ga.totals.resident)
                    .zip(num(&gb.totals.resident))
                    .map(|(x, y)| y - x),
            ),
            ("gpu", byt(&ga.totals.gpu), byt(&gb.totals.gpu), None),
            ("swapped", byt(&ga.totals.swapped), byt(&gb.totals.swapped), None),
            (
                "cpu",
                f.opt_cpu(ga.totals.cpu_pct.value),
                f.opt_cpu(gb.totals.cpu_pct.value),
                None,
            ),
            (
                "processes",
                ga.totals.process_count.to_string(),
                gb.totals.process_count.to_string(),
                None,
            ),
            (
                "idle",
                ga.idle_for_s
                    .map(|s| f.duration(s))
                    .unwrap_or_else(|| "active".into()),
                gb.idle_for_s
                    .map(|s| f.duration(s))
                    .unwrap_or_else(|| "active".into()),
                None,
            ),
            (
                "reclaim",
                byt(&ga.reclaim_gain),
                byt(&gb.reclaim_gain),
                num(&ga.reclaim_gain)
                    .zip(num(&gb.reclaim_gain))
                    .map(|(x, y)| y - x),
            ),
            ("flags", flags(ga), flags(gb), None),
        ]
    };
    let h = rows.len() as u16 + 5;
    let inner = boxed(c, buf, area, "Compare · c/esc close", w, h);
    let mut lines: Vec<L> = vec![Line::from(vec![
        sp(c.fit("", 14), Style::default()),
        sp(
            format!("{} ", c.fit(&app.label(ga), colw - 1)),
            c.t("text.headline"),
        ),
        sp(
            format!("{} ", c.fit(&app.label(gb), colw - 1)),
            c.t("text.headline"),
        ),
        sp("difference", c.t("text.label")),
    ])];
    for (label, x, y, d) in rows {
        let diff = d
            .map(|d| {
                let s = f.bytes(d.abs() as u64);
                if d >= 0.0 {
                    format!("+{s}")
                } else {
                    format!("-{s}")
                }
            })
            .unwrap_or_default();
        lines.push(Line::from(vec![
            sp(c.fit(label, 14), c.t("text.label")),
            sp(format!("{} ", c.fit(&x, colw - 1)), c.t("text.number")),
            sp(format!("{} ", c.fit(&y, colw - 1)), c.t("text.number")),
            sp(diff, c.t("ui.muted")),
        ]));
    }
    // sparklines side by side
    let sa = app.sparks.get(&ga.id).cloned().unwrap_or_default();
    let sb = app.sparks.get(&gb.id).cloned().unwrap_or_default();
    lines.push(Line::from(vec![
        sp(c.fit("history", 14), c.t("text.label")),
        sp(
            c.fit(
                &super::widgets::sparkline_u64(&sa, colw - 2, app.sparklines, c.g),
                colw,
            ),
            c.t("chart.spark"),
        ),
        sp(
            c.fit(
                &super::widgets::sparkline_u64(&sb, colw - 2, app.sparklines, c.g),
                colw,
            ),
            c.t("chart.spark"),
        ),
    ]));
    paragraph(c, buf, inner, lines, 0);
}

fn flags(g: &oomtop_core::Group) -> String {
    let mut v = Vec::new();
    if g.orphan {
        v.push("orphan");
    }
    if g.lower_bound {
        v.push("lower bound");
    }
    if g.protected {
        v.push("protected");
    }
    if oomtop_core::headroom::is_reclaim_candidate(g) {
        v.push("reclaimable");
    }
    if v.is_empty() {
        "-".into()
    } else {
        v.join(", ")
    }
}

pub(crate) fn palette(c: &Ctx, buf: &mut Buffer, area: Rect) {
    let app = c.app;
    let (title, prefix) = match &app.input {
        Some(Input::Command(_)) => ("Command", ":"),
        _ => ("Palette · search, filter, commands", "> "),
    };
    let w = area.width.saturating_sub(4).min(90);
    let n = app.palette.len().max(1) as u16;
    let inner = boxed(c, buf, area, title, w, n + 4);
    let buffer = app
        .input
        .as_ref()
        .map(|i| i.buffer().to_string())
        .unwrap_or_default();
    let cursor = if c.g.ascii { "_" } else { "▏" };
    put(
        buf,
        inner,
        inner.y,
        &Line::from(vec![
            sp(prefix, c.t("text.key")),
            sp(buffer, c.t("ui.accent")),
            sp(cursor, c.t("ui.accent")),
        ]),
    );
    if app.palette.is_empty() {
        put(
            buf,
            inner,
            inner.y + 1,
            &Line::from(sp(
                "type a name, a filter (mem>2G kind:daemon) or words (gpu hogs, why slow, can I load 13g)",
                c.t("ui.faint"),
            )),
        );
        return;
    }
    let iw = inner.width as usize;
    for (i, s) in app.palette.iter().enumerate() {
        let y = inner.y + 1 + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let sel = i == app.palette_sel;
        let label_w = (iw * 3 / 5).max(10);
        let mut line = Line::from(vec![
            sp(
                if sel {
                    format!("{} ", c.g.select)
                } else {
                    "  ".into()
                },
                c.t("ui.accent"),
            ),
            sp(c.fit(&s.label, label_w), c.t("ui.text")),
            sp(c.fit(&s.detail, iw.saturating_sub(label_w + 2)), c.t("ui.muted")),
        ]);
        if sel {
            line = line.style(c.t("ui.selection"));
        }
        put(buf, inner, y, &line);
    }
}

pub(crate) fn help(c: &Ctx, buf: &mut Buffer, area: Rect) {
    let app = c.app;
    let mut lines: Vec<L> = Vec::new();
    let pairs: Vec<(String, String)> = if app.help_lines_cache.is_empty() {
        crate::help_pairs(&oomtop_config::keymap::preset("default").unwrap_or_default())
    } else {
        app.help_lines_cache.clone()
    };
    lines.push(Line::from(sp("Keys (from your keymap)", c.t("text.headline"))));
    for (keys, desc) in &pairs {
        lines.push(Line::from(vec![
            sp(format!("  {}", c.fit(keys, 22)), c.t("text.key")),
            sp(desc.clone(), c.t("ui.text")),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(sp(
        "Glyphs (meaning never by color alone)",
        c.t("text.headline"),
    )));
    let g = c.g;
    for (glyph, text) in [
        (g.ok, "ok / reclaimable"),
        (g.warn, "warning (pressure, orphan, suspended)"),
        (g.crit, "critical (OOM forecast, heavy thermal)"),
        (g.select, "selected row"),
        (g.moved, "row moved this refresh (shown once, no animation)"),
        (g.pin, "pinned (always in Your things)"),
        (g.lower, "lower bound (VM footprint under-counts)"),
        (g.approx, "estimate"),
    ] {
        lines.push(Line::from(vec![
            sp(format!("  {glyph}  "), c.t("text.key")),
            sp(text, c.t("ui.text")),
        ]));
    }
    lines.push(Line::from(sp(
        "  n/a  unavailable (never shown as zero) — details say why",
        c.t("ui.text"),
    )));
    lines.push(Line::default());
    lines.push(Line::from(sp("Filters", c.t("text.headline"))));
    for l in [
        "  mem>2G kind:daemon idle>30m · gpu>1G · owner:\"Claude Code\" · sandbox:* · state:orphan",
        "  space = and · or · -term / not term · (groups) · key:a,b = a or b",
        "  words work too: gpu hogs · what's eating memory · why slow · can I load 13g · idle stuff",
        "  : commands: :reclaim :headroom 13G :why :pin NAME :mode pressure|auto :sort cpu :save NAME",
    ] {
        lines.push(Line::from(sp(l, c.t("ui.text"))));
    }
    lines.push(Line::default());
    lines.push(Line::from(sp("Safety", c.t("text.headline"))));
    for l in [
        "  x/z always confirm inline and name the target and its estimated gain; nothing is sent before y.",
        "  Stop sends SIGTERM to the group root. SIGKILL only if it ignored SIGTERM and you confirm again.",
        "  (pid, start time) is re-checked right before any signal. Protected: pid 1, system UI, oomtop,",
        "  its terminal and shell, other users' processes, and protected.names from config.",
        "  Suspend (SIGSTOP) is CPU/thermal relief only — it frees no memory and is never part of reclaim.",
    ] {
        lines.push(Line::from(sp(l, c.t("ui.text"))));
    }
    let limited: Vec<String> = app
        .snapshot
        .source_status
        .iter()
        .filter_map(|(k, v)| match v {
            oomtop_core::SourceStatus::Available => None,
            oomtop_core::SourceStatus::Partial(r) => Some(format!("  {k}: partial — {r}")),
            oomtop_core::SourceStatus::Unavailable(r) => Some(format!("  {k}: unavailable — {r}")),
        })
        .collect();
    if !limited.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(sp("Sources", c.t("text.headline"))));
        for l in limited {
            lines.push(Line::from(sp(l, c.t("ui.muted"))));
        }
    }
    let w = area.width.saturating_sub(2).min(110);
    let h = area.height;
    let inner = boxed(c, buf, area, "Help · j/k scroll · any key closes", w, h);
    let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
    paragraph(c, buf, inner, lines, (app.help_scroll as u16).min(max_scroll));
}

/// Full-screen settings (UX §12.8): sections, effective value, origin, where else it's set, live preview.
pub(crate) fn settings_screen(
    c: &Ctx,
    buf: &mut Buffer,
    area: Rect,
    st: &SettingsState,
    regions: &mut Regions,
) {
    let w = area.width as usize;
    let mut y = area.y;
    put(
        buf,
        area,
        y,
        &rule(
            c,
            "Settings",
            "a view over your config files · changes preview live",
            w,
        ),
    );
    y += 1;
    let show_picker = c.class == crate::layout::LayoutClass::Wide
        && st.row().map(|r| r.key == "appearance.theme").unwrap_or(false);
    let picker_w: usize = if show_picker { 34 } else { 0 };
    let list_w = w.saturating_sub(picker_w);
    let footer_h = 4u16;
    let list_h = (area.y + area.height).saturating_sub(y + footer_h) as usize;
    let rows = &st.model.rows;
    let off = crate::layout::scroll_offset(0, st.selected, list_h, rows.len());
    // Longest section title ("Personalization", 15) + a leading and a trailing space.
    let sec_w = 17usize;
    let key_w = 30usize;
    let val_w = 20usize;
    let origin_w = list_w.saturating_sub(sec_w + key_w + val_w + 4);
    let mut last_section: Option<Section> = rows
        .get(off.saturating_sub(1))
        .map(|r| r.section)
        .filter(|_| off > 0);
    for (k, i) in (off..rows.len().min(off + list_h)).enumerate() {
        let r = &rows[i];
        let sel = i == st.selected;
        let section = if last_section != Some(r.section) {
            r.section.title()
        } else {
            ""
        };
        last_section = Some(r.section);
        let value = match (&st.editing, sel) {
            (Some(buf_text), true) => format!("{buf_text}{}", if c.g.ascii { "_" } else { "▏" }),
            _ => r.value.clone(),
        };
        let origin = if r.also_in.is_empty() {
            r.origin.clone()
        } else {
            format!("{} (also in {})", r.origin, r.also_in.join(", "))
        };
        let changeable = if r.choices.is_empty() {
            " "
        } else if c.g.ascii {
            "<>"
        } else {
            "‹›"
        };
        let mut line = Line::from(vec![
            sp(format!(" {}", c.fit(section, sec_w - 1)), c.t("text.label")),
            sp(c.fit(&r.key, key_w), c.t("ui.text")),
            sp(format!("{} ", c.fit(changeable, 2)), c.t("ui.faint")),
            sp(
                c.fit(&value, val_w),
                if r.origin.starts_with("runtime") {
                    c.t("ui.accent")
                } else {
                    c.t("text.number")
                },
            ),
            sp(format!(" {}", c.fit(&origin, origin_w)), c.t("ui.muted")),
        ]);
        if sel {
            line = line.style(c.t("ui.selection"));
        }
        let yy = area.y + 1 + k as u16;
        put(buf, Rect::new(area.x, yy, list_w as u16, 1), yy, &line);
        y = yy + 1;
    }
    regions.settings = Some((
        Rect::new(area.x, area.y + 1, list_w as u16, y.saturating_sub(area.y + 1)),
        off,
    ));
    if show_picker {
        let px = area.x + list_w as u16;
        let pr = Rect::new(px, area.y + 1, picker_w as u16, list_h as u16);
        let cur = st.row().map(|r| r.value.clone()).unwrap_or_default();
        put(
            buf,
            pr,
            pr.y,
            &Line::from(sp(c.fit(" Themes (live preview)", picker_w), c.t("text.label"))),
        );
        for (i, t) in st.model.themes.iter().enumerate() {
            let yy = pr.y + 1 + i as u16;
            if yy >= pr.y + pr.height {
                break;
            }
            let mark = if t.name == cur { c.g.select } else { " " };
            let line = Line::from(vec![
                sp(format!(" {mark} "), c.t("ui.accent")),
                sp(c.fit(&t.name, 18), c.t("ui.text")),
                sp(
                    rjust(&t.badge, 10),
                    if t.badge == "ok" {
                        c.t("state.ok")
                    } else {
                        c.t("state.warn")
                    },
                ),
            ]);
            put(buf, pr, yy, &line);
        }
    }
    // footer: doc, save target, keys, message
    let fy = area.y + area.height - footer_h;
    let doc = st
        .row()
        .map(|r| {
            format!(
                " {} — {}",
                r.key,
                if r.doc.is_empty() {
                    "(no description)"
                } else {
                    &r.doc
                }
            )
        })
        .unwrap_or_default();
    put(buf, area, fy, &Line::from(sp(c.fit(&doc, w), c.t("ui.text"))));
    let target = st
        .model
        .layer_paths
        .iter()
        .find(|(l, _)| *l == st.layer)
        .map(|(_, p)| p.display().to_string())
        .unwrap_or_else(|| st.layer.label().to_string());
    let pending = if st.pending.is_empty() {
        "no unsaved changes".to_string()
    } else {
        format!(
            "{} unsaved change{}",
            st.pending.len(),
            if st.pending.len() == 1 { "" } else { "s" }
        )
    };
    let layer_word = match st.layer {
        SaveLayer::User => "user",
        SaveLayer::Host => "host",
        SaveLayer::Dropin => "drop-in",
    };
    put(
        buf,
        area,
        fy + 1,
        &Line::from(vec![
            sp(" save target ", c.t("text.label")),
            sp(format!("{layer_word}: {target}"), c.t("text.link")),
            sp(format!("  {} {pending}", c.g.sep), c.t("ui.muted")),
        ]),
    );
    let keys = "←/→ change · enter edit/apply for session · s save · tab save target · e open file · t theme · esc revert";
    let keys = if c.g.ascii {
        keys.replace('·', "|").replace("←/→", "h/l")
    } else {
        keys.to_string()
    };
    put(
        buf,
        area,
        fy + 2,
        &Line::from(sp(c.fit(&format!(" {keys}"), w), c.t("text.key"))),
    );
    if let Some(m) = &st.message {
        put(
            buf,
            area,
            fy + 3,
            &Line::from(sp(c.fit(&format!(" {m}"), w), c.t("state.info"))),
        );
    }
}

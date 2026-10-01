//! Body views (UX §2, SPEC §12.1): 1 Home (Your things + ranked groups with nested members, "because" hints,
//! details pane in wide layouts) · 2 Processes · 3 Models · 4 Sandboxes · 5 Reclaim · 6 Timeline, focus mode,
//! the message line (input, confirmation hint, toast, status) and the footer tabs.

use super::widgets::{column_chart, sparkline, sparkline_u64};
use super::{put, rjust, rule, Ctx};
use crate::app::{Input, Row, RowKey, View};
use crate::layout::{scroll_offset, HitTarget, LayoutClass, Regions};
use oomtop_core::modes::Mode;
use oomtop_core::units::format_duration;
use oomtop_core::{Group, GroupKind, ProcState, Process, Quality};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

type L = Line<'static>;

fn sp(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

fn kind_token(k: GroupKind) -> &'static str {
    match k {
        GroupKind::AgentSession => "kind.agent",
        GroupKind::ModelServer => "kind.model",
        GroupKind::Sandbox => "kind.sandbox",
        GroupKind::BuildDaemon => "kind.daemon",
        GroupKind::App => "kind.app",
        GroupKind::System => "kind.system",
        GroupKind::Other => "kind.other",
    }
}

/// Pads the spans to `w` cells and applies the selection style to the whole row.
fn finish(c: &Ctx, mut spans: Vec<Span<'static>>, w: usize, selected: bool) -> L {
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    if used < w {
        spans.push(Span::raw(" ".repeat(w - used)));
    }
    let line = Line::from(spans);
    if selected {
        line.style(c.t("ui.selection"))
    } else {
        line
    }
}

fn marker(c: &Ctx, row: &Row, selected: bool) -> Span<'static> {
    let g = c.g;
    let m = if selected {
        g.select
    } else if row.moved {
        g.moved
    } else {
        " "
    };
    sp(format!(" {m} "), c.t("ui.accent"))
}

fn mem_text(c: &Ctx, g: &Group) -> String {
    let v = c.app.fmt.opt_bytes(g.totals.footprint.value);
    if g.lower_bound {
        format!("{}{v}", c.g.lower)
    } else {
        v
    }
}

fn group_state(c: &Ctx, g: &Group) -> (String, &'static str) {
    let app = c.app;
    if let Some(m) = app
        .snapshot
        .model_servers
        .iter()
        .find(|m| m.group_id.as_deref() == Some(g.id.as_str()))
    {
        if m.busy.value == Some(true) {
            let p = m
                .progress
                .as_ref()
                .map(|p| {
                    format!(
                        "{} {}/{}",
                        if p.label.is_empty() { "busy" } else { &p.label },
                        p.done,
                        p.total
                    )
                })
                .unwrap_or_else(|| "busy".into());
            return (p, "state.info");
        }
    }
    if g.root
        .and_then(|r| app.process(r))
        .map(|p| p.state == ProcState::Stopped)
        .unwrap_or(false)
    {
        return (format!("{} suspended", c.g.warn), "state.warn");
    }
    if g.orphan {
        return (format!("{} orphan", c.g.warn), "state.warn");
    }
    if g.idle {
        return (
            format!(
                "idle {}",
                g.idle_for_s.map(|s| app.fmt.duration(s)).unwrap_or_default()
            ),
            "ui.muted",
        );
    }
    (String::new(), "ui.muted")
}

fn group_tag(c: &Ctx, g: &Group) -> (String, &'static str) {
    if g.is_self {
        ("this".into(), "ui.muted")
    } else if g.protected || g.kind == GroupKind::System {
        ("protected".into(), "ui.muted")
    } else if oomtop_core::headroom::is_reclaim_candidate(g) {
        (format!("{} reclaimable", c.g.ok), "state.ok")
    } else if g.kind == GroupKind::Sandbox {
        ("sandbox".into(), "kind.sandbox")
    } else {
        (String::new(), "ui.muted")
    }
}

fn group_name(c: &Ctx, g: &Group) -> String {
    let mut name = String::new();
    if c.app.pinned.contains(&g.fingerprint) {
        name.push_str(c.g.pin);
        name.push(' ');
    }
    name.push_str(&c.app.label(g));
    if g.totals.process_count > 1 {
        name.push_str(&format!(" ({})", g.totals.process_count));
    }
    name
}

/// Column widths for group rows (each includes its leading space). Columns are dropped by priority until the
/// name gets a readable width: because → kind → sparkline → cpu → tag → state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GroupCols {
    pub kind: usize,
    pub state: usize,
    pub mem: usize,
    pub spark: usize,
    pub cpu: usize,
    pub tag: usize,
    pub because: usize,
    pub name: usize,
}

pub(crate) fn group_cols(class: LayoutClass, w: usize) -> GroupCols {
    let wide = class == LayoutClass::Wide;
    let mut c = GroupCols {
        kind: 8,
        state: 15,
        mem: 9,
        spark: if wide { 14 } else { 8 },
        cpu: 7,
        tag: 15,
        because: if wide { 40 } else { 0 },
        name: 0,
    };
    let min_name = if wide { 30 } else { 22 };
    let fixed = |c: &GroupCols| 3 + c.kind + c.state + c.mem + c.spark + c.cpu + c.tag + c.because;
    for step in 0..6 {
        if w.saturating_sub(fixed(&c)) >= min_name {
            break;
        }
        match step {
            0 => c.because = 0,
            1 => c.kind = 0,
            2 => c.spark = 0,
            3 => c.cpu = 0,
            4 => c.tag = 0,
            _ => c.state = 0,
        }
    }
    c.name = w.saturating_sub(fixed(&c)).max(10);
    // Very wide terminals: cap the name and give the rest to the "because" hint.
    if wide && c.because > 0 && c.name > 56 {
        c.because += c.name - 56;
        c.name = 56;
    }
    c
}

// -------------------------------------------------------------------------------------------------------
// Custom columns (UX §12.6: `[columns.groups]`, `[columns.processes]`, `[[columns.custom]]`)
// -------------------------------------------------------------------------------------------------------

/// One cell: text, style, right-aligned.
type Cell = (String, Style, bool);

fn custom_cell(c: &Ctx, id: &str, fields: &oomtop_config::layout::Fields) -> Option<Cell> {
    let col = c.app.layout.as_ref()?.custom(id)?;
    Some((col.render(fields), c.t("text.number"), col.align_right))
}

fn group_cell(c: &Ctx, g: &Group, id: &str) -> Cell {
    let app = c.app;
    let f = &app.fmt;
    let num = c.t("text.number");
    match id {
        "name" => {
            let style = if app.muted.contains(&g.fingerprint) {
                c.t("ui.muted")
            } else {
                c.t(kind_token(g.kind))
            };
            (group_name(c, g), style, false)
        }
        "kind" => (g.kind.alias().to_string(), c.t("text.label"), false),
        "footprint" => {
            let style = if app.changed.contains(&g.id) {
                num.add_modifier(Modifier::BOLD)
            } else {
                num
            };
            (mem_text(c, g), style, true)
        }
        "resident" => (f.measured_bytes(&g.totals.resident, c.g.ascii), num, true),
        "gpu" => (f.opt_bytes(g.totals.gpu.value), num, true),
        "swapped" => (f.opt_bytes(g.totals.swapped.value), num, true),
        "cpu" => (f.opt_cpu(g.totals.cpu_pct.value), num, true),
        "trend" => (String::new(), c.t("chart.spark"), false),
        "state" => {
            let (text, tok) = group_state(c, g);
            (text, c.t(tok), false)
        }
        "idle" => (
            g.idle_for_s
                .filter(|_| g.idle)
                .map(|s| f.duration(s))
                .unwrap_or_default(),
            c.t("ui.muted"),
            true,
        ),
        "members" => (g.totals.process_count.to_string(), num, true),
        "reclaim" => (
            if oomtop_core::headroom::is_reclaim_candidate(g) {
                format!("{}{}", c.g.approx, f.opt_bytes(g.reclaim_gain.value))
            } else {
                String::new()
            },
            c.t("state.ok"),
            true,
        ),
        "confidence" => (
            format!("{:?}", g.confidence).to_lowercase(),
            c.t("ui.muted"),
            false,
        ),
        "because" => {
            let b = app.because(g);
            (
                if b.is_empty() { b } else { format!("because: {b}") },
                c.t("ui.muted"),
                false,
            )
        }
        other => custom_cell(c, other, &oomtop_config::layout::group_fields(g, &app.snapshot))
            .unwrap_or_else(|| ("?".into(), c.t("state.stale"), false)),
    }
}

/// Lays cells out in solved column widths; the first column has no leading space.
fn cells_line(
    c: &Ctx,
    marker: Span<'static>,
    cols: &[(String, usize)],
    cell: impl Fn(&str, usize) -> Cell,
) -> Vec<Span<'static>> {
    let mut spans = vec![marker];
    for (i, (id, width)) in cols.iter().enumerate() {
        let (text, style, right) = cell(id, *width);
        let inner = if i == 0 { *width } else { width.saturating_sub(1) };
        let body = if right && text.width() <= inner {
            rjust(&text, inner)
        } else {
            c.fit(&text, inner)
        };
        spans.push(sp(if i == 0 { body } else { format!(" {body}") }, style));
    }
    spans
}

fn custom_group_row(
    c: &Ctx,
    g: &Group,
    row: &Row,
    selected: bool,
    w: usize,
    set: &oomtop_config::layout::ColumnSet,
) -> L {
    let cols = crate::columns::solve(set, w, 3, 12);
    let spans = cells_line(c, marker(c, row, selected), &cols, |id, width| {
        if id == "trend" {
            let spark = c
                .app
                .sparks
                .get(&g.id)
                .map(|v| sparkline_u64(v, width.saturating_sub(2), c.app.sparklines, c.g))
                .unwrap_or_default();
            return (spark, c.t("chart.spark"), false);
        }
        group_cell(c, g, id)
    });
    finish(c, spans, w, selected)
}

/// Column titles for a custom column set (the line under the list rule).
fn custom_header(c: &Ctx, set: &oomtop_config::layout::ColumnSet, w: usize) -> L {
    let cols = crate::columns::solve(set, w, 3, 12);
    let layout = c.app.layout.as_ref();
    let spans = cells_line(c, sp("   ", Style::default()), &cols, |id, _| {
        let custom = layout.and_then(|l| l.custom(id));
        (
            crate::columns::title(id, custom),
            c.t("text.label"),
            crate::columns::right_aligned(id, custom),
        )
    });
    Line::from(spans)
}

/// Field values of a process for custom columns: the process's own numbers over its group's host fields.
fn process_fields(c: &Ctx, p: &Process) -> oomtop_config::layout::Fields {
    let s = &c.app.snapshot;
    let mut fields = match s.group_of(p.id) {
        Some(g) => oomtop_config::layout::group_fields(g, s),
        None => oomtop_config::layout::group_fields(&Group::default(), s),
    };
    let b = |m: &oomtop_core::Measured<u64>| m.value.map(|v| v as f64);
    fields.insert("footprint", b(&p.mem.footprint_or_pss));
    fields.insert("resident", b(&p.mem.resident));
    fields.insert("gpu", b(&p.mem.gpu));
    fields.insert("swapped", b(&p.mem.swapped));
    fields.insert("cpu", p.cpu_pct.value);
    fields.insert("members", Some(1.0));
    fields.insert("idle_s", p.idle_for_s.value.map(|v| v as f64));
    fields
}

fn process_cell(c: &Ctx, p: &Process, id: &str) -> Cell {
    let app = c.app;
    let f = &app.fmt;
    let num = c.t("text.number");
    let g = app.snapshot.group_of(p.id);
    match id {
        "pid" => (p.id.pid.to_string(), c.t("text.label"), true),
        "name" => (
            p.name.clone(),
            g.map(|g| c.t(kind_token(g.kind))).unwrap_or(c.t("ui.text")),
            false,
        ),
        "user" => (
            p.user
                .clone()
                .or_else(|| p.uid.map(|u| format!("uid {u}")))
                .unwrap_or_default(),
            c.t("ui.muted"),
            false,
        ),
        "group" => (
            g.map(|g| app.label(g)).unwrap_or_default(),
            c.t("ui.muted"),
            false,
        ),
        "footprint" => (f.measured_bytes(&p.mem.footprint_or_pss, c.g.ascii), num, true),
        "resident" => (f.opt_bytes(p.mem.resident.value), num, true),
        "gpu" => (f.opt_bytes(p.mem.gpu.value), num, true),
        "swapped" => (f.opt_bytes(p.mem.swapped.value), num, true),
        "cpu" => (f.opt_cpu(p.cpu_pct.value), num, true),
        "state" => (proc_state_word(p).to_string(), c.t("ui.muted"), false),
        "idle" => (
            p.idle_for_s
                .value
                .filter(|s| *s >= 60)
                .map(|s| f.duration(s))
                .unwrap_or_default(),
            c.t("ui.muted"),
            true,
        ),
        // No per-process history is kept; the column stays empty rather than inventing a trend.
        "trend" => (String::new(), c.t("chart.spark"), false),
        // Redacted on screen too (screen sharing, SPEC §13).
        "cmdline" => (
            oomtop_core::redact::redact_cmdline(&p.cmdline).join(" "),
            c.t("ui.muted"),
            false,
        ),
        other => custom_cell(c, other, &process_fields(c, p))
            .unwrap_or_else(|| ("?".into(), c.t("state.stale"), false)),
    }
}

fn group_row(c: &Ctx, g: &Group, row: &Row, selected: bool, w: usize) -> L {
    if let Some(set) = c.app.layout.as_ref().and_then(|l| l.group_columns()) {
        return custom_group_row(c, g, row, selected, w, set);
    }
    let app = c.app;
    let f = &app.fmt;
    let mem = mem_text(c, g);
    let mem_style = if app.changed.contains(&g.id) {
        c.t("text.number").add_modifier(Modifier::BOLD)
    } else {
        c.t("text.number")
    };
    let name_style = if app.muted.contains(&g.fingerprint) {
        c.t("ui.muted")
    } else {
        c.t(kind_token(g.kind))
    };
    let mut spans = vec![marker(c, row, selected)];
    if c.class == LayoutClass::Compact {
        // One metric column chosen by the current mode (UX §2).
        let metric = match app.mode {
            Mode::Throttle => f.opt_cpu(g.totals.cpu_pct.value),
            Mode::Working if g.totals.gpu.value.is_some() => f.opt_bytes(g.totals.gpu.value),
            _ => mem,
        };
        let (state, stok) = group_state(c, g);
        let state_w = if w >= 64 && !state.is_empty() { 12 } else { 0 };
        let (flag, ftok) = if g.orphan
            || g.root
                .and_then(|r| app.process(r))
                .map(|p| p.state == ProcState::Stopped)
                .unwrap_or(false)
        {
            (c.g.warn, "state.warn")
        } else if oomtop_core::headroom::is_reclaim_candidate(g) {
            (c.g.ok, "state.ok")
        } else {
            (" ", "ui.muted")
        };
        let name_w = w.saturating_sub(3 + 9 + 2 + state_w);
        spans.push(sp(c.fit(&group_name(c, g), name_w), name_style));
        spans.push(sp(format!(" {flag}"), c.t(ftok)));
        if state_w > 0 {
            spans.push(sp(format!(" {}", c.fit(&state, state_w - 1)), c.t(stok)));
        }
        spans.push(sp(rjust(&metric, 9), mem_style));
        return finish(c, spans, w, selected);
    }
    let cols = group_cols(c.class, w);
    spans.push(sp(c.fit(&group_name(c, g), cols.name), name_style));
    if cols.kind > 0 {
        spans.push(sp(
            format!(" {}", c.fit(g.kind.alias(), cols.kind - 1)),
            c.t("text.label"),
        ));
    }
    if cols.state > 0 {
        let (state, stok) = group_state(c, g);
        spans.push(sp(format!(" {}", c.fit(&state, cols.state - 1)), c.t(stok)));
    }
    spans.push(sp(format!(" {}", rjust(&mem, cols.mem - 1)), mem_style));
    if cols.spark > 0 {
        let spark = app
            .sparks
            .get(&g.id)
            .map(|v| sparkline_u64(v, cols.spark - 2, app.sparklines, c.g))
            .unwrap_or_default();
        spans.push(sp(
            format!("  {}", c.fit(&spark, cols.spark - 2)),
            c.t("chart.spark"),
        ));
    }
    if cols.cpu > 0 {
        spans.push(sp(
            format!(" {}", rjust(&f.opt_cpu(g.totals.cpu_pct.value), cols.cpu - 1)),
            c.t("text.number"),
        ));
    }
    if cols.tag > 0 {
        let (tag, ttok) = group_tag(c, g);
        spans.push(sp(format!("  {}", c.fit(&tag, cols.tag - 2)), c.t(ttok)));
    }
    if cols.because > 0 {
        let because = app.because(g);
        let text = if because.is_empty() {
            String::new()
        } else {
            format!("because: {because}")
        };
        spans.push(sp(
            format!("  {}", c.fit(&text, cols.because - 2)),
            c.t("ui.muted"),
        ));
    }
    finish(c, spans, w, selected)
}

fn member_row(c: &Ctx, p: &Process, last: bool, selected: bool, w: usize) -> L {
    let f = &c.app.fmt;
    let branch = if last { c.g.branch_last } else { c.g.branch };
    let mut spans = vec![sp(
        if selected {
            format!(" {} ", c.g.select)
        } else {
            "   ".into()
        },
        c.t("ui.accent"),
    )];
    let name = format!("{branch} {} · pid {}", p.name, p.id.pid);
    let state = match p.state {
        ProcState::Stopped => "suspended".to_string(),
        ProcState::Zombie => "zombie".to_string(),
        _ => p
            .idle_for_s
            .value
            .filter(|s| *s >= 60)
            .map(|s| format!("idle {}", f.duration(s)))
            .unwrap_or_default(),
    };
    match c.class {
        LayoutClass::Compact => {
            spans.push(sp(c.fit(&name, w.saturating_sub(12)), c.t("ui.text")));
            spans.push(sp(
                rjust(&f.opt_bytes(p.mem.footprint_or_pss.value), 9),
                c.t("text.number"),
            ));
        }
        class => {
            // Same columns as the group rows, so members line up under their group.
            let cols = group_cols(class, w);
            spans.push(sp(c.fit(&format!("  {name}"), cols.name), c.t("ui.text")));
            if cols.kind > 0 {
                spans.push(Span::raw(" ".repeat(cols.kind)));
            }
            if cols.state > 0 {
                spans.push(sp(format!(" {}", c.fit(&state, cols.state - 1)), c.t("ui.muted")));
            }
            spans.push(sp(
                format!(
                    " {}",
                    rjust(&f.opt_bytes(p.mem.footprint_or_pss.value), cols.mem - 1)
                ),
                c.t("text.number"),
            ));
            if cols.spark > 0 {
                spans.push(Span::raw(" ".repeat(cols.spark)));
            }
            if cols.cpu > 0 {
                spans.push(sp(
                    format!(" {}", rjust(&f.opt_cpu(p.cpu_pct.value), cols.cpu - 1)),
                    c.t("text.number"),
                ));
            }
        }
    }
    finish(c, spans, w, selected)
}

fn proc_header(c: &Ctx, w: usize) -> L {
    if let Some(set) = c.app.layout.as_ref().and_then(|l| l.process_columns()) {
        return custom_header(c, set, w);
    }
    let label = c.t("text.label");
    let s = match c.class {
        LayoutClass::Compact => format!(
            "   {:>7} {}{:>9}{:>6}",
            "PID",
            c.fit("NAME", w.saturating_sub(33)),
            "MEM",
            "CPU"
        ),
        LayoutClass::Standard => format!(
            "   {:>7} {} {} {:>8} {:>8} {:>6} {:<9}",
            "PID",
            c.fit("NAME", w.saturating_sub(3 + 8 + 21 + 9 + 9 + 7 + 10 + 1)),
            c.fit("GROUP", 20),
            "MEM",
            "RES",
            "CPU",
            "STATE"
        ),
        LayoutClass::Wide => format!(
            "   {:>7} {} {} {:>8} {:>8} {:>9} {:>6} {:<9} {:>7}",
            "PID",
            c.fit("NAME", w.saturating_sub(3 + 8 + 25 + 9 + 9 + 10 + 7 + 10 + 8 + 1)),
            c.fit("GROUP", 24),
            "MEM",
            "RES",
            "NON-RES",
            "CPU",
            "STATE",
            "IDLE"
        ),
    };
    Line::from(sp(c.fit(&s, w), label))
}

fn proc_state_word(p: &Process) -> &'static str {
    match p.state {
        ProcState::Running => "running",
        ProcState::Sleeping => "sleeping",
        ProcState::Idle => "idle",
        ProcState::Stopped => "suspended",
        ProcState::Zombie => "zombie",
        ProcState::Unknown => "-",
    }
}

fn proc_row(c: &Ctx, p: &Process, row: &Row, selected: bool, w: usize, tree: &str) -> L {
    if let Some(set) = c.app.layout.as_ref().and_then(|l| l.process_columns()) {
        let cols = crate::columns::solve(set, w, 3, 12);
        let spans = cells_line(c, marker(c, row, selected), &cols, |id, _| process_cell(c, p, id));
        return finish(c, spans, w, selected);
    }
    let app = c.app;
    let f = &app.fmt;
    let g = app.snapshot.group_of(p.id);
    let group = g.map(|g| app.label(g)).unwrap_or_default();
    let mut spans = vec![marker(c, row, selected)];
    let name = format!("{tree}{}", p.name);
    let fp = f.opt_bytes(p.mem.footprint_or_pss.value);
    let fp = if p.mem.footprint_or_pss.quality == Quality::Estimate {
        format!("{}{fp}", c.g.approx)
    } else {
        fp
    };
    spans.push(sp(format!("{:>7} ", p.id.pid), c.t("text.label")));
    let kind_style = g.map(|g| c.t(kind_token(g.kind))).unwrap_or(c.t("ui.text"));
    match c.class {
        LayoutClass::Compact => {
            spans.push(sp(c.fit(&name, w.saturating_sub(33)), kind_style));
            spans.push(sp(rjust(&fp, 9), c.t("text.number")));
            spans.push(sp(rjust(&f.opt_cpu(p.cpu_pct.value), 6), c.t("text.number")));
        }
        LayoutClass::Standard => {
            let name_w = w.saturating_sub(3 + 8 + 21 + 9 + 9 + 7 + 10 + 1);
            spans.push(sp(c.fit(&name, name_w), kind_style));
            spans.push(sp(format!(" {}", c.fit(&group, 20)), c.t("ui.muted")));
            spans.push(sp(format!(" {}", rjust(&fp, 8)), c.t("text.number")));
            spans.push(sp(
                format!(" {}", rjust(&f.opt_bytes(p.mem.resident.value), 8)),
                c.t("text.number"),
            ));
            spans.push(sp(
                format!(" {}", rjust(&f.opt_cpu(p.cpu_pct.value), 6)),
                c.t("text.number"),
            ));
            spans.push(sp(format!(" {:<9}", proc_state_word(p)), c.t("ui.muted")));
        }
        LayoutClass::Wide => {
            let name_w = w.saturating_sub(3 + 8 + 25 + 9 + 9 + 10 + 7 + 10 + 8 + 1);
            spans.push(sp(c.fit(&name, name_w), kind_style));
            spans.push(sp(format!(" {}", c.fit(&group, 24)), c.t("ui.muted")));
            spans.push(sp(format!(" {}", rjust(&fp, 8)), c.t("text.number")));
            spans.push(sp(
                format!(" {}", rjust(&f.opt_bytes(p.mem.resident.value), 8)),
                c.t("text.number"),
            ));
            let nr = p
                .mem
                .non_resident_est
                .value
                .map(|v| format!("{}{}", c.g.approx, f.bytes(v)))
                .unwrap_or_else(|| "n/a".into());
            spans.push(sp(format!(" {}", rjust(&nr, 9)), c.t("text.number")));
            spans.push(sp(
                format!(" {}", rjust(&f.opt_cpu(p.cpu_pct.value), 6)),
                c.t("text.number"),
            ));
            spans.push(sp(format!(" {:<9}", proc_state_word(p)), c.t("ui.muted")));
            let idle = p
                .idle_for_s
                .value
                .filter(|s| *s >= 60)
                .map(|s| f.duration(s))
                .unwrap_or_default();
            spans.push(sp(format!(" {}", rjust(&idle, 7)), c.t("ui.muted")));
        }
    }
    finish(c, spans, w, selected)
}

/// A table column for priority-based dropping (UX §2, §8): whole low-priority columns go first when the
/// width runs out, so no value is ever clipped mid-number and units stay visible. `width == 0` marks the one
/// flexible (name) column, which takes the rest (at least [`FLEX_MIN`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Column {
    pub id: &'static str,
    pub header: &'static str,
    pub width: usize,
    pub right: bool,
    /// 0 = never dropped; higher numbers are dropped first.
    pub prio: u8,
}

const FLEX_MIN: usize = 10;
/// The row marker ("▸ ") before the first column.
const MARKER_W: usize = 3;

pub(crate) const MODEL_COLS: &[Column] = &[
    Column {
        id: "kind",
        header: "KIND",
        width: 9,
        right: false,
        prio: 0,
    },
    Column {
        id: "endpoint",
        header: "ENDPOINT",
        width: 25,
        right: false,
        prio: 6,
    },
    Column {
        id: "models",
        header: "MODELS",
        width: 0,
        right: false,
        prio: 0,
    },
    Column {
        id: "weights",
        header: "WEIGHTS",
        width: 8,
        right: true,
        prio: 3,
    },
    Column {
        id: "kv",
        header: "KV",
        width: 8,
        right: true,
        prio: 5,
    },
    Column {
        id: "device",
        header: "DEVICE",
        width: 7,
        right: false,
        prio: 4,
    },
    Column {
        id: "speed",
        header: "SPEED",
        width: 12,
        right: true,
        prio: 2,
    },
    Column {
        id: "job",
        header: "JOB",
        width: 14,
        right: false,
        prio: 1,
    },
    Column {
        id: "mem",
        header: "MEM",
        width: 8,
        right: true,
        prio: 0,
    },
];

pub(crate) const SANDBOX_COLS: &[Column] = &[
    Column {
        id: "kind",
        header: "KIND",
        width: 14,
        right: false,
        prio: 3,
    },
    Column {
        id: "runtime",
        header: "RUNTIME",
        width: 25,
        right: false,
        prio: 5,
    },
    Column {
        id: "label",
        header: "LABEL",
        width: 0,
        right: false,
        prio: 0,
    },
    Column {
        id: "host",
        header: "HOST",
        width: 9,
        right: true,
        prio: 0,
    },
    Column {
        id: "config",
        header: "CONFIG",
        width: 8,
        right: true,
        prio: 4,
    },
    Column {
        id: "guest",
        header: "GUEST",
        width: 8,
        right: true,
        prio: 1,
    },
    Column {
        id: "started_by",
        header: "STARTED BY",
        width: 21,
        right: false,
        prio: 2,
    },
];

/// The columns that fit in `w` (in table order) and the flexible column's width.
pub(crate) fn fit_columns(all: &[Column], w: usize) -> (Vec<Column>, usize) {
    let mut cols: Vec<Column> = all.to_vec();
    let need = |cols: &[Column]| {
        MARKER_W
            + cols
                .iter()
                .map(|c| if c.width == 0 { FLEX_MIN } else { c.width })
                .sum::<usize>()
            + cols.len().saturating_sub(1)
    };
    while need(&cols) > w {
        let Some(worst) = cols
            .iter()
            .enumerate()
            .filter(|(_, c)| c.prio > 0)
            .max_by_key(|(_, c)| c.prio)
            .map(|(i, _)| i)
        else {
            break;
        };
        cols.remove(worst);
    }
    let flex = FLEX_MIN + w.saturating_sub(need(&cols));
    (cols, flex)
}

/// One cell: a leading space (except the first column), fitted or right-aligned to the column width.
fn column_cell(c: &Ctx, col: &Column, text: &str, flex: usize, i: usize) -> String {
    let width = if col.width == 0 { flex } else { col.width };
    let cell = if col.right && text.width() <= width {
        rjust(text, width)
    } else {
        c.fit(text, width)
    };
    if i == 0 {
        cell
    } else {
        format!(" {cell}")
    }
}

fn header_text(c: &Ctx, all: &[Column], w: usize) -> String {
    let (cols, flex) = fit_columns(all, w);
    let mut s = " ".repeat(MARKER_W);
    for (i, col) in cols.iter().enumerate() {
        s.push_str(&column_cell(c, col, col.header, flex, i));
    }
    s
}

fn model_row(c: &Ctx, id: &str, row: &Row, selected: bool, w: usize) -> L {
    let app = c.app;
    let f = &app.fmt;
    let Some(m) = app.snapshot.model_servers.iter().find(|m| m.id == id) else {
        return Line::default();
    };
    let kind = format!("{:?}", m.kind).to_lowercase();
    let models: Vec<String> = m.models.iter().map(|x| x.name.clone()).collect();
    let weights: u64 = m.models.iter().filter_map(|x| x.weights_bytes.value).sum();
    let kv: Option<u64> = {
        let v: Vec<u64> = m.models.iter().filter_map(|x| x.kv_bytes.value).collect();
        (!v.is_empty()).then(|| v.iter().sum())
    };
    let speed = m
        .s_per_step
        .value
        .map(|s| format!("{s:.1} s/step"))
        .or_else(|| m.tok_s.value.map(|t| format!("{t:.0} tok/s")))
        .unwrap_or_else(|| "-".into());
    let job = match (&m.progress, m.busy.value) {
        (Some(p), _) => format!("{} {}/{}", p.label, p.done, p.total),
        (None, Some(true)) => "busy".into(),
        (None, Some(false)) => "idle".into(),
        _ => "n/a".into(),
    };
    let mem = m
        .group_id
        .as_deref()
        .and_then(|g| app.group(g))
        .map(|g| mem_text(c, g))
        .unwrap_or_else(|| "n/a".into());
    let device = m
        .models
        .first()
        .map(|x| format!("{:?}", x.device).to_lowercase())
        .unwrap_or_default();
    let mut spans = vec![marker(c, row, selected)];
    match c.class {
        LayoutClass::Compact => {
            spans.push(sp(
                c.fit(&format!("{kind} {}", models.join(", ")), w.saturating_sub(3 + 18)),
                c.t("kind.model"),
            ));
            spans.push(sp(format!(" {}", c.fit(&job, 8)), c.t("state.info")));
            spans.push(sp(rjust(&mem, 9), c.t("text.number")));
        }
        _ => {
            let endpoint = m.endpoint.clone().unwrap_or_else(|| "no endpoint".into());
            let weights = if weights > 0 {
                f.bytes(weights)
            } else {
                "n/a".into()
            };
            let (cols, flex) = fit_columns(MODEL_COLS, w);
            for (i, col) in cols.iter().enumerate() {
                let (text, tok) = match col.id {
                    "kind" => (kind.clone(), "kind.model"),
                    "endpoint" => (endpoint.clone(), "text.link"),
                    "models" => (models.join(", "), "ui.text"),
                    "weights" => (weights.clone(), "text.number"),
                    "kv" => (f.opt_bytes(kv), "text.number"),
                    "device" => (device.clone(), "text.label"),
                    "speed" => (speed.clone(), "text.number"),
                    "job" => (job.clone(), "state.info"),
                    _ => (mem.clone(), "text.number"),
                };
                spans.push(sp(column_cell(c, col, &text, flex, i), c.t(tok)));
            }
        }
    }
    finish(c, spans, w, selected)
}

fn sandbox_row(c: &Ctx, id: &str, row: &Row, selected: bool, w: usize) -> L {
    let app = c.app;
    let f = &app.fmt;
    let Some(sb) = app.snapshot.sandboxes.iter().find(|s| s.id == id) else {
        return Line::default();
    };
    let host = sandbox_host_text(c, sb);
    let owner = sb
        .started_by_group
        .as_deref()
        .and_then(|g| app.group(g))
        .map(|g| app.label(g))
        .unwrap_or_else(|| "-".into());
    let kind = format!("{:?}", sb.kind).to_lowercase();
    let mut spans = vec![marker(c, row, selected)];
    match c.class {
        LayoutClass::Compact => {
            spans.push(sp(c.fit(&sb.label, w.saturating_sub(3 + 9)), c.t("kind.sandbox")));
            spans.push(sp(rjust(&host, 9), c.t("text.number")));
        }
        _ => {
            let (cols, flex) = fit_columns(SANDBOX_COLS, w);
            for (i, col) in cols.iter().enumerate() {
                let (text, tok) = match col.id {
                    "kind" => (kind.clone(), "text.label"),
                    "runtime" => (sb.runtime.clone(), "ui.muted"),
                    "label" => (sb.label.clone(), "kind.sandbox"),
                    "host" => (host.clone(), "text.number"),
                    "config" => (f.opt_bytes(sb.configured_mem.value), "text.number"),
                    "guest" => (f.opt_bytes(sb.guest_mem.value), "text.number"),
                    _ => (owner.clone(), "ui.muted"),
                };
                spans.push(sp(column_cell(c, col, &text, flex, i), c.t(tok)));
            }
        }
    }
    finish(c, spans, w, selected)
}

/// Host-side footprint of a sandbox: the sum over its host processes, `n/a` when none is measured (never 0),
/// with the lower-bound mark for VMs.
fn sandbox_host_text(c: &Ctx, sb: &oomtop_core::Sandbox) -> String {
    let vals: Vec<u64> = sb
        .host_pids
        .iter()
        .filter_map(|p| c.app.process(*p))
        .filter_map(|p| p.mem.footprint_or_pss.value)
        .collect();
    if vals.is_empty() {
        return "n/a".into();
    }
    let host = c.app.fmt.bytes(vals.iter().sum());
    if sb.footprint_lower_bound {
        format!("{}{host}", c.g.lower)
    } else {
        host
    }
}

fn reclaim_all_row(c: &Ctx, row: &Row, selected: bool, w: usize) -> L {
    // Exactly the targets the confirmation will name (filter, caller session and protect rules applied).
    let (targets, _) = c.app.reclaim_all_targets();
    let gain: u64 = targets.iter().filter_map(|t| t.expected_gain).sum();
    let swap: u64 = targets
        .iter()
        .filter_map(|t| c.app.group(&t.group_id))
        .filter_map(|g| g.swap_gain.value)
        .sum();
    let stop = c.app.key_for("stop").unwrap_or_else(|| ":stop".into());
    let text = format!(
        "All {} candidates · ≈{} RAM{} — {stop} stops them all (one confirmation)",
        targets.len(),
        c.app.fmt.bytes(gain),
        if swap > 0 {
            format!(" + ≈{} swap", c.app.fmt.bytes(swap))
        } else {
            String::new()
        }
    );
    let spans = vec![
        marker(c, row, selected),
        sp(c.fit(&text, w.saturating_sub(3)), c.t("state.ok")),
    ];
    finish(c, spans, w, selected)
}

fn reclaim_row(c: &Ctx, g: &Group, row: &Row, selected: bool, w: usize) -> L {
    let app = c.app;
    let f = &app.fmt;
    let why = if g.orphan {
        let owner = g
            .owner_group
            .as_deref()
            .and_then(|o| app.group(o))
            .map(|o| app.label(o));
        match owner {
            Some(o) => format!("orphan of {o}"),
            None => "orphan".into(),
        }
    } else if g.idle {
        format!("idle {}", g.idle_for_s.map(|s| f.duration(s)).unwrap_or_default())
    } else {
        String::new()
    };
    let gain = format!("{}{}", c.g.approx, f.opt_bytes(g.reclaim_gain.value));
    let swap = g
        .swap_gain
        .value
        .map(|s| format!("+{}{} swap", c.g.approx, f.bytes(s)))
        .unwrap_or_default();
    let mut spans = vec![marker(c, row, selected)];
    let name_w = match c.class {
        LayoutClass::Compact => w.saturating_sub(3 + 10),
        _ => w.saturating_sub(3 + 9 + 22 + 10 + 15).max(10),
    };
    spans.push(sp(c.fit(&group_name(c, g), name_w), c.t(kind_token(g.kind))));
    if c.class != LayoutClass::Compact {
        spans.push(sp(format!(" {}", c.fit(g.kind.alias(), 8)), c.t("text.label")));
        spans.push(sp(format!(" {}", c.fit(&why, 21)), c.t("ui.muted")));
    }
    spans.push(sp(format!(" {}", rjust(&gain, 9)), c.t("state.ok")));
    if c.class != LayoutClass::Compact {
        spans.push(sp(format!(" {}", c.fit(&swap, 14)), c.t("mem.swap")));
    }
    finish(c, spans, w, selected)
}

fn past_row(c: &Ctx, id: &str, row: &Row, selected: bool, w: usize) -> L {
    let Some(frame) = c.app.timeline.current() else {
        return Line::default();
    };
    let Some(r) = frame.rows.iter().find(|r| &*r.id == id) else {
        return Line::default();
    };
    let mut spans = vec![marker(c, row, selected)];
    let name_w = w.saturating_sub(3 + 9 + 9 + 16).max(8);
    spans.push(sp(c.fit(&r.label, name_w), c.t(kind_token(r.kind))));
    spans.push(sp(format!(" {}", c.fit(r.kind.alias(), 8)), c.t("text.label")));
    spans.push(sp(
        format!(" {}", rjust(&c.app.fmt.opt_bytes(r.footprint), 8)),
        c.t("text.number"),
    ));
    if r.reclaimable {
        spans.push(sp(format!("  {} reclaimable", c.g.ok), c.t("state.ok")));
    }
    finish(c, spans, w, selected)
}

fn row_line(c: &Ctx, rows: &[Row], i: usize, selected: bool, w: usize) -> L {
    let row = &rows[i];
    let app = c.app;
    if let Some(conf) = &app.confirm {
        if conf.row.as_ref() == Some(&row.key) {
            let text = format!(" {} {}", c.g.warn, conf.prompt);
            return Line::from(sp(
                c.fit(&text, w),
                c.t("state.warn").add_modifier(Modifier::REVERSED),
            ));
        }
    }
    match &row.key {
        RowKey::Group(id) => match app.group(id) {
            Some(g) if app.view == View::Reclaim => reclaim_row(c, g, row, selected, w),
            Some(g) => group_row(c, g, row, selected, w),
            None => Line::default(),
        },
        RowKey::Member(gid, pid) => {
            let last = rows
                .get(i + 1)
                .map(|n| !matches!(&n.key, RowKey::Member(g, _) if g == gid))
                .unwrap_or(true);
            match app.process(*pid) {
                Some(p) => member_row(c, p, last, selected, w),
                None => Line::default(),
            }
        }
        RowKey::Proc(pid) => match app.process(*pid) {
            Some(p) => proc_row(c, p, row, selected, w, &tree_prefix(c, rows, i)),
            None => Line::default(),
        },
        RowKey::Model(id) => model_row(c, id, row, selected, w),
        RowKey::Sandbox(id) => sandbox_row(c, id, row, selected, w),
        RowKey::ReclaimAll => reclaim_all_row(c, row, selected, w),
        RowKey::Past(id) => past_row(c, id, row, selected, w),
    }
}

/// htop-style tree branches for a Processes row in tree mode (F5): `│ ├─ ` / `└─ `, empty at depth 0.
fn tree_prefix(c: &Ctx, rows: &[Row], i: usize) -> String {
    let d = rows[i].depth as usize;
    if d == 0 {
        return String::new();
    }
    let later = &rows[i + 1..];
    // Is there another row at depth `k` below this one before the tree climbs above `k`?
    let continues = |k: usize| {
        later
            .iter()
            .map(|r| r.depth as usize)
            .take_while(|&x| x >= k)
            .any(|x| x == k)
    };
    let mut s = String::new();
    for k in 1..d {
        s.push_str(if continues(k) { c.g.tick } else { " " });
        s.push(' ');
    }
    s.push_str(if continues(d) { c.g.branch } else { c.g.branch_last });
    s.push_str(c.g.hline);
    s.push(' ');
    s
}

/// Draws the scrolling list into `r`; `max_rows` caps visible rows (compact "top 5"), adding "… N more".
fn list(c: &Ctx, buf: &mut Buffer, r: Rect, regions: &mut Regions, max_rows: Option<usize>, empty: &str) {
    let app = c.app;
    let w = r.width as usize;
    if r.height == 0 {
        return;
    }
    if app.rows.is_empty() {
        let msg = if app.filter.is_some() {
            format!(
                "   nothing matches the filter {:?} — esc clears it",
                app.filter.as_deref().unwrap_or("")
            )
        } else {
            format!("   {empty}")
        };
        put(buf, r, r.y, &Line::from(sp(c.fit(&msg, w), c.t("ui.muted"))));
        return;
    }
    let mut height = r.height as usize;
    let mut more = false;
    if let Some(m) = max_rows {
        if app.rows.len() > m && height > m {
            height = m;
            more = true;
        }
    }
    if !more && app.rows.len() > height && max_rows.is_some() {
        height = height.saturating_sub(1);
        more = true;
    }
    let off = scroll_offset(app.list_offset.get(), app.selected, height, app.rows.len());
    app.list_offset.set(off);
    let end = (off + height).min(app.rows.len());
    for (k, i) in (off..end).enumerate() {
        let line = row_line(c, &app.rows, i, i == app.selected, w);
        put(buf, r, r.y + k as u16, &line);
    }
    regions.list = Some((Rect::new(r.x, r.y, r.width, (end - off) as u16), off));
    if more {
        let rest = app.rows.len().saturating_sub(end);
        if rest > 0 {
            put(
                buf,
                r,
                r.y + (end - off) as u16,
                &Line::from(sp(format!("   {} {rest} more", c.g.ellipsis), c.t("ui.muted"))),
            );
        }
    }
}

fn chips_line(c: &Ctx, w: usize, y: u16, x0: u16, regions: &mut Regions) -> L {
    let app = c.app;
    if app.chips.is_empty() {
        return Line::from(sp(
            c.fit(
                "   nothing pinned yet — p pins the selected row; what you open often shows up here",
                w,
            ),
            c.t("ui.muted"),
        ));
    }
    let n = app.chips.len();
    let text_for = |chip: &crate::app::Chip, short: bool| -> String {
        let star = if chip.pinned {
            format!("{} ", c.g.pin)
        } else {
            String::new()
        };
        let count = if chip.count > 1 {
            format!(" ×{}", chip.count)
        } else {
            String::new()
        };
        let mem = chip
            .footprint
            .map(|b| format!("  {}", app.fmt.bytes(b)))
            .unwrap_or_default();
        let status = if chip.status.is_empty() {
            String::new()
        } else if short {
            format!("  {}", chip.status.split(" · ").next().unwrap_or(""))
        } else {
            format!("  {}", chip.status)
        };
        format!("{star}{}{count}{mem}{status}", chip.label)
    };
    let avail = w.saturating_sub(1);
    let total = |short: bool| {
        app.chips
            .iter()
            .map(|ch| text_for(ch, short).width() + 4)
            .sum::<usize>()
    };
    let short = total(false) > avail;
    let fits = total(short) <= avail;
    let per = (avail / n).saturating_sub(4).max(8);
    let mut spans = vec![Span::raw(" ")];
    let mut x = 1usize;
    for (i, chip) in app.chips.iter().enumerate() {
        let t = text_for(chip, short);
        let cell = if fits {
            t
        } else {
            c.fit(&t, per).trim_end().to_string()
        };
        let style = if chip.running {
            c.t("ui.text")
        } else {
            c.t("ui.faint")
        };
        if let Some(id) = &chip.group_id {
            regions.hits.push((
                Rect::new(x0 + x as u16, y, cell.width() as u16, 1),
                HitTarget::Chip(id.clone()),
            ));
        }
        x += cell.width() + 4;
        spans.push(sp(cell, style));
        if i + 1 < n {
            spans.push(Span::raw("    "));
        }
    }
    Line::from(spans)
}

pub(crate) fn body(c: &Ctx, buf: &mut Buffer, area: Rect, regions: &mut Regions) {
    match c.app.view {
        View::Home => home(c, buf, area, regions),
        View::Processes => simple(
            c,
            buf,
            area,
            regions,
            "Processes",
            proc_header(c, area.width as usize),
        ),
        View::Models => {
            let hdr = model_header(c, area.width as usize);
            simple(c, buf, area, regions, "Models", hdr)
        }
        View::Sandboxes => {
            let hdr = sandbox_header(c, area.width as usize);
            simple(c, buf, area, regions, "Sandboxes", hdr)
        }
        View::Reclaim => reclaim(c, buf, area, regions),
        View::Timeline => timeline(c, buf, area, regions),
    }
}

fn model_header(c: &Ctx, w: usize) -> L {
    let s = match c.class {
        LayoutClass::Compact => format!(
            "   {}{:>9}",
            c.fit("SERVER / MODELS", w.saturating_sub(3 + 18)),
            "JOB  MEM"
        ),
        _ => header_text(c, MODEL_COLS, w),
    };
    Line::from(sp(c.fit(&s, w), c.t("text.label")))
}

fn sandbox_header(c: &Ctx, w: usize) -> L {
    let s = match c.class {
        LayoutClass::Compact => format!("   {}{:>9}", c.fit("SANDBOX", w.saturating_sub(12)), "HOST"),
        _ => header_text(c, SANDBOX_COLS, w),
    };
    Line::from(sp(c.fit(&s, w), c.t("text.label")))
}

fn details_height(c: &Ctx, area: Rect) -> u16 {
    if c.class == LayoutClass::Wide && area.height >= 18 {
        7
    } else {
        0
    }
}

fn details_pane(c: &Ctx, buf: &mut Buffer, r: Rect) {
    if r.height == 0 {
        return;
    }
    let w = r.width as usize;
    put(buf, r, r.y, &rule(c, "Details", "", w));
    let lines = detail_lines(c, w);
    for (k, l) in lines.into_iter().take(r.height as usize - 1).enumerate() {
        put(buf, r, r.y + 1 + k as u16, &l);
    }
}

/// One-line Leftovers card in Leftovers mode (or whenever orphans exist under memory pressure).
fn leftovers_card(c: &Ctx, w: usize) -> Option<L> {
    let app = c.app;
    if !matches!(app.mode, Mode::Leftovers | Mode::Pressure) {
        return None;
    }
    let orphans: Vec<&Group> = app.snapshot.groups.iter().filter(|g| g.orphan).collect();
    if orphans.is_empty() {
        return None;
    }
    let items: Vec<String> = orphans
        .iter()
        .take(3)
        .map(|g| {
            let owner = g
                .owner_group
                .as_deref()
                .and_then(|o| app.group(o))
                .map(|o| format!(" from {}", app.label(o)))
                .unwrap_or_default();
            format!(
                "{}{owner} {}",
                app.label(g),
                app.fmt.opt_bytes(g.totals.footprint.value)
            )
        })
        .collect();
    let more = orphans.len().saturating_sub(3);
    let key = app
        .headline
        .action_key
        .map(|k| format!(" {} {k} reclaims", c.g.sep))
        .unwrap_or_default();
    let text = format!(
        " {} Leftovers: {}{}{key}",
        c.g.warn,
        items.join(&format!(" {} ", c.g.sep)),
        if more > 0 {
            format!(" +{more} more")
        } else {
            String::new()
        }
    );
    Some(Line::from(sp(c.fit(&text, w), c.t("state.warn"))))
}

/// Right side of the "Ranked" rule: active filter, sort, and the find/why keys.
fn ranked_rule_right(c: &Ctx) -> String {
    let app = c.app;
    let sort = match app.home_sort {
        crate::app::HomeSort::Rank => String::new(),
        other => format!("sorted by {} · ", other.as_str()),
    };
    let filter = app
        .filter
        .as_ref()
        .map(|f| format!("filter {f} · "))
        .unwrap_or_default();
    let hints: Vec<String> = [("filter", "filter"), ("command", "command"), ("why", "why")]
        .iter()
        .filter_map(|(a, t)| app.key_for(a).map(|k| format!("{k} {t}")))
        .collect();
    format!("{filter}{sort}{}", hints.join(&format!(" {} ", c.g.sep)))
}

/// Height a layout slot needs; `None` = flexible (takes a share of what is left).
fn slot_height(c: &Ctx, slot: &str, w: usize) -> Option<u16> {
    let rules = !c.app.compact_density;
    Some(match slot {
        "your-things" => 1 + u16::from(rules),
        "cards" => u16::from(leftovers_card(c, w).is_some()),
        "timeline" => 3 + u16::from(rules),
        "details" => 7,
        // header/headline/meters/footer live outside the body; unknown slots take nothing
        "header" | "headline" | "meters" | "footer" => 0,
        "ranked" => return None,
        _ => 0,
    })
}

/// Draws one layout slot into `r`.
fn draw_slot(c: &Ctx, buf: &mut Buffer, r: Rect, regions: &mut Regions, slot: &str) {
    if r.height == 0 || r.width == 0 {
        return;
    }
    let app = c.app;
    let w = r.width as usize;
    let rules = !app.compact_density;
    let mut y = r.y;
    match slot {
        "your-things" => {
            if rules {
                put(buf, r, y, &rule(c, "Your things", "", w));
                y += 1;
            }
            let chips = chips_line(c, w, y, r.x, regions);
            put(buf, r, y, &chips);
        }
        "cards" => {
            if let Some(card) = leftovers_card(c, w) {
                put(buf, r, y, &card);
            }
        }
        "timeline" => {
            if rules {
                put(buf, r, y, &rule(c, "Timeline", "6 for the full view", w));
                y += 1;
            }
            for line in chart_lines(c, w) {
                put(buf, r, y, &line);
                y += 1;
            }
        }
        "details" => details_pane(c, buf, r),
        "ranked" => {
            if rules {
                put(buf, r, y, &rule(c, "Ranked", &ranked_rule_right(c), w));
                y += 1;
            }
            if let Some(set) = app.layout.as_ref().and_then(|l| l.group_columns()) {
                if y < r.y + r.height {
                    put(buf, r, y, &custom_header(c, set, w));
                    y += 1;
                }
            }
            list(
                c,
                buf,
                Rect::new(r.x, y, r.width, (r.y + r.height).saturating_sub(y)),
                regions,
                Some(usize::MAX),
                "no groups yet — waiting for the first sample",
            );
        }
        _ => {}
    }
}

/// Home screen of a named layout (UX §12.6): rows top to bottom, fixed-height slots first, the flexible
/// ones (`ranked`, splits holding it) share the rest; splits divide the width by their percentages.
fn home_layout(
    c: &Ctx,
    buf: &mut Buffer,
    area: Rect,
    regions: &mut Regions,
    l: &crate::columns::ActiveLayout,
) {
    use crate::columns::SlotRow;
    let w = area.width as usize;
    let rows = l.rows(area.width);
    let heights: Vec<Option<u16>> = rows
        .iter()
        .map(|r| match r {
            SlotRow::Slot(s) => slot_height(c, s, w),
            SlotRow::Split(parts) => {
                let hs: Vec<Option<u16>> = parts.iter().map(|(s, _)| slot_height(c, s, w)).collect();
                if hs.iter().all(|h| h.is_some()) {
                    hs.into_iter().flatten().max()
                } else {
                    None
                }
            }
        })
        .collect();
    let fixed: u16 = heights.iter().flatten().sum();
    let flex_n = heights.iter().filter(|h| h.is_none()).count() as u16;
    let spare = area.height.saturating_sub(fixed);
    let base = spare / flex_n.max(1);
    let mut flex_seen = 0u16;
    let mut y = area.y;
    let bottom = area.y + area.height;
    for (row, h) in rows.iter().zip(heights) {
        if y >= bottom {
            break;
        }
        let h = match h {
            Some(h) => h,
            None => {
                flex_seen += 1;
                // the last flexible row takes the rounding remainder
                if flex_seen == flex_n {
                    spare - base * (flex_n - 1)
                } else {
                    base
                }
            }
        }
        .min(bottom - y);
        let rect = Rect::new(area.x, y, area.width, h);
        match row {
            SlotRow::Slot(s) => draw_slot(c, buf, rect, regions, s),
            SlotRow::Split(parts) => {
                let pcts: Vec<Option<u8>> = parts.iter().map(|(_, p)| *p).collect();
                let mut x = area.x;
                for ((slot, _), pw) in parts.iter().zip(crate::columns::split_widths(area.width, &pcts)) {
                    draw_slot(c, buf, Rect::new(x, y, pw, h), regions, slot);
                    x += pw;
                }
            }
        }
        y += h;
    }
}

fn home(c: &Ctx, buf: &mut Buffer, area: Rect, regions: &mut Regions) {
    if let Some(l) = &c.app.layout {
        return home_layout(c, buf, area, regions, l);
    }
    let app = c.app;
    let w = area.width as usize;
    let mut y = area.y;
    let rules = !app.compact_density && c.class != LayoutClass::Compact;
    if rules {
        put(buf, area, y, &rule(c, "Your things", "", w));
        y += 1;
    }
    let chips = chips_line(c, w, y, area.x, regions);
    put(buf, area, y, &chips);
    y += 1;
    // Leftovers card (UX §3): orphans with the sessions that spawned them, one keypress to reclaim.
    if let Some(card) = leftovers_card(c, w) {
        put(buf, area, y, &card);
        y += 1;
    }
    if !app.compact_density {
        let right = match c.class {
            LayoutClass::Compact => String::new(),
            _ => ranked_rule_right(c),
        };
        put(buf, area, y, &rule(c, "Ranked", &right, w));
        y += 1;
    }
    let dh = details_height(c, area);
    let list_h = (area.y + area.height).saturating_sub(y + dh);
    list(
        c,
        buf,
        Rect::new(area.x, y, area.width, list_h),
        regions,
        Some(usize::MAX),
        "no groups yet — waiting for the first sample",
    );
    if dh > 0 {
        details_pane(
            c,
            buf,
            Rect::new(area.x, area.y + area.height - dh, area.width, dh),
        );
    }
}

fn simple(c: &Ctx, buf: &mut Buffer, area: Rect, regions: &mut Regions, title: &str, header: L) {
    let app = c.app;
    let w = area.width as usize;
    let mut y = area.y;
    let n = app.rows.len();
    let plural = |one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let count = match app.view {
        View::Processes => plural("process", "processes"),
        View::Models => plural("server", "servers"),
        View::Sandboxes => plural("sandbox", "sandboxes"),
        _ => String::new(),
    };
    let sort = if app.view == View::Processes {
        let key = app.key_for("sort-by").unwrap_or_else(|| ":sort".into());
        let tree = match (app.proc_tree, app.key_for("tree")) {
            (true, Some(k)) => format!(" {} tree ({k})", c.g.sep),
            (true, None) => format!(" {} tree", c.g.sep),
            _ => String::new(),
        };
        format!(
            " {} sorted by {} ({key}){tree}",
            c.g.sep,
            format!("{:?}", app.proc_sort).to_lowercase()
        )
    } else {
        String::new()
    };
    if !app.compact_density {
        put(buf, area, y, &rule(c, title, &format!("{count}{sort}"), w));
        y += 1;
    }
    put(buf, area, y, &header);
    y += 1;
    let dh = details_height(c, area);
    let mut list_h = (area.y + area.height).saturating_sub(y + dh);
    // Models view: model files on disk below the servers (SPEC §10), at most half of the space.
    let disk = if app.view == View::Models && !app.disk_models.is_empty() {
        let want = (app.disk_models.len() + 1) as u16;
        let rows_min = (n.max(1) + 1) as u16;
        let h = want.min(list_h.saturating_sub(rows_min));
        (h >= 2).then(|| disk_lines(c, w, h as usize))
    } else {
        None
    };
    let full_h = list_h;
    if let Some(d) = &disk {
        // Servers first (no scrolling needed for a handful), then a blank line and the files.
        list_h = (n.max(1) as u16).min(list_h.saturating_sub(d.len() as u16 + 1));
    }
    let empty = match app.view {
        View::Models => "no model servers detected (Ollama, llama.cpp, sd.cpp, vLLM, LM Studio, MLX)",
        View::Sandboxes => "no sandboxes, VMs or containers detected",
        _ => "no processes",
    };
    list(
        c,
        buf,
        Rect::new(area.x, y, area.width, list_h),
        regions,
        None,
        empty,
    );
    if let Some(d) = disk {
        let top = y + list_h + 1;
        for (dy, l) in (top..y + full_h).zip(d) {
            put(buf, area, dy, &l);
        }
    }
    if dh > 0 {
        details_pane(
            c,
            buf,
            Rect::new(area.x, area.y + area.height - dh, area.width, dh),
        );
    }
}

/// "On disk" rule plus up to `h − 1` model files, largest first: size, format, name, loaded by / last used.
fn disk_lines(c: &Ctx, w: usize, h: usize) -> Vec<L> {
    let app = c.app;
    let f = &app.fmt;
    let files = &app.disk_models;
    let total: u64 = files.iter().map(|m| m.size).sum();
    let dups = files.iter().filter(|m| m.duplicate).count();
    let right = format!(
        "{} files {} {}{}",
        files.len(),
        c.g.sep,
        f.bytes(total),
        if dups > 0 {
            format!(" {} {dups} duplicates", c.g.sep)
        } else {
            String::new()
        }
    );
    let mut out = vec![rule(c, "On disk", &right, w)];
    let now = app.snapshot.taken_at_ms;
    let shown = h.saturating_sub(1);
    for (i, m) in files.iter().enumerate().take(shown) {
        if i + 1 == shown && files.len() > shown {
            out.push(finish(
                c,
                vec![sp(
                    format!("   … {} more (oomtop models)", files.len() - i),
                    c.t("ui.muted"),
                )],
                w,
                false,
            ));
            break;
        }
        let (state, tok) = match m.loaded_by(&app.snapshot) {
            Some(by) => (format!("loaded · {by}"), "state.ok"),
            None => (
                m.last_used_ms
                    .filter(|t| *t > 0 && *t <= now)
                    .map(|t| format!("used {} ago", format_duration((now - t) / 1000)))
                    .unwrap_or_default(),
                "ui.muted",
            ),
        };
        let dup = if m.duplicate { " dup" } else { "" };
        let fixed = 3 + 9 + 12 + 22;
        let name_w = w.saturating_sub(fixed).max(8);
        let spans = vec![
            sp("   ", c.t("ui.text")),
            sp(rjust(&f.bytes(m.size), 8), c.t("text.number")),
            sp(format!(" {}", c.fit(&m.format, 11)), c.t("text.label")),
            sp(format!(" {}", c.fit(&m.name, name_w)), c.t("ui.text")),
            sp(format!(" {}", c.fit(&format!("{state}{dup}"), 21)), c.t(tok)),
        ];
        out.push(finish(c, spans, w, false));
    }
    out
}

fn reclaim(c: &Ctx, buf: &mut Buffer, area: Rect, regions: &mut Regions) {
    let app = c.app;
    let f = &app.fmt;
    let w = area.width as usize;
    let mut y = area.y;
    let (targets, _) = app.reclaim_all_targets();
    let gain: u64 = targets.iter().filter_map(|t| t.expected_gain).sum();
    let after = app.headroom.headroom.map(|h| h + gain as i64);
    let summary = format!(
        " Reclaimable {}{} across {} group{} {} headroom {} {} {}{} after reclaim {} suspend is CPU relief only and never counted",
        c.g.approx,
        f.bytes(gain),
        targets.len(),
        if targets.len() == 1 { "" } else { "s" },
        c.g.sep,
        app.headroom.headroom.map(|h| f.signed(h)).unwrap_or_else(|| "n/a".into()),
        if c.g.ascii { "->" } else { "→" },
        c.g.approx,
        after.map(|h| f.signed(h)).unwrap_or_else(|| "n/a".into()),
        c.g.sep
    );
    put(
        buf,
        area,
        y,
        &Line::from(sp(c.fit(&summary, w), c.t("text.number"))),
    );
    y += 1;
    if !app.compact_density {
        put(
            buf,
            area,
            y,
            &rule(c, "Candidates", "largest gain first · x stop (confirms)", w),
        );
        y += 1;
    }
    let dh = details_height(c, area);
    let list_h = (area.y + area.height).saturating_sub(y + dh);
    list(
        c,
        buf,
        Rect::new(area.x, y, area.width, list_h),
        regions,
        None,
        "nothing to reclaim — no idle build daemons, orphans or idle model servers",
    );
    if dh > 0 {
        details_pane(
            c,
            buf,
            Rect::new(area.x, area.y + area.height - dh, area.width, dh),
        );
    }
}

/// Resamples `values` to exactly `n` points (bucket averages when shrinking, nearest when stretching), so a
/// chart always spans its width; the time axis is the recorded span.
fn resample(values: &[f64], n: usize) -> Vec<f64> {
    if values.is_empty() || n == 0 {
        return Vec::new();
    }
    if values.len() >= n {
        (0..n)
            .map(|i| {
                let a = i * values.len() / n;
                let b = ((i + 1) * values.len() / n).max(a + 1);
                values[a..b].iter().sum::<f64>() / (b - a) as f64
            })
            .collect()
    } else {
        (0..n)
            .map(|i| values[(i * (values.len() - 1) + (n - 1) / 2) / (n - 1).max(1)])
            .collect()
    }
}

const CHART_LABEL_W: usize = 8;
const CHART_VALUE_W: usize = 10;

fn chart_width(w: usize) -> usize {
    w.saturating_sub(CHART_LABEL_W + CHART_VALUE_W + 1).max(4)
}

/// Samples with gaps: an unavailable sample repeats the last known value (leading gaps take the first known
/// one), so a missing reading never plots as a drop to zero. All-unavailable → empty (nothing drawn).
fn fill_gaps(vals: &[Option<f64>]) -> Vec<f64> {
    let Some(first) = vals.iter().flatten().next().copied() else {
        return Vec::new();
    };
    let mut last = first;
    vals.iter()
        .map(|v| {
            if let Some(x) = v {
                last = *x;
            }
            last
        })
        .collect()
}

/// The AVAIL / SWAP / CPU chart lines of the timeline (also the layout `timeline` slot), newest right.
fn chart_lines(c: &Ctx, w: usize) -> Vec<L> {
    let app = c.app;
    let f = &app.fmt;
    let tl = &app.timeline;
    let chart_w = chart_width(w);
    let per = if app.sparklines == oomtop_config::model::Sparklines::Braille && c.g.braille {
        2
    } else {
        1
    };
    let series = |get: &dyn Fn(&crate::timeline::Frame) -> Option<f64>| -> Vec<f64> {
        fill_gaps(&tl.frames.iter().map(get).collect::<Vec<_>>())
    };
    let charts: [(&str, Vec<f64>, &str, String); 3] = [
        (
            "AVAIL",
            series(&|fr| fr.available.map(|v| v as f64)),
            "mem.free",
            f.opt_bytes(tl.current().and_then(|fr| fr.available)),
        ),
        (
            "SWAP",
            series(&|fr| fr.swap_used.map(|v| v as f64)),
            "mem.swap",
            f.opt_bytes(tl.current().and_then(|fr| fr.swap_used)),
        ),
        (
            "CPU",
            series(&|fr| fr.cpu_pct),
            "chart.spark",
            tl.current()
                .and_then(|fr| fr.cpu_pct)
                .map(|c| format!("{c:.0}%"))
                .unwrap_or_else(|| "n/a".into()),
        ),
    ];
    charts
        .iter()
        .map(|(label, vals, tok, cur)| {
            // sparklines = none → plain numbers only
            let text = sparkline(&resample(vals, chart_w * per), chart_w, app.sparklines, c.g);
            Line::from(vec![
                sp(
                    format!(" {:<w$}", label, w = CHART_LABEL_W - 1),
                    c.t("text.label"),
                ),
                sp(c.fit(&text, chart_w), c.t(tok)),
                sp(format!(" {}", rjust(cur, CHART_VALUE_W - 1)), c.t("text.number")),
            ])
        })
        .collect()
}

fn timeline(c: &Ctx, buf: &mut Buffer, area: Rect, regions: &mut Regions) {
    let app = c.app;
    let f = &app.fmt;
    let tl = &app.timeline;
    let w = area.width as usize;
    let mut y = area.y;
    put(
        buf,
        area,
        y,
        &rule(
            c,
            &format!(
                "Timeline {} last {}",
                c.g.sep,
                tl.frames
                    .back()
                    .zip(tl.frames.front())
                    .map(|(b, a)| crate::format::precise_duration(b.t_ms.saturating_sub(a.t_ms) / 1000))
                    .unwrap_or_else(|| "0s".into())
            ),
            &format!(
                "h/l scrub {} pgup/pgdn 1 min {} g oldest {} G live",
                c.g.sep, c.g.sep, c.g.sep
            ),
            w,
        ),
    );
    y += 1;
    let label_w = CHART_LABEL_W;
    let chart_w = chart_width(w);
    let frames: Vec<&crate::timeline::Frame> = tl.frames.iter().collect();
    let n = frames.len();
    for line in chart_lines(c, w) {
        put(buf, area, y, &line);
        y += 1;
    }
    let chart_x = area.x + label_w as u16;
    let cols_used = if n > 1 { chart_w } else { 1 };
    regions.timeline = Some(Rect::new(chart_x, area.y + 1, cols_used.max(1) as u16, 4));
    // markers row + cursor row
    if n > 0 {
        let col_of = |i: usize| -> usize {
            if n <= 1 {
                0
            } else {
                (i * (cols_used.max(1) - 1)) / (n - 1)
            }
        };
        let mut marks = vec![' '; chart_w];
        let t0 = frames[0].t_ms;
        let t1 = frames[n - 1].t_ms.max(t0 + 1);
        for m in &tl.markers {
            let frac = (m.t_ms.saturating_sub(t0)) as f64 / (t1 - t0) as f64;
            let col = ((frac * (cols_used.max(1) - 1) as f64).round() as usize).min(chart_w - 1);
            marks[col] = c.g.tick.chars().next().unwrap_or('|');
        }
        let cur_i = tl.scrub.unwrap_or(n - 1);
        let mut cursor = vec![' '; chart_w];
        cursor[col_of(cur_i).min(chart_w - 1)] = c.g.cursor.chars().next().unwrap_or('^');
        put(
            buf,
            area,
            y,
            &Line::from(vec![
                sp(format!(" {:<w$}", "MARKS", w = label_w - 1), c.t("text.label")),
                sp(marks.into_iter().collect::<String>(), c.t("state.warn")),
            ]),
        );
        y += 1;
        put(
            buf,
            area,
            y,
            &Line::from(vec![
                sp(" ".repeat(label_w), Style::default()),
                sp(cursor.into_iter().collect::<String>(), c.t("ui.accent")),
            ]),
        );
        y += 1;
        if let Some(fr) = tl.current() {
            let now = frames[n - 1].t_ms;
            let when = if tl.scrub.is_none() {
                "live".to_string()
            } else {
                f.at(fr.t_ms, now)
            };
            let text = format!(
                " Viewing {when} {} mode {} {} avail {} {} swap {} / {} {} cpu {}",
                c.g.sep,
                fr.mode,
                c.g.sep,
                f.opt_bytes(fr.available),
                c.g.sep,
                f.opt_bytes(fr.swap_used),
                f.opt_bytes(fr.swap_total),
                c.g.sep,
                fr.cpu_pct
                    .map(|v| format!("{v:.0}%"))
                    .unwrap_or_else(|| "n/a".into())
            );
            put(buf, area, y, &Line::from(sp(c.fit(&text, w), c.t("text.number"))));
            y += 1;
            let recent: Vec<String> = tl
                .markers
                .iter()
                .rev()
                .filter(|m| m.t_ms <= fr.t_ms)
                .take(6)
                .map(|m| format!("{} ({})", m.text, f.at(m.t_ms, now)))
                .collect();
            let text = if recent.is_empty() {
                " no insights yet".to_string()
            } else {
                format!(" {}", recent.join(&format!(" {} ", c.g.sep)))
            };
            put(buf, area, y, &Line::from(sp(c.fit(&text, w), c.t("ui.muted"))));
            y += 1;
        }
    }
    if y < area.y + area.height && !app.compact_density {
        put(buf, area, y, &rule(c, "Ranked as it was", "", w));
        y += 1;
    }
    let list_h = (area.y + area.height).saturating_sub(y);
    list(
        c,
        buf,
        Rect::new(area.x, y, area.width, list_h),
        regions,
        None,
        "no frames recorded yet",
    );
    // The list keeps the chart region for scrubbing; clicks on rows still select.
}

// -------------------------------------------------------------------------------------------------------
// Details text (pane in wide layouts, overlay otherwise)
// -------------------------------------------------------------------------------------------------------

fn quality_word(q: &Quality) -> String {
    match q {
        Quality::Exact => "exact".into(),
        Quality::Estimate => "estimate".into(),
        Quality::Unavailable(r) => format!("unavailable: {r}"),
    }
}

fn action_hints(c: &Ctx, g: Option<&Group>) -> L {
    let app = c.app;
    let refused = g.and_then(|g| {
        if g.is_self {
            Some("oomtop itself")
        } else if g.protected || g.kind == GroupKind::System {
            Some("protected")
        } else if app.protect.caller_group.as_deref() == Some(g.id.as_str()) {
            Some("calling agent's session")
        } else {
            None
        }
    });
    let key = |k: &str, text: &str, enabled: bool| -> Vec<Span<'static>> {
        let style = if enabled { c.t("text.key") } else { c.t("ui.faint") };
        vec![
            sp(format!("[{k}]"), style),
            sp(
                format!(" {text}   "),
                if enabled { c.t("ui.text") } else { c.t("ui.faint") },
            ),
        ]
    };
    let can_act = g.is_some() && refused.is_none();
    let mut spans = vec![Span::raw(" ")];
    // Keys come from the active keymap; an unbound action is not advertised.
    for (action, text, enabled) in [
        ("stop", "stop", can_act),
        ("suspend", "suspend", can_act),
        ("pin", "pin", g.is_some()),
        ("mute", "mute", g.is_some()),
        ("watch", "watch", g.is_some()),
        ("compare", "compare", g.is_some()),
        ("why", "why ranked here", g.is_some()),
    ] {
        if let Some(k) = app.key_for(action) {
            spans.extend(key(&k, text, enabled));
        }
    }
    if let Some(r) = refused {
        spans.push(sp(format!("actions refused: {r}"), c.t("ui.muted")));
    }
    Line::from(spans)
}

/// Details of the selected row (UX §5.5 "Details pane": members, history, owner chain, actions, why).
pub(crate) fn detail_lines(c: &Ctx, w: usize) -> Vec<L> {
    let app = c.app;
    let Some(row) = app.selected_row() else {
        return vec![Line::from(sp(" nothing selected", c.t("ui.muted")))];
    };
    match &row.key {
        RowKey::Proc(pid) | RowKey::Member(_, pid) => match app.process(*pid) {
            Some(p) => process_details(c, p, w),
            None => vec![Line::from(sp(" process ended", c.t("ui.muted")))],
        },
        RowKey::Model(id) => model_details(c, id),
        RowKey::Sandbox(id) => sandbox_details(c, id),
        RowKey::ReclaimAll => {
            let (targets, refused) = app.reclaim_all_targets();
            let mut out = vec![Line::from(sp(
                " Stops every candidate below with SIGTERM after one confirmation; SIGKILL is never sent without a second one.",
                c.t("ui.text"),
            ))];
            for t in targets.iter().take(4) {
                let g = app.group(&t.group_id);
                out.push(Line::from(sp(
                    format!(
                        "   {} {} {} {}{}{}",
                        c.g.sep,
                        t.label,
                        g.map(|g| g.kind.alias()).unwrap_or(""),
                        c.g.approx,
                        app.fmt.opt_bytes(t.expected_gain),
                        // Same rule as the other idle labels: under a minute is noise (a replay or a fresh start).
                        g.and_then(|g| g.idle_for_s)
                            .filter(|s| *s >= 60)
                            .map(|s| format!(", idle {}", app.fmt.duration(s)))
                            .unwrap_or_default()
                    ),
                    c.t("ui.muted"),
                )));
            }
            if !refused.is_empty() {
                out.push(Line::from(sp(
                    format!("   refused: {}", refused.join("; ")),
                    c.t("ui.faint"),
                )));
            }
            out
        }
        RowKey::Group(id) | RowKey::Past(id) => match app.group(id) {
            Some(g) => group_details(c, g, w),
            None => vec![Line::from(sp(" this group no longer exists", c.t("ui.muted")))],
        },
    }
}

fn group_details(c: &Ctx, g: &Group, w: usize) -> Vec<L> {
    let app = c.app;
    let f = &app.fmt;
    let mut members: Vec<&Process> = g.members.iter().filter_map(|m| app.process(m.id)).collect();
    members.sort_by_key(|p| std::cmp::Reverse(p.mem.footprint_or_pss.value.unwrap_or(0)));
    let arrow = if c.g.ascii { "->" } else { "→" };
    let list: Vec<String> = members
        .iter()
        .take(4)
        .map(|p| {
            let idle = p
                .idle_for_s
                .value
                .filter(|s| *s >= 60)
                .map(|s| format!(", idle {}", f.duration(s)))
                .unwrap_or_default();
            format!("{} ({}{idle})", p.name, f.opt_bytes(p.mem.footprint_or_pss.value))
        })
        .collect();
    let more = members.len().saturating_sub(4);
    let l1 = format!(
        " {} {arrow} {}{}",
        app.label(g),
        list.join(&format!(" {} ", c.g.sep)),
        if more > 0 {
            format!(" {} +{more} more", c.g.sep)
        } else {
            String::new()
        }
    );
    let owner = g
        .owner_group
        .as_deref()
        .and_then(|o| app.group(o))
        .map(|o| format!(" {} started by {}", c.g.sep, app.label(o)));
    let root = g
        .root
        .map(|r| format!(" {} root pid {}", c.g.sep, r.pid))
        .unwrap_or_default();
    let lb = if g.lower_bound {
        format!(
            " {} footprint is a lower bound{}",
            c.g.sep,
            g.configured_mem
                .map(|b| format!(" (VM configured {})", f.bytes(b)))
                .unwrap_or_default()
        )
    } else {
        String::new()
    };
    let l2 = format!(
        " {} {} {} process{} {} grouped by {} ({:?} confidence){root}{}{lb}",
        g.kind.as_str(),
        c.g.sep,
        g.totals.process_count,
        if g.totals.process_count == 1 { "" } else { "es" },
        c.g.sep,
        g.matched_by.clone().unwrap_or_else(|| "heuristics".into()),
        g.confidence,
        owner.unwrap_or_default()
    )
    .replace("High confidence", "high confidence")
    .replace("Medium confidence", "medium confidence")
    .replace("Low confidence", "low confidence");
    let t = &g.totals;
    let l3 = format!(
        " footprint {} ({}, {}) {s} resident {} {s} gpu {} {s} swapped {} {s} reclaim {} {s} swap freed {} {s} cpu {}",
        f.measured_bytes(&t.footprint, c.g.ascii),
        t.footprint.source,
        quality_word(&t.footprint.quality),
        f.measured_bytes(&t.resident, c.g.ascii),
        f.measured_bytes(&t.gpu, c.g.ascii),
        f.measured_bytes(&t.swapped, c.g.ascii),
        f.measured_bytes(&g.reclaim_gain, c.g.ascii),
        f.measured_bytes(&g.swap_gain, c.g.ascii),
        f.opt_cpu(t.cpu_pct.value),
        s = c.g.sep
    );
    let series = app.sparks.get(&g.id).cloned().unwrap_or_default();
    let spark = sparkline_u64(&series, (w / 3).clamp(8, 40), app.sparklines, c.g);
    let (mn, mx) = (series.iter().min().copied(), series.iter().max().copied());
    let trend = if series.len() < 2 {
        format!(" history: collecting ({} sample so far)", series.len())
    } else {
        format!(
            " history {spark} min {} max {} over {} samples",
            f.opt_bytes(mn),
            f.opt_bytes(mx),
            series.len()
        )
    };
    let l4 = format!(
        "{trend}{}",
        if g.idle {
            format!(
                " {} idle {}",
                c.g.sep,
                g.idle_for_s.map(|s| f.duration(s)).unwrap_or_default()
            )
        } else {
            String::new()
        }
    );
    vec![
        Line::from(sp(c.fit(&l1, w), c.t("ui.text"))),
        Line::from(sp(c.fit(&l2, w), c.t("ui.muted"))),
        Line::from(sp(c.fit(&l3, w), c.t("text.number"))),
        Line::from(sp(c.fit(&l4, w), c.t("chart.spark"))),
        Line::from(sp(format!(" because: {}", app.because(g)), c.t("ui.muted"))),
        action_hints(c, Some(g)),
    ]
}

fn process_details(c: &Ctx, p: &Process, w: usize) -> Vec<L> {
    let app = c.app;
    let f = &app.fmt;
    let g = app.snapshot.group_of(p.id);
    let s = c.g.sep;
    let l1 = format!(
        " {} pid {} (ppid {}) {s} {} {s} {} {s} {} threads {s} group {}",
        p.name,
        p.id.pid,
        p.ppid.map(|x| x.to_string()).unwrap_or_else(|| "?".into()),
        p.user.clone().unwrap_or_else(|| p
            .uid
            .map(|u| format!("uid {u}"))
            .unwrap_or_else(|| "user ?".into())),
        proc_state_word(p),
        p.threads.map(|t| t.to_string()).unwrap_or_else(|| "?".into()),
        g.map(|g| app.label(g)).unwrap_or_else(|| "unattributed".into())
    );
    // Command lines are shown redacted on screen too (screen sharing, SPEC §13).
    let cmd = oomtop_core::redact::redact_cmdline(&p.cmdline).join(" ");
    let l2 = format!(" cmd {}", if cmd.is_empty() { p.exe.clone() } else { cmd });
    let m = &p.mem;
    let l3 = format!(
        " footprint {} ({}, {}) {s} resident {} {s} non-resident {} (compressed or swapped, est.) {s} gpu {} {s} swapped {}",
        f.measured_bytes(&m.footprint_or_pss, c.g.ascii),
        m.footprint_or_pss.source,
        quality_word(&m.footprint_or_pss.quality),
        f.measured_bytes(&m.resident, c.g.ascii),
        f.measured_bytes(&m.non_resident_est, c.g.ascii),
        f.measured_bytes(&m.gpu, c.g.ascii),
        f.measured_bytes(&m.swapped, c.g.ascii),
    );
    let mut extra = Vec::new();
    if let Some(cwd) = &p.cwd {
        extra.push(format!("cwd {cwd}"));
    }
    if let Some(a) = &p.markers.agent {
        extra.push(format!(
            "agent {a}{}",
            p.markers
                .session_id
                .as_ref()
                .map(|s| format!(" session {}", s.chars().take(8).collect::<String>()))
                .unwrap_or_default()
        ));
    }
    if !p.markers.keys.is_empty() {
        extra.push(format!("markers {}", p.markers.keys.join(",")));
    }
    if let Some(o) = p.oom_score {
        extra.push(format!("oom_score {o}"));
    }
    if !p.model_files.is_empty() {
        extra.push(format!("models {}", p.model_files.join(", ")));
    }
    if let Some(i) = p.idle_for_s.value.filter(|s| *s >= 60) {
        extra.push(format!(
            "idle {}{}",
            f.duration(i),
            if p.idle_for_s.quality == Quality::Estimate {
                " (≥ observed window)"
            } else {
                ""
            }
        ));
    }
    let l4 = format!(" {}", extra.join(&format!(" {s} ")));
    vec![
        Line::from(sp(c.fit(&l1, w), c.t("ui.text"))),
        Line::from(sp(c.fit(&l2, w), c.t("ui.muted"))),
        Line::from(sp(c.fit(&l3, w), c.t("text.number"))),
        Line::from(sp(c.fit(&l4, w), c.t("ui.muted"))),
        Line::from(sp(
            " actions target the group root, never a helper",
            c.t("ui.faint"),
        )),
        action_hints(c, g),
    ]
}

fn model_details(c: &Ctx, id: &str) -> Vec<L> {
    let app = c.app;
    let f = &app.fmt;
    let Some(m) = app.snapshot.model_servers.iter().find(|m| m.id == id) else {
        return vec![];
    };
    let s = c.g.sep;
    let status = match &m.status {
        oomtop_core::SourceStatus::Available => "adapter ok".to_string(),
        oomtop_core::SourceStatus::Partial(r) => format!("adapter partial: {r}"),
        oomtop_core::SourceStatus::Unavailable(r) => format!("adapter unavailable: {r}"),
    };
    let mut out = vec![Line::from(sp(
        format!(
            " {:?} {} {s} {status} {s} pids {}",
            m.kind,
            m.endpoint.clone().unwrap_or_else(|| "no endpoint".into()),
            m.pids
                .iter()
                .map(|p| p.pid.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        c.t("ui.text"),
    ))];
    for x in &m.models {
        out.push(Line::from(sp(
            format!(
                "   {} {s} weights {} {s} kv {} {s} on {:?}{}",
                x.name,
                f.measured_bytes(&x.weights_bytes, c.g.ascii),
                f.measured_bytes(&x.kv_bytes, c.g.ascii),
                x.device,
                x.file.as_ref().map(|p| format!(" {s} {p}")).unwrap_or_default()
            ),
            c.t("text.number"),
        )));
    }
    out.push(Line::from(sp(
        format!(
            " throughput {} {s} queue {} {s} {}",
            m.s_per_step
                .value
                .map(|v| format!("{v:.1} s/step"))
                .or_else(|| m.tok_s.value.map(|t| format!("{t:.0} tok/s")))
                .unwrap_or_else(|| "n/a".into()),
            m.queue
                .value
                .map(|q| q.to_string())
                .unwrap_or_else(|| "n/a".into()),
            m.progress
                .as_ref()
                .map(|p| format!("{} {}/{}", p.label, p.done, p.total))
                .unwrap_or_else(|| if m.busy.value == Some(true) {
                    "busy".into()
                } else {
                    "idle".into()
                })
        ),
        c.t("state.info"),
    )));
    let g = m.group_id.as_deref().and_then(|g| app.group(g));
    out.push(action_hints(c, g));
    out
}

fn sandbox_details(c: &Ctx, id: &str) -> Vec<L> {
    let app = c.app;
    let f = &app.fmt;
    let Some(sb) = app.snapshot.sandboxes.iter().find(|s| s.id == id) else {
        return vec![];
    };
    let s = c.g.sep;
    let owner = sb
        .started_by_group
        .as_deref()
        .and_then(|g| app.group(g))
        .map(|g| app.label(g));
    let host = sandbox_host_text(c, sb);
    let mut out = vec![
        Line::from(sp(
            format!(
                " {} {s} {:?} via {} {s} started by {}",
                sb.label,
                sb.kind,
                sb.runtime,
                owner.clone().unwrap_or_else(|| "unknown".into())
            ),
            c.t("ui.text"),
        )),
        Line::from(sp(
            format!(
                " host cost {host} {s} configured {} {s} guest {} {s} limit {}",
                f.measured_bytes(&sb.configured_mem, c.g.ascii),
                f.measured_bytes(&sb.guest_mem, c.g.ascii),
                sb.limits
                    .mem_max
                    .map(|b| f.bytes(b))
                    .unwrap_or_else(|| "none".into())
            ),
            c.t("text.number"),
        )),
    ];
    if sb.footprint_lower_bound {
        out.push(Line::from(sp(
            " the host footprint under-counts this VM (SPEC §5); compare with the configured size",
            c.t("ui.muted"),
        )));
    }
    if let Some(o) = owner {
        out.push(Line::from(sp(
            format!(" reclaim by quitting {o} — its VM belongs to it"),
            c.t("ui.muted"),
        )));
    }
    let g = sb.host_pids.first().and_then(|p| app.snapshot.group_of(*p));
    out.push(action_hints(c, g));
    out
}

// -------------------------------------------------------------------------------------------------------
// Focus mode (w)
// -------------------------------------------------------------------------------------------------------

pub(crate) fn focus(c: &Ctx, buf: &mut Buffer, area: Rect) {
    let app = c.app;
    let f = &app.fmt;
    let w = area.width as usize;
    let Some(key) = &app.focus else { return };
    let Some(g) = app.group_for_key(key) else {
        put(
            buf,
            area,
            area.y,
            &Line::from(sp(
                " this entity is no longer running — w or esc to leave",
                c.t("ui.muted"),
            )),
        );
        return;
    };
    let mut y = area.y;
    let s = c.g.sep;
    put(
        buf,
        area,
        y,
        &rule(
            c,
            &format!("Watch {s} {}", app.label(g)),
            &format!(
                "{} {s} {} process{} {s} w/esc leave",
                g.kind.alias(),
                g.totals.process_count,
                if g.totals.process_count == 1 { "" } else { "es" }
            ),
            w,
        ),
    );
    y += 1;
    let total = app
        .snapshot
        .memory
        .total
        .value
        .unwrap_or(app.snapshot.host.mem_total)
        .max(1);
    // Unavailable is said as such, never as 0 (SPEC §5).
    let held = match g.totals.footprint.value {
        Some(fp) => format!(
            "{}{} ({:.0}% of RAM)",
            if g.lower_bound { c.g.lower } else { "" },
            f.bytes(fp),
            fp as f64 * 100.0 / total as f64
        ),
        None => "n/a memory".to_string(),
    };
    let cost = format!(
        " Costs {held} + {} CPU to keep running {s} stopping frees {}{} RAM{}",
        f.opt_cpu(g.totals.cpu_pct.value),
        c.g.approx,
        f.opt_bytes(g.reclaim_gain.value),
        g.swap_gain
            .value
            .map(|b| format!(" + {}{} swap", c.g.approx, f.bytes(b)))
            .unwrap_or_default()
    );
    put(
        buf,
        area,
        y,
        &Line::from(sp(c.fit(&cost, w), c.t("text.headline"))),
    );
    y += 1;
    // memory over time
    let series: Vec<f64> = app
        .sparks
        .get(&g.id)
        .map(|v| v.iter().map(|x| *x as f64).collect())
        .unwrap_or_default();
    let chart_rows = if area.height >= 24 { 5 } else { 3 };
    let chart_w = w.saturating_sub(12).max(4);
    let chart = column_chart(&series, chart_w, chart_rows, c.g);
    let (mn, mx) = (
        series.iter().copied().fold(f64::MAX, f64::min),
        series.iter().copied().fold(0.0f64, f64::max),
    );
    for (i, line) in chart.iter().enumerate() {
        let label = if i == 0 {
            f.bytes(mx as u64)
        } else if i == chart_rows - 1 && !series.is_empty() {
            f.bytes(mn as u64)
        } else {
            String::new()
        };
        put(
            buf,
            area,
            y,
            &Line::from(vec![
                sp(format!(" {} ", rjust(&label, 9)), c.t("text.label")),
                sp(line.clone(), c.t("chart.spark")),
            ]),
        );
        y += 1;
    }
    put(
        buf,
        area,
        y,
        &Line::from(sp(
            format!(" memory over the last {} samples (oldest left)", series.len()),
            c.t("ui.muted"),
        )),
    );
    y += 1;
    // context lines: throttle + model progress
    let t = &app.snapshot.thermal;
    let thermal = format!(
        " thermal {} {s} {} {s} Low Power Mode {} {s} {} {s} package {}",
        t.pressure
            .value
            .map(|p| format!("{p:?}").to_lowercase())
            .unwrap_or_else(|| "n/a".into()),
        t.throttle_factor
            .value
            .map(oomtop_core::throttle::speed_label)
            .unwrap_or_else(|| "speed n/a (idle)".into()),
        match t.low_power_mode.value {
            Some(true) => "on",
            Some(false) => "off",
            None => "n/a",
        },
        match (t.on_battery.value, t.battery_pct.value) {
            (Some(true), Some(b)) => format!("on battery {b:.0}%"),
            (Some(false), _) => "on power".into(),
            _ => "power n/a".into(),
        },
        t.package_power_w
            .value
            .map(|p| format!("{p:.1} W"))
            .unwrap_or_else(|| "n/a".into())
    );
    put(buf, area, y, &Line::from(sp(c.fit(&thermal, w), c.t("ui.muted"))));
    y += 1;
    if let Some(m) = app
        .snapshot
        .model_servers
        .iter()
        .find(|m| m.group_id.as_deref() == Some(g.id.as_str()))
    {
        let mut spans = vec![sp(" model ".to_string(), c.t("text.label"))];
        if let Some(p) = &m.progress {
            let bw = 20usize;
            let done = if p.total > 0 {
                (p.done as usize * bw) / p.total as usize
            } else {
                0
            };
            spans.extend(super::widgets::bar_spans(
                c,
                &[(done.min(bw), c.g.full, c.t("chart.bar.fill"))],
                bw,
            ));
            spans.push(sp(
                format!(" {} {}/{}", p.label, p.done, p.total),
                c.t("state.info"),
            ));
        }
        let speed = m
            .s_per_step
            .value
            .map(|v| format!("  {v:.1} s/step"))
            .or_else(|| m.tok_s.value.map(|v| format!("  {v:.0} tok/s")))
            .unwrap_or_default();
        spans.push(sp(speed, c.t("text.number")));
        if let (Some(p), Some(sps)) = (&m.progress, m.s_per_step.value) {
            let left = p.total.saturating_sub(p.done) as f64 * sps;
            spans.push(sp(
                format!("  ETA ~{}", f.duration(left as u64)),
                c.t("text.number"),
            ));
        }
        let names: Vec<String> = m.models.iter().map(|x| x.name.clone()).collect();
        spans.push(sp(format!("  {}", names.join(", ")), c.t("ui.muted")));
        put(buf, area, y, &Line::from(spans));
        y += 1;
    }
    if y < area.y + area.height {
        put(buf, area, y, &rule(c, "Processes", "tree by parent", w));
        y += 1;
    }
    // child tree (DFS by ppid inside the group)
    let members: Vec<&Process> = g.members.iter().filter_map(|m| app.process(m.id)).collect();
    let pids: std::collections::HashSet<u32> = members.iter().map(|p| p.id.pid).collect();
    let mut order: Vec<(usize, &Process)> = Vec::new();
    let mut roots: Vec<&Process> = members
        .iter()
        .copied()
        .filter(|p| p.ppid.map(|pp| !pids.contains(&pp)).unwrap_or(true))
        .collect();
    roots.sort_by_key(|p| std::cmp::Reverse(p.mem.footprint_or_pss.value.unwrap_or(0)));
    fn walk<'a>(p: &'a Process, depth: usize, members: &[&'a Process], out: &mut Vec<(usize, &'a Process)>) {
        if out.len() > 500 || depth > 32 {
            return;
        }
        out.push((depth, p));
        let mut kids: Vec<&Process> = members
            .iter()
            .copied()
            .filter(|k| k.ppid == Some(p.id.pid) && k.id != p.id)
            .collect();
        kids.sort_by_key(|k| std::cmp::Reverse(k.mem.footprint_or_pss.value.unwrap_or(0)));
        for k in kids {
            walk(k, depth + 1, members, out);
        }
    }
    for r in roots {
        walk(r, 0, &members, &mut order);
    }
    let hl = match key {
        RowKey::Proc(pid) | RowKey::Member(_, pid) => Some(*pid),
        _ => None,
    };
    let hdr = format!(
        " {:>7} {} {:>8} {:>8} {:>9} {:>6} {:<9} {:>7}",
        "PID",
        c.fit("NAME", w.saturating_sub(62)),
        "MEM",
        "RES",
        "NON-RES",
        "CPU",
        "STATE",
        "IDLE"
    );
    if y < area.y + area.height {
        put(buf, area, y, &Line::from(sp(c.fit(&hdr, w), c.t("text.label"))));
        y += 1;
    }
    for (depth, p) in order {
        if y >= area.y + area.height {
            break;
        }
        let indent = "  ".repeat(depth.min(8));
        let name = if depth > 0 {
            format!("{indent}{} {}", c.g.branch_last, p.name)
        } else {
            p.name.clone()
        };
        let text = format!(
            " {:>7} {} {:>8} {:>8} {:>9} {:>6} {:<9} {:>7}",
            p.id.pid,
            c.fit(&name, w.saturating_sub(62)),
            f.opt_bytes(p.mem.footprint_or_pss.value),
            f.opt_bytes(p.mem.resident.value),
            p.mem
                .non_resident_est
                .value
                .map(|v| format!("{}{}", c.g.approx, f.bytes(v)))
                .unwrap_or_else(|| "n/a".into()),
            f.opt_cpu(p.cpu_pct.value),
            proc_state_word(p),
            p.idle_for_s
                .value
                .filter(|s| *s >= 60)
                .map(|s| f.duration(s))
                .unwrap_or_default()
        );
        let style = if Some(p.id) == hl {
            c.t("ui.selection")
        } else {
            c.t("ui.text")
        };
        put(buf, area, y, &Line::from(sp(c.fit(&text, w), style)));
        y += 1;
    }
    if y < area.y + area.height {
        let src = format!(
            " sources: footprint {} ({}) {s} resident {} ({})",
            g.totals.footprint.source,
            quality_word(&g.totals.footprint.quality),
            g.totals.resident.source,
            quality_word(&g.totals.resident.quality)
        );
        put(
            buf,
            area,
            area.y + area.height - 1,
            &Line::from(sp(c.fit(&src, w), c.t("ui.faint"))),
        );
    }
}

// -------------------------------------------------------------------------------------------------------
// Message line & footer
// -------------------------------------------------------------------------------------------------------

/// The line above the footer: input, confirmation hint, toast or status (in that priority).
pub(crate) fn message_line(c: &Ctx, width: usize) -> Option<(L, ())> {
    let app = c.app;
    if let Some(input) = &app.input {
        let (prefix, hint) = match input {
            Input::Filter(_) => ("/", "enter apply · esc cancel · e.g. mem>2G kind:daemon idle>30m"),
            Input::Command(_) => (":", "enter run · tab next suggestion · esc cancel"),
            Input::Palette(_) => ("> ", "enter run · up/down choose · esc cancel"),
            Input::Rename(_) => ("rename: ", "enter save (empty clears) · esc cancel"),
        };
        let cursor = if c.g.ascii { "_" } else { "▏" };
        let mut spans = vec![
            sp(format!(" {prefix}"), c.t("text.key")),
            sp(input.buffer().to_string(), c.t("ui.accent")),
            sp(cursor.to_string(), c.t("ui.accent")),
        ];
        if let Some(e) = &app.filter_error {
            spans.push(sp(format!("   {} {e}", c.g.warn), c.t("state.warn")));
        } else {
            spans.push(sp(format!("   {hint}"), c.t("ui.faint")));
        }
        return Some((Line::from(spans), ()));
    }
    if let Some(conf) = &app.confirm {
        let visible = conf
            .row
            .as_ref()
            .map(|k| app.rows.iter().any(|r| &r.key == k))
            .unwrap_or(false)
            && app.overlay.is_none()
            && app.focus.is_none();
        let text = if visible {
            " y confirms · any other key cancels — nothing is sent until you confirm".to_string()
        } else {
            format!(" {} {}", c.g.warn, conf.prompt)
        };
        return Some((Line::from(sp(c.fit(&text, width), c.t("state.warn"))), ()));
    }
    if let Some((t, _)) = &app.toast {
        return Some((
            Line::from(vec![
                sp(format!(" {} ", c.g.warn), c.t("state.warn")),
                sp(t.clone(), c.t("ui.text")),
            ]),
            (),
        ));
    }
    app.status
        .as_ref()
        .map(|s| (Line::from(sp(format!(" {s}"), c.t("ui.muted"))), ()))
}

/// The htop-style function-key bar (UX §7): `F1Help  F2Setup  F3Search …  F10Quit`. Keys come from the active
/// keymap (an unbound F-key shows an empty label, a remapped one its new action), labels are drawn in the
/// selection style (reverse video in the `terminal` theme — no painted background). Narrow terminals get
/// shorter labels, then fewer keys; F10 (quit) is kept last. Every item is clickable.
pub(crate) fn footer(c: &Ctx, buf: &mut Buffer, r: Rect, regions: &mut Regions) {
    let items = fkey_items(c.app);
    let w = r.width as usize;
    let key_st = c.t("ui.text");
    let lab_st = c.t("ui.selection");
    // Levels: htop's padded 6-char labels, natural labels, short labels, keys only.
    let fits = |lvl: usize, keep: &[usize]| -> usize {
        keep.iter()
            .map(|&i| {
                let (k, l, a) = &items[i];
                k.width() + label_at(l, a.as_deref(), lvl).width()
            })
            .sum()
    };
    let all: Vec<usize> = (0..items.len()).collect();
    let mut level = 0;
    while level < 2 && fits(level, &all) > w {
        level += 1;
    }
    let mut keep = all;
    // Still too wide with short labels: drop the least essential keys first (F10 quit always stays), and only
    // when most keys would be gone fall back to keys without labels.
    for f in DROP_ORDER {
        if fits(level, &keep) <= w || keep.len() <= 6 {
            break;
        }
        keep.retain(|&i| i != f - 1);
    }
    if fits(level, &keep) > w {
        level = 3;
        keep = (0..items.len()).collect();
        while fits(level, &keep) > w && keep.len() > 1 {
            keep.remove(keep.len() - 2);
        }
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut x = r.x;
    for i in keep {
        let (k, l, action) = &items[i];
        let label = label_at(l, action.as_deref(), level);
        let wd = (k.width() + label.width()) as u16;
        if let Some(a) = action {
            regions
                .hits
                .push((Rect::new(x, r.y, wd, 1), HitTarget::Action(a.clone())));
        }
        spans.push(sp(k.clone(), key_st));
        spans.push(sp(label, lab_st));
        x += wd;
    }
    // htop fills the rest of the bar in the label style.
    let used = (x - r.x) as usize;
    if used < w {
        spans.push(sp(" ".repeat(w - used), lab_st));
    }
    put(buf, r, r.y, &Line::from(spans));
}

/// `(key text, full label, action)` for F1..F10 from the active keymap.
pub(crate) fn fkey_items(app: &crate::app::App) -> Vec<(String, String, Option<String>)> {
    app.fkeys
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let label = a
                .as_deref()
                .map(crate::app::fkey_label)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| a.clone().unwrap_or_default());
            (format!("F{}", i + 1), label, a.clone())
        })
        .collect()
}

/// F-keys dropped first when the bar does not fit (1-based): Why, Reclaim, Setup, Tree, Filter, Search, SortBy,
/// Stop, Help — Quit is never dropped.
const DROP_ORDER: [usize; 9] = [7, 8, 2, 5, 4, 3, 6, 9, 1];

/// A label at a width level: 0 = padded to 6 + space (htop), 1 = natural + space, 2 = short (≤ 4) + space,
/// 3 = key only.
fn label_at(label: &str, action: Option<&str>, level: usize) -> String {
    match level {
        0 => format!("{label:<6} "),
        1 => format!("{label} "),
        2 => {
            let short = action.map(crate::app::fkey_short).filter(|s| !s.is_empty());
            match short {
                Some(s) => format!("{s} "),
                None => format!("{} ", label.chars().take(4).collect::<String>()),
            }
        }
        _ => " ".into(),
    }
}

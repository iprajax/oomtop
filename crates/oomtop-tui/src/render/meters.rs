//! htop-style meter block (UX §2): per-core CPU bars `0[|||||      21.5%]` in 1–4 columns, then `Mem[…]`,
//! `Swp[…]` (and `GPU[…]`) on the left and `Tasks: … thr; … running`, `Load average: …`, `Uptime: …` on the
//! right. The Mem bar is oomtop's truth (apps · gpu/metal · compressed · wired · cache), each segment with its
//! own character so it reads without color (htop's monochrome convention); the numbers are always printed.
//!
//! Degrades by width and height: per-core → one aggregate `CPU[…]` bar (compact widths, first frame without
//! per-core data, too many cores for the space) → one-row mini meters on very short terminals.

use super::{put, Ctx};
use crate::app::View;
use crate::layout::{HitTarget, LayoutClass, Regions};
use oomtop_core::CoreKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Characters of the Mem bar segments: apps, gpu/metal, compressed, wired, cache (same in ASCII and Unicode,
/// like htop, so the bar looks like htop and still distinguishes segments on a monochrome terminal).
pub(crate) const MEM_CHARS: [&str; 5] = ["|", "#", "*", "=", "."];
/// Fill character of CPU / swap / GPU bars.
const FILL: &str = "|";
/// At most this many rows of per-core bars before columns are added (or the aggregate bar is used).
const MAX_CORE_ROWS: usize = 4;
/// Narrowest per-core column that still shows `label[||| 100.0%]` readably.
const MIN_CORE_COL: usize = 18;
/// Below this terminal height the meters collapse to one row.
const MINI_BELOW_HEIGHT: u16 = 16;

/// CPU/GPU fill token by load: state colors carry "how busy" (the percentage is always printed too).
fn load_token(pct: f64) -> &'static str {
    if pct >= 90.0 {
        "state.crit"
    } else if pct >= 75.0 {
        "state.warn"
    } else {
        "state.ok"
    }
}

/// One `label[bar   text]` meter: `segs` = (cells, char, style) filled left to right over `inner` cells, with
/// `text` right-aligned inside the brackets (it overwrites the bar's tail, as in htop).
fn meter<'a>(
    c: &Ctx,
    label: &str,
    label_w: usize,
    inner: usize,
    segs: &[(usize, &str, Style)],
    text: &str,
) -> Vec<Span<'a>> {
    let mut cells: Vec<(&str, Style)> = Vec::with_capacity(inner);
    for (n, ch, st) in segs {
        for _ in 0..*n {
            if cells.len() < inner {
                cells.push((ch, *st));
            }
        }
    }
    let track = c.t("chart.bar.track");
    while cells.len() < inner {
        cells.push((" ", track));
    }
    let text: String = if text.width() > inner {
        text.chars()
            .rev()
            .take(inner)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    } else {
        text.to_string()
    };
    let tw = text.width();
    let num = c.t("ui.muted");
    let mut spans = Vec::new();
    let pad = label_w.saturating_sub(label.width());
    spans.push(Span::styled(
        format!("{}{label}", " ".repeat(pad)),
        c.t("text.label"),
    ));
    spans.push(Span::styled("[".to_string(), c.t("text.key")));
    // merge runs of equal style
    let bar_end = inner - tw;
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, st) in cells.iter().take(bar_end) {
        if run_style != Some(*st) {
            if let Some(s) = run_style {
                spans.push(Span::styled(std::mem::take(&mut run), s));
            }
            run_style = Some(*st);
        }
        run.push_str(ch);
    }
    if let Some(s) = run_style {
        spans.push(Span::styled(run, s));
    }
    spans.push(Span::styled(text, num));
    spans.push(Span::styled("]".to_string(), c.t("text.key")));
    spans
}

fn pct_cells(pct: f64, inner: usize) -> usize {
    ((pct.clamp(0.0, 100.0) / 100.0) * inner as f64).round() as usize
}

fn cpu_meter<'a>(c: &Ctx, label: &str, label_w: usize, width: usize, pct: Option<f64>) -> Vec<Span<'a>> {
    let inner = width.saturating_sub(label_w + 2).max(1);
    match pct {
        Some(p) => meter(
            c,
            label,
            label_w,
            inner,
            &[(pct_cells(p, inner), FILL, c.t(load_token(p)))],
            &format!("{p:.1}%"),
        ),
        None => meter(c, label, label_w, inner, &[], "n/a"),
    }
}

fn mem_meter<'a>(c: &Ctx, label_w: usize, width: usize) -> Vec<Span<'a>> {
    let s = &c.app.snapshot;
    let f = &c.app.fmt;
    let total = s.memory.total.value.unwrap_or(s.host.mem_total);
    let gpu = super::header::unified_gpu(c);
    let seg = super::header::memory_segments(s, gpu);
    let inner = width.saturating_sub(label_w + 2).max(1);
    let cells = super::widgets::segment_cells(&seg, total, inner);
    let toks = ["mem.app", "mem.gpu", "mem.compressed", "mem.wired", "mem.cache"];
    let segs: Vec<(usize, &str, Style)> = (0..5).map(|i| (cells[i], MEM_CHARS[i], c.t(toks[i]))).collect();
    let used: u64 = seg[..4].iter().sum();
    let text = if s.memory.total.value.is_some() || total > 0 {
        format!("{}/{}", f.bytes(used), f.bytes(total))
    } else {
        "n/a".into()
    };
    meter(c, "Mem", label_w, inner, &segs, &text)
}

fn swap_meter<'a>(c: &Ctx, label_w: usize, width: usize) -> Vec<Span<'a>> {
    let m = &c.app.snapshot.memory;
    let f = &c.app.fmt;
    let inner = width.saturating_sub(label_w + 2).max(1);
    match (m.swap_used.value, m.swap_total.value) {
        (Some(u), Some(t)) => {
            let pct = if t > 0 { u as f64 / t as f64 * 100.0 } else { 0.0 };
            let tok = if pct >= 95.0 {
                "state.crit"
            } else if pct >= 80.0 {
                "state.warn"
            } else {
                "mem.swap"
            };
            meter(
                c,
                "Swp",
                label_w,
                inner,
                &[(pct_cells(pct, inner), FILL, c.t(tok))],
                &format!("{}/{}", f.bytes(u), f.bytes(t)),
            )
        }
        _ => meter(c, "Swp", label_w, inner, &[], "n/a"),
    }
}

fn gpu_meter<'a>(c: &Ctx, label_w: usize, width: usize) -> Option<Vec<Span<'a>>> {
    let a = c.app.snapshot.accelerators.first()?;
    Some(cpu_meter(c, "GPU", label_w, width, a.util_pct.value))
}

/// "58 days, 11:23:04" (htop's format); `None` when the boot time is unknown.
pub(crate) fn uptime_text(boot_ms: Option<u64>, now_ms: u64) -> Option<String> {
    let secs = now_ms.checked_sub(boot_ms?)? / 1000;
    let (d, rem) = (secs / 86_400, secs % 86_400);
    let hms = format!("{:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60);
    Some(match d {
        0 => hms,
        1 => format!("1 day, {hms}"),
        n => format!("{n} days, {hms}"),
    })
}

/// Process / thread / running counts: the collector's [`oomtop_core::TaskCounts`] when filled, else derived
/// from the process list.
pub(crate) fn task_counts(s: &oomtop_core::Snapshot) -> (u32, Option<u32>, u32) {
    let t = &s.cpu.tasks;
    if t.processes > 0 {
        return (t.processes, t.threads, t.running);
    }
    let threads: Vec<u32> = s.processes.iter().filter_map(|p| p.threads).collect();
    let running = s
        .processes
        .iter()
        .filter(|p| p.state == oomtop_core::ProcState::Running)
        .count() as u32;
    (
        s.processes.len() as u32,
        (!threads.is_empty()).then(|| threads.iter().sum()),
        running,
    )
}

/// Right column lines: Tasks, Load average, Uptime (+ host when there is a spare row).
fn right_lines<'a>(c: &Ctx, rows: usize) -> Vec<Vec<Span<'a>>> {
    let s = &c.app.snapshot;
    let lab = c.t("text.label");
    let num = c.t("ui.accent");
    let mut out = Vec::new();
    let (procs, threads, running) = task_counts(s);
    let mut tasks = vec![
        Span::styled("Tasks: ".to_string(), lab),
        Span::styled(procs.to_string(), num),
    ];
    if let Some(t) = threads {
        tasks.push(Span::styled(", ".to_string(), lab));
        tasks.push(Span::styled(t.to_string(), num));
        tasks.push(Span::styled(" thr".to_string(), lab));
    }
    tasks.push(Span::styled("; ".to_string(), lab));
    tasks.push(Span::styled(running.to_string(), num));
    tasks.push(Span::styled(" running".to_string(), lab));
    out.push(tasks);
    let cpu = &s.cpu;
    let la = [cpu.load_avg_1.value, cpu.load_avg_5.value, cpu.load_avg_15.value];
    let mut load = vec![Span::styled("Load average: ".to_string(), lab)];
    if la.iter().all(Option::is_none) {
        load.push(Span::styled("n/a".to_string(), c.t("state.stale")));
    } else {
        for (i, v) in la.iter().enumerate() {
            let t = v.map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".into());
            let st = if i == 0 { num } else { c.t("text.number") };
            if i > 0 {
                load.push(Span::raw(" "));
            }
            load.push(Span::styled(t, st));
        }
    }
    out.push(load);
    let up = uptime_text(s.host.boot_time_ms, s.taken_at_ms).unwrap_or_else(|| "n/a".into());
    out.push(vec![
        Span::styled("Uptime: ".to_string(), lab),
        Span::styled(up, num),
    ]);
    if rows > out.len() {
        let total = s.memory.total.value.unwrap_or(s.host.mem_total);
        let cores = match (s.host.cores_performance, s.host.cores_efficiency) {
            (Some(p), Some(e)) => format!("{} cores ({p}P + {e}E)", s.host.cores_logical),
            _ => format!("{} cores", s.host.cores_logical),
        };
        out.push(vec![
            Span::styled("Host: ".to_string(), lab),
            Span::styled(
                format!(
                    "{} {} {} {} {}{}",
                    oomtop_core::machine::display_name(&s.host),
                    c.g.sep,
                    cores,
                    c.g.sep,
                    c.app.fmt.bytes(total),
                    if s.host.unified_memory { " unified" } else { "" }
                ),
                c.t("text.number"),
            ),
        ]);
    }
    out
}

fn core_label(kinds: &[CoreKind], i: usize) -> String {
    match kinds.get(i) {
        Some(CoreKind::Performance) => format!("P{i}"),
        Some(CoreKind::Efficiency) => format!("E{i}"),
        _ => i.to_string(),
    }
}

fn clip<'a>(mut spans: Vec<Span<'a>>, w: usize) -> Vec<Span<'a>> {
    while spans.iter().map(|s| s.content.width()).sum::<usize>() > w && !spans.is_empty() {
        spans.pop();
    }
    spans
}

/// What the meter block will look like for this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MeterMode {
    /// Per-core bars in `cols` columns.
    PerCore { cols: usize, rows: usize },
    /// One `CPU[…]` bar on the left with Mem/Swp.
    Aggregate,
    /// One row: `CPU[…] Mem[…] Swp[…]`.
    Mini,
}

pub(crate) fn mode_for(c: &Ctx, area: Rect) -> MeterMode {
    let w = area.width as usize;
    if area.height < MINI_BELOW_HEIGHT || w < 50 {
        return MeterMode::Mini;
    }
    let n = c.app.snapshot.cpu.per_core_pct.len();
    if n == 0 || c.class == LayoutClass::Compact {
        return MeterMode::Aggregate;
    }
    let max_rows = if area.height >= 40 { MAX_CORE_ROWS } else { 3 };
    let mut cols = if c.class == LayoutClass::Wide { 4 } else { 2 };
    while n.div_ceil(cols) > max_rows && w / (cols + 1) >= MIN_CORE_COL {
        cols += 1;
    }
    let rows = n.div_ceil(cols);
    if rows > max_rows {
        return MeterMode::Aggregate;
    }
    MeterMode::PerCore { cols, rows }
}

/// Draws the meter block at `y`; returns the rows used.
pub(crate) fn render(c: &Ctx, buf: &mut Buffer, area: Rect, y: u16, regions: &mut Regions) -> u16 {
    let w = area.width as usize;
    let s = &c.app.snapshot;
    let mode = mode_for(c, area);
    let procs = HitTarget::View(View::Processes);
    if mode == MeterMode::Mini {
        let third = w / 3;
        let mut spans = cpu_meter(c, "CPU", 3, third.saturating_sub(1), s.cpu.total_pct.value);
        spans.push(Span::raw(" "));
        spans.extend(mem_meter(c, 3, third.saturating_sub(1)));
        spans.push(Span::raw(" "));
        spans.extend(swap_meter(c, 3, w - 2 * third));
        put(buf, area, y, &Line::from(clip(spans, w)));
        regions.hits.push((Rect::new(area.x, y, area.width, 1), procs));
        return 1;
    }
    let mut row = y;
    let kinds = &s.cpu.core_kinds;
    let label_w = match mode {
        MeterMode::PerCore { .. } => (0..s.cpu.per_core_pct.len())
            .map(|i| core_label(kinds, i).width())
            .max()
            .unwrap_or(1)
            .max(3),
        _ => 3,
    };
    if let MeterMode::PerCore { cols, rows } = mode {
        let gap = 1;
        let col_w = (w.saturating_sub(1) + gap) / cols - gap;
        for r in 0..rows {
            let mut spans = vec![Span::raw(" ")];
            for col in 0..cols {
                let i = col * rows + r;
                let Some(p) = s.cpu.per_core_pct.get(i) else {
                    break;
                };
                if col > 0 {
                    spans.push(Span::raw(" ".repeat(gap)));
                }
                spans.extend(cpu_meter(c, &core_label(kinds, i), label_w, col_w, Some(*p)));
            }
            put(buf, area, row, &Line::from(clip(spans, w)));
            regions
                .hits
                .push((Rect::new(area.x, row, area.width, 1), procs.clone()));
            row += 1;
        }
    }
    // Bottom part: left bars, right text column (htop: Mem/Swp left, Tasks/Load/Uptime right).
    let two = w >= 70;
    let left_w = if two { (w - 1) / 2 } else { w - 1 };
    let mut left: Vec<(Vec<Span>, HitTarget)> = Vec::new();
    if mode == MeterMode::Aggregate {
        left.push((
            cpu_meter(c, "CPU", label_w, left_w, s.cpu.total_pct.value),
            procs.clone(),
        ));
    }
    left.push((mem_meter(c, label_w, left_w), procs.clone()));
    left.push((swap_meter(c, label_w, left_w), HitTarget::View(View::Timeline)));
    if let Some(g) = gpu_meter(c, label_w, left_w) {
        left.push((g, HitTarget::View(View::Models)));
    }
    let right = if two {
        right_lines(c, left.len())
    } else {
        Vec::new()
    };
    let n = left.len().max(right.len());
    let mut left = left.into_iter();
    let mut right = right.into_iter();
    for _ in 0..n {
        let mut spans = vec![Span::raw(" ")];
        let mut used = 1;
        if let Some((l, hit)) = left.next() {
            let lw: usize = l.iter().map(|s| s.content.width()).sum();
            regions
                .hits
                .push((Rect::new(area.x, row, (lw + 1) as u16, 1), hit));
            used += lw;
            spans.extend(l);
        }
        if let Some(r) = right.next() {
            let at = 1 + left_w + 2;
            spans.push(Span::raw(" ".repeat(at.saturating_sub(used))));
            spans.extend(r);
        }
        put(buf, area, row, &Line::from(clip(spans, w)));
        row += 1;
    }
    row - y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_like_htop() {
        let day = 86_400_000;
        assert_eq!(uptime_text(Some(0), 3_723_000).as_deref(), Some("01:02:03"));
        assert_eq!(
            uptime_text(Some(0), day + 5_000).as_deref(),
            Some("1 day, 00:00:05")
        );
        assert_eq!(
            uptime_text(Some(1_000), 58 * day + 41_000_000).as_deref(),
            Some("58 days, 11:23:19")
        );
        assert_eq!(uptime_text(None, 5), None);
        assert_eq!(uptime_text(Some(10), 5), None, "boot in the future → unknown");
    }

    #[test]
    fn core_labels_tag_performance_and_efficiency() {
        let k = [CoreKind::Efficiency, CoreKind::Performance];
        assert_eq!(core_label(&k, 0), "E0");
        assert_eq!(core_label(&k, 1), "P1");
        assert_eq!(core_label(&k, 2), "2", "unknown kind → plain index");
        assert_eq!(core_label(&[], 7), "7");
    }

    #[test]
    fn load_tokens_escalate() {
        assert_eq!(load_token(10.0), "state.ok");
        assert_eq!(load_token(80.0), "state.warn");
        assert_eq!(load_token(95.0), "state.crit");
    }
}

//! Declarative layouts and columns (UX §12.6), consumed from `oomtop_config::layout`.
//!
//! `layout.name = ""` / `"adaptive"` keeps the built-in adaptive screen (UX §2). A named layout
//! (`layouts/<name>.toml`, or a saved view's `layout = "…"`) decides, per size class, which slots the Home screen
//! shows and in what order (`header`, `headline`, `meters`, `your-things`, `cards`, `ranked`, `details`,
//! `timeline`, `footer`, and horizontal splits such as `{ split = ["ranked:65%", "details:35%"] }`), plus which
//! columns group and process rows carry, their widths, the default sort and custom expression columns.
//!
//! Pure: the event loop reads the file (`oomtop_config::layout::load_layout`) and hands the parsed [`Layout`]
//! to [`ActiveLayout::new`]; column widths are solved here so rendering stays a straight write.

use oomtop_config::layout::{parse_split_part, ColumnSet, CompiledColumn, Layout, Row, SizeClass, Width};

/// A named layout ready to render: the parsed file plus its compiled custom columns.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveLayout {
    pub name: String,
    pub layout: Layout,
    pub custom: Vec<CompiledColumn>,
}

impl ActiveLayout {
    /// Compiles custom columns; an invalid expression or format is an error naming the column.
    pub fn new(layout: Layout) -> Result<Self, String> {
        let custom = layout.compile_custom().map_err(|(_, msg)| msg)?;
        Ok(ActiveLayout {
            name: layout.name.clone(),
            layout,
            custom,
        })
    }

    /// Group columns, when the layout defines `[columns.groups]` (else the built-in adaptive columns).
    pub fn group_columns(&self) -> Option<&ColumnSet> {
        self.layout.columns.groups.as_ref().filter(|c| !c.show.is_empty())
    }

    /// Process columns, when the layout defines `[columns.processes]`.
    pub fn process_columns(&self) -> Option<&ColumnSet> {
        self.layout
            .columns
            .processes
            .as_ref()
            .filter(|c| !c.show.is_empty())
    }

    pub fn custom(&self, id: &str) -> Option<&CompiledColumn> {
        self.custom.iter().find(|c| c.id == id)
    }

    /// Home screen rows for a terminal width (falls back to the built-in rows of that size class).
    pub fn rows(&self, width: u16) -> Vec<SlotRow> {
        self.layout
            .screen(SizeClass::for_width(width))
            .rows
            .iter()
            .map(|r| match r {
                Row::Slot(s) => SlotRow::Slot(s.clone()),
                Row::Split { split } => SlotRow::Split(
                    split
                        .iter()
                        .filter_map(|p| parse_split_part(p).ok())
                        .map(|p| (p.slot, p.pct))
                        .collect(),
                ),
            })
            .collect()
    }

    /// Which header parts the Home screen shows: `(headline, meters)`. `header` means both.
    pub fn header_parts(&self, width: u16) -> (bool, bool) {
        let rows = self.rows(width);
        let has = |name: &str| {
            rows.iter().any(|r| match r {
                SlotRow::Slot(s) => s == name,
                SlotRow::Split(parts) => parts.iter().any(|(s, _)| s == name),
            })
        };
        let header = has("header");
        (header || has("headline"), header || has("meters"))
    }
}

/// A layout row resolved for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlotRow {
    Slot(String),
    /// (slot, width %) left to right; parts without a % share what is left.
    Split(Vec<(String, Option<u8>)>),
}

/// Splits `total` cells among parts with optional percentages (rest shared equally), never exceeding `total`.
pub fn split_widths(total: u16, parts: &[Option<u8>]) -> Vec<u16> {
    if parts.is_empty() {
        return Vec::new();
    }
    let fixed: u32 = parts.iter().flatten().map(|p| *p as u32).sum::<u32>().min(100);
    let free = parts.iter().filter(|p| p.is_none()).count() as u32;
    let share = (100 - fixed).checked_div(free).unwrap_or(0);
    let mut out: Vec<u16> = parts
        .iter()
        .map(|p| (total as u32 * p.map(|x| x as u32).unwrap_or(share) / 100) as u16)
        .collect();
    // Hand rounding leftovers to the last part so the row is filled exactly.
    let used: u32 = out.iter().map(|w| *w as u32).sum();
    if let Some(last) = out.last_mut() {
        *last = (*last as u32 + (total as u32).saturating_sub(used)) as u16;
    }
    out
}

/// Default cell width of a built-in column (including its leading space), `None` = flex.
pub fn default_width(id: &str) -> Option<usize> {
    Some(match id {
        "name" | "cmdline" => return None,
        "pid" => 8,
        "kind" => 9,
        "footprint" | "resident" | "gpu" | "swapped" | "reclaim" => 9,
        "cpu" => 7,
        "trend" => 14,
        "state" => 16,
        "idle" => 8,
        "members" => 5,
        "confidence" => 8,
        "because" => 40,
        "user" => 10,
        "group" => 22,
        _ => 10,
    })
}

/// Column title for the header line.
pub fn title(id: &str, custom: Option<&CompiledColumn>) -> String {
    if let Some(c) = custom {
        return c.title.clone();
    }
    match id {
        "footprint" => "MEM".into(),
        "resident" => "RES".into(),
        "members" => "PROCS".into(),
        "reclaim" => "FREES".into(),
        "confidence" => "CONF".into(),
        other => other.to_uppercase(),
    }
}

/// Numeric columns are right-aligned.
pub fn right_aligned(id: &str, custom: Option<&CompiledColumn>) -> bool {
    match custom {
        Some(c) => c.align_right,
        None => matches!(
            id,
            "pid" | "footprint" | "resident" | "gpu" | "swapped" | "cpu" | "idle" | "members" | "reclaim"
        ),
    }
}

/// Solves column widths for a row of `total` cells. `marker` cells are reserved on the left (selection mark).
/// Columns that do not fit are dropped from the right, but the flex column (name) is always kept with at least
/// `min_flex` cells. Returns `(id, width)` in display order.
pub fn solve(set: &ColumnSet, total: usize, marker: usize, min_flex: usize) -> Vec<(String, usize)> {
    let avail = total.saturating_sub(marker);
    let width_of = |id: &str| -> Option<usize> {
        match set.width.get(id) {
            Some(Width::Cells(n)) => Some(*n as usize + 1),
            Some(Width::Named(s)) if s == "flex" => None,
            Some(Width::Named(s)) => s
                .strip_suffix('%')
                .and_then(|p| p.trim().parse::<usize>().ok())
                .map(|p| (avail * p.min(100) / 100).max(2)),
            None => default_width(id),
        }
    };
    // (id, fixed width, is_flex). Exactly one flex column: the first flex one, else `name`, else the first.
    let mut cols: Vec<(String, usize, bool)> = Vec::new();
    let flex_pos = set
        .show
        .iter()
        .position(|id| width_of(id).is_none())
        .or_else(|| set.show.iter().position(|id| id == "name"))
        .unwrap_or(0);
    for (i, id) in set.show.iter().enumerate() {
        let flex = i == flex_pos;
        cols.push((id.clone(), width_of(id).unwrap_or(24), flex));
    }
    let fixed = |cols: &[(String, usize, bool)]| -> usize { cols.iter().filter(|c| !c.2).map(|c| c.1).sum() };
    // Drop the right-most non-flex column until the flex column gets `min_flex` cells.
    while fixed(&cols) + min_flex > avail {
        let Some(drop) = cols.iter().rposition(|c| !c.2) else {
            break;
        };
        cols.remove(drop);
    }
    let rest = avail.saturating_sub(fixed(&cols)).max(1);
    cols.into_iter()
        .map(|(id, w, flex)| (id, if flex { rest } else { w }))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use oomtop_config::layout::{parse_layout, CustomColumn};
    use std::collections::BTreeMap;
    use std::path::Path;

    const LLM_DEV: &str = r#"
name = "llm-dev"
[wide]
rows = ["header", "your-things", { split = ["ranked:65%", "details:35%"] }]
[standard]
rows = ["headline", "ranked"]
[columns.groups]
show  = ["name", "footprint", "gpu", "trend", "state", "mem_share", "because"]
width = { name = "flex", footprint = 9, gpu = 8, trend = 12 }
sort  = "footprint"
[[columns.custom]]
id = "mem_share"
title = "MEM%"
expr = "footprint / host.mem.total * 100"
format = "{:.0}%"
"#;

    pub(crate) fn llm_dev() -> ActiveLayout {
        ActiveLayout::new(parse_layout(LLM_DEV, Path::new("llm-dev.toml")).unwrap()).unwrap()
    }

    #[test]
    fn rows_and_header_parts_per_size_class() {
        let l = llm_dev();
        assert_eq!(
            l.rows(160),
            vec![
                SlotRow::Slot("header".into()),
                SlotRow::Slot("your-things".into()),
                SlotRow::Split(vec![("ranked".into(), Some(65)), ("details".into(), Some(35))]),
            ]
        );
        assert_eq!(l.header_parts(160), (true, true));
        assert_eq!(l.header_parts(100), (true, false), "standard: headline only");
        // compact is not defined → built-in compact rows (headline, your-things, ranked)
        assert_eq!(l.header_parts(60), (true, false));
        assert!(l.custom("mem_share").is_some());
        assert_eq!(l.group_columns().unwrap().sort.as_deref(), Some("footprint"));
        assert!(l.process_columns().is_none());
    }

    #[test]
    fn invalid_custom_column_is_an_error() {
        let mut layout = parse_layout(LLM_DEV, Path::new("llm-dev.toml")).unwrap();
        layout.columns.custom.push(CustomColumn {
            id: "bad".into(),
            expr: "nosuchfield * 2".into(),
            ..Default::default()
        });
        let e = ActiveLayout::new(layout).unwrap_err();
        assert!(e.contains("bad"), "{e}");
    }

    #[test]
    fn split_widths_fill_exactly() {
        assert_eq!(split_widths(100, &[Some(65), Some(35)]), vec![65, 35]);
        assert_eq!(split_widths(101, &[Some(65), Some(35)]), vec![65, 36]);
        assert_eq!(split_widths(90, &[Some(50), None, None]), vec![45, 22, 23]);
        assert!(split_widths(10, &[]).is_empty());
    }

    #[test]
    fn solve_keeps_flex_and_drops_from_the_right() {
        let set = ColumnSet {
            show: ["name", "footprint", "gpu", "trend", "because"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            width: BTreeMap::from([("gpu".to_string(), Width::Cells(8))]),
            sort: None,
        };
        let wide = solve(&set, 140, 3, 20);
        let ids: Vec<&str> = wide.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(ids, ["name", "footprint", "gpu", "trend", "because"]);
        assert_eq!(wide.iter().map(|(_, w)| w).sum::<usize>(), 137, "fills the row");
        assert_eq!(wide[2].1, 9, "explicit width + separator");
        let narrow = solve(&set, 60, 3, 20);
        let ids: Vec<&str> = narrow.iter().map(|(i, _)| i.as_str()).collect();
        assert_eq!(
            ids,
            ["name", "footprint", "gpu", "trend"],
            "because dropped first"
        );
        assert!(narrow[0].1 >= 20);
        let tiny = solve(&set, 30, 3, 20);
        assert_eq!(tiny.len(), 1, "only the flex column survives");
        assert_eq!(tiny[0].1, 27);
    }
}

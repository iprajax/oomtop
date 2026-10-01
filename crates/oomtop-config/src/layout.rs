//! Layouts & columns (UX §12.6): declarative panel/column layouts in `layouts/<name>.toml`; the ranked
//! serving slots (UX §5.5) fill them. Custom columns are small sandboxed expressions ([`crate::expr`]).
//!
//! ```toml
//! # ~/.config/oomtop/layouts/llm-dev.toml
//! name = "llm-dev"
//! [wide]                     # ≥ 140 cols; also [standard] (80–139) and [compact] (< 80)
//! rows = ["header", "your-things", { split = ["ranked:65%", "details:35%"] }]
//! [columns.groups]
//! show  = ["name", "footprint", "gpu", "trend", "state", "because"]
//! width = { name = "flex", footprint = 9, gpu = 8, trend = 12 }
//! sort  = "rank"             # rank (personalized) | footprint | gpu | cpu | name | idle | reclaim
//! [[columns.custom]]
//! id = "mem_share"
//! title = "MEM%"
//! expr = "footprint / host.mem.total * 100"
//! format = "{:.0}%"
//! ```

use crate::expr::{check_format, compile, format_value, Expr};
use crate::layered::{closest, key_lines, line_of};
use crate::ConfigError;
use oomtop_core::{Group, Snapshot};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Slots a layout row can hold (UX §2, §5.5).
pub const SLOTS: &[&str] = &[
    "header",
    "headline",
    "meters",
    "your-things",
    "cards",
    "ranked",
    "details",
    "timeline",
    "footer",
];

/// Built-in group columns.
pub const GROUP_COLUMNS: &[&str] = &[
    "name",
    "kind",
    "footprint",
    "resident",
    "gpu",
    "swapped",
    "cpu",
    "trend",
    "state",
    "idle",
    "members",
    "reclaim",
    "confidence",
    "because",
];

/// Built-in process columns.
pub const PROCESS_COLUMNS: &[&str] = &[
    "pid",
    "name",
    "user",
    "group",
    "footprint",
    "resident",
    "gpu",
    "swapped",
    "cpu",
    "state",
    "idle",
    "trend",
    "cmdline",
];

/// Sort keys.
pub const SORTS: &[&str] = &["rank", "footprint", "gpu", "cpu", "name", "idle", "reclaim"];

/// Fields available to custom column expressions: (name, description). Byte fields are bytes, times seconds,
/// CPU in % of one core, flags 1/0.
pub const ENTITY_FIELDS: &[(&str, &str)] = &[
    ("footprint", "group footprint / PSS in bytes (true memory)"),
    ("resident", "resident set in bytes"),
    ("gpu", "GPU memory in bytes"),
    ("swapped", "swapped bytes (Linux; unavailable on macOS)"),
    ("cpu", "CPU % of one core"),
    ("members", "number of processes"),
    ("reclaim", "estimated RAM freed if stopped, bytes"),
    ("swap_gain", "estimated swap freed if stopped, bytes"),
    ("idle_s", "seconds idle"),
    ("idle", "1 if idle"),
    ("orphan", "1 if orphaned"),
    ("protected", "1 if protected"),
    ("lower_bound", "1 if footprint is a lower bound (VMs)"),
    ("configured_mem", "configured VM memory, bytes"),
    ("host.mem.total", "host RAM, bytes"),
    ("host.mem.available", "host available memory, bytes"),
    ("host.mem.free", "host free memory, bytes"),
    ("host.mem.cached", "host file cache, bytes"),
    ("host.mem.wired", "host wired memory, bytes"),
    ("host.mem.compressed", "host compressor / zswap pool, bytes"),
    ("host.mem.app", "host anonymous app memory, bytes"),
    ("host.swap.used", "swap used, bytes"),
    ("host.swap.total", "swap total, bytes"),
    ("host.cpu.total", "machine CPU %, 0..100"),
    ("host.cores", "logical cores"),
];

/// Names of [`ENTITY_FIELDS`].
pub fn field_names() -> Vec<&'static str> {
    ENTITY_FIELDS.iter().map(|(n, _)| *n).collect()
}

/// Terminal size class (UX §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeClass {
    /// ≥ 140 columns.
    Wide,
    /// 80–139 columns.
    Standard,
    /// < 80 columns.
    Compact,
}

impl SizeClass {
    pub fn for_width(cols: u16) -> SizeClass {
        match cols {
            140.. => SizeClass::Wide,
            80..=139 => SizeClass::Standard,
            _ => SizeClass::Compact,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            SizeClass::Wide => "wide",
            SizeClass::Standard => "standard",
            SizeClass::Compact => "compact",
        }
    }
}

/// One layout row: a slot name, or a horizontal split `{ split = ["ranked:65%", "details:35%"] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Row {
    Slot(String),
    Split { split: Vec<String> },
}

/// A parsed split part: slot + optional percentage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPart {
    pub slot: String,
    pub pct: Option<u8>,
}

/// `"ranked:65%"` → (ranked, 65).
pub fn parse_split_part(s: &str) -> Result<SplitPart, String> {
    match s.split_once(':') {
        None => Ok(SplitPart {
            slot: s.trim().to_string(),
            pct: None,
        }),
        Some((slot, pct)) => {
            let n = pct
                .trim()
                .strip_suffix('%')
                .unwrap_or(pct.trim())
                .parse::<u8>()
                .map_err(|_| format!("bad width {pct:?} in {s:?} (use e.g. \"ranked:65%\")"))?;
            if n == 0 || n > 100 {
                return Err(format!("width {n}% out of range in {s:?}"));
            }
            Ok(SplitPart {
                slot: slot.trim().to_string(),
                pct: Some(n),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Screen {
    /// Rows top to bottom.
    pub rows: Vec<Row>,
}

/// Column width: a number of cells, `"flex"` (take the rest) or a percentage `"20%"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Width {
    Cells(u16),
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ColumnSet {
    /// Columns in display order (built-in ids or custom column ids).
    pub show: Vec<String>,
    pub width: BTreeMap<String, Width>,
    /// rank (personalized) | footprint | gpu | cpu | name | idle | reclaim
    pub sort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CustomColumn {
    pub id: String,
    pub title: String,
    /// Expression over entity fields (see `oomtop_config::layout::ENTITY_FIELDS`).
    pub expr: String,
    /// `{}`, `{:.N}`, `{:bytes}`, `{:dur}` or `{:pct}` with optional surrounding text.
    pub format: String,
    /// left | right (numbers default to right).
    pub align: String,
}

impl Default for CustomColumn {
    fn default() -> Self {
        CustomColumn {
            id: String::new(),
            title: String::new(),
            expr: String::new(),
            format: "{}".into(),
            align: "right".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Columns {
    pub groups: Option<ColumnSet>,
    pub processes: Option<ColumnSet>,
    pub custom: Vec<CustomColumn>,
}

/// A layout file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub name: String,
    pub wide: Option<Screen>,
    pub standard: Option<Screen>,
    pub compact: Option<Screen>,
    pub columns: Columns,
}

fn slot(s: &str) -> Row {
    Row::Slot(s.to_string())
}

/// The built-in adaptive layout (UX §2).
pub fn builtin_layout() -> Layout {
    let cols = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    Layout {
        name: "adaptive".into(),
        wide: Some(Screen {
            rows: vec![
                slot("header"),
                slot("your-things"),
                slot("ranked"),
                slot("details"),
            ],
        }),
        standard: Some(Screen {
            rows: vec![slot("header"), slot("your-things"), slot("ranked")],
        }),
        compact: Some(Screen {
            rows: vec![slot("headline"), slot("your-things"), slot("ranked")],
        }),
        columns: Columns {
            groups: Some(ColumnSet {
                show: cols(&["name", "footprint", "gpu", "trend", "state", "because"]),
                width: BTreeMap::from([
                    ("name".into(), Width::Named("flex".into())),
                    ("footprint".into(), Width::Cells(9)),
                    ("gpu".into(), Width::Cells(8)),
                    ("trend".into(), Width::Cells(7)),
                ]),
                sort: Some("rank".into()),
            }),
            processes: Some(ColumnSet {
                show: cols(&["pid", "name", "footprint", "resident", "cpu", "state", "group"]),
                width: BTreeMap::from([("name".into(), Width::Named("flex".into()))]),
                sort: Some("footprint".into()),
            }),
            custom: Vec::new(),
        },
    }
}

/// A custom column ready to evaluate.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledColumn {
    pub id: String,
    pub title: String,
    pub expr: Expr,
    pub format: String,
    pub align_right: bool,
}

impl CompiledColumn {
    /// Evaluates and formats for one entity.
    pub fn render(&self, fields: &Fields) -> String {
        format_value(
            &self.format,
            self.expr.eval(&|f| fields.get(f).copied().flatten()),
        )
    }
}

/// Field values for one entity (`None` = unavailable).
pub type Fields = BTreeMap<&'static str, Option<f64>>;

/// Field values of a group (and its host) for custom columns. Pure.
pub fn group_fields(g: &Group, s: &Snapshot) -> Fields {
    let b = |m: &oomtop_core::Measured<u64>| m.value.map(|v| v as f64);
    let flag = |x: bool| Some(if x { 1.0 } else { 0.0 });
    let m = &s.memory;
    let total = m
        .total
        .value
        .or((s.host.mem_total > 0).then_some(s.host.mem_total))
        .map(|v| v as f64);
    BTreeMap::from([
        ("footprint", b(&g.totals.footprint)),
        ("resident", b(&g.totals.resident)),
        ("gpu", b(&g.totals.gpu)),
        ("swapped", b(&g.totals.swapped)),
        ("cpu", g.totals.cpu_pct.value),
        (
            "members",
            Some(g.members.len().max(g.totals.process_count as usize) as f64),
        ),
        ("reclaim", b(&g.reclaim_gain)),
        ("swap_gain", b(&g.swap_gain)),
        ("idle_s", g.idle_for_s.map(|v| v as f64)),
        ("idle", flag(g.idle)),
        ("orphan", flag(g.orphan)),
        ("protected", flag(g.protected)),
        ("lower_bound", flag(g.lower_bound)),
        ("configured_mem", g.configured_mem.map(|v| v as f64)),
        ("host.mem.total", total),
        ("host.mem.available", b(&m.available)),
        ("host.mem.free", b(&m.free)),
        ("host.mem.cached", b(&m.cached)),
        ("host.mem.wired", b(&m.wired)),
        ("host.mem.compressed", b(&m.compressed)),
        ("host.mem.app", b(&m.app)),
        ("host.swap.used", b(&m.swap_used)),
        ("host.swap.total", b(&m.swap_total)),
        ("host.cpu.total", s.cpu.total_pct.value),
        (
            "host.cores",
            (s.host.cores_logical > 0).then_some(s.host.cores_logical as f64),
        ),
    ])
}

impl Layout {
    /// The screen for a size class, falling back to the built-in layout's.
    pub fn screen(&self, size: SizeClass) -> Screen {
        let pick = |l: &Layout| match size {
            SizeClass::Wide => l.wide.clone(),
            SizeClass::Standard => l.standard.clone(),
            SizeClass::Compact => l.compact.clone(),
        };
        pick(self).or_else(|| pick(&builtin_layout())).unwrap_or_default()
    }

    /// Group columns (falling back to the built-in set).
    pub fn group_columns(&self) -> ColumnSet {
        self.columns
            .groups
            .clone()
            .or_else(|| builtin_layout().columns.groups)
            .unwrap_or_default()
    }

    /// Process columns (falling back to the built-in set).
    pub fn process_columns(&self) -> ColumnSet {
        self.columns
            .processes
            .clone()
            .or_else(|| builtin_layout().columns.processes)
            .unwrap_or_default()
    }

    /// Compiles custom columns (validated fields and formats).
    pub fn compile_custom(&self) -> Result<Vec<CompiledColumn>, (String, String)> {
        let fields = field_names();
        self.columns
            .custom
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let key = format!("columns.custom.{i}");
                let expr = compile(&c.expr, Some(&fields))
                    .map_err(|e| (format!("{key}.expr"), format!("column {:?}: expr {e}", c.id)))?;
                check_format(&c.format)
                    .map_err(|e| (format!("{key}.format"), format!("column {:?}: {e}", c.id)))?;
                Ok(CompiledColumn {
                    id: c.id.clone(),
                    title: if c.title.is_empty() {
                        c.id.clone()
                    } else {
                        c.title.clone()
                    },
                    expr,
                    format: c.format.clone(),
                    align_right: c.align != "left",
                })
            })
            .collect()
    }

    /// Semantic checks: (dotted key, message) pairs.
    pub fn problems(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let custom_ids: Vec<&str> = self.columns.custom.iter().map(|c| c.id.as_str()).collect();
        for (size, screen) in [
            ("wide", &self.wide),
            ("standard", &self.standard),
            ("compact", &self.compact),
        ] {
            let Some(screen) = screen else { continue };
            let key = format!("{size}.rows");
            if screen.rows.is_empty() {
                out.push((key.clone(), format!("[{size}] rows is empty")));
            }
            for row in &screen.rows {
                let parts: Vec<Result<SplitPart, String>> = match row {
                    Row::Slot(s) => vec![Ok(SplitPart {
                        slot: s.clone(),
                        pct: None,
                    })],
                    Row::Split { split } => split.iter().map(|p| parse_split_part(p)).collect(),
                };
                let mut total = 0u32;
                for p in parts {
                    match p {
                        Err(e) => out.push((key.clone(), format!("[{size}] {e}"))),
                        Ok(p) => {
                            total += p.pct.unwrap_or(0) as u32;
                            if !SLOTS.contains(&p.slot.as_str()) {
                                out.push((key.clone(), unknown("slot", &p.slot, SLOTS)));
                            }
                        }
                    }
                }
                if total > 100 {
                    out.push((
                        key.clone(),
                        format!("[{size}] split widths add up to {total}% (> 100%)"),
                    ));
                }
                if let Row::Split { split } = row {
                    if split.len() < 2 {
                        out.push((key.clone(), format!("[{size}] a split needs at least two parts")));
                    }
                }
            }
        }
        for (name, set, builtin) in [
            ("groups", &self.columns.groups, GROUP_COLUMNS),
            ("processes", &self.columns.processes, PROCESS_COLUMNS),
        ] {
            let Some(set) = set else { continue };
            let known: Vec<&str> = builtin
                .iter()
                .copied()
                .chain(custom_ids.iter().copied())
                .collect();
            for c in &set.show {
                if !known.contains(&c.as_str()) {
                    out.push((format!("columns.{name}.show"), unknown("column", c, &known)));
                }
            }
            for (c, w) in &set.width {
                if !known.contains(&c.as_str()) {
                    out.push((format!("columns.{name}.width.{c}"), unknown("column", c, &known)));
                }
                if let Width::Named(n) = w {
                    let pct_ok = n
                        .strip_suffix('%')
                        .and_then(|p| p.parse::<u8>().ok())
                        .is_some_and(|p| (1..=100).contains(&p));
                    if n != "flex" && !pct_ok {
                        out.push((
                            format!("columns.{name}.width.{c}"),
                            format!("width {n:?} for {c}: use a number of cells, \"flex\" or \"NN%\""),
                        ));
                    }
                }
            }
            if let Some(sort) = &set.sort {
                if !SORTS.contains(&sort.as_str()) && !custom_ids.contains(&sort.as_str()) {
                    out.push((format!("columns.{name}.sort"), unknown("sort", sort, SORTS)));
                }
            }
        }
        let mut seen = Vec::new();
        for (i, c) in self.columns.custom.iter().enumerate() {
            let key = format!("columns.custom.{i}");
            let valid_id = !c.id.is_empty()
                && c.id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-');
            if !valid_id {
                out.push((
                    format!("{key}.id"),
                    format!("custom column id {:?} must be [a-z0-9_-]+", c.id),
                ));
            }
            if GROUP_COLUMNS.contains(&c.id.as_str()) || PROCESS_COLUMNS.contains(&c.id.as_str()) {
                out.push((
                    format!("{key}.id"),
                    format!("custom column id {:?} shadows a built-in column", c.id),
                ));
            }
            if seen.contains(&c.id) {
                out.push((
                    format!("{key}.id"),
                    format!("duplicate custom column id {:?}", c.id),
                ));
            }
            seen.push(c.id.clone());
            if !matches!(c.align.as_str(), "left" | "right") {
                out.push((
                    format!("{key}.align"),
                    format!("align must be left or right, not {:?}", c.align),
                ));
            }
        }
        if let Err((k, m)) = self.compile_custom() {
            out.push((k, m));
        }
        out
    }
}

fn unknown(what: &str, name: &str, known: &[&str]) -> String {
    match closest(name, known.iter().copied()) {
        Some(c) => format!("unknown {what} {name:?} — did you mean {c:?}?"),
        None => format!("unknown {what} {name:?} (known: {})", known.join(", ")),
    }
}

/// Parses and validates a layout file; every problem carries file:line.
pub fn parse_layout(text: &str, path: &Path) -> Result<Layout, Vec<ConfigError>> {
    let mut layout: Layout = toml::from_str(text).map_err(|e| {
        vec![ConfigError {
            path: Some(path.to_path_buf()),
            line: e.span().map(|s| line_of(text, s.start)),
            message: crate::layered::hint_for(e.message())
                .map(|h| format!("{} — {h}", e.message().trim()))
                .unwrap_or_else(|| e.message().trim().to_string()),
        }]
    })?;
    if layout.name.is_empty() {
        layout.name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
    }
    let problems = layout.problems();
    if problems.is_empty() {
        return Ok(layout);
    }
    let lines = key_lines(text);
    Err(problems
        .into_iter()
        .map(|(key, message)| {
            let mut k = key.as_str();
            let line = loop {
                if let Some(l) = lines.get(k) {
                    break Some(*l);
                }
                match k.rsplit_once('.') {
                    Some((p, _)) => k = p,
                    None => break None,
                }
            };
            ConfigError {
                path: Some(path.to_path_buf()),
                line,
                message,
            }
        })
        .collect())
}

/// Loads `layouts/<name>.toml`; an empty name or `"adaptive"` is the built-in layout.
pub fn load_layout(name: &str, layouts_dir: &Path) -> Result<Layout, Vec<ConfigError>> {
    if name.is_empty() || name == "adaptive" {
        return Ok(builtin_layout());
    }
    let p = layouts_dir.join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&p).map_err(|e| {
        vec![ConfigError {
            path: Some(p.clone()),
            line: None,
            message: format!("layout {name:?}: {e}"),
        }]
    })?;
    parse_layout(&text, &p)
}

/// Layout names: `adaptive` plus `layouts/*.toml`.
pub fn list_layouts(layouts_dir: &Path) -> Vec<String> {
    let mut v = vec!["adaptive".to_string()];
    v.extend(
        crate::paths::toml_files(layouts_dir)
            .iter()
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())),
    );
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::{GroupTotals, Measured};

    const UX_EXAMPLE: &str = r#"# ~/.config/oomtop/layouts/llm-dev.toml
name = "llm-dev"
[wide]                     # ≥ 140 cols; also [standard] and [compact]
rows = ["header", "your-things", { split = ["ranked:65%", "details:35%"] }]
[columns.groups]
show  = ["name", "footprint", "gpu", "trend", "state", "because", "mem_share"]
width = { name = "flex", footprint = 9, gpu = 8, trend = 12 }
sort  = "rank"             # rank (personalized) | footprint | gpu | cpu | name

[[columns.custom]]
id = "mem_share"
title = "MEM%"
expr = "footprint / host.mem.total * 100"
format = "{:.0}%"
"#;

    #[test]
    fn ux_example_parses_and_evaluates() {
        let l = parse_layout(UX_EXAMPLE, Path::new("llm-dev.toml")).unwrap();
        assert_eq!(l.name, "llm-dev");
        let wide = l.screen(SizeClass::Wide);
        assert_eq!(wide.rows.len(), 3);
        assert_eq!(
            wide.rows[2],
            Row::Split {
                split: vec!["ranked:65%".into(), "details:35%".into()]
            }
        );
        assert_eq!(
            parse_split_part("ranked:65%").unwrap(),
            SplitPart {
                slot: "ranked".into(),
                pct: Some(65)
            }
        );
        // standard falls back to the built-in layout
        assert_eq!(l.screen(SizeClass::Standard), builtin_layout().standard.unwrap());
        let cols = l.compile_custom().unwrap();
        let mut snap = Snapshot::default();
        snap.memory.total = Measured::exact(24 * 1024 * 1024 * 1024, "sysctl hw.memsize");
        let g = Group {
            totals: GroupTotals {
                footprint: Measured::exact(10_630_000_000, "footprint"),
                ..Default::default()
            },
            ..Default::default()
        };
        let f = group_fields(&g, &snap);
        assert_eq!(cols[0].render(&f), "41%");
        assert_eq!(cols[0].title, "MEM%");
        // unavailable → "–", never 0
        let empty = group_fields(&Group::default(), &Snapshot::default());
        assert_eq!(cols[0].render(&empty), "–%");
        assert_eq!(f.len(), ENTITY_FIELDS.len(), "every documented field is provided");
    }

    #[test]
    fn problems_have_lines() {
        let text = "[wide]\nrows = [\"header\", \"rankd\", { split = [\"ranked:80%\", \"details:40%\"] }]\n[columns.groups]\nshow = [\"name\", \"footprnt\"]\nwidth = { name = \"wide\" }\nsort = \"size\"\n[[columns.custom]]\nid = \"cpu\"\nexpr = \"cpu *\"\nformat = \"{:.0}\"\n";
        let errs = parse_layout(text, Path::new("bad.toml")).unwrap_err();
        let msgs: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
        assert!(
            msgs.iter()
                .any(|m| m.contains("bad.toml:2: unknown slot \"rankd\" — did you mean \"ranked\"?")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("bad.toml:2: [wide] split widths add up to 120%")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("bad.toml:4: unknown column \"footprnt\"")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("bad.toml:5: width \"wide\"")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("bad.toml:6: unknown sort \"size\"")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("bad.toml:8: custom column id \"cpu\" shadows")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("bad.toml:9: column \"cpu\": expr")),
            "{msgs:#?}"
        );
        let e = parse_layout("[wide]\nrow = []\n", Path::new("x.toml")).unwrap_err();
        assert!(
            e[0].to_string().contains("x.toml:2: unknown field `row`"),
            "{}",
            e[0]
        );
        assert!(e[0].message.contains("did you mean `rows`?"));
    }

    #[test]
    fn load_and_list() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("llm-dev.toml"), UX_EXAMPLE).unwrap();
        assert_eq!(
            list_layouts(d.path()),
            vec!["adaptive".to_string(), "llm-dev".into()]
        );
        assert_eq!(load_layout("", d.path()).unwrap(), builtin_layout());
        assert_eq!(load_layout("llm-dev", d.path()).unwrap().name, "llm-dev");
        assert!(load_layout("nope", d.path()).is_err());
        assert!(builtin_layout().problems().is_empty());
        assert_eq!(SizeClass::for_width(200), SizeClass::Wide);
        assert_eq!(SizeClass::for_width(80), SizeClass::Standard);
        assert_eq!(SizeClass::for_width(60), SizeClass::Compact);
    }
}

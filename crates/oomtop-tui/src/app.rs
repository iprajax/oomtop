//! TUI state machine (UX §2–§9): snapshot → headroom → mode → ranking → served rows; keys/mouse → effects.
//! Pure — no terminal or file I/O — so every behaviour is unit-testable with fixture snapshots; `lib.rs` owns
//! the event loop and performs [`Effect`]s (signals via the `Actuator`, learning via `oomtop-state`, config
//! writes via `oomtop-config`).
//!
//! Safety rules implemented here (SPEC §7, §13): actions target group roots only; every stop/suspend is an
//! inline confirmation naming the target and its estimated gain; SIGTERM first, SIGKILL only after a second
//! explicit confirmation on a target that ignored SIGTERM; `(pid, start_time)` is re-verified against the
//! latest snapshot before a plan is released (and again by the actuator right before signalling); protected
//! groups, oomtop itself and its ancestors are refused.

use crate::format::{ordinal, Fmt};
use crate::layout::{HitTarget, LayoutClass, Regions};
use crate::settings::{SettingsCmd, SettingsModel, SettingsState};
use crate::timeline::{Frame, FrameRow, Marker, Timeline};
use oomtop_config::keymap::Keymap;
use oomtop_config::model::{Motion, SavedView, Sparklines};
use oomtop_config::Config;
use oomtop_core::actions::{
    is_protected_process, plan_group, ActionKind, ActionOutcome, ActionPlan, ActionTarget, ProtectContext,
};
use oomtop_core::can_fit::{can_fit, reclaim_candidates, Fit, Need};
use oomtop_core::headline::{input_from, render as render_headline, Headline};
use oomtop_core::headroom::{compute, is_reclaim_candidate, Headroom, HeadroomConfig};
use oomtop_core::history::History;
use oomtop_core::modes::{signals, Mode, ModeMachine};
use oomtop_core::query::{
    eval, link_entities, parse_filter, understand, EntityView, Expr, Intent, Metric, VocabEntity, Vocabulary,
};
use oomtop_core::ranking::{
    explain_rank, group_candidates, rank, RankState, RankWeights, RankedRow, ScoreParts,
};
use oomtop_core::units::parse_bytes;
use oomtop_core::why::{explain, Cause};
use oomtop_core::{Group, GroupKind, ProcId, ProcState, Process, Quality, Snapshot};
use ratatui::layout::Rect;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};

// ---------------------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum View {
    Home,
    Processes,
    Models,
    Sandboxes,
    Reclaim,
    Timeline,
}

impl View {
    pub const ALL: [View; 6] = [
        View::Home,
        View::Processes,
        View::Models,
        View::Sandboxes,
        View::Reclaim,
        View::Timeline,
    ];
    pub fn title(self) -> &'static str {
        match self {
            View::Home => "Home",
            View::Processes => "Processes",
            View::Models => "Models",
            View::Sandboxes => "Sandboxes",
            View::Reclaim => "Reclaim",
            View::Timeline => "Timeline",
        }
    }
    pub fn from_action(a: &str) -> Option<View> {
        Some(match a {
            "view:home" | "view:groups" => View::Home,
            "view:processes" => View::Processes,
            "view:models" => View::Models,
            "view:sandboxes" => View::Sandboxes,
            "view:reclaim" => View::Reclaim,
            "view:timeline" => View::Timeline,
            _ => return None,
        })
    }
    fn from_query(v: oomtop_core::query::View) -> View {
        use oomtop_core::query::View as Q;
        match v {
            Q::Home => View::Home,
            Q::Processes => View::Processes,
            Q::Models => View::Models,
            Q::Sandboxes => View::Sandboxes,
            Q::Reclaim => View::Reclaim,
            Q::Timeline => View::Timeline,
        }
    }
}

/// Side effects the event loop performs.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    Quit,
    /// Execute a plan the user just confirmed (targets re-verified against the latest snapshot).
    Execute(ActionPlan),
    Pin {
        fingerprint: String,
        pinned: bool,
    },
    Mute {
        fingerprint: String,
        until_ms: Option<u64>,
    },
    Less {
        fingerprint: String,
    },
    Rename {
        fingerprint: String,
        alias: Option<String>,
    },
    /// The user selected/expanded an entity (learning signal; `searched` = after a query).
    Selected {
        fingerprint: String,
        searched: bool,
    },
    /// A query the user ran (learned completion).
    Query(String),
    /// Open the settings screen (the loop builds the model from the config layers).
    OpenSettings,
    Settings(SettingsCmd),
    /// Persist a saved query as an alias (`:save name`).
    SaveAlias {
        name: String,
        query: String,
    },
    /// Open a path (cwd, model file) with the OS opener.
    Open(String),
    /// Sample now instead of waiting for the refresh interval (`ctrl-l`).
    Refresh,
    /// Switch to a named layout (`:layout NAME`, or a saved view's `layout`); the loop reads the file.
    Layout(String),
}

/// What the input line is collecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Filter(String),
    Command(String),
    Palette(String),
    Rename(String),
}

impl Input {
    pub fn buffer(&self) -> &str {
        match self {
            Input::Filter(b) | Input::Command(b) | Input::Palette(b) | Input::Rename(b) => b,
        }
    }
    fn buffer_mut(&mut self) -> &mut String {
        match self {
            Input::Filter(b) | Input::Command(b) | Input::Palette(b) | Input::Rename(b) => b,
        }
    }
}

/// Identity of a list row; survives refreshes and reorders.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowKey {
    Group(String),
    Member(String, ProcId),
    Proc(ProcId),
    Model(String),
    Sandbox(String),
    /// "All reclaim candidates" row in the Reclaim view.
    ReclaimAll,
    /// A row of the scrubbed timeline frame (informational).
    Past(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub key: RowKey,
    pub depth: u8,
    pub moved: bool,
}

/// Overlays drawn over the body.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    Help,
    Details,
    /// "Why is this here?" for the selected row.
    WhyRank,
    /// `:why` — ranked causes of slowness / pressure.
    WhySlow,
    /// A computed answer (e.g. `:headroom 13G`), one line per entry.
    Answer(String, Vec<String>),
}

/// An inline confirmation.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub plan: ActionPlan,
    pub prompt: String,
    pub row: Option<RowKey>,
}

/// A signal that was sent and is being followed up (SIGKILL offer, measured gain).
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub target: ActionTarget,
    pub sent_ms: u64,
    pub available_before: Option<u64>,
    pub nagged: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Undo {
    Pin(String, bool),
    Mute(String, bool),
    Rename(String, Option<String>),
    Less(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeSort {
    Rank,
    Footprint,
    Cpu,
    Name,
    Gpu,
    Idle,
    Reclaim,
}

impl HomeSort {
    /// Parses a sort key (`:sort`, layout `sort`): rank | footprint | gpu | cpu | name | idle | reclaim.
    pub fn parse(s: &str) -> Option<HomeSort> {
        Some(match s.to_lowercase().as_str() {
            "rank" => HomeSort::Rank,
            "footprint" | "mem" | "memory" => HomeSort::Footprint,
            "cpu" => HomeSort::Cpu,
            "name" => HomeSort::Name,
            "gpu" => HomeSort::Gpu,
            "idle" => HomeSort::Idle,
            "reclaim" => HomeSort::Reclaim,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HomeSort::Rank => "rank",
            HomeSort::Footprint => "memory",
            HomeSort::Cpu => "cpu",
            HomeSort::Name => "name",
            HomeSort::Gpu => "gpu",
            HomeSort::Idle => "idle",
            HomeSort::Reclaim => "reclaim",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcSort {
    Footprint,
    Resident,
    Cpu,
    Pid,
    Name,
}

/// A "Your things" chip (UX §5.5): pinned + top-affinity running entities, aggregated by fingerprint.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    pub fingerprint: String,
    pub label: String,
    pub count: usize,
    pub footprint: Option<u64>,
    pub status: String,
    pub pinned: bool,
    pub running: bool,
    pub group_id: Option<String>,
}

/// A palette suggestion.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub label: String,
    pub detail: String,
    pub action: PaletteAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PaletteAction {
    Select(String),
    Filter(String),
    Command(String),
    Action(String),
    View(View),
}

/// Mouse input, already decoded from crossterm (`at_ms` from a monotonic clock, for double clicks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Down,
    Drag,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseInput {
    pub kind: MouseKind,
    pub col: u16,
    pub row: u16,
    pub at_ms: u64,
}

/// Commands accepted by `:` (and listed by the palette).
pub const COMMANDS: &[(&str, &str)] = &[
    ("reclaim", "show reclaim candidates"),
    ("headroom", "can I load SIZE now? e.g. :headroom 13G"),
    ("why", "why is it slow / under pressure"),
    ("pin", "pin an entity: :pin sd-server"),
    ("mute", "mute an entity: :mute chrome"),
    ("rename", "rename the selected entity"),
    ("mode", "pin a mode: :mode pressure | auto"),
    (
        "sort",
        "sort: rank | footprint | gpu | cpu | name | idle | reclaim | pid | resident",
    ),
    ("layout", "switch layout: :layout llm-dev | adaptive"),
    ("filter", "apply a filter: :filter kind:daemon"),
    ("clear", "clear the filter"),
    ("save", "save the current filter as an alias: :save gpu-work"),
    ("view", "apply a saved view: :view gpu-work"),
    ("stop", "stop the selected group (confirms)"),
    ("suspend", "suspend/resume the selected group (confirms)"),
    ("settings", "open settings"),
    ("help", "show keys"),
    ("quit", "quit"),
];

/// Short tag telling same-named groups apart: the session hash for agent sessions (`agent:3f2a…` → `3f2a`),
/// else the root pid.
pub fn disambiguator(g: &Group) -> String {
    let tail = g.id.rsplit(':').next().unwrap_or(&g.id);
    if tail.len() >= 6 && tail.chars().all(|c| c.is_ascii_hexdigit()) {
        tail.chars().take(4).collect()
    } else if let Some(r) = g.root {
        format!("pid {}", r.pid)
    } else {
        tail.chars().take(8).collect()
    }
}

/// Keymap key string → the short form shown in hints: `ctrl-k` → `^K`, `space` → `space`, `x` → `x`.
pub fn display_key(k: &str) -> String {
    match k.strip_prefix("ctrl-") {
        Some(rest) if rest.chars().count() == 1 => format!("^{}", rest.to_uppercase()),
        _ => k.to_string(),
    }
}

/// Actions bound (global context) to F1..F10, in order; `None` for an unbound key.
pub fn fkey_actions(km: &Keymap) -> Vec<Option<String>> {
    (1..=10)
        .map(|n| km.action("global", &format!("F{n}")).map(str::to_string))
        .collect()
}

/// Short htop-style label for an action on the function-key bar ("Help", "Setup", "SortBy"…).
pub fn fkey_label(action: &str) -> &'static str {
    match action {
        "help" => "Help",
        "settings" => "Setup",
        "palette" => "Search",
        "filter" => "Filter",
        "tree" => "Tree",
        "sort-by" => "SortBy",
        "sort" => "Sort",
        "why" => "Why",
        "view:reclaim" => "Reclaim",
        "stop" => "Stop",
        "suspend" => "Pause",
        "quit" => "Quit",
        "command" => "Cmd",
        "watch" => "Watch",
        "compare" => "Compare",
        "pin" => "Pin",
        "mute" => "Mute",
        "refresh" => "Refresh",
        "view:home" => "Home",
        "view:processes" => "Procs",
        "view:models" => "Models",
        "view:sandboxes" => "Sandbox",
        "view:timeline" => "Timeline",
        "headline-action" => "Action",
        _ => "",
    }
}

/// Short (≤ 4 chars) function-key label for narrow terminals; empty → the long label is truncated.
pub fn fkey_short(action: &str) -> &'static str {
    match action {
        "help" => "Help",
        "settings" => "Set",
        "palette" => "Find",
        "filter" => "Filt",
        "tree" => "Tree",
        "sort-by" | "sort" => "Sort",
        "why" => "Why",
        "view:reclaim" => "Recl",
        "stop" => "Stop",
        "quit" => "Quit",
        _ => "",
    }
}

/// Sort keys offered by the F6 "SortBy" picker for a view (first = the view's default).
pub fn sort_choices(view: View) -> &'static [(&'static str, &'static str)] {
    match view {
        View::Processes => &[
            ("footprint", "true memory"),
            ("cpu", "CPU %"),
            ("resident", "resident (RSS)"),
            ("pid", "process id"),
            ("name", "name"),
        ],
        _ => &[
            ("rank", "personalized rank (default)"),
            ("footprint", "true memory"),
            ("cpu", "CPU %"),
            ("gpu", "GPU memory"),
            ("idle", "idle time"),
            ("reclaim", "reclaimable memory"),
            ("name", "name"),
        ],
    }
}

/// Keys of the default preset, for hints when no keymap was handed over.
const DEFAULT_HINTS: &[(&str, &str)] = &[
    ("headline-action", "r"),
    ("stop", "x"),
    ("suspend", "z"),
    ("filter", "/"),
    ("command", ":"),
    ("palette", "^K"),
    ("pin", "p"),
    ("mute", "m"),
    ("watch", "w"),
    ("compare", "c"),
    ("why", "i"),
    ("settings", ","),
    ("help", "?"),
    ("quit", "q"),
    ("sort-by", "F6"),
    ("tree", "F5"),
];

/// Double-click window.
const DOUBLE_CLICK_MS: u64 = 400;
/// After this long without exiting, a SIGTERM target gets the SIGKILL offer hint.
const KILL_OFFER_AFTER_MS: u64 = 5_000;
/// Sparkline points kept per group.
pub const SPARK_POINTS: usize = 40;
/// Mute duration for `m` (30 days).
pub const MUTE_MS: u64 = 30 * 86_400_000;
/// Toast lifetime.
const TOAST_MS: u64 = 6_000;

// ---------------------------------------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------------------------------------

/// A model file on disk (SPEC §10), from the CLI's model index; shown under the Models view.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiskModel {
    /// Display name (`org/repo/file`, `model:tag`, file name).
    pub name: String,
    /// Path as found and with symlinks resolved; either may match a process's mapped weight file.
    pub path: String,
    pub real_path: String,
    pub size: u64,
    /// `gguf`, `safetensors`, `ollama`, …
    pub format: String,
    /// Last access or modification, ms since the epoch.
    pub last_used_ms: Option<u64>,
    /// Another file has the same size and partial hash.
    pub duplicate: bool,
}

impl DiskModel {
    /// Who has this file loaded in `s`: the model server's label, else the process name.
    pub fn loaded_by(&self, s: &Snapshot) -> Option<String> {
        let hit = |f: &str| f == self.path || f == self.real_path;
        for ms in &s.model_servers {
            if ms.models.iter().any(|m| m.file.as_deref().is_some_and(hit)) {
                return Some(ms.id.clone());
            }
        }
        s.processes
            .iter()
            .find(|p| p.model_files.iter().any(|f| hit(f)))
            .map(|p| p.name.clone())
    }
}

/// Parent → child order for the Processes tree (F5), keeping the current sort among siblings. A process whose
/// parent is not in `procs` (filtered out, exited, pid 1) is a root. Depth is capped so a deep chain stays
/// readable; cycles (pid reuse races) cannot loop because every process is placed once.
pub fn process_tree(procs: &[&Process]) -> Vec<(u8, ProcId)> {
    const MAX_DEPTH: u8 = 16;
    let present: HashSet<u32> = procs.iter().map(|p| p.id.pid).collect();
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (i, p) in procs.iter().enumerate() {
        match p.ppid {
            Some(pp) if pp != p.id.pid && present.contains(&pp) => children.entry(pp).or_default().push(i),
            _ => roots.push(i),
        }
    }
    let mut out = Vec::with_capacity(procs.len());
    let mut placed = vec![false; procs.len()];
    let mut stack: Vec<(usize, u8)> = roots.into_iter().rev().map(|i| (i, 0)).collect();
    while let Some((i, d)) = stack.pop() {
        if std::mem::replace(&mut placed[i], true) {
            continue;
        }
        out.push((d, procs[i].id));
        if let Some(kids) = children.get(&procs[i].id.pid) {
            for &k in kids.iter().rev() {
                stack.push((k, (d + 1).min(MAX_DEPTH)));
            }
        }
    }
    // Anything left (a parent cycle) is appended flat.
    for (i, p) in procs.iter().enumerate() {
        if !placed[i] {
            out.push((0, p.id));
        }
    }
    out
}

#[derive(Debug)]
pub struct App {
    pub snapshot: Snapshot,
    /// Model files on disk (Models view, "On disk" section); empty until the CLI's index arrives.
    pub disk_models: Vec<DiskModel>,
    /// A snapshot was shown before this one (insights compare against it). Only the flag is kept: the
    /// previous snapshot itself is dropped after `update` so the TUI never holds two full snapshots
    /// (~700 processes each; SPEC §14 RSS budget).
    had_prev: bool,
    pub headroom: Headroom,
    pub headline: Headline,
    pub modes: ModeMachine,
    pub mode: Mode,
    pub causes: Vec<Cause>,

    // config
    pub fmt: Fmt,
    pub weights: RankWeights,
    pub protect: ProtectContext,
    pub headroom_cfg: HeadroomConfig,
    pub motion: Motion,
    pub sparklines: Sparklines,
    pub compact_density: bool,
    pub header_rows: Vec<String>,
    pub aliases: BTreeMap<String, String>,
    pub views: Vec<SavedView>,
    pub ascii: bool,

    // personalization (loaded by the loop; never persisted from here)
    pub affinity: HashMap<String, f64>,
    pub frecency: HashMap<String, f64>,
    pub pinned: HashSet<String>,
    pub muted: HashSet<String>,
    pub less: HashMap<String, u32>,
    pub renames: HashMap<String, String>,
    pub entity_names: HashMap<String, String>,
    pub past_queries: Vec<String>,

    // ranking
    rank_state: RankState,
    /// Home ranking as served (after filter), stable order.
    pub ranked: Vec<RankedRow>,
    pub parts: HashMap<String, ScoreParts>,
    pub home_sort: HomeSort,
    pub proc_sort: ProcSort,
    group_idx: HashMap<String, usize>,
    proc_idx: HashMap<ProcId, usize>,
    first_seen: HashMap<String, u64>,
    pub sparks: HashMap<String, Vec<u64>>,
    pub swap_series: Vec<u64>,
    pub avail_series: Vec<u64>,
    pub cpu_series: Vec<f64>,
    /// Groups whose numbers changed noticeably this refresh (one-refresh bold marker).
    pub changed: HashSet<String>,
    pub chips: Vec<Chip>,
    dup_labels: HashSet<String>,
    /// Time of the first snapshot (novelty applies to entities seen later).
    boot_ms: Option<u64>,

    // list
    pub rows: Vec<Row>,
    pub selected: usize,
    selected_key: Option<RowKey>,
    pub expanded: HashSet<String>,
    pub list_offset: Cell<usize>,
    pub regions: RefCell<Regions>,
    pub area: Rect,

    // modes of interaction
    pub view: View,
    pub overlay: Option<Overlay>,
    pub help_scroll: usize,
    pub focus: Option<RowKey>,
    pub compare: Option<(RowKey, Option<RowKey>)>,
    pub input: Option<Input>,
    pub palette: Vec<Suggestion>,
    pub palette_sel: usize,
    pub filter: Option<String>,
    filter_expr: Option<Expr>,
    pub filter_error: Option<String>,
    filter_before_edit: Option<String>,
    pub status: Option<String>,
    pub toast: Option<(String, u64)>,
    pub confirm: Option<Confirm>,
    pub pending: Vec<Pending>,
    undo: Vec<Undo>,
    pub timeline: Timeline,
    pub settings: Option<SettingsState>,
    last_click: Option<(u64, u16, u16)>,
    /// Keystrokes since the last selection (UX §5.6 metric).
    pub keystrokes: u32,
    /// First key(s) of a chord being typed (e.g. `g` of vim's `g g`).
    pub pending_chord: Option<String>,
    typed_since_select: bool,
    /// The headline action key triggers this (e.g. 'r' → reclaim).
    pub help_lines_cache: Vec<(String, String)>,
    /// First key bound to each action in the active keymap (footer and details hints, UX §7).
    pub key_hints: HashMap<String, String>,
    /// A named layout from `layouts/<name>.toml` (UX §12.6); `None` = built-in adaptive layout.
    pub layout: Option<crate::columns::ActiveLayout>,
    /// Action bound to F1..F10 in the active keymap (the htop-style function-key bar, UX §7).
    pub fkeys: Vec<Option<String>>,
    /// Processes view as a parent → child tree (F5), like htop's tree mode.
    pub proc_tree: bool,
}

impl App {
    pub fn new(cfg: &Config) -> Self {
        let mut app = App {
            snapshot: Snapshot::default(),
            disk_models: Vec::new(),
            had_prev: false,
            headroom: Headroom::default(),
            headline: render_headline(&Default::default()),
            modes: ModeMachine::new(),
            mode: Mode::Calm,
            causes: Vec::new(),
            fmt: Fmt::default(),
            weights: RankWeights::default(),
            protect: ProtectContext::default(),
            headroom_cfg: HeadroomConfig::default(),
            motion: Motion::Marks,
            sparklines: Sparklines::Braille,
            compact_density: false,
            header_rows: Vec::new(),
            aliases: BTreeMap::new(),
            views: Vec::new(),
            ascii: false,
            affinity: HashMap::new(),
            frecency: HashMap::new(),
            pinned: HashSet::new(),
            muted: HashSet::new(),
            less: HashMap::new(),
            renames: HashMap::new(),
            entity_names: HashMap::new(),
            past_queries: Vec::new(),
            rank_state: RankState::new(),
            ranked: Vec::new(),
            parts: HashMap::new(),
            home_sort: HomeSort::Rank,
            proc_sort: ProcSort::Footprint,
            group_idx: HashMap::new(),
            proc_idx: HashMap::new(),
            first_seen: HashMap::new(),
            sparks: HashMap::new(),
            swap_series: Vec::new(),
            avail_series: Vec::new(),
            cpu_series: Vec::new(),
            changed: HashSet::new(),
            chips: Vec::new(),
            dup_labels: HashSet::new(),
            boot_ms: None,
            rows: Vec::new(),
            selected: 0,
            selected_key: None,
            expanded: HashSet::new(),
            list_offset: Cell::new(0),
            regions: RefCell::new(Regions::default()),
            area: Rect::new(0, 0, 120, 40),
            view: View::Home,
            overlay: None,
            help_scroll: 0,
            focus: None,
            compare: None,
            input: None,
            palette: Vec::new(),
            palette_sel: 0,
            filter: None,
            filter_expr: None,
            filter_error: None,
            filter_before_edit: None,
            status: None,
            toast: None,
            confirm: None,
            pending: Vec::new(),
            undo: Vec::new(),
            timeline: Timeline::default(),
            settings: None,
            last_click: None,
            keystrokes: 0,
            pending_chord: None,
            typed_since_select: false,
            help_lines_cache: Vec::new(),
            key_hints: HashMap::new(),
            layout: None,
            fkeys: fkey_actions(&oomtop_config::keymap::preset("default").unwrap_or_default()),
            proc_tree: false,
        };
        app.apply_config(cfg);
        app
    }

    /// Applies (re)loaded config values that the state machine uses.
    pub fn apply_config(&mut self, cfg: &Config) {
        self.fmt = Fmt::from_config(&cfg.format, self.snapshot.host.cores_logical);
        self.weights = cfg.personalization.weights.to_core();
        self.headroom_cfg = cfg.headroom_config();
        self.motion = cfg.appearance.motion;
        self.sparklines = cfg.appearance.sparklines;
        self.compact_density = cfg.appearance.density == oomtop_config::model::Density::Compact;
        self.header_rows = cfg.appearance.header.clone();
        self.aliases = cfg.aliases.clone();
        self.views = cfg.views.clone();
        self.ascii = cfg.appearance.glyphs == oomtop_config::model::Glyphs::Ascii;
        for n in &cfg.protected.names {
            if !self.protect.protected_names.contains(n) {
                self.protect.protected_names.push(n.clone());
            }
        }
    }

    /// Sets the protect context (from the CLI), keeping config-protected names.
    pub fn set_protect(&mut self, ctx: ProtectContext) {
        let extra = std::mem::take(&mut self.protect.protected_names);
        self.protect = ctx;
        for n in extra {
            if !self.protect.protected_names.contains(&n) {
                self.protect.protected_names.push(n);
            }
        }
    }

    /// Adopts the active keymap: the `?` help lines and the key shown next to each action in the footer and
    /// the details hints (so a remapped or htop keymap never advertises a key that does something else).
    pub fn set_keymap(&mut self, km: &Keymap) {
        self.help_lines_cache = crate::help_pairs(km);
        self.fkeys = fkey_actions(km);
        self.key_hints.clear();
        for action in oomtop_config::keymap::ACTIONS {
            let keys = km.keys_for(action);
            // Prefer a single key over a chord, and a short key over a long one.
            if let Some(k) = keys
                .iter()
                .filter(|k| !k.contains(' '))
                .min_by_key(|k| (k.chars().count() > 1, k.len()))
                .or(keys.first())
            {
                self.key_hints.insert(action.to_string(), display_key(k));
            }
        }
    }

    /// Display form of the key bound to `action` ("x", "^K", "F9"); `None` when the action is unbound.
    pub fn key_for(&self, action: &str) -> Option<String> {
        if self.key_hints.is_empty() {
            // No keymap handed over (tests, headless renders): the default preset's keys.
            return DEFAULT_HINTS
                .iter()
                .find(|(a, _)| *a == action)
                .map(|(_, k)| k.to_string());
        }
        self.key_hints.get(action).cloned()
    }

    /// Switches the layout (UX §12.6). A layout's `[columns.groups] sort` / `[columns.processes] sort` becomes
    /// the default sort; `None` returns to the built-in adaptive layout.
    pub fn set_layout(&mut self, layout: Option<crate::columns::ActiveLayout>) {
        if let Some(l) = &layout {
            if let Some(sort) = l
                .group_columns()
                .and_then(|c| c.sort.as_deref())
                .and_then(HomeSort::parse)
            {
                self.home_sort = sort;
            }
            if let Some(sort) = l.process_columns().and_then(|c| c.sort.as_deref()) {
                self.proc_sort = match sort {
                    "cpu" => ProcSort::Cpu,
                    "name" => ProcSort::Name,
                    "pid" => ProcSort::Pid,
                    "resident" => ProcSort::Resident,
                    _ => ProcSort::Footprint,
                };
            }
        }
        self.layout = layout;
        self.rank_state = RankState::new();
        self.rank_home();
        self.rebuild_rows();
    }

    /// Layout class for the last known terminal size.
    pub fn layout(&self) -> LayoutClass {
        LayoutClass::for_width(self.area.width)
    }

    pub fn now_ms(&self) -> u64 {
        self.snapshot.taken_at_ms
    }

    // -----------------------------------------------------------------------------------------------------
    // Lookups
    // -----------------------------------------------------------------------------------------------------

    pub fn group(&self, id: &str) -> Option<&Group> {
        self.group_idx.get(id).and_then(|i| self.snapshot.groups.get(*i))
    }

    pub fn process(&self, id: ProcId) -> Option<&Process> {
        self.proc_idx
            .get(&id)
            .and_then(|i| self.snapshot.processes.get(*i))
    }

    /// User rename, else the group label.
    pub fn base_label(&self, g: &Group) -> String {
        self.renames
            .get(&g.fingerprint)
            .cloned()
            .unwrap_or_else(|| g.label.clone())
    }

    /// Display label: [`Self::base_label`], disambiguated with a short id when several groups share it (four
    /// "Claude Code" sessions); oomtop's own group is marked.
    pub fn label(&self, g: &Group) -> String {
        let base = self.base_label(g);
        if g.is_self && !base.contains("this") {
            return format!("{base} (this)");
        }
        if self.dup_labels.contains(&base) {
            return format!("{base} · {}", disambiguator(g));
        }
        base
    }

    fn compute_dup_labels(&mut self) {
        let mut seen: HashMap<String, u32> = HashMap::new();
        for g in &self.snapshot.groups {
            *seen.entry(self.base_label(g)).or_insert(0) += 1;
        }
        self.dup_labels = seen.into_iter().filter(|(_, n)| *n > 1).map(|(k, _)| k).collect();
    }

    /// The group a row refers to (members/processes → their group; models/sandboxes → their host group).
    pub fn group_for_key(&self, key: &RowKey) -> Option<&Group> {
        match key {
            RowKey::Group(id) | RowKey::Member(id, _) | RowKey::Past(id) => self.group(id),
            RowKey::Proc(pid) => self.snapshot.group_of(*pid),
            RowKey::Model(id) => {
                let m = self.snapshot.model_servers.iter().find(|m| m.id == *id)?;
                m.group_id
                    .as_deref()
                    .and_then(|g| self.group(g))
                    .or_else(|| m.pids.first().and_then(|p| self.snapshot.group_of(*p)))
            }
            RowKey::Sandbox(id) => {
                let sb = self.snapshot.sandboxes.iter().find(|s| s.id == *id)?;
                sb.host_pids.first().and_then(|p| self.snapshot.group_of(*p))
            }
            RowKey::ReclaimAll => None,
        }
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    pub fn selected_group(&self) -> Option<&Group> {
        self.selected_row().and_then(|r| self.group_for_key(&r.key))
    }

    /// Fingerprints of the Home list as served (top 10), for impressions.
    pub fn served_fingerprints(&self) -> Vec<String> {
        self.ranked
            .iter()
            .take(10)
            .filter_map(|r| self.group(&r.id).map(|g| g.fingerprint.clone()))
            .collect()
    }

    /// Builds an impression for the learning log (UX §5.6) and resets the keystroke counter.
    pub fn take_impression(&mut self, selected_fp: &str) -> oomtop_state::Impression {
        let imp = oomtop_state::Impression {
            at_ms: self.now_ms(),
            served: self.served_fingerprints(),
            selected: Some(selected_fp.to_string()),
            keystrokes: self.keystrokes,
            typed: self.typed_since_select,
            reformulated: false,
            dismissed: false,
        };
        self.keystrokes = 0;
        self.typed_since_select = false;
        imp
    }

    // -----------------------------------------------------------------------------------------------------
    // Update pipeline
    // -----------------------------------------------------------------------------------------------------

    /// Ingests a new snapshot and recomputes headroom, mode, ranking, headline, timeline and rows.
    pub fn update(&mut self, snapshot: Snapshot, history: &History) {
        let now = snapshot.taken_at_ms;
        if self.boot_ms.is_none() {
            self.boot_ms = Some(now);
        }
        self.fmt.cores = snapshot.host.cores_logical.max(1);
        self.headroom = compute(&snapshot, &self.headroom_cfg);
        let sig = signals(&snapshot, &self.headroom);
        let before = self.mode;
        let had_snapshot = self.had_prev || !self.snapshot.groups.is_empty() || self.snapshot.taken_at_ms > 0;
        self.mode = self.modes.update(now, &sig);
        let mode_change = (had_snapshot && self.mode != before).then(|| self.mode.as_str());

        // One-refresh change markers: compared against the previous snapshot (old index still in place).
        self.changed.clear();
        if self.motion == Motion::Marks && had_snapshot && self.snapshot.taken_at_ms != now {
            for g in &snapshot.groups {
                let (Some(cur), Some(p)) = (
                    g.totals.footprint.value,
                    self.group(&g.id).and_then(|pg| pg.totals.footprint.value),
                ) else {
                    continue;
                };
                if cur.abs_diff(p) >= (256u64 << 20).max(p / 10) {
                    self.changed.insert(g.id.clone());
                }
            }
        }
        for g in &snapshot.groups {
            self.first_seen.entry(g.id.clone()).or_insert(now);
        }
        let live: HashSet<&str> = snapshot.groups.iter().map(|g| g.id.as_str()).collect();
        self.first_seen.retain(|k, _| live.contains(k.as_str()));

        let mut old = std::mem::replace(&mut self.snapshot, snapshot);
        // Insights compare groups and the forecast only: free the old processes (the bulk of a snapshot)
        // now, before this update allocates the new rows and indices — the TUI's heap peak, and so its RSS,
        // is set by how many full snapshots are alive at once (SPEC §14).
        old.processes = Vec::new();
        let prev_for_insights = if had_snapshot { Some(old) } else { None };
        self.group_idx = self
            .snapshot
            .groups
            .iter()
            .enumerate()
            .map(|(i, g)| (g.id.clone(), i))
            .collect();
        self.proc_idx = self
            .snapshot
            .processes
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id, i))
            .collect();

        self.compute_dup_labels();
        self.series_from(history);
        self.causes = explain(&self.snapshot, history);
        self.rank_home();
        self.build_chips();
        let mut hin = input_from(&self.snapshot, &self.headroom, self.mode);
        hin.your_things = self.chip_summaries();
        hin.units = Some(self.fmt.units.as_str().to_string());
        self.headline = render_headline(&hin);
        self.follow_pending();

        // insights → markers/toasts
        let mut markers: Vec<Marker> =
            self.timeline
                .insights(prev_for_insights.as_ref(), &self.snapshot, mode_change);
        for m in markers.drain(..) {
            let yours = m
                .fingerprint
                .as_ref()
                .map(|fp| self.pinned.contains(fp) || self.chips.iter().any(|c| &c.fingerprint == fp))
                .unwrap_or(false);
            let text = m.text.clone();
            if self.timeline.mark(m, yours) {
                self.toast = Some((text, now + TOAST_MS));
            }
        }
        if self
            .toast
            .as_ref()
            .map(|(_, until)| now >= *until)
            .unwrap_or(false)
        {
            self.toast = None;
        }
        self.push_frame();
        self.had_prev = prev_for_insights.is_some();
        drop(prev_for_insights);
        self.rebuild_rows();
    }

    fn series_from(&mut self, history: &History) {
        let pts: Vec<_> = history.iter().collect();
        let start = pts.len().saturating_sub(SPARK_POINTS);
        let tail = &pts[start..];
        self.swap_series = tail.iter().filter_map(|p| p.swap_used).collect();
        self.avail_series = tail.iter().filter_map(|p| p.available).collect();
        self.cpu_series = tail.iter().filter_map(|p| p.cpu_total_pct).collect();
        let mut sparks: HashMap<String, Vec<u64>> = HashMap::with_capacity(self.snapshot.groups.len());
        for g in &self.snapshot.groups {
            // History stores group keys (see `history::group_key`); points where the group is absent are 0.
            let key = oomtop_core::history::group_key(&g.id);
            let mut seen = false;
            let mut series: Vec<u64> = Vec::with_capacity(tail.len());
            for p in tail {
                match p.group_keys.binary_search_by_key(&key, |(k, _)| *k) {
                    Ok(i) => {
                        seen = true;
                        series.push(p.group_keys[i].1);
                    }
                    // Before the group first appeared the series is padded with zeros, like a gap.
                    Err(_) => series.push(0),
                }
            }
            if !seen {
                series = g.totals.footprint.value.into_iter().collect();
            }
            sparks.insert(g.id.clone(), series);
        }
        self.sparks = sparks;
    }

    fn entity_view(&self, g: &Group) -> EntityView {
        let mut v = EntityView::from_group(g, &self.snapshot);
        if let Some(a) = self.renames.get(&g.fingerprint) {
            v.aliases.push(a.clone());
        }
        if self.pinned.contains(&g.fingerprint) {
            v.state.push("pinned".into());
        }
        if self.muted.contains(&g.fingerprint) {
            v.state.push("muted".into());
        }
        if is_reclaim_candidate(g) {
            v.state.push("reclaimable".into());
        }
        v
    }

    fn process_view(&self, p: &Process) -> EntityView {
        let g = self.snapshot.group_of(p.id);
        let mut state = Vec::new();
        match p.state {
            ProcState::Running => state.push("running".to_string()),
            ProcState::Stopped => state.push("stopped".to_string()),
            ProcState::Zombie => state.push("zombie".to_string()),
            _ => {}
        }
        if p.idle_for_s.value.unwrap_or(0) >= 1800 {
            state.push("idle".into());
        }
        EntityView {
            id: format!("{}", p.id.pid),
            kind: g.map(|g| g.kind).unwrap_or_default(),
            name: p.name.clone(),
            aliases: g.map(|g| vec![g.label.clone()]).unwrap_or_default(),
            owner: g.map(|g| g.label.clone()),
            mem: p.mem.footprint_or_pss.value,
            gpu: p.mem.gpu.value,
            cpu: p.cpu_pct.value,
            idle_s: p.idle_for_s.value,
            state,
            sandbox: None,
        }
    }

    fn matches_filter_group(&self, g: &Group) -> bool {
        match &self.filter_expr {
            Some(e) => eval(e, &self.entity_view(g)),
            None => true,
        }
    }

    fn rank_home(&mut self) {
        let s = &self.snapshot;
        let mut cands = group_candidates(s, self.mode, &self.affinity, &self.muted);
        let cold = self.affinity.is_empty();
        let now = s.taken_at_ms;
        for c in cands.iter_mut() {
            let Some(g) = self.group_idx.get(&c.id).and_then(|i| s.groups.get(*i)) else {
                continue;
            };
            if let Some(n) = self.less.get(&g.fingerprint) {
                c.noise += 0.25 * *n as f64;
            }
            if self.pinned.contains(&g.fingerprint) {
                c.affinity = c.affinity.max(0.8);
            }
            // Cold start priors (UX §5.4): model servers and agent sessions start with a boost.
            if cold && matches!(g.kind, GroupKind::ModelServer | GroupKind::AgentSession) {
                c.affinity = c.affinity.max(0.15);
            }
            // Novelty is for entities that matter: a group holding < 1 % of RAM and < 1 % of the CPUs gets none,
            // so a 2 MB helper that just started never outranks a 450 MB app (UX §5.3 "new large process").
            let total = s.memory.total.value.unwrap_or(s.host.mem_total).max(1) as f64;
            let share = g.totals.footprint.value.unwrap_or(0) as f64 / total;
            let cpu_share =
                g.totals.cpu_pct.value.unwrap_or(0.0) / (100.0 * s.host.cores_logical.max(1) as f64);
            if share < 0.01 && cpu_share < 0.01 {
                c.novelty = 0.0;
            }
            // Exploration: a small, decaying novelty boost for entities first seen in the last minute.
            if let Some(t0) = self.first_seen.get(&c.id) {
                let age = now.saturating_sub(*t0) as f64 / 1000.0;
                // Only entities that appeared after oomtop started count as new.
                if self.boot_ms.map(|b| *t0 > b).unwrap_or(false) && age < 60.0 {
                    c.novelty = 0.1 * (1.0 - age / 60.0);
                }
            }
        }
        let ranked = rank(&cands, &self.weights, 0.15);
        self.parts = ranked.iter().map(|(id, p)| (id.clone(), p.clone())).collect();
        let mut scored: Vec<(String, f64)> = ranked
            .into_iter()
            .filter(|(id, _)| {
                self.group(id)
                    .map(|g| self.matches_filter_group(g))
                    .unwrap_or(false)
            })
            .map(|(id, p)| (id, p.total))
            .collect();
        match self.home_sort {
            HomeSort::Rank => {
                let sel_group = self.selected_key.as_ref().and_then(|k| match k {
                    RowKey::Group(id) | RowKey::Member(id, _) => Some(id.clone()),
                    _ => None,
                });
                self.ranked = self.rank_state.apply(&scored, sel_group.as_deref(), 0.05);
                if self.motion == Motion::None {
                    for r in self.ranked.iter_mut() {
                        r.moved = false;
                    }
                }
            }
            other => {
                let key = |id: &str| -> (i64, String) {
                    let g = self.group(id);
                    match other {
                        HomeSort::Footprint => (
                            -(g.and_then(|g| g.totals.footprint.value).unwrap_or(0) as i64),
                            id.to_string(),
                        ),
                        HomeSort::Cpu => (
                            -(g.and_then(|g| g.totals.cpu_pct.value).unwrap_or(0.0) * 100.0) as i64,
                            id.to_string(),
                        ),
                        HomeSort::Gpu => (
                            -(g.and_then(|g| g.totals.gpu.value).unwrap_or(0) as i64),
                            id.to_string(),
                        ),
                        HomeSort::Idle => (
                            -(g.and_then(|g| g.idle_for_s).unwrap_or(0) as i64),
                            id.to_string(),
                        ),
                        HomeSort::Reclaim => (
                            -(g.and_then(|g| g.reclaim_gain.value).unwrap_or(0) as i64),
                            id.to_string(),
                        ),
                        _ => (0, g.map(|g| g.label.to_lowercase()).unwrap_or_default()),
                    }
                };
                scored.sort_by_key(|(id, _)| key(id));
                self.ranked = scored
                    .into_iter()
                    .map(|(id, score)| RankedRow {
                        id,
                        score,
                        moved: false,
                    })
                    .collect();
            }
        }
    }

    fn model_status(&self, g: &Group) -> Option<String> {
        let m = self
            .snapshot
            .model_servers
            .iter()
            .find(|m| m.group_id.as_deref() == Some(g.id.as_str()))?;
        if m.busy.value == Some(true) {
            let mut s = m
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
            if let Some(sps) = m.s_per_step.value {
                s.push_str(&format!(" · {sps:.1} s/step"));
            } else if let Some(t) = m.tok_s.value {
                s.push_str(&format!(" · {t:.0} tok/s"));
            }
            Some(s)
        } else {
            Some("idle".into())
        }
    }

    fn build_chips(&mut self) {
        let mut by_fp: Vec<Chip> = Vec::new();
        for g in &self.snapshot.groups {
            if g.fingerprint.is_empty() || g.is_self {
                continue;
            }
            if let Some(c) = by_fp.iter_mut().find(|c| c.fingerprint == g.fingerprint) {
                c.count += 1;
                c.footprint = Some(c.footprint.unwrap_or(0) + g.totals.footprint.value.unwrap_or(0));
                continue;
            }
            let status = self.model_status(g).unwrap_or_else(|| {
                if g.orphan {
                    "orphan".into()
                } else if g.idle {
                    format!(
                        "idle {}",
                        g.idle_for_s.map(|s| self.fmt.duration(s)).unwrap_or_default()
                    )
                } else {
                    String::new()
                }
            });
            by_fp.push(Chip {
                fingerprint: g.fingerprint.clone(),
                label: self.base_label(g),
                count: 1,
                footprint: g.totals.footprint.value,
                status,
                pinned: self.pinned.contains(&g.fingerprint),
                running: true,
                group_id: Some(g.id.clone()),
            });
        }
        let mut chips: Vec<Chip> = Vec::new();
        let mut pinned: Vec<&String> = self.pinned.iter().collect();
        pinned.sort();
        for fp in pinned {
            if let Some(c) = by_fp.iter().find(|c| &c.fingerprint == fp) {
                chips.push(c.clone());
            } else {
                chips.push(Chip {
                    fingerprint: fp.clone(),
                    label: self
                        .renames
                        .get(fp)
                        .or_else(|| self.entity_names.get(fp))
                        .cloned()
                        .unwrap_or_else(|| fp.chars().take(12).collect()),
                    count: 0,
                    footprint: None,
                    status: "not running".into(),
                    pinned: true,
                    running: false,
                    group_id: None,
                });
            }
        }
        let mut aff: Vec<(&Chip, f64)> = by_fp
            .iter()
            .filter(|c| !c.pinned && !self.muted.contains(&c.fingerprint))
            .filter_map(|c| {
                let a = self.affinity.get(&c.fingerprint).copied().unwrap_or(0.0);
                (a > 0.05).then_some((c, a))
            })
            .collect();
        aff.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.label.cmp(&b.0.label))
        });
        for (c, _) in aff.into_iter().take(3) {
            if chips.len() >= 4 {
                break;
            }
            chips.push(c.clone());
        }
        // Cold-start priors: model servers and agent sessions on a machine where they're detected.
        if chips.len() < 4 {
            let mut priors: Vec<&Chip> = by_fp
                .iter()
                .filter(|c| !chips.iter().any(|x| x.fingerprint == c.fingerprint))
                .filter(|c| !self.muted.contains(&c.fingerprint))
                .filter(|c| {
                    c.group_id
                        .as_deref()
                        .and_then(|id| self.group(id))
                        .map(|g| matches!(g.kind, GroupKind::ModelServer | GroupKind::AgentSession))
                        .unwrap_or(false)
                })
                .collect();
            priors.sort_by_key(|c| std::cmp::Reverse(c.footprint.unwrap_or(0)));
            for c in priors {
                if chips.len() >= 4 {
                    break;
                }
                chips.push(c.clone());
            }
        }
        chips.truncate(4);
        self.chips = chips;
    }

    fn chip_summaries(&self) -> Vec<String> {
        self.chips
            .iter()
            .filter(|c| c.running)
            .map(|c| {
                if c.count > 1 {
                    format!("{} {} sessions", c.count, c.label)
                } else if c.status.is_empty() {
                    c.label.clone()
                } else {
                    let short = c.status.split(" · ").next().unwrap_or("");
                    format!("{} {}", c.label.split(" · ").next().unwrap_or(&c.label), short)
                }
            })
            .collect()
    }

    fn push_frame(&mut self) {
        let picked: Vec<(String, String, GroupKind, Option<u64>, bool)> = self
            .ranked
            .iter()
            .take(crate::timeline::ROWS_PER_FRAME)
            .filter_map(|r| {
                let g = self.group(&r.id)?;
                Some((
                    g.id.clone(),
                    self.label(g),
                    g.kind,
                    g.totals.footprint.value,
                    is_reclaim_candidate(g),
                ))
            })
            .collect();
        let rows: Vec<FrameRow> = picked
            .into_iter()
            .map(|(id, label, kind, footprint, reclaimable)| FrameRow {
                id: self.timeline.intern(&id),
                label: self.timeline.intern(&label),
                kind,
                footprint,
                reclaimable,
            })
            .collect();
        let m = &self.snapshot.memory;
        self.timeline.push(Frame {
            t_ms: self.snapshot.taken_at_ms,
            available: m.available.value,
            total: m.total.value,
            swap_used: m.swap_used.value,
            swap_total: m.swap_total.value,
            cpu_pct: self.snapshot.cpu.total_pct.value,
            pressure: m.pressure.value,
            mode: self.mode.as_str(),
            rows,
        });
    }

    fn follow_pending(&mut self) {
        let now = self.now_ms();
        let avail = self.headroom.available_now.value;
        let mut msgs = Vec::new();
        let proc_idx = &self.proc_idx;
        let fmt = self.fmt.clone();
        self.pending.retain_mut(|p| {
            if !proc_idx.contains_key(&p.target.root) {
                let freed = match (p.available_before, avail) {
                    (Some(b), Some(a)) if a > b => Some(a - b),
                    _ => None,
                };
                let est = p.target.expected_gain.map(|g| format!("est. {}", fmt.bytes(g)));
                msgs.push(match (freed, est) {
                    (Some(f), Some(e)) => format!("{} exited — freed {} ({e})", p.target.label, fmt.bytes(f)),
                    (None, Some(e)) => {
                        format!("{} exited ({e}; freed memory not yet visible)", p.target.label)
                    }
                    _ => format!("{} exited", p.target.label),
                });
                false
            } else {
                if !p.nagged
                    && now.saturating_sub(p.sent_ms) >= KILL_OFFER_AFTER_MS
                    && p.target.kind == ActionKind::Terminate
                {
                    p.nagged = true;
                    msgs.push(format!(
                        "{} is still running {}s after SIGTERM — press x again to force kill (SIGKILL)",
                        p.target.label,
                        now.saturating_sub(p.sent_ms) / 1000
                    ));
                }
                true
            }
        });
        if let Some(m) = msgs.pop() {
            self.status = Some(m);
        }
    }

    /// Rebuilds the flattened list for the current view, restoring the selection by identity.
    pub fn rebuild_rows(&mut self) {
        let key = self
            .selected_key
            .clone()
            .or_else(|| self.rows.get(self.selected).map(|r| r.key.clone()));
        let mut rows: Vec<Row> = Vec::new();
        match self.view {
            View::Home => {
                for r in &self.ranked {
                    let Some(g) = self.group(&r.id) else { continue };
                    rows.push(Row {
                        key: RowKey::Group(r.id.clone()),
                        depth: 0,
                        moved: r.moved,
                    });
                    if self.expanded.contains(&r.id) {
                        let mut members: Vec<&Process> =
                            g.members.iter().filter_map(|m| self.process(m.id)).collect();
                        members.sort_by_key(|p| std::cmp::Reverse(p.mem.footprint_or_pss.value.unwrap_or(0)));
                        for p in members {
                            rows.push(Row {
                                key: RowKey::Member(r.id.clone(), p.id),
                                depth: 1,
                                moved: false,
                            });
                        }
                    }
                }
            }
            View::Processes => {
                let mut procs: Vec<&Process> = self
                    .snapshot
                    .processes
                    .iter()
                    .filter(|p| match &self.filter_expr {
                        Some(e) => eval(e, &self.process_view(p)),
                        None => true,
                    })
                    .collect();
                match self.proc_sort {
                    ProcSort::Footprint => procs.sort_by_key(|p| {
                        (
                            std::cmp::Reverse(p.mem.footprint_or_pss.value.unwrap_or(0)),
                            p.id.pid,
                        )
                    }),
                    ProcSort::Resident => procs
                        .sort_by_key(|p| (std::cmp::Reverse(p.mem.resident.value.unwrap_or(0)), p.id.pid)),
                    ProcSort::Cpu => procs.sort_by(|a, b| {
                        b.cpu_pct
                            .value
                            .unwrap_or(0.0)
                            .partial_cmp(&a.cpu_pct.value.unwrap_or(0.0))
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then(a.id.pid.cmp(&b.id.pid))
                    }),
                    ProcSort::Pid => procs.sort_by_key(|p| p.id.pid),
                    ProcSort::Name => procs.sort_by(|a, b| {
                        a.name
                            .to_lowercase()
                            .cmp(&b.name.to_lowercase())
                            .then(a.id.pid.cmp(&b.id.pid))
                    }),
                }
                if self.proc_tree {
                    rows.extend(process_tree(&procs).into_iter().map(|(depth, id)| Row {
                        key: RowKey::Proc(id),
                        depth,
                        moved: false,
                    }));
                } else {
                    rows.extend(procs.into_iter().map(|p| Row {
                        key: RowKey::Proc(p.id),
                        depth: 0,
                        moved: false,
                    }));
                }
            }
            View::Models => {
                rows.extend(self.snapshot.model_servers.iter().map(|m| Row {
                    key: RowKey::Model(m.id.clone()),
                    depth: 0,
                    moved: false,
                }));
            }
            View::Sandboxes => {
                rows.extend(self.snapshot.sandboxes.iter().map(|s| Row {
                    key: RowKey::Sandbox(s.id.clone()),
                    depth: 0,
                    moved: false,
                }));
            }
            View::Reclaim => {
                let cands: Vec<String> = reclaim_candidates(&self.snapshot)
                    .into_iter()
                    .filter(|c| self.protect.caller_group.as_deref() != Some(c.group_id.as_str()))
                    .filter(|c| {
                        self.group(&c.group_id)
                            .map(|g| self.matches_filter_group(g))
                            .unwrap_or(false)
                    })
                    .map(|c| c.group_id)
                    .collect();
                if cands.len() >= 2 {
                    rows.push(Row {
                        key: RowKey::ReclaimAll,
                        depth: 0,
                        moved: false,
                    });
                }
                rows.extend(cands.into_iter().map(|id| Row {
                    key: RowKey::Group(id),
                    depth: 0,
                    moved: false,
                }));
            }
            View::Timeline => {
                if let Some(f) = self.timeline.current() {
                    rows.extend(f.rows.iter().map(|r| Row {
                        key: RowKey::Past(r.id.to_string()),
                        depth: 0,
                        moved: false,
                    }));
                }
            }
        }
        self.rows = rows;
        let restored = key.as_ref().and_then(|k| {
            self.rows.iter().position(|r| &r.key == k).or_else(|| match k {
                // a member that vanished → its group
                RowKey::Member(g, _) => self.rows.iter().position(|r| r.key == RowKey::Group(g.clone())),
                _ => None,
            })
        });
        if let Some(i) = restored {
            self.selected = i;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.selected_key = self.rows.get(self.selected).map(|r| r.key.clone());
    }

    fn select(&mut self, i: usize) {
        if self.rows.is_empty() {
            self.selected = 0;
            self.selected_key = None;
            return;
        }
        self.selected = i.min(self.rows.len() - 1);
        self.selected_key = self.rows.get(self.selected).map(|r| r.key.clone());
    }

    fn select_key(&mut self, key: RowKey) {
        if let Some(i) = self.rows.iter().position(|r| r.key == key) {
            self.select(i);
        }
    }

    // -----------------------------------------------------------------------------------------------------
    // Explanations
    // -----------------------------------------------------------------------------------------------------

    /// Short "because …" hint for the wide layout (UX §2).
    pub fn because(&self, g: &Group) -> String {
        let mut out: Vec<String> = Vec::new();
        let parts = self.parts.get(&g.id);
        if self.pinned.contains(&g.fingerprint) {
            out.push("pinned".into());
        }
        if g.orphan {
            let owner = g
                .owner_group
                .as_deref()
                .and_then(|o| self.group(o))
                .map(|o| self.label(o));
            out.push(match owner {
                Some(o) => format!("orphan of {o}"),
                None => "orphan".into(),
            });
        }
        if parts.map(|p| p.actionability > 0.02).unwrap_or(false) {
            out.push(if g.idle {
                "idle cost".into()
            } else {
                "reclaimable".into()
            });
            if self.mode == Mode::Pressure {
                out.push("pressure".into());
            }
        }
        if g.kind == GroupKind::Sandbox {
            if let Some(o) = g.owner_group.as_deref().and_then(|o| self.group(o)) {
                out.push(format!("sandbox · started by {}", self.label(o)));
            }
        }
        if parts.map(|p| p.affinity > 0.1).unwrap_or(false) && !self.pinned.contains(&g.fingerprint) {
            out.push("you open it often".into());
        }
        if self.filter_expr.is_some() {
            out.push("matches filter".into());
        }
        if self.muted.contains(&g.fingerprint) {
            out.push("muted".into());
        }
        if out.is_empty() {
            let total = self
                .snapshot
                .memory
                .total
                .value
                .unwrap_or(self.snapshot.host.mem_total)
                .max(1);
            if let Some(f) = g.totals.footprint.value {
                let pct = f as f64 * 100.0 / total as f64;
                if pct >= 5.0 {
                    out.push(format!("holds {pct:.0}% of RAM"));
                }
            }
        }
        out.truncate(2);
        out.join(" · ")
    }

    /// "Why ranked here" lines (UX §9): position, top contributions in plain words, and the raw parts.
    pub fn why_lines(&self) -> Vec<String> {
        let Some(g) = self.selected_group() else {
            return vec!["Nothing selected.".into()];
        };
        let pos = self.ranked.iter().position(|r| r.id == g.id).map(|p| p + 1);
        let Some(parts) = self.parts.get(&g.id) else {
            return vec![format!("{} is not ranked in this view.", self.label(g))];
        };
        let mut words: Vec<String> = Vec::new();
        if let Some(f) = g.totals.footprint.value {
            let total = self
                .snapshot
                .memory
                .total
                .value
                .unwrap_or(self.snapshot.host.mem_total)
                .max(1);
            words.push(format!(
                "holds {} ({:.0}% of RAM) (salience {:+.2})",
                self.fmt.bytes(f),
                f as f64 * 100.0 / total as f64,
                parts.salience
            ));
        }
        if parts.affinity.abs() > 1e-6 {
            let why = if self.pinned.contains(&g.fingerprint) {
                "you pinned it".to_string()
            } else if self.affinity.contains_key(&g.fingerprint) {
                "you open it often".to_string()
            } else {
                "model servers and agents start with a boost on this machine".to_string()
            };
            words.push(format!("{why} (affinity {:+.2})", parts.affinity));
        }
        if parts.actionability.abs() > 1e-6 {
            words.push(format!(
                "could free {} (actionability {:+.2})",
                self.fmt.opt_bytes(g.reclaim_gain.value),
                parts.actionability
            ));
        }
        if parts.noise.abs() > 1e-6 {
            let why = if self.muted.contains(&g.fingerprint) {
                "muted"
            } else if self.less.contains_key(&g.fingerprint) {
                "you asked for less like this"
            } else {
                "system noise prior"
            };
            words.push(format!("{why} (noise {:+.2})", parts.noise));
        }
        if parts.novelty.abs() > 1e-6 {
            words.push(format!("new entity (novelty {:+.2})", parts.novelty));
        }
        if g.totals.process_count > 1 {
            words.push(format!("{} processes grouped", g.totals.process_count));
        }
        let head = match pos {
            Some(p) => format!("{}: {}", ordinal(p), self.label(g)),
            None => self.label(g),
        };
        let mut out = vec![head, explain_rank(pos.unwrap_or(0).max(1), parts)];
        out.extend(words.into_iter().map(|w| format!("  {w}")));
        out.push(format!(
            "  score {:.2} in {} mode",
            parts.total,
            self.mode.as_str()
        ));
        if let Some(m) = &g.matched_by {
            out.push(format!("  grouped by {m} ({:?} confidence)", g.confidence).to_lowercase());
        }
        out
    }

    // -----------------------------------------------------------------------------------------------------
    // Keys
    // -----------------------------------------------------------------------------------------------------

    /// Keymap context for the current state ("details" when a details overlay or focus mode is open).
    pub fn key_context(&self) -> &'static str {
        if matches!(self.overlay, Some(Overlay::Details)) || self.focus.is_some() {
            "details"
        } else {
            "global"
        }
    }

    /// One key through raw handling, then the keymap, then the headline action key.
    pub fn on_key(&mut self, key: &str, km: &Keymap) -> Option<Effect> {
        self.keystrokes = self.keystrokes.saturating_add(1);
        let (consumed, eff) = self.on_raw_key(key);
        if consumed {
            return eff;
        }
        let ctx = self.key_context();
        let full = match self.pending_chord.take() {
            Some(prefix) => format!("{prefix} {key}"),
            None => key.to_string(),
        };
        if let Some(a) = km.action(ctx, &full).map(str::to_string) {
            return self.on_action(&a);
        }
        if km.is_chord_prefix(ctx, &full) {
            self.status = Some(format!("{full} …"));
            self.pending_chord = Some(full);
            return None;
        }
        if full != key {
            // An unfinished chord followed by an unbound key: drop the chord (vim behaviour).
            self.status = None;
            return None;
        }
        None
    }

    /// The headline's one best action (UX §9): `r` → reclaim all candidates (one keypress + confirm).
    pub fn headline_action(&mut self) -> Option<Effect> {
        self.set_view(View::Reclaim);
        if let Some(i) = self.rows.iter().position(|r| r.key == RowKey::ReclaimAll) {
            self.select(i);
        }
        self.begin_stop();
        None
    }

    /// Handles a raw key while a modal state (settings, confirmation, input line, overlay) is active.
    /// Returns `(consumed, effect)`.
    pub fn on_raw_key(&mut self, key: &str) -> (bool, Option<Effect>) {
        if let Some(st) = self.settings.as_mut() {
            let cmd = st.on_key(key);
            if matches!(cmd, Some(SettingsCmd::Revert) | Some(SettingsCmd::Apply)) {
                self.settings = None;
            }
            return (true, cmd.map(Effect::Settings));
        }
        if self.confirm.is_some() {
            return (true, self.on_confirm_key(key));
        }
        if self.input.is_some() {
            return (true, self.on_input_key(key));
        }
        match &self.overlay {
            Some(Overlay::Help) => {
                match key {
                    "down" | "j" => self.help_scroll += 1,
                    "up" | "k" => self.help_scroll = self.help_scroll.saturating_sub(1),
                    "pagedown" => self.help_scroll += 10,
                    "pageup" => self.help_scroll = self.help_scroll.saturating_sub(10),
                    _ => {
                        self.overlay = None;
                        self.help_scroll = 0;
                    }
                }
                return (true, None);
            }
            Some(Overlay::WhyRank) | Some(Overlay::WhySlow) | Some(Overlay::Answer(..)) => {
                if matches!(key, "esc" | "enter" | "q" | "i") {
                    self.overlay = None;
                    return (true, None);
                }
            }
            Some(Overlay::Details) => {
                if matches!(key, "esc" | "q") {
                    self.overlay = None;
                    return (true, None);
                }
            }
            None => {}
        }
        if key == "esc" {
            if self.compare.is_some() {
                self.compare = None;
            } else if self.focus.is_some() {
                self.focus = None;
            } else if self.filter.is_some() {
                self.set_filter(None);
                self.status = Some("filter cleared".into());
            } else if self.view == View::Timeline && self.timeline.scrub.is_some() {
                self.timeline.live();
                self.rebuild_rows();
            } else {
                self.status = None;
            }
            return (true, None);
        }
        (false, None)
    }

    fn on_confirm_key(&mut self, key: &str) -> Option<Effect> {
        let c = self.confirm.take()?;
        if !matches!(key, "y" | "Y") {
            self.status = Some("cancelled — nothing was sent".into());
            return None;
        }
        // Re-verify identity against the latest snapshot: the exact (pid, start_time) must still be there.
        let gone: Vec<&str> = c
            .plan
            .targets
            .iter()
            .filter(|t| self.process(t.root).is_none())
            .map(|t| t.label.as_str())
            .collect();
        if !gone.is_empty() {
            self.status = Some(format!(
                "{} changed (exited or pid reused) — nothing was sent",
                gone.join(", ")
            ));
            return None;
        }
        let mut plan = c.plan;
        plan.confirmed = true;
        plan.confirmed_kill = plan.targets.iter().any(|t| t.kind == ActionKind::Kill);
        Some(Effect::Execute(plan))
    }

    fn on_input_key(&mut self, key: &str) -> Option<Effect> {
        let input = self.input.as_mut()?;
        match key {
            "esc" => {
                let was = self.input.take();
                if let Some(Input::Filter(_)) = was {
                    let before = self.filter_before_edit.take();
                    self.set_filter(before);
                }
                self.palette.clear();
                None
            }
            "enter" => {
                let done = self.input.take()?;
                self.typed_since_select = true;
                self.submit_input(done)
            }
            "backspace" => {
                input.buffer_mut().pop();
                self.after_input_edit();
                None
            }
            "ctrl-u" => {
                input.buffer_mut().clear();
                self.after_input_edit();
                None
            }
            "up" | "ctrl-p" => {
                self.palette_sel = self.palette_sel.saturating_sub(1);
                None
            }
            "down" | "ctrl-n" | "tab" => {
                if !self.palette.is_empty() {
                    self.palette_sel = (self.palette_sel + 1).min(self.palette.len() - 1);
                }
                None
            }
            "space" => {
                input.buffer_mut().push(' ');
                self.after_input_edit();
                None
            }
            k if k.chars().count() == 1 => {
                input.buffer_mut().push_str(k);
                self.after_input_edit();
                None
            }
            _ => None,
        }
    }

    fn after_input_edit(&mut self) {
        match self.input.clone() {
            Some(Input::Filter(b)) => self.live_filter(&b),
            Some(Input::Palette(b)) | Some(Input::Command(b)) => {
                let text = if matches!(self.input, Some(Input::Command(_))) {
                    format!(":{b}")
                } else {
                    b
                };
                self.palette = self.suggestions(&text);
                self.palette_sel = 0;
            }
            _ => {}
        }
    }

    /// Expands filter aliases (recursively, with cycle detection — `oomtop_config::views::expand`), so
    /// `llm gpu>1G` and `-llm` work; a cycle or a command alias used inside a filter is an error.
    fn expand_alias(&self, text: &str) -> Result<String, String> {
        match oomtop_config::views::expand(text, &self.aliases)? {
            oomtop_config::views::Expanded::Filter(f) => Ok(f),
            oomtop_config::views::Expanded::Command(c) => Err(format!("{c} is a command — run it with :")),
        }
    }

    /// Applies a filter as the user types; an invalid prefix keeps the previous valid filter.
    fn live_filter(&mut self, text: &str) {
        let t = match self.expand_alias(text) {
            Ok(t) => t,
            Err(e) => {
                self.filter_error = Some(e);
                return;
            }
        };
        if t.trim().is_empty() {
            self.filter_error = None;
            self.filter_expr = None;
            self.rank_state = RankState::new();
            self.rank_home();
            self.rebuild_rows();
            return;
        }
        match parse_filter(&t) {
            Ok(e) => {
                self.filter_error = None;
                self.filter_expr = Some(e);
                self.rank_state = RankState::new();
                self.rank_home();
                self.rebuild_rows();
            }
            Err(e) => self.filter_error = Some(e.to_string()),
        }
    }

    /// Sets (or clears) the committed filter.
    pub fn set_filter(&mut self, f: Option<String>) {
        let f = f.filter(|s| !s.trim().is_empty());
        self.filter_expr = f
            .as_deref()
            .and_then(|t| self.expand_alias(t).ok())
            .and_then(|t| parse_filter(&t).ok());
        self.filter_error = None;
        self.filter = if self.filter_expr.is_some() { f } else { None };
        self.rank_state = RankState::new();
        self.rank_home();
        self.rebuild_rows();
    }

    fn vocabulary(&self) -> Vocabulary {
        let mut seen: HashSet<&str> = HashSet::new();
        let priors = oomtop_core::ranking::ColdStartPriors::detect(&self.snapshot);
        Vocabulary {
            entities: self
                .snapshot
                .groups
                .iter()
                .filter(|g| seen.insert(g.id.as_str()))
                .map(|g| VocabEntity {
                    id: g.id.clone(),
                    name: g.label.clone(),
                    aliases: self.renames.get(&g.fingerprint).cloned().into_iter().collect(),
                    prior: priors.map(|p| p.for_kind(g.kind)).unwrap_or(0.0),
                    frecency: self.frecency.get(&g.fingerprint).copied().unwrap_or(0.0).max(0.0)
                        + if self.pinned.contains(&g.fingerprint) {
                            3.0
                        } else {
                            0.0
                        },
                })
                .collect(),
        }
    }

    fn submit_input(&mut self, done: Input) -> Option<Effect> {
        match done {
            Input::Filter(f) => {
                self.filter_before_edit = None;
                let text = f.trim().to_string();
                let alias = self.aliases.get(&text).cloned();
                if let Some(cmd) = alias.as_deref().and_then(|a| a.strip_prefix(':')) {
                    let cmd = cmd.to_string();
                    return self.run_command(&cmd);
                }
                let expanded = match self.expand_alias(&text) {
                    Ok(t) => t,
                    Err(e) => {
                        self.status = Some(format!("filter: {e} (kept the previous filter)"));
                        return None;
                    }
                };
                match parse_filter(&expanded) {
                    Ok(_) | Err(oomtop_core::query::QueryError::Empty) => {}
                    Err(e) => {
                        self.status = Some(format!("filter: {e} (kept the previous filter)"));
                        return None;
                    }
                }
                self.set_filter(Some(text.clone()));
                // Entity linking: move the cursor to the best frecency-boosted match (`cl` → your Claude Code).
                let words: Vec<String> = text.split_whitespace().map(|w| w.to_lowercase()).collect();
                let targets = link_entities(&words, &self.vocabulary());
                if let Some((id, _)) = targets.first() {
                    if self.view == View::Home || self.view == View::Reclaim {
                        self.select_key(RowKey::Group(id.clone()));
                    }
                }
                if text.is_empty() {
                    None
                } else {
                    Some(Effect::Query(text))
                }
            }
            Input::Command(c) => {
                // A highlighted suggestion completes the command when the user moved the highlight or the typed
                // command still waits for its argument (`sort ` from the F6 picker); otherwise what was typed runs.
                let picked = self.palette.get(self.palette_sel).and_then(|s| match &s.action {
                    PaletteAction::Command(full)
                        if (self.palette_sel > 0 || c.ends_with(' '))
                            && full.starts_with(c.trim())
                            && full.trim() != c.trim() =>
                    {
                        Some(full.clone())
                    }
                    _ => None,
                });
                self.palette.clear();
                let c = picked.unwrap_or(c);
                let eff = self.run_command(&c);
                if eff.is_none() && !c.trim().is_empty() {
                    return Some(Effect::Query(format!(":{}", c.trim())));
                }
                eff
            }
            Input::Palette(p) => {
                let sel = self.palette.get(self.palette_sel).cloned();
                self.palette.clear();
                let eff = match sel {
                    Some(s) => self.run_palette(s.action),
                    None if !p.trim().is_empty() => self.run_understanding(&p),
                    None => None,
                };
                if eff.is_none() && !p.trim().is_empty() {
                    return Some(Effect::Query(p.trim().to_string()));
                }
                eff
            }
            Input::Rename(name) => {
                let g = self.selected_group()?;
                let fp = g.fingerprint.clone();
                let old = self.renames.get(&fp).cloned();
                let alias = Some(name.trim().to_string()).filter(|s| !s.is_empty());
                match &alias {
                    Some(a) => self.renames.insert(fp.clone(), a.clone()),
                    None => self.renames.remove(&fp),
                };
                self.undo.push(Undo::Rename(fp.clone(), old));
                self.status = Some(match &alias {
                    Some(a) => format!("renamed to {a} (usable in queries; u to undo)"),
                    None => "rename cleared".into(),
                });
                self.build_chips();
                Some(Effect::Rename {
                    fingerprint: fp,
                    alias,
                })
            }
        }
    }

    // -----------------------------------------------------------------------------------------------------
    // Commands & palette
    // -----------------------------------------------------------------------------------------------------

    /// Palette suggestions for `text` (UX §5.2): interpretation, entities, commands, past queries.
    pub fn suggestions(&self, text: &str) -> Vec<Suggestion> {
        let mut out: Vec<Suggestion> = Vec::new();
        let t = text.trim();
        if let Some(cmd) = t.strip_prefix(':') {
            let head = cmd.split_whitespace().next().unwrap_or("");
            // `:sort <key>`: the F6 "SortBy" picker — every sort key of this view, filtered by what is typed.
            if head == "sort" && (cmd.len() > 4 || text.ends_with(' ')) {
                let arg = cmd.get(4..).unwrap_or("").trim().to_lowercase();
                let current = match self.view {
                    View::Processes => format!("{:?}", self.proc_sort).to_lowercase(),
                    _ => self.home_sort.as_str().replace("memory", "footprint"),
                };
                for (key, doc) in sort_choices(self.view) {
                    if arg.is_empty() || key.starts_with(&arg) {
                        let mark = if *key == current { " (current)" } else { "" };
                        out.push(Suggestion {
                            label: format!(":sort {key}"),
                            detail: format!("{doc}{mark}"),
                            action: PaletteAction::Command(format!("sort {key}")),
                        });
                    }
                }
                return out;
            }
            for (name, doc) in COMMANDS {
                if name.starts_with(head) || oomtop_core::query::fuzzy_score(head, name).is_some() {
                    let full = if cmd.starts_with(name) {
                        cmd.to_string()
                    } else {
                        name.to_string()
                    };
                    out.push(Suggestion {
                        label: format!(":{full}"),
                        detail: doc.to_string(),
                        action: PaletteAction::Command(full),
                    });
                }
            }
            return out.into_iter().take(8).collect();
        }
        if t.is_empty() {
            for q in self.past_queries.iter().take(4) {
                out.push(Suggestion {
                    label: q.clone(),
                    detail: "recent query".into(),
                    action: if let Some(c) = q.strip_prefix(':') {
                        PaletteAction::Command(c.to_string())
                    } else {
                        PaletteAction::Filter(q.clone())
                    },
                });
            }
            for c in &self.chips {
                if let Some(id) = &c.group_id {
                    out.push(Suggestion {
                        label: c.label.clone(),
                        detail: "your thing".into(),
                        action: PaletteAction::Select(id.clone()),
                    });
                }
            }
            // Insights: the latest timeline markers, one keypress to see them in context.
            for m in self.timeline.markers.iter().rev().take(2) {
                out.push(Suggestion {
                    label: m.text.clone(),
                    detail: format!("insight {}", self.fmt.at(m.t_ms, self.now_ms())),
                    action: PaletteAction::View(View::Timeline),
                });
            }
            for (name, doc) in COMMANDS.iter().take(4) {
                out.push(Suggestion {
                    label: format!(":{name}"),
                    detail: doc.to_string(),
                    action: PaletteAction::Command(name.to_string()),
                });
            }
            return out.into_iter().take(8).collect();
        }
        let u = understand(t, &self.vocabulary());
        let intent_suggestion = |i: &Intent| -> Option<Suggestion> {
            Some(match i {
                Intent::RankBy(m) => Suggestion {
                    label: format!(
                        "Rank by {}",
                        match m {
                            Metric::Mem => "memory",
                            Metric::Gpu => "GPU memory",
                            Metric::Cpu => "CPU",
                            Metric::Idle => "idle time",
                        }
                    ),
                    detail: "sort the Home list".into(),
                    action: PaletteAction::Command(match m {
                        Metric::Cpu => "sort cpu".into(),
                        _ => "sort footprint".into(),
                    }),
                },
                Intent::Explain => Suggestion {
                    label: "Why is it slow?".into(),
                    detail: "ranked causes with evidence".into(),
                    action: PaletteAction::Command("why".into()),
                },
                Intent::Headroom => Suggestion {
                    label: match u.need_bytes {
                        Some(b) => format!("Can I load {}?", self.fmt.bytes(b)),
                        None => "Headroom".into(),
                    },
                    detail: "headroom + can_fit".into(),
                    action: PaletteAction::Command(match u.need_bytes {
                        Some(b) => format!("headroom {b}"),
                        None => "headroom".into(),
                    }),
                },
                Intent::Reclaim => Suggestion {
                    label: "Show reclaim candidates".into(),
                    detail: "idle daemons, orphans, idle model servers".into(),
                    action: PaletteAction::View(View::Reclaim),
                },
                Intent::Navigate(v) => {
                    let v = View::from_query(*v);
                    Suggestion {
                        label: format!("Go to {}", v.title()),
                        detail: "view".into(),
                        action: PaletteAction::View(v),
                    }
                }
                Intent::Find => return None,
            })
        };
        if let Some(f) = &u.filter {
            let _ = f;
            out.push(Suggestion {
                label: format!("Filter: {t}"),
                detail: "structured filter".into(),
                action: PaletteAction::Filter(t.to_string()),
            });
        }
        if u.confidence >= 0.5 {
            if let Some(s) = intent_suggestion(&u.intent) {
                out.push(s);
            }
        }
        for (id, _) in u.targets.iter().take(5) {
            if let Some(g) = self.group(id) {
                out.push(Suggestion {
                    label: self.label(g),
                    detail: format!(
                        "{} · {}",
                        g.kind.alias(),
                        self.fmt.opt_bytes(g.totals.footprint.value)
                    ),
                    action: PaletteAction::Select(id.clone()),
                });
            }
        }
        if u.confidence < 0.5 {
            for alt in u.alternatives.iter().take(3) {
                if let Some(s) = intent_suggestion(alt) {
                    out.push(s);
                }
            }
        }
        for q in &self.past_queries {
            if q != t && oomtop_core::query::fuzzy_score(t, q).is_some() {
                out.push(Suggestion {
                    label: q.clone(),
                    detail: "recent query".into(),
                    action: PaletteAction::Filter(q.clone()),
                });
            }
        }
        if out.is_empty() {
            out.push(Suggestion {
                label: format!("Filter: {t}"),
                detail: "name contains".into(),
                action: PaletteAction::Filter(t.to_string()),
            });
        }
        out.truncate(8);
        out
    }

    fn run_palette(&mut self, a: PaletteAction) -> Option<Effect> {
        match a {
            PaletteAction::Select(id) => {
                if self.view != View::Home {
                    self.set_view(View::Home);
                }
                if !self.rows.iter().any(|r| r.key == RowKey::Group(id.clone())) {
                    self.set_filter(None);
                }
                self.select_key(RowKey::Group(id.clone()));
                let fp = self.group(&id)?.fingerprint.clone();
                Some(Effect::Selected {
                    fingerprint: fp,
                    searched: true,
                })
            }
            PaletteAction::Filter(f) => {
                self.input = Some(Input::Filter(f));
                let done = self.input.take()?;
                self.submit_input(done)
            }
            PaletteAction::Command(c) => self.run_command(&c),
            PaletteAction::Action(a) => self.on_action(&a),
            PaletteAction::View(v) => {
                self.set_view(v);
                None
            }
        }
    }

    fn run_understanding(&mut self, text: &str) -> Option<Effect> {
        let s = self.suggestions(text);
        let first = s.into_iter().next()?;
        self.run_palette(first.action)
    }

    fn find_group_by_name(&self, name: &str) -> Option<String> {
        let words: Vec<String> = name.split_whitespace().map(|w| w.to_lowercase()).collect();
        link_entities(&words, &self.vocabulary())
            .first()
            .map(|(id, _)| id.clone())
    }

    /// Runs a `:` command (without the colon).
    pub fn run_command(&mut self, cmd: &str) -> Option<Effect> {
        let mut parts = cmd.split_whitespace();
        let name = parts.next().unwrap_or("").to_lowercase();
        let args: Vec<&str> = parts.collect();
        let rest = args.join(" ");
        match name.as_str() {
            "" => None,
            "reclaim" => {
                self.set_view(View::Reclaim);
                None
            }
            "why" => {
                self.overlay = Some(Overlay::WhySlow);
                None
            }
            "headroom" | "fit" => {
                let lines = self.headroom_answer(args.first().copied());
                self.overlay = Some(Overlay::Answer("Headroom".into(), lines));
                None
            }
            "pin" | "mute" => {
                if !rest.is_empty() {
                    let id = self.find_group_by_name(&rest);
                    match id {
                        Some(id) => {
                            if self.view != View::Home {
                                self.set_view(View::Home);
                            }
                            self.select_key(RowKey::Group(id));
                        }
                        None => {
                            self.status = Some(format!("no entity matches {rest:?}"));
                            return None;
                        }
                    }
                }
                self.on_action(&name)
            }
            "rename" => {
                if rest.is_empty() {
                    return self.on_action("rename");
                }
                self.input = Some(Input::Rename(rest));
                let done = self.input.take()?;
                self.submit_input(done)
            }
            "mode" => {
                match args.first().copied() {
                    None | Some("auto") | Some("unpin") => {
                        self.modes.pin(None);
                        self.status = Some("mode follows the situation".into());
                    }
                    Some(m) => match Mode::parse(m) {
                        Some(mode) => {
                            self.modes.pin(Some(mode));
                            self.mode = mode;
                            self.status = Some(format!("mode pinned: {m} (:mode auto to unpin)"));
                            self.rank_home();
                            self.rebuild_rows();
                        }
                        None => {
                            self.status = Some(format!(
                                "unknown mode {m:?} (calm, pressure, throttle, working, leftovers)"
                            ))
                        }
                    },
                }
                None
            }
            "sort" => {
                let what = args.first().copied().unwrap_or("rank");
                self.apply_sort(what);
                None
            }
            "filter" => {
                self.set_filter(Some(rest.clone()));
                if self.filter.is_none() && !rest.is_empty() {
                    if let Err(e) = parse_filter(&rest) {
                        self.status = Some(format!("filter: {e}"));
                    }
                }
                None
            }
            "clear" => {
                self.set_filter(None);
                None
            }
            "save" => {
                let Some(n) = args.first() else {
                    self.status = Some(":save NAME saves the current filter as an alias".into());
                    return None;
                };
                let Some(q) = self.filter.clone() else {
                    self.status = Some("no filter to save — type / first".into());
                    return None;
                };
                // Alias names are single words usable inside queries: no filter keys or grammar words
                // (`kind`, `or`, …), nothing that would nest a TOML table.
                let valid = n
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    && !oomtop_config::views::RESERVED.contains(&n.to_lowercase().as_str())
                    && !COMMANDS.iter().any(|(c, _)| c == n);
                if !valid {
                    self.status = Some(format!(
                        "can't save as {n:?}: use letters, digits, - or _, not a filter key or command name"
                    ));
                    return None;
                }
                self.aliases.insert(n.to_string(), q.clone());
                Some(Effect::SaveAlias {
                    name: n.to_string(),
                    query: q,
                })
            }
            "view" => {
                let Some(n) = args.first() else {
                    let names: Vec<&str> = self.views.iter().map(|v| v.name.as_str()).collect();
                    self.status = Some(if names.is_empty() {
                        "no saved views ([[views]] in config.toml)".into()
                    } else {
                        format!("views: {}", names.join(", "))
                    });
                    return None;
                };
                if let Some(v) = self.views.iter().find(|v| v.name == *n).cloned() {
                    self.set_filter(Some(v.query.clone()));
                    self.status = Some(format!("view {}: {}", v.name, v.query));
                    if !v.layout.is_empty() {
                        return Some(Effect::Layout(v.layout));
                    }
                } else if let Some(a) = self.aliases.get(*n).cloned() {
                    self.set_filter(Some(a));
                } else {
                    self.status = Some(format!("no saved view {n:?}"));
                }
                None
            }
            "layout" => match args.first() {
                Some(n) => Some(Effect::Layout(n.to_string())),
                None => {
                    self.status = Some(format!(
                        "layout: {} (:layout NAME, :layout adaptive)",
                        self.layout
                            .as_ref()
                            .map(|l| l.name.as_str())
                            .unwrap_or("adaptive")
                    ));
                    None
                }
            },
            "stop" => self.on_action("stop"),
            "suspend" => self.on_action("suspend"),
            "settings" => self.on_action("settings"),
            "help" => self.on_action("help"),
            "quit" | "q" => Some(Effect::Quit),
            "home" | "processes" | "models" | "sandboxes" | "timeline" => {
                if let Some(v) = View::from_action(&format!("view:{name}")) {
                    self.set_view(v);
                }
                None
            }
            other => {
                if let Some(a) = self.aliases.get(other).cloned() {
                    if let Some(c) = a.strip_prefix(':') {
                        let c = c.to_string();
                        return self.run_command(&c);
                    }
                    self.set_filter(Some(a));
                    return None;
                }
                self.status = Some(format!("unknown command :{other} (try ctrl-k)"));
                None
            }
        }
    }

    fn apply_sort(&mut self, what: &str) {
        let w = what.to_lowercase();
        match w.as_str() {
            "rank" => self.home_sort = HomeSort::Rank,
            "footprint" | "mem" | "memory" => {
                self.home_sort = HomeSort::Footprint;
                self.proc_sort = ProcSort::Footprint;
            }
            "cpu" => {
                self.home_sort = HomeSort::Cpu;
                self.proc_sort = ProcSort::Cpu;
            }
            "name" => {
                self.home_sort = HomeSort::Name;
                self.proc_sort = ProcSort::Name;
            }
            "gpu" => self.home_sort = HomeSort::Gpu,
            "idle" => self.home_sort = HomeSort::Idle,
            "reclaim" => self.home_sort = HomeSort::Reclaim,
            "pid" => self.proc_sort = ProcSort::Pid,
            "resident" | "rss" => self.proc_sort = ProcSort::Resident,
            _ => {
                self.status = Some(format!("unknown sort {what:?}"));
                return;
            }
        }
        self.status = Some(format!("sorted by {w}"));
        self.rank_state = RankState::new();
        self.rank_home();
        self.rebuild_rows();
    }

    /// `:headroom [SIZE]` answer lines (can_fit is advisory, SPEC §8.2).
    pub fn headroom_answer(&self, size: Option<&str>) -> Vec<String> {
        let h = &self.headroom;
        let f = &self.fmt;
        let mut out = vec![match h.headroom {
            Some(x) => format!(
                "Headroom {} = available {} − safety margin {}",
                f.signed(x),
                f.opt_bytes(h.available_now.value),
                f.bytes(h.safety_margin)
            ),
            None => "Headroom unavailable (no available-memory reading)".into(),
        }];
        for g in &h.gpu {
            out.push(format!(
                "GPU {}: budget {} · in use {} · free {}",
                g.accelerator_id,
                f.opt_bytes(g.budget.value),
                f.opt_bytes(g.in_use.value),
                f.opt_bytes(g.free.value)
            ));
        }
        let Some(size) = size else {
            out.push(format!("Reclaimable {}", f.opt_bytes(h.reclaimable.value)));
            return out;
        };
        let bytes = match parse_bytes(size) {
            Ok(b) => b,
            Err(e) => {
                out.push(format!("{size:?}: {e}"));
                return out;
            }
        };
        let need = Need {
            bytes,
            gpu_bytes: None,
            label: None,
        };
        let a = can_fit(&need, h, &reclaim_candidates(&self.snapshot));
        out.push(match &a.fit {
            Fit::Yes => format!("{}: yes — fits now", f.bytes(bytes)),
            Fit::YesAfterReclaim { reclaim, gain } => format!(
                "{}: yes after stopping {} (≈{} freed) — press r",
                f.bytes(bytes),
                reclaim
                    .iter()
                    .map(|c| c.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                f.bytes(*gain)
            ),
            Fit::No { shortfall } => {
                if a.reason.is_empty() {
                    format!(
                        "{}: no — short by {} even after reclaim",
                        f.bytes(bytes),
                        f.bytes(*shortfall)
                    )
                } else {
                    // The core's reason in the TUI's own units, so the line never mixes "13.0G" and "GiB".
                    let reason = oomtop_core::can_fit::reason_with(&a, &|b| f.bytes(b));
                    format!("{}: no — {reason}", f.bytes(bytes))
                }
            }
        });
        if !a.reason.is_empty() && !matches!(a.fit, Fit::No { .. }) {
            out.push(oomtop_core::can_fit::reason_with(&a, &|b| f.bytes(b)));
        }
        out.extend(a.notes.iter().cloned());
        out.push(format!(
            "valid for {} s — advisory (another process may load first)",
            a.valid_for_s
        ));
        out
    }

    // -----------------------------------------------------------------------------------------------------
    // Actions
    // -----------------------------------------------------------------------------------------------------

    fn set_view(&mut self, v: View) {
        if self.view != v {
            self.view = v;
            self.selected = 0;
            self.selected_key = None;
            // The old view's rows must not restore a selection into the new view.
            self.rows.clear();
            self.list_offset.set(0);
        }
        self.overlay = None;
        self.focus = None;
        if v == View::Timeline {
            self.timeline.live();
        }
        self.rebuild_rows();
    }

    fn move_sel(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len() as isize;
        let i = (self.selected as isize + delta).clamp(0, n - 1) as usize;
        self.select(i);
    }

    fn page(&self) -> isize {
        (self.area.height as isize - 12).max(5)
    }

    /// Handles a keymap action.
    pub fn on_action(&mut self, action: &str) -> Option<Effect> {
        if let Some(v) = View::from_action(action) {
            self.set_view(v);
            return None;
        }
        let timeline = self.view == View::Timeline && self.focus.is_none();
        match action {
            "quit" => {
                if self.overlay.is_some() || self.focus.is_some() || self.compare.is_some() {
                    self.overlay = None;
                    self.focus = None;
                    self.compare = None;
                    return None;
                }
                return Some(Effect::Quit);
            }
            "help" => {
                self.overlay = if self.overlay == Some(Overlay::Help) {
                    None
                } else {
                    Some(Overlay::Help)
                };
                self.help_scroll = 0;
            }
            "move:up" => self.move_sel(-1),
            "move:down" => self.move_sel(1),
            "move:page-up" if timeline => {
                self.timeline.scrub_by(-30);
                self.rebuild_rows();
            }
            "move:page-down" if timeline => {
                self.timeline.scrub_by(30);
                self.rebuild_rows();
            }
            "move:page-up" => self.move_sel(-self.page()),
            "move:page-down" => self.move_sel(self.page()),
            "move:top" if timeline => {
                self.timeline.oldest();
                self.rebuild_rows();
            }
            "move:bottom" if timeline => {
                self.timeline.live();
                self.rebuild_rows();
            }
            "move:top" => self.select(0),
            "move:bottom" => self.select(self.rows.len().saturating_sub(1)),
            "view:prev" => {
                let i = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
                self.set_view(View::ALL[(i + View::ALL.len() - 1) % View::ALL.len()]);
            }
            "refresh" => return Some(Effect::Refresh),
            "headline-action" => {
                if self.headline.action_key.is_some() {
                    return self.headline_action();
                }
                self.status = Some("The headline has no action right now.".into());
                return None;
            }
            "open:model" => {
                let key = self.selected_row()?.key.clone();
                let file = match key {
                    RowKey::Model(id) => self
                        .snapshot
                        .model_servers
                        .iter()
                        .find(|m| m.id == id)
                        .and_then(|m| m.models.iter().find_map(|x| x.file.clone())),
                    RowKey::Proc(id) | RowKey::Member(_, id) => {
                        self.process(id).and_then(|p| p.model_files.first().cloned())
                    }
                    _ => self.selected_group().and_then(|g| {
                        g.members
                            .iter()
                            .filter_map(|m| self.process(m.id))
                            .find_map(|p| p.model_files.first().cloned())
                    }),
                };
                match file {
                    Some(f) => return Some(Effect::Open(f)),
                    None => self.status = Some("no model file known for this row".into()),
                }
            }
            "open:log" => {
                self.status =
                    Some("no log file is known for this entity (adapters report logs from M5)".into());
            }
            "view:next" => {
                let i = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
                self.set_view(View::ALL[(i + 1) % View::ALL.len()]);
            }
            "filter" => {
                self.filter_before_edit = self.filter.clone();
                self.input = Some(Input::Filter(self.filter.clone().unwrap_or_default()));
            }
            "command" => {
                self.input = Some(Input::Command(String::new()));
                self.palette = self.suggestions(":");
                self.palette_sel = 0;
            }
            "palette" => {
                self.input = Some(Input::Palette(String::new()));
                self.palette = self.suggestions("");
                self.palette_sel = 0;
            }
            "expand" if timeline => {
                self.timeline.scrub_by(1);
                self.rebuild_rows();
            }
            "collapse" if timeline => {
                self.timeline.scrub_by(-1);
                self.rebuild_rows();
            }
            "expand" => return self.expand(),
            "collapse" => self.collapse(),
            "stop" => self.begin_stop(),
            "suspend" => self.begin_suspend(),
            "pin" => return self.toggle_pin(),
            "mute" => return self.toggle_mute(),
            "less" => {
                let g = self.selected_group()?;
                let fp = g.fingerprint.clone();
                let label = self.label(g);
                *self.less.entry(fp.clone()).or_insert(0) += 1;
                self.undo.push(Undo::Less(fp.clone()));
                self.status = Some(format!("showing less like {label} (u to undo)"));
                self.rank_home();
                self.rebuild_rows();
                return Some(Effect::Less { fingerprint: fp });
            }
            "rename" => {
                let g = self.selected_group()?;
                let cur = self.renames.get(&g.fingerprint).cloned().unwrap_or_default();
                self.input = Some(Input::Rename(cur));
            }
            "undo" => return self.undo_last(),
            "pin-mode" => {
                if self.modes.is_pinned() {
                    self.modes.pin(None);
                    self.status = Some("mode follows the situation again".into());
                } else {
                    self.modes.pin(Some(self.mode));
                    self.status = Some(format!("mode pinned: {} (M to unpin)", self.mode.as_str()));
                }
            }
            "why" => {
                self.overlay = if self.overlay == Some(Overlay::WhyRank) {
                    None
                } else {
                    Some(Overlay::WhyRank)
                };
            }
            "watch" => {
                if self.focus.is_some() {
                    self.focus = None;
                } else if let Some(r) = self.selected_row() {
                    let key = match &r.key {
                        RowKey::Member(g, _) | RowKey::Past(g) => RowKey::Group(g.clone()),
                        RowKey::ReclaimAll => return None,
                        k => k.clone(),
                    };
                    self.focus = Some(key);
                    self.overlay = None;
                }
            }
            "compare" => self.compare_step(),
            "settings" => return Some(Effect::OpenSettings),
            "sort-by" => {
                // htop F6: choose the sort column (a picker over the command palette).
                self.input = Some(Input::Command("sort ".into()));
                self.palette = self.suggestions(":sort ");
                let cur = self.palette.iter().position(|s| s.detail.ends_with("(current)"));
                self.palette_sel = cur.unwrap_or(0);
            }
            "tree" => match self.view {
                View::Processes => {
                    self.proc_tree = !self.proc_tree;
                    self.status = Some(
                        if self.proc_tree {
                            "tree: processes nested under their parents (F5 for a flat list)"
                        } else {
                            "flat list (F5 for the process tree)"
                        }
                        .into(),
                    );
                    self.rebuild_rows();
                }
                View::Home | View::Reclaim => {
                    let ids: Vec<String> = self.ranked.iter().map(|r| r.id.clone()).collect();
                    if ids.iter().any(|id| self.expanded.contains(id)) {
                        self.expanded.clear();
                        self.status = Some("collapsed all groups".into());
                    } else {
                        self.expanded.extend(ids);
                        self.status = Some("expanded all groups (F5 to collapse)".into());
                    }
                    if self.view != View::Home {
                        self.set_view(View::Home);
                    }
                    self.rebuild_rows();
                }
                _ => self.status = Some("tree applies to Home and Processes".into()),
            },
            "sort" => {
                if self.view == View::Processes {
                    self.proc_sort = match self.proc_sort {
                        ProcSort::Footprint => ProcSort::Cpu,
                        ProcSort::Cpu => ProcSort::Resident,
                        ProcSort::Resident => ProcSort::Pid,
                        ProcSort::Pid => ProcSort::Name,
                        ProcSort::Name => ProcSort::Footprint,
                    };
                    self.status = Some(format!("sorted by {:?}", self.proc_sort).to_lowercase());
                } else {
                    self.home_sort = match self.home_sort {
                        HomeSort::Rank => HomeSort::Footprint,
                        HomeSort::Footprint => HomeSort::Cpu,
                        HomeSort::Cpu => HomeSort::Name,
                        HomeSort::Name | HomeSort::Gpu | HomeSort::Idle | HomeSort::Reclaim => HomeSort::Rank,
                    };
                    self.status = Some(format!("sorted by {:?}", self.home_sort).to_lowercase());
                    self.rank_state = RankState::new();
                    self.rank_home();
                }
                self.rebuild_rows();
            }
            "open:cwd" => {
                let key = self.selected_row()?.key.clone();
                let path = match key {
                    RowKey::Proc(id) | RowKey::Member(_, id) => self.process(id).and_then(|p| p.cwd.clone()),
                    RowKey::Model(id) => self
                        .snapshot
                        .model_servers
                        .iter()
                        .find(|m| m.id == id)
                        .and_then(|m| m.models.first().and_then(|x| x.file.clone())),
                    _ => self
                        .selected_group()
                        .and_then(|g| g.root)
                        .and_then(|r| self.process(r))
                        .and_then(|p| p.cwd.clone()),
                };
                match path {
                    Some(p) => return Some(Effect::Open(p)),
                    None => self.status = Some("no cwd or file known for this row".into()),
                }
            }
            _ => {}
        }
        None
    }

    fn expand(&mut self) -> Option<Effect> {
        let row = self.selected_row()?.clone();
        match &row.key {
            RowKey::Group(id) if self.view == View::Home => {
                if self.expanded.contains(id) {
                    if self.layout() != LayoutClass::Wide {
                        self.overlay = Some(Overlay::Details);
                    }
                } else {
                    self.expanded.insert(id.clone());
                    self.rebuild_rows();
                }
            }
            RowKey::ReclaimAll => {
                self.begin_stop();
                return None;
            }
            RowKey::Past(_) => return None,
            _ => {
                if self.layout() != LayoutClass::Wide || self.view != View::Home {
                    self.overlay = Some(Overlay::Details);
                }
            }
        }
        let g = self.group_for_key(&row.key)?;
        Some(Effect::Selected {
            fingerprint: g.fingerprint.clone(),
            searched: self.filter.is_some(),
        })
    }

    fn collapse(&mut self) {
        if self.overlay.is_some() {
            self.overlay = None;
            return;
        }
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        match row.key {
            RowKey::Member(g, _) => {
                self.expanded.remove(&g);
                self.rebuild_rows();
                self.select_key(RowKey::Group(g));
            }
            RowKey::Group(g) if self.expanded.remove(&g) => self.rebuild_rows(),
            _ => {}
        }
    }

    fn compare_step(&mut self) {
        let Some(key) = self.selected_row().map(|r| match &r.key {
            RowKey::Member(g, _) | RowKey::Past(g) => RowKey::Group(g.clone()),
            k => k.clone(),
        }) else {
            return;
        };
        if key == RowKey::ReclaimAll {
            return;
        }
        match self.compare.take() {
            None => {
                self.compare = Some((key, None));
                self.status = Some("compare: select a second entity and press c".into());
            }
            Some((a, None)) if a != key => {
                self.compare = Some((a, Some(key)));
                self.status = None;
            }
            Some((a, None)) => {
                self.compare = Some((a, None));
                self.status = Some("compare: pick a different entity".into());
            }
            Some((_, Some(_))) => {
                self.compare = None;
            }
        }
    }

    fn toggle_pin(&mut self) -> Option<Effect> {
        let g = self.selected_group()?;
        let fp = g.fingerprint.clone();
        let label = self.label(g);
        let was = self.pinned.contains(&fp);
        if was {
            self.pinned.remove(&fp);
        } else {
            self.pinned.insert(fp.clone());
            self.entity_names.insert(fp.clone(), label.clone());
        }
        self.undo.push(Undo::Pin(fp.clone(), was));
        self.status = Some(format!(
            "{} {label} (u to undo)",
            if was { "unpinned" } else { "pinned" }
        ));
        self.build_chips();
        self.rank_home();
        self.rebuild_rows();
        Some(Effect::Pin {
            fingerprint: fp,
            pinned: !was,
        })
    }

    fn toggle_mute(&mut self) -> Option<Effect> {
        let g = self.selected_group()?;
        let fp = g.fingerprint.clone();
        let label = self.label(g);
        let was = self.muted.contains(&fp);
        if was {
            self.muted.remove(&fp);
        } else {
            self.muted.insert(fp.clone());
        }
        self.undo.push(Undo::Mute(fp.clone(), was));
        self.status = Some(if was {
            format!("unmuted {label}")
        } else {
            format!("muted {label} for 30 days — ranked lower, never hidden (u to undo)")
        });
        self.build_chips();
        self.rank_home();
        self.rebuild_rows();
        Some(Effect::Mute {
            fingerprint: fp,
            until_ms: (!was).then(|| self.now_ms() + MUTE_MS),
        })
    }

    fn undo_last(&mut self) -> Option<Effect> {
        let Some(u) = self.undo.pop() else {
            self.status = Some("nothing to undo (stops are never undoable — they always confirm)".into());
            return None;
        };
        let eff = match u {
            Undo::Pin(fp, was) => {
                if was {
                    self.pinned.insert(fp.clone());
                } else {
                    self.pinned.remove(&fp);
                }
                self.status = Some(format!(
                    "undone: {}",
                    if was { "pin restored" } else { "unpinned" }
                ));
                Effect::Pin {
                    fingerprint: fp,
                    pinned: was,
                }
            }
            Undo::Mute(fp, was) => {
                if was {
                    self.muted.insert(fp.clone());
                } else {
                    self.muted.remove(&fp);
                }
                self.status = Some(format!("undone: {}", if was { "muted again" } else { "unmuted" }));
                Effect::Mute {
                    fingerprint: fp,
                    until_ms: was.then(|| self.now_ms() + MUTE_MS),
                }
            }
            Undo::Rename(fp, old) => {
                match &old {
                    Some(a) => self.renames.insert(fp.clone(), a.clone()),
                    None => self.renames.remove(&fp),
                };
                self.status = Some("undone: rename".into());
                Effect::Rename {
                    fingerprint: fp,
                    alias: old,
                }
            }
            Undo::Less(fp) => {
                if let Some(n) = self.less.get_mut(&fp) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        self.less.remove(&fp);
                    }
                }
                self.status = Some("undone: show less".into());
                self.rank_home();
                self.rebuild_rows();
                // "Less" events are append-only in the profile; the in-session penalty is reverted.
                return None;
            }
        };
        self.build_chips();
        self.rank_home();
        self.rebuild_rows();
        Some(eff)
    }

    /// Checks the root process of a target against the protect rules (belt and braces over `plan_group`).
    fn protected_root(&self, t: &ActionTarget) -> Option<String> {
        let p = self.process(t.root)?;
        is_protected_process(p, &self.snapshot, &self.protect).then(|| format!("{} is protected", t.label))
    }

    fn plan_for(&self, g: &Group, kind: ActionKind) -> Result<ActionTarget, String> {
        let mut t = plan_group(g, kind, &self.protect).map_err(|r| r.to_string())?;
        if let Some(msg) = self.protected_root(&t) {
            return Err(msg);
        }
        t.label = self.label(g);
        Ok(t)
    }

    fn begin_stop(&mut self) {
        let Some(row) = self.selected_row().cloned() else {
            self.status = Some("nothing selected".into());
            return;
        };
        let f = self.fmt.clone();
        if row.key == RowKey::ReclaimAll {
            let (targets, refused) = self.reclaim_all_targets();
            if targets.is_empty() {
                self.status = Some(if refused.is_empty() {
                    "nothing to reclaim".into()
                } else {
                    refused.join("; ")
                });
                return;
            }
            let gain: u64 = targets.iter().filter_map(|t| t.expected_gain).sum();
            let names: Vec<&str> = targets.iter().map(|t| t.label.as_str()).collect();
            let prompt = format!(
                "Stop {} group{} ({}; ≈{} freed) with SIGTERM? y / n",
                targets.len(),
                if targets.len() == 1 { "" } else { "s" },
                names.join(", "),
                f.bytes(gain)
            );
            self.confirm = Some(Confirm {
                plan: ActionPlan {
                    targets,
                    ..Default::default()
                },
                prompt,
                row: Some(RowKey::ReclaimAll),
            });
            return;
        }
        let Some(g) = self.group_for_key(&row.key) else {
            self.status = Some("this row has no group to act on".into());
            return;
        };
        let label = self.label(g);
        // Second x on a target that ignored SIGTERM → SIGKILL offer (second explicit confirmation).
        let root = g.root;
        let pending_term = self
            .pending
            .iter()
            .find(|p| Some(p.target.root) == root && p.target.kind == ActionKind::Terminate)
            .cloned();
        if let Some(p) = pending_term {
            if self.process(p.target.root).is_some() {
                // "Ignored SIGTERM" needs evidence: a sample taken after the signal still shows the process.
                if self.now_ms() <= p.sent_ms {
                    self.status = Some(format!(
                        "{label}: SIGTERM sent — waiting for the next sample before SIGKILL can be offered"
                    ));
                    return;
                }
                let mut t = p.target.clone();
                t.kind = ActionKind::Kill;
                let secs = self.now_ms().saturating_sub(p.sent_ms) / 1000;
                self.confirm = Some(Confirm {
                    prompt: format!(
                        "{label} ignored SIGTERM ({secs}s ago). Force kill with SIGKILL (≈{} freed, no cleanup)? y / n",
                        f.opt_bytes(t.expected_gain)
                    ),
                    plan: ActionPlan {
                        targets: vec![t],
                        ..Default::default()
                    },
                    row: Some(row.key.clone()),
                });
                return;
            }
        }
        match self.plan_for(g, ActionKind::Terminate) {
            Ok(t) => {
                let helper = matches!(row.key, RowKey::Member(..) | RowKey::Proc(_))
                    && self.selected_process_id() != Some(t.root);
                let scope = if helper {
                    format!(
                        " — acts on the group root ({} process{})",
                        g.totals.process_count,
                        if g.totals.process_count == 1 { "" } else { "es" }
                    )
                } else if g.totals.process_count > 1 {
                    format!(", {} processes", g.totals.process_count)
                } else {
                    String::new()
                };
                let gain = match t.expected_gain {
                    Some(b) => format!("≈{} freed", f.bytes(b)),
                    None => "gain unknown".into(),
                };
                // Same rule as the adapters' stop guard (SPEC §10): say when a job runs, and when "idle"
                // is only a CPU heuristic (a GPU-bound job can run with an idle CPU).
                let server = self
                    .snapshot
                    .model_servers
                    .iter()
                    .find(|m| m.group_id.as_deref() == Some(g.id.as_str()));
                let warn = match server {
                    Some(m) if m.busy.value == Some(true) => " A job is running.",
                    Some(m) if m.queue.value.unwrap_or(0) > 0 => " Requests are queued.",
                    Some(m) if m.busy.quality != Quality::Exact => " Idle state not confirmed by the server.",
                    _ => "",
                };
                self.confirm = Some(Confirm {
                    prompt: format!("Stop {label} (SIGTERM, {gain}{scope})?{warn} y / n"),
                    plan: ActionPlan {
                        targets: vec![t],
                        ..Default::default()
                    },
                    row: Some(row.key.clone()),
                });
            }
            Err(e) => self.status = Some(format!("can't stop: {e}")),
        }
    }

    /// Targets of "reclaim all" (the Reclaim view's first row and the headline key): every reclaim candidate
    /// that matches the filter, minus the calling agent's session, planned against the protect rules. The row,
    /// the view summary and the confirmation all use this list, so what is shown is what is sent.
    pub fn reclaim_all_targets(&self) -> (Vec<ActionTarget>, Vec<String>) {
        let mut targets = Vec::new();
        let mut refused = Vec::new();
        for c in reclaim_candidates(&self.snapshot) {
            if self.protect.caller_group.as_deref() == Some(c.group_id.as_str()) {
                continue;
            }
            let Some(g) = self.group(&c.group_id) else {
                continue;
            };
            if !self.matches_filter_group(g) {
                continue;
            }
            match self.plan_for(g, ActionKind::Terminate) {
                Ok(mut t) => {
                    // The candidate's gain is the one can_fit and the headline use.
                    t.expected_gain = t.expected_gain.or(Some(c.gain));
                    targets.push(t)
                }
                Err(e) => refused.push(e),
            }
        }
        (targets, refused)
    }

    fn selected_process_id(&self) -> Option<ProcId> {
        match self.selected_row()?.key {
            RowKey::Member(_, id) | RowKey::Proc(id) => Some(id),
            _ => None,
        }
    }

    fn begin_suspend(&mut self) {
        let Some(row) = self.selected_row().cloned() else {
            return;
        };
        if row.key == RowKey::ReclaimAll {
            self.status = Some("suspend frees no memory, so it is never part of reclaim".into());
            return;
        }
        let Some(g) = self.group_for_key(&row.key) else {
            self.status = Some("this row has no group to act on".into());
            return;
        };
        let label = self.label(g);
        let stopped = g
            .root
            .and_then(|r| self.process(r))
            .map(|p| p.state == ProcState::Stopped)
            .unwrap_or(false);
        let kind = if stopped {
            ActionKind::Resume
        } else {
            ActionKind::Suspend
        };
        // Suspend's gain is CPU (and heat), never memory: name it so the confirmation shows what it buys.
        let cpu = g
            .totals
            .cpu_pct
            .value
            .map(|c| format!("≈{} CPU", self.fmt.cpu(c)))
            .unwrap_or_else(|| "CPU n/a".into());
        match self.plan_for(g, kind) {
            Ok(t) => {
                let prompt = if stopped {
                    format!("Resume {label} (SIGCONT)? y / n")
                } else {
                    format!(
                        "Suspend {label} (SIGSTOP, {cpu} relief)? CPU/thermal relief only — frees no memory. y / n"
                    )
                };
                self.confirm = Some(Confirm {
                    plan: ActionPlan {
                        targets: vec![t],
                        ..Default::default()
                    },
                    prompt,
                    row: Some(row.key.clone()),
                });
            }
            Err(e) => self.status = Some(format!("can't suspend: {e}")),
        }
    }

    /// Records the actuator's outcomes: status line + follow-up for SIGTERM (kill offer, measured gain).
    pub fn on_outcomes(&mut self, outcomes: &[ActionOutcome]) {
        let now = self.now_ms();
        let avail = self.headroom.available_now.value;
        let mut msgs = Vec::new();
        for o in outcomes {
            msgs.push(format!("{}: {}", o.target.label, o.message));
            if o.ok && matches!(o.target.kind, ActionKind::Terminate | ActionKind::Kill) {
                self.pending.retain(|p| p.target.root != o.target.root);
                self.pending.push(Pending {
                    target: o.target.clone(),
                    sent_ms: now,
                    available_before: avail,
                    nagged: false,
                });
            }
        }
        if !msgs.is_empty() {
            self.status = Some(msgs.join("; "));
        }
    }

    // -----------------------------------------------------------------------------------------------------
    // Settings & personalization inputs from the loop
    // -----------------------------------------------------------------------------------------------------

    pub fn show_settings(&mut self, model: SettingsModel) {
        self.settings = Some(SettingsState::new(model));
        self.overlay = None;
    }

    /// Loads personalization state (from `oomtop-state`): frecency → affinity, pins, mutes, renames.
    pub fn set_profile(
        &mut self,
        frecency: HashMap<String, f64>,
        entities: &[oomtop_state::EntityRecord],
        past_queries: Vec<String>,
    ) {
        self.affinity = frecency
            .iter()
            .map(|(k, v)| (k.clone(), oomtop_core::ranking::affinity_from_frecency(*v)))
            .filter(|(_, a)| *a > 0.0)
            .collect();
        self.frecency = frecency;
        let now = self.now_ms();
        for e in entities {
            self.entity_names
                .insert(e.fingerprint.clone(), e.display_name.clone());
            if e.pinned {
                self.pinned.insert(e.fingerprint.clone());
            }
            if e.muted_until_ms.map(|u| u > now).unwrap_or(false) {
                self.muted.insert(e.fingerprint.clone());
            }
            if let Some(a) = &e.alias {
                self.renames.insert(e.fingerprint.clone(), a.clone());
            }
        }
        self.past_queries = past_queries;
        self.build_chips();
        self.rank_home();
        self.rebuild_rows();
    }

    // -----------------------------------------------------------------------------------------------------
    // Mouse
    // -----------------------------------------------------------------------------------------------------

    pub fn on_mouse(&mut self, m: MouseInput) -> Option<Effect> {
        // A pending confirmation only accepts `y` from the keyboard: a click cancels it (like any other key)
        // and scrolling is ignored, so the row it names never slides away under the prompt.
        if self.confirm.is_some() {
            if m.kind == MouseKind::Down {
                self.confirm = None;
                self.status = Some("cancelled — nothing was sent".into());
            }
            return None;
        }
        let regions = self.regions.borrow().clone();
        match m.kind {
            MouseKind::ScrollUp => {
                if let Some(st) = self.settings.as_mut() {
                    st.move_by(-3);
                } else if self.overlay == Some(Overlay::Help) {
                    self.help_scroll = self.help_scroll.saturating_sub(3);
                } else {
                    self.move_sel(-3);
                }
                None
            }
            MouseKind::ScrollDown => {
                if let Some(st) = self.settings.as_mut() {
                    st.move_by(3);
                } else if self.overlay == Some(Overlay::Help) {
                    self.help_scroll += 3;
                } else {
                    self.move_sel(3);
                }
                None
            }
            MouseKind::Drag => {
                if let Some(r) = regions.timeline.filter(|r| r.width > 1) {
                    if self.view == View::Timeline && m.col >= r.x {
                        let frac = (m.col - r.x) as f64 / (r.width - 1) as f64;
                        self.timeline.scrub_to_fraction(frac);
                        self.rebuild_rows();
                    }
                }
                None
            }
            MouseKind::Down => {
                if self.settings.is_some() {
                    if let Some(i) = regions.settings_index(m.col, m.row) {
                        if let Some(st) = self.settings.as_mut() {
                            if i < st.model.rows.len() {
                                if st.selected == i {
                                    return st.cycle(1).map(Effect::Settings);
                                }
                                st.selected = i;
                            }
                        }
                    }
                    return None;
                }
                if let Some(t) = regions.hit(m.col, m.row).cloned() {
                    self.last_click = None;
                    return match t {
                        HitTarget::View(v) => {
                            self.set_view(v);
                            None
                        }
                        HitTarget::WhySlow => {
                            self.overlay = Some(Overlay::WhySlow);
                            None
                        }
                        HitTarget::Help => self.on_action("help"),
                        HitTarget::Action(a) => self.on_action(&a),
                        HitTarget::Chip(id) => {
                            if self.view != View::Home {
                                self.set_view(View::Home);
                            }
                            self.select_key(RowKey::Group(id.clone()));
                            let fp = self.group(&id).map(|g| g.fingerprint.clone())?;
                            Some(Effect::Selected {
                                fingerprint: fp,
                                searched: false,
                            })
                        }
                    };
                }
                if self.view == View::Timeline {
                    if let Some(r) = regions
                        .timeline
                        .filter(|r| crate::layout::contains(*r, m.col, m.row) && r.width > 1)
                    {
                        let frac = (m.col - r.x) as f64 / (r.width - 1) as f64;
                        self.timeline.scrub_to_fraction(frac);
                        self.rebuild_rows();
                        return None;
                    }
                }
                if let Some(i) = regions.list_index(m.col, m.row) {
                    if i >= self.rows.len() {
                        return None;
                    }
                    let double = self
                        .last_click
                        .map(|(t, c, r)| {
                            m.at_ms.saturating_sub(t) <= DOUBLE_CLICK_MS
                                && r == m.row
                                && c.abs_diff(m.col) <= 2
                        })
                        .unwrap_or(false);
                    self.last_click = Some((m.at_ms, m.col, m.row));
                    let was_selected = self.selected == i;
                    self.select(i);
                    if double {
                        self.last_click = None;
                        self.overlay = Some(Overlay::Details);
                        return None;
                    }
                    if was_selected {
                        return self.expand();
                    }
                }
                None
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fixtures;

    pub(crate) fn fixture() -> Snapshot {
        fixtures::motivating()
    }

    pub(crate) fn app_with(s: Snapshot) -> App {
        let mut app = App::new(&Config::default());
        app.set_protect(fixtures::protect());
        let h = fixtures::history_for(&s, 30);
        app.update(s, &h);
        app
    }

    fn km() -> Keymap {
        oomtop_config::keymap::preset("default").unwrap()
    }

    fn press(app: &mut App, keys: &[&str]) -> Vec<Effect> {
        let k = km();
        keys.iter().filter_map(|key| app.on_key(key, &k)).collect()
    }

    fn select_group(app: &mut App, id: &str) {
        let i = app
            .rows
            .iter()
            .position(|r| r.key == RowKey::Group(id.into()))
            .unwrap();
        app.select(i);
    }

    #[test]
    fn first_sample_is_pressure_mode_with_headline() {
        let app = app_with(fixture());
        assert_eq!(app.mode, Mode::Pressure);
        assert!(
            app.headline.text.starts_with("Tight on memory"),
            "{}",
            app.headline.text
        );
        assert_eq!(app.headline.action_key, Some('r'));
        assert!(!app.rows.is_empty());
    }

    #[test]
    fn headline_action_is_a_keymap_action() {
        // Default preset: `r` runs the headline's action and the hint says [r].
        let mut app = app_with(fixture());
        app.set_keymap(&km());
        assert_eq!(app.key_for("headline-action").as_deref(), Some("r"));
        press(&mut app, &["r"]);
        assert!(app.confirm.is_some(), "r opens the reclaim confirmation");
        // A user who binds `r` to refresh gets refresh, and the hint follows the rebinding.
        let (binds, errs) = oomtop_config::keymap::parse_keymap_file(
            "[global]\n\"r\" = \"refresh\"\n\"R\" = \"headline-action\"\n",
            std::path::Path::new("keymap.toml"),
        );
        assert!(errs.is_empty(), "{errs:?}");
        let mut user = km();
        user.apply_user(&binds);
        let mut app = app_with(fixture());
        app.set_keymap(&user);
        assert_eq!(app.key_for("headline-action").as_deref(), Some("R"));
        assert_eq!(app.on_key("r", &user), Some(Effect::Refresh));
        assert!(app.confirm.is_none());
    }

    #[test]
    fn stop_needs_confirmation_and_names_target_and_gain() {
        let mut app = app_with(fixture());
        select_group(&mut app, "daemon:gradle");
        assert!(press(&mut app, &["x"]).is_empty());
        let c = app.confirm.clone().unwrap();
        assert!(
            c.prompt.starts_with("Stop GradleDaemon 9.8 (SIGTERM, ≈"),
            "{}",
            c.prompt
        );
        assert!(c.prompt.contains("freed"));
        assert_eq!(press(&mut app, &["n"]), vec![]);
        assert!(app.confirm.is_none());
        assert_eq!(app.status.as_deref(), Some("cancelled — nothing was sent"));
        let eff = press(&mut app, &["x", "y"]);
        match &eff[..] {
            [Effect::Execute(p)] => {
                assert!(p.confirmed && !p.confirmed_kill);
                assert_eq!(p.targets[0].kind, ActionKind::Terminate);
                assert_eq!(p.targets[0].root.pid, 5100);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sigkill_only_on_second_confirmation_after_sigterm() {
        let mut app = app_with(fixture());
        select_group(&mut app, "daemon:gradle");
        let eff = press(&mut app, &["x", "y"]);
        let Effect::Execute(plan) = &eff[0] else { panic!() };
        app.on_outcomes(&[ActionOutcome {
            target: plan.targets[0].clone(),
            ok: true,
            message: "sent SIGTERM".into(),
            measured_gain: None,
        }]);
        // Still alive 6 s later → hint, then x offers SIGKILL.
        let mut s = fixture();
        s.taken_at_ms += 6_000;
        app.update(s, &History::default());
        assert!(app
            .status
            .as_deref()
            .unwrap()
            .contains("press x again to force kill"));
        select_group(&mut app, "daemon:gradle");
        press(&mut app, &["x"]);
        assert!(app.confirm.as_ref().unwrap().prompt.contains("SIGKILL"));
        let eff = press(&mut app, &["y"]);
        match &eff[..] {
            [Effect::Execute(p)] => {
                assert!(p.confirmed && p.confirmed_kill);
                assert_eq!(p.targets[0].kind, ActionKind::Kill);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn reverify_identity_before_release() {
        let mut app = app_with(fixture());
        select_group(&mut app, "daemon:gradle");
        press(&mut app, &["x"]);
        // pid reused: same pid, different start time.
        let mut s = fixture();
        s.taken_at_ms += 2000;
        for p in s.processes.iter_mut() {
            if p.id.pid == 5100 {
                p.id.start_time += 1;
            }
        }
        for g in s.groups.iter_mut() {
            for m in g.members.iter_mut() {
                if m.id.pid == 5100 {
                    m.id.start_time += 1;
                }
            }
            if g.root.map(|r| r.pid) == Some(5100) {
                g.root = Some(ProcId::new(5100, g.root.unwrap().start_time + 1));
            }
        }
        app.update(s, &History::default());
        assert!(app.confirm.is_some(), "confirmation survives a refresh");
        assert_eq!(press(&mut app, &["y"]), vec![]);
        assert!(app.status.as_deref().unwrap().contains("nothing was sent"));
    }

    #[test]
    fn protected_self_and_system_are_refused() {
        let mut app = app_with(fixture());
        app.set_filter(Some("WindowServer or oomtop".into()));
        for id in ["system:windowserver", "self:oomtop"] {
            select_group(&mut app, id);
            press(&mut app, &["x"]);
            assert!(app.confirm.is_none(), "{id}");
            assert!(
                app.status.as_deref().unwrap().starts_with("can't stop"),
                "{:?}",
                app.status
            );
        }
    }

    #[test]
    fn suspend_is_relief_only_and_resume_when_stopped() {
        let mut app = app_with(fixture());
        select_group(&mut app, "model:sd-server");
        press(&mut app, &["z"]);
        let c = app.confirm.clone().unwrap();
        assert!(c.prompt.contains("frees no memory"));
        assert_eq!(c.plan.targets[0].expected_gain, None);
        press(&mut app, &["esc"]);
        let mut s = fixture();
        s.taken_at_ms += 2000;
        for p in s.processes.iter_mut() {
            if p.id.pid == 4242 {
                p.state = ProcState::Stopped;
            }
        }
        app.update(s, &History::default());
        select_group(&mut app, "model:sd-server");
        press(&mut app, &["z"]);
        assert!(app.confirm.as_ref().unwrap().prompt.starts_with("Resume"));
    }

    #[test]
    fn headline_key_reclaims_all_with_one_confirmation() {
        let mut app = app_with(fixture());
        press(&mut app, &["r"]);
        assert_eq!(app.view, View::Reclaim);
        let c = app.confirm.clone().unwrap();
        assert!(c.prompt.starts_with("Stop 3 groups"), "{}", c.prompt);
        let eff = press(&mut app, &["y"]);
        let [Effect::Execute(p)] = &eff[..] else {
            panic!("{eff:?}")
        };
        let pids: Vec<u32> = p.targets.iter().map(|t| t.root.pid).collect();
        assert!(
            pids.contains(&5100) && pids.contains(&5200) && pids.contains(&6100),
            "{pids:?}"
        );
    }

    #[test]
    fn views_filter_and_keys() {
        let mut app = app_with(fixture());
        press(
            &mut app,
            &["/", "k", "i", "n", "d", ":", "d", "a", "e", "m", "o", "n"],
        );
        assert_eq!(app.rows.len(), 2, "live filter while typing");
        let eff = press(&mut app, &["enter"]);
        assert_eq!(eff, vec![Effect::Query("kind:daemon".into())]);
        assert_eq!(app.filter.as_deref(), Some("kind:daemon"));
        press(&mut app, &["esc"]);
        assert!(app.filter.is_none());
        press(&mut app, &["5"]);
        assert_eq!(app.view, View::Reclaim);
        assert_eq!(app.rows[0].key, RowKey::ReclaimAll);
        press(&mut app, &["tab"]);
        assert_eq!(app.view, View::Timeline);
        press(&mut app, &["2"]);
        assert_eq!(app.view, View::Processes);
        assert_eq!(app.rows.len(), app.snapshot.processes.len());
        press(&mut app, &["1", "G"]);
        assert_eq!(app.selected, app.rows.len() - 1);
        press(&mut app, &["g", "j", "j", "k"]);
        assert_eq!(app.selected, 1);
        assert_eq!(press(&mut app, &["q"]), vec![Effect::Quit]);
    }

    #[test]
    fn invalid_filter_keeps_previous() {
        let mut app = app_with(fixture());
        app.set_filter(Some("kind:daemon".into()));
        press(&mut app, &["/", "ctrl-u", "m", "e", "m", ">"]);
        assert!(app.filter_error.is_some());
        press(&mut app, &["enter"]);
        assert_eq!(app.filter.as_deref(), Some("kind:daemon"));
        assert!(app
            .status
            .as_deref()
            .unwrap()
            .contains("kept the previous filter"));
    }

    #[test]
    fn typing_cl_selects_your_claude_code_not_clang() {
        let mut app = app_with(fixture());
        let mut f = HashMap::new();
        f.insert("fp-claude-code".to_string(), 6.0);
        app.set_profile(f, &[], vec![]);
        press(&mut app, &["/", "c", "l", "enter"]);
        let g = app.selected_group().unwrap();
        assert_eq!(g.label, "Claude Code", "selected {}", g.id);
    }

    #[test]
    fn typing_cl_cold_start_prefers_the_detected_agent_over_clang() {
        // No history at all (UX §5.4 cold start): the detected Claude Code session outranks `clang`.
        let mut app = app_with(fixture());
        press(&mut app, &["/", "c", "l", "enter"]);
        let g = app.selected_group().unwrap();
        assert_eq!(g.kind, GroupKind::AgentSession, "selected {} ({})", g.id, g.label);
    }

    #[test]
    fn expand_nests_members_and_collapse_returns() {
        let mut app = app_with(fixture());
        app.area = Rect::new(0, 0, 160, 50);
        select_group(&mut app, "app:google-chrome");
        let eff = press(&mut app, &["enter"]);
        assert!(matches!(&eff[..], [Effect::Selected { fingerprint, .. }] if fingerprint == "fp-chrome"));
        let n = app
            .rows
            .iter()
            .filter(|r| matches!(&r.key, RowKey::Member(g, _) if g == "app:google-chrome"))
            .count();
        assert_eq!(n, 14);
        press(&mut app, &["j", "j"]);
        assert!(matches!(app.selected_row().unwrap().key, RowKey::Member(..)));
        // x on a helper acts on the group root.
        press(&mut app, &["x"]);
        let c = app.confirm.clone().unwrap();
        assert_eq!(c.plan.targets[0].root.pid, 800);
        assert!(c.prompt.contains("acts on the group root"), "{}", c.prompt);
        press(&mut app, &["n", "h"]);
        assert_eq!(
            app.selected_row().unwrap().key,
            RowKey::Group("app:google-chrome".into())
        );
        assert!(!app.rows.iter().any(|r| matches!(r.key, RowKey::Member(..))));
    }

    #[test]
    fn pin_mute_rename_undo() {
        let mut app = app_with(fixture());
        select_group(&mut app, "app:google-chrome");
        let eff = press(&mut app, &["p"]);
        assert_eq!(
            eff,
            vec![Effect::Pin {
                fingerprint: "fp-chrome".into(),
                pinned: true
            }]
        );
        assert!(app.chips.iter().any(|c| c.fingerprint == "fp-chrome" && c.pinned));
        let eff = press(&mut app, &["u"]);
        assert_eq!(
            eff,
            vec![Effect::Pin {
                fingerprint: "fp-chrome".into(),
                pinned: false
            }]
        );
        select_group(&mut app, "app:google-chrome");
        let eff = press(&mut app, &["m"]);
        assert!(matches!(
            &eff[..],
            [Effect::Mute {
                until_ms: Some(_),
                ..
            }]
        ));
        assert!(app.muted.contains("fp-chrome"));
        press(&mut app, &["u"]);
        assert!(!app.muted.contains("fp-chrome"));
        select_group(&mut app, "app:google-chrome");
        press(&mut app, &["n", "ctrl-u", "b", "r", "o", "w", "s", "e", "r"]);
        let eff = press(&mut app, &["enter"]);
        assert_eq!(
            eff,
            vec![Effect::Rename {
                fingerprint: "fp-chrome".into(),
                alias: Some("browser".into())
            }]
        );
        assert_eq!(app.label(app.group("app:google-chrome").unwrap()), "browser");
        // renames are usable in queries
        app.set_filter(Some("name:browser".into()));
        assert_eq!(app.rows.len(), 1);
        app.set_filter(None);
        press(&mut app, &["u"]);
        assert_eq!(
            app.label(app.group("app:google-chrome").unwrap()),
            "Google Chrome"
        );
        let eff = press(&mut app, &["-"]);
        assert!(matches!(&eff[..], [Effect::Less { .. }]));
        press(&mut app, &["u", "u"]);
        assert!(app.status.as_deref().unwrap().starts_with("nothing to undo"));
    }

    #[test]
    fn overlays_focus_compare_mode_pin() {
        let mut app = app_with(fixture());
        press(&mut app, &["?"]);
        assert_eq!(app.overlay, Some(Overlay::Help));
        press(&mut app, &["j", "esc"]);
        assert_eq!(app.overlay, None);
        select_group(&mut app, "daemon:gradle");
        press(&mut app, &["i"]);
        assert_eq!(app.overlay, Some(Overlay::WhyRank));
        let why = app.why_lines();
        assert!(why[0].contains("GradleDaemon"), "{why:?}");
        assert!(why.iter().any(|l| l.contains("could free")), "{why:?}");
        press(&mut app, &["i"]);
        press(&mut app, &["w"]);
        assert_eq!(app.focus, Some(RowKey::Group("daemon:gradle".into())));
        assert_eq!(app.key_context(), "details");
        press(&mut app, &["w"]);
        assert!(app.focus.is_none());
        press(&mut app, &["c"]);
        select_group(&mut app, "daemon:kotlin");
        press(&mut app, &["c"]);
        assert_eq!(
            app.compare,
            Some((
                RowKey::Group("daemon:gradle".into()),
                Some(RowKey::Group("daemon:kotlin".into()))
            ))
        );
        press(&mut app, &["esc"]);
        assert!(app.compare.is_none());
        press(&mut app, &["M"]);
        assert!(app.modes.is_pinned());
        press(&mut app, &["M"]);
        assert!(!app.modes.is_pinned());
        assert_eq!(press(&mut app, &[","]), vec![Effect::OpenSettings]);
    }

    #[test]
    fn command_bar_and_palette() {
        let mut app = app_with(fixture());
        press(&mut app, &[":"]);
        for c in "headroom 13G".chars() {
            press(&mut app, &[&c.to_string()]);
        }
        press(&mut app, &["enter"]);
        match &app.overlay {
            Some(Overlay::Answer(_, lines)) => {
                assert!(lines.iter().any(|l| l.starts_with("13.0G: ")), "{lines:?}")
            }
            other => panic!("{other:?}"),
        }
        press(&mut app, &["esc"]);
        press(&mut app, &[":", "w", "h", "y", "enter"]);
        assert_eq!(app.overlay, Some(Overlay::WhySlow));
        press(&mut app, &["esc", ":"]);
        for c in "mode throttle".chars() {
            press(&mut app, &[&c.to_string()]);
        }
        press(&mut app, &["enter"]);
        assert_eq!(app.mode, Mode::Throttle);
        assert!(app.modes.is_pinned());
        press(&mut app, &["ctrl-k"]);
        assert!(matches!(app.input, Some(Input::Palette(_))));
        for c in "gpu hogs".chars() {
            press(&mut app, &[&c.to_string()]);
        }
        assert_eq!(app.palette[0].label, "Rank by GPU memory");
        press(&mut app, &["enter"]);
        assert_eq!(app.home_sort, HomeSort::Footprint);
        press(&mut app, &["ctrl-k", "m", "o", "d", "e", "l", "s", "enter"]);
        assert_eq!(app.view, View::Models);
    }

    #[test]
    fn timeline_scrub() {
        let mut app = app_with(fixture());
        for i in 1..5u64 {
            let mut s = fixture();
            s.taken_at_ms += i * 2000;
            app.update(s, &History::default());
        }
        press(&mut app, &["6"]);
        assert_eq!(app.timeline.len(), 5);
        press(&mut app, &["h", "h"]);
        assert_eq!(app.timeline.scrub, Some(2));
        assert!(matches!(app.rows[0].key, RowKey::Past(_)));
        press(&mut app, &["g"]);
        assert_eq!(app.timeline.scrub, Some(0));
        press(&mut app, &["G"]);
        assert_eq!(app.timeline.scrub, None);
    }

    #[test]
    fn htop_preset_k_stops() {
        let mut app = app_with(fixture());
        select_group(&mut app, "daemon:gradle");
        let km = oomtop_config::keymap::preset("htop").unwrap();
        app.on_key("k", &km);
        assert!(app.confirm.is_some());
        app.on_key("n", &km);
        app.on_key("F9", &km);
        assert!(app.confirm.is_some());
    }

    #[test]
    fn stable_rows_under_cursor() {
        let mut app = app_with(fixture());
        select_group(&mut app, "app:claude");
        let before = app.selected;
        // Claude app suddenly balloons; the selected row must not move.
        for i in 1..4u64 {
            let mut s = fixture();
            s.taken_at_ms += i * 2000;
            for g in s.groups.iter_mut() {
                if g.id == "app:claude" {
                    g.totals.footprint = oomtop_core::Measured::exact(12 << 30, "t");
                }
            }
            app.update(s, &History::default());
            assert_eq!(app.selected, before, "refresh {i}");
            assert_eq!(
                app.selected_row().unwrap().key,
                RowKey::Group("app:claude".into())
            );
        }
    }

    #[test]
    fn mouse_select_expand_double_click() {
        let mut app = app_with(fixture());
        app.regions.borrow_mut().list = Some((Rect::new(0, 10, 100, 10), 0));
        app.on_mouse(MouseInput {
            kind: MouseKind::Down,
            col: 5,
            row: 12,
            at_ms: 1000,
        });
        assert_eq!(app.selected, 2);
        app.on_mouse(MouseInput {
            kind: MouseKind::Down,
            col: 5,
            row: 12,
            at_ms: 1200,
        });
        assert_eq!(app.overlay, Some(Overlay::Details));
        app.overlay = None;
        app.on_mouse(MouseInput {
            kind: MouseKind::ScrollDown,
            col: 5,
            row: 12,
            at_ms: 5000,
        });
        assert_eq!(app.selected, 5);
        app.regions
            .borrow_mut()
            .hits
            .push((Rect::new(0, 30, 10, 1), HitTarget::View(View::Models)));
        app.on_mouse(MouseInput {
            kind: MouseKind::Down,
            col: 3,
            row: 30,
            at_ms: 9000,
        });
        assert_eq!(app.view, View::Models);
    }

    #[test]
    fn your_things_cold_start_and_affinity() {
        let app = app_with(fixture());
        let labels: Vec<&str> = app.chips.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(
            labels,
            ["sd-server · qwen-image-studio", "Claude Code"],
            "cold-start priors"
        );
        assert_eq!(app.chips[1].count, 4);
        let mut app = app_with(fixture());
        let mut f = HashMap::new();
        f.insert("fp-chrome".to_string(), 9.0);
        app.set_profile(
            f,
            &[oomtop_state::EntityRecord {
                fingerprint: "fp-gone".into(),
                display_name: "ollama".into(),
                pinned: true,
                ..Default::default()
            }],
            vec!["kind:daemon".into()],
        );
        let labels: Vec<&str> = app.chips.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels[0], "ollama", "pinned first (not running, dimmed)");
        assert!(!app.chips[0].running);
        assert_eq!(labels[1], "Google Chrome");
    }

    /// "Ignored SIGTERM" needs a sample taken after the signal: x right after y does not offer SIGKILL.
    #[test]
    fn sigkill_is_not_offered_before_a_fresh_sample() {
        let mut app = app_with(fixture());
        select_group(&mut app, "daemon:gradle");
        let eff = press(&mut app, &["x", "y"]);
        let Effect::Execute(plan) = &eff[0] else { panic!() };
        app.on_outcomes(&[ActionOutcome {
            target: plan.targets[0].clone(),
            ok: true,
            message: "sent SIGTERM".into(),
            measured_gain: None,
        }]);
        press(&mut app, &["x"]);
        assert!(app.confirm.is_none(), "no SIGKILL offer on the same sample");
        assert!(
            app.status
                .as_deref()
                .unwrap()
                .contains("waiting for the next sample"),
            "{:?}",
            app.status
        );
        let mut s = fixture();
        s.taken_at_ms += 2_000;
        app.update(s, &History::default());
        select_group(&mut app, "daemon:gradle");
        press(&mut app, &["x"]);
        assert!(app.confirm.as_ref().unwrap().prompt.contains("SIGKILL"));
    }

    /// A click while a confirmation is pending cancels it (nothing sent); scrolling does not move the row away.
    #[test]
    fn mouse_cancels_a_pending_confirmation() {
        let mut app = app_with(fixture());
        select_group(&mut app, "daemon:gradle");
        press(&mut app, &["x"]);
        let sel = app.selected;
        let scroll = MouseInput {
            kind: MouseKind::ScrollDown,
            col: 5,
            row: 20,
            at_ms: 1,
        };
        assert_eq!(app.on_mouse(scroll), None);
        assert_eq!(app.selected, sel, "scroll ignored while confirming");
        assert!(app.confirm.is_some());
        let click = MouseInput {
            kind: MouseKind::Down,
            col: 5,
            row: 20,
            at_ms: 2,
        };
        assert_eq!(app.on_mouse(click), None);
        assert!(app.confirm.is_none());
        assert_eq!(app.status.as_deref(), Some("cancelled — nothing was sent"));
        assert!(press(&mut app, &["y"]).is_empty(), "a later y sends nothing");
    }

    #[test]
    fn suspend_confirmation_names_the_cpu_relief() {
        let mut app = app_with(fixture());
        select_group(&mut app, "model:sd-server");
        press(&mut app, &["z"]);
        let p = app.confirm.clone().unwrap().prompt;
        assert!(p.starts_with("Suspend sd-server"), "{p}");
        assert!(p.contains("% CPU relief"), "{p}");
        assert!(p.contains("frees no memory"), "{p}");
    }

    /// The Reclaim-all row, the view summary and the confirmation use one target list: the calling agent's
    /// session and filtered-out groups are never in it.
    #[test]
    fn reclaim_all_targets_match_the_confirmation() {
        let mut app = app_with(fixture());
        let (all, _) = app.reclaim_all_targets();
        assert_eq!(all.len(), 3);
        let caller = all[0].group_id.clone();
        let mut ctx = fixtures::protect();
        ctx.caller_group = Some(caller.clone());
        app.set_protect(ctx);
        let (t, _) = app.reclaim_all_targets();
        assert_eq!(t.len(), 2);
        assert!(t.iter().all(|x| x.group_id != caller));
        press(&mut app, &["r"]);
        let c = app.confirm.clone().unwrap();
        assert_eq!(c.plan.targets, t, "what is shown is what is sent");
        assert!(c.prompt.starts_with("Stop 2 groups"), "{}", c.prompt);
        press(&mut app, &["n"]);
        app.set_filter(Some("kind:daemon".into()));
        let (t, _) = app.reclaim_all_targets();
        assert!(t.iter().all(|x| x.group_id.starts_with("daemon:")), "{t:?}");
    }

    #[test]
    fn key_hints_follow_the_keymap() {
        let mut app = app_with(fixture());
        assert_eq!(
            app.key_for("palette").as_deref(),
            Some("^K"),
            "default hints without a keymap"
        );
        let mut km = oomtop_config::keymap::preset("htop").unwrap();
        app.set_keymap(&km);
        assert_eq!(app.key_for("stop").as_deref(), Some("x"));
        assert_eq!(app.key_for("help").as_deref(), Some("?"));
        // Unbinding x leaves the htop keys; the hint follows.
        for binds in km.contexts.values_mut() {
            binds.retain(|(k, _)| k != "x");
        }
        app.set_keymap(&km);
        assert_eq!(app.key_for("stop").as_deref(), Some("k"));
        assert!(app
            .help_lines_cache
            .iter()
            .any(|(k, d)| k.contains("F9") && d.starts_with("stop")));
        assert_eq!(display_key("ctrl-k"), "^K");
        assert_eq!(display_key("F9"), "F9");
    }

    /// Filter aliases expand recursively (config `views::expand`); a cycle is an error, never a silent filter.
    #[test]
    fn aliases_expand_recursively_and_cycles_are_errors() {
        let mut app = app_with(fixture());
        app.aliases.insert("daemons".into(), "kind:daemon".into());
        app.aliases.insert("bigdaemons".into(), "daemons mem>2G".into());
        app.set_filter(Some("bigdaemons".into()));
        assert_eq!(app.filter.as_deref(), Some("bigdaemons"));
        let ids: Vec<String> = app.ranked.iter().map(|r| r.id.clone()).collect();
        assert!(
            !ids.is_empty() && ids.iter().all(|i| i.starts_with("daemon:")),
            "{ids:?}"
        );
        app.set_filter(None);
        app.aliases.insert("a".into(), "b".into());
        app.aliases.insert("b".into(), "a".into());
        press(&mut app, &["/", "a", "enter"]);
        assert!(app.filter.is_none());
        assert!(
            app.status.as_deref().unwrap().contains("cycle"),
            "{:?}",
            app.status
        );
    }

    #[test]
    fn layout_commands_and_layout_sort() {
        let mut app = app_with(fixture());
        assert_eq!(
            app.run_command("layout llm-dev"),
            Some(Effect::Layout("llm-dev".into()))
        );
        app.views.push(oomtop_config::model::SavedView {
            name: "gpu-work".into(),
            query: "kind:model".into(),
            layout: "llm-dev".into(),
        });
        assert_eq!(
            app.run_command("view gpu-work"),
            Some(Effect::Layout("llm-dev".into()))
        );
        assert_eq!(app.filter.as_deref(), Some("kind:model"));
        app.set_filter(None);
        app.set_layout(Some(crate::columns::tests::llm_dev()));
        assert_eq!(app.home_sort, HomeSort::Footprint, "layout sort = footprint");
        let fps: Vec<u64> = app
            .ranked
            .iter()
            .filter_map(|r| app.group(&r.id).and_then(|g| g.totals.footprint.value))
            .collect();
        assert!(fps.windows(2).all(|w| w[0] >= w[1]), "{fps:?}");
        app.set_layout(None);
        assert!(app.layout.is_none());
    }

    #[test]
    fn sort_by_gpu_idle_and_reclaim() {
        let mut app = app_with(fixture());
        for (what, expect) in [
            ("reclaim", HomeSort::Reclaim),
            ("idle", HomeSort::Idle),
            ("gpu", HomeSort::Gpu),
        ] {
            app.run_command(&format!("sort {what}"));
            assert_eq!(app.home_sort, expect);
        }
        app.run_command("sort reclaim");
        let gains: Vec<u64> = app
            .ranked
            .iter()
            .map(|r| app.group(&r.id).and_then(|g| g.reclaim_gain.value).unwrap_or(0))
            .collect();
        assert!(gains.windows(2).all(|w| w[0] >= w[1]), "{gains:?}");
        app.run_command("sort idle");
        let idle: Vec<u64> = app
            .ranked
            .iter()
            .map(|r| app.group(&r.id).and_then(|g| g.idle_for_s).unwrap_or(0))
            .collect();
        assert!(idle.windows(2).all(|w| w[0] >= w[1]), "{idle:?}");
        assert_eq!(HomeSort::parse("MEM"), Some(HomeSort::Footprint));
        assert_eq!(HomeSort::parse("nope"), None);
    }

    #[test]
    fn save_rejects_bad_alias_names() {
        let mut app = app_with(fixture());
        app.set_filter(Some("kind:daemon".into()));
        for bad in ["kind", "or", "a.b", "reclaim", "x/y"] {
            assert_eq!(app.run_command(&format!("save {bad}")), None, "{bad}");
            assert!(app.status.as_deref().unwrap().starts_with("can't save"), "{bad}");
            assert!(!app.aliases.contains_key(bad));
        }
        assert_eq!(
            app.run_command("save my-daemons"),
            Some(Effect::SaveAlias {
                name: "my-daemons".into(),
                query: "kind:daemon".into()
            })
        );
    }
}

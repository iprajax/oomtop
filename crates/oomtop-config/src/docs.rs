//! One doc line per setting. Used by `oomtop config init` and checked by a test so that every setting is
//! documented (CLAUDE.md engineering rule).

pub const DOCS: &[(&str, &str)] = &[
    ("general.refresh_ms", "per-process refresh in ms (TUI, serve)"),
    ("general.host_refresh_ms", "host stats refresh in ms"),
    ("general.expensive_refresh_ms", "expensive reads (smaps rotation, adapters) in ms"),
    ("general.mouse", "enable mouse support in the TUI"),
    ("general.live_reload", "reload config, themes, keymap and layouts on save (debounced 200 ms)"),
    ("general.watch_poll_ms", "0 = native file events; > 0 = poll every N ms (network filesystems)"),
    ("general.other_users", "show other users' processes (false = only yours; host totals stay whole-machine)"),
    ("thresholds.idle_cpu_pct", "per-core CPU % below which a process counts as inactive"),
    ("thresholds.idle_after_s", "seconds without CPU/disk activity before a process is idle"),
    ("thresholds.orphan_age_s", "minimum age in seconds before a leftover counts as an orphan"),
    ("headroom.min_margin", "minimum safety margin (e.g. \"1.5G\")"),
    ("headroom.margin_pct", "safety margin as % of RAM (the larger of this and min_margin wins)"),
    ("headroom.pressure_boost_pct", "extra margin % when under pressure or swap is growing"),
    ("headroom.margin_override", "fixed safety margin (e.g. \"2G\"); empty = computed"),
    ("protected.names", "extra process names never offered for stop/suspend"),
    ("models.folders", "folders indexed for model files"),
    ("adapters.enabled", "probe local model-server APIs (127.0.0.1 only)"),
    ("adapters.timeout_ms", "adapter HTTP timeout in ms"),
    ("adapters.ports", "port overrides per server kind, e.g. { ollama = 11434 }"),
    ("privacy.marker_allowlist", "environment keys read as session markers (values hashed, never stored)"),
    ("privacy.redact_exports", "redact command lines in json/ndjson output (serve and MCP always redact)"),
    ("appearance.theme", "terminal (default) | mono | ember | mint | sand | coral | high-contrast | colorblind | none | <user theme>"),
    ("appearance.appearance", "auto | dark | light"),
    ("appearance.color", "auto | truecolor | 256 | 16 | none"),
    ("appearance.background", "transparent (never paint) | theme (paint the theme surface)"),
    ("appearance.glyphs", "unicode | nerd | ascii"),
    ("appearance.borders", "rounded | plain | thick | double | none"),
    ("appearance.density", "comfortable | compact"),
    ("appearance.motion", "marks (one-refresh change markers) | none"),
    ("appearance.sparklines", "braille | blocks | none"),
    ("appearance.header", "header rows in display order: meters (htop-style per-core CPU, Mem, Swp, tasks/load/uptime) · headline · memory · swap · accelerators · thermal"),
    ("format.memory_units", "iec (GiB) | si (GB)"),
    ("format.decimals", "decimals for memory values"),
    ("format.cpu", "per-core (100% = 1 core) | total (100% = machine)"),
    ("format.time", "relative (\"idle 5h\") | clock"),
    ("keys.preset", "default | vim | emacs | htop"),
    ("layout.name", "layout from layouts/<name>.toml; empty = built-in adaptive layout"),
    ("personalization.learn", "learn from what you open, pin and search (local only)"),
    ("personalization.half_life_days", "frecency half-life in days"),
    ("personalization.retention_days", "days the local impression/selection log is kept"),
    ("personalization.log_max_mb", "size cap of the local impression/selection log in MB"),
    ("personalization.weights.salience", "ranking weight: share of memory/GPU/CPU"),
    ("personalization.weights.affinity", "ranking weight: your frecency for the entity"),
    ("personalization.weights.actionability", "ranking weight: reclaimable bytes / fixable cause"),
    ("personalization.weights.query", "ranking weight: active query match"),
    ("personalization.weights.noise", "ranking weight: muted / system noise penalty"),
    ("serve.listen", "HTTP listen address for `oomtop serve` (127.0.0.1 unless changed)"),
    ("mcp.allow_actions", "register the elicitation-gated MCP reclaim tool"),
    ("aliases", "query aliases, e.g. llm = \"kind:model,agent\""),
    ("views", "saved views: [[views]] name/query/layout"),
];

/// One line per section, printed above the section in `oomtop config init`.
pub const SECTION_DOCS: &[(&str, &str)] = &[
    ("general", "Sampling cadence and general behavior"),
    (
        "thresholds",
        "When a process counts as idle or orphaned (SPEC §7)",
    ),
    (
        "headroom",
        "Safety margin for headroom and can_fit answers (SPEC §8.1)",
    ),
    (
        "protected",
        "Processes that are never offered for stop/suspend (SPEC §13)",
    ),
    ("models", "Model files on disk (SPEC §10)"),
    (
        "adapters",
        "Local model-server / sandbox APIs, 127.0.0.1 only, never a port scan (SPEC §10)",
    ),
    ("adapters.ports", "Port overrides per server kind"),
    (
        "privacy",
        "What is read from process environments and what is exported (SPEC §13)",
    ),
    (
        "appearance",
        "Theme and look (UX §12.5); the default theme inherits your terminal's colors",
    ),
    ("format", "Number and time formatting (UX §12.5)"),
    (
        "keys",
        "Key preset; remap individual keys in keymap.toml (UX §12.7)",
    ),
    (
        "layout",
        "Panel/column layout from layouts/<name>.toml (UX §12.6)",
    ),
    (
        "personalization",
        "Local learning (UX §5); nothing leaves this machine",
    ),
    (
        "personalization.weights",
        "Ranking weights (UX §5.4); fixed defaults keep rankings explainable",
    ),
    ("serve", "`oomtop serve` HTTP endpoint (SPEC §12.2)"),
    ("mcp", "`oomtop mcp` server for agents (SPEC §12.3)"),
    (
        "aliases",
        "Query aliases usable in `/`, `:` and saved views (UX §12.7)",
    ),
    (
        "views",
        "Saved views: named queries with an optional layout (UX §12.7)",
    ),
];

/// Commented examples for settings whose default is empty.
pub const EXAMPLES: &[(&str, &[&str])] = &[
    ("adapters.ports", &["ollama = 11434", "llama_cpp = 8080"]),
    (
        "aliases",
        &["hogs = \":sort footprint desc\"", "llm  = \"kind:model,agent\""],
    ),
    (
        "views",
        &[
            "[[views]]",
            "name   = \"gpu-work\"",
            "query  = \"gpu>500M or kind:model\"",
            "layout = \"llm-dev\"",
        ],
    ),
    ("protected", &["names = [\"postgres\", \"Xcode\"]"]),
];

pub fn doc_for(key: &str) -> Option<&'static str> {
    DOCS.iter().find(|(k, _)| *k == key).map(|(_, d)| *d)
}

pub fn section_doc(section: &str) -> Option<&'static str> {
    SECTION_DOCS.iter().find(|(k, _)| *k == section).map(|(_, d)| *d)
}

pub fn examples_for(key: &str) -> &'static [&'static str] {
    EXAMPLES
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, e)| *e)
        .unwrap_or(&[])
}

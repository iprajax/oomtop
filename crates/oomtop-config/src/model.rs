//! The `Config` struct (UX §12.5, SPEC §15). Every field has a default and a doc line in
//! [`crate::docs::DOCS`] (enforced by a test), and appears in the JSON Schema and `oomtop config init`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// TUI/serve refresh for per-process data, ms.
    pub refresh_ms: u64,
    /// Host stats refresh, ms.
    pub host_refresh_ms: u64,
    /// Expensive reads (smaps rotation, adapters), ms.
    pub expensive_refresh_ms: u64,
    pub mouse: bool,
    /// Reload config/themes/keymap/layouts on save (debounced 200 ms).
    pub live_reload: bool,
    /// 0 = native file events (FSEvents/inotify); > 0 = poll every N ms (network filesystems).
    pub watch_poll_ms: u64,
    /// Show other users' processes (like htop). `false` keeps only this user's processes; host totals stay
    /// whole-machine.
    pub other_users: bool,
}

impl Default for General {
    fn default() -> Self {
        General {
            refresh_ms: 2000,
            host_refresh_ms: 1000,
            expensive_refresh_ms: 10_000,
            mouse: true,
            live_reload: true,
            watch_poll_ms: 0,
            other_users: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Thresholds {
    pub idle_cpu_pct: f64,
    pub idle_after_s: u64,
    pub orphan_age_s: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            idle_cpu_pct: 0.5,
            idle_after_s: 1800,
            orphan_age_s: 600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct HeadroomSection {
    /// Minimum safety margin, e.g. "1.5G".
    pub min_margin: String,
    pub margin_pct: f64,
    pub pressure_boost_pct: f64,
    /// Fixed margin override, e.g. "2G"; empty = computed.
    pub margin_override: String,
}

impl Default for HeadroomSection {
    fn default() -> Self {
        HeadroomSection {
            min_margin: "1.5G".into(),
            margin_pct: 8.0,
            pressure_boost_pct: 50.0,
            margin_override: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Protected {
    /// Extra process names never offered for actions.
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Models {
    /// Folders indexed for model files.
    pub folders: Vec<String>,
}

impl Default for Models {
    fn default() -> Self {
        Models {
            folders: vec![
                "~/.cache/huggingface".into(),
                "~/.ollama/models".into(),
                "~/.lmstudio/models".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Adapters {
    pub enabled: bool,
    pub timeout_ms: u64,
    /// Port overrides per server kind (host is always 127.0.0.1), e.g. ollama = 11434.
    pub ports: BTreeMap<String, u16>,
}

impl Default for Adapters {
    fn default() -> Self {
        Adapters {
            enabled: true,
            timeout_ms: 200,
            ports: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Privacy {
    /// Environment keys read as session markers (values hashed, never stored).
    pub marker_allowlist: Vec<String>,
    /// Redact command lines in exports (json, ndjson, serve, MCP).
    pub redact_exports: bool,
}

impl Default for Privacy {
    fn default() -> Self {
        Privacy {
            marker_allowlist: oomtop_core::redact::default_allowlist(),
            redact_exports: true,
        }
    }
}

macro_rules! str_enum {
    ($name:ident { $($(#[$vm:meta])* $var:ident = $s:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
        pub enum $name {
            $( $(#[$vm])* #[serde(rename = $s)] $var, )+
        }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $( $name::$var => $s, )+ }
            }
        }
    };
}

str_enum!(AppearanceMode { #[default] Auto = "auto", Dark = "dark", Light = "light" });
str_enum!(ColorMode { #[default] Auto = "auto", Truecolor = "truecolor", C256 = "256", C16 = "16", None = "none" });
str_enum!(Background { #[default] Transparent = "transparent", Theme = "theme" });
str_enum!(Glyphs { #[default] Unicode = "unicode", Nerd = "nerd", Ascii = "ascii" });
str_enum!(Borders { #[default] Rounded = "rounded", Plain = "plain", Thick = "thick", Double = "double", None = "none" });
str_enum!(Density { #[default] Comfortable = "comfortable", Compact = "compact" });
str_enum!(Motion { #[default] Marks = "marks", None = "none" });
str_enum!(Sparklines { #[default] Braille = "braille", Blocks = "blocks", None = "none" });
str_enum!(MemoryUnits { #[default] Iec = "iec", Si = "si" });
str_enum!(CpuFormat { #[default] PerCore = "per-core", Total = "total" });
str_enum!(TimeFormat { #[default] Relative = "relative", Clock = "clock" });

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Appearance {
    pub theme: String,
    pub appearance: AppearanceMode,
    pub color: ColorMode,
    pub background: Background,
    pub glyphs: Glyphs,
    pub borders: Borders,
    pub density: Density,
    pub motion: Motion,
    pub sparklines: Sparklines,
    /// Header rows in display order.
    pub header: Vec<String>,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            theme: "terminal".into(),
            appearance: AppearanceMode::Auto,
            color: ColorMode::Auto,
            background: Background::Transparent,
            glyphs: Glyphs::Unicode,
            borders: Borders::Rounded,
            density: Density::Comfortable,
            motion: Motion::Marks,
            sparklines: Sparklines::Braille,
            header: ["meters", "headline", "memory", "swap", "accelerators", "thermal"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Format {
    pub memory_units: MemoryUnits,
    pub decimals: u8,
    pub cpu: CpuFormat,
    pub time: TimeFormat,
}

impl Default for Format {
    fn default() -> Self {
        Format {
            memory_units: MemoryUnits::Iec,
            decimals: 1,
            cpu: CpuFormat::PerCore,
            time: TimeFormat::Relative,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Keys {
    /// default | vim | emacs | htop
    pub preset: String,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            preset: "default".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutSel {
    /// Layout name from layouts/<name>.toml; empty = built-in adaptive layout.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Weights {
    pub salience: f64,
    pub affinity: f64,
    pub actionability: f64,
    pub query: f64,
    pub noise: f64,
}

impl Default for Weights {
    fn default() -> Self {
        let w = oomtop_core::ranking::RankWeights::default();
        Weights {
            salience: w.salience,
            affinity: w.affinity,
            actionability: w.actionability,
            query: w.query,
            noise: w.noise,
        }
    }
}

impl Weights {
    pub fn to_core(&self) -> oomtop_core::ranking::RankWeights {
        oomtop_core::ranking::RankWeights {
            salience: self.salience,
            affinity: self.affinity,
            actionability: self.actionability,
            query: self.query,
            noise: self.noise,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Personalization {
    pub learn: bool,
    pub half_life_days: f64,
    /// Impression/selection log retention in days (UX §5.6).
    pub retention_days: u32,
    /// Impression/selection log size cap in MB (UX §5.6).
    pub log_max_mb: u32,
    pub weights: Weights,
}

impl Default for Personalization {
    fn default() -> Self {
        Personalization {
            learn: true,
            half_life_days: 7.0,
            retention_days: 30,
            log_max_mb: 5,
            weights: Weights::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Serve {
    /// Listen address; 127.0.0.1 unless explicitly changed (SPEC §13).
    pub listen: String,
}

impl Default for Serve {
    fn default() -> Self {
        Serve {
            listen: "127.0.0.1:9469".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Mcp {
    /// Register the elicitation-gated `reclaim` tool.
    pub allow_actions: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SavedView {
    pub name: String,
    pub query: String,
    pub layout: String,
}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub thresholds: Thresholds,
    pub headroom: HeadroomSection,
    pub protected: Protected,
    pub models: Models,
    pub adapters: Adapters,
    pub privacy: Privacy,
    pub appearance: Appearance,
    pub format: Format,
    pub keys: Keys,
    pub layout: LayoutSel,
    pub personalization: Personalization,
    pub serve: Serve,
    pub mcp: Mcp,
    /// Query aliases (UX §12.7), e.g. llm = "kind:model,agent".
    pub aliases: BTreeMap<String, String>,
    /// Saved views (UX §12.7).
    pub views: Vec<SavedView>,
}

impl Config {
    /// Core headroom tunables.
    pub fn headroom_config(&self) -> oomtop_core::headroom::HeadroomConfig {
        let d = oomtop_core::headroom::HeadroomConfig::default();
        oomtop_core::headroom::HeadroomConfig {
            min_margin: oomtop_core::units::parse_bytes(&self.headroom.min_margin).unwrap_or(d.min_margin),
            margin_pct: self.headroom.margin_pct,
            pressure_boost_pct: self.headroom.pressure_boost_pct,
            margin_override: if self.headroom.margin_override.trim().is_empty() {
                None
            } else {
                oomtop_core::units::parse_bytes(&self.headroom.margin_override).ok()
            },
        }
    }

    /// Core idle/orphan tunables.
    pub fn idle_config(&self) -> oomtop_core::idle::IdleConfig {
        oomtop_core::idle::IdleConfig {
            cpu_pct_threshold: self.thresholds.idle_cpu_pct,
            idle_after_s: self.thresholds.idle_after_s,
            orphan_age_s: self.thresholds.orphan_age_s,
        }
    }
}

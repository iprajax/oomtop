//! In-app settings (UX §12.8): a **view over the config files**, not a separate store. Every row shows the
//! effective value, its origin (`default`, `config.toml:12`, `host-air.toml:3`, `env`, `flag`) and where else
//! it is set. Changes preview live (the event loop re-renders with them), `Esc` reverts, `Enter` applies for
//! the session, `s` saves to the chosen layer with comment-preserving edits (`oomtop_config::write`), `e`
//! opens the file at the right line in `$EDITOR`.
//!
//! This module is pure: the event loop builds a [`SettingsModel`] from `Loaded::effective()` and performs the
//! writes; the state machine here only decides *what* to preview/save.

use std::path::PathBuf;

/// Sections in display order (UX §12.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    Appearance,
    Layout,
    Columns,
    Keys,
    Behavior,
    Personalization,
    Privacy,
}

impl Section {
    pub const ALL: [Section; 7] = [
        Section::Appearance,
        Section::Layout,
        Section::Columns,
        Section::Keys,
        Section::Behavior,
        Section::Personalization,
        Section::Privacy,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::Layout => "Layout",
            Section::Columns => "Columns",
            Section::Keys => "Keys",
            Section::Behavior => "Behavior",
            Section::Personalization => "Personalization",
            Section::Privacy => "Privacy",
        }
    }

    /// Section for a dotted key; `None` = not editable here (tables such as aliases/views).
    pub fn of(key: &str) -> Option<Section> {
        let top = key.split('.').next().unwrap_or("");
        Some(match top {
            "appearance" | "format" => Section::Appearance,
            "layout" => Section::Layout,
            "keys" => Section::Keys,
            "general" | "thresholds" | "headroom" | "protected" | "models" | "adapters" => Section::Behavior,
            "personalization" => Section::Personalization,
            "privacy" | "serve" | "mcp" => Section::Privacy,
            _ => return None,
        })
    }
}

/// Where `s` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SaveLayer {
    /// `~/.config/oomtop/config.toml`
    User,
    /// `config.d/host-<hostname>.toml`
    Host,
    /// `config.d/50-settings.toml`
    Dropin,
}

impl SaveLayer {
    pub fn next(self) -> SaveLayer {
        match self {
            SaveLayer::User => SaveLayer::Host,
            SaveLayer::Host => SaveLayer::Dropin,
            SaveLayer::Dropin => SaveLayer::User,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SaveLayer::User => "config.toml",
            SaveLayer::Host => "host file",
            SaveLayer::Dropin => "drop-in 50-settings.toml",
        }
    }
}

/// File name of the drop-in the settings screen creates.
pub const DROPIN_NAME: &str = "50-settings.toml";

/// One editable setting.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingRow {
    pub key: String,
    pub section: Section,
    /// Display value (strings unquoted).
    pub value: String,
    /// TOML literal of the value (strings quoted).
    pub raw: String,
    /// Value is a TOML string (edits get quoted).
    pub quoted: bool,
    /// Origin label ("default", "config.toml:12", "env OOMTOP_…", "flag", "runtime").
    pub origin: String,
    /// Other files that also set this key ("config.toml:4") — i.e. it is overridden elsewhere.
    pub also_in: Vec<String>,
    pub doc: String,
    /// Allowed values for cycling (empty = free text).
    pub choices: Vec<String>,
}

/// Theme picker entry: name + contrast badge ("ok", "2 issues").
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeChoice {
    pub name: String,
    pub badge: String,
}

/// What the loop hands the settings screen.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SettingsModel {
    pub rows: Vec<SettingRow>,
    pub themes: Vec<ThemeChoice>,
    /// Paths for each save layer (shown in the footer).
    pub layer_paths: Vec<(SaveLayer, PathBuf)>,
}

/// Allowed values for enumerated settings.
pub fn choices_for(key: &str, themes: &[ThemeChoice]) -> Vec<String> {
    let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect();
    match key {
        "appearance.theme" => themes.iter().map(|t| t.name.clone()).collect(),
        "appearance.appearance" => v(&["auto", "dark", "light"]),
        "appearance.color" => v(&["auto", "truecolor", "256", "16", "none"]),
        "appearance.background" => v(&["transparent", "theme"]),
        "appearance.glyphs" => v(&["unicode", "nerd", "ascii"]),
        "appearance.borders" => v(&["rounded", "plain", "thick", "double", "none"]),
        "appearance.density" => v(&["comfortable", "compact"]),
        "appearance.motion" => v(&["marks", "none"]),
        "appearance.sparklines" => v(&["braille", "blocks", "none"]),
        "format.memory_units" => v(&["iec", "si"]),
        "format.cpu" => v(&["per-core", "total"]),
        "format.time" => v(&["relative", "clock"]),
        "keys.preset" => v(oomtop_config::keymap::PRESETS),
        _ => Vec::new(),
    }
}

impl SettingsModel {
    /// Builds rows from `(key, toml literal, origin label, also-set-in)` tuples, as produced by the loop from
    /// `Loaded::effective()`. Keys without a section (aliases, views) are left to the files.
    pub fn from_entries(
        entries: Vec<(String, String, String, Vec<String>)>,
        themes: Vec<ThemeChoice>,
    ) -> Self {
        let mut rows: Vec<SettingRow> = entries
            .into_iter()
            .filter_map(|(key, raw, origin, also_in)| {
                let section = Section::of(&key)?;
                let quoted = raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2;
                let value = if quoted {
                    raw[1..raw.len() - 1].to_string()
                } else {
                    raw.clone()
                };
                let mut choices = choices_for(&key, &themes);
                if choices.is_empty() && (raw == "true" || raw == "false") {
                    choices = vec!["true".into(), "false".into()];
                }
                Some(SettingRow {
                    doc: oomtop_config::docs::doc_for(&key).unwrap_or("").to_string(),
                    key,
                    section,
                    value,
                    raw,
                    quoted,
                    origin,
                    also_in,
                    choices,
                })
            })
            .collect();
        rows.sort_by(|a, b| a.section.cmp(&b.section).then(a.key.cmp(&b.key)));
        SettingsModel {
            rows,
            themes,
            layer_paths: Vec::new(),
        }
    }

    /// Offers `layout.name` as a choice between the built-in adaptive layout and `layouts/*.toml`.
    pub fn set_layouts(&mut self, names: Vec<String>) {
        for r in self.rows.iter_mut().filter(|r| r.key == "layout.name") {
            let mut choices = names.clone();
            // The config default is "" (= adaptive); keep the current value cyclable.
            if !choices.contains(&r.value) {
                choices.insert(0, r.value.clone());
            }
            r.choices = choices;
        }
    }
}

/// What a settings keypress asks the loop to do.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsCmd {
    /// Live preview one change (key, TOML literal).
    Preview(String, String),
    /// Revert every previewed change and close.
    Revert,
    /// Keep previewed changes for this session and close.
    Apply,
    /// Save pending changes to the layer.
    Save(SaveLayer, Vec<(String, String)>),
    /// Open the file for the layer (or the selected row's origin file) at the key's line.
    Edit(SaveLayer, String),
}

/// Settings screen state.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsState {
    pub model: SettingsModel,
    pub selected: usize,
    /// Changes previewed this session, in order (key, TOML literal); later wins.
    pub pending: Vec<(String, String)>,
    pub layer: SaveLayer,
    /// Free-text edit buffer for the selected row.
    pub editing: Option<String>,
    pub message: Option<String>,
    /// The row as it was before the last change, so a preview the loop rejects can be rolled back.
    before_last: Option<(usize, SettingRow)>,
    pending_before: Option<(String, String)>,
}

impl SettingsState {
    pub fn new(model: SettingsModel) -> Self {
        SettingsState {
            model,
            selected: 0,
            pending: Vec::new(),
            layer: SaveLayer::User,
            editing: None,
            message: None,
            before_last: None,
            pending_before: None,
        }
    }

    /// Rolls back the last change when the loop could not apply it (an invalid value): the row shows its
    /// previous value and origin again, the change is not saved, and the message says why.
    pub fn reject(&mut self, key: &str, why: &str) {
        if let Some((idx, row)) = self.before_last.take().filter(|(_, r)| r.key == key) {
            if let Some(slot) = self.model.rows.get_mut(idx) {
                *slot = row;
            }
        }
        self.pending.retain(|(k, _)| k != key);
        if let Some(p) = self.pending_before.take().filter(|(k, _)| k == key) {
            self.pending.push(p);
        }
        self.message = Some(format!("{key}: {why} — not applied"));
    }

    pub fn row(&self) -> Option<&SettingRow> {
        self.model.rows.get(self.selected)
    }

    fn literal(row: &SettingRow, v: &str) -> String {
        if row.quoted {
            format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
        } else {
            v.to_string()
        }
    }

    fn set(&mut self, idx: usize, value: String) -> SettingsCmd {
        let before = self.model.rows[idx].clone();
        let was_pending = self.pending.iter().find(|(k, _)| *k == before.key).cloned();
        self.before_last = Some((idx, before));
        self.pending_before = was_pending;
        let row = &mut self.model.rows[idx];
        let lit = Self::literal(row, &value);
        row.value = value;
        row.raw = lit.clone();
        row.origin = "runtime (unsaved)".into();
        let key = row.key.clone();
        self.pending.retain(|(k, _)| *k != key);
        self.pending.push((key.clone(), lit.clone()));
        SettingsCmd::Preview(key, lit)
    }

    /// Cycles an enumerated value (`dir` = +1 / -1).
    pub fn cycle(&mut self, dir: isize) -> Option<SettingsCmd> {
        let idx = self.selected;
        let row = self.model.rows.get(idx)?;
        if row.choices.is_empty() {
            self.message = Some(format!("{}: press enter to edit the value", row.key));
            return None;
        }
        let n = row.choices.len() as isize;
        let cur = row
            .choices
            .iter()
            .position(|c| *c == row.value)
            .map(|i| i as isize)
            .unwrap_or(-1);
        let next = ((cur + dir).rem_euclid(n)) as usize;
        let v = row.choices[next].clone();
        Some(self.set(idx, v))
    }

    pub fn move_by(&mut self, delta: isize) {
        let n = self.model.rows.len();
        if n == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, n as isize - 1) as usize;
        self.message = None;
    }

    /// Handles a key; returns a command for the loop.
    pub fn on_key(&mut self, key: &str) -> Option<SettingsCmd> {
        if let Some(buf) = self.editing.as_mut() {
            match key {
                "esc" => self.editing = None,
                "enter" => {
                    let v = self.editing.take().unwrap_or_default();
                    let idx = self.selected;
                    if idx < self.model.rows.len() {
                        return Some(self.set(idx, v));
                    }
                }
                "backspace" => {
                    buf.pop();
                }
                "ctrl-u" => buf.clear(),
                "space" => buf.push(' '),
                k if k.chars().count() == 1 => buf.push_str(k),
                _ => {}
            }
            return None;
        }
        match key {
            "esc" | "q" => Some(SettingsCmd::Revert),
            "up" | "k" => {
                self.move_by(-1);
                None
            }
            "down" | "j" => {
                self.move_by(1);
                None
            }
            "pageup" => {
                self.move_by(-10);
                None
            }
            "pagedown" => {
                self.move_by(10);
                None
            }
            "g" | "home" => {
                self.selected = 0;
                None
            }
            "G" | "end" => {
                self.selected = self.model.rows.len().saturating_sub(1);
                None
            }
            "right" | "l" | " " | "space" => self.cycle(1),
            "left" | "h" => self.cycle(-1),
            "enter" => {
                let row = self.row()?;
                if row.choices.is_empty() {
                    self.editing = Some(row.value.clone());
                    None
                } else {
                    Some(SettingsCmd::Apply)
                }
            }
            "a" => Some(SettingsCmd::Apply),
            "tab" => {
                self.layer = self.layer.next();
                self.message = Some(format!("save target: {}", self.layer.label()));
                None
            }
            "s" => {
                if self.pending.is_empty() {
                    self.message = Some("nothing to save".into());
                    None
                } else {
                    Some(SettingsCmd::Save(self.layer, self.pending.clone()))
                }
            }
            "e" => {
                let key = self.row()?.key.clone();
                Some(SettingsCmd::Edit(self.layer, key))
            }
            "t" => {
                // Jump to the theme picker row.
                if let Some(i) = self.model.rows.iter().position(|r| r.key == "appearance.theme") {
                    self.selected = i;
                }
                None
            }
            _ => None,
        }
    }

    /// Marks pending changes as saved to `origin` (called by the loop after a successful write).
    pub fn saved(&mut self, origin: &str) {
        let keys: Vec<String> = self.pending.drain(..).map(|(k, _)| k).collect();
        for r in &mut self.model.rows {
            if keys.contains(&r.key) {
                r.origin = origin.to_string();
            }
        }
        self.message = Some(format!("saved {} setting(s) to {origin}", keys.len()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> SettingsModel {
        SettingsModel::from_entries(
            vec![
                (
                    "appearance.theme".into(),
                    "\"terminal\"".into(),
                    "default".into(),
                    vec![],
                ),
                (
                    "appearance.density".into(),
                    "\"comfortable\"".into(),
                    "config.toml:4".into(),
                    vec![],
                ),
                (
                    "general.refresh_ms".into(),
                    "2000".into(),
                    "host-air.toml:2".into(),
                    vec!["config.toml:7".into()],
                ),
                ("general.mouse".into(), "true".into(), "default".into(), vec![]),
                (
                    "aliases.llm".into(),
                    "\"kind:model\"".into(),
                    "config.toml:9".into(),
                    vec![],
                ),
            ],
            vec![
                ThemeChoice {
                    name: "none".into(),
                    badge: "ok".into(),
                },
                ThemeChoice {
                    name: "terminal".into(),
                    badge: "ok".into(),
                },
            ],
        )
    }

    #[test]
    fn rows_sections_and_choices() {
        let m = model();
        let keys: Vec<&str> = m.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "appearance.density",
                "appearance.theme",
                "general.mouse",
                "general.refresh_ms"
            ],
            "aliases are left to the files; sorted by section then key"
        );
        assert_eq!(m.rows[1].choices, ["none", "terminal"]);
        assert_eq!(m.rows[2].choices, ["true", "false"]);
        assert!(m.rows[3].choices.is_empty());
        assert_eq!(m.rows[3].also_in, ["config.toml:7"]);
        assert_eq!(m.rows[1].value, "terminal");
        assert!(!m.rows[1].doc.is_empty());
    }

    #[test]
    fn preview_edit_save_revert() {
        let mut s = SettingsState::new(model());
        s.selected = 1;
        assert_eq!(
            s.on_key("right"),
            Some(SettingsCmd::Preview("appearance.theme".into(), "\"none\"".into()))
        );
        assert_eq!(s.row().unwrap().origin, "runtime (unsaved)");
        s.on_key("down");
        s.on_key("down");
        assert_eq!(s.on_key("enter"), None, "free text → edit mode");
        for _ in 0..4 {
            s.on_key("backspace");
        }
        for c in ["5", "0", "0", "0"] {
            s.on_key(c);
        }
        assert_eq!(
            s.on_key("enter"),
            Some(SettingsCmd::Preview("general.refresh_ms".into(), "5000".into()))
        );
        s.on_key("tab");
        assert_eq!(s.layer, SaveLayer::Host);
        match s.on_key("s") {
            Some(SettingsCmd::Save(SaveLayer::Host, changes)) => assert_eq!(
                changes,
                vec![
                    ("appearance.theme".to_string(), "\"none\"".to_string()),
                    ("general.refresh_ms".to_string(), "5000".to_string())
                ]
            ),
            other => panic!("{other:?}"),
        }
        s.saved("host-air.toml");
        assert!(s.pending.is_empty());
        assert_eq!(s.on_key("s"), None);
        assert_eq!(s.on_key("esc"), Some(SettingsCmd::Revert));
    }

    /// A value the loop cannot apply is rolled back: not shown as the effective value, not saved.
    #[test]
    fn rejected_preview_is_rolled_back() {
        let mut s = SettingsState::new(model());
        s.selected = 3; // general.refresh_ms (free text)
        s.on_key("enter");
        s.on_key("ctrl-u");
        for c in ["f", "a", "s", "t"] {
            s.on_key(c);
        }
        let Some(SettingsCmd::Preview(k, v)) = s.on_key("enter") else {
            panic!("preview expected")
        };
        assert_eq!((k.as_str(), v.as_str()), ("general.refresh_ms", "fast"));
        s.reject("general.refresh_ms", "invalid type: string, expected u64");
        let r = s.row().unwrap();
        assert_eq!(r.value, "2000");
        assert_eq!(r.origin, "host-air.toml:2");
        assert!(s.pending.is_empty(), "nothing to save");
        assert!(s.message.as_deref().unwrap().contains("not applied"));
        assert_eq!(s.on_key("s"), None);
    }

    #[test]
    fn layout_choices() {
        let mut m = SettingsModel::from_entries(
            vec![("layout.name".into(), "\"\"".into(), "default".into(), vec![])],
            vec![],
        );
        m.set_layouts(vec!["adaptive".into(), "llm-dev".into()]);
        assert_eq!(m.rows[0].section, Section::Layout);
        assert_eq!(m.rows[0].choices, ["", "adaptive", "llm-dev"]);
    }
}

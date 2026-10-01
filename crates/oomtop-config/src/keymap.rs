//! Keymaps (UX §7, §12.7): presets `default | vim | emacs | htop`, user remapping from `keymap.toml`, and
//! conflict detection for `oomtop keys list --conflicts` (must be empty for the presets — UX §11 test 12).
//!
//! `keymap.toml`:
//! ```toml
//! [global]
//! "ctrl-k" = "palette"
//! "K"      = "stop"          # a second binding for stop
//! "g g"    = "move:top"      # chords: keys separated by spaces
//! "x"      = "none"          # unbind
//! [details]
//! "o"      = "open:cwd"
//! ```
//! Key syntax: a character (`x`, `G`, `/`, `-`), a named key (`enter`, `esc`, `tab`, `backtab`, `space`,
//! `backspace`, `delete`, `insert`, `home`, `end`, `pageup`, `pagedown`, `up`, `down`, `left`, `right`,
//! `F1`–`F24`), with optional modifiers `ctrl-`, `alt-`, `shift-` (`+` also accepted, `C-`/`M-` too).

use crate::layered::{closest, key_lines, line_of};
use crate::ConfigError;
use std::collections::BTreeMap;
use std::path::Path;

/// All bindable actions.
pub const ACTIONS: &[&str] = &[
    "move:up",
    "move:down",
    "move:top",
    "move:bottom",
    "move:page-up",
    "move:page-down",
    "expand",
    "collapse",
    "view:home",
    "view:processes",
    "view:models",
    "view:sandboxes",
    "view:reclaim",
    "view:timeline",
    "view:next",
    "view:prev",
    "filter",
    "command",
    "palette",
    "stop",
    "suspend",
    "pin",
    "mute",
    "less",
    "rename",
    "undo",
    "pin-mode",
    "why",
    "watch",
    "compare",
    "settings",
    "help",
    "quit",
    "sort",
    "sort-by",
    "tree",
    "refresh",
    "headline-action",
    "open:cwd",
    "open:log",
    "open:model",
];

/// Accepted synonyms → canonical action.
pub const ACTION_ALIASES: &[(&str, &str)] = &[
    ("view:groups", "view:home"),
    ("kill", "stop"),
    ("search", "filter"),
    ("top", "move:top"),
    ("bottom", "move:bottom"),
];

/// Binding contexts. `global` applies everywhere; the others override it while that view/pane has focus.
pub const CONTEXTS: &[&str] = &[
    "global",
    "home",
    "processes",
    "models",
    "sandboxes",
    "reclaim",
    "timeline",
    "details",
    "palette",
    "settings",
];

/// Special action value that removes a binding.
pub const UNBIND: &str = "none";

/// A context ("global", "details", …) → ordered (key, action) bindings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Keymap {
    pub preset: String,
    pub contexts: BTreeMap<String, Vec<(String, String)>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub context: String,
    pub key: String,
    pub actions: Vec<String>,
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {:?}: {}",
            self.context,
            self.key,
            self.actions.join(" vs ")
        )
    }
}

fn b(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, a)| (k.to_string(), a.to_string()))
        .collect()
}

/// UX §7 table.
const DEFAULT_GLOBAL: &[(&str, &str)] = &[
    ("up", "move:up"),
    ("k", "move:up"),
    ("down", "move:down"),
    ("j", "move:down"),
    ("enter", "expand"),
    ("right", "expand"),
    ("l", "expand"),
    ("left", "collapse"),
    ("h", "collapse"),
    ("g", "move:top"),
    ("G", "move:bottom"),
    ("home", "move:top"),
    ("end", "move:bottom"),
    ("pageup", "move:page-up"),
    ("pagedown", "move:page-down"),
    ("1", "view:home"),
    ("2", "view:processes"),
    ("3", "view:models"),
    ("4", "view:sandboxes"),
    ("5", "view:reclaim"),
    ("6", "view:timeline"),
    ("t", "view:timeline"),
    ("tab", "view:next"),
    ("backtab", "view:prev"),
    ("/", "filter"),
    (":", "command"),
    ("ctrl-k", "palette"),
    ("x", "stop"),
    ("z", "suspend"),
    ("p", "pin"),
    ("m", "mute"),
    ("-", "less"),
    ("n", "rename"),
    ("u", "undo"),
    ("M", "pin-mode"),
    ("i", "why"),
    ("w", "watch"),
    ("c", "compare"),
    (",", "settings"),
    ("?", "help"),
    ("q", "quit"),
    ("ctrl-l", "refresh"),
    ("r", "headline-action"),
    // htop-style function-key bar (UX §7), shared by every preset.
    ("F1", "help"),
    ("F2", "settings"),
    ("F3", "palette"),
    ("F4", "filter"),
    ("F5", "tree"),
    ("F6", "sort-by"),
    ("F7", "why"),
    ("F8", "view:reclaim"),
    ("F9", "stop"),
    ("F10", "quit"),
];

/// A built-in preset.
pub fn preset(name: &str) -> Option<Keymap> {
    let mut global = b(DEFAULT_GLOBAL);
    let replace = |global: &mut Vec<(String, String)>, pairs: &[(&str, &str)]| {
        for (k, a) in pairs {
            global.retain(|(key, _)| key != k);
            global.push((k.to_string(), a.to_string()));
        }
    };
    match name {
        "default" => {}
        "vim" => {
            // `g g` for top (so a single `g` is a chord prefix, not an action)
            global.retain(|(k, _)| k != "g");
            replace(
                &mut global,
                &[
                    ("g g", "move:top"),
                    ("ctrl-d", "move:page-down"),
                    ("ctrl-u", "move:page-up"),
                    ("ctrl-f", "move:page-down"),
                    ("ctrl-b", "move:page-up"),
                ],
            );
        }
        "emacs" => replace(
            &mut global,
            &[
                ("ctrl-n", "move:down"),
                ("ctrl-p", "move:up"),
                ("ctrl-f", "expand"),
                ("ctrl-b", "collapse"),
                ("ctrl-s", "filter"),
                ("alt-x", "command"),
                ("ctrl-v", "move:page-down"),
                ("alt-v", "move:page-up"),
                ("alt-<", "move:top"),
                ("alt->", "move:bottom"),
                ("ctrl-/", "undo"),
            ],
        ),
        "htop" => {
            // In the htop preset `k` means stop (kill), so it is no longer "up" (arrows still move). The F-keys
            // come from the shared bar (F3 search, F4 filter, F5 tree, F6 sort by, F9 kill, F10 quit).
            global.retain(|(k, _)| k != "k");
            replace(&mut global, &[("k", "stop")]);
        }
        _ => return None,
    }
    let mut contexts = BTreeMap::new();
    contexts.insert("global".to_string(), global);
    contexts.insert(
        "details".to_string(),
        b(&[("o", "open:cwd"), ("L", "open:log"), ("O", "open:model")]),
    );
    Some(Keymap {
        preset: name.to_string(),
        contexts,
    })
}

pub const PRESETS: &[&str] = &["default", "vim", "emacs", "htop"];

const NAMED: &[(&str, &str)] = &[
    ("up", "up"),
    ("down", "down"),
    ("left", "left"),
    ("right", "right"),
    ("enter", "enter"),
    ("return", "enter"),
    ("cr", "enter"),
    ("esc", "esc"),
    ("escape", "esc"),
    ("tab", "tab"),
    ("backtab", "backtab"),
    ("space", "space"),
    ("spc", "space"),
    ("backspace", "backspace"),
    ("bs", "backspace"),
    ("delete", "delete"),
    ("del", "delete"),
    ("insert", "insert"),
    ("ins", "insert"),
    ("home", "home"),
    ("end", "end"),
    ("pageup", "pageup"),
    ("pgup", "pageup"),
    ("pagedown", "pagedown"),
    ("pgdn", "pagedown"),
];

fn normalize_part(part: &str) -> Result<String, String> {
    if part.chars().count() == 1 {
        return Ok(part.to_string());
    }
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    let mut rest = part;
    loop {
        let lower = rest.to_ascii_lowercase();
        let mut matched = false;
        for (prefixes, flag) in [
            (&["ctrl-", "ctrl+", "control-", "c-"][..], 0u8),
            (&["alt-", "alt+", "meta-", "m-", "opt-", "option-", "a-"][..], 1),
            (&["shift-", "shift+", "s-"][..], 2),
        ] {
            if let Some(p) = prefixes
                .iter()
                .find(|p| lower.starts_with(**p) && rest.len() > p.len())
            {
                rest = &rest[p.len()..];
                match flag {
                    0 => ctrl = true,
                    1 => alt = true,
                    _ => shift = true,
                }
                matched = true;
                break;
            }
        }
        if !matched {
            break;
        }
    }
    let key = if rest.chars().count() == 1 {
        let c = rest.chars().next().unwrap_or(' ');
        if shift && c.is_ascii_alphabetic() {
            shift = false;
            c.to_ascii_uppercase().to_string()
        } else if ctrl && c.is_ascii_alphabetic() {
            c.to_ascii_lowercase().to_string()
        } else {
            c.to_string()
        }
    } else {
        let lower = rest.to_ascii_lowercase();
        if let Some((_, canon)) = NAMED.iter().find(|(n, _)| *n == lower) {
            if *canon == "tab" && shift {
                shift = false;
                "backtab".to_string()
            } else {
                canon.to_string()
            }
        } else if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
            if (1..=24).contains(&n) {
                format!("F{n}")
            } else {
                return Err(format!("no such function key {rest:?}"));
            }
        } else {
            let names: Vec<&str> = NAMED.iter().map(|(n, _)| *n).collect();
            return Err(match closest(&lower, names.iter().copied()) {
                Some(c) => format!("unknown key {rest:?} — did you mean {c:?}?"),
                None => format!("unknown key {rest:?}"),
            });
        }
    };
    let mut out = String::new();
    if ctrl {
        out.push_str("ctrl-");
    }
    if alt {
        out.push_str("alt-");
    }
    if shift {
        out.push_str("shift-");
    }
    out.push_str(&key);
    Ok(out)
}

/// Canonical form of a key or chord (`"C-K"` → `"ctrl-k"`, `"shift-g"` → `"G"`, `"g  g"` → `"g g"`).
pub fn normalize_key(key: &str) -> Result<String, String> {
    let parts: Vec<&str> = key.split_whitespace().collect();
    if parts.is_empty() {
        // a literal space
        return if key == " " {
            Ok("space".into())
        } else {
            Err("empty key".into())
        };
    }
    if parts.len() > 4 {
        return Err("chords are limited to 4 keys".into());
    }
    parts
        .iter()
        .map(|p| normalize_part(p))
        .collect::<Result<Vec<_>, _>>()
        .map(|v| v.join(" "))
}

/// Canonical action name (aliases resolved), or `None` if unknown.
pub fn canonical_action(action: &str) -> Option<&'static str> {
    if let Some(a) = ACTIONS.iter().find(|a| **a == action) {
        return Some(a);
    }
    ACTION_ALIASES
        .iter()
        .find(|(alias, _)| *alias == action)
        .map(|(_, a)| *a)
}

impl Keymap {
    /// Applies user remaps: a user binding replaces any preset binding of the same key in that context;
    /// the action `"none"` removes the binding.
    pub fn apply_user(&mut self, user: &BTreeMap<String, BTreeMap<String, String>>) {
        for (ctx, binds) in user {
            let list = self.contexts.entry(ctx.clone()).or_default();
            for (k, a) in binds {
                let key = normalize_key(k).unwrap_or_else(|_| k.clone());
                list.retain(|(existing, _)| *existing != key);
                if a != UNBIND {
                    let action = canonical_action(a)
                        .map(str::to_string)
                        .unwrap_or_else(|| a.clone());
                    list.push((key, action));
                }
            }
        }
    }

    /// Action bound to `key` in `context` (falls back to global).
    pub fn action(&self, context: &str, key: &str) -> Option<&str> {
        let find = |c: &str| {
            self.contexts
                .get(c)?
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, a)| a.as_str())
        };
        find(context).or_else(|| find("global"))
    }

    /// Keys bound to an action (for the `?` help and footer hints).
    pub fn keys_for(&self, action: &str) -> Vec<&str> {
        self.contexts
            .values()
            .flatten()
            .filter(|(_, a)| a == action)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    /// Every binding as (context, key, action), sorted by context then action (`oomtop keys list`).
    pub fn bindings(&self) -> Vec<(String, String, String)> {
        let mut v: Vec<(String, String, String)> = self
            .contexts
            .iter()
            .flat_map(|(c, binds)| binds.iter().map(|(k, a)| (c.clone(), k.clone(), a.clone())))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)).then(a.1.cmp(&b.1)));
        v
    }

    /// True when `prefix` is a strict prefix of a chord bound in `context` or global (the TUI waits for more
    /// keys).
    pub fn is_chord_prefix(&self, context: &str, prefix: &str) -> bool {
        let p = format!("{prefix} ");
        [context, "global"].iter().any(|c| {
            self.contexts
                .get(*c)
                .is_some_and(|binds| binds.iter().any(|(k, _)| k.starts_with(&p)))
        })
    }

    /// Problems for `oomtop keys list --conflicts`:
    /// - the same key bound to different actions within one context;
    /// - a key that is also the first key of a chord in the same context or in global (ambiguous: the chord
    ///   could never be typed, or the key would always wait);
    /// - unknown actions and invalid key syntax.
    ///
    /// A context binding that overrides a global one is intentional and not a conflict.
    pub fn conflicts(&self) -> Vec<Conflict> {
        let mut out = Vec::new();
        let global: Vec<(String, String)> = self.contexts.get("global").cloned().unwrap_or_default();
        for (ctx, binds) in &self.contexts {
            let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (k, a) in binds {
                let key = normalize_key(k).unwrap_or_else(|_| k.clone());
                by_key.entry(key).or_default().push(a.clone());
            }
            for (k, mut acts) in by_key.clone() {
                acts.sort();
                acts.dedup();
                if acts.len() > 1 {
                    out.push(Conflict {
                        context: ctx.clone(),
                        key: k.to_string(),
                        actions: acts,
                    });
                }
            }
            // chord prefixes: a bound key K and a chord "K …" (same context, or this context vs global)
            let mut scope: Vec<(String, String)> = binds.clone();
            if ctx != "global" {
                scope.extend(global.iter().filter(|(gk, _)| !by_key.contains_key(gk)).cloned());
            }
            for (k, a) in binds {
                let key = normalize_key(k).unwrap_or_else(|_| k.clone());
                let p = format!("{key} ");
                if let Some((chord, ca)) = scope.iter().find(|(c, _)| c.starts_with(&p)) {
                    out.push(Conflict {
                        context: ctx.clone(),
                        key: key.clone(),
                        actions: vec![a.clone(), format!("prefix of chord {chord:?} ({ca})")],
                    });
                }
            }
            // the other direction: a global single key that starts a chord of this context (in this context
            // the global key would never fire, or the chord never complete)
            if ctx != "global" {
                for (gk, ga) in global.iter().filter(|(gk, _)| !by_key.contains_key(gk)) {
                    let p = format!("{gk} ");
                    if let Some((chord, ca)) = binds.iter().find(|(c, _)| c.starts_with(&p)) {
                        out.push(Conflict {
                            context: ctx.clone(),
                            key: gk.clone(),
                            actions: vec![
                                format!("{ga} (global)"),
                                format!("prefix of chord {chord:?} ({ca})"),
                            ],
                        });
                    }
                }
            }
            for (k, a) in binds {
                if canonical_action(a).is_none() {
                    out.push(Conflict {
                        context: ctx.clone(),
                        key: k.clone(),
                        actions: vec![format!("unknown action {a}")],
                    });
                }
                if let Err(e) = normalize_key(k) {
                    out.push(Conflict {
                        context: ctx.clone(),
                        key: k.clone(),
                        actions: vec![format!("invalid key: {e}")],
                    });
                }
            }
        }
        out
    }
}

fn err(path: &Path, line: Option<usize>, message: String) -> ConfigError {
    ConfigError {
        path: Some(path.to_path_buf()),
        line,
        message,
    }
}

/// Parses `keymap.toml` text into context → key → action, validating keys, actions and contexts.
/// Invalid entries are skipped and reported with file:line.
pub fn parse_keymap_file(
    text: &str,
    path: &Path,
) -> (BTreeMap<String, BTreeMap<String, String>>, Vec<ConfigError>) {
    let mut errors = Vec::new();
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => {
            errors.push(err(
                path,
                e.span().map(|s| line_of(text, s.start)),
                e.message().to_string(),
            ));
            return (out, errors);
        }
    };
    let lines = key_lines(text);
    for (ctx, v) in &table {
        let line = lines.get(ctx.as_str()).copied();
        let Some(binds) = v.as_table() else {
            errors.push(err(
                path,
                line,
                format!("{ctx:?} must be a [context] table of \"key\" = \"action\" (the preset is keys.preset in config.toml)"),
            ));
            continue;
        };
        if !CONTEXTS.contains(&ctx.as_str()) {
            let hint = closest(ctx, CONTEXTS.iter().copied())
                .map(|c| format!(" — did you mean [{c}]?"))
                .unwrap_or_default();
            errors.push(err(
                path,
                line,
                format!(
                    "unknown context [{ctx}]{hint} (contexts: {})",
                    CONTEXTS.join(", ")
                ),
            ));
            continue;
        }
        for (k, a) in binds {
            let line = lines.get(&format!("{ctx}.{k}")).copied().or(line);
            let Some(a) = a.as_str() else {
                errors.push(err(path, line, format!("[{ctx}] {k:?}: action must be a string")));
                continue;
            };
            let key = match normalize_key(k) {
                Ok(key) => key,
                Err(e) => {
                    errors.push(err(path, line, format!("[{ctx}] {k:?}: {e}")));
                    continue;
                }
            };
            if a != UNBIND && canonical_action(a).is_none() {
                let hint = closest(a, ACTIONS.iter().copied())
                    .map(|c| format!(" — did you mean {c:?}?"))
                    .unwrap_or_default();
                errors.push(err(
                    path,
                    line,
                    format!("[{ctx}] {k:?}: unknown action {a:?}{hint}"),
                ));
                continue;
            }
            out.entry(ctx.clone()).or_default().insert(key, a.to_string());
        }
    }
    (out, errors)
}

/// Loads a preset plus the user's `keymap.toml` (`[context] "key" = "action"`).
pub fn load_keymap(preset_name: &str, user_file: Option<&Path>) -> (Keymap, Vec<ConfigError>) {
    let mut errors = Vec::new();
    let mut km = preset(preset_name).unwrap_or_else(|| {
        errors.push(ConfigError {
            path: None,
            line: None,
            message: format!(
                "unknown key preset {preset_name:?} (presets: {}); using default",
                PRESETS.join(", ")
            ),
        });
        preset("default").unwrap_or_default()
    });
    if let Some(p) = user_file {
        match std::fs::read_to_string(p) {
            Ok(text) => {
                let (user, errs) = parse_keymap_file(&text, p);
                errors.extend(errs);
                km.apply_user(&user);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => errors.push(err(p, None, format!("cannot read: {e} (using the preset only)"))),
        }
    }
    (km, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_no_conflicts() {
        for p in PRESETS {
            let km = preset(p).unwrap();
            assert!(km.conflicts().is_empty(), "{p}: {:?}", km.conflicts());
        }
        assert_eq!(preset("htop").unwrap().action("global", "k"), Some("stop"));
        assert_eq!(preset("htop").unwrap().action("global", "F9"), Some("stop"));
        assert_eq!(preset("default").unwrap().action("global", "k"), Some("move:up"));
        assert_eq!(preset("default").unwrap().action("details", "x"), Some("stop"));
        assert_eq!(preset("vim").unwrap().action("global", "g g"), Some("move:top"));
        assert!(preset("vim").unwrap().is_chord_prefix("global", "g"));
        assert!(preset("nope").is_none());
    }

    #[test]
    fn default_preset_matches_ux_table() {
        // UX §7: every key of the interaction table, exactly once, with the documented action
        let km = preset("default").unwrap();
        let expect = [
            ("up", "move:up"),
            ("down", "move:down"),
            ("j", "move:down"),
            ("k", "move:up"),
            ("enter", "expand"),
            ("right", "expand"),
            ("l", "expand"),
            ("left", "collapse"),
            ("h", "collapse"),
            ("g", "move:top"),
            ("G", "move:bottom"),
            ("1", "view:home"),
            ("2", "view:processes"),
            ("3", "view:models"),
            ("4", "view:sandboxes"),
            ("5", "view:reclaim"),
            ("6", "view:timeline"),
            ("t", "view:timeline"),
            ("tab", "view:next"),
            ("/", "filter"),
            (":", "command"),
            ("ctrl-k", "palette"),
            ("x", "stop"),
            ("z", "suspend"),
            ("p", "pin"),
            ("m", "mute"),
            ("-", "less"),
            ("n", "rename"),
            ("u", "undo"),
            ("M", "pin-mode"),
            ("i", "why"),
            ("w", "watch"),
            ("c", "compare"),
            (",", "settings"),
            ("?", "help"),
            ("q", "quit"),
        ];
        for (k, a) in expect {
            assert_eq!(km.action("global", k), Some(a), "{k}");
        }
        for (_, k, a) in km.bindings() {
            assert!(canonical_action(&a).is_some(), "{k}: {a}");
            assert_eq!(normalize_key(&k).unwrap(), k, "preset keys are canonical");
        }
    }

    #[test]
    fn every_preset_has_the_htop_function_key_bar() {
        // UX §7: F1 Help · F2 Setup · F3 Search · F4 Filter · F5 Tree · F6 SortBy · F7 Why · F8 Reclaim ·
        // F9 Stop · F10 Quit, in every preset, conflict-free.
        let bar = [
            ("F1", "help"),
            ("F2", "settings"),
            ("F3", "palette"),
            ("F4", "filter"),
            ("F5", "tree"),
            ("F6", "sort-by"),
            ("F7", "why"),
            ("F8", "view:reclaim"),
            ("F9", "stop"),
            ("F10", "quit"),
        ];
        for p in PRESETS {
            let km = preset(p).unwrap();
            for (k, a) in bar {
                assert_eq!(km.action("global", k), Some(a), "{p}: {k}");
            }
            assert!(km.conflicts().is_empty(), "{p}: {:?}", km.conflicts());
        }
    }

    #[test]
    fn global_key_that_starts_a_context_chord_is_a_conflict() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("keymap.toml");
        // `o x` in details while global `o` is unbound → fine; `x y` in details while global `x` = stop → ambiguous
        std::fs::write(&f, "[details]\n\"x y\" = \"open:log\"\n").unwrap();
        let (km, errs) = load_keymap("default", Some(&f));
        assert!(errs.is_empty(), "{errs:?}");
        let c = km.conflicts();
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].context, "details");
        assert_eq!(c[0].key, "x");
        assert!(c[0].to_string().contains("stop (global)"), "{}", c[0]);
        // a chord that does not start with a bound global key is fine
        std::fs::write(&f, "[details]\n\"b y\" = \"open:log\"\n").unwrap();
        let (km, _) = load_keymap("default", Some(&f));
        assert!(km.conflicts().is_empty(), "{:?}", km.conflicts());
    }

    #[test]
    fn unreadable_keymap_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let (_, errs) = load_keymap("default", Some(&d.path().join("missing.toml")));
        assert!(errs.is_empty());
        let (km, errs) = load_keymap("default", Some(d.path()));
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].message.starts_with("cannot read"));
        assert_eq!(km, preset("default").unwrap());
    }

    #[test]
    fn key_normalization() {
        for (raw, canon) in [
            ("C-K", "ctrl-k"),
            ("ctrl+k", "ctrl-k"),
            ("Ctrl-K", "ctrl-k"),
            ("shift-g", "G"),
            ("S-tab", "backtab"),
            ("f9", "F9"),
            ("M-x", "alt-x"),
            ("g  g", "g g"),
            ("-", "-"),
            ("ctrl--", "ctrl--"),
            ("PgDn", "pagedown"),
            ("Return", "enter"),
            ("alt-<", "alt-<"),
        ] {
            assert_eq!(normalize_key(raw).as_deref(), Ok(canon), "{raw}");
        }
        assert!(normalize_key("F99").is_err());
        assert!(normalize_key("entr")
            .unwrap_err()
            .contains("did you mean \"enter\"?"));
        assert!(normalize_key("").is_err());
    }

    #[test]
    fn user_remap() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("keymap.toml");
        std::fs::write(
            &f,
            "[global]\n\"K\" = \"stop\"\n\"q\" = \"help\"\n\"C-L\" = \"none\"\n\"g g\" = \"view:groups\"\n\"g\" = \"none\"\n[details]\n\"o\" = \"open:cwd\"\n",
        )
        .unwrap();
        let (km, errs) = load_keymap("default", Some(&f));
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(km.action("global", "K"), Some("stop"));
        assert_eq!(km.action("global", "q"), Some("help"));
        assert_eq!(km.action("global", "ctrl-l"), None, "unbound");
        assert_eq!(km.action("global", "g g"), Some("view:home"), "alias resolved");
        assert!(km.conflicts().is_empty(), "{:?}", km.conflicts());
        let mut bad = km.clone();
        bad.contexts
            .get_mut("global")
            .unwrap()
            .push(("x".into(), "quit".into()));
        assert_eq!(bad.conflicts().len(), 1);
    }

    #[test]
    fn chord_prefix_and_bad_entries_are_reported() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("keymap.toml");
        // `g g` while `g` is still move:top → ambiguous
        std::fs::write(&f, "[global]\n\"g g\" = \"move:top\"\n").unwrap();
        let (km, errs) = load_keymap("default", Some(&f));
        assert!(errs.is_empty());
        let c = km.conflicts();
        assert_eq!(c.len(), 1, "{c:?}");
        assert!(c[0].to_string().contains("prefix of chord \"g g\""), "{}", c[0]);

        std::fs::write(
            &f,
            "preset = \"vim\"\n[global]\n\"x\" = \"stp\"\n\"F99\" = \"quit\"\n[detials]\n\"o\" = \"open:cwd\"\n",
        )
        .unwrap();
        let (km, errs) = load_keymap("vim", Some(&f));
        assert_eq!(errs.len(), 4, "{errs:?}");
        assert!(
            errs.iter()
                .any(|e| e.line == Some(1) && e.message.contains("keys.preset")),
            "{errs:?}"
        );
        let all: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
        assert!(
            all.iter()
                .any(|e| e.contains(":3: [global] \"x\": unknown action \"stp\" — did you mean \"stop\"?")),
            "{all:?}"
        );
        assert!(all.iter().any(|e| e.contains(":4: [global] \"F99\"")), "{all:?}");
        assert!(
            all.iter()
                .any(|e| e.contains(":5: unknown context [detials] — did you mean [details]?")),
            "{all:?}"
        );
        assert_eq!(
            km.action("global", "x"),
            Some("stop"),
            "invalid entries are skipped"
        );

        // duplicate keys are a TOML error with a line (UX §12.7 example binds "g g" twice)
        std::fs::write(
            &f,
            "[global]\n\"g g\" = \"view:groups\"\n\"g g\" = \"move:top\"\n",
        )
        .unwrap();
        let (_, errs) = load_keymap("default", Some(&f));
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].line, Some(3));
        let (_, errs) = load_keymap("bogus", None);
        assert!(errs[0].message.contains("unknown key preset"));
    }
}

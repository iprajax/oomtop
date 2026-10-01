//! Layered config (UX §12.1.3), later wins:
//! `built-in defaults → /etc/oomtop/ → ~/.config/oomtop/config.toml → config.d/*.toml (alphabetical) →
//! config.d/host-<hostname>.toml → OOMTOP_* env vars → CLI flags → runtime toggles`.
//!
//! Every leaf value records its [`Origin`] (file:line for files). A layer that fails to parse or validate is
//! reported with file:line and a fix hint, and **its last good version stays in effect** (when one is known
//! from an earlier load through the same [`LayerCache`], i.e. during live reload); otherwise the layer is
//! skipped. Env vars and flags are applied one by one, so one bad variable does not discard the others.

use crate::model::Config;
use crate::paths::{short_hostname, system_dir, toml_files, ConfigPaths};
use crate::ConfigError;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use toml::{Table, Value};

/// Where an effective value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Default,
    File { path: PathBuf, line: Option<usize> },
    Env(String),
    Flag,
    Runtime,
}

impl std::fmt::Display for Origin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Origin::Default => write!(f, "default"),
            Origin::File { path, line } => {
                // user files by name (`config.toml:12`, `host-air.toml:3`, UX §12.8); the system layer by its
                // full path so `/etc/oomtop/config.toml` is never mistaken for the user's `config.toml`
                let name = if path.starts_with(system_dir()) {
                    path.display().to_string()
                } else {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                };
                match line {
                    Some(l) => write!(f, "{name}:{l}"),
                    None => write!(f, "{name}"),
                }
            }
            Origin::Env(k) => write!(f, "env {k}"),
            Origin::Flag => write!(f, "flag"),
            Origin::Runtime => write!(f, "runtime"),
        }
    }
}

impl Origin {
    /// Short label for the settings screen: `default`, `config.toml:12`, `host-air.toml:3`, `env`, `flag`,
    /// `runtime` (UX §12.8).
    pub fn short(&self) -> String {
        match self {
            Origin::Env(_) => "env".into(),
            o => o.to_string(),
        }
    }
}

/// Inputs to [`load_layered`]. `Default` = the real user environment.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    /// `--config` / `OOMTOP_CONFIG`: replaces `config.toml` (drop-ins still apply from its directory).
    pub config_path: Option<PathBuf>,
    /// Override the user config dir (tests); default `paths::config_dir()`.
    pub config_dir: Option<PathBuf>,
    /// Override `/etc/oomtop` (tests). `Some(empty path)` disables the system layer.
    pub system_dir: Option<PathBuf>,
    pub hostname: Option<String>,
    /// Environment pairs; `None` = read `OOMTOP_*` from the process environment.
    pub env: Option<Vec<(String, String)>>,
    /// CLI flag overrides as (dotted key, TOML value literal or bare string).
    pub flags: Vec<(String, String)>,
}

impl LoadOptions {
    /// The user config directory these options resolve to.
    pub fn user_dir(&self) -> PathBuf {
        self.config_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .or_else(|| self.config_dir.clone())
            .unwrap_or_else(crate::paths::config_dir)
    }

    /// Paths of the user config directory.
    pub fn paths(&self) -> ConfigPaths {
        ConfigPaths::new(&self.user_dir())
    }

    /// The main config file (`--config` or `config.toml`).
    pub fn main_file(&self) -> PathBuf {
        self.config_path
            .clone()
            .unwrap_or_else(|| self.paths().config_toml)
    }

    pub fn hostname(&self) -> String {
        self.hostname.clone().unwrap_or_else(short_hostname)
    }

    /// The system layer directory, if enabled.
    pub fn system(&self) -> Option<PathBuf> {
        let sys = self.system_dir.clone().unwrap_or_else(system_dir);
        (!sys.as_os_str().is_empty()).then_some(sys)
    }

    /// Every config file in precedence order (existing or not): system, main, drop-ins, host file.
    pub fn layer_files(&self) -> Vec<PathBuf> {
        let mut v = Vec::new();
        if let Some(sys) = self.system() {
            v.push(sys.join("config.toml"));
            v.extend(toml_files(&sys.join("config.d")));
        }
        v.push(self.main_file());
        let paths = self.paths();
        v.extend(
            toml_files(&paths.config_d)
                .into_iter()
                .filter(|p| !is_host_file(p)),
        );
        v.push(paths.host_file(&self.hostname()));
        v
    }
}

fn is_host_file(p: &Path) -> bool {
    p.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("host-"))
}

/// Last good version of each config file, so an invalid edit keeps the previous values (UX §11 test 9).
#[derive(Debug, Clone, Default)]
pub struct LayerCache {
    files: HashMap<PathBuf, (String, Table)>,
}

impl LayerCache {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn len(&self) -> usize {
        self.files.len()
    }
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Result of loading.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// Dotted leaf key → origin.
    pub origins: BTreeMap<String, Origin>,
    /// Files that were read (in precedence order).
    pub files: Vec<PathBuf>,
    /// Problems found (the offending layer was skipped or kept at its last good version).
    pub errors: Vec<ConfigError>,
    merged: Table,
    /// Dotted key → every non-default layer that set it, in precedence order.
    set_by: BTreeMap<String, Vec<Origin>>,
}

/// One effective setting (for `oomtop config print --effective --origin` and the settings screen).
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveEntry {
    pub key: String,
    pub value: String,
    pub origin: Origin,
}

/// Flattens a table into dotted leaf keys. Empty tables are leaves.
pub fn leaves(t: &Table) -> Vec<(String, Value)> {
    fn walk(prefix: &str, v: &Value, out: &mut Vec<(String, Value)>) {
        match v {
            Value::Table(t) if !t.is_empty() => {
                for (k, v) in t {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    walk(&key, v, out);
                }
            }
            _ => out.push((prefix.to_string(), v.clone())),
        }
    }
    let mut out = Vec::new();
    for (k, v) in t {
        walk(k, v, &mut out);
    }
    out
}

pub(crate) fn set_path(t: &mut Table, key: &str, v: Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let mut cur = t;
    for p in &parts[..parts.len() - 1] {
        let entry = cur
            .entry(p.to_string())
            .or_insert_with(|| Value::Table(Table::new()));
        if !entry.is_table() {
            *entry = Value::Table(Table::new());
        }
        let Value::Table(next) = entry else { return };
        cur = next;
    }
    cur.insert(parts[parts.len() - 1].to_string(), v);
}

/// Merges `src` over `dst`. Tables merge key by key, so an empty table in a later layer (e.g. an
/// uncommented `[appearance]` header with nothing under it) changes nothing instead of resetting the section.
pub(crate) fn deep_merge(dst: &mut Table, src: &Table) {
    for (k, v) in src {
        match (dst.get_mut(k), v) {
            (Some(Value::Table(d)), Value::Table(s)) => deep_merge(d, s),
            _ => {
                dst.insert(k.clone(), v.clone());
            }
        }
    }
}

/// 1-based line of a byte offset.
pub fn line_of(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    text.as_bytes()[..end].iter().filter(|b| **b == b'\n').count() + 1
}

/// Every dotted key of a TOML document (tables, arrays of tables and leaves) → 1-based line of its key.
pub fn key_lines(text: &str) -> BTreeMap<String, usize> {
    use toml_edit::{Item, TableLike};
    fn walk(t: &dyn TableLike, prefix: &str, text: &str, out: &mut BTreeMap<String, usize>) {
        for (k, item) in t.iter() {
            let key = if prefix.is_empty() {
                k.to_string()
            } else {
                format!("{prefix}.{k}")
            };
            let span = t
                .get_key_value(k)
                .and_then(|(key, _)| key.span())
                .or_else(|| item.span());
            if let Some(s) = span {
                out.entry(key.clone()).or_insert_with(|| line_of(text, s.start));
            }
            match item {
                Item::Table(sub) => {
                    if let Some(s) = sub.span() {
                        out.entry(key.clone()).or_insert_with(|| line_of(text, s.start));
                    }
                    walk(sub, &key, text, out);
                }
                Item::ArrayOfTables(a) => {
                    for (i, sub) in a.iter().enumerate() {
                        if let Some(s) = sub.span() {
                            let l = line_of(text, s.start);
                            out.entry(key.clone()).or_insert(l);
                            out.entry(format!("{key}.{i}")).or_insert(l);
                        }
                        walk(sub, &format!("{key}.{i}"), text, out);
                    }
                }
                Item::Value(toml_edit::Value::InlineTable(it)) => walk(it, &key, text, out),
                _ => {}
            }
        }
    }
    let mut out = BTreeMap::new();
    if let Ok(doc) = toml_edit::Document::parse(text) {
        walk(doc.as_table(), "", text, &mut out);
    }
    out
}

/// Best-effort 1-based line of a dotted key in TOML text (the key itself, else its nearest parent).
pub fn find_line(text: &str, key: &str) -> Option<usize> {
    let lines = key_lines(text);
    let mut k = key.to_string();
    loop {
        if let Some(l) = lines.get(&k) {
            return Some(*l);
        }
        match k.rsplit_once('.') {
            Some((parent, _)) => k = parent.to_string(),
            None => return None,
        }
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != *cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Closest candidate within edit distance 2 (or a shared prefix of ≥ 3 chars).
pub fn closest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    candidates
        .into_iter()
        .map(|c| (levenshtein(word, c), c))
        .filter(|(d, c)| *d <= 2 || (word.len() >= 3 && c.starts_with(word)))
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// A fix hint for a serde/toml message ("unknown field `thme`, expected one of `theme`, …").
pub fn hint_for(message: &str) -> Option<String> {
    let bad = message.split('`').nth(1)?;
    let (_, expected) = message.split_once("expected")?;
    let options: Vec<&str> = expected.split('`').skip(1).step_by(2).collect();
    closest(bad, options.iter().copied()).map(|c| format!("did you mean `{c}`?"))
}

fn with_hint(message: &str) -> String {
    let m = message.trim().to_string();
    match hint_for(&m) {
        Some(h) => format!("{m} — {h}"),
        None => m,
    }
}

fn toml_error(path: &Path, text: &str, e: &toml::de::Error) -> ConfigError {
    ConfigError {
        path: Some(path.to_path_buf()),
        line: e.span().map(|s| line_of(text, s.start)),
        message: with_hint(e.message()),
    }
}

/// Parses a flag/env value: TOML literal if it parses, else a bare string.
pub fn parse_value(s: &str) -> Value {
    match toml::from_str::<Table>(&format!("v = {s}")) {
        Ok(mut t) => t.remove("v").unwrap_or_else(|| Value::String(s.to_string())),
        Err(_) => Value::String(s.to_string()),
    }
}

/// Parses and validates one layer's text. `Err` carries file:line.
pub fn parse_layer(path: &Path, text: &str) -> Result<Table, ConfigError> {
    let table: Table = toml::from_str(text).map_err(|e| toml_error(path, text, &e))?;
    // Deserializing the text directly keeps spans, so type errors point at the offending line.
    toml::from_str::<Config>(text).map_err(|e| toml_error(path, text, &e))?;
    Ok(table)
}

struct Acc<'c> {
    merged: Table,
    origins: BTreeMap<String, Origin>,
    set_by: BTreeMap<String, Vec<Origin>>,
    files: Vec<PathBuf>,
    errors: Vec<ConfigError>,
    cache: &'c mut LayerCache,
}

impl Acc<'_> {
    /// Merges a layer if the result still deserializes; returns the error otherwise.
    fn apply(&mut self, layer: &Table, origin_of: &dyn Fn(&str) -> Origin) -> Result<(), String> {
        let mut candidate = self.merged.clone();
        deep_merge(&mut candidate, layer);
        Value::Table(candidate.clone())
            .try_into::<Config>()
            .map_err(|e| with_hint(&e.to_string()))?;
        self.merged = candidate;
        for (k, v) in leaves(layer) {
            // an empty table sets nothing (see `deep_merge`), so it takes no origin either
            if matches!(&v, Value::Table(t) if t.is_empty()) {
                continue;
            }
            let prefix = format!("{k}.");
            self.origins.retain(|existing, _| !existing.starts_with(&prefix));
            let o = origin_of(&k);
            self.set_by.entry(k.clone()).or_default().push(o.clone());
            self.origins.insert(k, o);
        }
        Ok(())
    }

    fn apply_text(&mut self, path: &Path, text: &str, table: &Table) -> Result<(), String> {
        let lines = key_lines(text);
        let p = path.to_path_buf();
        self.apply(table, &|k| Origin::File {
            path: p.clone(),
            line: nearest_line(&lines, k),
        })
    }

    fn file(&mut self, path: &Path) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                // a missing layer is normal; an unreadable one (permissions, not UTF-8, a directory) is reported
                if e.kind() != std::io::ErrorKind::NotFound {
                    let kept = self
                        .cache
                        .files
                        .get(path)
                        .cloned()
                        .is_some_and(|(old_text, old)| self.apply_text(path, &old_text, &old).is_ok());
                    self.errors.push(ConfigError {
                        path: Some(path.to_path_buf()),
                        line: None,
                        message: format!(
                            "cannot read: {e}{}",
                            if kept {
                                " (keeping the last good version of this file)"
                            } else {
                                " (this file is ignored until fixed)"
                            }
                        ),
                    });
                    if !kept {
                        self.cache.files.remove(path);
                    }
                    return;
                }
                self.cache.files.remove(path);
                return;
            }
        };
        self.files.push(path.to_path_buf());
        let parsed = parse_layer(path, &text).and_then(|t| {
            self.apply_text(path, &text, &t)
                .map(|_| t)
                .map_err(|m| ConfigError {
                    path: Some(path.to_path_buf()),
                    line: None,
                    message: m,
                })
        });
        match parsed {
            Ok(t) => {
                self.cache.files.insert(path.to_path_buf(), (text, t));
            }
            Err(mut e) => {
                match self.cache.files.get(path).cloned() {
                    Some((old_text, old)) if self.apply_text(path, &old_text, &old).is_ok() => {
                        e.message
                            .push_str(" (keeping the last good version of this file)");
                    }
                    _ => e.message.push_str(" (this file is ignored until fixed)"),
                }
                self.errors.push(e);
            }
        }
    }

    /// One env var / flag: TOML literal first, then the raw string (so `OOMTOP_APPEARANCE__COLOR=256` works).
    fn single(&mut self, key: &str, raw: &str, origin: Origin, label: &str) {
        let mut attempts = vec![parse_value(raw)];
        if !matches!(attempts[0], Value::String(_)) {
            attempts.push(Value::String(raw.to_string()));
        }
        let mut last_err = String::new();
        for v in attempts {
            let mut layer = Table::new();
            set_path(&mut layer, key, v);
            match self.apply(&layer, &|_| origin.clone()) {
                Ok(()) => return,
                Err(e) => last_err = e,
            }
        }
        self.errors.push(ConfigError {
            path: None,
            line: None,
            message: format!("{label}: {last_err} (ignored)"),
        });
    }
}

fn nearest_line(lines: &BTreeMap<String, usize>, key: &str) -> Option<usize> {
    let mut k = key;
    loop {
        if let Some(l) = lines.get(k) {
            return Some(*l);
        }
        k = k.rsplit_once('.')?.0;
    }
}

fn default_table() -> Table {
    match Value::try_from(Config::default()) {
        Ok(Value::Table(t)) => t,
        _ => Table::new(),
    }
}

/// `OOMTOP_APPEARANCE__THEME` → `appearance.theme` (double underscore = nesting). `None` for variables that are
/// not settings (`OOMTOP_CONFIG`, single-underscore names).
pub fn env_key(var: &str) -> Option<String> {
    let rest = var.strip_prefix("OOMTOP_")?;
    if rest == "CONFIG" || !rest.contains("__") {
        return None;
    }
    let key = rest.to_ascii_lowercase().replace("__", ".");
    (!key.split('.').any(str::is_empty)).then_some(key)
}

/// Loads all layers.
pub fn load_layered(opts: &LoadOptions) -> Loaded {
    load_layered_cached(opts, &mut LayerCache::new())
}

/// Loads all layers; files that became invalid fall back to their version in `cache` (live reload).
pub fn load_layered_cached(opts: &LoadOptions, cache: &mut LayerCache) -> Loaded {
    let defaults = default_table();
    let mut acc = Acc {
        origins: leaves(&defaults)
            .into_iter()
            .map(|(k, _)| (k, Origin::Default))
            .collect(),
        merged: defaults,
        set_by: BTreeMap::new(),
        files: Vec::new(),
        errors: Vec::new(),
        cache,
    };
    for f in opts.layer_files() {
        acc.file(&f);
    }

    let env: Vec<(String, String)> = match &opts.env {
        Some(e) => e.clone(),
        None => std::env::vars()
            .filter(|(k, _)| k.starts_with("OOMTOP_"))
            .collect(),
    };
    let mut env = env;
    env.sort();
    for (k, v) in &env {
        if k == "OOMTOP_NO_LEARN" {
            if v != "0" && !v.is_empty() && !v.eq_ignore_ascii_case("false") {
                acc.single("personalization.learn", "false", Origin::Env(k.clone()), k);
            }
            continue;
        }
        if let Some(key) = env_key(k) {
            acc.single(&key, v, Origin::Env(k.clone()), k);
        }
    }
    for (k, v) in &opts.flags {
        acc.single(k, v, Origin::Flag, &format!("--set {k}"));
    }
    let config = Value::Table(acc.merged.clone())
        .try_into::<Config>()
        .unwrap_or_default();
    Loaded {
        config,
        origins: acc.origins,
        files: acc.files,
        errors: acc.errors,
        merged: acc.merged,
        set_by: acc.set_by,
    }
}

impl Loaded {
    /// Every effective leaf value with its origin.
    pub fn effective(&self) -> Vec<EffectiveEntry> {
        leaves(&self.merged)
            .into_iter()
            .map(|(key, v)| {
                let origin = self.origin_of_leaf(&key);
                EffectiveEntry {
                    value: v.to_string(),
                    key,
                    origin,
                }
            })
            .collect()
    }

    fn origin_of_leaf(&self, key: &str) -> Origin {
        let mut k = key;
        loop {
            if let Some(o) = self.origins.get(k) {
                return o.clone();
            }
            match k.rsplit_once('.') {
                Some((parent, _)) => k = parent,
                None => return Origin::Default,
            }
        }
    }

    /// Origin of one dotted key.
    pub fn origin(&self, key: &str) -> Origin {
        self.origin_of_leaf(key)
    }

    /// Every layer that set `key` (lowest precedence first). More than one entry means the value is
    /// overridden elsewhere (UX §12.8).
    pub fn set_by(&self, key: &str) -> Vec<Origin> {
        self.set_by
            .iter()
            .filter(|(k, _)| *k == key || k.starts_with(&format!("{key}.")))
            .flat_map(|(_, v)| v.iter().cloned())
            .collect()
    }

    /// The effective value of one dotted key as TOML text.
    pub fn value(&self, key: &str) -> Option<String> {
        let mut cur = &self.merged;
        let parts: Vec<&str> = key.split('.').collect();
        for (i, p) in parts.iter().enumerate() {
            let v = cur.get(*p)?;
            if i + 1 == parts.len() {
                return Some(v.to_string());
            }
            cur = v.as_table()?;
        }
        None
    }

    /// The effective configuration as TOML.
    pub fn effective_toml(&self) -> String {
        toml::to_string(&self.config).unwrap_or_default()
    }

    /// Applies a runtime toggle (not persisted).
    pub fn set_runtime(&mut self, key: &str, value: &str) -> Result<(), ConfigError> {
        let mut last = None;
        let mut attempts = vec![parse_value(value)];
        if !matches!(attempts[0], Value::String(_)) {
            attempts.push(Value::String(value.to_string()));
        }
        for v in attempts {
            let mut layer = Table::new();
            set_path(&mut layer, key, v);
            let mut candidate = self.merged.clone();
            deep_merge(&mut candidate, &layer);
            match Value::Table(candidate.clone()).try_into::<Config>() {
                Ok(cfg) => {
                    self.merged = candidate;
                    self.config = cfg;
                    let prefix = format!("{key}.");
                    self.origins.retain(|k, _| !k.starts_with(&prefix));
                    self.origins.insert(key.to_string(), Origin::Runtime);
                    self.set_by
                        .entry(key.to_string())
                        .or_default()
                        .push(Origin::Runtime);
                    return Ok(());
                }
                Err(e) => last = Some(e),
            }
        }
        Err(ConfigError {
            path: None,
            line: None,
            message: with_hint(&last.map(|e| e.to_string()).unwrap_or_default()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(dir: &Path) -> LoadOptions {
        LoadOptions {
            config_dir: Some(dir.to_path_buf()),
            system_dir: Some(PathBuf::new()),
            hostname: Some("air".into()),
            env: Some(vec![]),
            ..Default::default()
        }
    }

    #[test]
    fn precedence_and_origins() {
        let d = tempfile::tempdir().unwrap();
        let sys = d.path().join("etc");
        std::fs::create_dir_all(sys.join("config.d")).unwrap();
        std::fs::write(
            sys.join("config.toml"),
            "[appearance]\ntheme = \"mint\"\nborders = \"thick\"\n[general]\nmouse = false\n",
        )
        .unwrap();
        std::fs::write(
            sys.join("config.d/10-site.toml"),
            "[general]\nhost_refresh_ms = 1500\n",
        )
        .unwrap();
        let user = d.path().join("user");
        std::fs::create_dir_all(user.join("config.d")).unwrap();
        std::fs::write(
            user.join("config.toml"),
            "# mine\n[appearance]\ntheme = \"mono\"\ndensity = \"compact\"\n",
        )
        .unwrap();
        std::fs::write(
            user.join("config.d/10-x.toml"),
            "[appearance]\ntheme = \"ember\"\n",
        )
        .unwrap();
        std::fs::write(
            user.join("config.d/05-a.toml"),
            "[appearance]\ntheme = \"sand\"\n",
        )
        .unwrap();
        std::fs::write(
            user.join("config.d/host-air.toml"),
            "[general]\nrefresh_ms = 5000\n[appearance]\ntheme = \"coral\"\n",
        )
        .unwrap();
        std::fs::write(
            user.join("config.d/host-other.toml"),
            "[general]\nrefresh_ms = 1\n",
        )
        .unwrap();
        let mut o = opts(&user);
        o.system_dir = Some(sys.clone());
        o.env = Some(vec![
            ("OOMTOP_APPEARANCE__BORDERS".into(), "plain".into()),
            ("OOMTOP_NO_LEARN".into(), "1".into()),
            ("OOMTOP_PERSONALIZATION__WEIGHTS__AFFINITY".into(), "0.9".into()),
            ("OOMTOP_CONFIG".into(), "/nope".into()),
            ("OOMTOP_UNRELATED".into(), "x".into()),
        ]);
        o.flags = vec![("format.decimals".into(), "2".into())];
        let l = load_layered(&o);
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert_eq!(l.config.appearance.theme, "coral", "host file wins over drop-ins");
        assert_eq!(l.config.appearance.density, crate::model::Density::Compact);
        assert_eq!(l.config.general.refresh_ms, 5000);
        assert!(!l.config.general.mouse, "system layer applies");
        assert_eq!(l.config.general.host_refresh_ms, 1500);
        assert_eq!(l.config.appearance.borders, crate::model::Borders::Plain);
        assert!(!l.config.personalization.learn);
        assert_eq!(l.config.personalization.weights.affinity, 0.9);
        assert_eq!(l.config.format.decimals, 2);
        assert_eq!(l.origin("appearance.theme").to_string(), "host-air.toml:4");
        assert_eq!(l.origin("appearance.density").to_string(), "config.toml:4");
        assert_eq!(l.origin("general.refresh_ms").to_string(), "host-air.toml:2");
        assert_eq!(l.origin("general.mouse").to_string(), "config.toml:5");
        assert_eq!(
            l.origin("appearance.borders").to_string(),
            "env OOMTOP_APPEARANCE__BORDERS"
        );
        assert_eq!(l.origin("appearance.borders").short(), "env");
        assert_eq!(l.origin("format.decimals"), Origin::Flag);
        assert_eq!(l.origin("serve.listen"), Origin::Default);
        // theme was set by 5 layers: system, config.toml, 05-a, 10-x, host
        let by: Vec<String> = l
            .set_by("appearance.theme")
            .iter()
            .map(|o| o.to_string())
            .collect();
        assert_eq!(
            by,
            [
                "config.toml:2",
                "config.toml:3",
                "05-a.toml:2",
                "10-x.toml:2",
                "host-air.toml:4"
            ]
        );
        assert_eq!(l.value("appearance.theme").as_deref(), Some("\"coral\""));
        assert_eq!(l.files.len(), 6);
        assert!(l
            .effective()
            .iter()
            .any(|e| e.key == "general.refresh_ms" && e.value == "5000"));
    }

    #[test]
    fn invalid_layer_is_skipped_with_location_and_hint() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("config.toml"), "[appearance]\ntheme = \"mono\"\n").unwrap();
        std::fs::create_dir(d.path().join("config.d")).unwrap();
        std::fs::write(
            d.path().join("config.d/20-bad.toml"),
            "[general]\nmouse = true\nrefresh_ms = \"fast\"\n",
        )
        .unwrap();
        std::fs::write(d.path().join("config.d/30-syntax.toml"), "[general\nx=1\n").unwrap();
        std::fs::write(
            d.path().join("config.d/40-typo.toml"),
            "\n[appearance]\nthme = \"x\"\n",
        )
        .unwrap();
        let l = load_layered(&opts(d.path()));
        assert_eq!(l.config.appearance.theme, "mono");
        assert_eq!(l.config.general.refresh_ms, 2000);
        assert_eq!(l.errors.len(), 3, "{:?}", l.errors);
        assert_eq!(l.errors[0].line, Some(3));
        assert_eq!(l.errors[1].line, Some(1));
        assert_eq!(l.errors[2].line, Some(3));
        assert!(
            l.errors[2].message.contains("did you mean `theme`?"),
            "{}",
            l.errors[2]
        );
        assert!(l.errors[2]
            .to_string()
            .contains("40-typo.toml:3: unknown field `thme`"));
    }

    #[test]
    fn env_values_are_coerced_and_isolated() {
        let d = tempfile::tempdir().unwrap();
        let mut o = opts(d.path());
        o.env = Some(vec![
            ("OOMTOP_APPEARANCE__COLOR".into(), "256".into()),
            ("OOMTOP_GENERAL__REFRESH_MS".into(), "fast".into()),
            ("OOMTOP_HEADROOM__MIN_MARGIN".into(), "2G".into()),
            ("OOMTOP_PROTECTED__NAMES".into(), "[\"postgres\"]".into()),
        ]);
        let l = load_layered(&o);
        assert_eq!(l.config.appearance.color, crate::model::ColorMode::C256);
        assert_eq!(l.config.headroom.min_margin, "2G");
        assert_eq!(l.config.protected.names, vec!["postgres".to_string()]);
        assert_eq!(l.config.general.refresh_ms, 2000);
        assert_eq!(l.errors.len(), 1);
        assert!(l.errors[0].message.starts_with("OOMTOP_GENERAL__REFRESH_MS:"));
        assert_eq!(env_key("OOMTOP_A__B_C"), Some("a.b_c".into()));
        assert_eq!(env_key("OOMTOP_A____B"), None);
        assert_eq!(env_key("OOMTOP_CONFIG"), None);
    }

    #[test]
    fn cache_keeps_last_good_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("config.toml");
        std::fs::write(&f, "[appearance]\ntheme = \"mono\"\n").unwrap();
        let mut cache = LayerCache::new();
        let o = opts(d.path());
        assert_eq!(
            load_layered_cached(&o, &mut cache).config.appearance.theme,
            "mono"
        );
        std::fs::write(&f, "[appearance]\ntheme = 3\n").unwrap();
        let l = load_layered_cached(&o, &mut cache);
        assert_eq!(l.config.appearance.theme, "mono");
        assert_eq!(l.errors.len(), 1);
        assert!(l.errors[0].message.contains("keeping the last good version"));
        assert_eq!(l.errors[0].line, Some(2));
        // without the cache the file is ignored
        let l = load_layered(&o);
        assert_eq!(l.config.appearance.theme, "terminal");
        assert!(l.errors[0].message.contains("ignored until fixed"));
    }

    #[test]
    fn lines_of_nested_keys() {
        let text = "a = 1\n[p]\nx.y = 2\nz = { q = 3 }\n\n[[views]]\nname = \"v\"\n[[views]]\nname = \"w\"\n";
        let l = key_lines(text);
        assert_eq!(l["a"], 1);
        assert_eq!(l["p"], 2);
        assert_eq!(l["p.x.y"], 3);
        assert_eq!(l["p.z.q"], 4);
        assert_eq!(l["views"], 6);
        assert_eq!(l["views.1.name"], 9);
        assert_eq!(find_line(text, "p.z.nope"), Some(4));
        assert_eq!(find_line(text, "nope"), None);
    }

    #[test]
    fn empty_section_header_keeps_lower_layers() {
        // regression: an uncommented `[appearance]` header with nothing under it used to replace the whole
        // section with defaults (dropping the system layer's theme) and claim every appearance.* origin
        let d = tempfile::tempdir().unwrap();
        let sys = d.path().join("etc");
        std::fs::create_dir_all(&sys).unwrap();
        std::fs::write(
            sys.join("config.toml"),
            "[appearance]\ntheme = \"mint\"\n[adapters.ports]\nollama = 11500\n",
        )
        .unwrap();
        let user = d.path().join("user");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(
            user.join("config.toml"),
            "# from `config init`, headers uncommented only\n[appearance]\n\n[adapters]\n[adapters.ports]\n[aliases]\n",
        )
        .unwrap();
        let mut o = opts(&user);
        o.system_dir = Some(sys.clone());
        let l = load_layered(&o);
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert_eq!(l.config.appearance.theme, "mint");
        assert_eq!(l.config.adapters.ports.get("ollama"), Some(&11500));
        assert_eq!(
            l.origin("appearance.theme"),
            Origin::File {
                path: sys.join("config.toml"),
                line: Some(2)
            }
        );
        assert_eq!(l.origin("appearance.density"), Origin::Default);
        assert!(l.set_by("appearance").len() == 1, "{:?}", l.set_by("appearance"));
    }

    #[test]
    fn system_origin_is_labelled_by_full_path() {
        let o = Origin::File {
            path: system_dir().join("config.toml"),
            line: Some(2),
        };
        assert_eq!(o.to_string(), "/etc/oomtop/config.toml:2");
        let o = Origin::File {
            path: PathBuf::from("/home/u/.config/oomtop/config.d/host-air.toml"),
            line: Some(3),
        };
        assert_eq!(o.short(), "host-air.toml:3");
    }

    #[test]
    fn unreadable_layer_is_reported_not_silently_skipped() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("config.d")).unwrap();
        // a directory where a drop-in file is expected cannot be read as text
        std::fs::create_dir_all(d.path().join("config.toml")).unwrap();
        std::fs::write(d.path().join("config.d/bin.toml"), [0xff, 0xfe, 0x00]).unwrap();
        let l = load_layered(&opts(d.path()));
        assert_eq!(l.errors.len(), 2, "{:?}", l.errors);
        assert!(l.errors.iter().all(|e| e.message.starts_with("cannot read")));
        assert_eq!(l.config, Config::default());
    }

    #[test]
    fn runtime_toggle() {
        let d = tempfile::tempdir().unwrap();
        let mut l = load_layered(&opts(d.path()));
        l.set_runtime("appearance.theme", "none").unwrap();
        assert_eq!(l.config.appearance.theme, "none");
        assert_eq!(l.origin("appearance.theme"), Origin::Runtime);
        l.set_runtime("appearance.color", "16").unwrap();
        assert_eq!(l.config.appearance.color, crate::model::ColorMode::C16);
        assert!(l.set_runtime("general.refresh_ms", "\"x\"").is_err());
        let e = l.set_runtime("appearance.density", "compat").unwrap_err();
        assert!(e.message.contains("did you mean `compact`?"), "{e}");
    }
}

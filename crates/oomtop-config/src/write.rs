//! Comment-preserving writes (UX §12.8, §12.9 `oomtop config set`): edits go through `toml_edit`, so user
//! comments, key order and formatting survive. The result is validated before it is written, and files are
//! replaced atomically (write to a temp file in the same directory, then rename).

use crate::layered::{closest, line_of};
use crate::model::Config;
use crate::paths::ConfigPaths;
use crate::ConfigError;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, InlineTable, Item, Table, TableLike, Value};

/// Which file a setting is saved to (`--layer user|host|dropin:<name>`, the settings screen's `s`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layer {
    /// `config.toml`.
    User,
    /// `config.d/host-<hostname>.toml`.
    Host,
    /// `config.d/<name>.toml`.
    Dropin(String),
    /// An explicit file (`--config PATH`).
    File(PathBuf),
}

impl Layer {
    /// Parses `user`, `host` or `dropin:<name>`.
    pub fn parse(s: &str) -> Result<Layer, ConfigError> {
        let err = |m: String| ConfigError {
            path: None,
            line: None,
            message: m,
        };
        match s {
            "user" | "config" => Ok(Layer::User),
            "host" => Ok(Layer::Host),
            other => match other.strip_prefix("dropin:") {
                Some(name) => {
                    let stem = name.strip_suffix(".toml").unwrap_or(name);
                    let ok = !stem.is_empty()
                        && !stem.starts_with("host-")
                        && !stem.starts_with('.')
                        && stem
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
                    if ok {
                        Ok(Layer::Dropin(stem.to_string()))
                    } else {
                        Err(err(format!(
                            "invalid drop-in name {name:?}: use letters, digits, '-', '_' (not host-*)"
                        )))
                    }
                }
                None => Err(err(format!(
                    "unknown layer {other:?}: expected user, host or dropin:<name>"
                ))),
            },
        }
    }

    /// The file this layer lives in.
    pub fn path(&self, paths: &ConfigPaths, hostname: &str) -> PathBuf {
        match self {
            Layer::User => paths.config_toml.clone(),
            Layer::Host => paths.host_file(hostname),
            Layer::Dropin(n) => paths.dropin(n),
            Layer::File(p) => p.clone(),
        }
    }
}

/// Parses a value literal ("2", "true", "\"x\"", "[\"a\"]"); anything else becomes a string.
pub fn edit_value(raw: &str) -> Value {
    raw.parse::<Value>().unwrap_or_else(|_| Value::from(raw))
}

fn plain(message: impl Into<String>) -> ConfigError {
    ConfigError {
        path: None,
        line: None,
        message: message.into(),
    }
}

fn parse_doc(text: &str) -> Result<DocumentMut, ConfigError> {
    text.parse().map_err(|e: toml_edit::TomlError| ConfigError {
        path: None,
        line: e.span().map(|s| line_of(text, s.start)),
        message: e.message().to_string(),
    })
}

fn split_key(key: &str) -> Result<Vec<&str>, ConfigError> {
    let parts: Vec<&str> = key.split('.').collect();
    if parts.is_empty() || parts.iter().any(|p| p.trim().is_empty()) {
        return Err(plain(format!("invalid key {key:?}")));
    }
    Ok(parts)
}

enum Target<'a> {
    Table(&'a mut Table),
    Inline(&'a mut InlineTable),
}

/// Descends to (creating as needed) the table that holds the last key part. Missing intermediate tables that
/// only hold sub-tables are created implicit, so no empty `[header]` is written for them.
fn descend<'a>(target: Target<'a>, sections: &[&str]) -> Result<Target<'a>, ConfigError> {
    let Some((first, rest)) = sections.split_first() else {
        return Ok(target);
    };
    match target {
        Target::Table(t) => {
            if t.get(first).is_none_or(Item::is_none) {
                let mut nt = Table::new();
                nt.set_implicit(!rest.is_empty());
                t.insert(first, Item::Table(nt));
            }
            match t.get_mut(first) {
                Some(Item::Table(nt)) => descend(Target::Table(nt), rest),
                Some(Item::Value(Value::InlineTable(it))) => descend(Target::Inline(it), rest),
                _ => Err(plain(format!("{first} is not a table"))),
            }
        }
        Target::Inline(it) => {
            if it.get(first).is_none() {
                it.insert(*first, Value::InlineTable(InlineTable::new()));
            }
            match it.get_mut(first) {
                Some(Value::InlineTable(x)) => descend(Target::Inline(x), rest),
                _ => Err(plain(format!("{first} is not a table"))),
            }
        }
    }
}

/// Sets `key` (dotted) in TOML `text`, returning the new text. Existing comments/decor are kept; values inside
/// inline tables are edited in place.
pub fn set_in_text(text: &str, key: &str, raw_value: &str) -> Result<String, ConfigError> {
    let mut last_err = None;
    let mut candidates = vec![edit_value(raw_value)];
    if !matches!(candidates[0], Value::String(_)) {
        // e.g. `appearance.color 256` is the string "256", not an integer
        candidates.push(Value::from(raw_value));
    }
    for v in candidates {
        match set_value_in_text(text, key, v) {
            Ok(out) => return Ok(out),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| plain("no value")))
}

fn set_value_in_text(text: &str, key: &str, mut v: Value) -> Result<String, ConfigError> {
    let mut doc = parse_doc(text)?;
    let parts = split_key(key)?;
    let (leaf, sections) = parts.split_last().ok_or_else(|| plain("empty key"))?;
    let target = descend(Target::Table(doc.as_table_mut()), sections)?;
    match target {
        Target::Table(table) => match table.get_mut(leaf) {
            Some(Item::Value(existing)) => {
                *v.decor_mut() = existing.decor().clone();
                *existing = v;
            }
            Some(Item::None) | None => {
                table.insert(leaf, Item::Value(v));
            }
            Some(_) => return Err(plain(format!("{key} is a table; set one of its keys instead"))),
        },
        Target::Inline(it) => match it.get_mut(leaf) {
            Some(existing) => {
                if existing.is_inline_table() {
                    return Err(plain(format!("{key} is a table; set one of its keys instead")));
                }
                *v.decor_mut() = existing.decor().clone();
                *existing = v;
            }
            None => {
                it.insert(*leaf, v);
                it.fmt();
            }
        },
    }
    let out = doc.to_string();
    validate_text(&out)?;
    Ok(out)
}

/// Removes `key` from TOML `text` (comments elsewhere are kept). Returns `Ok(None)` when the key is not set.
pub fn unset_in_text(text: &str, key: &str) -> Result<Option<String>, ConfigError> {
    let mut doc = parse_doc(text)?;
    let parts = split_key(key)?;
    let (leaf, sections) = parts.split_last().ok_or_else(|| plain("empty key"))?;
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for s in sections {
        match table.get_mut(s).and_then(Item::as_table_like_mut) {
            Some(t) => table = t,
            None => return Ok(None),
        }
    }
    if table.remove(leaf).is_none() {
        return Ok(None);
    }
    let out = doc.to_string();
    validate_text(&out)?;
    Ok(Some(out))
}

/// Every settable dotted key (leaves of the defaults, plus map sections whose keys are user-defined).
pub fn known_keys() -> Vec<String> {
    let t = match toml::Value::try_from(Config::default()) {
        Ok(toml::Value::Table(t)) => t,
        _ => return Vec::new(),
    };
    crate::layered::leaves(&t).into_iter().map(|(k, _)| k).collect()
}

/// Validates a single layer's text against the schema (merged over defaults), with file:line on errors.
pub fn validate_text(text: &str) -> Result<(), ConfigError> {
    crate::layered::parse_layer(Path::new(""), text)
        .map(|_| ())
        .map_err(|mut e| {
            e.path = None;
            e
        })
}

/// Resolves a symlinked config file to its target (dotfile managers such as stow/chezmoi/home-manager link
/// `~/.config/oomtop/config.toml` into a repo), so an atomic replace edits the real file instead of replacing
/// the link with a plain copy. Relative link targets resolve against the link's directory.
pub(crate) fn resolve_symlinks(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    for _ in 0..16 {
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => match std::fs::read_link(&p) {
                Ok(target) if target.is_absolute() => p = target,
                Ok(target) => p = p.parent().map(|d| d.join(&target)).unwrap_or(target),
                Err(_) => break,
            },
            _ => break,
        }
    }
    p
}

/// Writes `text` to `path` atomically (temp file in the same directory, fsync, rename), creating parent
/// directories. A symlinked `path` keeps its link: the link's target is replaced.
pub fn write_atomic(path: &Path, text: &str) -> Result<(), ConfigError> {
    use std::io::Write;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let target = resolve_symlinks(path);
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| ConfigError::io(path, &e))?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tmp-{}-{seq}", std::process::id()));
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(text.as_bytes())?;
        f.sync_all()
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(ConfigError::io(&tmp, &e));
    }
    if let Ok(meta) = std::fs::metadata(&target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ConfigError::io(path, &e)
    })
}

fn with_path(path: &Path, mut e: ConfigError) -> ConfigError {
    e.path = Some(path.to_path_buf());
    e
}

/// Sets `key` in the file at `path` (created if missing), preserving comments.
pub fn set_value(path: &Path, key: &str, raw_value: &str) -> Result<(), ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(ConfigError::io(path, &e)),
    };
    let out = set_in_text(&text, key, raw_value).map_err(|e| with_path(path, suggest(key, e)))?;
    write_atomic(path, &out)
}

/// Removes `key` from the file at `path`; `Ok(false)` when it was not set there.
pub fn unset_value(path: &Path, key: &str) -> Result<bool, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(ConfigError::io(path, &e)),
    };
    match unset_in_text(&text, key).map_err(|e| with_path(path, e))? {
        Some(out) => write_atomic(path, &out).map(|_| true),
        None => Ok(false),
    }
}

/// Sets `key` in a layer, returning the file written.
pub fn set_in_layer(
    paths: &ConfigPaths,
    hostname: &str,
    layer: &Layer,
    key: &str,
    raw_value: &str,
) -> Result<PathBuf, ConfigError> {
    let path = layer.path(paths, hostname);
    set_value(&path, key, raw_value)?;
    Ok(path)
}

fn suggest(key: &str, mut e: ConfigError) -> ConfigError {
    if e.message.contains("unknown field") && !e.message.contains("did you mean") {
        let keys = known_keys();
        if let Some(c) = closest(key, keys.iter().map(String::as_str)) {
            e.message.push_str(&format!(" — did you mean `{c}`?"));
        }
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_comments_and_order() {
        let text = "# my settings\n[appearance]\n# the theme\ntheme = \"mono\"   # trailing\ndensity = \"compact\"\n\n[general]\nrefresh_ms = 2000\n";
        let out = set_in_text(text, "appearance.theme", "ember").unwrap();
        assert_eq!(
            out,
            "# my settings\n[appearance]\n# the theme\ntheme = \"ember\"   # trailing\ndensity = \"compact\"\n\n[general]\nrefresh_ms = 2000\n"
        );
        let out = set_in_text(&out, "format.decimals", "2").unwrap();
        assert!(out.ends_with("[format]\ndecimals = 2\n"), "{out}");
        assert!(set_in_text(text, "general.refresh_ms", "\"fast\"").is_err());
        assert!(set_in_text(text, "nope.key", "1").is_err());
        assert!(set_in_text(text, "appearance", "1").is_err());
    }

    #[test]
    fn string_fallback_and_nested_tables() {
        let out = set_in_text("", "appearance.color", "256").unwrap();
        assert_eq!(out, "[appearance]\ncolor = \"256\"\n");
        let out = set_in_text("", "personalization.weights.affinity", "0.8").unwrap();
        assert_eq!(out, "[personalization.weights]\naffinity = 0.8\n");
        let out = set_in_text("adapters = { enabled = false }\n", "adapters.timeout_ms", "300").unwrap();
        assert_eq!(out, "adapters = { enabled = false, timeout_ms = 300 }\n");
        let out = set_in_text(
            "[adapters]\nports = { ollama = 1 }\n",
            "adapters.ports.ollama",
            "11434",
        )
        .unwrap();
        assert_eq!(out, "[adapters]\nports = { ollama = 11434 }\n");
        let out = set_in_text("", "aliases.llm", "kind:model,agent").unwrap();
        assert_eq!(out, "[aliases]\nllm = \"kind:model,agent\"\n");
    }

    #[test]
    fn unset_keeps_rest() {
        let text = "# top\n[appearance]\ntheme = \"mono\" # t\n# keep me\ndensity = \"compact\"\n";
        let out = unset_in_text(text, "appearance.theme").unwrap().unwrap();
        assert_eq!(out, "# top\n[appearance]\n# keep me\ndensity = \"compact\"\n");
        assert_eq!(unset_in_text(text, "general.mouse").unwrap(), None);
    }

    #[test]
    fn layers_and_files() {
        let d = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::new(d.path());
        assert_eq!(Layer::parse("user").unwrap(), Layer::User);
        assert_eq!(
            Layer::parse("dropin:50-ui").unwrap(),
            Layer::Dropin("50-ui".into())
        );
        assert!(Layer::parse("dropin:host-x").is_err());
        assert!(Layer::parse("dropin:../x").is_err());
        assert!(Layer::parse("site").is_err());
        let p = set_in_layer(&paths, "air", &Layer::Host, "general.refresh_ms", "3000").unwrap();
        assert_eq!(p, d.path().join("config.d/host-air.toml"));
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[general]\nrefresh_ms = 3000\n"
        );
        let p = set_in_layer(
            &paths,
            "air",
            &Layer::Dropin("50-ui".into()),
            "appearance.theme",
            "mint",
        )
        .unwrap();
        assert_eq!(p, d.path().join("config.d/50-ui.toml"));
        assert!(unset_value(&p, "appearance.theme").unwrap());
        assert!(!unset_value(&p, "appearance.theme").unwrap());
        let e = set_value(&paths.config_toml, "appearance.thme", "x").unwrap_err();
        assert!(e.message.contains("did you mean `theme`?"), "{e}");
        assert_eq!(e.line, Some(2));
        assert_eq!(e.path.as_deref(), Some(paths.config_toml.as_path()));
        assert!(!paths.config_toml.exists(), "invalid edits never write");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_dotfile_keeps_its_link() {
        // regression: rename() over a symlink replaced the link with a plain file (breaks stow/chezmoi repos)
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("dotfiles/oomtop");
        std::fs::create_dir_all(&repo).unwrap();
        let real = repo.join("config.toml");
        std::fs::write(&real, "# mine\n[appearance]\ntheme = \"mono\"\n").unwrap();
        let cfg = d.path().join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        let link = cfg.join("config.toml");
        std::os::unix::fs::symlink("../dotfiles/oomtop/config.toml", &link).unwrap();
        set_value(&link, "appearance.theme", "ember").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            "# mine\n[appearance]\ntheme = \"ember\"\n"
        );
        assert!(unset_value(&link, "appearance.theme").unwrap());
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "# mine\n[appearance]\n");
        // no temp files anywhere
        assert_eq!(std::fs::read_dir(&repo).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&cfg).unwrap().count(), 1);
    }

    #[test]
    fn unset_reports_unreadable_files() {
        let d = tempfile::tempdir().unwrap();
        assert!(!unset_value(&d.path().join("missing.toml"), "a.b").unwrap());
        // a directory is not readable as a file: an error, not "not set"
        assert!(unset_value(d.path(), "appearance.theme").is_err());
    }

    #[test]
    fn writes_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("sub/config.toml");
        set_value(&p, "appearance.theme", "none").unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[appearance]\ntheme = \"none\"\n"
        );
        let leftovers: Vec<_> = std::fs::read_dir(d.path().join("sub")).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp files left behind");
    }
}

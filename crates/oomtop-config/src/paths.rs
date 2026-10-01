//! File layout (UX §12.2): XDG paths on **both** OSes (macOS also honors `$XDG_CONFIG_HOME`, falling back to
//! `~/.config`, not `~/Library`).
//!
//! ```text
//! ~/.config/oomtop/            config.toml · config.d/*.toml · config.d/host-<hostname>.toml
//!                              themes/<name>.toml · keymap.toml · layouts/<name>.toml · rules.d/*.toml
//! ~/.local/state/oomtop/       state.db (profile, learning, lineage journal)
//! /etc/oomtop/                 system layer: config.toml · config.d/*.toml
//! ```

use std::path::{Path, PathBuf};

/// `$XDG_CONFIG_HOME/oomtop` or `~/.config/oomtop`.
pub fn config_dir() -> PathBuf {
    xdg_from(std::env::var_os("XDG_CONFIG_HOME"), ".config")
}

/// `$XDG_STATE_HOME/oomtop` or `~/.local/state/oomtop` (profile, learning, lineage journal).
pub fn state_dir() -> PathBuf {
    xdg_from(std::env::var_os("XDG_STATE_HOME"), ".local/state")
}

/// System-wide layer.
pub fn system_dir() -> PathBuf {
    PathBuf::from("/etc/oomtop")
}

/// Resolves an XDG base directory. Per the XDG Base Directory spec, a value that is empty or not absolute is
/// ignored and the fallback under `$HOME` is used.
pub fn xdg_from(value: Option<std::ffi::OsString>, fallback: &str) -> PathBuf {
    match value.map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(v) => v.join("oomtop"),
        None => home().join(fallback).join("oomtop"),
    }
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Expands a leading `~` / `~/`.
pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        return home();
    }
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

/// Short hostname (before the first dot), used for `config.d/host-<hostname>.toml`. Characters that are not
/// safe in a file name are replaced with `-`.
pub fn short_hostname() -> String {
    let h = gethostname::gethostname().to_string_lossy().into_owned();
    sanitize_hostname(&h)
}

/// `"Alexs-MacBook-Air.local"` → `"Alexs-MacBook-Air"`. An empty or unusable name becomes
/// `"localhost"`, so the host file is never `host-.toml`.
pub fn sanitize_hostname(h: &str) -> String {
    let s: String = h
        .trim()
        .split('.')
        .next()
        .unwrap_or("")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.trim_matches('-').is_empty() {
        "localhost".into()
    } else {
        s
    }
}

/// All files of one config directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub dir: PathBuf,
    pub config_toml: PathBuf,
    pub config_d: PathBuf,
    pub themes: PathBuf,
    pub keymap: PathBuf,
    pub layouts: PathBuf,
    pub rules_d: PathBuf,
}

impl ConfigPaths {
    pub fn new(dir: &Path) -> Self {
        ConfigPaths {
            dir: dir.to_path_buf(),
            config_toml: dir.join("config.toml"),
            config_d: dir.join("config.d"),
            themes: dir.join("themes"),
            keymap: dir.join("keymap.toml"),
            layouts: dir.join("layouts"),
            rules_d: dir.join("rules.d"),
        }
    }

    pub fn default_user() -> Self {
        Self::new(&config_dir())
    }

    /// `config.d/host-<hostname>.toml`.
    pub fn host_file(&self, hostname: &str) -> PathBuf {
        self.config_d.join(format!("host-{hostname}.toml"))
    }

    /// `config.d/<name>.toml` (`.toml` appended when missing).
    pub fn dropin(&self, name: &str) -> PathBuf {
        let file = if name.ends_with(".toml") {
            name.to_string()
        } else {
            format!("{name}.toml")
        };
        self.config_d.join(file)
    }

    /// `themes/<name>.toml`.
    pub fn theme_file(&self, name: &str) -> PathBuf {
        self.themes.join(format!("{name}.toml"))
    }

    /// `layouts/<name>.toml`.
    pub fn layout_file(&self, name: &str) -> PathBuf {
        self.layouts.join(format!("{name}.toml"))
    }

    /// What the live reloader watches: the whole config directory when it exists (covers every file above,
    /// including ones created later); otherwise the nearest existing ancestor, so creating the directory is
    /// noticed too.
    pub fn watch_paths(&self) -> Vec<PathBuf> {
        if self.dir.exists() {
            return vec![self.dir.clone()];
        }
        let mut cur = self.dir.parent();
        while let Some(p) = cur {
            if p.exists() {
                return vec![p.to_path_buf()];
            }
            cur = p.parent();
        }
        Vec::new()
    }
}

/// Sorted `*.toml` files of a directory (missing directory → empty).
pub fn toml_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "toml"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_rules() {
        assert_eq!(
            xdg_from(Some("/x/cfg".into()), ".config"),
            PathBuf::from("/x/cfg/oomtop")
        );
        // relative and empty values are ignored (XDG spec)
        assert_eq!(
            xdg_from(Some("rel".into()), ".config"),
            home().join(".config/oomtop")
        );
        assert_eq!(
            xdg_from(Some("".into()), ".local/state"),
            home().join(".local/state/oomtop")
        );
        assert_eq!(xdg_from(None, ".config"), home().join(".config/oomtop"));
    }

    #[test]
    fn layout_of_files() {
        let p = ConfigPaths::new(Path::new("/c/oomtop"));
        assert_eq!(
            p.host_file("air"),
            PathBuf::from("/c/oomtop/config.d/host-air.toml")
        );
        assert_eq!(p.dropin("50-x"), PathBuf::from("/c/oomtop/config.d/50-x.toml"));
        assert_eq!(
            p.dropin("50-x.toml"),
            PathBuf::from("/c/oomtop/config.d/50-x.toml")
        );
        assert_eq!(
            p.theme_file("ember"),
            PathBuf::from("/c/oomtop/themes/ember.toml")
        );
        assert_eq!(sanitize_hostname("Alexs-MacBook-Air.local"), "Alexs-MacBook-Air");
        assert_eq!(sanitize_hostname("we ird/host"), "we-ird-host");
        assert_eq!(sanitize_hostname(""), "localhost");
        assert_eq!(sanitize_hostname(".local"), "localhost");
        assert_eq!(sanitize_hostname("//"), "localhost");
        assert_eq!(expand_tilde("~/x"), home().join("x"));
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }

    #[test]
    fn watch_paths_fall_back_to_ancestor() {
        let d = tempfile::tempdir().unwrap();
        let p = ConfigPaths::new(&d.path().join("a/b/oomtop"));
        assert_eq!(p.watch_paths(), vec![d.path().to_path_buf()]);
        std::fs::create_dir_all(&p.dir).unwrap();
        assert_eq!(p.watch_paths(), vec![p.dir.clone()]);
        std::fs::write(p.dir.join("b.toml"), "").unwrap();
        std::fs::write(p.dir.join("a.toml"), "").unwrap();
        std::fs::write(p.dir.join("c.txt"), "").unwrap();
        assert_eq!(
            toml_files(&p.dir),
            vec![p.dir.join("a.toml"), p.dir.join("b.toml")]
        );
    }
}

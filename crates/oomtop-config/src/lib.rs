//! # oomtop-config
//!
//! Everything under `~/.config/oomtop/` (UX §12), so `oomtop-core` stays pure:
//!
//! | Module | UX | What |
//! |---|---|---|
//! | [`paths`] | §12.2 | XDG paths on both OSes (config, state, `/etc/oomtop`) |
//! | [`layered`] | §12.1.3 | precedence defaults → /etc → config.toml → config.d → host file → env → flags → runtime, origins (file:line) |
//! | [`watch`] | §12.2 | live reload (debounced 200 ms), last good config kept on invalid edits |
//! | [`write`] | §12.8 | comment-preserving `toml_edit` writes to a chosen layer |
//! | [`template`], [`docs`], [`schema`] | §12.9 | `config init` template, per-setting docs, JSON Schemas for every file |
//! | [`theme`], [`import`] | §12.3–12.4 | semantic tokens, built-in themes, contrast guard, base16/iTerm2/Ghostty/Kitty/Alacritty import |
//! | [`keymap`] | §7, §12.7 | presets default/vim/emacs/htop, remapping, conflict detection |
//! | [`layout`], [`expr`] | §12.6 | layouts, columns, sandboxed custom-column expressions |
//! | [`views`] | §12.7 | aliases and saved views |
//! | [`validate`] | §12.9 | `config validate` over all files |

pub mod docs;
pub mod expr;
pub mod import;
pub mod keymap;
pub mod layered;
pub mod layout;
pub mod model;
pub mod paths;
pub mod schema;
pub mod template;
pub mod theme;
pub mod validate;
pub mod views;
pub mod watch;
pub mod write;

pub use layered::{load_layered, EffectiveEntry, LoadOptions, Loaded, Origin};
pub use model::Config;
pub use template::{init_template, json_schema};
pub use watch::LiveConfig;
pub use write::{set_value, Layer};

use std::path::{Path, PathBuf};

/// A config problem with a location when known ("file:line: message").
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}{}", location(.path, .line), .message)]
pub struct ConfigError {
    pub path: Option<PathBuf>,
    pub line: Option<usize>,
    pub message: String,
}

fn location(path: &Option<PathBuf>, line: &Option<usize>) -> String {
    match (path, line) {
        (Some(p), Some(l)) => format!("{}:{l}: ", p.display()),
        (Some(p), None) => format!("{}: ", p.display()),
        _ => String::new(),
    }
}

impl ConfigError {
    pub fn io(path: &Path, e: &std::io::Error) -> Self {
        ConfigError {
            path: Some(path.to_path_buf()),
            line: None,
            message: e.to_string(),
        }
    }
}

/// Loads with the real environment (`OOMTOP_CONFIG` honored).
pub fn load_default() -> Loaded {
    let opts = LoadOptions {
        config_path: std::env::var_os("OOMTOP_CONFIG").map(PathBuf::from),
        ..Default::default()
    };
    load_layered(&opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let e = ConfigError {
            path: Some("/x/config.toml".into()),
            line: Some(3),
            message: "bad".into(),
        };
        assert_eq!(e.to_string(), "/x/config.toml:3: bad");
        let e = ConfigError {
            path: None,
            line: None,
            message: "bad".into(),
        };
        assert_eq!(e.to_string(), "bad");
    }

    #[test]
    fn defaults_map_to_core() {
        let c = Config::default();
        assert_eq!(
            c.headroom_config(),
            oomtop_core::headroom::HeadroomConfig::default()
        );
        assert_eq!(c.idle_config(), oomtop_core::idle::IdleConfig::default());
        assert_eq!(
            c.personalization.weights.to_core(),
            oomtop_core::ranking::RankWeights::default()
        );
        assert_eq!(c.appearance.theme, "terminal");
        assert_eq!(c.serve.listen, "127.0.0.1:9469");
    }
}

//! Terminal capability detection (UX §12.10): once at start, from the environment. Honors `NO_COLOR`,
//! `CLICOLOR_FORCE`, `TERM=dumb` (→ plain mode), `COLORTERM`, tmux/screen and SSH. Runtime queries that need
//! the terminal (Kitty keyboard protocol) are done by the event loop and stored in `kitty_keyboard`.

use crate::style::ColorDepth;
use oomtop_config::model::ColorMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermCaps {
    pub depth: ColorDepth,
    pub dumb: bool,
    pub no_color: bool,
    pub tmux: bool,
    pub term: String,
    pub colorterm: String,
    /// Synchronized output (DEC 2026). Unknown terminals ignore the sequence, so it is on unless `dumb`.
    pub sync_output: bool,
    /// `CLICOLOR_FORCE` set (color even when stdout is not a TTY; only affects `--plain` callers).
    pub clicolor_force: bool,
    /// Over SSH: no assumptions about local fonts or clipboard.
    pub ssh: bool,
    /// UTF-8 locale; without it the ASCII glyph set is used.
    pub unicode: bool,
    /// OSC 8 hyperlinks are known to work (iTerm2, WezTerm, kitty, Ghostty, VTE, Windows Terminal).
    pub osc8: bool,
    /// SGR mouse reporting (every terminal crossterm supports, except `dumb`).
    pub mouse: bool,
    /// Kitty keyboard protocol (set by the loop after querying the terminal).
    pub kitty_keyboard: bool,
    /// `TERM_PROGRAM` (for `oomtop doctor`).
    pub term_program: String,
}

/// Detects from explicit environment pairs (testable).
pub fn detect_from(env: &[(String, String)], mode: ColorMode) -> TermCaps {
    let get = |k: &str| env.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    let term = get("TERM").unwrap_or_default();
    let colorterm = get("COLORTERM").unwrap_or_default();
    let term_program = get("TERM_PROGRAM").unwrap_or_default();
    let no_color = get("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false);
    let clicolor_force = get("CLICOLOR_FORCE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    let dumb = term == "dumb";
    let tmux = get("TMUX").is_some() || term.starts_with("tmux") || term.starts_with("screen");
    let ssh = get("SSH_CONNECTION").is_some() || get("SSH_TTY").is_some();
    let locale = get("LC_ALL")
        .filter(|v| !v.is_empty())
        .or_else(|| get("LC_CTYPE").filter(|v| !v.is_empty()))
        .or_else(|| get("LANG"))
        .unwrap_or_default()
        .to_ascii_lowercase();
    // No locale at all (common in minimal SSH sessions): assume UTF-8, as every modern terminal is.
    let unicode = locale.is_empty() || locale.contains("utf-8") || locale.contains("utf8");
    let osc8 = matches!(
        term_program.as_str(),
        "iTerm.app" | "WezTerm" | "ghostty" | "vscode" | "Hyper"
    ) || term.contains("kitty")
        || term.contains("ghostty")
        || get("VTE_VERSION").is_some()
        || get("WT_SESSION").is_some();
    let auto = if no_color || dumb {
        ColorDepth::None
    } else if colorterm == "truecolor" || colorterm == "24bit" {
        ColorDepth::Truecolor
    } else if term.contains("256color") || term.contains("kitty") || term.contains("ghostty") {
        ColorDepth::Ansi256
    } else {
        ColorDepth::Ansi16
    };
    let depth = match mode {
        ColorMode::Auto => auto,
        ColorMode::Truecolor => ColorDepth::Truecolor,
        ColorMode::C256 => ColorDepth::Ansi256,
        ColorMode::C16 => ColorDepth::Ansi16,
        ColorMode::None => ColorDepth::None,
    };
    // NO_COLOR always wins (UX §12.1.5).
    let depth = if no_color { ColorDepth::None } else { depth };
    TermCaps {
        depth,
        dumb,
        no_color,
        tmux,
        term,
        colorterm,
        // Terminal.app ignores DEC mode 2026 (harmless to send, but don't advertise it in `doctor`).
        sync_output: !dumb && term_program != "Apple_Terminal",
        clicolor_force,
        ssh,
        unicode,
        osc8,
        mouse: !dumb,
        kitty_keyboard: false,
        term_program,
    }
}

/// Environment keys read by [`detect`].
pub const ENV_KEYS: &[&str] = &[
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "TMUX",
    "CLICOLOR_FORCE",
    "SSH_CONNECTION",
    "SSH_TTY",
    "LC_ALL",
    "LC_CTYPE",
    "LANG",
    "TERM_PROGRAM",
    "VTE_VERSION",
    "WT_SESSION",
];

/// Detects from the process environment.
pub fn detect(mode: ColorMode) -> TermCaps {
    let env: Vec<(String, String)> = ENV_KEYS
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    detect_from(&env, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(p: &[(&str, &str)]) -> Vec<(String, String)> {
        p.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn detection() {
        assert_eq!(
            detect_from(&e(&[("COLORTERM", "truecolor")]), ColorMode::Auto).depth,
            ColorDepth::Truecolor
        );
        assert_eq!(
            detect_from(&e(&[("TERM", "xterm-256color")]), ColorMode::Auto).depth,
            ColorDepth::Ansi256
        );
        assert_eq!(
            detect_from(
                &e(&[("TERM", "xterm-256color"), ("NO_COLOR", "1")]),
                ColorMode::Truecolor
            )
            .depth,
            ColorDepth::None
        );
        let d = detect_from(&e(&[("TERM", "dumb")]), ColorMode::Auto);
        assert!(d.dumb && !d.sync_output && !d.mouse);
        assert_eq!(d.depth, ColorDepth::None);
        assert!(detect_from(&e(&[("TERM", "tmux-256color")]), ColorMode::Auto).tmux);
        assert_eq!(
            detect_from(&e(&[("TERM", "xterm")]), ColorMode::C16).depth,
            ColorDepth::Ansi16
        );
    }

    #[test]
    fn locale_ssh_and_links() {
        let d = detect_from(&e(&[("LANG", "C"), ("SSH_TTY", "/dev/ttys001")]), ColorMode::Auto);
        assert!(!d.unicode && d.ssh);
        let d = detect_from(&e(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "C")]), ColorMode::Auto);
        assert!(d.unicode);
        assert!(detect_from(&e(&[("TERM_PROGRAM", "iTerm.app")]), ColorMode::Auto).osc8);
        assert!(!detect_from(&e(&[("TERM_PROGRAM", "Apple_Terminal")]), ColorMode::Auto).osc8);
        assert!(!detect_from(&e(&[("TERM_PROGRAM", "Apple_Terminal")]), ColorMode::Auto).sync_output);
        assert!(detect_from(&e(&[("TERM_PROGRAM", "ghostty")]), ColorMode::Auto).sync_output);
        assert!(detect_from(&e(&[("CLICOLOR_FORCE", "1")]), ColorMode::Auto).clicolor_force);
    }
}

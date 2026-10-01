//! `oomtop theme list | preview | check | import | export` (UX §12.3–12.4, §12.9).

use super::{out, usage, Ctx};
use crate::ansi::paint;
use crate::ThemeCmd;
use anyhow::{anyhow, bail, Context, Result};
use oomtop_config::model::{AppearanceMode, Background};
use oomtop_config::theme::{
    check_named, is_builtin_name, list_themes, load_theme_for, theme_to_toml, Style, Theme, Variant, TOKENS,
    TRUECOLOR_THEMES,
};
use oomtop_core::can_fit::EXIT_ERROR;
use oomtop_tui::style::ColorDepth;

/// The variant to use: `--variant`, then `appearance.appearance`, then the terminal's background (OSC 11),
/// then dark.
pub fn resolve_variant(ctx: &Ctx, flag: Option<&str>) -> Variant {
    resolve_variant_with(ctx, flag, None)
}

/// [`resolve_variant`] with an already-probed OSC 11 background (`appearance = "auto"`: OSC 11, then
/// `$COLORFGBG`, then dark — UX §12.4).
pub fn resolve_variant_with(ctx: &Ctx, flag: Option<&str>, background: Option<(u8, u8, u8)>) -> Variant {
    if let Some(v) = flag.and_then(Variant::parse) {
        return v;
    }
    let mode = ctx.config().appearance.appearance;
    let background = match (mode, background) {
        (AppearanceMode::Auto, None) => crate::termprobe::probe().and_then(|p| p.background),
        (_, b) => b,
    };
    Variant::resolve(mode, background, std::env::var("COLORFGBG").ok().as_deref())
}

fn depth(ctx: &Ctx) -> ColorDepth {
    oomtop_tui::caps::detect(ctx.config().appearance.color).depth
}

fn attrs(s: &Style) -> String {
    let mut v = Vec::new();
    for (on, a) in [
        (s.bold, "bold"),
        (s.dim, "dim"),
        (s.italic, "italic"),
        (s.underline, "underline"),
        (s.reverse, "reverse"),
    ] {
        if on {
            v.push(a);
        }
    }
    v.join(" ")
}

fn sample_for(token: &str) -> &'static str {
    match token.split('.').next().unwrap_or("") {
        "mem" => "████████",
        "chart" => "▁▂▃▅▇█▇▅",
        "state" => "● status",
        "kind" => "▌ group",
        _ => "Sample text",
    }
}

/// The preview: a mock headline + memory bar, then one line per token.
pub fn preview_text(t: &Theme, depth: ColorDepth, paint_bg: bool) -> String {
    let st = |k: &str| t.style(k);
    let p = |text: &str, k: &str| paint(text, &st(k), depth, paint_bg);
    let mut o = String::new();
    o.push_str(&format!(
        "theme {}{}{} · {}\n\n",
        t.name,
        t.inherits
            .as_ref()
            .map(|i| format!(" (inherits {i})"))
            .unwrap_or_default(),
        t.appearance
            .as_ref()
            .map(|a| format!(" · {a}"))
            .unwrap_or_default(),
        depth.as_str()
    ));
    o.push_str(&format!(
        "  {}\n",
        p(
            "Tight: 1.2G headroom. Idle Gradle daemons hold 6.1G — press r to reclaim.",
            "text.headline"
        )
    ));
    o.push_str(&format!(
        "  {}{}{}{}{}{}  {} {}\n",
        p("██████████", "mem.app"),
        p("████████", "mem.gpu"),
        p("███", "mem.compressed"),
        p("██", "mem.wired"),
        p("███", "mem.cache"),
        p("░░░░", "mem.free"),
        p("21.4", "text.number"),
        p("GiB of 24 GiB", "text.unit"),
    ));
    o.push_str(&format!(
        "  {} {}  {} {}  {} {}  {}  {}\n\n",
        p("●", "state.ok"),
        p("ok", "ui.muted"),
        p("▲", "state.warn"),
        p("warn", "ui.muted"),
        p("■", "state.crit"),
        p("crit", "ui.muted"),
        p("[r] reclaim", "text.key"),
        p("docs ↗", "text.link"),
    ));
    let mut section = "";
    for tok in TOKENS {
        let sec = tok.split('.').next().unwrap_or("");
        if sec != section {
            o.push_str(&format!("[{sec}]\n"));
            section = sec;
        }
        let s = st(tok);
        o.push_str(&format!(
            "  {:<20} {}  fg={:<14} bg={:<10} {}\n",
            tok,
            paint(sample_for(tok), &s, depth, paint_bg),
            s.fg.clone().unwrap_or_else(|| "-".into()),
            s.bg.clone().unwrap_or_else(|| "-".into()),
            attrs(&s)
        ));
    }
    o
}

pub fn run(ctx: &Ctx, cmd: ThemeCmd) -> Result<i32> {
    let paths = ctx.paths();
    let dir = paths.themes.clone();
    match cmd {
        ThemeCmd::List => {
            let current = &ctx.config().appearance.theme;
            for t in list_themes(Some(&dir)) {
                let user = dir.join(format!("{t}.toml")).is_file();
                let mut tags = Vec::new();
                if t == *current {
                    tags.push("current");
                }
                if user {
                    tags.push(if is_builtin_name(&t) {
                        "user override"
                    } else {
                        "user"
                    });
                } else {
                    tags.push("built-in");
                }
                if TRUECOLOR_THEMES.contains(&t.as_str()) {
                    tags.push("light/dark truecolor");
                } else if t == "terminal" {
                    tags.push("16 ANSI, no background");
                } else if t == "none" {
                    tags.push("monochrome");
                }
                out(&format!("{t:<18} {}\n", tags.join(" · ")));
            }
            Ok(0)
        }
        ThemeCmd::Preview { name, variant } => {
            let name = name.unwrap_or_else(|| ctx.config().appearance.theme.clone());
            let v = resolve_variant(ctx, variant.as_deref());
            let t = load_theme_for(&name, Some(&dir), v).map_err(|e| anyhow!("{e}"))?;
            let paint_bg = ctx.config().appearance.background == Background::Theme && t.name != "terminal";
            let d = if t.name == "none" {
                ColorDepth::None
            } else {
                depth(ctx)
            };
            out(&preview_text(&t, d, paint_bg));
            Ok(0)
        }
        ThemeCmd::Check { name, variant } => {
            let variants: Vec<Variant> = match variant.as_deref().and_then(Variant::parse) {
                Some(v) => vec![v],
                None if TRUECOLOR_THEMES.contains(&name.as_str())
                    || dir.join(format!("{name}.toml")).is_file() =>
                {
                    vec![Variant::Dark, Variant::Light]
                }
                None => vec![Variant::Dark],
            };
            let mut bad = false;
            for v in variants {
                let (t, issues) = check_named(&name, Some(&dir), v).map_err(|e| anyhow!("{e}"))?;
                if issues.is_empty() {
                    out(&format!("{} ({}): ok\n", t.name, v.as_str()));
                } else {
                    bad = true;
                    for i in issues {
                        out(&format!(
                            "{} ({}): {}: {}\n",
                            t.name,
                            v.as_str(),
                            i.token,
                            i.message
                        ));
                    }
                }
            }
            Ok(if bad { EXIT_ERROR } else { 0 })
        }
        ThemeCmd::Import {
            file,
            name,
            force,
            print,
        } => {
            let mut imp = oomtop_config::import::import_file(&file).map_err(|e| anyhow!("{e}"))?;
            if let Some(n) = name {
                let slug = oomtop_config::import::theme_slug(&n);
                if slug.is_empty() {
                    return usage(format!("--name {n:?} has no usable characters"));
                }
                imp.theme.name = slug;
            }
            let text = theme_to_toml(&imp.theme);
            for n in &imp.nudged {
                eprintln!("oomtop: adjusted {}: {}", n.token, n.message);
            }
            if print {
                out(&text);
                return Ok(0);
            }
            let target = paths.theme_file(&imp.theme.name);
            if target.exists() && !force {
                bail!(
                    "{} exists (use --force to overwrite, or --name to pick another name)",
                    target.display()
                );
            }
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            oomtop_config::write::write_atomic(&target, &text).map_err(|e| anyhow!("{e}"))?;
            out(&format!(
                "imported {} ({}) → {}\nuse it: oomtop config set appearance.theme {}\n",
                imp.theme.name,
                imp.format.as_str(),
                target.display(),
                imp.theme.name
            ));
            Ok(0)
        }
        ThemeCmd::Export {
            name,
            variant,
            output,
        } => {
            let v = variant.as_deref().and_then(Variant::parse).unwrap_or_default();
            let t = load_theme_for(&name, Some(&dir), v).map_err(|e| anyhow!("{e}"))?;
            let text = format!(
                "# exported by `oomtop theme export {name}` ({} variant): every token, references resolved\n{}",
                v.as_str(),
                theme_to_toml(&t)
            );
            match output {
                Some(p) => {
                    oomtop_config::write::write_atomic(&p, &text).map_err(|e| anyhow!("{e}"))?;
                    out(&format!("wrote {}\n", p.display()));
                }
                None => out(&text),
            }
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every SGR parameter list in `s`.
    fn sgr_params(s: &str) -> Vec<Vec<u32>> {
        s.split("\x1b[")
            .skip(1)
            .filter_map(|chunk| chunk.split_once('m'))
            .map(|(p, _)| p.split(';').filter_map(|x| x.parse().ok()).collect())
            .collect()
    }

    fn paints_background(params: &[u32]) -> bool {
        // Skip 38;5;n / 38;2;r;g;b payloads, then look for 40-49 / 100-107.
        let mut i = 0;
        while i < params.len() {
            match params[i] {
                38 => i += if params.get(i + 1) == Some(&5) { 3 } else { 5 },
                40..=49 | 100..=107 => return true,
                _ => i += 1,
            }
        }
        false
    }

    #[test]
    fn terminal_preview_never_paints_background() {
        let t = oomtop_config::theme::load_theme("terminal", None).unwrap();
        for d in [ColorDepth::Ansi16, ColorDepth::Ansi256, ColorDepth::Truecolor] {
            let s = preview_text(&t, d, false);
            let all = sgr_params(&s);
            assert!(!all.is_empty(), "the preview is colored");
            assert!(all.iter().all(|p| !paints_background(p)), "{all:?}");
        }
        let none = oomtop_config::theme::load_theme("none", None).unwrap();
        let s = preview_text(&none, ColorDepth::None, false);
        assert!(
            sgr_params(&s)
                .iter()
                .flatten()
                .all(|p| !(30..=49).contains(p) && !(90..=107).contains(p)),
            "none has no colors"
        );
    }
}

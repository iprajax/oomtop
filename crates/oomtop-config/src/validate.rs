//! `oomtop config validate` (UX §12.9): schema-checks **all** files — config layers, themes, keymap, layouts,
//! rules, aliases and saved views — and reports every problem with file:line and a fix hint.

use crate::layered::{line_of, load_layered, LoadOptions, Loaded};
use crate::paths::toml_files;
use crate::ConfigError;
use std::path::Path;

/// Result of a full validation.
#[derive(Debug, Clone)]
pub struct Report {
    pub loaded: Loaded,
    /// Every file that was looked at.
    pub checked: Vec<std::path::PathBuf>,
    pub errors: Vec<ConfigError>,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Validates a rules file (`rules.d/*.toml`) against the core rule format and compiles its globs/regexes.
pub fn validate_rules_text(text: &str, path: &Path) -> Vec<ConfigError> {
    let err = |line, message: String| ConfigError {
        path: Some(path.to_path_buf()),
        line,
        message,
    };
    let set: oomtop_core::attribution::RuleSet = match toml::from_str(text) {
        Ok(s) => s,
        Err(e) => {
            let m = e.message().trim().to_string();
            let m = match crate::layered::hint_for(&m) {
                Some(h) => format!("{m} — {h}"),
                None => m,
            };
            return vec![err(e.span().map(|s| line_of(text, s.start)), m)];
        }
    };
    let lines = crate::layered::key_lines(text);
    match oomtop_core::attribution::CompiledRules::compile(&set) {
        Ok(_) => Vec::new(),
        Err(e) => {
            // point at the offending rule when we can tell which one
            let msg = e.to_string();
            let line = set
                .rules
                .iter()
                .enumerate()
                .find(|(_, r)| {
                    let id =
                        r.id.clone()
                            .unwrap_or_else(|| oomtop_core::attribution::slug(&r.label));
                    msg.contains(&format!("{id:?}")) || msg.contains(&format!("{:?}", r.label))
                })
                .and_then(|(i, _)| lines.get(&format!("group.{i}")).copied());
            vec![err(line, msg)]
        }
    }
}

/// Validates everything under the resolved config directory.
pub fn validate_all(opts: &LoadOptions) -> Report {
    let loaded = load_layered(opts);
    let paths = opts.paths();
    let mut errors = loaded.errors.clone();
    let mut checked = loaded.files.clone();

    // themes
    for f in toml_files(&paths.themes) {
        checked.push(f.clone());
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        match crate::theme::parse_theme(&text, &f.display().to_string()) {
            Err(e) => errors.push(ConfigError {
                path: Some(f.clone()),
                line: None,
                message: e.to_string(),
            }),
            Ok(t) => {
                let lines = crate::layered::key_lines(&text);
                // user files: structure only (contrast is nudged at load and shown by `theme check`)
                for issue in crate::theme::check_structure(&t, false) {
                    errors.push(ConfigError {
                        path: Some(f.clone()),
                        line: lines.get(&issue.token).copied().or_else(|| {
                            // quoted dotted token keys: [ui] "border.focus"
                            issue
                                .token
                                .split_once('.')
                                .and_then(|(sec, rest)| lines.get(&format!("{sec}.{rest}")).copied())
                        }),
                        message: format!("{}: {}", issue.token, issue.message),
                    });
                }
                if let Some(parent) = &t.inherits {
                    let name = f
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if let Err(e) = crate::theme::load_theme(&name, Some(&paths.themes)) {
                        errors.push(ConfigError {
                            path: Some(f.clone()),
                            line: lines.get("inherits").copied(),
                            message: format!("inherits {parent:?}: {e}"),
                        });
                    }
                }
            }
        }
    }
    // the selected theme must exist
    let theme = &loaded.config.appearance.theme;
    if let Err(e) = crate::theme::load_theme(theme, Some(&paths.themes)) {
        errors.push(origin_error(&loaded, "appearance.theme", e.to_string()));
    }

    // keymap
    if paths.keymap.is_file() {
        checked.push(paths.keymap.clone());
    }
    let (km, kerrs) = crate::keymap::load_keymap(
        &loaded.config.keys.preset,
        Some(&paths.keymap).filter(|p| p.is_file()).map(|p| p.as_path()),
    );
    for e in kerrs {
        if e.path.is_none() {
            errors.push(origin_error(&loaded, "keys.preset", e.message));
        } else {
            errors.push(e);
        }
    }
    let km_lines = std::fs::read_to_string(&paths.keymap)
        .map(|t| crate::layered::key_lines(&t))
        .unwrap_or_default();
    for c in km.conflicts() {
        let chord = c
            .actions
            .iter()
            .find_map(|a| a.split('"').nth(1).map(str::to_string));
        let line = km_lines
            .get(&format!("{}.{}", c.context, c.key))
            .or_else(|| chord.and_then(|ch| km_lines.get(&format!("{}.{ch}", c.context))))
            .copied();
        errors.push(ConfigError {
            path: Some(paths.keymap.clone()),
            line,
            message: format!("key conflict {c}"),
        });
    }

    // layouts
    for f in toml_files(&paths.layouts) {
        checked.push(f.clone());
        if let Ok(text) = std::fs::read_to_string(&f) {
            if let Err(es) = crate::layout::parse_layout(&text, &f) {
                errors.extend(es);
            }
        }
    }
    let layout = &loaded.config.layout.name;
    if !layout.is_empty() && !crate::layout::list_layouts(&paths.layouts).contains(layout) {
        errors.push(origin_error(
            &loaded,
            "layout.name",
            format!("no layout {layout:?} in {}", paths.layouts.display()),
        ));
    }

    // rules
    for f in toml_files(&paths.rules_d) {
        checked.push(f.clone());
        if let Ok(text) = std::fs::read_to_string(&f) {
            errors.extend(validate_rules_text(&text, &f));
        }
    }

    // aliases & views
    errors.extend(crate::views::validate(&loaded, Some(&paths.layouts)));

    // settings with extra constraints
    for (key, ok, msg) in [
        (
            "headroom.min_margin",
            oomtop_core::units::parse_bytes(&loaded.config.headroom.min_margin).is_ok(),
            "must be a size like \"1.5G\"",
        ),
        (
            "headroom.margin_override",
            loaded.config.headroom.margin_override.trim().is_empty()
                || oomtop_core::units::parse_bytes(&loaded.config.headroom.margin_override).is_ok(),
            "must be empty or a size like \"2G\"",
        ),
        (
            "keys.preset",
            crate::keymap::PRESETS.contains(&loaded.config.keys.preset.as_str()),
            "must be default, vim, emacs or htop",
        ),
        (
            "serve.listen",
            loaded.config.serve.listen.parse::<std::net::SocketAddr>().is_ok(),
            "must be host:port, e.g. 127.0.0.1:9469",
        ),
        (
            "personalization.half_life_days",
            loaded.config.personalization.half_life_days > 0.0,
            "must be > 0",
        ),
        (
            "general.refresh_ms",
            loaded.config.general.refresh_ms >= 250,
            "must be ≥ 250 ms (perf budget, SPEC §14)",
        ),
        (
            "general.host_refresh_ms",
            loaded.config.general.host_refresh_ms >= 250,
            "must be ≥ 250 ms (perf budget, SPEC §14)",
        ),
        (
            "general.watch_poll_ms",
            loaded.config.general.watch_poll_ms == 0 || loaded.config.general.watch_poll_ms >= 100,
            "must be 0 (native file events) or ≥ 100 ms",
        ),
        (
            "personalization.retention_days",
            loaded.config.personalization.retention_days >= 1,
            "must be ≥ 1 day",
        ),
        (
            "personalization.log_max_mb",
            loaded.config.personalization.log_max_mb >= 1,
            "must be ≥ 1 MB",
        ),
    ] {
        if !ok && !errors.iter().any(|e| e.message.contains(key)) {
            let m = format!("{key}: {msg}");
            if key == "keys.preset" {
                continue; // already reported by load_keymap
            }
            errors.push(origin_error(&loaded, key, m));
        }
    }
    let mut seen = std::collections::HashSet::new();
    errors.retain(|e| seen.insert(e.to_string()));
    Report {
        loaded,
        checked,
        errors,
    }
}

fn origin_error(loaded: &Loaded, key: &str, message: String) -> ConfigError {
    match loaded.origin(key) {
        crate::layered::Origin::File { path, line } => ConfigError {
            path: Some(path),
            line,
            message,
        },
        o => ConfigError {
            path: None,
            line: None,
            message: format!("{message} (set by {o})"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
    fn empty_dir_is_valid() {
        let d = tempfile::tempdir().unwrap();
        let r = validate_all(&opts(d.path()));
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn reports_every_kind_with_locations() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        for sub in ["themes", "layouts", "rules.d"] {
            std::fs::create_dir(p.join(sub)).unwrap();
        }
        std::fs::write(
            p.join("config.toml"),
            "[appearance]\ntheme = \"nightt\"\n[serve]\nlisten = \"localhost\"\n[layout]\nname = \"gone\"\n",
        )
        .unwrap();
        std::fs::write(p.join("themes/x.toml"), "[ui]\naccent = \"#3\"\n").unwrap();
        std::fs::write(
            p.join("keymap.toml"),
            "[global]\n\"x\" = \"quit\"\n\"q\" = \"quit\"\n\"g g\" = \"move:top\"\n",
        )
        .unwrap();
        std::fs::write(p.join("layouts/l.toml"), "[wide]\nrows = [\"nope\"]\n").unwrap();
        std::fs::write(
            p.join("rules.d/10-mine.toml"),
            "[[group]]\nkind = \"model\"\nlabel = \"S\"\nmatch.cmdline = [\"(unclosed\"]\n",
        )
        .unwrap();
        std::fs::write(
            p.join("rules.d/20-bad.toml"),
            "[[group]]\nkind = \"modle\"\nlabel = \"S\"\n",
        )
        .unwrap();
        let r = validate_all(&opts(p));
        let msgs: Vec<String> = r.errors.iter().map(|e| e.to_string()).collect();
        let has = |s: &str| msgs.iter().any(|m| m.contains(s));
        assert!(has("config.toml:2: theme \"nightt\" not found"), "{msgs:#?}");
        assert!(has("config.toml:4: serve.listen: must be host:port"), "{msgs:#?}");
        assert!(has("config.toml:6: no layout \"gone\""), "{msgs:#?}");
        assert!(has("x.toml:2: ui.accent: invalid color \"#3\""), "{msgs:#?}");
        assert!(has("keymap.toml:4: key conflict [global] \"g\""), "{msgs:#?}");
        assert!(has("l.toml:2: unknown slot \"nope\""), "{msgs:#?}");
        assert!(has("10-mine.toml:1: rule \"s\": bad regex"), "{msgs:#?}");
        assert!(has("20-bad.toml:2: unknown variant `modle`"), "{msgs:#?}");
        assert!(r.checked.len() >= 5);
    }

    #[test]
    fn low_contrast_user_theme_is_not_a_validation_error() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("themes")).unwrap();
        std::fs::write(
            d.path().join("themes/pale.toml"),
            "inherits = \"mono-light\"\n[ui]\ntext = \"#dddddd\"\naccent = \"#3b82f6\"\n",
        )
        .unwrap();
        std::fs::write(d.path().join("config.toml"), "[appearance]\ntheme = \"pale\"\n").unwrap();
        let r = validate_all(&opts(d.path()));
        assert!(r.is_ok(), "{:?}", r.errors);
    }

    #[test]
    fn numeric_limits_are_checked() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            "[general]\nhost_refresh_ms = 10\nwatch_poll_ms = 5\n[personalization]\nretention_days = 0\nlog_max_mb = 0\n",
        )
        .unwrap();
        let r = validate_all(&opts(d.path()));
        let msgs: Vec<String> = r.errors.iter().map(|e| e.to_string()).collect();
        for want in [
            "config.toml:2: general.host_refresh_ms",
            "config.toml:3: general.watch_poll_ms",
            "config.toml:5: personalization.retention_days",
            "config.toml:6: personalization.log_max_mb",
        ] {
            assert!(msgs.iter().any(|m| m.contains(want)), "{want}: {msgs:#?}");
        }
    }

    #[test]
    fn rules_text_ok() {
        let ok = "[[group]]\nkind = \"agent_session\"\nlabel = \"Claude Code\"\nmatch.exe = [\"claude\"]\nsession_key = \"env:CLAUDE_CODE_SESSION_ID\"\n";
        assert!(validate_rules_text(ok, Path::new("r.toml")).is_empty());
    }
}

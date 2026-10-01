//! UX §11 acceptance tests owned by oomtop-config: 9 (live reload), 10 (comment-preserving save),
//! 11 (base16 import passes theme check), 12 (no key conflicts in the default/vim/htop presets).

use oomtop_config::layered::{LoadOptions, Origin};
use oomtop_config::paths::ConfigPaths;
use oomtop_config::watch::{LiveConfig, WatchMode};
use oomtop_config::write::{set_in_layer, Layer};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn opts(dir: &Path) -> LoadOptions {
    LoadOptions {
        config_dir: Some(dir.to_path_buf()),
        system_dir: Some(PathBuf::new()),
        hostname: Some("air".into()),
        env: Some(vec![]),
        ..Default::default()
    }
}

/// Waits until `cond` holds for the current config. Condition-based (not "generation moved"), so an extra
/// event burst from an earlier write can never satisfy the wait before the edit under test is applied.
fn wait_until(
    live: &LiveConfig,
    cap: Duration,
    cond: impl Fn(&oomtop_config::Loaded) -> bool,
) -> Option<Duration> {
    let start = Instant::now();
    while start.elapsed() < cap {
        if cond(&live.current()) {
            return Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

fn live_reload(mode: WatchMode, cap: Duration) {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("oomtop");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("config.toml");
    std::fs::write(&file, "# mine\n[appearance]\ntheme = \"mono\"\n").unwrap();
    let live = LiveConfig::start(opts(&dir), mode, true).unwrap();
    assert_eq!(live.current().config.appearance.theme, "mono");
    std::thread::sleep(Duration::from_millis(300)); // let the watcher settle

    // a valid edit applies within 1 s
    std::fs::write(
        &file,
        "# mine\n[appearance]\ntheme = \"ember\"\ndensity = \"compact\"\n",
    )
    .unwrap();
    let took = wait_until(&live, cap, |l| {
        l.config.appearance.theme == "ember"
            && l.config.appearance.density == oomtop_config::model::Density::Compact
    })
    .expect("reload after a valid edit");
    assert!(took < Duration::from_secs(1), "applied in {took:?}");
    let cur = live.current();
    assert_eq!(cur.config.appearance.theme, "ember");
    assert!(cur.errors.is_empty());
    assert_eq!(cur.origin("appearance.density").to_string(), "config.toml:4");

    // an invalid edit shows file:line and keeps the old config
    std::fs::write(
        &file,
        "# mine\n[appearance]\ntheme = \"ember\"\ndensity = \"roomy\"\n",
    )
    .unwrap();
    wait_until(&live, cap, |l| !l.errors.is_empty()).expect("reload after an invalid edit");
    let cur = live.current();
    assert_eq!(cur.config.appearance.theme, "ember");
    assert_eq!(
        cur.config.appearance.density,
        oomtop_config::model::Density::Compact,
        "last good value kept"
    );
    assert_eq!(cur.errors.len(), 1, "{:?}", cur.errors);
    let e = cur.errors[0].to_string();
    assert!(e.contains("config.toml:4: unknown variant `roomy`"), "{e}");
    assert!(e.contains("keeping the last good version"), "{e}");

    // fixing it clears the error; a new drop-in is picked up too (both writes may land in separate bursts)
    std::fs::create_dir_all(dir.join("config.d")).unwrap();
    std::fs::write(dir.join("config.d/50-x.toml"), "[format]\ndecimals = 2\n").unwrap();
    std::fs::write(&file, "# mine\n[appearance]\ntheme = \"mint\"\n").unwrap();
    wait_until(&live, cap, |l| {
        l.config.appearance.theme == "mint" && l.config.format.decimals == 2 && l.errors.is_empty()
    })
    .expect("reload after the fix");
    let cur = live.current();
    assert_eq!(cur.config.appearance.theme, "mint");
    assert_eq!(cur.config.format.decimals, 2);
    assert!(cur.errors.is_empty(), "{:?}", cur.errors);
}

#[test]
fn ux11_test9_live_reload_poll() {
    live_reload(
        WatchMode::Poll(Duration::from_millis(100)),
        Duration::from_secs(3),
    );
}

#[test]
fn ux11_test9_live_reload_native() {
    live_reload(WatchMode::Native, Duration::from_secs(3));
}

#[test]
fn runtime_toggles_survive_reload_and_are_not_persisted() {
    let d = tempfile::tempdir().unwrap();
    let file = d.path().join("config.toml");
    std::fs::write(&file, "[appearance]\ntheme = \"mono\"\n").unwrap();
    let live = LiveConfig::start(opts(d.path()), WatchMode::Native, false).unwrap();
    live.set_runtime("appearance.theme", "coral").unwrap();
    std::fs::write(&file, "[appearance]\ntheme = \"sand\"\ndensity = \"compact\"\n").unwrap();
    live.reload_now();
    let cur = live.current();
    assert_eq!(cur.config.appearance.theme, "coral");
    assert_eq!(cur.origin("appearance.theme"), Origin::Runtime);
    assert_eq!(
        cur.config.appearance.density,
        oomtop_config::model::Density::Compact
    );
    assert!(!std::fs::read_to_string(&file).unwrap().contains("coral"));
}

const USER_CONFIG: &str = r#"# ~/.config/oomtop/config.toml — my settings
# (kept in dotfiles; please don't reformat)

[general]
refresh_ms = 2000        # 2 s is plenty on battery

# --- look -------------------------------------------------------------
[appearance]
theme      = "terminal"  # follow Ghostty
density    = "comfortable"
header     = ["headline", "memory", "swap"]   # no accelerators row

[headroom]
min_margin = "2G"        # fanless Air: be conservative

[aliases]
llm = "kind:model,agent" # my LLM stuff

[[views]]
name  = "gpu-work"
query = "gpu>500M or llm"
"#;

#[test]
fn ux11_test10_settings_save_preserves_comments_and_order() {
    let d = tempfile::tempdir().unwrap();
    let paths = ConfigPaths::new(d.path());
    std::fs::write(&paths.config_toml, USER_CONFIG).unwrap();
    // what the settings screen does on `s` (save to the chosen layer)
    for (key, value) in [
        ("appearance.theme", "ember"),
        ("appearance.density", "compact"),
        ("headroom.min_margin", "2.5G"),
        ("personalization.weights.affinity", "0.8"),
        ("aliases.hogs", ":sort footprint desc"),
        ("appearance.color", "256"),
    ] {
        set_in_layer(&paths, "air", &Layer::User, key, value).unwrap();
    }
    let out = std::fs::read_to_string(&paths.config_toml).unwrap();
    // every original comment survives, in order
    let comments = |t: &str| -> Vec<String> {
        t.lines()
            .filter_map(|l| l.find('#').map(|i| l[i..].to_string()))
            .collect()
    };
    let orig = comments(USER_CONFIG);
    let now = comments(&out);
    let mut it = now.iter();
    for c in &orig {
        assert!(it.any(|n| n == c), "comment {c:?} lost or reordered:\n{out}");
    }
    // key order kept: theme before density before header
    let pos = |needle: &str| {
        out.find(needle)
            .unwrap_or_else(|| panic!("{needle} missing:\n{out}"))
    };
    assert!(pos("theme      = \"ember\"") < pos("density    = \"compact\""));
    assert!(pos("density    = \"compact\"") < pos("header"));
    assert!(pos("[general]") < pos("[appearance]") && pos("[appearance]") < pos("[headroom]"));
    // and the file still loads with the new values
    let l = oomtop_config::load_layered(&opts(d.path()));
    assert!(l.errors.is_empty(), "{:?}", l.errors);
    assert_eq!(l.config.appearance.theme, "ember");
    assert_eq!(l.config.personalization.weights.affinity, 0.8);
    assert_eq!(l.config.aliases["hogs"], ":sort footprint desc");
    insta::assert_snapshot!("settings_save", out);

    // saving to the host layer leaves config.toml untouched
    let before = std::fs::read_to_string(&paths.config_toml).unwrap();
    let host = set_in_layer(&paths, "air", &Layer::Host, "general.refresh_ms", "5000").unwrap();
    assert_eq!(std::fs::read_to_string(&paths.config_toml).unwrap(), before);
    let l = oomtop_config::load_layered(&opts(d.path()));
    assert_eq!(l.config.general.refresh_ms, 5000);
    assert_eq!(l.origin("general.refresh_ms").to_string(), "host-air.toml:2");
    assert_eq!(l.set_by("general.refresh_ms").len(), 2, "overridden elsewhere");
    assert!(host.ends_with("config.d/host-air.toml"));
}

const BASE16_EIGHTIES: &str = r#"scheme: "Eighties"
author: "Chris Kempson (http://chriskempson.com)"
base00: "2d2d2d"
base01: "393939"
base02: "515151"
base03: "747369"
base04: "a09f93"
base05: "d3d0c8"
base06: "e8e6df"
base07: "f2f0ec"
base08: "f2777a"
base09: "f99157"
base0A: "ffcc66"
base0B: "99cc99"
base0C: "66cccc"
base0D: "6699cc"
base0E: "cc99cc"
base0F: "d27b53"
"#;

const BASE16_ONE_LIGHT: &str = r##"system: "base16"
name: "One Light"
author: "Daniel Pfeifer (http://github.com/purpleKarrot)"
variant: "light"
palette:
  base00: "#fafafa"
  base01: "#f0f0f1"
  base02: "#e5e5e6"
  base03: "#a0a1a7"
  base04: "#696c77"
  base05: "#383a42"
  base06: "#202227"
  base07: "#090a0b"
  base08: "#ca1243"
  base09: "#d75f00"
  base0A: "#c18401"
  base0B: "#50a14f"
  base0C: "#0184bc"
  base0D: "#4078f2"
  base0E: "#a626a4"
  base0F: "#986801"
"##;

#[test]
fn ux11_test11_base16_import_passes_theme_check() {
    let d = tempfile::tempdir().unwrap();
    let themes = d.path().join("themes");
    std::fs::create_dir_all(&themes).unwrap();
    for (file, text) in [
        ("eighties.yaml", BASE16_EIGHTIES),
        ("one-light.yaml", BASE16_ONE_LIGHT),
    ] {
        let src = d.path().join(file);
        std::fs::write(&src, text).unwrap();
        // `oomtop theme import <file>` = import_file + write themes/<name>.toml
        let imp = oomtop_config::import::import_file(&src).unwrap();
        let out = themes.join(format!("{}.toml", imp.theme.name));
        std::fs::write(&out, oomtop_config::theme::theme_to_toml(&imp.theme)).unwrap();
        // `oomtop theme check <name>`
        for v in [
            oomtop_config::theme::Variant::Dark,
            oomtop_config::theme::Variant::Light,
        ] {
            let (theme, issues) =
                oomtop_config::theme::check_named(&imp.theme.name, Some(&themes), v).unwrap();
            assert!(issues.is_empty(), "{file}: {issues:?}");
            assert_eq!(theme.tokens.len(), oomtop_config::theme::TOKENS.len());
        }
        // and `oomtop config validate` is clean
        let r = oomtop_config::validate::validate_all(&opts(d.path()));
        assert!(r.is_ok(), "{file}: {:?}", r.errors);
    }
}

#[test]
fn ux11_test12_presets_have_no_conflicts() {
    for p in ["default", "vim", "htop", "emacs"] {
        let (km, errs) = oomtop_config::keymap::load_keymap(p, None);
        assert!(errs.is_empty());
        let c = km.conflicts();
        assert!(c.is_empty(), "{p}: {c:?}");
    }
}

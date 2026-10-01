//! Render snapshots (UX §8, CLAUDE.md "snapshot tests at 60/80/120/200 columns and in 16-color + none modes")
//! plus rendering invariants: the `terminal` theme never paints a background, no blue/purple, `none` mode has
//! no colors at all, no emoji, and the UX layer stays within its frame budget on a 700-process snapshot.
//!
//! Snapshot format: each screen line, followed by a style line where every cell carries a letter for its style
//! (`.` = unstyled); the legend at the bottom maps letters to styles. That keeps colors and attributes
//! reviewable in plain text.

use oomtop_config::keymap::{preset, Keymap};
use oomtop_config::theme::terminal_theme;
use oomtop_config::Config;
use oomtop_core::history::History;
use oomtop_tui::app::{App, RowKey};
use oomtop_tui::fixtures;
use oomtop_tui::render::{draw_with, Glyphs, ASCII, UNICODE};
use oomtop_tui::style::{ColorDepth, Palette};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::Terminal;
use std::collections::BTreeMap;

fn km() -> Keymap {
    preset("default").unwrap()
}

fn app_for(s: oomtop_core::Snapshot, w: u16, h: u16) -> App {
    let mut app = App::new(&Config::default());
    app.set_protect(fixtures::protect());
    app.area = Rect::new(0, 0, w, h);
    let hist = fixtures::history_for(&s, 30);
    app.update(s, &hist);
    app
}

fn press(app: &mut App, keys: &[&str]) {
    let k = km();
    for key in keys {
        let _ = app.on_key(key, &k);
    }
}

fn render(app: &App, w: u16, h: u16, depth: ColorDepth, g: &Glyphs) -> Buffer {
    let p = Palette::new(terminal_theme(), depth, false);
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| draw_with(f, app, &p, g, oomtop_config::model::Borders::Rounded))
        .unwrap();
    t.backend().buffer().clone()
}

fn style_name(s: Style) -> String {
    let mut parts = Vec::new();
    if let Some(fg) = s.fg {
        parts.push(format!("fg:{fg:?}"));
    }
    if let Some(bg) = s.bg {
        parts.push(format!("bg:{bg:?}"));
    }
    let m = s.add_modifier;
    for (flag, name) in [
        (Modifier::BOLD, "bold"),
        (Modifier::DIM, "dim"),
        (Modifier::ITALIC, "italic"),
        (Modifier::UNDERLINED, "underline"),
        (Modifier::REVERSED, "reverse"),
    ] {
        if m.contains(flag) {
            parts.push(name.to_string());
        }
    }
    parts.join(" ")
}

/// Text grid + per-cell style letters + legend.
fn styled(buf: &Buffer) -> String {
    let mut legend: BTreeMap<String, char> = BTreeMap::new();
    let letters: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        .chars()
        .collect();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut text = String::new();
        let mut codes = String::new();
        let mut skip = 0usize;
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = cell.symbol();
            text.push_str(sym);
            let w = unicode_width::UnicodeWidthStr::width(sym).max(1);
            skip = w - 1;
            let st = Style::default()
                .fg(cell.fg)
                .bg(cell.bg)
                .add_modifier(cell.modifier);
            let st = Style {
                fg: (cell.fg != Color::Reset).then_some(cell.fg),
                bg: (cell.bg != Color::Reset).then_some(cell.bg),
                ..st
            };
            let name = style_name(st);
            let code = if name.is_empty() {
                '.'
            } else {
                let n = legend.len();
                *legend.entry(name).or_insert_with(|| letters[n % letters.len()])
            };
            for _ in 0..w {
                codes.push(code);
            }
        }
        out.push_str(text.trim_end());
        out.push('\n');
        let trimmed = codes.trim_end_matches('.');
        out.push_str(&format!("{trimmed}\n"));
    }
    out.push_str("--- legend\n");
    let mut inv: Vec<(char, String)> = legend.into_iter().map(|(k, v)| (v, k)).collect();
    inv.sort();
    for (c, name) in inv {
        out.push_str(&format!("{c} = {name}\n"));
    }
    out
}

fn text(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// Code points with a default emoji presentation (their width varies between terminals, UX §8).
fn is_emoji(ch: char) -> bool {
    let c = ch as u32;
    (0x1F000..=0x1FAFF).contains(&c)
        || [
            0x231A, 0x231B, 0x23E9, 0x23EA, 0x23EB, 0x23EC, 0x23F0, 0x23F3, 0x25FD, 0x25FE, 0x2614, 0x2615,
            0x267F, 0x2693, 0x26A1, 0x26AA, 0x26AB, 0x26BD, 0x26BE, 0x26C4, 0x26C5, 0x26CE, 0x26D4, 0x26EA,
            0x26F2, 0x26F3, 0x26F5, 0x26FA, 0x26FD, 0x2705, 0x270A, 0x270B, 0x2728, 0x274C, 0x274E, 0x2753,
            0x2754, 0x2755, 0x2757, 0x2795, 0x2796, 0x2797, 0x27B0, 0x27BF, 0x2B1B, 0x2B1C, 0x2B50, 0x2B55,
        ]
        .contains(&c)
        || (0x2648..=0x2653).contains(&c)
        || c == 0xFE0F
}

/// Invariants that hold for every frame.
fn check_invariants(buf: &Buffer, depth: ColorDepth) {
    for cell in buf.content() {
        assert!(
            cell.bg == Color::Reset,
            "terminal theme must never paint a background: {:?} at {:?}",
            cell.bg,
            cell.symbol()
        );
        assert!(
            !matches!(
                cell.fg,
                Color::Blue | Color::Magenta | Color::LightBlue | Color::LightMagenta
            ),
            "no blue/purple defaults: {:?} on {:?}",
            cell.fg,
            cell.symbol()
        );
        if depth == ColorDepth::None {
            assert_eq!(
                cell.fg,
                Color::Reset,
                "none mode has no colors: {:?}",
                cell.symbol()
            );
        }
        for ch in cell.symbol().chars() {
            assert!(!is_emoji(ch), "no emoji: {ch:?}");
            assert!(!ch.is_control(), "control character in a cell: {ch:?}");
        }
    }
}

fn snap(name: &str, app: &App, w: u16, h: u16, depth: ColorDepth, g: &Glyphs) {
    let buf = render(app, w, h, depth, g);
    check_invariants(&buf, depth);
    insta::assert_snapshot!(name, styled(&buf));
}

fn size_for(w: u16) -> u16 {
    match w {
        60 => 24,
        80 => 30,
        120 => 36,
        _ => 50,
    }
}

#[test]
fn home_at_all_widths_in_16_color_and_none() {
    for w in [60u16, 80, 120, 200] {
        let h = size_for(w);
        for (depth, tag) in [(ColorDepth::Ansi16, "16"), (ColorDepth::None, "none")] {
            let app = app_for(fixtures::motivating(), w, h);
            snap(&format!("home_{w}_{tag}"), &app, w, h, depth, &UNICODE);
        }
    }
}

#[test]
fn home_ascii_60() {
    let mut app = app_for(fixtures::motivating(), 60, 24);
    app.ascii = true;
    snap("home_60_ascii", &app, 60, 24, ColorDepth::None, &ASCII);
    let t = text(&render(&app, 60, 24, ColorDepth::None, &ASCII));
    assert!(t.is_ascii(), "--ascii renders ASCII only:\n{t}");
}

#[test]
fn calm_machine_80() {
    let app = app_for(fixtures::calm(), 80, 30);
    assert!(app.headline.text.starts_with("All good"), "{}", app.headline.text);
    snap("calm_80_16", &app, 80, 30, ColorDepth::Ansi16, &UNICODE);
}

/// Throttle mode moves thermals up (accelerator/power row right under the headline) and changes the headline.
#[test]
fn throttle_mode_wide() {
    let mut s = fixtures::calm();
    s.thermal.pressure = oomtop_core::Measured::exact(oomtop_core::ThermalPressure::Heavy, "notify");
    s.thermal.throttle_factor = oomtop_core::Measured::exact(0.45, "ioreport");
    let app = app_for(s, 160, 36);
    assert_eq!(app.mode, oomtop_core::modes::Mode::Throttle);
    assert!(
        app.headline.text.starts_with("Running at 45% speed"),
        "{}",
        app.headline.text
    );
    let t = text(&render(&app, 160, 36, ColorDepth::Ansi16, &UNICODE));
    let lines: Vec<&str> = t.lines().collect();
    // Meter block first (htop), then the headline, then — in Throttle mode — the accelerator/power row.
    let head = lines
        .iter()
        .position(|l| l.contains("Running at 45% speed"))
        .expect(&t);
    assert!(
        lines[head + 1].trim_start().starts_with("GPU "),
        "accelerators row moved up under the headline:\n{t}"
    );
    snap("throttle_160_16", &app, 160, 36, ColorDepth::Ansi16, &UNICODE);
}

#[test]
fn views_at_80_and_120() {
    for w in [80u16, 120] {
        let h = size_for(w);
        for (key, name) in [
            ("2", "processes"),
            ("3", "models"),
            ("4", "sandboxes"),
            ("5", "reclaim"),
        ] {
            let mut app = app_for(fixtures::motivating(), w, h);
            press(&mut app, &[key]);
            snap(
                &format!("{name}_{w}_16"),
                &app,
                w,
                h,
                ColorDepth::Ansi16,
                &UNICODE,
            );
        }
    }
}

fn timeline_app(w: u16, h: u16) -> App {
    let mut app = app_for(fixtures::motivating(), w, h);
    let mut hist = fixtures::history_for(&fixtures::motivating(), 30);
    for i in 1..=40u64 {
        let mut s = fixtures::motivating();
        s.taken_at_ms += i * 2000;
        let grow = (i * 30) << 20;
        s.memory.swap_used.value = Some((5u64 << 30) + grow);
        s.memory.available.value = s.memory.available.value.map(|v| v.saturating_sub(grow / 2));
        if i < 20 {
            s.groups.retain(|g| g.id != "daemon:kotlin");
        }
        hist.push_snapshot(&s);
        app.update(s, &hist);
    }
    app
}

#[test]
fn timeline_live_and_scrubbed() {
    let mut app = timeline_app(120, 36);
    press(&mut app, &["6"]);
    assert!(app
        .timeline
        .markers
        .iter()
        .any(|m| m.text == "KotlinCompileDaemon started"));
    assert!(app.timeline.markers.iter().any(|m| m.text.starts_with("swap +")));
    snap("timeline_120_live", &app, 120, 36, ColorDepth::Ansi16, &UNICODE);
    press(&mut app, &["pageup"]);
    assert_eq!(app.timeline.scrub, Some(10));
    snap(
        "timeline_120_scrubbed",
        &app,
        120,
        36,
        ColorDepth::Ansi16,
        &UNICODE,
    );
    // ranked list as it was: Kotlin had not started yet 30 frames ago
    assert!(!app
        .rows
        .iter()
        .any(|r| r.key == RowKey::Past("daemon:kotlin".into())));
}

#[test]
fn overlays_and_modes() {
    let w = 120;
    let h = 36;
    let base = || app_for(fixtures::motivating(), w, h);
    let mut app = base();
    press(&mut app, &["?"]);
    snap("help_120", &app, w, h, ColorDepth::Ansi16, &UNICODE);

    let mut app = base();
    let i = app
        .rows
        .iter()
        .position(|r| r.key == RowKey::Group("daemon:gradle".into()))
        .unwrap();
    for _ in 0..i {
        press(&mut app, &["j"]);
    }
    press(&mut app, &["i"]);
    snap("why_rank_120", &app, w, h, ColorDepth::Ansi16, &UNICODE);
    press(&mut app, &["esc", "x"]);
    snap("confirm_inline_120", &app, w, h, ColorDepth::Ansi16, &UNICODE);
    press(&mut app, &["n", "w"]);
    snap("focus_gradle_120", &app, w, h, ColorDepth::Ansi16, &UNICODE);
    press(&mut app, &["w", "c", "k", "c"]);
    snap("compare_120", &app, w, h, ColorDepth::Ansi16, &UNICODE);

    let mut app = base();
    press(&mut app, &[":", "w", "h", "y", "enter"]);
    snap("why_slow_120", &app, w, h, ColorDepth::Ansi16, &UNICODE);

    let mut app = base();
    press(&mut app, &[":"]);
    for ch in "headroom 13G".chars() {
        let k = if ch == ' ' {
            "space".to_string()
        } else {
            ch.to_string()
        };
        press(&mut app, &[&k]);
    }
    press(&mut app, &["enter"]);
    snap("headroom_answer_120", &app, w, h, ColorDepth::Ansi16, &UNICODE);

    let mut app = base();
    press(&mut app, &["ctrl-k", "c", "l"]);
    snap("palette_120", &app, w, h, ColorDepth::Ansi16, &UNICODE);

    let mut app = base();
    press(
        &mut app,
        &["/", "k", "i", "n", "d", ":", "d", "a", "e", "m", "o", "n"],
    );
    snap("filter_typing_120", &app, w, h, ColorDepth::Ansi16, &UNICODE);
}

#[test]
fn details_overlay_in_standard_layout() {
    let mut app = app_for(fixtures::motivating(), 80, 30);
    press(&mut app, &["enter", "enter"]);
    assert_eq!(app.overlay, Some(oomtop_tui::app::Overlay::Details));
    snap("details_overlay_80", &app, 80, 30, ColorDepth::Ansi16, &UNICODE);
}

#[test]
fn expanded_members_wide() {
    let mut app = app_for(fixtures::motivating(), 200, 50);
    let i = app
        .rows
        .iter()
        .position(|r| r.key == RowKey::Group("app:google-chrome".into()))
        .unwrap();
    for _ in 0..i {
        press(&mut app, &["j"]);
    }
    press(&mut app, &["enter", "p"]);
    snap("expanded_pinned_200", &app, 200, 50, ColorDepth::Ansi16, &UNICODE);
}

#[test]
fn settings_screen() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(
        d.path().join("config.toml"),
        "# mine\n[appearance]\ndensity = \"compact\"\n[general]\nrefresh_ms = 3000\n",
    )
    .unwrap();
    let opts = oomtop_config::LoadOptions {
        config_dir: Some(d.path().to_path_buf()),
        system_dir: Some(std::path::PathBuf::new()),
        hostname: Some("air".into()),
        env: Some(vec![]),
        ..Default::default()
    };
    let loaded = oomtop_config::load_layered(&opts);
    let paths = oomtop_config::paths::ConfigPaths::new(std::path::Path::new("/home/me/.config/oomtop"));
    let mut model = oomtop_tui::settings_model(&loaded, &[], &paths, &paths.config_toml, "air");
    // the temp dir path must not leak into the golden
    for r in model.rows.iter_mut() {
        r.also_in.clear();
    }
    for w in [80u16, 140] {
        let h = 32;
        let mut app = app_for(fixtures::motivating(), w, h);
        app.show_settings(model.clone());
        let theme_row = app
            .settings
            .as_ref()
            .unwrap()
            .model
            .rows
            .iter()
            .position(|r| r.key == "appearance.theme")
            .unwrap();
        app.settings.as_mut().unwrap().selected = theme_row;
        press(&mut app, &["right"]);
        snap(&format!("settings_{w}"), &app, w, h, ColorDepth::Ansi16, &UNICODE);
    }
}

#[test]
fn every_width_from_60_to_300_renders_without_overflow() {
    let app = app_for(fixtures::motivating(), 60, 24);
    for w in (60u16..=300).step_by(7) {
        let mut a = app_for(fixtures::motivating(), w, 30);
        a.area = Rect::new(0, 0, w, 30);
        let buf = render(&a, w, 30, ColorDepth::Ansi16, &UNICODE);
        check_invariants(&buf, ColorDepth::Ansi16);
        let t = text(&buf);
        assert!(t.contains("oomtop"), "{w}");
        assert!(t.lines().count() == 30);
    }
    // tiny windows degrade to a message instead of panicking
    let buf = render(&app, 15, 4, ColorDepth::None, &UNICODE);
    assert!(text(&buf).contains("too small"));
}

/// UX §8 "units always shown": at standard widths the Models / Sandboxes tables drop whole low-priority
/// columns (endpoint, KV, device, runtime …) instead of clipping a value mid-number; MEM / HOST always stay.
#[test]
fn models_and_sandboxes_drop_whole_columns_never_clip_values() {
    let clipped = |tok: &str| {
        tok.starts_with(|c: char| c.is_ascii_digit()) && (tok.ends_with('.') || tok.ends_with('…'))
    };
    for w in (80u16..=139).step_by(3) {
        let h = size_for(w);
        let mut app = app_for(fixtures::motivating(), w, h);
        press(&mut app, &["3"]);
        let t = text(&render(&app, w, h, ColorDepth::None, &UNICODE));
        let header = t
            .lines()
            .find(|l| l.contains("KIND") && l.contains("MODELS"))
            .unwrap_or_else(|| panic!("{w}: no models header\n{t}"));
        assert!(header.trim_end().ends_with("MEM"), "{w}: {header}");
        let row = t.lines().find(|l| l.contains("sdcpp")).unwrap();
        let last = row.split_whitespace().last().unwrap();
        assert!(
            last.ends_with('G') || last.ends_with('M') || last == "n/a",
            "{w}: {row}"
        );
        assert!(!row.split_whitespace().any(clipped), "{w}: {row}");

        let mut app = app_for(fixtures::motivating(), w, h);
        press(&mut app, &["4"]);
        let t = text(&render(&app, w, h, ColorDepth::None, &UNICODE));
        let header = t
            .lines()
            .find(|l| l.contains("LABEL") && l.contains("HOST"))
            .unwrap_or_else(|| panic!("{w}: no sandboxes header\n{t}"));
        assert!(
            header.contains("GUEST") && header.contains("STARTED BY"),
            "{w}: {header}"
        );
        for row in t.lines().filter(|l| l.contains("vm") || l.contains("container")) {
            assert!(!row.split_whitespace().any(clipped), "{w}: {row}");
        }
    }
}

/// UX acceptance #6: with NO_COLOR at 80 columns every state is still distinguishable (glyph + word).
#[test]
fn no_color_states_are_distinguishable() {
    let app = app_for(fixtures::motivating(), 80, 30);
    let t = text(&render(&app, 80, 30, ColorDepth::None, &UNICODE));
    for needle in [
        "▲ Pressure",
        "▸",
        "idle 3h40m",
        "● reclaimable",
        "orphan",
        "≥1.5G",
        "protected",
    ] {
        assert!(t.contains(needle), "missing {needle:?} in:\n{t}");
    }
}

/// SPEC §14: UX layer ≤ 5 ms per frame (update = query + ranking; draw = layout + render) on ~700 processes.
#[test]
fn render_budget_700_processes() {
    let s = fixtures::large(700);
    let hist = fixtures::history_for(&s, 300);
    let mut app = App::new(&Config::default());
    app.set_protect(fixtures::protect());
    app.area = Rect::new(0, 0, 200, 50);
    app.update(s.clone(), &hist);
    let p = Palette::new(terminal_theme(), ColorDepth::Ansi16, false);
    let mut t = Terminal::new(TestBackend::new(200, 50)).unwrap();
    let n = 40u32;
    let mut update_total = std::time::Duration::ZERO;
    let mut draw_total = std::time::Duration::ZERO;
    for i in 0..n {
        let mut snap = s.clone();
        snap.taken_at_ms += (i as u64 + 1) * 2000;
        let t0 = std::time::Instant::now();
        app.update(snap, &hist);
        update_total += t0.elapsed();
        let t1 = std::time::Instant::now();
        t.draw(|f| draw_with(f, &app, &p, &UNICODE, oomtop_config::model::Borders::Rounded))
            .unwrap();
        draw_total += t1.elapsed();
    }
    // processes view (700 rows sorted) too
    press(&mut app, &["2"]);
    let t2 = std::time::Instant::now();
    for _ in 0..n {
        t.draw(|f| draw_with(f, &app, &p, &UNICODE, oomtop_config::model::Borders::Rounded))
            .unwrap();
    }
    let proc_draw = t2.elapsed() / n;
    // a named layout with custom expression columns and a split (UX §12.6)
    press(&mut app, &["1"]);
    app.set_layout(Some(llm_dev()));
    let t3 = std::time::Instant::now();
    for _ in 0..n {
        t.draw(|f| draw_with(f, &app, &p, &UNICODE, oomtop_config::model::Borders::Rounded))
            .unwrap();
    }
    let layout_draw = t3.elapsed() / n;
    let upd = update_total / n;
    let drw = draw_total / n;
    eprintln!(
        "700 processes, {} groups, 300 history points: update {:?}/refresh, draw {:?}/frame (home), {:?}/frame (processes), {:?}/frame (llm-dev layout){}",
        app.snapshot.groups.len(),
        upd,
        drw,
        proc_draw,
        layout_draw,
        if cfg!(debug_assertions) { " [debug build]" } else { " [release build]" }
    );
    let budget = if cfg!(debug_assertions) {
        std::time::Duration::from_millis(60)
    } else {
        std::time::Duration::from_millis(5)
    };
    assert!(drw < budget, "home draw {drw:?} ≥ {budget:?}");
    assert!(proc_draw < budget, "processes draw {proc_draw:?} ≥ {budget:?}");
    assert!(layout_draw < budget, "layout draw {layout_draw:?} ≥ {budget:?}");
    assert!(upd + drw < budget * 2, "update+draw {:?}", upd + drw);
}

#[test]
fn vim_chord_g_g() {
    let mut app = app_for(fixtures::motivating(), 120, 36);
    let vim = preset("vim").unwrap();
    for k in ["j", "j", "j"] {
        app.on_key(k, &vim);
    }
    assert_eq!(app.selected, 3);
    app.on_key("g", &vim);
    assert_eq!(app.selected, 3, "g alone waits for the chord");
    assert!(app.pending_chord.is_some());
    app.on_key("g", &vim);
    assert_eq!(app.selected, 0);
    app.on_key("G", &vim);
    assert_eq!(app.selected, app.rows.len() - 1);
    // unfinished chord + other key → dropped
    app.on_key("g", &vim);
    app.on_key("x", &vim);
    assert!(app.confirm.is_none() && app.pending_chord.is_none());
    let _ = History::default();
}

const LLM_DEV: &str = r#"
name = "llm-dev"
[wide]
rows = ["header", "your-things", { split = ["ranked:65%", "details:35%"] }]
[standard]
rows = ["headline", "cards", "ranked", "timeline"]
[columns.groups]
show  = ["name", "footprint", "gpu", "trend", "state", "mem_share", "because"]
width = { name = "flex", footprint = 9, gpu = 8, trend = 12 }
sort  = "footprint"
[columns.processes]
show = ["pid", "name", "footprint", "cpu", "cmdline"]
[[columns.custom]]
id = "mem_share"
title = "MEM%"
expr = "footprint / host.mem.total * 100"
format = "{:.0}%"
"#;

fn llm_dev() -> oomtop_tui::columns::ActiveLayout {
    let l = oomtop_config::layout::parse_layout(LLM_DEV, std::path::Path::new("llm-dev.toml")).unwrap();
    oomtop_tui::columns::ActiveLayout::new(l).unwrap()
}

/// UX §12.6: a named layout drives the Home slots (split ranked/details in wide, headline-only header and a
/// timeline slot in standard), the group/process columns and their titles, and custom expression columns.
#[test]
fn custom_layout_wide_and_standard() {
    let mut app = app_for(fixtures::motivating(), 160, 40);
    app.set_layout(Some(llm_dev()));
    let t = text(&render(&app, 160, 40, ColorDepth::Ansi16, &UNICODE));
    assert!(t.contains("MEM%"), "custom column title:\n{t}");
    assert!(t.contains("41%"), "sd-server share of RAM:\n{t}");
    assert!(t.contains("─ Details"), "details in the split:\n{t}");
    let ranked_line = t.lines().find(|l| l.contains("─ Ranked")).unwrap();
    assert!(ranked_line.contains("─ Details"), "side by side:\n{ranked_line}");
    snap("layout_llm_dev_160", &app, 160, 40, ColorDepth::Ansi16, &UNICODE);

    let mut app = app_for(fixtures::motivating(), 100, 30);
    app.set_layout(Some(llm_dev()));
    let t = text(&render(&app, 100, 30, ColorDepth::None, &UNICODE));
    assert!(!t.contains(" MEM ▕"), "standard rows have no meters:\n{t}");
    assert!(t.contains("Tight on memory"), "headline kept:\n{t}");
    assert!(
        t.contains("─ Timeline") && t.contains("AVAIL"),
        "timeline slot:\n{t}"
    );
    snap(
        "layout_llm_dev_100_none",
        &app,
        100,
        30,
        ColorDepth::None,
        &UNICODE,
    );

    // processes view with custom columns: command lines are redacted on screen
    let mut s = fixtures::motivating();
    if let Some(p) = s.processes.iter_mut().find(|p| p.id.pid == 4242) {
        p.cmdline.push("--api-key=sk-live-123456".into());
    }
    let mut app = app_for(s, 120, 30);
    app.set_layout(Some(llm_dev()));
    press(&mut app, &["2"]);
    let t = text(&render(&app, 120, 30, ColorDepth::Ansi16, &UNICODE));
    assert!(t.contains("CMDLINE"), "{t}");
    assert!(!t.contains("sk-live-123456"), "secrets never on screen:\n{t}");
}

/// `--ascii` guarantees an ASCII-only screen, the settings screen included.
#[test]
fn ascii_settings_screen_is_ascii_only() {
    let d = tempfile::tempdir().unwrap();
    let opts = oomtop_config::LoadOptions {
        config_dir: Some(d.path().to_path_buf()),
        system_dir: Some(std::path::PathBuf::new()),
        hostname: Some("air".into()),
        env: Some(vec![]),
        ..Default::default()
    };
    let loaded = oomtop_config::load_layered(&opts);
    let paths = oomtop_config::paths::ConfigPaths::new(d.path());
    let model = oomtop_tui::settings_model(&loaded, &[], &paths, &paths.config_toml, "air");
    let mut app = app_for(fixtures::motivating(), 140, 30);
    app.show_settings(model);
    let buf = render(&app, 140, 30, ColorDepth::None, &ASCII);
    for cell in buf.content() {
        assert!(cell.symbol().is_ascii(), "non-ASCII {:?}", cell.symbol());
    }
}

/// Throttle mode in the standard layout gets its own temps/power/clock row (UX §3), without duplicating the
/// GPU/CPU figures on the swap line.
#[test]
fn throttle_mode_standard_shows_thermal_row() {
    let mut s = fixtures::calm();
    s.thermal.pressure = oomtop_core::Measured::exact(oomtop_core::ThermalPressure::Heavy, "notify");
    s.thermal.throttle_factor = oomtop_core::Measured::exact(0.45, "ioreport");
    let app = app_for(s, 120, 36);
    let t = text(&render(&app, 120, 36, ColorDepth::Ansi16, &UNICODE));
    let lines: Vec<&str> = t.lines().collect();
    // The accelerator/thermal row ("GPU 58% …", not the "GPU[…]" meter) sits right under the headline.
    let row = lines
        .iter()
        .position(|l| l.trim_start().starts_with("GPU "))
        .expect(&t);
    let head = lines.iter().position(|l| l.contains("Running at")).expect(&t);
    assert!(row <= head + 2, "thermal row near the top:\n{t}");
    let swap = lines.iter().find(|l| l.contains("SWAP")).unwrap();
    assert!(!swap.contains("GPU"), "no duplicate GPU on the swap line: {swap}");
}

/// Footer and details hints come from the active keymap (htop preset without `x`: stop is `k`).
#[test]
fn footer_hints_follow_the_keymap() {
    let mut app = app_for(fixtures::motivating(), 200, 50);
    let mut km = preset("htop").unwrap();
    for binds in km.contexts.values_mut() {
        binds.retain(|(k, _)| k != "x");
    }
    app.set_keymap(&km);
    let t = text(&render(&app, 200, 50, ColorDepth::None, &UNICODE));
    let footer = t.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
    assert!(footer.starts_with("F1Help"), "htop F-key bar: {footer}");
    assert!(footer.contains("F9Stop"), "{footer}");
    assert!(t.contains("[k] stop"), "details hints:\n{t}");
    // Remapping stays truthful: F5 → watch shows "Watch", an unbound F7 shows no label.
    let mut km = preset("default").unwrap();
    let g = km.contexts.get_mut("global").unwrap();
    g.retain(|(k, _)| k != "F5" && k != "F7");
    g.push(("F5".into(), "watch".into()));
    app.set_keymap(&km);
    let t = text(&render(&app, 200, 50, ColorDepth::None, &UNICODE));
    let footer = t.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
    assert!(footer.contains("F5Watch"), "{footer}");
    assert!(!footer.contains("Why"), "{footer}");
    assert!(
        footer.contains("F7       F8"),
        "unbound F7 keeps its slot: {footer}"
    );
}

/// The htop function-key bar at every width and in every preset: always on the last line, F1 first and F10
/// Quit last, labels shortened (never clipped mid-word) and low-priority keys dropped on narrow terminals.
#[test]
fn function_key_bar_per_width_and_preset() {
    use unicode_width::UnicodeWidthStr;
    for p in oomtop_config::keymap::PRESETS {
        for (w, expect) in [
            (
                200,
                "F1Help   F2Setup  F3Search F4Filter F5Tree   F6SortBy F7Why    F8Reclaim F9Stop   F10Quit",
            ),
            (
                120,
                "F1Help   F2Setup  F3Search F4Filter F5Tree   F6SortBy F7Why    F8Reclaim F9Stop   F10Quit",
            ),
            (
                80,
                "F1Help F2Setup F3Search F4Filter F5Tree F6SortBy F7Why F8Reclaim F9Stop F10Quit",
            ),
            (60, "F1Help F2Set F3Find F4Filt F5Tree F6Sort F9Stop F10Quit"),
        ] {
            let mut app = app_for(fixtures::motivating(), w, 30);
            app.set_keymap(&preset(p).unwrap());
            let t = text(&render(&app, w, 30, ColorDepth::None, &UNICODE));
            let last = t.lines().last().unwrap();
            assert_eq!(last.trim_end(), expect, "{p} at {w}");
            assert!(last.width() <= w as usize);
        }
    }
    // Clicking a label runs its action: F8 → Reclaim view.
    let mut app = app_for(fixtures::motivating(), 120, 30);
    let t = text(&render(&app, 120, 30, ColorDepth::None, &UNICODE));
    let last = t.lines().last().unwrap();
    let col = last.find("F8").unwrap() as u16 + 3;
    app.on_mouse(oomtop_tui::app::MouseInput {
        kind: oomtop_tui::app::MouseKind::Down,
        col,
        row: 29,
        at_ms: 1,
    });
    assert_eq!(app.view, oomtop_tui::app::View::Reclaim);
}

/// A sandbox whose host processes report no footprint shows n/a, never 0.
#[test]
fn unmeasured_sandbox_host_is_not_zero() {
    let mut s = fixtures::motivating();
    let pids: Vec<oomtop_core::ProcId> = s.sandboxes[0].host_pids.clone();
    for p in s.processes.iter_mut().filter(|p| pids.contains(&p.id)) {
        p.mem.footprint_or_pss = oomtop_core::Measured::unavailable("t", "needs root");
    }
    let mut app = app_for(s, 120, 30);
    press(&mut app, &["4"]);
    let t = text(&render(&app, 120, 30, ColorDepth::None, &UNICODE));
    let row = t
        .lines()
        .find(|l| l.contains(&app.snapshot.sandboxes[0].label))
        .unwrap();
    assert!(row.contains("n/a") && !row.contains(" 0B"), "{row}");
}

/// Truecolor themes keep a visible selection without painting a background (reverse fallback).
#[test]
fn mono_theme_selection_is_visible_when_transparent() {
    let app = app_for(fixtures::motivating(), 120, 36);
    let mono = oomtop_config::theme::builtin_theme("mono").unwrap();
    let p = Palette::new(mono, ColorDepth::Truecolor, false);
    let mut t = Terminal::new(TestBackend::new(120, 36)).unwrap();
    t.draw(|f| draw_with(f, &app, &p, &UNICODE, oomtop_config::model::Borders::Rounded))
        .unwrap();
    let buf = t.backend().buffer().clone();
    let y = (0..36u16)
        .find(|y| buf[(1u16, *y)].symbol() == "▸")
        .expect("selected row");
    assert!(buf[(10u16, y)].modifier.contains(Modifier::REVERSED));
    assert!(
        buf.content().iter().all(|c| c.bg == Color::Reset),
        "nothing painted"
    );
}

/// Process names, labels and command lines are untrusted: control characters (ESC, BEL, CR…) must never reach
/// the terminal, or a process named "\x1b]52;c;…\x07" could write the clipboard or rewrite the screen.
#[test]
fn control_characters_in_data_never_reach_the_terminal() {
    let evil = "evil\u{1b}]52;c;aGVsbG8=\u{7}\u{1b}[2J\r\u{9b}31m\u{0}name";
    let mut s = fixtures::motivating();
    for p in s.processes.iter_mut().filter(|p| p.id.pid == 4242) {
        p.name = evil.into();
        p.cmdline.push(evil.into());
        p.cwd = Some(format!("/tmp/{evil}"));
    }
    for g in s.groups.iter_mut().filter(|g| g.id == "model:sd-server") {
        g.label = evil.into();
    }
    let mut app = app_for(s, 200, 50);
    let mut views: Vec<Vec<&str>> = vec![vec![], vec!["2"], vec!["3"], vec!["1", "enter"], vec!["w"]];
    views.push(vec!["esc", "enter", "enter"]);
    for keys in views {
        press(&mut app, &keys);
        for depth in [ColorDepth::Ansi16, ColorDepth::None] {
            let buf = render(&app, 200, 50, depth, &UNICODE);
            for cell in buf.content() {
                assert!(
                    !cell.symbol().chars().any(|c| c.is_control()),
                    "control character reached the buffer after {keys:?}: {:?}",
                    cell.symbol()
                );
            }
        }
    }
    let text = oomtop_tui::plain_summary(&app.snapshot, &app.headroom, &app.headline.text);
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "plain output is sanitized too"
    );
}

/// Models view lists weight files on disk under the servers: which one is loaded (matched against the
/// processes' mapped weight files), when the others were last used, duplicates (SPEC §10).
#[test]
fn models_view_shows_files_on_disk() {
    use oomtop_tui::DiskModel;
    let s = fixtures::motivating();
    let now = s.taken_at_ms;
    let mut app = app_for(s, 120, 36);
    app.disk_models = vec![
        DiskModel {
            name: "qwen-image-Q4_K.gguf".into(),
            path: "/Users/user/models/qwen-image-Q4_K.gguf".into(),
            real_path: "/Users/user/models/qwen-image-Q4_K.gguf".into(),
            size: 12 << 30,
            format: "gguf".into(),
            last_used_ms: Some(now),
            duplicate: false,
        },
        DiskModel {
            name: "Qwen3VL-8B-Instruct-Q4_K_M.gguf".into(),
            path: "/Users/user/models/Qwen3VL-8B-Instruct-Q4_K_M.gguf".into(),
            real_path: "/Users/user/models/Qwen3VL-8B-Instruct-Q4_K_M.gguf".into(),
            size: 5 << 30,
            format: "gguf".into(),
            last_used_ms: Some(now - 3 * 3_600_000),
            duplicate: true,
        },
    ];
    press(&mut app, &["3"]);
    let t = text(&render(&app, 120, 36, ColorDepth::Ansi16, &UNICODE));
    assert!(t.contains("On disk"), "{t}");
    assert!(t.contains("2 files"), "{t}");
    assert!(t.contains("loaded"), "the mapped file shows as loaded:\n{t}");
    assert!(t.contains("used 3h ago dup"), "{t}");
    snap("models_disk_120_16", &app, 120, 36, ColorDepth::Ansi16, &UNICODE);
}

/// F5 in Processes: htop-style tree (children under parents with ├─ / └─ branches); F5 again → flat.
/// F6: the SortBy picker lists this view's sort keys and marks the current one; Enter applies.
#[test]
fn processes_tree_and_sort_by_picker() {
    let mut app = app_for(fixtures::motivating(), 120, 40);
    press(&mut app, &["2", "F5"]);
    assert!(app.proc_tree);
    let t = text(&render(&app, 120, 40, ColorDepth::None, &UNICODE));
    assert!(t.contains("├─ ") || t.contains("└─ "), "tree branches:\n{t}");
    assert!(t.contains("tree (F5)"), "{t}");
    snap("processes_tree_120", &app, 120, 40, ColorDepth::Ansi16, &UNICODE);
    // every process still listed exactly once
    assert_eq!(app.rows.len(), app.snapshot.processes.len());
    press(&mut app, &["F5"]);
    assert!(!app.proc_tree);
    press(&mut app, &["F6"]);
    let labels: Vec<&str> = app.palette.iter().map(|s| s.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            ":sort footprint",
            ":sort cpu",
            ":sort resident",
            ":sort pid",
            ":sort name"
        ]
    );
    assert!(app.palette[app.palette_sel].detail.ends_with("(current)"));
    snap("sort_by_picker_120", &app, 120, 40, ColorDepth::Ansi16, &UNICODE);
    press(&mut app, &["down", "enter"]);
    assert_eq!(app.proc_sort, oomtop_tui::app::ProcSort::Cpu);
    // Home: F5 expands every group, again collapses.
    press(&mut app, &["1", "F5"]);
    assert!(app.rows.iter().any(|r| r.depth == 1));
    press(&mut app, &["F5"]);
    assert!(app.rows.iter().all(|r| r.depth == 0));
}

/// Meter block adapts: per-core columns when wide, aggregate CPU bar when per-core data is missing (first
/// frame) or the terminal is narrow, one-row mini meters on very short terminals; P/E tags from core kinds.
#[test]
fn meter_block_adapts_to_width_height_and_data() {
    let wide = text(&render(
        &app_for(fixtures::motivating(), 160, 40),
        160,
        40,
        ColorDepth::None,
        &UNICODE,
    ));
    let first: Vec<&str> = wide.lines().take(4).collect();
    assert!(
        first[0].contains("E0[") && first[0].contains("P9["),
        "4 columns: {first:?}"
    );
    assert!(wide.contains("Tasks: 761, 3412 thr; 3 running"));
    assert!(wide.contains("Load average: 3.20 2.74 2.41"));
    assert!(wide.contains("Uptime: 58 days, 11:23:04"));
    assert!(wide.contains("Mem[") && wide.contains("20.1G/24.0G]"));
    assert!(wide.contains("Swp[") && wide.contains("6.1G/7.0G]"));
    // no per-core data yet → one CPU bar
    let mut s = fixtures::motivating();
    s.cpu.per_core_pct.clear();
    let t = text(&render(&app_for(s, 120, 40), 120, 40, ColorDepth::None, &UNICODE));
    assert!(t.lines().next().unwrap().trim_start().starts_with("CPU["), "{t}");
    // unknown kinds → plain indexes
    let mut s = fixtures::motivating();
    s.cpu.core_kinds.clear();
    let t = text(&render(&app_for(s, 160, 40), 160, 40, ColorDepth::None, &UNICODE));
    assert!(t.lines().next().unwrap().contains("  0["), "{t}");
    // very short terminal → one meter row, the list still gets space
    let t = text(&render(
        &app_for(fixtures::motivating(), 120, 14),
        120,
        14,
        ColorDepth::None,
        &UNICODE,
    ));
    let l0 = t.lines().next().unwrap();
    assert!(
        l0.contains("CPU[") && l0.contains("Mem[") && l0.contains("Swp["),
        "{t}"
    );
    assert!(t.contains("▸"), "list visible:\n{t}");
    // meters can be turned off via appearance.header; the old MEM bar comes back
    let mut cfg = Config::default();
    cfg.appearance.header.retain(|r| r != "meters");
    let mut app = App::new(&cfg);
    app.set_protect(fixtures::protect());
    let s = fixtures::motivating();
    let h = fixtures::history_for(&s, 30);
    app.update(s, &h);
    let t = text(&render(&app, 160, 40, ColorDepth::None, &UNICODE));
    assert!(!t.contains("Mem[") && t.contains(" MEM ▕"), "{t}");
}

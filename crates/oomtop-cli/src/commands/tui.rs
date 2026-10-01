//! Default command: the TUI (SPEC §12.1, UX). Falls back to the plain summary for `--plain`, `TERM=dumb`
//! and non-TTY stdout (handled inside `oomtop_tui::run`).

use super::Ctx;
use crate::actuator::SignalActuator;
use anyhow::Result;
use oomtop_config::keymap::load_keymap;
use oomtop_config::theme::{load_theme_for, none_theme, terminal_theme, Variant};
use oomtop_core::actions::Actuator;
use oomtop_core::provider::SnapshotProvider;

pub fn run(ctx: &Ctx) -> Result<i32> {
    let cfg = ctx.config();
    let paths = ctx.paths();
    // Light/dark only matters for truecolor themes; `terminal` inherits the terminal's own palette.
    let name = cfg.appearance.theme.as_str();
    // One terminal probe (OSC 11 background) shared by the first theme load and later reloads/previews. It
    // runs on a helper thread while the first sample is taken: a terminal that never answers costs its 200 ms
    // cap once, in parallel, not before sampling (first frame < 300 ms, SPEC §14).
    let probe_thread = if ctx.global.plain {
        None
    } else {
        std::thread::Builder::new()
            .name("oomtop-termprobe".into())
            .spawn(crate::termprobe::probe)
            .ok()
    };
    let mut engine = ctx.engine(false)?;
    // First sample fills the protect context; CPU % arrives with the second sample (SPEC §6.2).
    let first = engine.snapshot();
    let probe = probe_thread.and_then(|h| h.join().ok()).flatten();
    let background = probe.as_ref().and_then(|p| p.background);
    // The probe asked `CSI ? u` with a 200 ms cap; its answer (or the lack of one) replaces crossterm's own
    // query, which waits up to 2 s for DA1 on terminals that never answer.
    let kitty_keyboard = probe.as_ref().map(|p| p.kitty_keyboard.is_some());
    // No DA1 within the cap: the answers may still come (slow SSH) — the TUI drops them instead of reading
    // them as keys.
    let late_replies = probe.as_ref().is_some_and(|p| !p.da1);
    let variant = if name == "terminal" || name == "none" || ctx.global.plain {
        Variant::Dark
    } else {
        super::theme::resolve_variant_with(ctx, None, background)
    };
    let theme = load_theme_for(name, Some(&paths.themes), variant).unwrap_or_else(|e| {
        eprintln!("oomtop: theme: {e}; using terminal");
        terminal_theme()
    });
    let theme = if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        none_theme()
    } else {
        theme
    };
    let keymap_file = paths.keymap.is_file().then_some(paths.keymap.as_path());
    let (keymap, kerrs) = load_keymap(&cfg.keys.preset, keymap_file);
    for e in kerrs {
        eprintln!("oomtop: keymap: {e}");
    }
    let protect = engine.protect.clone();
    let actuator: Option<Box<dyn Actuator>> = if engine.is_live() {
        let mut a = SignalActuator::new();
        a.register_gentle(&first);
        Some(Box::new(a))
    } else {
        None // replay: nothing on this machine to act on
    };
    let state = if ctx.no_learn { None } else { ctx.state() };
    // Model files on disk (Models view): indexed on a helper thread so the first frame never waits for it.
    let disk_models = if ctx.replaying() {
        None
    } else {
        let folders = cfg.models.folders.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("oomtop-model-index".into())
            .spawn(move || {
                let idx = super::models::build_index(&folders, true);
                let _ = tx.send(super::models::disk_models(&idx));
            });
        spawned.ok().map(|_| rx)
    };
    let extras = oomtop_tui::TuiExtras {
        loaded: Some(ctx.loaded.clone()),
        load_options: Some(ctx.opts.clone()),
        paths: Some(paths.clone()),
        watch: true,
        background,
        disk_models,
        kitty_keyboard,
        late_replies,
    };
    oomtop_tui::run_with(
        oomtop_tui::TuiOptions {
            provider: Box::new(engine),
            actuator,
            config: cfg.clone(),
            theme,
            keymap,
            state,
            protect,
            plain: ctx.global.plain,
            ascii: ctx.global.ascii,
            no_learn: ctx.no_learn,
        },
        extras,
    )?;
    Ok(0)
}

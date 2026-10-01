//! `oomtop keys list [--conflicts] [--preset NAME]` (UX §7, §12.7).

use super::{out, usage, Ctx};
use crate::KeysCmd;
use anyhow::Result;
use oomtop_config::keymap::{load_keymap, PRESETS};
use oomtop_core::can_fit::EXIT_ERROR;

pub fn run(ctx: &Ctx, cmd: KeysCmd) -> Result<i32> {
    let KeysCmd::List { conflicts, preset } = cmd;
    let name = preset.unwrap_or_else(|| ctx.config().keys.preset.clone());
    if !PRESETS.contains(&name.as_str()) {
        return usage(format!(
            "unknown preset {name:?} (expected {})",
            PRESETS.join(" | ")
        ));
    }
    let keymap_file = ctx.paths().keymap;
    let user = keymap_file.is_file().then_some(keymap_file.as_path());
    let (km, errs) = load_keymap(&name, user);
    for e in &errs {
        eprintln!("oomtop: keymap: {e}");
    }
    let source = format!(
        "preset {}{}",
        km.preset,
        if user.is_some() {
            format!(" + {}", keymap_file.display())
        } else {
            String::new()
        }
    );
    if conflicts {
        let c = km.conflicts();
        for x in &c {
            out(&format!("{x}\n"));
        }
        if c.is_empty() {
            // Say so, so an empty answer can't be mistaken for a check that didn't run.
            out(&format!(
                "no conflicts ({source}, {} bindings checked)\n",
                km.bindings().len()
            ));
        }
        return Ok(if c.is_empty() && errs.is_empty() {
            0
        } else {
            EXIT_ERROR
        });
    }
    out(&format!("{source}\n"));
    let mut current = String::new();
    for (context, key, action) in km.bindings() {
        if context != current {
            out(&format!("\n[{context}]\n"));
            current = context;
        }
        out(&format!("  {key:<12} {action}\n"));
    }
    Ok(if errs.is_empty() { 0 } else { EXIT_ERROR })
}

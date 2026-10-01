//! Runs the TUI on deterministic fixture snapshots (no sampling, no signals): the motivating machine with swap
//! slowly growing and the model server finishing its job. Useful for screenshots/recordings and for trying
//! the keys without touching real processes — the actuator is `NoopActuator`, so confirmed stops report
//! "actions are disabled".
//!
//! ```text
//! cargo run -p oomtop-tui --example fixture_demo            # TUI
//! cargo run -p oomtop-tui --example fixture_demo -- --plain # screen-reader summary
//! ```

use oomtop_config::keymap::preset;
use oomtop_config::theme::terminal_theme;
use oomtop_config::Config;
use oomtop_core::history::History;
use oomtop_core::provider::SnapshotProvider;
use oomtop_core::Snapshot;
use oomtop_tui::{fixtures, run, TuiOptions};

struct Demo {
    tick: u64,
    history: History,
}

impl SnapshotProvider for Demo {
    fn snapshot(&mut self) -> Snapshot {
        self.tick += 1;
        let mut s = fixtures::motivating();
        s.taken_at_ms += self.tick * 2000;
        let grow = (self.tick * 12) << 20;
        s.memory.swap_used.value = s
            .memory
            .swap_used
            .value
            .map(|v| (v.saturating_sub(1 << 30) + grow).min(7 << 30));
        s.memory.available.value = s.memory.available.value.map(|v| v.saturating_sub(grow / 3));
        if let Some(m) = s.model_servers.first_mut() {
            if let Some(p) = m.progress.as_mut() {
                p.done = (3 + self.tick / 4).min(6) as u32;
            }
            if self.tick > 12 {
                m.busy.value = Some(false);
                m.progress = None;
            }
        }
        self.history.push_snapshot(&s);
        s
    }
    fn history(&self) -> &History {
        &self.history
    }
}

fn main() {
    let plain = std::env::args().any(|a| a == "--plain");
    let ascii = std::env::args().any(|a| a == "--ascii");
    let first = fixtures::motivating();
    let history = fixtures::history_for(&first, 30);
    let opts = TuiOptions {
        provider: Box::new(Demo { tick: 0, history }),
        actuator: Some(Box::new(oomtop_core::actions::NoopActuator)),
        config: Config::default(),
        theme: terminal_theme(),
        keymap: preset("default").expect("default keymap"),
        state: None,
        protect: fixtures::protect(),
        plain,
        ascii,
        no_learn: true,
    };
    if let Err(e) = run(opts) {
        eprintln!("oomtop demo: {e}");
        std::process::exit(1);
    }
}

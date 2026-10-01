//! Headless render of a snapshot JSON (as written by `oomtop json` or `oomtop --replay FIXTURE json`) into
//! plain text, at a chosen size and view — for reviewing layouts on real data without a terminal.
//!
//! ```text
//! oomtop --replay fixtures/macos/m5-air-agents.json json > /tmp/s.json
//! cargo run -p oomtop-tui --example render_json -- /tmp/s.json 140 40 [keys…]
//! ```
//! Extra arguments are keys fed to the app before rendering (e.g. `5` for Reclaim, `?` for help).

use oomtop_config::keymap::preset;
use oomtop_config::theme::terminal_theme;
use oomtop_config::Config;
use oomtop_core::history::History;
use oomtop_core::Snapshot;
use oomtop_tui::app::App;
use oomtop_tui::render::{draw_with, UNICODE};
use oomtop_tui::style::{ColorDepth, Palette};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: render_json SNAPSHOT.json [WIDTH] [HEIGHT] [KEYS…]");
        std::process::exit(2);
    };
    let w: u16 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(120);
    let h: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(36);
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    let snap: Snapshot = serde_json::from_str(&text).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    let mut app = App::new(&Config::default());
    app.area = Rect::new(0, 0, w, h);
    let mut hist = History::default();
    hist.push_snapshot(&snap);
    app.update(snap, &hist);
    let km = preset("default").expect("default keymap");
    for k in args.iter().skip(3) {
        let _ = app.on_key(k, &km);
    }
    let p = Palette::new(terminal_theme(), ColorDepth::None, false);
    let mut t = Terminal::new(TestBackend::new(w, h)).expect("backend");
    t.draw(|f| draw_with(f, &app, &p, &UNICODE, oomtop_config::model::Borders::Rounded))
        .expect("draw");
    let buf = t.backend().buffer().clone();
    for y in 0..h {
        let mut line = String::new();
        for x in 0..w {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
}

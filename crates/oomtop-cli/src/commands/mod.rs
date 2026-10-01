//! Subcommand implementations. Every command gets a [`Ctx`]: the layered config (UX §12.1.3) built from the
//! global flags, the config paths it resolved to, and helpers to build the engine and format numbers.

mod config;
mod doctor;
mod export;
mod keys;
mod measure;
mod models;
mod profile;
mod reclaim;
mod theme;
mod tui;

use crate::engine::Engine;
use crate::{Cli, Cmd, GlobalOpts};
use anyhow::{anyhow, Result};
use oomtop_config::layered::{load_layered, LoadOptions, Loaded};
use oomtop_config::paths::ConfigPaths;
use oomtop_core::units::{format_bytes, UnitSystem};
use std::io::{BufRead, IsTerminal, Write};

/// A bad argument discovered after parsing (exit code 2, like clap's own usage errors).
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// `Err(UsageError)` as an anyhow error.
pub fn usage<T>(msg: impl Into<String>) -> Result<T> {
    Err(anyhow::Error::new(UsageError(msg.into())))
}

pub struct Ctx {
    pub global: GlobalOpts,
    pub opts: LoadOptions,
    pub loaded: Loaded,
    /// `--no-learn`, `OOMTOP_NO_LEARN=1` or `personalization.learn = false`.
    pub no_learn: bool,
}

impl Ctx {
    pub fn new(global: GlobalOpts) -> Result<Self> {
        let mut flags: Vec<(String, String)> = Vec::new();
        for kv in &global.set {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| UsageError(format!("--set expects KEY=VALUE, got {kv:?}")))?;
            let k = k.trim();
            if k.is_empty() {
                return usage(format!("--set expects KEY=VALUE, got {kv:?}"));
            }
            flags.push((k.to_string(), v.trim().to_string()));
        }
        let quote = |s: &str| toml::Value::String(s.to_string()).to_string();
        if let Some(t) = &global.theme {
            flags.push(("appearance.theme".into(), quote(t)));
        }
        if let Some(c) = global.color {
            flags.push(("appearance.color".into(), quote(c.as_str())));
        }
        if global.ascii {
            flags.push(("appearance.glyphs".into(), quote("ascii")));
        }
        if global.no_learn {
            flags.push(("personalization.learn".into(), "false".into()));
        }
        let opts = LoadOptions {
            config_path: global.config.clone(),
            flags,
            ..Default::default()
        };
        let loaded = load_layered(&opts);
        let no_learn = global.no_learn || !loaded.config.personalization.learn;
        Ok(Ctx {
            global,
            opts,
            loaded,
            no_learn,
        })
    }

    /// Reports config problems once on stderr (the offending layer was skipped).
    fn warn_config_errors(&self) {
        for e in &self.loaded.errors {
            eprintln!("oomtop: config: {e}");
        }
    }

    pub fn config(&self) -> &oomtop_config::Config {
        &self.loaded.config
    }

    /// Paths of the effective user config directory (moves with `--config`).
    pub fn paths(&self) -> ConfigPaths {
        self.opts.paths()
    }

    pub fn units(&self) -> UnitSystem {
        match self.loaded.config.format.memory_units {
            oomtop_config::model::MemoryUnits::Si => UnitSystem::Si,
            oomtop_config::model::MemoryUnits::Iec => UnitSystem::Iec,
        }
    }

    /// Bytes in the configured units and decimals.
    pub fn fmt(&self, b: u64) -> String {
        format_bytes(b, self.units(), self.loaded.config.format.decimals as usize)
    }

    pub fn fmt_opt(&self, b: Option<u64>) -> String {
        b.map(|b| self.fmt(b)).unwrap_or_else(|| "n/a".into())
    }

    pub fn replaying(&self) -> bool {
        self.global.replay.is_some()
    }

    /// The state store (none while replaying: a fixture must never write this machine's journal).
    pub fn state(&self) -> Option<oomtop_state::StateDb> {
        if self.replaying() {
            return None;
        }
        match oomtop_state::StateDb::open_default() {
            Ok(mut db) => {
                db.set_learning(!self.no_learn);
                Some(db)
            }
            Err(e) => {
                eprintln!("oomtop: state: {e} (continuing without the lineage journal)");
                None
            }
        }
    }

    /// The pipeline: live sampling of this machine, or `--replay FIXTURE`. `on_demand` = two samples 250 ms
    /// apart per snapshot (one-shot commands and MCP), so CPU % and rates are real.
    pub fn engine(&self, on_demand: bool) -> Result<Engine> {
        let dir = self.opts.user_dir();
        let mut e = match &self.global.replay {
            Some(p) => Engine::replay_in(&self.loaded.config, p, Some(&dir)).map_err(|e| anyhow!(e))?,
            None => Engine::live_in(
                &self.loaded.config,
                self.state(),
                on_demand,
                self.global.offline || !self.loaded.config.adapters.enabled,
                Some(&dir),
            ),
        };
        e.set_learning(!self.no_learn);
        for err in &e.rule_errors {
            eprintln!("oomtop: rules: {err}");
        }
        Ok(e)
    }
}

/// Writes to stdout; a closed pipe (`| head`) is not an error.
pub fn out(s: &str) {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(s.as_bytes());
    let _ = o.flush();
}

pub fn json_pretty<T: serde::Serialize>(v: &T) -> Result<String> {
    Ok(serde_json::to_string_pretty(v)? + "\n")
}

/// Asks a yes/no question on the terminal. `Ok(None)` when stdin is not a terminal (no one to ask).
pub fn confirm(question: &str) -> Result<Option<bool>> {
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(Some(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    )))
}

pub fn dispatch(cli: Cli) -> Result<i32> {
    let ctx = Ctx::new(cli.global)?;
    // `config validate` / `doctor` report config errors themselves.
    let reports_errors = matches!(
        cli.command,
        Some(Cmd::Config {
            cmd: crate::ConfigCmd::Validate
        }) | Some(Cmd::Doctor { .. })
    );
    if !reports_errors {
        ctx.warn_config_errors();
    }
    match cli.command {
        None => tui::run(&ctx),
        Some(Cmd::Headroom(a)) => measure::headroom(&ctx, a),
        Some(Cmd::Why { json }) => measure::why(&ctx, json),
        Some(Cmd::Models { json, no_hash }) => models::run(&ctx, json, no_hash),
        Some(Cmd::Reclaim(a)) => reclaim::run(&ctx, a),
        Some(Cmd::Json { compact }) => export::json(&ctx, compact),
        Some(Cmd::Ndjson {
            interval,
            count,
            discard,
        }) => export::ndjson(&ctx, interval, count, discard),
        Some(Cmd::Serve { listen, max_requests }) => export::serve(&ctx, listen, max_requests),
        Some(Cmd::Mcp { allow_actions }) => export::mcp(&ctx, allow_actions),
        Some(Cmd::Doctor { json, no_probe }) => doctor::run(&ctx, json, no_probe),
        Some(Cmd::Config { cmd }) => config::run(&ctx, cmd),
        Some(Cmd::Theme { cmd }) => theme::run(&ctx, cmd),
        Some(Cmd::Keys { cmd }) => keys::run(&ctx, cmd),
        Some(Cmd::Profile { cmd }) => profile::run(&ctx, cmd),
    }
}

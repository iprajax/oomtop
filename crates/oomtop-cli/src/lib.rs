//! # oomtop-cli
//!
//! The `oomtop` binary (SPEC §12.2, UX §12.9, §6): every subcommand wired to the other crates through the
//! APIs in `docs/CONTRACTS.md`. The pipeline lives in [`engine::Engine`]; signals only ever go through
//! [`actuator::SignalActuator`] after an explicit confirmation.
//!
//! Exit codes: `0` ok / yes · `1` error · `2` usage · `3` yes after reclaim · `4` no.

pub mod actuator;
pub mod ansi;
pub mod commands;
pub mod engine;
pub mod profile;
pub mod termprobe;

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// `13G`, `500M`, `1.5GiB` → bytes (clap value parser; a bad value is a usage error, exit 2).
pub fn parse_size(s: &str) -> Result<u64, String> {
    oomtop_core::units::parse_bytes(s).map_err(|e| format!("{e} (examples: 13G, 500M, 1.5GiB)"))
}

/// `2s`, `500ms`, `1m` → duration (clap value parser).
pub fn parse_interval(s: &str) -> Result<Duration, String> {
    let secs =
        oomtop_core::units::parse_duration_s(s).map_err(|e| format!("{e} (examples: 2s, 500ms, 1m)"))?;
    if !secs.is_finite() || secs <= 0.0 {
        return Err("must be greater than zero".into());
    }
    Ok(Duration::from_secs_f64(secs))
}

#[derive(Debug, Parser)]
#[command(
    name = "oomtop",
    version,
    about = "See the OOM coming. Who's eating your machine: agents, models, sandboxes.",
    long_about = "oomtop: true memory accounting (GPU/Metal, compressed, swapped), attribution of every process \
                  to the agent session / app / sandbox / model server that owns it, headroom checks, OOM \
                  forecast and throttle explanations. Run without a subcommand for the TUI.",
    after_help = "Exit codes: 0 ok/yes · 1 error · 2 usage · 3 yes after reclaim · 4 no.\n\
                  Docs: oomtop <command> --help · oomtop config init --print (every setting) · oomtop doctor"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
    #[command(flatten)]
    pub global: GlobalOpts,
}

/// Flags accepted by every subcommand.
#[derive(Debug, Clone, Default, Args)]
pub struct GlobalOpts {
    /// Config file to use instead of ~/.config/oomtop/config.toml (drop-ins are read next to it).
    #[arg(long, global = true, env = "OOMTOP_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Don't learn from this session (personalization off). Also OOMTOP_NO_LEARN=1.
    #[arg(long, global = true)]
    pub no_learn: bool,
    /// Linear, screen-reader-friendly output instead of the TUI.
    #[arg(long, global = true)]
    pub plain: bool,
    /// ASCII glyphs only.
    #[arg(long, global = true)]
    pub ascii: bool,
    /// Theme name (overrides appearance.theme for this run).
    #[arg(long, global = true, value_name = "NAME")]
    pub theme: Option<String>,
    /// Color depth (overrides appearance.color for this run).
    #[arg(long, global = true, value_enum, value_name = "DEPTH")]
    pub color: Option<ColorArg>,
    /// Don't contact local adapter endpoints (process-level detection only).
    #[arg(long, global = true)]
    pub offline: bool,
    /// Replay a recorded fixture instead of sampling this machine (actions are disabled).
    #[arg(long, global = true, value_name = "FIXTURE")]
    pub replay: Option<PathBuf>,
    /// Override any setting for this run, e.g. --set appearance.theme=none (repeatable).
    #[arg(long = "set", global = true, value_name = "KEY=VALUE")]
    pub set: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorArg {
    Auto,
    Truecolor,
    #[value(name = "256")]
    C256,
    #[value(name = "16")]
    C16,
    None,
}

impl ColorArg {
    pub fn as_str(self) -> &'static str {
        match self {
            ColorArg::Auto => "auto",
            ColorArg::Truecolor => "truecolor",
            ColorArg::C256 => "256",
            ColorArg::C16 => "16",
            ColorArg::None => "none",
        }
    }
}

/// Where a model's weights are expected to live when estimating `--model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum Offload {
    /// GPU-resident on unified-memory Macs with a Metal budget (llama.cpp/Ollama default), host RAM otherwise.
    #[default]
    Auto,
    /// Offloaded to the GPU: the need must also fit the GPU budget.
    Gpu,
    /// CPU only: host RAM.
    Cpu,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Can I load this now? Exit 0 yes · 3 yes after reclaim · 4 no · 1 unmeasurable.
    Headroom(HeadroomArgs),
    /// Ranked causes of slowness / memory pressure, with evidence.
    Why {
        #[arg(long)]
        json: bool,
    },
    /// Idle build daemons, orphans and idle model servers; stops only after confirmation (or --yes).
    Reclaim(ReclaimArgs),
    /// Model files on disk: size, last use, duplicates, and which server has each loaded.
    Models {
        #[arg(long)]
        json: bool,
        /// Skip the partial hash of same-size files (duplicate detection).
        #[arg(long)]
        no_hash: bool,
    },
    /// One snapshot as JSON (schema_version 1, command lines redacted).
    Json {
        /// Single-line output.
        #[arg(long)]
        compact: bool,
    },
    /// Snapshots as newline-delimited JSON (one per interval).
    Ndjson {
        /// Time between snapshots (min 250ms).
        #[arg(long, default_value = "2s", value_parser = parse_interval)]
        interval: Duration,
        /// Stop after N snapshots.
        #[arg(long, value_name = "N")]
        count: Option<u64>,
        /// Run the sampling pipeline but print nothing (perf measurement, SPEC §14).
        #[arg(long, hide = true)]
        discard: bool,
    },
    /// HTTP JSON + Prometheus /metrics (binds 127.0.0.1 unless told otherwise).
    Serve {
        /// Listen address (default from serve.listen: 127.0.0.1:9469).
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,
        /// Exit after N requests (testing).
        #[arg(long, hide = true)]
        max_requests: Option<usize>,
    },
    /// MCP server over stdio (read-only tools unless --allow-actions).
    Mcp {
        /// Register the elicitation-gated reclaim tool.
        #[arg(long)]
        allow_actions: bool,
    },
    /// Detected capabilities, each source's status and why, config/state locations.
    Doctor {
        #[arg(long)]
        json: bool,
        /// Don't query the terminal (OSC 11 / Kitty keyboard / DECRQM); report environment-based guesses only.
        #[arg(long)]
        no_probe: bool,
    },
    /// Configuration files (layered, comment-preserving).
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Themes (semantic tokens).
    Theme {
        #[command(subcommand)]
        cmd: ThemeCmd,
    },
    /// Key bindings.
    Keys {
        #[command(subcommand)]
        cmd: KeysCmd,
    },
    /// Local personalization profile (never leaves this machine).
    Profile {
        #[command(subcommand)]
        cmd: ProfileCmd,
    },
}

#[derive(Debug, Args)]
pub struct HeadroomArgs {
    /// Host RAM needed, e.g. 13G, 500M.
    #[arg(long, value_name = "SIZE", value_parser = parse_size, conflicts_with = "model")]
    pub need: Option<u64>,
    /// GPU memory that must also fit the GPU budget (discrete GPUs), e.g. 8G.
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub gpu: Option<u64>,
    /// Model file or directory (.gguf, safetensors/MLX dir); reads headers only, never weights.
    #[arg(long, value_name = "PATH")]
    pub model: Option<PathBuf>,
    /// Context length for the KV-cache estimate (default: the model's, capped at 8192).
    #[arg(long, value_name = "N", requires = "model")]
    pub ctx: Option<u64>,
    /// KV cache type: f16 | q8_0 | q4_0 | … (llama.cpp --cache-type-k).
    #[arg(long, value_name = "TYPE", default_value = "f16", requires = "model")]
    pub kv_type: String,
    /// Parallel sequences (llama.cpp -np / OLLAMA_NUM_PARALLEL).
    #[arg(long, value_name = "N", default_value_t = 1, requires = "model")]
    pub parallel: u64,
    /// Where the model runs.
    #[arg(long, value_enum, default_value_t = Offload::Auto, requires = "model")]
    pub offload: Offload,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct ReclaimArgs {
    /// Show what would be stopped and the estimated gain; do nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Don't ask: confirm the listed plan (SIGTERM / gentle unload only; SIGKILL always asks again).
    #[arg(long)]
    pub yes: bool,
    /// Only these group ids (comma separated; must be reclaim candidates).
    #[arg(long, value_delimiter = ',', value_name = "IDS")]
    pub groups: Vec<String>,
    /// How long to wait for stopped groups to exit before offering SIGKILL.
    #[arg(long, default_value = "5s", value_parser = parse_interval)]
    pub wait: Duration,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    /// Write a fully commented config.toml (every option at its default) and the JSON Schemas.
    Init {
        /// Overwrite an existing config.toml.
        #[arg(long)]
        force: bool,
        /// Print to stdout instead of writing.
        #[arg(long)]
        print: bool,
    },
    /// Open config.toml (or this host's override) in $VISUAL / $EDITOR, then validate it.
    Edit {
        #[arg(long)]
        host: bool,
    },
    /// Print the configuration (merged TOML, or every leaf with --effective).
    Print {
        /// One `key = value` line per setting.
        #[arg(long)]
        effective: bool,
        /// Show which layer set each value (implies --effective).
        #[arg(long)]
        origin: bool,
        /// Only this key (or section prefix).
        #[arg(value_name = "KEY")]
        key: Option<String>,
    },
    /// Check every file (config layers, themes, keymap, layouts, rules) with file:line errors.
    Validate,
    /// Set a value with a comment-preserving edit (validated before writing).
    Set {
        key: String,
        value: String,
        /// user | host | dropin:<name>
        #[arg(long, default_value = "user")]
        layer: String,
    },
    /// Remove a key from a layer (falls back to lower layers / defaults).
    Unset {
        key: String,
        #[arg(long, default_value = "user")]
        layer: String,
    },
    /// Print a JSON Schema (config | theme | keymap | layout | rules), or write all with --write.
    Schema {
        #[arg(default_value = "config")]
        kind: String,
        /// Write every schema plus a .taplo.toml into DIR.
        #[arg(long, value_name = "DIR")]
        write: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ThemeCmd {
    /// Built-in and user themes.
    List,
    /// Show every token rendered in this terminal.
    Preview {
        /// Theme name (default: the configured theme).
        name: Option<String>,
        /// Variant for truecolor themes (default: detected from the terminal background).
        #[arg(long, value_parser = ["dark", "light"])]
        variant: Option<String>,
    },
    /// Validate a theme: unknown tokens/colors, blue/purple defaults, contrast.
    Check {
        name: String,
        #[arg(long, value_parser = ["dark", "light"])]
        variant: Option<String>,
    },
    /// Import a base16/base24, iTerm2, Ghostty, Kitty or Alacritty scheme into ~/.config/oomtop/themes/.
    Import {
        file: PathBuf,
        /// Theme name (default: from the scheme).
        #[arg(long)]
        name: Option<String>,
        /// Overwrite an existing user theme of the same name.
        #[arg(long)]
        force: bool,
        /// Print the theme instead of writing it.
        #[arg(long)]
        print: bool,
    },
    /// Print a theme as an editable theme file (resolved, all tokens).
    Export {
        name: String,
        #[arg(long, value_parser = ["dark", "light"])]
        variant: Option<String>,
        /// Write to this file instead of stdout.
        #[arg(long, short, value_name = "FILE")]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub enum KeysCmd {
    /// Effective bindings per context (preset + keymap.toml).
    List {
        /// Only print conflicting bindings (exit 1 if any).
        #[arg(long)]
        conflicts: bool,
        /// Preset to show instead of the configured one (default | vim | emacs | htop).
        #[arg(long)]
        preset: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProfileCmd {
    /// Machine profile, detected roles and learned entities.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Everything learned, as JSON (no command lines, no environments).
    Export,
    /// Forget pins, mutes, renames, learned ranking and the machine profile (lineage is kept).
    Reset {
        #[arg(long)]
        yes: bool,
    },
    /// Local UX metrics: Hit@3, keystrokes to target, reformulation and dismiss rates.
    Stats {
        #[arg(long)]
        json: bool,
    },
}

/// Parses args and runs; returns the process exit code.
pub fn run<I, T>(args: I) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let code = e.exit_code();
            let _ = e.print();
            return code;
        }
    };
    match commands::dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            if let Some(u) = e.downcast_ref::<commands::UsageError>() {
                eprintln!("oomtop: {u}");
                return oomtop_core::can_fit::EXIT_USAGE;
            }
            eprintln!("oomtop: {e:#}");
            oomtop_core::can_fit::EXIT_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn usage_errors_exit_2() {
        assert_eq!(run(["oomtop", "headroom", "--need", "1G", "--model", "x"]), 2);
        assert_eq!(run(["oomtop", "headroom", "--need", "banana"]), 2);
        assert_eq!(
            run(["oomtop", "headroom", "--ctx", "4096"]),
            2,
            "--ctx requires --model"
        );
        assert_eq!(run(["oomtop", "ndjson", "--interval", "0s"]), 2);
        assert_eq!(run(["oomtop", "--color", "purple", "json"]), 2);
        assert_eq!(run(["oomtop", "nope"]), 2);
        assert_eq!(run(["oomtop", "--version"]), 0);
    }

    #[test]
    fn value_parsers() {
        assert_eq!(parse_size("13G").unwrap(), 13 << 30);
        assert!(parse_size("13 bananas").is_err());
        assert_eq!(parse_interval("500ms").unwrap(), Duration::from_millis(500));
        assert!(parse_interval("-1s").is_err());
    }

    #[test]
    fn global_flags_anywhere() {
        let c =
            Cli::try_parse_from(["oomtop", "json", "--offline", "--set", "a.b=1", "--color", "256"]).unwrap();
        assert!(c.global.offline);
        assert_eq!(c.global.set, vec!["a.b=1"]);
        assert_eq!(c.global.color, Some(ColorArg::C256));
    }
}

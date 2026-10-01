//! `oomtop config init | edit | print | validate | set | unset | schema` (UX §12.9).

use super::{out, usage, Ctx};
use crate::ConfigCmd;
use anyhow::{anyhow, bail, Context, Result};
use oomtop_config::layered::load_layered;
use oomtop_config::schema::{schema_text, taplo_config, write_schemas, SchemaKind, SCHEMA_KINDS};
use oomtop_config::write::{set_value, unset_value, Layer};
use oomtop_core::can_fit::EXIT_ERROR;
use std::path::PathBuf;

/// The file a `--layer` resolves to. With `--config PATH`, `user` means that file.
fn layer_file(ctx: &Ctx, layer: &str) -> Result<PathBuf> {
    let l = match Layer::parse(layer) {
        Ok(l) => l,
        Err(e) => return usage(format!("--layer: {}", e.message)),
    };
    let l = match (&l, &ctx.global.config) {
        (Layer::User, Some(p)) => Layer::File(p.clone()),
        _ => l,
    };
    Ok(l.path(&ctx.paths(), &ctx.opts.hostname()))
}

/// Splits `$VISUAL`/`$EDITOR` like a shell would for the common cases ("code -w", "nvim").
fn editor_command() -> (String, Vec<String>) {
    let raw = std::env::var("VISUAL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".into());
    let mut parts = raw.split_whitespace().map(str::to_string);
    let prog = parts.next().unwrap_or_else(|| "vi".into());
    (prog, parts.collect())
}

pub fn run(ctx: &Ctx, cmd: ConfigCmd) -> Result<i32> {
    let paths = ctx.paths();
    match cmd {
        ConfigCmd::Init { force, print } => {
            let t = oomtop_config::init_template();
            if print {
                out(&t);
                return Ok(0);
            }
            let main = ctx.opts.main_file();
            if main.exists() && !force {
                bail!(
                    "{} exists (use --force to overwrite, --print to see the template)",
                    main.display()
                );
            }
            let dir = main.parent().map(PathBuf::from).unwrap_or(paths.dir.clone());
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            oomtop_config::write::write_atomic(&main, &t).map_err(|e| anyhow!("{e}"))?;
            out(&format!("wrote {}\n", main.display()));
            let schema_dir = dir.join("schemas");
            match write_schemas(&schema_dir) {
                Ok(files) => {
                    for f in files {
                        out(&format!("wrote {}\n", f.display()));
                    }
                    let taplo = dir.join(".taplo.toml");
                    if !taplo.exists() {
                        std::fs::write(&taplo, taplo_config("schemas"))?;
                        out(&format!(
                            "wrote {} (editor completion via taplo)\n",
                            taplo.display()
                        ));
                    }
                }
                Err(e) => eprintln!("oomtop: schemas: {e}"),
            }
            Ok(0)
        }
        ConfigCmd::Edit { host } => {
            let file = if host {
                paths.host_file(&ctx.opts.hostname())
            } else {
                ctx.opts.main_file()
            };
            if !file.exists() {
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let initial = if host {
                    format!(
                        "# Settings for host {:?} only (config.d/host-<hostname>.toml wins over config.toml).\n",
                        ctx.opts.hostname()
                    )
                } else {
                    oomtop_config::init_template()
                };
                std::fs::write(&file, initial)?;
            }
            let (prog, args) = editor_command();
            let status = std::process::Command::new(&prog)
                .args(&args)
                .arg(&file)
                .status()
                .with_context(|| format!("starting editor {prog:?} (set $VISUAL or $EDITOR)"))?;
            if !status.success() {
                bail!("editor {prog:?} exited with {status}");
            }
            let report = oomtop_config::validate::validate_all(&ctx.opts);
            if report.is_ok() {
                out(&format!("{}: ok\n", file.display()));
                Ok(0)
            } else {
                for e in &report.errors {
                    out(&format!("{e}\n"));
                }
                Ok(EXIT_ERROR)
            }
        }
        ConfigCmd::Print {
            effective,
            origin,
            key,
        } => {
            let matches = |k: &str| {
                key.as_deref()
                    .is_none_or(|want| k == want || k.starts_with(&format!("{want}.")))
            };
            if effective || origin || key.is_some() {
                let mut any = false;
                for e in ctx.loaded.effective() {
                    if !matches(&e.key) {
                        continue;
                    }
                    any = true;
                    if origin {
                        out(&format!("{} = {}    # {}\n", e.key, e.value, e.origin));
                    } else {
                        out(&format!("{} = {}\n", e.key, e.value));
                    }
                }
                if !any {
                    if let Some(k) = key {
                        return usage(format!(
                            "unknown setting {k:?} (see `oomtop config init --print`)"
                        ));
                    }
                }
            } else {
                out(&ctx.loaded.effective_toml());
            }
            Ok(0)
        }
        ConfigCmd::Validate => {
            let report = oomtop_config::validate::validate_all(&ctx.opts);
            if report.is_ok() {
                out(&format!(
                    "ok: {} file(s) checked{}\n",
                    report.checked.len(),
                    if report.checked.is_empty() {
                        " (defaults only)"
                    } else {
                        ""
                    }
                ));
                for f in &report.checked {
                    out(&format!("  {}\n", f.display()));
                }
                Ok(0)
            } else {
                for e in &report.errors {
                    out(&format!("{e}\n"));
                }
                Ok(EXIT_ERROR)
            }
        }
        ConfigCmd::Set { key, value, layer } => {
            let path = layer_file(ctx, &layer)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            set_value(&path, &key, &value).map_err(|e| anyhow!("{e}"))?;
            // Show what is effective now (a higher layer may still win).
            let now = load_layered(&ctx.opts);
            let origin = now.origin(&key);
            out(&format!("{key} = {value}  → {}\n", path.display()));
            let written_here = matches!(&origin, oomtop_config::Origin::File { path: p, .. } if *p == path);
            if !written_here {
                out(&format!(
                    "note: {key} is still set by {origin}, which takes precedence\n"
                ));
            }
            Ok(0)
        }
        ConfigCmd::Unset { key, layer } => {
            let path = layer_file(ctx, &layer)?;
            let removed = unset_value(&path, &key).map_err(|e| anyhow!("{e}"))?;
            if removed {
                out(&format!("removed {key} from {}\n", path.display()));
            } else {
                out(&format!("{key} was not set in {}\n", path.display()));
            }
            Ok(0)
        }
        ConfigCmd::Schema { kind, write } => {
            if let Some(dir) = write {
                let files = write_schemas(&dir).with_context(|| format!("writing {}", dir.display()))?;
                for f in files {
                    out(&format!("wrote {}\n", f.display()));
                }
                let taplo = dir.join(".taplo.toml");
                std::fs::write(&taplo, taplo_config("."))?;
                out(&format!("wrote {}\n", taplo.display()));
                return Ok(0);
            }
            let Some(k) = SchemaKind::parse(&kind) else {
                let all: Vec<&str> = SCHEMA_KINDS.iter().map(|k| k.as_str()).collect();
                return usage(format!("unknown schema {kind:?}: expected {}", all.join(" | ")));
            };
            out(&schema_text(k));
            Ok(0)
        }
    }
}

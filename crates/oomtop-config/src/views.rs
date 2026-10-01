//! Aliases and saved views (UX §12.7):
//! ```toml
//! [aliases]
//! hogs = ":sort footprint desc"      # a command alias (value starts with ':')
//! llm  = "kind:model,agent"          # a filter alias, usable inside queries: `llm gpu>1G`, `-llm`
//! [[views]]
//! name = "gpu-work"
//! query = "gpu>500M or kind:model"
//! layout = "llm-dev"
//! ```
//! Filter aliases expand as a parenthesized group, so `-llm` / `not llm` negate the whole alias. Expansion
//! is recursive (an alias may use another) with cycle detection.

use crate::layered::{closest, Loaded, Origin};
use crate::model::{Config, SavedView};
use crate::ConfigError;
use std::collections::BTreeMap;
use std::path::Path;

/// Words an alias may not use (filter keys and grammar words).
pub const RESERVED: &[&str] = &[
    "kind", "owner", "name", "mem", "gpu", "cpu", "idle", "state", "is", "sandbox", "or", "and", "not",
];

const MAX_DEPTH: usize = 8;

/// An expanded query: a filter, or a command (`:sort footprint desc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expanded {
    Filter(String),
    Command(String),
}

fn split_words(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    for c in input.chars() {
        if c == '"' {
            in_q = !in_q;
            cur.push(c);
            continue;
        }
        if in_q {
            cur.push(c);
            continue;
        }
        match c {
            c if c.is_whitespace() => flush(&mut cur, &mut out),
            '(' if cur == "-" || cur == "!" => {
                cur.push('(');
                flush(&mut cur, &mut out);
            }
            '(' | ')' => {
                flush(&mut cur, &mut out);
                out.push(c.to_string());
            }
            _ => cur.push(c),
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn is_command(v: &str) -> bool {
    v.trim_start().starts_with(':')
}

fn expand_filter(
    input: &str,
    aliases: &BTreeMap<String, String>,
    stack: &mut Vec<String>,
) -> Result<String, String> {
    if stack.len() > MAX_DEPTH {
        return Err("aliases nested too deeply".into());
    }
    let mut out = Vec::new();
    for w in split_words(input) {
        let (neg, name) = match w.strip_prefix(['-', '!']) {
            Some(rest) if !rest.is_empty() && !rest.starts_with('(') => (&w[..1], rest),
            _ => ("", w.as_str()),
        };
        match aliases.get(name) {
            Some(v) if !is_command(v) => {
                if stack.iter().any(|s| s == name) {
                    return Err(format!("alias cycle: {} → {name}", stack.join(" → ")));
                }
                stack.push(name.to_string());
                let inner = expand_filter(v, aliases, stack)?;
                stack.pop();
                out.push(format!("{neg}({inner})"));
            }
            Some(_) => return Err(format!("command alias {name:?} cannot be used inside a filter")),
            None => out.push(w),
        }
    }
    Ok(out.join(" "))
}

/// Expands aliases in a query. A query that *is* a command alias (optionally followed by more words)
/// becomes that command with the words appended.
pub fn expand(input: &str, aliases: &BTreeMap<String, String>) -> Result<Expanded, String> {
    let trimmed = input.trim();
    let first = trimmed.split_whitespace().next().unwrap_or("");
    if let Some(v) = aliases.get(first).filter(|v| is_command(v)) {
        let rest = trimmed[first.len()..].trim();
        let cmd = if rest.is_empty() {
            v.trim().to_string()
        } else {
            format!("{} {rest}", v.trim())
        };
        return Ok(Expanded::Command(cmd));
    }
    if is_command(trimmed) {
        return Ok(Expanded::Command(trimmed.to_string()));
    }
    expand_filter(trimmed, aliases, &mut Vec::new()).map(Expanded::Filter)
}

/// A saved view by name.
pub fn find_view<'a>(cfg: &'a Config, name: &str) -> Option<&'a SavedView> {
    cfg.views.iter().find(|v| v.name == name)
}

/// Problems with aliases and views as (dotted key, message).
pub fn problems(cfg: &Config, layouts_dir: Option<&Path>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (name, value) in &cfg.aliases {
        let key = format!("aliases.{name}");
        let valid = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && !name.starts_with('-');
        if !valid {
            out.push((
                key.clone(),
                format!("alias name {name:?} must be a single word [a-z0-9_-]"),
            ));
            continue;
        }
        if RESERVED.contains(&name.as_str()) {
            out.push((
                key.clone(),
                format!("alias {name:?} would shadow a filter key/keyword"),
            ));
            continue;
        }
        if value.trim().is_empty() {
            out.push((key.clone(), format!("alias {name:?} is empty")));
            continue;
        }
        if is_command(value) {
            continue;
        }
        match expand_filter(value, &cfg.aliases, &mut vec![name.clone()]) {
            Err(e) => out.push((key, format!("alias {name:?}: {e}"))),
            Ok(expanded) => {
                if let Err(e) = oomtop_core::query::parse_filter(&expanded) {
                    out.push((key, format!("alias {name:?}: {e}")));
                }
            }
        }
    }
    let layouts = layouts_dir.map(crate::layout::list_layouts);
    let mut seen: Vec<&str> = Vec::new();
    for (i, v) in cfg.views.iter().enumerate() {
        let key = format!("views.{i}");
        if v.name.trim().is_empty() {
            out.push((format!("{key}.name"), format!("view #{} has no name", i + 1)));
        } else if seen.contains(&v.name.as_str()) {
            out.push((format!("{key}.name"), format!("duplicate view name {:?}", v.name)));
        }
        seen.push(&v.name);
        match expand(&v.query, &cfg.aliases) {
            Err(e) => out.push((format!("{key}.query"), format!("view {:?}: {e}", v.name))),
            Ok(Expanded::Filter(f)) => {
                if let Err(e) = oomtop_core::query::parse_filter(&f) {
                    out.push((format!("{key}.query"), format!("view {:?}: {e}", v.name)));
                }
            }
            Ok(Expanded::Command(_)) => {}
        }
        if !v.layout.is_empty() {
            if let Some(ls) = &layouts {
                if !ls.contains(&v.layout) {
                    let hint = closest(&v.layout, ls.iter().map(String::as_str))
                        .map(|c| format!(" — did you mean {c:?}?"))
                        .unwrap_or_default();
                    out.push((
                        format!("{key}.layout"),
                        format!("view {:?}: no layout {:?} in layouts/{hint}", v.name, v.layout),
                    ));
                }
            }
        }
    }
    out
}

/// [`problems`] with file:line taken from where the key was set.
pub fn validate(loaded: &Loaded, layouts_dir: Option<&Path>) -> Vec<ConfigError> {
    problems(&loaded.config, layouts_dir)
        .into_iter()
        .map(|(key, message)| {
            let origin = loaded.origin(&key);
            let (path, line) = match origin {
                Origin::File { path, line } => {
                    // arrays of tables record one origin; find the exact entry in the file
                    let exact = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|t| crate::layered::find_line(&t, &key));
                    (Some(path), exact.or(line))
                }
                Origin::Env(k) => {
                    return ConfigError {
                        path: None,
                        line: None,
                        message: format!("{k}: {message}"),
                    }
                }
                _ => (None, None),
            };
            ConfigError { path, line, message }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("hogs".to_string(), ":sort footprint desc".to_string()),
            ("llm".to_string(), "kind:model,agent".to_string()),
            ("heavy-llm".to_string(), "llm gpu>1G".to_string()),
            ("loop-a".to_string(), "loop-b".to_string()),
            ("loop-b".to_string(), "loop-a".to_string()),
        ])
    }

    #[test]
    fn expansion() {
        let a = aliases();
        assert_eq!(
            expand("llm", &a).unwrap(),
            Expanded::Filter("(kind:model,agent)".into())
        );
        assert_eq!(
            expand("heavy-llm or mem>2G", &a).unwrap(),
            Expanded::Filter("((kind:model,agent) gpu>1G) or mem>2G".into())
        );
        assert_eq!(
            expand("-llm owner:\"Claude Code\"", &a).unwrap(),
            Expanded::Filter("-(kind:model,agent) owner:\"Claude Code\"".into())
        );
        assert_eq!(
            expand("hogs", &a).unwrap(),
            Expanded::Command(":sort footprint desc".into())
        );
        assert_eq!(expand(":why", &a).unwrap(), Expanded::Command(":why".into()));
        assert!(expand("loop-a", &a).unwrap_err().contains("cycle"));
        assert!(expand("mem>2G hogs", &a).is_err());
        // expanded filters parse with the core grammar
        for q in ["llm", "heavy-llm or mem>2G", "-llm", "not llm", "(llm)"] {
            let Expanded::Filter(f) = expand(q, &a).unwrap() else {
                panic!()
            };
            oomtop_core::query::parse_filter(&f).unwrap_or_else(|e| panic!("{q} → {f}: {e}"));
        }
    }

    #[test]
    fn validation_with_lines() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("layouts")).unwrap();
        std::fs::write(d.path().join("layouts/llm-dev.toml"), "name = \"llm-dev\"\n").unwrap();
        std::fs::write(
            d.path().join("config.toml"),
            "[aliases]\nllm = \"kind:model,agent\"\nkind = \"x\"\nbad = \"mem>>2\"\nhogs = \":sort footprint desc\"\n\n[[views]]\nname = \"gpu-work\"\nquery = \"gpu>500M or llm\"\nlayout = \"llm-dev\"\n\n[[views]]\nname = \"gpu-work\"\nquery = \"(\"\nlayout = \"llm-deb\"\n",
        )
        .unwrap();
        let loaded = crate::layered::load_layered(&crate::layered::LoadOptions {
            config_dir: Some(d.path().to_path_buf()),
            system_dir: Some(Default::default()),
            hostname: Some("h".into()),
            env: Some(vec![]),
            ..Default::default()
        });
        assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
        let errs = validate(&loaded, Some(&d.path().join("layouts")));
        let msgs: Vec<String> = errs.iter().map(|e| e.to_string()).collect();
        assert_eq!(errs.len(), 5, "{msgs:#?}");
        assert!(
            msgs.iter()
                .any(|m| m.ends_with("config.toml:3: alias \"kind\" would shadow a filter key/keyword")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("config.toml:4: alias \"bad\"")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("duplicate view name \"gpu-work\"")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("config.toml:14: view \"gpu-work\": empty expression")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("config.toml:13: duplicate view name")),
            "{msgs:#?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("did you mean \"llm-dev\"?")),
            "{msgs:#?}"
        );
        assert_eq!(find_view(&loaded.config, "gpu-work").unwrap().layout, "llm-dev");
    }
}

//! Rule files: parsing, the embedded built-in set, `rules.d` loading, merging and linting (SPEC §15).

use oomtop_core::attribution::{CompiledRules, RuleError, RuleSet};
use oomtop_core::redact::is_allowlisted;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Priority added to user rules so they win over built-ins.
pub const USER_PRIORITY_BOOST: i32 = 100;

/// Rule files larger than this are rejected (a rules file is a few KiB; this guards against mistakes).
pub const MAX_RULE_FILE_BYTES: u64 = 1024 * 1024;

const BUILTIN_TOML: &str = include_str!("builtin.toml");

#[derive(Debug, Error, Clone, PartialEq)]
pub enum RuleLoadError {
    /// TOML syntax or schema error; `msg` carries the line/column from the parser.
    #[error("{path}: {msg}")]
    Parse { path: String, msg: String },
    #[error("{path}: {msg}")]
    Io { path: String, msg: String },
    /// A glob/regex that does not compile, or an empty label.
    #[error("{path}: {source}")]
    Compile { path: String, source: RuleError },
}

/// A non-fatal problem with a rule (the rule still loads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleWarning {
    /// Rule id (explicit `id` or label slug).
    pub rule: String,
    pub message: String,
}

impl std::fmt::Display for RuleWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rule {:?}: {}", self.rule, self.message)
    }
}

/// Parses a rules file (`[[group]]` tables). Unknown fields are errors (typos should not silently match
/// nothing).
pub fn parse_rules(text: &str, origin: &str) -> Result<RuleSet, RuleLoadError> {
    toml::from_str::<RuleSet>(text).map_err(|e| RuleLoadError::Parse {
        path: origin.to_string(),
        msg: e.to_string().trim_end().to_string(),
    })
}

/// The built-in rule set (embedded `builtin.toml`).
pub fn builtin_rules() -> RuleSet {
    // The embedded file is covered by tests; an error here is a build-time bug, so fall back to empty.
    parse_rules(BUILTIN_TOML, "builtin.toml").unwrap_or_default()
}

/// Loads `*.toml` from a rules directory in alphabetical order. Bad files are reported and skipped (each
/// file is compiled on its own so one bad rule doesn't disable the rest); hidden files are ignored; a
/// missing directory is not an error. Priorities are returned as written (see [`merge_user_rules`]).
pub fn load_rules_dir(dir: &Path) -> (RuleSet, Vec<RuleLoadError>) {
    let mut set = RuleSet::default();
    let mut errors = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (set, errors),
        Err(e) => {
            errors.push(RuleLoadError::Io {
                path: dir.display().to_string(),
                msg: e.to_string(),
            });
            return (set, errors);
        }
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
        })
        .collect();
    files.sort();
    for f in files {
        let path = f.display().to_string();
        match std::fs::metadata(&f) {
            Ok(m) if !m.is_file() => continue,
            Ok(m) if m.len() > MAX_RULE_FILE_BYTES => {
                errors.push(RuleLoadError::Io {
                    path,
                    msg: format!("file too large ({} bytes, max {MAX_RULE_FILE_BYTES})", m.len()),
                });
                continue;
            }
            Err(e) => {
                errors.push(RuleLoadError::Io {
                    path,
                    msg: e.to_string(),
                });
                continue;
            }
            Ok(_) => {}
        }
        match std::fs::read_to_string(&f) {
            Err(e) => errors.push(RuleLoadError::Io {
                path,
                msg: e.to_string(),
            }),
            Ok(text) => match parse_rules(&text, &path) {
                Err(e) => errors.push(e),
                Ok(rs) => match CompiledRules::compile(&rs) {
                    Ok(_) => set.rules.extend(rs.rules),
                    Err(source) => errors.push(RuleLoadError::Compile { path, source }),
                },
            },
        }
    }
    (set, errors)
}

/// Built-in rules followed by user rules. Each user rule gets [`USER_PRIORITY_BOOST`] added; a user rule
/// with an explicit `id` equal to a built-in rule's id replaces that built-in (so users can retune or
/// neutralise a built-in without forking the whole set).
pub fn merge_user_rules(builtin: RuleSet, user: RuleSet) -> RuleSet {
    let replaced: Vec<String> = user.rules.iter().filter_map(|r| r.id.clone()).collect();
    let mut rules: Vec<_> = builtin
        .rules
        .into_iter()
        .filter(|r| !replaced.contains(&r.rule_id()))
        .collect();
    rules.extend(user.rules.into_iter().map(|mut r| {
        r.priority = r.priority.saturating_add(USER_PRIORITY_BOOST);
        r
    }));
    RuleSet { rules }
}

/// Non-fatal checks: marker keys (and `session_key`) must be in the privacy allowlist or they can never
/// match; `session_key` must be `env:KEY`; a rule needs at least one match field; ids must be unique.
pub fn lint_rules(set: &RuleSet, allowlist: &[String]) -> Vec<RuleWarning> {
    let mut out = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for r in &set.rules {
        let id = r.rule_id();
        *seen.entry(id.clone()).or_default() += 1;
        let m = &r.matcher;
        let has_identity = !(m.exe.is_empty()
            && m.name.is_empty()
            && m.script.is_empty()
            && m.cmdline.is_empty()
            && m.cwd.is_empty()
            && m.bundle.is_empty()
            && m.cgroup.is_empty());
        if !has_identity && m.env.is_empty() {
            out.push(RuleWarning {
                rule: id.clone(),
                message: "no match fields: the rule matches nothing".into(),
            });
        }
        for k in &m.env {
            if !is_allowlisted(k, allowlist) {
                out.push(RuleWarning {
                    rule: id.clone(),
                    message: format!(
                        "marker key {k:?} is not in privacy.marker_allowlist, so it is never read"
                    ),
                });
            }
        }
        if let Some(sk) = &r.session_key {
            match sk.strip_prefix("env:") {
                Some(k) if !k.is_empty() => {
                    if !is_allowlisted(k, allowlist) {
                        out.push(RuleWarning {
                            rule: id.clone(),
                            message: format!("session_key {sk:?}: {k:?} is not in privacy.marker_allowlist"),
                        });
                    }
                }
                _ => out.push(RuleWarning {
                    rule: id.clone(),
                    message: format!("session_key {sk:?} must look like \"env:KEY\""),
                }),
            }
        }
    }
    for (id, n) in seen {
        if n > 1 {
            out.push(RuleWarning {
                rule: id,
                message: format!("{n} rules share this id; their groups can merge into each other"),
            });
        }
    }
    out
}

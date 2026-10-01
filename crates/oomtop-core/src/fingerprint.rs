//! Entity fingerprints (UX §4): `hash(kind, exe_basename, cmdline_template, project_root?, bundle_id?)`.
//!
//! The command-line template is argv with values normalized so an entity survives restarts, version bumps
//! and port changes:
//!
//! | value | placeholder |
//! |---|---|
//! | secrets (`--token x`, `KEY=…`, URL credentials, known token formats, high-entropy strings) | `<secret>` |
//! | integers, floats, sizes/durations (`7861`, `0.5`, `4g`, `30s`) | `<n>` |
//! | versions (`9.8.0`, `v1.2.3-rc1`) | `<ver>` |
//! | UUIDs / long hex ids | `<uuid>` / `<hex>` |
//! | URLs, `host:port`, IP addresses | `<url>`, `<addr>`, `<ip>` |
//! | paths | project-relative when under the project root, else `<file>` (has an extension) / `<path>` |
//! | free text (prompts, contains whitespace) | `<text>` |
//!
//! Interpreters are collapsed to what they run (`python3 studio.py` → `studio.py`, `node …/node_modules/
//! @scope/pkg/cli.js` → `@scope/pkg`, `python -m http.server` → `http.server`, `java … GradleDaemon` →
//! `GradleDaemon`). Secrets are placeholder-ized **before** anything is hashed, and raw command lines are
//! never stored.

use crate::model::{GroupKind, Process};
use crate::redact::{is_secret_flag, looks_like_secret, redact_text, REDACTED};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::sync::OnceLock;

/// Template tokens kept (head + 7 arguments).
const MAX_TOKENS: usize = 8;

struct Re {
    num: Regex,
    num_unit: Regex,
    ver: Regex,
    uuid: Regex,
    hex: Regex,
    ip: Regex,
    addr: Regex,
    interp: Regex,
}

fn re() -> &'static Re {
    static R: OnceLock<Re> = OnceLock::new();
    R.get_or_init(|| Re {
        num: Regex::new(r"^[+-]?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?$").expect("static regex"),
        num_unit: Regex::new(r"^[0-9]+(\.[0-9]+)?([a-zA-Z]{1,3}|%)$").expect("static regex"),
        ver: Regex::new(r"^v?[0-9]+(\.[0-9]+)+([-.+][A-Za-z0-9.]+)?$").expect("static regex"),
        uuid: Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
            .expect("static regex"),
        hex: Regex::new(r"(?i)^(0x)?[0-9a-f]{16,}$").expect("static regex"),
        ip: Regex::new(r"^([0-9]{1,3}\.){3}[0-9]{1,3}$|^\[?[0-9a-fA-F]*:[0-9a-fA-F:]+\]?$").expect("static regex"),
        addr: Regex::new(r"^(\[[0-9a-fA-F:]+\]|[A-Za-z0-9.-]+):[0-9]{1,5}$").expect("static regex"),
        interp: Regex::new(
            r"^(python|pypy|Python)[0-9.]*$|^node[0-9]*$|^(ruby|perl|php|bun|deno|tsx|ts-node|lua|luajit|Rscript|julia|elixir|erl)[0-9.]*$",
        )
        .expect("static regex"),
    })
}

const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "ash", "nu",
];

fn basename(s: &str) -> &str {
    let s = s.trim_end_matches('/');
    s.rsplit('/').next().unwrap_or(s)
}

fn strip_root<'a>(v: &'a str, root: &str) -> Option<&'a str> {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return None;
    }
    let rest = v.strip_prefix(root)?;
    if rest.is_empty() {
        Some(".")
    } else {
        rest.strip_prefix('/')
    }
}

/// Normalizes one argument value (see the module table).
pub fn normalize_value(v: &str, project_root: Option<&str>) -> String {
    let r = re();
    if v.is_empty() {
        return String::new();
    }
    if v.contains(REDACTED) {
        return "<secret>".into();
    }
    if r.uuid.is_match(v) {
        return "<uuid>".into();
    }
    if r.num.is_match(v) || r.num_unit.is_match(v) {
        return "<n>".into();
    }
    if r.ip.is_match(v) {
        return "<ip>".into();
    }
    if r.ver.is_match(v) {
        return "<ver>".into();
    }
    if r.hex.is_match(v) {
        return "<hex>".into();
    }
    if v.contains("://") {
        return "<url>".into();
    }
    if r.addr.is_match(v) {
        return "<addr>".into();
    }
    let pathish = v.starts_with('/') || v.starts_with('~') || v.starts_with("./") || v.starts_with("../");
    if !pathish && v.chars().any(char::is_whitespace) {
        return "<text>".into();
    }
    if looks_like_secret(v) {
        return "<secret>".into();
    }
    if pathish || v.contains('/') {
        if let Some(rel) = project_root.and_then(|root| strip_root(v, root)) {
            return stable_root(rel);
        }
        // relative paths are already relative to the working directory (the project root)
        let relative = !v.starts_with('/') && !v.starts_with('~') && !v.contains("..") && !v.contains(':');
        if relative && project_root.is_some() {
            let rel = v.trim_start_matches("./");
            if !rel.is_empty() {
                return stable_root(rel);
            }
        }
        let b = basename(v);
        return if b.contains('.') && !b.starts_with('.') {
            "<file>".into()
        } else {
            "<path>".into()
        };
    }
    v.to_string()
}

/// Index of the Java main class / jar argument.
fn java_main(argv: &[String]) -> Option<(usize, bool)> {
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "-jar" => return (i + 1 < argv.len()).then_some((i + 1, true)),
            "-cp" | "-classpath" | "--class-path" | "-p" | "--module-path" | "--add-opens"
            | "--add-exports" | "--add-modules" | "--add-reads" => i += 2,
            "-m" | "--module" => return (i + 1 < argv.len()).then_some((i + 1, false)),
            // `@argfile` expands to more options
            _ if a.starts_with('-') || a.starts_with('@') => i += 1,
            _ => return Some((i, false)),
        }
    }
    None
}

/// For `…/node_modules/<pkg>/…` or `…/node_modules/@scope/<pkg>/…` returns the package name.
fn node_package(script: &str) -> Option<String> {
    let idx = script.rfind("node_modules/")?;
    let rest = &script[idx + "node_modules/".len()..];
    let mut parts = rest.split('/');
    let first = parts.next()?;
    if first.starts_with('@') {
        let second = parts.next()?;
        Some(format!("{first}/{second}"))
    } else if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

fn script_head(script: &str, project_root: Option<&str>) -> String {
    if let Some(pkg) = node_package(script) {
        let file = basename(script);
        let stem = file
            .strip_suffix(".js")
            .or_else(|| file.strip_suffix(".mjs"))
            .or_else(|| file.strip_suffix(".cjs"))
            .or_else(|| file.strip_suffix(".ts"))
            .unwrap_or(file);
        let pkg_name = pkg.rsplit('/').next().unwrap_or(&pkg);
        return if ["cli", "index", "main", "bin", "run", pkg_name].contains(&stem) || stem.is_empty() {
            pkg
        } else {
            format!("{pkg}/{stem}")
        };
    }
    match project_root.and_then(|r| strip_root(script, r)) {
        Some(rel) => rel.to_string(),
        None => basename(script).to_string(),
    }
}

/// Words that run the next word as the actual command (`env`, `exec`, `nohup`, …).
const PREFIX_COMMANDS: &[&str] = &["env", "exec", "nohup", "command", "time", "nice", "caffeinate"];

/// True for an environment assignment word (`FOO=bar`).
fn is_assignment(w: &str) -> bool {
    w.split_once('=')
        .map(|(k, _)| {
            !k.is_empty()
                && !k.starts_with(|c: char| c.is_ascii_digit())
                && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .unwrap_or(false)
}

/// First real command word of inline shell code (`FOO=1 exec nohup ./x --y` → `x`); never a secret.
fn shell_command_word(code: &str) -> Option<String> {
    let w = code
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '"' || c == '\'' || c == '(' || c == ')'))
        .find(|w| {
            !w.is_empty()
                && !is_assignment(w)
                && !w.starts_with('-')
                && !PREFIX_COMMANDS.contains(&basename(w))
        })?;
    let b = basename(w);
    (!b.is_empty() && redact_text(b) == b).then(|| b.to_string())
}

/// Shell short-flag cluster that takes inline code (`-c`, `-lc`, `-ec`, `-euxc`).
fn is_shell_code_flag(a: &str) -> bool {
    a.len() > 1
        && a.starts_with('-')
        && !a.starts_with("--")
        && a[1..].bytes().all(|b| b.is_ascii_alphabetic())
        && a.contains('c')
}

/// Head token and index of the first argument that follows it.
fn head_of(argv: &[String], project_root: Option<&str>) -> (String, usize) {
    let (head, next) = head_of_raw(argv, project_root);
    // the head is shown as a label: never let a secret-looking value through
    if head.is_empty() || redact_text(&head) == head {
        (head, next)
    } else {
        ("<secret>".into(), next)
    }
}

fn head_of_raw(argv: &[String], project_root: Option<&str>) -> (String, usize) {
    let r = re();
    let exe = basename(&argv[0]).trim_start_matches('-').to_string();
    if exe == "java" {
        return match java_main(argv) {
            Some((i, true)) => (basename(&argv[i]).to_string(), i + 1),
            Some((i, false)) => {
                let main = argv[i].as_str();
                let main = main.rsplit('/').next().unwrap_or(main);
                (main.rsplit('.').next().unwrap_or(main).to_string(), i + 1)
            }
            None => (exe, argv.len()),
        };
    }
    // `env [-i] [FOO=bar …] cmd …` → what `cmd` is
    if exe == "env" {
        if let Some(j) = argv
            .iter()
            .skip(1)
            .position(|a| !a.starts_with('-') && !is_assignment(a))
        {
            let (h, n) = head_of_raw(&argv[j + 1..], project_root);
            return (h, n + j + 1);
        }
        return (exe, argv.len());
    }
    let is_shell = SHELLS.contains(&exe.as_str());
    if r.interp.is_match(&exe) || is_shell {
        let node_like = exe.starts_with("node") || exe.starts_with("bun");
        let mut i = 1;
        while i < argv.len() {
            let a = argv[i].as_str();
            let inline = if is_shell {
                is_shell_code_flag(a)
            } else {
                matches!(a, "-c" | "-e" | "--eval") || (node_like && matches!(a, "-p" | "--print"))
            };
            if inline {
                // inline code: keep only the interpreter and (for shells) the first command word.
                let first = argv
                    .iter()
                    .skip(i + 1)
                    .find(|a| !a.starts_with('-'))
                    .and_then(|c| shell_command_word(c));
                return match (is_shell, first) {
                    (true, Some(w)) => (
                        format!("{exe} -c {}", normalize_value(&w, project_root)),
                        argv.len(),
                    ),
                    _ => (format!("{exe} -c"), argv.len()),
                };
            }
            match a {
                // python -m module
                "-m" if !is_shell => {
                    return match argv.get(i + 1) {
                        Some(m) => (m.clone(), i + 2),
                        None => (exe, argv.len()),
                    };
                }
                // options that take a separate value
                "-r" | "--require" | "--import" | "--loader" | "-W" | "-X" | "-I" if !is_shell => i += 2,
                "-o" | "+o" | "-O" | "+O" if is_shell => i += 2,
                _ if a.starts_with('-') || (is_shell && a.starts_with('+')) => i += 1,
                _ => return (script_head(a, project_root), i + 1),
            }
        }
        return (exe, argv.len());
    }
    (exe, 1)
}

/// argv as the program was started, when the process rewrote its title into one string (Linux
/// `setproctitle`, e.g. `nginx: master process …`, `postgres: user db …`, `node server.js --port 80`).
fn effective_argv(argv: &[String]) -> Cow<'_, [String]> {
    let [only] = argv else {
        return Cow::Borrowed(argv);
    };
    if !only.chars().any(char::is_whitespace) {
        return Cow::Borrowed(argv);
    }
    let words: Vec<&str> = only.split_whitespace().collect();
    let Some(&w0) = words.first() else {
        return Cow::Borrowed(argv);
    };
    // `name: state text` → just the name (the rest is per-connection state)
    if let Some(name) = w0.strip_suffix(':').filter(|n| !n.is_empty()) {
        return Cow::Owned(vec![name.to_string()]);
    }
    // `node server.js --port 80`, `/usr/bin/python3 /srv/app.py`
    let first = basename(w0);
    if re().interp.is_match(first) || SHELLS.contains(&first) || first == "java" || first == "env" {
        return Cow::Owned(words.iter().map(|w| w.to_string()).collect());
    }
    // `/path/with spaces/tool --flag x` → split where the options start (`App Helper (GPU)` stays whole)
    let b = only.as_bytes();
    let opt = (0..b.len().saturating_sub(2)).find(|&j| {
        b[j] == b' ' && b[j + 1] == b'-' && (b[j + 2].is_ascii_alphanumeric() || b[j + 2] == b'-')
    });
    match opt {
        Some(k) => {
            let mut out = vec![only[..k].trim_end().to_string()];
            out.extend(only[k..].split_whitespace().map(str::to_string));
            Cow::Owned(out)
        }
        None => Cow::Borrowed(argv),
    }
}

/// Builds the command-line template (UX §4 examples).
pub fn cmdline_template(argv: &[String], project_root: Option<&str>) -> String {
    if argv.is_empty() || argv[0].is_empty() {
        return String::new();
    }
    let argv = effective_argv(argv);
    let argv = argv.as_ref();
    let (head, rest_start) = head_of(argv, project_root);
    let mut out = vec![head];
    let mut next_secret = false;
    for a in argv.iter().skip(rest_start) {
        if out.len() >= MAX_TOKENS {
            break;
        }
        if next_secret {
            out.push("<secret>".into());
            next_secret = false;
            continue;
        }
        next_secret = is_secret_flag(a);
        let a = redact_text(a);
        if a == REDACTED {
            out.push("<secret>".into());
        } else if let Some((flag, val)) = a.split_once('=').filter(|(f, _)| f.starts_with('-')) {
            out.push(format!("{flag}={}", normalize_value(val, project_root)));
        } else if let Some((key, val)) = a
            .split_once('=')
            .filter(|(k, _)| !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            out.push(format!("{key}={}", normalize_value(val, project_root)));
        } else if a.starts_with('-') && a.len() > 1 && !re().num.is_match(&a) {
            out.push(a);
        } else {
            out.push(normalize_value(&a, project_root));
        }
    }
    out.join(" ")
}

/// Hex fingerprint (16 chars) of the normalized identity parts.
pub fn fingerprint(
    kind: GroupKind,
    exe_basename: &str,
    template: &str,
    project_root: Option<&str>,
    bundle_id: Option<&str>,
) -> String {
    let mut h = Sha256::new();
    h.update(b"oomtop-fp-v1\0");
    for part in [
        kind.as_str(),
        exe_basename,
        template,
        project_root.unwrap_or(""),
        bundle_id.unwrap_or(""),
    ] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn is_home_dir(c: &str) -> bool {
    let parts: Vec<&str> = c.trim_end_matches('/').split('/').collect();
    matches!(
        parts.as_slice(),
        ["", "Users", _] | ["", "home", _] | ["", "root"] | ["", "var", "root"]
    )
}

/// Project root guess: the working directory, unless it is `/`, a home directory or empty.
pub fn project_root(p: &Process) -> Option<&str> {
    p.cwd
        .as_deref()
        .map(|c| c.trim_end_matches('/'))
        .filter(|c| !c.is_empty() && !is_home_dir(c))
}

/// Template of a process (argv, or the executable when argv is unreadable; kernel threads by name).
pub fn process_template(p: &Process) -> String {
    if p.cmdline.is_empty() || p.cmdline[0].is_empty() {
        if p.exe.is_empty() {
            return p.name.clone();
        }
        return cmdline_template(std::slice::from_ref(&p.exe), project_root(p));
    }
    cmdline_template(&p.cmdline, project_root(p))
}

/// The template's first token — what the process *is* ("sd-server", "studio.py", "GradleDaemon",
/// "Google Chrome Helper (GPU)"); may contain spaces.
pub fn template_head(p: &Process) -> String {
    if p.cmdline.is_empty() || p.cmdline[0].is_empty() {
        if p.exe.is_empty() {
            return p.name.clone();
        }
        return head_of(std::slice::from_ref(&p.exe), project_root(p)).0;
    }
    head_of(&effective_argv(&p.cmdline), project_root(p)).0
}

/// Stable executable basename: the exe file name, unless it is a bare version (self-updating CLIs such as
/// `~/.local/share/claude/versions/2.1.3`), in which case argv[0] / the process name is used.
pub fn exe_basename(p: &Process) -> String {
    let r = re();
    let exe = basename(&p.exe);
    if !exe.is_empty() && !r.ver.is_match(exe) && !r.num.is_match(exe) {
        return exe.to_string();
    }
    let a0 = p
        .cmdline
        .first()
        .map(|a| basename(a).trim_start_matches('-'))
        .unwrap_or("");
    if !a0.is_empty() && !r.ver.is_match(a0) {
        return a0.to_string();
    }
    p.name.clone()
}

/// Project root with version / number / id path components placeholder-ized (`~/.gradle/daemon/9.8.0` →
/// `~/.gradle/daemon/<ver>`), so the hashed root survives upgrades.
pub fn stable_root(root: &str) -> String {
    let r = re();
    root.split('/')
        .map(|c| {
            if r.uuid.is_match(c) {
                "<uuid>"
            } else if r.num.is_match(c) {
                "<n>"
            } else if r.ver.is_match(c) {
                "<ver>"
            } else if r.hex.is_match(c) {
                "<hex>"
            } else {
                c
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Fingerprint of a process as an entity of `kind`.
pub fn process_fingerprint(kind: GroupKind, p: &Process) -> String {
    let root = project_root(p).map(stable_root);
    fingerprint(
        kind,
        &exe_basename(p),
        &process_template(p),
        root.as_deref(),
        p.bundle_id.as_deref(),
    )
}

/// Fingerprint of a group rooted at `root`. Agent sessions and apps hash only the template head (their
/// arguments are prompts, resume ids or per-launch flags), so "Claude Code in project X" or "Chrome" stays
/// one entity; everything else uses the full template.
pub fn group_fingerprint(kind: GroupKind, root: &Process) -> String {
    match kind {
        GroupKind::AgentSession | GroupKind::App => {
            let head = template_head(root);
            let head = head.as_str();
            let proj = if kind == GroupKind::App {
                None
            } else {
                project_root(root).map(stable_root)
            };
            fingerprint(
                kind,
                &exe_basename(root),
                head,
                proj.as_deref(),
                root.bundle_id.as_deref(),
            )
        }
        _ => process_fingerprint(kind, root),
    }
}

/// Derived display name, e.g. "sd-server · qwen-image-studio".
pub fn display_name(p: &Process) -> String {
    let mut head = template_head(p);
    if head.is_empty() {
        head = p.name.clone();
    }
    let r = re();
    match project_root(p).map(basename).filter(|b| {
        !b.is_empty() && *b != head && !r.ver.is_match(b) && !r.num.is_match(b) && !r.hex.is_match(b)
    }) {
        Some(proj) => format!("{head} · {proj}"),
        None => head,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn ux_examples() {
        assert_eq!(cmdline_template(&v(&["python3", "studio.py"]), None), "studio.py");
        assert_eq!(
            cmdline_template(
                &v(&[
                    "/opt/sd/bin/sd-server",
                    "--listen-port",
                    "7861",
                    "--diffusion-model",
                    "/m/qwen.gguf"
                ]),
                None
            ),
            "sd-server --listen-port <n> --diffusion-model <file>"
        );
        assert_eq!(
            cmdline_template(
                &v(&[
                    "java",
                    "-Xmx2g",
                    "-cp",
                    "/a/b.jar",
                    "org.gradle.launcher.daemon.bootstrap.GradleDaemon",
                    "9.8.0"
                ]),
                None
            ),
            "GradleDaemon <ver>"
        );
        assert_eq!(
            cmdline_template(
                &v(&["srv", "--token", "abc", "--x=sk-abcdefghijklmnopqrst"]),
                None
            ),
            "srv --token <secret> --x=<secret>"
        );
    }

    #[test]
    fn interpreters_and_placeholders() {
        let root = Some("/Users/p/proj");
        assert_eq!(
            cmdline_template(
                &v(&["python3", "/Users/p/proj/app/studio.py", "--port", "7860"]),
                root
            ),
            "app/studio.py --port <n>"
        );
        assert_eq!(
            cmdline_template(&v(&["python3.12", "-m", "http.server", "8000"]), None),
            "http.server <n>"
        );
        assert_eq!(
            cmdline_template(
                &v(&[
                    "node",
                    "/usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                    "--resume"
                ]),
                None
            ),
            "@anthropic-ai/claude-code --resume"
        );
        assert_eq!(
            cmdline_template(&v(&["/bin/sh", "-c", "next dev -p 3000"]), None),
            "sh -c next"
        );
        assert_eq!(
            cmdline_template(&v(&["bash", "-c", "-l", "cargo test"]), None),
            "bash -c cargo"
        );
        assert_eq!(
            cmdline_template(&v(&["node", "/p/node_modules/typescript/lib/tsserver.js"]), None),
            "typescript/tsserver"
        );
        assert_eq!(
            cmdline_template(
                &v(&["java", "Main", "--dir", "/Users/x/Library/Application Support/k"]),
                None
            ),
            "Main --dir <path>"
        );
        assert_eq!(
            cmdline_template(
                &v(&[
                    "srv",
                    "--bind",
                    "127.0.0.1:8080",
                    "--id",
                    "2f1c3a4b-1111-2222-3333-444455556666"
                ]),
                None
            ),
            "srv --bind <addr> --id <secret>"
        );
        assert_eq!(
            cmdline_template(&v(&["claude", "-p", "fix the failing test"]), None),
            "claude -p <text>"
        );
        assert_eq!(
            cmdline_template(&v(&["srv", "HF_TOKEN=hf_abcdefghijklmnopqrstu"]), None),
            "srv HF_TOKEN=<secret>"
        );
        assert_eq!(
            cmdline_template(&v(&["srv", "https://u:p@h/x"]), None),
            "srv <secret>"
        );
        assert_eq!(
            cmdline_template(&v(&["srv", "http://127.0.0.1:11434"]), None),
            "srv <url>"
        );
        assert_eq!(normalize_value("10.0.0.1", None), "<ip>");
        assert_eq!(normalize_value("/Users/p/proj", root), ".");
        assert_eq!(normalize_value("/Users/p/project2/x", root), "<path>");
    }

    #[test]
    fn fingerprints_survive_port_and_version_changes() {
        let a = cmdline_template(&v(&["sd-server", "--listen-port", "7861"]), None);
        let b = cmdline_template(&v(&["sd-server", "--listen-port", "7862"]), None);
        assert_eq!(
            fingerprint(GroupKind::ModelServer, "sd-server", &a, None, None),
            fingerprint(GroupKind::ModelServer, "sd-server", &b, None, None)
        );
        assert_eq!(fingerprint(GroupKind::App, "x", "y", None, None).len(), 16);
        let p = |exe: &str| Process {
            exe: exe.into(),
            name: "claude".into(),
            cmdline: v(&["claude"]),
            cwd: Some("/Users/p/proj".into()),
            ..Default::default()
        };
        let a = p("/Users/p/.local/share/claude/versions/2.1.3");
        let b = p("/Users/p/.local/share/claude/versions/2.2.0");
        assert_eq!(exe_basename(&a), "claude");
        assert_eq!(
            group_fingerprint(GroupKind::AgentSession, &a),
            group_fingerprint(GroupKind::AgentSession, &b)
        );
        assert_eq!(display_name(&a), "claude · proj");
        assert_eq!(
            stable_root("/Users/p/.gradle/daemon/9.8.0"),
            "/Users/p/.gradle/daemon/<ver>"
        );
    }

    #[test]
    fn shell_inline_code_never_leaks_assignments() {
        for (argv, want) in [
            (&["sh", "-c", "API_KEY=sk-123 node x.js"][..], "sh -c node"),
            (
                &["bash", "-ec", "FOO=1 exec nohup ./serve --port 80"][..],
                "bash -c serve",
            ),
            (&["zsh", "-lc", "env GITHUB_TOKEN=abc make"][..], "zsh -c make"),
            (&["sh", "-c", "sk-ant-api03-abcdefghijklmnop"][..], "sh -c"),
        ] {
            let t = cmdline_template(&v(argv), None);
            assert_eq!(t, want, "{argv:?}");
        }
    }

    #[test]
    fn rewritten_titles_are_split_and_redacted() {
        assert_eq!(
            cmdline_template(&v(&["node server.js --token=abc123 --port 8080"]), None),
            "server.js --token=<secret> --port <n>"
        );
        assert_eq!(
            cmdline_template(&v(&["postgres: dev app 127.0.0.1(5432) idle"]), None),
            "postgres"
        );
        assert_eq!(
            cmdline_template(&v(&["/usr/sbin/nginx -g daemon off;"]), None),
            "nginx -g daemon off;"
        );
        // a path with spaces is not a title
        let chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome Helper (GPU)";
        assert_eq!(
            cmdline_template(&v(&[chrome]), None),
            "Google Chrome Helper (GPU)"
        );
        let p = Process {
            cmdline: v(&["mytool --api-key sk-proj-abcdefghijklmnopqrstuv"]),
            ..Default::default()
        };
        assert_eq!(template_head(&p), "mytool");
        assert!(!display_name(&p).contains("sk-"));
        // a secret-looking head is never shown
        let p = Process {
            cmdline: v(&["/opt/x/ghp_abcdefghijklmnopqrstuvwxyz0123"]),
            ..Default::default()
        };
        assert_eq!(template_head(&p), "<secret>");
    }

    #[test]
    fn interpreter_flag_arity() {
        assert_eq!(cmdline_template(&v(&["python3", "-O", "app.py"]), None), "app.py");
        assert_eq!(
            cmdline_template(&v(&["python3", "-X", "dev", "app.py"]), None),
            "app.py"
        );
        assert_eq!(
            cmdline_template(&v(&["bash", "-o", "pipefail", "build.sh"]), None),
            "build.sh"
        );
        assert_eq!(
            cmdline_template(&v(&["node", "-p", "process.env.HOME"]), None),
            "node -c"
        );
        assert_eq!(
            cmdline_template(
                &v(&["/usr/bin/env", "-i", "FOO=1", "python3", "tool.py", "--n", "3"]),
                None
            ),
            "tool.py --n <n>"
        );
        assert_eq!(
            cmdline_template(
                &v(&["java", "@/tmp/args.txt", "-cp", "a.jar", "com.x.Main"]),
                None
            ),
            "Main"
        );
    }

    #[test]
    fn relative_paths_are_project_relative() {
        let root = Some("/Users/p/proj");
        assert_eq!(normalize_value("./src/main.rs", root), "src/main.rs");
        assert_eq!(normalize_value("build/tmp/12345/out", root), "build/tmp/<n>/out");
        assert_eq!(normalize_value("/Users/p/proj/out/9.1.0/x", root), "out/<ver>/x");
        assert_eq!(normalize_value("../other/x.rs", root), "<file>");
        assert_eq!(normalize_value("src/main.rs", None), "<file>");
    }

    #[test]
    fn home_is_not_a_project() {
        let p = Process {
            cwd: Some("/Users/user".into()),
            ..Default::default()
        };
        assert_eq!(project_root(&p), None);
    }
}

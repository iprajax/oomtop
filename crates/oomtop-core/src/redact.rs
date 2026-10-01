//! Privacy (SPEC §13): command-line redaction for exports, the marker-key allowlist, and marker hashing.
//!
//! - Environments are read only for allowlisted keys; values are never stored (session ids are hashed).
//! - Every export (json, ndjson, serve, MCP) must call [`redact_snapshot`].
//! - Redaction is deliberately conservative: when in doubt a value is replaced. Things redacted:
//!   `--token=…`-style flags and the argument after a bare secret flag (`--api-key sk-…`), `KEY=value`
//!   assignments (any ALL-CAPS environment-style assignment and any key naming a secret), credentials in URLs
//!   (`scheme://user:pw@host`, `scheme://token@host`) and secret query parameters, `Authorization`-style
//!   headers, well-known token formats (OpenAI/Anthropic/GitHub/GitLab/Slack/AWS/HF/Google/Stripe/npm/JWT),
//!   UUIDs (session ids are often passed on the command line) and long high-entropy strings.

use crate::model::{Markers, Snapshot};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

pub const REDACTED: &str = "<redacted>";

/// Default marker-key allowlist (config `privacy.marker_allowlist` extends/overrides it).
/// Keys only — e.g. `CLAUDE_CODE_MESSAGING_TOKEN` is deliberately absent.
pub const DEFAULT_MARKER_ALLOWLIST: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_ENTRYPOINT",
    "CODEX_SANDBOX",
    "CODEX_SESSION_ID",
    "CURSOR_AGENT",
    "CURSOR_TRACE_ID",
    "GEMINI_CLI",
    "GOOSE_SESSION_ID",
    "AIDER_SESSION",
    "OOMTOP_SESSION",
    "TERM_PROGRAM",
];

/// Minimum length of a standalone string considered for high-entropy redaction.
const ENTROPY_MIN_LEN: usize = 32;
/// Shannon entropy (bits per char) above which a long token-shaped string is treated as a secret.
const ENTROPY_BITS: f64 = 4.2;

/// True if `key` is in the allowlist (exact match; a trailing `*` in an entry is a prefix match).
pub fn is_allowlisted(key: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|a| match a.strip_suffix('*') {
        Some(prefix) => key.starts_with(prefix),
        None => a == key,
    })
}

pub fn default_allowlist() -> Vec<String> {
    DEFAULT_MARKER_ALLOWLIST.iter().map(|s| s.to_string()).collect()
}

/// Stable, non-reversible hash of a marker value (hex, 16 chars).
pub fn hash_marker(value: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"oomtop-marker-v1:");
    h.update(value.as_bytes());
    let d = h.finalize();
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn agent_for_key(key: &str) -> Option<&'static str> {
    if key.starts_with("CLAUDE") {
        Some("claude-code")
    } else if key.starts_with("CODEX") {
        Some("codex")
    } else if key.starts_with("CURSOR") {
        Some("cursor")
    } else if key.starts_with("GEMINI") {
        Some("gemini")
    } else if key.starts_with("GOOSE") {
        Some("goose")
    } else if key.starts_with("AIDER") {
        Some("aider")
    } else {
        None
    }
}

/// Builds [`Markers`] from environment pairs. Non-allowlisted pairs are skipped without being copied; the
/// only value that survives is a *hash* of the first non-empty `*SESSION_ID` value.
pub fn markers_from_env<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
    allowlist: &[String],
) -> Markers {
    let mut m = Markers::default();
    for (k, v) in pairs {
        if !is_allowlisted(k, allowlist) {
            continue;
        }
        m.keys.push(k.to_string());
        if m.agent.is_none() {
            m.agent = agent_for_key(k).map(str::to_string);
        }
        if k.ends_with("SESSION_ID") && !v.trim().is_empty() && m.session_id.is_none() {
            m.session_id = Some(hash_marker(v));
        }
    }
    m.keys.sort();
    m.keys.dedup();
    m
}

struct Patterns {
    flag_eq: Regex,
    kv_secret: Regex,
    kv_env: Regex,
    url_creds: Regex,
    url_query: Regex,
    auth_header: Regex,
    bearer: Regex,
    known_tokens: Regex,
    uuid: Regex,
    token_shape: Regex,
    hex_long: Regex,
    inline_flag: Regex,
    inline_kv: Regex,
    inline_userpass: Regex,
    inline_mysql: Regex,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| Patterns {
        flag_eq: Regex::new(
            r"(?is)^(--?[a-z0-9_.-]*(token|api[-_]?key|secret|password|passwd|pass|pwd|auth|credential|cookie|bearer|private[-_]?key|key)[a-z0-9_.-]*)=(.+)$",
        )
        .expect("static regex"),
        kv_secret: Regex::new(
            r"(?s)^([A-Za-z_][A-Za-z0-9_.-]*(?i:token|key|secret|password|passwd|pass|pwd|auth|cred|cookie)[A-Za-z0-9_.-]*)=(.+)$",
        )
        .expect("static regex"),
        // ALL-CAPS environment-style assignment (`FOO=bar cmd`, `env HF_HOME=… …`): values may be secrets.
        kv_env: Regex::new(r"(?s)^([A-Z][A-Z0-9_]*)=(.+)$").expect("static regex"),
        // Any userinfo in a URL (user:password@ or token@) is a credential.
        url_creds: Regex::new(r"([a-zA-Z][a-zA-Z0-9+.-]*://)[^/\s@?#]+@").expect("static regex"),
        url_query: Regex::new(
            r"(?i)([?&;](?:[a-z0-9_.-]*(?:token|key|secret|password|passwd|auth|sig|signature|credential|session)[a-z0-9_.-]*|code)=)[^&;#\s]+",
        )
        .expect("static regex"),
        auth_header: Regex::new(
            r"(?i)((?:proxy-)?authorization:\s*(?:basic|bearer|token|digest)?\s*|x-api-key:\s*|api-key:\s*|cookie:\s*|x-auth-token:\s*)\S.*$",
        )
        .expect("static regex"),
        bearer: Regex::new(r"(?i)\b(bearer\s+)[A-Za-z0-9._~+/=-]+").expect("static regex"),
        known_tokens: Regex::new(concat!(
            r"(sk-ant-[A-Za-z0-9_-]{10,}|sk-(?:proj-|live-|test-)?[A-Za-z0-9_-]{16,}|[sr]k_(?:live|test)_[A-Za-z0-9]{10,}",
            r"|gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,}",
            r"|xox[abprs]-[A-Za-z0-9-]{10,}|AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}|hf_[A-Za-z0-9]{20,}",
            r"|AIza[0-9A-Za-z_-]{35}|npm_[A-Za-z0-9]{36}|pypi-[A-Za-z0-9_-]{50,}|gsk_[A-Za-z0-9]{20,}|xai-[A-Za-z0-9]{20,}",
            r"|eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})"
        ))
        .expect("static regex"),
        uuid: Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").expect("static regex"),
        token_shape: Regex::new(r"^[A-Za-z0-9_+/=-]+$").expect("static regex"),
        hex_long: Regex::new(r"(?i)^[0-9a-f]{32,}$").expect("static regex"),
        // Inside a string with whitespace (`sh -c "…"`, a rewritten process title): `--api-key=v`,
        // `--token v` (the value must not itself be a flag).
        inline_flag: Regex::new(
            r#"(?i)((?:^|\s)--?[a-z0-9_.-]*(?:token|api[-_]?key|secret|password|passphrase|passwd|auth|credential|cookie|private[-_]?key)[a-z0-9_.-]*)(=|\s+)("[^"]*"|'[^']*'|[^\s"'-][^\s]*)"#,
        )
        .expect("static regex"),
        // …and `NAME=value` assignments that are ALL-CAPS or name a secret (`OPENAI_API_KEY=sk-… node x`).
        inline_kv: Regex::new(
            r#"((?:^|[\s;&|(])(?:[A-Z][A-Z0-9_]*|[A-Za-z_][A-Za-z0-9_]*(?i:token|key|secret|password|passwd|pwd|auth|cred|cookie)[A-Za-z0-9_]*)=)("[^"]*"|'[^']*'|[^\s;&|)]+)"#,
        )
        .expect("static regex"),
        // `curl -u bob:pw`, `--user=bob:pw`, `--proxy-user 'al:pw'` inside a shell string: keep the user name.
        // A value with `://` is a URL (handled by `url_creds`), not a user:password pair.
        inline_userpass: Regex::new(
            r#"((?:^|\s)(?:-u|-U|--user|--proxy-user)(?:=|\s+)["']?)([^\s:"'/]+):([^\s"'/][^\s"']*|/[^\s"'/][^\s"']*)"#,
        )
        .expect("static regex"),
        // `mysql -pSECRET` / `mysqldump … -pSECRET` inside a shell string (up to the end of that command).
        inline_mysql: Regex::new(
            r#"(?i)((?:^|[\s;&|(/"'`])(?:mysql|mariadb)[a-z_-]*(?:\s[^;&|\n]*?)?\s)-p([^\s"']+)"#,
        )
        .expect("static regex"),
    })
}

/// Words that name a secret anywhere at the end of a flag, compared after lowercasing and removing `-`/`_`
/// (so `--accessToken`, `--client_secret`, `--githubToken`, `--db-passphrase` all match).
const SECRET_FLAG_SUFFIXES: &[&str] = &[
    "token",
    "secret",
    "password",
    "passphrase",
    "passwd",
    "apikey",
    "credential",
    "credentials",
    "privatekey",
    "cookie",
    "auth",
    "bearer",
];

/// True if a bare flag (e.g. `--token`, `--api-key`, `--password`, `--accessToken`) takes a secret as its
/// next argument.
pub fn is_secret_flag(arg: &str) -> bool {
    if !arg.starts_with('-') || arg.contains('=') {
        return false;
    }
    let a = arg.trim_start_matches('-').to_ascii_lowercase();
    if a.is_empty() {
        return false;
    }
    let squashed: String = a.chars().filter(|c| *c != '-' && *c != '_').collect();
    if SECRET_FLAG_SUFFIXES.iter().any(|k| squashed.ends_with(k)) {
        return true;
    }
    // Short words that are only secrets as a whole word (`--pass`, `--ssh-key`, not `--bypass`/`--monkey`).
    ["pass", "key"]
        .iter()
        .any(|k| a == *k || a.ends_with(&format!("-{k}")) || a.ends_with(&format!("_{k}")))
}

/// Flags whose next argument is `user:password` (curl `-u`, `--user`, `--proxy-user`, wget/httpie style).
fn is_userpass_flag(arg: &str) -> bool {
    matches!(arg, "-u" | "-U" | "--user" | "--proxy-user")
}

/// `user:password` → `user:<redacted>` (values without `:` are left alone: `-u` is often just a name).
fn redact_userpass(v: &str) -> Option<String> {
    let (user, pw) = v.split_once(':')?;
    (!pw.is_empty() && !v.contains("://")).then(|| format!("{user}:{REDACTED}"))
}

fn shannon_bits(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    let bytes = s.as_bytes();
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let n = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// True if a standalone value looks like a secret or an identifier that must not leave the machine:
/// UUIDs (session ids), long hex strings, or long high-entropy token-shaped strings.
pub fn looks_like_secret(v: &str) -> bool {
    let p = patterns();
    if p.uuid.is_match(v) || p.hex_long.is_match(v) {
        return true;
    }
    if v.len() < ENTROPY_MIN_LEN || !p.token_shape.is_match(v) || v.starts_with('/') {
        return false;
    }
    let has_digit = v.bytes().any(|b| b.is_ascii_digit());
    let has_alpha = v.bytes().any(|b| b.is_ascii_alphabetic());
    has_digit && has_alpha && shannon_bits(v) >= ENTROPY_BITS
}

/// Redacts secrets inside one string (flags with `=`, KEY=value, URL credentials/query tokens, auth
/// headers, bearer tokens, known token formats, UUIDs and high-entropy values).
pub fn redact_text(s: &str) -> String {
    let p = patterns();
    if let Some(c) = p.flag_eq.captures(s) {
        return format!("{}={REDACTED}", &c[1]);
    }
    let spaced = s.chars().any(char::is_whitespace);
    // A key that names a secret loses its whole value (it may contain spaces); a generic ALL-CAPS
    // assignment at the start of a shell string (`FOO=1 cmd …`) is handled word by word below.
    if let Some(c) = p
        .kv_secret
        .captures(s)
        .or_else(|| (!spaced).then(|| p.kv_env.captures(s)).flatten())
    {
        return format!("{}={REDACTED}", &c[1]);
    }
    // `--flag=value` / `key=value` with a secret-looking value (or a nested assignment such as
    // `--env=OPENAI_API_KEY=sk-…`); otherwise the whole string.
    match s.split_once('=') {
        Some((k, v)) if !k.is_empty() && !v.bytes().all(|b| b == b'=') && !k.contains("://") && !spaced => {
            if looks_like_secret(v) {
                return format!("{k}={REDACTED}");
            }
            if v.contains('=') && !v.contains("://") {
                let inner = redact_text(v);
                if inner != v {
                    return format!("{k}={inner}");
                }
            }
        }
        _ => {
            if looks_like_secret(s) {
                return REDACTED.to_string();
            }
        }
    }
    let mut s = std::borrow::Cow::Borrowed(s);
    if spaced {
        // MySQL first: a password that happens to contain a secret word (`-pSECRET`, `-pmytoken`) would
        // otherwise read as a secret *flag* and take the next word (the database name) with it.
        s = p
            .inline_mysql
            .replace_all(&s, format!("${{1}}-p{REDACTED}").as_str())
            .into_owned()
            .into();
        s = p
            .inline_flag
            .replace_all(&s, format!("${{1}}${{2}}{REDACTED}").as_str())
            .into_owned()
            .into();
        s = p
            .inline_kv
            .replace_all(&s, format!("${{1}}{REDACTED}").as_str())
            .into_owned()
            .into();
        s = p
            .inline_userpass
            .replace_all(&s, format!("${{1}}${{2}}:{REDACTED}").as_str())
            .into_owned()
            .into();
    }
    let s = p
        .auth_header
        .replace_all(&s, format!("${{1}}{REDACTED}").as_str());
    let s = p.url_creds.replace_all(&s, format!("${{1}}{REDACTED}@").as_str());
    let s = p.url_query.replace_all(&s, format!("${{1}}{REDACTED}").as_str());
    let s = p.bearer.replace_all(&s, format!("${{1}}{REDACTED}").as_str());
    let s = p.known_tokens.replace_all(&s, REDACTED);
    if spaced {
        // standalone UUIDs / long hex / high-entropy words inside free text
        let words: Vec<&str> = s.split(' ').collect();
        if words
            .iter()
            .any(|w| looks_like_secret(w.trim_matches(|c| c == '"' || c == '\'')))
        {
            return words
                .iter()
                .map(|w| {
                    if looks_like_secret(w.trim_matches(|c| c == '"' || c == '\'')) {
                        REDACTED
                    } else {
                        w
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
        }
    }
    s.into_owned()
}

/// Redacts an argv: each element via [`redact_text`], the value following a secret flag, `user:password`
/// after `-u`/`--user`/`--proxy-user`, and MySQL-style attached passwords (`mysql -pSECRET`).
pub fn redact_cmdline(argv: &[String]) -> Vec<String> {
    // `mysql …`, but also `sudo mysql …`, `env X=1 mariadb …` and a shell command split into words
    // (`zsh -c mysql -pSECRET db`): a `-p…` after a MySQL client word, before the next `;`/`&&`/`|`.
    let is_mysql = |a: &str| {
        let base = a
            .rsplit('/')
            .next()
            .unwrap_or(a)
            .trim_start_matches(['"', '\'', '`']);
        base.starts_with("mysql") || base.starts_with("mariadb")
    };
    let mut mysql_like = false;
    let mut out = Vec::with_capacity(argv.len());
    let mut next_secret = false;
    let mut next_userpass = false;
    for a in argv {
        if next_secret {
            out.push(REDACTED.to_string());
            next_secret = false;
            next_userpass = false;
            continue;
        }
        if next_userpass {
            next_userpass = false;
            if let Some(r) = redact_userpass(a) {
                out.push(r);
                continue;
            }
        }
        if let Some((flag, v)) = a.split_once('=') {
            if is_userpass_flag(flag) {
                if let Some(r) = redact_userpass(v) {
                    out.push(format!("{flag}={r}"));
                    continue;
                }
            }
        }
        if mysql_like && a.len() > 2 && a.starts_with("-p") && !a.starts_with("--") {
            out.push(format!("-p{REDACTED}"));
            continue;
        }
        if is_mysql(a) {
            mysql_like = true;
        } else if matches!(a.as_str(), ";" | "&&" | "||" | "|" | "&") || a.ends_with(';') {
            mysql_like = false;
        }
        next_secret = is_secret_flag(a);
        next_userpass = !next_secret && is_userpass_flag(a);
        out.push(redact_text(a));
    }
    out
}

/// Clone of the snapshot safe for export: redacted command lines and paths; markers keep keys + hashed ids
/// only; model-server endpoints, model file paths, loaded-model names, job labels and every free-text label
/// derived from process data (group / sandbox labels, OOM victim names) go through [`redact_text`]. This is
/// the one export view: json, ndjson, serve and MCP all build on it.
pub fn redact_snapshot(s: &Snapshot) -> Snapshot {
    let mut out = s.clone();
    for p in &mut out.processes {
        p.cmdline = redact_cmdline(&p.cmdline);
        p.exe = redact_text(&p.exe);
        p.name = redact_text(&p.name);
        if let Some(cwd) = &p.cwd {
            p.cwd = Some(redact_text(cwd));
        }
        for f in &mut p.model_files {
            *f = redact_text(f);
        }
        // Markers never carry raw values; drop anything that is not a known hash shape defensively.
        if let Some(sid) = &p.markers.session_id {
            if sid.len() != 16 || !sid.bytes().all(|b| b.is_ascii_hexdigit()) {
                p.markers.session_id = Some(hash_marker(sid));
            }
        }
    }
    for g in &mut out.groups {
        g.label = redact_text(&g.label);
    }
    for sb in &mut out.sandboxes {
        sb.label = redact_text(&sb.label);
    }
    if let Some(v) = &mut out.oom.likely_victim {
        v.name = redact_text(&v.name);
    }
    for k in &mut out.oom.recent_kills {
        if let Some(n) = &k.victim_name {
            k.victim_name = Some(redact_text(n));
        }
    }
    for m in &mut out.model_servers {
        if let Some(e) = &m.endpoint {
            m.endpoint = Some(redact_text(e));
        }
        for model in &mut m.models {
            if let Some(f) = &model.file {
                model.file = Some(redact_text(f));
            }
            model.name = redact_text(&model.name);
        }
        // Job labels come from a model server's API and can carry user text (a prompt, a file name).
        if let Some(p) = &mut m.progress {
            p.label = redact_text(&p.label);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn redacts_cmdlines() {
        let r = redact_cmdline(&v(&[
            "server",
            "--token=abc123",
            "--api-key",
            "sk-verysecretvalue123456",
            "HF_TOKEN=hf_xxx",
            "https://user:pw@example.com/x?token=zzz&a=1",
            "--port",
            "7861",
            "Authorization: Bearer abc.def",
        ]));
        assert_eq!(
            r,
            v(&[
                "server",
                "--token=<redacted>",
                "--api-key",
                "<redacted>",
                "HF_TOKEN=<redacted>",
                "https://<redacted>@example.com/x?token=<redacted>&a=1",
                "--port",
                "7861",
                "Authorization: Bearer <redacted>",
            ])
        );
        assert_eq!(redact_text("run sk-ant-api03-abcdefghijkl"), "run <redacted>");
    }

    #[test]
    fn env_assignments_and_ids() {
        assert_eq!(redact_text("OPENAI_BASE=http://x"), "OPENAI_BASE=<redacted>");
        assert_eq!(redact_text("db_password=hunter2"), "db_password=<redacted>");
        assert_eq!(redact_text("2f1c3a4b-1111-2222-3333-444455556666"), REDACTED);
        assert_eq!(
            redact_text("--session=2f1c3a4b-1111-2222-3333-444455556666"),
            "--session=<redacted>"
        );
        assert_eq!(
            redact_text("https://ghp_abcdefghijklmnopqrstuvwx@github.com/o/r"),
            "https://<redacted>@github.com/o/r"
        );
        assert_eq!(redact_text("Qx7fT2mZp9LkV4bN8wR1sY6cH3jD0aEu"), REDACTED);
        // ordinary arguments survive
        for keep in [
            "--listen-port",
            "7861",
            "org.gradle.launcher.daemon.bootstrap.GradleDaemon",
            "/Users/x/models/qwen-image-edit-2509-Q4_K_M.gguf",
            "--add-opens=java.base/java.lang=ALL-UNNAMED",
            "Qwen-Image-Edit-2509-Q4_K_M",
            "http://127.0.0.1:11434/api/ps",
        ] {
            assert_eq!(redact_text(keep), keep, "{keep}");
        }
    }

    #[test]
    fn secrets_inside_shell_strings_and_titles() {
        let cases = [
            ("OPENAI_API_KEY=sk-abc123 node x.js", "OPENAI_API_KEY=<redacted>"),
            (
                "cd /x && GITHUB_TOKEN=abc123 make deploy",
                "cd /x && GITHUB_TOKEN=<redacted> make deploy",
            ),
            (
                "curl --token abc123 --verbose https://h",
                "curl --token <redacted> --verbose https://h",
            ),
            (
                "node server.js --api-key=abc123 --port 80",
                "node server.js --api-key=<redacted> --port 80",
            ),
            ("run --auth --verbose", "run --auth --verbose"),
            (
                "FOO=bar HF_HOME=/m python app.py",
                "FOO=<redacted> HF_HOME=<redacted> python app.py",
            ),
            (
                "resume 2f1c3a4b-1111-2222-3333-444455556666 now",
                "resume <redacted> now",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(redact_text(input), want, "{input}");
        }
        // nested assignment inside a non-secret flag
        assert_eq!(
            redact_text("--env=OPENAI_API_KEY=sk-abc"),
            "--env=OPENAI_API_KEY=<redacted>"
        );
        // multi-line values never escape the anchored patterns
        assert_eq!(redact_text("--token=abc\ndef"), "--token=<redacted>");
        assert_eq!(redact_text("API_KEY=abc\ndef"), "API_KEY=<redacted>");
        // ordinary free text survives
        assert_eq!(redact_text("fix the failing test"), "fix the failing test");
        assert_eq!(
            redact_text("nginx: master process /usr/sbin/nginx -g daemon off;"),
            "nginx: master process /usr/sbin/nginx -g daemon off;"
        );
    }

    #[test]
    fn export_redacts_labels() {
        let mut s = Snapshot::default();
        s.groups.push(crate::model::Group {
            label: "srv --token=abc123 x".into(),
            ..Default::default()
        });
        s.sandboxes.push(crate::model::Sandbox {
            label: "API_KEY=zzz".into(),
            ..Default::default()
        });
        s.oom.likely_victim = Some(crate::model::Victim {
            name: "tool --password hunter2".into(),
            ..Default::default()
        });
        s.processes.push(crate::model::Process {
            name: "tool API_KEY=qqq".into(),
            ..Default::default()
        });
        let json = serde_json::to_string(&redact_snapshot(&s)).unwrap();
        for leak in ["abc123", "zzz", "hunter2", "qqq"] {
            assert!(!json.contains(leak), "{leak} in {json}");
        }
    }

    #[test]
    fn markers_allowlist_only() {
        let allow = default_allowlist();
        let m = markers_from_env(
            [
                ("CLAUDECODE", "1"),
                ("CLAUDE_CODE_SESSION_ID", "sess-1"),
                ("CLAUDE_CODE_MESSAGING_TOKEN", "secret"),
                ("HOME", "/Users/x"),
            ],
            &allow,
        );
        assert_eq!(m.keys, v(&["CLAUDECODE", "CLAUDE_CODE_SESSION_ID"]));
        assert_eq!(m.agent.as_deref(), Some("claude-code"));
        assert_eq!(m.session_id, Some(hash_marker("sess-1")));
        assert_ne!(m.session_id.as_deref(), Some("sess-1"));
        assert!(is_allowlisted("AIDER_X", &["AIDER_*".to_string()]));
    }

    #[test]
    fn snapshot_export_is_redacted() {
        let mut s = Snapshot::default();
        s.processes.push(crate::model::Process {
            cmdline: v(&["srv", "--password", "hunter2", "KEY=v"]),
            ..Default::default()
        });
        s.processes[0].markers.session_id = Some("raw-session-value".into());
        let r = redact_snapshot(&s);
        assert_eq!(
            r.processes[0].cmdline,
            v(&["srv", "--password", REDACTED, "KEY=<redacted>"])
        );
        assert_eq!(
            r.processes[0].markers.session_id,
            Some(hash_marker("raw-session-value"))
        );
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("hunter2") && !json.contains("raw-session-value"));
    }

    #[test]
    fn camel_case_and_passphrase_flags_take_the_next_argument() {
        for flag in [
            "--accessToken",
            "--authToken",
            "--clientSecret",
            "--githubToken",
            "--passphrase",
            "--db_passphrase",
            "--apiKey",
            "--privateKey",
            "--sessionCookie",
            "--oauth",
            "--token",
            "--ssh-key",
            "--pass",
        ] {
            assert!(is_secret_flag(flag), "{flag}");
            assert_eq!(
                redact_cmdline(&v(&["tool", flag, "QQvalue1"])),
                v(&["tool", flag, REDACTED]),
                "{flag}"
            );
        }
        for flag in ["--bypass", "--monkey", "--author", "--verbose", "--port", "-p"] {
            assert!(!is_secret_flag(flag), "{flag}");
        }
        assert_eq!(
            redact_cmdline(&v(&["x", "--accessToken=EQtok1"])),
            v(&["x", "--accessToken=<redacted>"])
        );
        assert_eq!(
            redact_cmdline(&v(&["x", "--passphrase=QQpass8"])),
            v(&["x", "--passphrase=<redacted>"])
        );
    }

    #[test]
    fn user_password_pairs_and_mysql_passwords() {
        assert_eq!(
            redact_cmdline(&v(&["curl", "-u", "bob:QQcurlpw5", "https://x"])),
            v(&["curl", "-u", "bob:<redacted>", "https://x"])
        );
        assert_eq!(
            redact_cmdline(&v(&["curl", "--user", "alice:QQcurlpw6", "--proxy-user=u:p"])),
            v(&["curl", "--user", "alice:<redacted>", "--proxy-user=u:<redacted>"])
        );
        // A bare user name after -u is not a secret.
        assert_eq!(
            redact_cmdline(&v(&["ps", "-u", "root"])),
            v(&["ps", "-u", "root"])
        );
        assert_eq!(
            redact_cmdline(&v(&["/usr/bin/mysql", "-pQQmysql7", "-h", "db"])),
            v(&["/usr/bin/mysql", "-p<redacted>", "-h", "db"])
        );
        assert_eq!(redact_cmdline(&v(&["ssh", "-p2222"])), v(&["ssh", "-p2222"]));
        // In a free-text process title too.
        let t = redact_text("node server.js --accessToken hunter2hunterBBB --passphrase correcthorseCCC");
        assert!(
            !t.contains("hunter2hunterBBB") && !t.contains("correcthorseCCC"),
            "{t}"
        );
    }

    #[test]
    fn user_password_pairs_and_mysql_passwords_inside_shell_strings() {
        let cases = [
            (
                "curl -u bob:hunter2 https://x",
                "curl -u bob:<redacted> https://x",
            ),
            (
                "curl --proxy-user al:pw -s https://x",
                "curl --proxy-user al:<redacted> -s https://x",
            ),
            (
                "curl --user='carol:QQpw 9' https://x",
                "curl --user='carol:<redacted> 9' https://x",
            ),
            ("curl -u \"dave:pw\" x", "curl -u \"dave:<redacted>\" x"),
            (
                "cd /srv && mysql -pSECRET -h db app",
                "cd /srv && mysql -p<redacted> -h db app",
            ),
            (
                "/usr/bin/mysqldump -u root -pS3cr3t app > dump.sql",
                "/usr/bin/mysqldump -u root -p<redacted> app > dump.sql",
            ),
            ("mariadb -h db -pQQ7 x", "mariadb -h db -p<redacted> x"),
        ];
        for (input, want) in cases {
            assert_eq!(redact_text(input), want, "{input}");
        }
        // Not user:password pairs / not MySQL: left alone.
        for keep in [
            "ps -u root -o pid",
            "curl -u https://h/x",
            "rsync -p src/ dst/",
            "mysql -p -h db",
            "echo done; ssh -p2222 host",
        ] {
            assert_eq!(redact_text(keep), keep, "{keep}");
        }
        // The whole argv of `zsh -c '…'`, as `oomtop json` exports it.
        let argv = v(&[
            "/bin/zsh",
            "-c",
            "curl -u bob:hunter2 --proxy-user al:pw https://x && mysql -pSECRET db",
        ]);
        let out = redact_cmdline(&argv).join(" ");
        for leak in ["hunter2", ":pw", "SECRET"] {
            assert!(!out.contains(leak), "{leak} leaked: {out}");
        }
        assert!(
            out.contains("bob:<redacted>") && out.contains("-p<redacted>"),
            "{out}"
        );
    }

    #[test]
    fn mysql_passwords_take_only_the_password() {
        // A password containing a secret word must not read as a secret flag that eats the database name.
        for (input, want) in [
            ("mysql -pSECRET db;", "mysql -p<redacted> db;"),
            ("mysql -pmytoken db; echo ok", "mysql -p<redacted> db; echo ok"),
            ("mysql -pAuth9 -h db app", "mysql -p<redacted> -h db app"),
            // quoted shell strings as a whole argument (`zsh -c 'mysql -p… db'` printed as one string)
            ("zsh -c 'mysql -pPW db;'", "zsh -c 'mysql -p<redacted> db;'"),
            (
                "sh -c \"mysqldump -pPW db\"",
                "sh -c \"mysqldump -p<redacted> db\"",
            ),
        ] {
            assert_eq!(redact_text(input), want, "{input}");
        }
        let cases: [(&[&str], &[&str]); 5] = [
            // shell words split into argv: `-p…` after a MySQL word is a password even if argv[0] is a shell
            (
                &["/bin/zsh", "-c", "mysql", "-pSECRET", "db;"],
                &["/bin/zsh", "-c", "mysql", "-p<redacted>", "db;"],
            ),
            (
                &["sudo", "/usr/bin/mariadb", "-u", "root", "-pPW", "app"],
                &["sudo", "/usr/bin/mariadb", "-u", "root", "-p<redacted>", "app"],
            ),
            (&["mysql", "-pSECRET", "db;"], &["mysql", "-p<redacted>", "db;"]),
            // …but not after the command ended: `ssh -p2222` is a port
            (
                &["sh", "-c", "mysql", "db", ";", "ssh", "-p2222", "host"],
                &["sh", "-c", "mysql", "db", ";", "ssh", "-p2222", "host"],
            ),
            (&["rsync", "-p", "src/", "dst/"], &["rsync", "-p", "src/", "dst/"]),
        ];
        for (input, want) in cases {
            assert_eq!(redact_cmdline(&v(input)), v(want), "{input:?}");
        }
    }

    #[test]
    fn model_names_and_job_labels_are_redacted_in_every_export() {
        let secret = "sk-ant-api03-verysecretvalue1234567890abcdef";
        let mut s = Snapshot::default();
        s.model_servers.push(crate::model::ModelServer {
            models: vec![crate::model::LoadedModel {
                name: format!("model --token={secret}"),
                ..Default::default()
            }],
            progress: Some(crate::model::JobProgress {
                label: format!("prompt with {secret}"),
                ..Default::default()
            }),
            ..Default::default()
        });
        let json = serde_json::to_string(&redact_snapshot(&s)).unwrap();
        assert!(!json.contains(secret), "{json}");
    }
}

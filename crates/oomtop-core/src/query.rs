//! Query understanding (UX §5.2): one engine for `/` filters, `:` commands and the `Ctrl-K` palette.
//!
//! **Filter grammar** (defined once; used by `/`, aliases, saved views and rules):
//! - terms separated by spaces are AND (`and` / `&&` are accepted as no-ops);
//! - `or` (also `||`) is OR and binds looser than AND;
//! - `-term`, `!term` or `not term` negates; `-(…)` negates a group;
//! - parentheses group;
//! - `key:a,b` means `key:a or key:b`; values may be quoted (`owner:"Claude Code"`) and may use `*` globs;
//! - keys: `kind` (short aliases from SPEC §5: agent, app, model, sandbox, daemon, system, other; plurals and a
//!   few synonyms such as `vm`/`llm` are accepted), `owner`, `name`, `state` (alias `is`), `sandbox`, and the
//!   metrics `mem`, `gpu`, `cpu`, `idle`;
//! - comparisons `> >= < <= =` take units: bytes for `mem`/`gpu` (`2G`, `500M`, `1.5GiB`, `2GB`), percent of one
//!   core for `cpu` (`50`, `50%`), durations for `idle` (`30m`, `5h`, `1h30m`). Spaces around the operator are
//!   allowed (`mem > 2G`), as is `mem:>2G`.
//! - a bare word matches name, alias, owner, state flag or kind; a fully quoted word (`"a:b"`, `"Claude Code"`)
//!   is always a literal word (never `key:value` or a keyword), and commas inside quotes never split values
//!   (`owner:"Smith, John",bob` is two values).
//!
//! Example: `kind:model,agent or gpu>1G -muted`.
//!
//! **Plain words** go through normalize (case, units `13g`, `13 gb`, typos by Damerau-Levenshtein ≤ 1 on the
//! known vocabulary; never on entity-name tokens or their prefixes, and 4-letter words only by an adjacent
//! transposition) → intent classification (rules + keyword lexicon) → entity linking (fzf-style fuzzy score
//! boosted by frecency) → `{intent, filter, sort, targets, confidence}`. Low confidence (< [`LOW_CONFIDENCE`])
//! means the palette shows the top-3 [`Interpretation`]s instead of guessing.
//!
//! Everything here is deterministic, local and allocation-light (< 1 ms for typical inputs and a few hundred
//! entities).

use crate::headroom::is_reclaim_candidate;
use crate::model::{Group, GroupKind, Snapshot};
use crate::units::{parse_bytes, parse_duration_s, GIB, KIB, MIB, TIB};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use thiserror::Error;

/// Below this confidence the palette shows the top interpretations instead of acting.
pub const LOW_CONFIDENCE: f64 = 0.5;
/// Weight of frecency-derived affinity (0..1) in entity-linking scores.
pub const FRECENCY_LINK_BOOST: f64 = 0.5;
/// Minimum normalized fuzzy score for an entity to be linked at all.
pub const MIN_LINK_SCORE: f64 = 0.3;
/// Words shorter than this are never typo-corrected (`cpu` vs `gpu` would collide); words of exactly this
/// length only by an adjacent transposition.
pub const MIN_TYPO_LEN: usize = 4;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum QueryError {
    #[error("unknown key {0:?} (expected kind, owner, name, mem, gpu, cpu, idle, state, sandbox)")]
    UnknownKey(String),
    #[error("bad value {value:?} for {key}: {msg}")]
    BadValue { key: String, value: String, msg: String },
    #[error("unbalanced parentheses")]
    Parens,
    #[error("empty expression")]
    Empty,
    #[error("unterminated quote")]
    Quote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    Mem,
    Gpu,
    Cpu,
    Idle,
}

impl Metric {
    pub fn as_str(self) -> &'static str {
        match self {
            Metric::Mem => "mem",
            Metric::Gpu => "gpu",
            Metric::Cpu => "cpu",
            Metric::Idle => "idle",
        }
    }

    /// Accepts the key and its aliases (`memory`, `footprint`, `ram`, `vram`, `metal`, …).
    pub fn parse(s: &str) -> Option<Metric> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mem" | "memory" | "footprint" | "ram" => Some(Metric::Mem),
            "gpu" | "vram" | "metal" => Some(Metric::Gpu),
            "cpu" => Some(Metric::Cpu),
            "idle" => Some(Metric::Idle),
            _ => None,
        }
    }

    /// Human label for sort descriptions ("memory", "GPU memory", …).
    pub fn label(self) -> &'static str {
        match self {
            Metric::Mem => "memory",
            Metric::Gpu => "GPU memory",
            Metric::Cpu => "CPU",
            Metric::Idle => "idle time",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

impl CmpOp {
    pub fn as_str(self) -> &'static str {
        match self {
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Eq => "=",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldKey {
    Kind,
    Owner,
    Name,
    State,
    Sandbox,
}

impl FieldKey {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldKey::Kind => "kind",
            FieldKey::Owner => "owner",
            FieldKey::Name => "name",
            FieldKey::State => "state",
            FieldKey::Sandbox => "sandbox",
        }
    }

    /// Accepts the key and its aliases (`type`, `is`).
    pub fn parse(s: &str) -> Option<FieldKey> {
        match s.trim().to_ascii_lowercase().as_str() {
            "kind" | "type" => Some(FieldKey::Kind),
            "owner" => Some(FieldKey::Owner),
            "name" => Some(FieldKey::Name),
            "state" | "is" => Some(FieldKey::State),
            "sandbox" => Some(FieldKey::Sandbox),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Term {
    Field {
        key: FieldKey,
        values: Vec<String>,
    },
    /// `value` in base units: bytes (mem/gpu), percent of one core (cpu), seconds (idle).
    Cmp {
        metric: Metric,
        op: CmpOp,
        value: f64,
    },
    Word(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expr {
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Term(Term),
}

/// A typo or alias fix applied while understanding a query (shown as "showing results for …").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Correction {
    pub from: String,
    pub to: String,
}

// ---------------------------------------------------------------------------------------------------------
// Canonical rendering (saved views, palette labels)
// ---------------------------------------------------------------------------------------------------------

fn fmt_metric_value(metric: Metric, v: f64) -> String {
    match metric {
        Metric::Mem | Metric::Gpu => {
            let b = v.max(0.0).round() as u64;
            for (unit, suffix) in [(TIB, "T"), (GIB, "G"), (MIB, "M"), (KIB, "K")] {
                if b >= unit && b.is_multiple_of(unit) {
                    return format!("{}{suffix}", b / unit);
                }
            }
            for (unit, suffix) in [
                (1_000_000_000_000u64, "TB"),
                (1_000_000_000, "GB"),
                (1_000_000, "MB"),
                (1_000, "KB"),
            ] {
                if b >= unit && b.is_multiple_of(unit) {
                    return format!("{}{suffix}", b / unit);
                }
            }
            for (unit, suffix) in [(TIB, "T"), (GIB, "G"), (MIB, "M")] {
                if b >= unit {
                    let x = b as f64 / unit as f64;
                    let s = format!("{x:.2}");
                    let s = s.trim_end_matches('0').trim_end_matches('.');
                    return format!("{s}{suffix}");
                }
            }
            format!("{b}")
        }
        Metric::Cpu => {
            let s = format!("{v:.2}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
        Metric::Idle => {
            let s = v.max(0.0).round() as u64;
            for (unit, suffix) in [(86_400, "d"), (3_600, "h"), (60, "m")] {
                if s >= unit && s.is_multiple_of(unit) {
                    return format!("{}{suffix}", s / unit);
                }
            }
            format!("{s}s")
        }
    }
}

/// Quotes a value or word when the tokenizer would otherwise split or reinterpret it. A bare `word` also
/// needs quoting when it contains `:` or a comparison character (it would parse as `key:value`).
fn quote_if_needed(v: &str, word: bool) -> String {
    if v.is_empty()
        || v.chars()
            .any(|c| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '"' | QUOTED_COMMA))
        || v.starts_with(['-', '!'])
        || (word && v.contains([':', '<', '>', '=']))
        || ["or", "not", "and", "||", "|", "&&", "&"].contains(&v.to_ascii_lowercase().as_str())
    {
        format!("\"{}\"", v.replace('"', ""))
    } else {
        v.to_string()
    }
}

impl fmt::Display for Term {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Term::Field { key, values } => {
                let vs: Vec<String> = values.iter().map(|v| quote_if_needed(v, false)).collect();
                write!(f, "{}:{}", key.as_str(), vs.join(","))
            }
            Term::Cmp { metric, op, value } => write!(
                f,
                "{}{}{}",
                metric.as_str(),
                op.as_str(),
                fmt_metric_value(*metric, *value)
            ),
            Term::Word(w) => write!(f, "{}", quote_if_needed(w, true)),
        }
    }
}

impl fmt::Display for Expr {
    /// Canonical, re-parseable form: `kind:model,agent or gpu>1G -muted` round-trips.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Term(t) => write!(f, "{t}"),
            Expr::Not(x) => match x.as_ref() {
                Expr::Term(t) => write!(f, "-{t}"),
                other => write!(f, "-({other})"),
            },
            Expr::And(xs) => {
                let parts: Vec<String> = xs
                    .iter()
                    .map(|x| match x {
                        Expr::Or(_) => format!("({x})"),
                        _ => x.to_string(),
                    })
                    .collect();
                write!(f, "{}", parts.join(" "))
            }
            Expr::Or(xs) => {
                let parts: Vec<String> = xs.iter().map(|x| x.to_string()).collect();
                write!(f, "{}", parts.join(" or "))
            }
        }
    }
}

impl Expr {
    /// Metrics referenced by comparisons anywhere in the expression (deduplicated, in order).
    pub fn metrics(&self) -> Vec<Metric> {
        fn walk(e: &Expr, out: &mut Vec<Metric>) {
            match e {
                Expr::And(xs) | Expr::Or(xs) => xs.iter().for_each(|x| walk(x, out)),
                Expr::Not(x) => walk(x, out),
                Expr::Term(Term::Cmp { metric, .. }) => {
                    if !out.contains(metric) {
                        out.push(*metric);
                    }
                }
                Expr::Term(_) => {}
            }
        }
        let mut v = Vec::new();
        walk(self, &mut v);
        v
    }

    fn and_all(mut parts: Vec<Expr>) -> Option<Expr> {
        match parts.len() {
            0 => None,
            1 => parts.pop(),
            _ => Some(Expr::And(parts)),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Tokenizer + parser
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    Or,
    Not,
    Neg,
    Atom(String),
    /// A fully quoted atom (`"a:b"`, `"Claude Code"`): always a plain word, never `key:value` or a keyword.
    /// Quoted commas are still protected ([`QUOTED_COMMA`]) until the parser builds the term.
    Lit(String),
}

/// Stands in for a comma inside quotes while tokenizing, so `owner:"Smith, John"` stays one value.
const QUOTED_COMMA: char = '\u{1f}';

fn unprotect(s: &str) -> String {
    s.replace(QUOTED_COMMA, ",")
}

fn tokenize(input: &str) -> Result<Vec<Tok>, QueryError> {
    #[derive(Default)]
    struct Cur {
        text: String,
        any_quote: bool,
        starts_quoted: bool,
        /// `-"…"` / `!"…"`: a negated literal.
        neg: bool,
    }
    fn flush(cur: &mut Cur, toks: &mut Vec<Tok>) {
        let Cur {
            text,
            any_quote,
            starts_quoted,
            neg,
        } = std::mem::take(cur);
        if text.is_empty() {
            return;
        }
        if neg {
            toks.push(Tok::Neg);
        }
        if starts_quoted {
            toks.push(Tok::Lit(text));
            return;
        }
        if !any_quote {
            match text.to_ascii_lowercase().as_str() {
                "or" | "||" | "|" => return toks.push(Tok::Or),
                "not" | "!" => return toks.push(Tok::Not),
                "and" | "&&" | "&" | "-" => return,
                _ => {}
            }
        }
        if let Some(rest) = text.strip_prefix(['-', '!']).filter(|r| !r.is_empty()) {
            toks.push(Tok::Neg);
            toks.push(Tok::Atom(rest.to_string()));
            return;
        }
        toks.push(Tok::Atom(text));
    }

    let mut toks = Vec::new();
    let mut cur = Cur::default();
    let mut in_quote = false;
    for c in input.chars() {
        if in_quote {
            match c {
                '"' => in_quote = false,
                ',' => cur.text.push(QUOTED_COMMA),
                QUOTED_COMMA => {}
                c => cur.text.push(c),
            }
            continue;
        }
        match c {
            '"' => {
                if cur.text.is_empty() && !cur.any_quote {
                    cur.starts_quoted = true;
                } else if !cur.any_quote && (cur.text == "-" || cur.text == "!") {
                    cur.text.clear();
                    cur.neg = true;
                    cur.starts_quoted = true;
                }
                cur.any_quote = true;
                in_quote = true;
            }
            '(' => {
                if !cur.any_quote && (cur.text == "-" || cur.text == "!") {
                    cur.text.clear();
                    toks.push(Tok::Neg);
                } else {
                    flush(&mut cur, &mut toks);
                }
                toks.push(Tok::LParen);
            }
            ')' => {
                flush(&mut cur, &mut toks);
                toks.push(Tok::RParen);
            }
            QUOTED_COMMA => {}
            c if c.is_whitespace() => flush(&mut cur, &mut toks),
            c => cur.text.push(c),
        }
    }
    if in_quote {
        return Err(QueryError::Quote);
    }
    flush(&mut cur, &mut toks);
    Ok(merge_spaced_comparisons(toks))
}

fn is_known_key(s: &str) -> bool {
    Metric::parse(s).is_some() || FieldKey::parse(s).is_some()
}

fn is_op_char(c: char) -> bool {
    matches!(c, '<' | '>' | '=')
}

/// Joins `mem > 2G`, `mem >2G`, `mem> 2G` and `owner: claude` into single atoms.
fn merge_spaced_comparisons(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    for t in toks {
        if let Some(Tok::Atom(prev)) = out.last_mut() {
            let prev_open = prev.ends_with(is_op_char) || prev.ends_with(':');
            match &t {
                Tok::Atom(next) if prev_open || (is_known_key(prev) && next.starts_with(is_op_char)) => {
                    prev.push_str(next);
                    continue;
                }
                // `owner: "Claude Code"`: the quoted literal is the value (commas stay protected).
                Tok::Lit(next) if prev_open => {
                    prev.push_str(next);
                    continue;
                }
                _ => {}
            }
        }
        out.push(t);
    }
    out
}

struct Parser<'c> {
    toks: Vec<Tok>,
    pos: usize,
    corrections: &'c mut Vec<Correction>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }
    fn or_expr(&mut self) -> Result<Expr, QueryError> {
        let mut parts = vec![self.and_expr()?];
        while self.peek() == Some(&Tok::Or) {
            self.next();
            parts.push(self.and_expr()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expr::Or(parts)
        })
    }
    fn and_expr(&mut self) -> Result<Expr, QueryError> {
        let mut parts = Vec::new();
        while let Some(t) = self.peek() {
            if matches!(t, Tok::Or | Tok::RParen) {
                break;
            }
            parts.push(self.unary()?);
        }
        match parts.len() {
            0 => Err(QueryError::Empty),
            1 => Ok(parts.remove(0)),
            _ => Ok(Expr::And(parts)),
        }
    }
    fn unary(&mut self) -> Result<Expr, QueryError> {
        match self.peek() {
            Some(Tok::Not) | Some(Tok::Neg) => {
                self.next();
                Ok(Expr::Not(Box::new(self.unary()?)))
            }
            _ => self.atom(),
        }
    }
    fn atom(&mut self) -> Result<Expr, QueryError> {
        match self.next() {
            Some(Tok::LParen) => {
                let e = self.or_expr()?;
                if self.next() != Some(Tok::RParen) {
                    return Err(QueryError::Parens);
                }
                Ok(e)
            }
            Some(Tok::Atom(a)) => Ok(Expr::Term(parse_term_with(&a, self.corrections)?)),
            Some(Tok::Lit(w)) => Ok(Expr::Term(Term::Word(unprotect(&w)))),
            Some(Tok::RParen) => Err(QueryError::Parens),
            _ => Err(QueryError::Empty),
        }
    }
}

/// Splits `key<op>value`. Returns `None` for plain words.
fn split_key_op(atom: &str) -> Option<(&str, &str, &str)> {
    let i = atom.find(|c: char| c == ':' || is_op_char(c))?;
    let key = &atom[..i];
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphabetic() || c == '_') {
        return None;
    }
    let rest = &atom[i..];
    let op_len = if let Some(after) = rest.strip_prefix(':') {
        // `mem:>2G` style.
        if after.starts_with(">=") || after.starts_with("<=") || after.starts_with("==") {
            3
        } else if after.starts_with(is_op_char) {
            2
        } else {
            1
        }
    } else if rest.starts_with(">=") || rest.starts_with("<=") || rest.starts_with("==") {
        2
    } else {
        1
    };
    Some((key, &rest[..op_len], &rest[op_len..]))
}

fn cmp_op(op: &str) -> Option<CmpOp> {
    match op.trim_start_matches(':') {
        ">" => Some(CmpOp::Gt),
        ">=" => Some(CmpOp::Ge),
        "<" => Some(CmpOp::Lt),
        "<=" => Some(CmpOp::Le),
        "=" | "==" => Some(CmpOp::Eq),
        _ => None,
    }
}

enum Key {
    Metric(Metric),
    Field(FieldKey),
}

const CANONICAL_KEYS: &[&str] = &[
    "kind", "owner", "name", "state", "sandbox", "mem", "memory", "gpu", "cpu", "idle",
];

fn resolve_key(raw: &str, corrections: &mut Vec<Correction>) -> Result<Key, QueryError> {
    let k = raw.to_ascii_lowercase();
    if let Some(m) = Metric::parse(&k) {
        return Ok(Key::Metric(m));
    }
    if let Some(f) = FieldKey::parse(&k) {
        return Ok(Key::Field(f));
    }
    if k.chars().count() >= 3 {
        if let Some(fixed) = closest(&k, CANONICAL_KEYS.iter().copied(), 3) {
            corrections.push(Correction {
                from: k.clone(),
                to: fixed.to_string(),
            });
            return resolve_key(fixed, corrections);
        }
    }
    Err(QueryError::UnknownKey(k))
}

/// Kind value synonyms beyond `GroupKind::parse` (plurals, `vm`, `llm`, …).
fn kind_synonym(v: &str) -> Option<GroupKind> {
    let v = v.trim().to_ascii_lowercase();
    if let Some(k) = GroupKind::parse(&v) {
        return Some(k);
    }
    let k = match v.as_str() {
        "agents" | "session" | "sessions" | "agent_sessions" => GroupKind::AgentSession,
        "apps" | "application" | "applications" => GroupKind::App,
        "models" | "llm" | "llms" | "model_servers" => GroupKind::ModelServer,
        "sandboxes" | "vm" | "vms" | "container" | "containers" | "microvm" | "microvms" => {
            GroupKind::Sandbox
        }
        "daemons" | "build" | "builds" | "build_daemons" => GroupKind::BuildDaemon,
        "sys" => GroupKind::System,
        "others" => GroupKind::Other,
        _ => return None,
    };
    Some(k)
}

const KIND_WORDS: &[&str] = &[
    "agent",
    "agents",
    "app",
    "apps",
    "model",
    "models",
    "sandbox",
    "sandboxes",
    "daemon",
    "daemons",
    "system",
    "other",
    "containers",
    "container",
    "vms",
];

fn normalize_kind_value(v: &str, corrections: &mut Vec<Correction>) -> Option<String> {
    if v == "*" {
        return Some("*".into());
    }
    if let Some(k) = kind_synonym(v) {
        return Some(k.alias().to_string());
    }
    let lower = v.to_ascii_lowercase();
    if lower.chars().count() >= MIN_TYPO_LEN {
        let names = GroupKind::ALL
            .iter()
            .flat_map(|k| [k.alias(), k.as_str()])
            .chain(KIND_WORDS.iter().copied());
        if let Some(fixed) = closest(&lower, names, MIN_TYPO_LEN) {
            corrections.push(Correction {
                from: lower.clone(),
                to: fixed.to_string(),
            });
            return kind_synonym(fixed).map(|k| k.alias().to_string());
        }
    }
    None
}

fn normalize_state_value(v: &str) -> String {
    let v = v.trim().to_ascii_lowercase();
    match v.as_str() {
        "orphaned" | "orphans" => "orphan".into(),
        "working" | "generating" | "running" => "busy".into(),
        "lower_bound" | "lowerbound" => "partial".into(),
        "mine" | "pin" => "pinned".into(),
        "mute" => "muted".into(),
        _ => v,
    }
}

fn parse_metric_value(metric: Metric, key: &str, v: &str) -> Result<f64, QueryError> {
    let bad = |msg: String| QueryError::BadValue {
        key: key.into(),
        value: v.into(),
        msg,
    };
    let v = v.trim();
    if v.is_empty() {
        return Err(bad("missing value".into()));
    }
    match metric {
        Metric::Mem | Metric::Gpu => {
            let has_unit = v.chars().any(|c| c.is_ascii_alphabetic());
            let b = parse_bytes(v).map_err(|e| bad(e.to_string()))?;
            if !has_unit && b != 0 {
                return Err(bad("add a unit, e.g. 2G or 500M".into()));
            }
            Ok(b as f64)
        }
        Metric::Cpu => {
            let x = v
                .trim_end_matches('%')
                .parse::<f64>()
                .map_err(|e| bad(e.to_string()))?;
            if !x.is_finite() || x < 0.0 {
                return Err(bad("expected a non-negative percentage".into()));
            }
            Ok(x)
        }
        Metric::Idle => parse_duration_s(v).map_err(|e| bad(e.to_string())),
    }
}

fn parse_term_with(atom: &str, corrections: &mut Vec<Correction>) -> Result<Term, QueryError> {
    let Some((raw_key, op, value)) = split_key_op(atom) else {
        return Ok(Term::Word(unprotect(atom)));
    };
    let key = resolve_key(raw_key, corrections)?;
    let key_name = raw_key.to_ascii_lowercase();
    let is_colon = op == ":";
    match key {
        Key::Metric(metric) => {
            if is_colon {
                return Err(QueryError::BadValue {
                    key: key_name,
                    value: value.into(),
                    msg: format!("use a comparison, e.g. {}>2G", metric.as_str()),
                });
            }
            let op = cmp_op(op).ok_or_else(|| QueryError::BadValue {
                key: key_name.clone(),
                value: value.into(),
                msg: "unknown operator".into(),
            })?;
            Ok(Term::Cmp {
                metric,
                op,
                value: parse_metric_value(metric, &key_name, value)?,
            })
        }
        Key::Field(fk) => {
            if !(is_colon || op == "=" || op == "==") {
                return Err(QueryError::BadValue {
                    key: key_name,
                    value: value.into(),
                    msg: "comparisons need mem, gpu, cpu or idle".into(),
                });
            }
            let raw_values: Vec<&str> = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if raw_values.is_empty() {
                return Err(QueryError::BadValue {
                    key: key_name,
                    value: value.into(),
                    msg: "missing value".into(),
                });
            }
            let mut values = Vec::with_capacity(raw_values.len());
            for val in raw_values {
                let val = unprotect(val);
                let val = val.as_str();
                let v = match fk {
                    FieldKey::Kind => {
                        normalize_kind_value(val, corrections).ok_or_else(|| QueryError::BadValue {
                            key: key_name.clone(),
                            value: val.into(),
                            msg: "unknown kind (agent, app, model, sandbox, daemon, system, other)".into(),
                        })?
                    }
                    FieldKey::State => normalize_state_value(val),
                    _ => val.to_string(),
                };
                if !values.contains(&v) {
                    values.push(v);
                }
            }
            Ok(Term::Field { key: fk, values })
        }
    }
}

/// Parses one atom into a term (`kind:model,agent`, `mem>2G`, `claude`).
pub fn parse_term(atom: &str) -> Result<Term, QueryError> {
    parse_term_with(atom, &mut Vec::new())
}

fn parse_tokens(toks: Vec<Tok>, corrections: &mut Vec<Correction>) -> Result<Expr, QueryError> {
    if toks.is_empty() {
        return Err(QueryError::Empty);
    }
    let mut p = Parser {
        toks,
        pos: 0,
        corrections,
    };
    let e = p.or_expr()?;
    if p.pos < p.toks.len() {
        return Err(QueryError::Parens);
    }
    Ok(e)
}

/// Parses a filter expression (grammar in the module docs).
pub fn parse_filter(input: &str) -> Result<Expr, QueryError> {
    parse_tokens(tokenize(input)?, &mut Vec::new())
}

/// Like [`parse_filter`], also returning the key/kind typo corrections that were applied.
pub fn parse_filter_corrected(input: &str) -> Result<(Expr, Vec<Correction>), QueryError> {
    let mut c = Vec::new();
    let e = parse_tokens(tokenize(input)?, &mut c)?;
    Ok((e, c))
}

// ---------------------------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------------------------

/// Prefix of the state flag that carries a sandbox runtime (`runtime:docker`), matched by `sandbox:`.
pub const RUNTIME_FLAG_PREFIX: &str = "runtime:";

/// Flattened view of an entity for filter evaluation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EntityView {
    pub id: String,
    pub kind: GroupKind,
    pub name: String,
    pub aliases: Vec<String>,
    pub owner: Option<String>,
    pub mem: Option<u64>,
    pub gpu: Option<u64>,
    pub cpu: Option<f64>,
    pub idle_s: Option<u64>,
    /// Flags such as "idle", "orphan", "muted", "pinned", "protected", "reclaimable", "busy", "self",
    /// "partial" (footprint is a lower bound), and `runtime:<name>` for sandboxes.
    pub state: Vec<String>,
    pub sandbox: Option<String>,
}

impl EntityView {
    pub fn from_group(g: &Group, s: &Snapshot) -> Self {
        let mut state = Vec::new();
        let mut flag = |cond: bool, name: &str| {
            if cond {
                state.push(name.to_string());
            }
        };
        flag(g.idle, "idle");
        flag(g.orphan, "orphan");
        flag(g.protected, "protected");
        flag(g.is_self, "self");
        flag(g.lower_bound, "partial");
        flag(is_reclaim_candidate(g), "reclaimable");
        let busy = s
            .model_servers
            .iter()
            .any(|m| m.group_id.as_deref() == Some(g.id.as_str()) && m.busy.value == Some(true));
        flag(busy, "busy");

        let sandbox_rec = s.sandboxes.iter().find(|sb| {
            g.root.map(|r| sb.host_pids.contains(&r)).unwrap_or(false)
                || sb.host_pids.iter().any(|p| g.members.iter().any(|m| m.id == *p))
        });
        if let Some(rt) = sandbox_rec.map(|sb| sb.runtime.trim()).filter(|r| !r.is_empty()) {
            state.push(format!("{RUNTIME_FLAG_PREFIX}{}", rt.to_lowercase()));
        }
        let owner_id = g
            .owner_group
            .clone()
            .or_else(|| sandbox_rec.and_then(|sb| sb.started_by_group.clone()));
        let owner = owner_id
            .as_deref()
            .and_then(|o| s.group(o))
            .map(|o| o.label.clone());
        let is_sandbox = g.kind == GroupKind::Sandbox || sandbox_rec.is_some();
        let sandbox = is_sandbox.then(|| {
            sandbox_rec
                .map(|sb| sb.label.clone())
                .unwrap_or_else(|| g.label.clone())
        });
        EntityView {
            id: g.id.clone(),
            kind: g.kind,
            name: g.label.clone(),
            aliases: Vec::new(),
            owner,
            mem: g.totals.footprint.value,
            gpu: g.totals.gpu.value,
            cpu: g.totals.cpu_pct.value,
            idle_s: g.idle_for_s,
            state,
            sandbox,
        }
    }

    /// Sandbox runtime carried as a `runtime:<name>` flag, if any.
    pub fn sandbox_runtime(&self) -> Option<&str> {
        self.state
            .iter()
            .find_map(|s| s.strip_prefix(RUNTIME_FLAG_PREFIX))
    }

    /// Adds a profile flag (`pinned`, `muted`, …) once.
    pub fn add_state(&mut self, flag: &str) {
        let f = flag.to_ascii_lowercase();
        if !self.state.contains(&f) {
            self.state.push(f);
        }
    }

    /// Adds a user rename/alias usable in queries.
    pub fn add_alias(&mut self, alias: &str) {
        if !alias.is_empty() && !self.aliases.iter().any(|a| a == alias) {
            self.aliases.push(alias.to_string());
        }
    }
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(&needle.to_lowercase())
}

/// Case-insensitive match: substring when `pat` has no `*`, anchored glob otherwise.
fn text_match(text: &str, pat: &str) -> bool {
    if !pat.contains('*') {
        return contains_ci(text, pat);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let p: Vec<char> = pat.to_lowercase().chars().collect();
    // Iterative glob with backtracking on the last '*'.
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] != '*' && p[pi] == t[ti] {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

fn cmp(op: CmpOp, a: f64, b: f64) -> bool {
    match op {
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Eq => (a - b).abs() <= f64::EPSILON.max(b.abs() * 1e-9),
    }
}

fn name_match(v: &EntityView, pat: &str) -> bool {
    text_match(&v.name, pat) || v.aliases.iter().any(|a| text_match(a, pat))
}

/// Evaluates a filter against an entity. Unavailable metrics never satisfy a comparison (they are unknown,
/// not zero).
pub fn eval(e: &Expr, v: &EntityView) -> bool {
    match e {
        Expr::And(xs) => xs.iter().all(|x| eval(x, v)),
        Expr::Or(xs) => xs.iter().any(|x| eval(x, v)),
        Expr::Not(x) => !eval(x, v),
        Expr::Term(t) => match t {
            Term::Field { key, values } => values.iter().any(|val| match key {
                FieldKey::Kind => val == "*" || kind_synonym(val) == Some(v.kind),
                FieldKey::Owner => match v.owner.as_deref() {
                    Some(o) => val == "*" || text_match(o, val),
                    None => false,
                },
                FieldKey::Name => val == "*" || name_match(v, val),
                FieldKey::State if val.contains('*') => v.state.iter().any(|s| text_match(s, val)),
                FieldKey::State => v.state.iter().any(|s| s.eq_ignore_ascii_case(val)),
                FieldKey::Sandbox => match v.sandbox.as_deref() {
                    Some(sb) => {
                        val == "*"
                            || text_match(sb, val)
                            || v.sandbox_runtime().map(|r| text_match(r, val)).unwrap_or(false)
                    }
                    None => false,
                },
            }),
            Term::Cmp { metric, op, value } => {
                let x = match metric {
                    Metric::Mem => v.mem.map(|b| b as f64),
                    Metric::Gpu => v.gpu.map(|b| b as f64),
                    Metric::Cpu => v.cpu,
                    Metric::Idle => v.idle_s.map(|s| s as f64),
                };
                x.map(|x| cmp(*op, x, *value)).unwrap_or(false)
            }
            Term::Word(w) => {
                v.state.iter().any(|s| s.eq_ignore_ascii_case(w))
                    || name_match(v, w)
                    || v.owner.as_deref().map(|o| text_match(o, w)).unwrap_or(false)
                    || kind_synonym(w) == Some(v.kind)
            }
        },
    }
}

// ---------------------------------------------------------------------------------------------------------
// Understanding plain words
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Home,
    Processes,
    Models,
    Sandboxes,
    Reclaim,
    Timeline,
}

impl View {
    pub fn as_str(self) -> &'static str {
        match self {
            View::Home => "home",
            View::Processes => "processes",
            View::Models => "models",
            View::Sandboxes => "sandboxes",
            View::Reclaim => "reclaim",
            View::Timeline => "timeline",
        }
    }

    pub fn parse(s: &str) -> Option<View> {
        nav_word(&s.trim().to_ascii_lowercase())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Find,
    RankBy(Metric),
    Explain,
    Headroom,
    Reclaim,
    Navigate(View),
}

impl Intent {
    /// Short human label for palette choices.
    pub fn label(&self) -> String {
        match self {
            Intent::Find => "find".into(),
            Intent::RankBy(m) => format!("rank by {}", m.label()),
            Intent::Explain => "explain slowdown".into(),
            Intent::Headroom => "check headroom".into(),
            Intent::Reclaim => "reclaim memory".into(),
            Intent::Navigate(v) => format!("open {}", v.as_str()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub name: String,
    pub args: Vec<String>,
}

/// Something the user can name: a group/entity with display name, aliases/renames and frecency.
/// Ids starting with [`PROCESS_ID_PREFIX`] are single processes (they rank slightly below groups).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VocabEntity {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    /// Raw frecency score (UX §5.1), not normalized. Pinned entities carry [`PINNED_FRECENCY_BONUS`].
    pub frecency: f64,
    /// Cold-start prior from the machine profile (UX §5.4: agent CLIs and model servers detected on this
    /// machine), used only while the entity has no frecency — so with no history "cl" means the detected
    /// Claude Code session, not an unrelated `clang`. See [`crate::ranking::ColdStartPriors`].
    pub prior: f64,
}

/// Id prefix of process (not group) entities.
pub const PROCESS_ID_PREFIX: &str = "pid:";
/// Frecency added to pinned entities by [`Vocabulary::from_snapshot`].
pub const PINNED_FRECENCY_BONUS: f64 = 3.0;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Vocabulary {
    pub entities: Vec<VocabEntity>,
}

/// Maximum number of process entities added by [`Vocabulary::from_snapshot`].
pub const VOCAB_MAX_PROCESSES: usize = 200;
/// Maximum member-process names added as aliases of one group.
pub const VOCAB_MAX_MEMBER_ALIASES: usize = 8;

impl Vocabulary {
    /// Builds a vocabulary from a snapshot plus the profile (frecency / renames / pins keyed by fingerprint).
    /// Every group is an entity; its user rename and the distinct names of its member processes (e.g. the
    /// `claude` binary inside "Claude Code", at most [`VOCAB_MAX_MEMBER_ALIASES`]) are aliases, so typing a
    /// process name finds its group. Processes outside any group are added as `pid:<n>` entities (largest
    /// first, capped at [`VOCAB_MAX_PROCESSES`]).
    pub fn from_snapshot(
        s: &Snapshot,
        frecency_by_fp: &HashMap<String, f64>,
        renames_by_fp: &HashMap<String, String>,
        pinned_fps: &HashSet<String>,
    ) -> Vocabulary {
        let mut grouped: HashSet<crate::model::ProcId> = HashSet::new();
        let mut entities: Vec<VocabEntity> = Vec::with_capacity(s.groups.len());
        let priors = crate::ranking::ColdStartPriors::detect(s);
        for g in &s.groups {
            let mut aliases: Vec<String> = renames_by_fp.get(&g.fingerprint).cloned().into_iter().collect();
            let label = g.label.to_lowercase();
            for m in &g.members {
                grouped.insert(m.id);
                if aliases.len() >= VOCAB_MAX_MEMBER_ALIASES {
                    continue;
                }
                if let Some(p) = s.process(m.id) {
                    let n = p.name.trim();
                    if !n.is_empty()
                        && n.to_lowercase() != label
                        && !aliases.iter().any(|a| a.eq_ignore_ascii_case(n))
                    {
                        aliases.push(n.to_string());
                    }
                }
            }
            entities.push(VocabEntity {
                id: g.id.clone(),
                name: g.label.clone(),
                aliases,
                prior: priors.map(|p| p.for_kind(g.kind)).unwrap_or(0.0),
                frecency: frecency_by_fp
                    .get(&g.fingerprint)
                    .copied()
                    .unwrap_or(0.0)
                    .max(0.0)
                    + if pinned_fps.contains(&g.fingerprint) {
                        PINNED_FRECENCY_BONUS
                    } else {
                        0.0
                    },
            });
        }
        let mut procs: Vec<&crate::model::Process> = s
            .processes
            .iter()
            .filter(|p| !p.name.is_empty() && !grouped.contains(&p.id))
            .collect();
        procs.sort_by(|a, b| {
            b.mem
                .footprint_or_pss
                .value
                .unwrap_or(0)
                .cmp(&a.mem.footprint_or_pss.value.unwrap_or(0))
                .then(a.id.pid.cmp(&b.id.pid))
        });
        let mut seen: HashSet<String> = HashSet::new();
        let mut added = 0;
        for p in procs {
            if added >= VOCAB_MAX_PROCESSES {
                break;
            }
            if !seen.insert(p.name.to_lowercase()) {
                continue;
            }
            added += 1;
            entities.push(VocabEntity {
                id: format!("{PROCESS_ID_PREFIX}{}", p.id.pid),
                name: p.name.clone(),
                ..Default::default()
            });
        }
        Vocabulary { entities }
    }

    /// Lowercased name tokens of every entity (used so typo correction never "fixes" a real name).
    fn tokens(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for e in &self.entities {
            for n in std::iter::once(&e.name).chain(e.aliases.iter()) {
                out.extend(name_tokens(n));
            }
        }
        out
    }
}

/// Lowercased tokens of a display name: words split on punctuation/space, plus the camelCase humps of each
/// word (`GradleDaemon` → `gradledaemon`, `gradle`, `daemon`), so typos and prefixes can hit a hump.
pub fn name_tokens(name: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: String| {
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    };
    for word in name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        push(word.to_lowercase());
        let chars: Vec<char> = word.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let hump = chars[i - 1].is_lowercase() && chars[i].is_uppercase();
            // `HTTPServer` → `http`, `server`.
            let acronym_end = chars[i - 1].is_uppercase()
                && chars[i].is_uppercase()
                && chars.get(i + 1).is_some_and(|c| c.is_lowercase());
            if hump || acronym_end {
                push(chars[start..i].iter().collect::<String>().to_lowercase());
                start = i;
            }
        }
        if start > 0 {
            push(chars[start..].iter().collect::<String>().to_lowercase());
        }
    }
    out
}

/// One way to read an ambiguous input (the palette offers the top 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Interpretation {
    pub intent: Intent,
    #[serde(default)]
    pub filter: Option<Expr>,
    #[serde(default)]
    pub sort: Option<Metric>,
    #[serde(default)]
    pub targets: Vec<(String, f64)>,
    #[serde(default)]
    pub need_bytes: Option<u64>,
    /// Human label, e.g. "rank by memory", "go to Claude Code", "filter kind:model".
    pub label: String,
    /// 0..1.
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Understanding {
    pub intent: Intent,
    pub filter: Option<Expr>,
    pub sort: Option<Metric>,
    /// Linked entity ids with scores, best first.
    pub targets: Vec<(String, f64)>,
    pub need_bytes: Option<u64>,
    pub command: Option<Command>,
    /// 0..1; below [`LOW_CONFIDENCE`] the palette shows `interpretations` instead of guessing.
    pub confidence: f64,
    /// Intents of the runner-up interpretations when confidence is low (kept for older consumers).
    pub alternatives: Vec<Intent>,
    /// Top-3 interpretations, best first (always filled when anything was understood).
    #[serde(default)]
    pub interpretations: Vec<Interpretation>,
    /// Typo/alias fixes applied ("showing results for …").
    #[serde(default)]
    pub corrections: Vec<Correction>,
    /// The normalized input (lowercase, units joined, typos fixed).
    #[serde(default)]
    pub normalized: String,
    /// Filter syntax error, if the input looked structured but did not parse.
    #[serde(default)]
    pub error: Option<String>,
}

impl Understanding {
    fn empty() -> Self {
        Understanding {
            intent: Intent::Find,
            filter: None,
            sort: None,
            targets: Vec::new(),
            need_bytes: None,
            command: None,
            confidence: 0.0,
            alternatives: Vec::new(),
            interpretations: Vec::new(),
            corrections: Vec::new(),
            normalized: String::new(),
            error: None,
        }
    }

    /// True when the palette should offer choices instead of acting.
    pub fn is_ambiguous(&self) -> bool {
        self.confidence < LOW_CONFIDENCE
    }

    /// Best linked entity id, if any.
    pub fn top_target(&self) -> Option<&str> {
        self.targets.first().map(|(id, _)| id.as_str())
    }
}

const STOPWORDS: &[&str] = &[
    "what",
    "whats",
    "is",
    "the",
    "my",
    "me",
    "are",
    "i",
    "im",
    "can",
    "could",
    "show",
    "all",
    "a",
    "an",
    "of",
    "up",
    "to",
    "now",
    "stuff",
    "things",
    "who",
    "whos",
    "which",
    "please",
    "it",
    "its",
    "this",
    "that",
    "for",
    "on",
    "in",
    "any",
    "much",
    "how",
    "do",
    "does",
    "anything",
    "something",
    "list",
    "give",
    "tell",
    "get",
    "with",
    "there",
    "so",
    "be",
    "being",
    "am",
    "and",
    "or",
    "by",
    "from",
    "at",
    "if",
    "right",
    "currently",
    "most",
    "some",
    "lot",
    "lots",
    "again",
    "really",
    "very",
    "you",
    "your",
    "we",
    "our",
    "go",
    "open",
];
const RANK_WORDS: &[&str] = &[
    "hog",
    "hogs",
    "hogging",
    "eating",
    "eats",
    "eat",
    "using",
    "uses",
    "use",
    "biggest",
    "largest",
    "big",
    "heavy",
    "heaviest",
    "top",
    "consumers",
    "consuming",
    "hungry",
    "greedy",
    "usage",
];
const MEM_WORDS: &[&str] = &["memory", "mem", "ram", "footprint"];
const GPU_WORDS: &[&str] = &["gpu", "vram", "metal"];
const CPU_WORDS: &[&str] = &["cpu", "processor", "cores"];
const EXPLAIN_STRONG: &[&str] = &[
    "slow",
    "slowly",
    "slowness",
    "lag",
    "laggy",
    "lagging",
    "throttle",
    "throttling",
    "throttled",
    "sluggish",
    "slowdown",
    "stutter",
    "stuttering",
    "freeze",
    "freezing",
    "frozen",
];
const EXPLAIN_WEAK: &[&str] = &["why", "hot", "heat", "fan", "thermal", "explain"];
const HEADROOM_VERBS: &[&str] = &["load", "fit", "fits", "run", "start", "launch"];
const HEADROOM_NOUNS: &[&str] = &["headroom", "room", "space", "enough", "available"];
const RECLAIM_STRONG: &[&str] = &[
    "reclaim",
    "reclaimable",
    "leftovers",
    "leftover",
    "orphans",
    "orphan",
    "orphaned",
    "cleanup",
    "clean",
];
const RECLAIM_WEAK: &[&str] = &["idle", "unused", "free", "kill", "stop"];

fn nav_word(w: &str) -> Option<View> {
    match w {
        "models" | "model" => Some(View::Models),
        "sandboxes" | "sandbox" | "vms" | "vm" | "containers" | "container" => Some(View::Sandboxes),
        "processes" | "procs" | "process" => Some(View::Processes),
        "timeline" | "history" => Some(View::Timeline),
        "home" | "groups" => Some(View::Home),
        _ => None,
    }
}

const NAV_WORDS: &[&str] = &[
    "models",
    "sandboxes",
    "processes",
    "procs",
    "timeline",
    "history",
    "home",
    "groups",
];

fn lexicon() -> impl Iterator<Item = &'static str> {
    RANK_WORDS
        .iter()
        .chain(MEM_WORDS)
        .chain(GPU_WORDS)
        .chain(CPU_WORDS)
        .chain(EXPLAIN_STRONG)
        .chain(EXPLAIN_WEAK)
        .chain(HEADROOM_VERBS)
        .chain(HEADROOM_NOUNS)
        .chain(RECLAIM_STRONG)
        .chain(RECLAIM_WEAK)
        .chain(NAV_WORDS)
        .chain(KIND_WORDS)
        .copied()
}

fn in_lexicon(w: &str) -> bool {
    lexicon().any(|l| l == w)
}

/// Optimal-string-alignment Damerau-Levenshtein distance (adjacent transpositions count as one edit).
pub fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    // Three rolling rows: i-2, i-1, i.
    let mut prev2: Vec<usize> = vec![0; m + 1];
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut d = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d = d.min(prev2[j - 2] + 1);
            }
            cur[j] = d;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// Closest candidate within distance 1 (min length `min_len`); ties broken by length difference, then
/// alphabetically, so the result is deterministic.
fn closest<'a>(w: &str, candidates: impl Iterator<Item = &'a str>, min_len: usize) -> Option<&'a str> {
    let wl = w.chars().count();
    candidates
        .filter(|c| c.chars().count() >= min_len && *c != w)
        .filter(|c| damerau_levenshtein(c, w) <= 1)
        .min_by(|a, b| {
            let da = (a.chars().count() as i64 - wl as i64).abs();
            let db = (b.chars().count() as i64 - wl as i64).abs();
            da.cmp(&db).then(a.cmp(b))
        })
}

// fzf v2-style scoring constants.
const SCORE_MATCH: f64 = 16.0;
const GAP_START: f64 = -3.0;
const GAP_EXT: f64 = -1.0;
const BONUS_BOUNDARY_WHITE: f64 = 10.0;
const BONUS_BOUNDARY: f64 = 8.0;
const BONUS_CAMEL: f64 = 7.0;
const BONUS_CONSECUTIVE: f64 = 4.0;
const FIRST_CHAR_MULT: f64 = 2.0;

fn char_bonus(prev: Option<char>, cur: char) -> f64 {
    match prev {
        None => BONUS_BOUNDARY_WHITE,
        Some(p) if p.is_whitespace() => BONUS_BOUNDARY_WHITE,
        Some('-' | '_' | '.' | '/' | '·' | ':' | ',' | '(' | '[' | '@' | '\\') => BONUS_BOUNDARY,
        Some(p) if p.is_lowercase() && cur.is_uppercase() => BONUS_CAMEL,
        Some(p) if p.is_alphabetic() && cur.is_ascii_digit() => BONUS_CAMEL,
        _ => 0.0,
    }
}

/// fzf-style fuzzy score (higher is better): the best alignment of `pattern` as a case-insensitive
/// subsequence of `text`, rewarding matches at word starts / camelCase humps and consecutive runs and
/// penalizing gaps. `None` if `pattern` is not a subsequence of `text`. An empty pattern scores 0.
pub fn fuzzy_score(pattern: &str, text: &str) -> Option<f64> {
    let p: Vec<char> = pattern
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    if p.is_empty() {
        return Some(0.0);
    }
    let orig: Vec<char> = text.chars().collect();
    let t: Vec<char> = orig
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let n = t.len();
    if n < p.len() {
        return None;
    }
    let bonus: Vec<f64> = (0..n)
        .map(|j| char_bonus(if j == 0 { None } else { Some(orig[j - 1]) }, orig[j]))
        .collect();
    const NEG: f64 = f64::NEG_INFINITY;
    // prev[j]: best score with p[..i] matched and p[i-1] at t[j].
    let mut prev: Vec<f64> = vec![NEG; n];
    for (j, &c) in t.iter().enumerate() {
        if c == p[0] {
            prev[j] = SCORE_MATCH + bonus[j] * FIRST_CHAR_MULT;
        }
    }
    for &pc in p.iter().skip(1) {
        let mut cur: Vec<f64> = vec![NEG; n];
        // gap_best: best of prev[k] + gap penalty for k ≤ j-2, maintained incrementally.
        let mut gap_best = NEG;
        for j in 0..n {
            if j >= 2 {
                gap_best = (gap_best + GAP_EXT).max(prev[j - 2] + GAP_START);
            }
            if t[j] != pc {
                continue;
            }
            let consecutive = if j >= 1 && prev[j - 1] > NEG {
                prev[j - 1] + SCORE_MATCH + bonus[j].max(BONUS_CONSECUTIVE)
            } else {
                NEG
            };
            let gapped = if gap_best > NEG {
                gap_best + SCORE_MATCH + bonus[j]
            } else {
                NEG
            };
            cur[j] = consecutive.max(gapped);
        }
        prev = cur;
    }
    let best = prev.into_iter().fold(NEG, f64::max);
    if best == NEG {
        return None;
    }
    // Shorter texts win ties (fzf's length tiebreak).
    Some(best - n as f64 * 0.01)
}

/// [`fuzzy_score`] normalized to 0..1 against the ideal score for this pattern length (a prefix match of the
/// whole pattern scores ≈ 1).
pub fn fuzzy_match(pattern: &str, text: &str) -> Option<f64> {
    let m = pattern.chars().filter(|c| !c.is_whitespace()).count();
    if m == 0 {
        return Some(0.0);
    }
    let raw = fuzzy_score(pattern, text)?;
    let ideal = SCORE_MATCH
        + BONUS_BOUNDARY_WHITE * FIRST_CHAR_MULT
        + (m as f64 - 1.0) * (SCORE_MATCH + BONUS_CONSECUTIVE);
    Some((raw / ideal).clamp(0.0, 1.0))
}

fn kind_prior(e: &VocabEntity) -> f64 {
    if e.id.starts_with(PROCESS_ID_PREFIX) {
        -0.05
    } else {
        0.0
    }
}

fn affinity(frecency: f64) -> f64 {
    1.0 - (-frecency.max(0.0) / 5.0).exp()
}

/// Match quality (0..1.5) of the query words against one name.
fn name_match_quality(words: &[String], name: &str) -> f64 {
    let lname = name.to_lowercase();
    let joined = words.join(" ");
    if lname == joined {
        return 1.5; // exact: always beats frecency
    }
    let mut best = fuzzy_match(&joined, name).unwrap_or(0.0);
    if words.len() > 1 {
        let per: Vec<f64> = words
            .iter()
            .map(|w| fuzzy_match(w, name).unwrap_or(0.0))
            .collect();
        if per.iter().all(|s| *s > 0.0) {
            best = best.max(0.9 * per.iter().sum::<f64>() / per.len() as f64);
        }
    }
    // Typo tolerance on whole name tokens and camelCase humps (fuzzy subsequence can't see transpositions).
    let tokens = name_tokens(name);
    for w in words {
        if w.chars().count() >= MIN_TYPO_LEN {
            if tokens.iter().any(|t| t == w) {
                best = best.max(1.0);
            } else if tokens.iter().any(|t| damerau_levenshtein(t, w) <= 1) {
                best = best.max(0.7);
            }
        }
    }
    best
}

/// Links words to entities: fzf-style fuzzy match on names/aliases/renames (exact names score highest), plus
/// `FRECENCY_LINK_BOOST × affinity(frecency)` (or, without history, the machine-profile cold-start prior)
/// and small priors (pinned, groups over processes). Best first;
/// ties by id. Entities whose match is below [`MIN_LINK_SCORE`] are dropped.
pub fn link_entities(words: &[String], vocab: &Vocabulary) -> Vec<(String, f64)> {
    let words: Vec<String> = words
        .iter()
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<(String, f64)> = Vec::new();
    for e in &vocab.entities {
        let m = std::iter::once(&e.name)
            .chain(e.aliases.iter())
            .map(|n| name_match_quality(&words, n))
            .fold(0.0, f64::max);
        if m < MIN_LINK_SCORE {
            continue;
        }
        // Learned frecency replaces the cold-start prior (UX §5.4), exactly like the ranker.
        let cold = if e.frecency > 0.0 { 0.0 } else { e.prior.max(0.0) };
        let s = m + FRECENCY_LINK_BOOST * affinity(e.frecency) + kind_prior(e) + cold;
        out.push((e.id.clone(), (s * 1e6).round() / 1e6));
    }
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    out
}

// ---------------------------------------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------------------------------------

const BYTE_UNITS: &[&str] = &[
    "b", "k", "kb", "kib", "m", "mb", "mib", "g", "gb", "gib", "t", "tb", "tib",
];

fn unit_alias(u: &str) -> Option<&'static str> {
    Some(match u {
        "gig" | "gigs" | "gigabyte" | "gigabytes" => "GB",
        "meg" | "megs" | "megabyte" | "megabytes" => "MB",
        "terabyte" | "terabytes" => "TB",
        "kilobyte" | "kilobytes" => "KB",
        "gibibyte" | "gibibytes" => "GiB",
        _ => return None,
    })
}

const TIME_UNITS: &[&str] = &[
    "s", "sec", "secs", "min", "mins", "h", "hr", "hrs", "d", "day", "days", "hours", "hour", "minutes",
    "minute",
];

fn time_alias(u: &str) -> Option<&'static str> {
    Some(match u {
        "hours" | "hour" => "h",
        "minutes" | "minute" => "min",
        "seconds" | "second" => "s",
        _ => return None,
    })
}

enum Quantity {
    Bytes(u64),
    Duration(f64),
}

fn split_num(w: &str) -> Option<(&str, &str)> {
    let idx = w
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(w.len());
    if idx == 0 {
        return None;
    }
    Some((&w[..idx], &w[idx..]))
}

/// Interprets a number-with-unit word. `m` is ambiguous (MiB vs minutes): `prefer_time` decides.
fn quantity(w: &str, prefer_time: bool) -> Option<Quantity> {
    let (num, unit) = split_num(w)?;
    let unit_l = unit.to_ascii_lowercase();
    if unit_l.is_empty() {
        return None;
    }
    if unit_l == "m" {
        return if prefer_time {
            parse_duration_s(w).ok().map(Quantity::Duration)
        } else {
            parse_bytes(w).ok().map(Quantity::Bytes)
        };
    }
    if BYTE_UNITS.contains(&unit_l.as_str()) {
        return parse_bytes(w).ok().map(Quantity::Bytes);
    }
    if let Some(u) = unit_alias(&unit_l) {
        return parse_bytes(&format!("{num}{u}")).ok().map(Quantity::Bytes);
    }
    if TIME_UNITS.contains(&unit_l.as_str()) || time_alias(&unit_l).is_some() {
        let u = time_alias(&unit_l).unwrap_or(unit_l.as_str());
        return parse_duration_s(&format!("{num}{u}"))
            .ok()
            .map(Quantity::Duration);
    }
    // Compound durations like 1h30m.
    parse_duration_s(w).ok().map(Quantity::Duration)
}

/// Lowercases, strips punctuation (keeps `.`, digits, letters, `-`, `_`), and joins `13 gb` → `13gb`.
fn normalize_raw_words(input: &str) -> Vec<String> {
    let cleaned: String = input
        .to_lowercase()
        .replace(['\u{2019}', '\''], "")
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                ' '
            }
        })
        .collect();
    let raw: Vec<&str> = cleaned
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '.' || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let w = raw[i];
        let is_num =
            w.chars().all(|c| c.is_ascii_digit() || c == '.') && w.chars().any(|c| c.is_ascii_digit());
        if is_num && i + 1 < raw.len() {
            let u = raw[i + 1];
            if BYTE_UNITS.contains(&u)
                || unit_alias(u).is_some()
                || TIME_UNITS.contains(&u)
                || time_alias(u).is_some()
            {
                out.push(format!("{w}{u}"));
                i += 2;
                continue;
            }
        }
        out.push(w.to_string());
        i += 1;
    }
    out
}

/// True when `a` and `b` differ by exactly one swap of adjacent characters (`idel` ↔ `idle`).
fn is_adjacent_transposition(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len() != b.len() {
        return false;
    }
    let diff: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
    diff.len() == 2 && diff[1] == diff[0] + 1 && a[diff[0]] == b[diff[1]] && a[diff[1]] == b[diff[0]]
}

/// Fixes a typo against the lexicon (Damerau-Levenshtein ≤ 1). Never touches stopwords, numbers, entity name
/// tokens, or a word that is the **prefix** of an entity token (the palette re-understands on every keystroke,
/// so `clan` on the way to `clang` must not become `clean`). Words of exactly [`MIN_TYPO_LEN`] characters are
/// only fixed by an adjacent transposition (`idel` → `idle`), because a single insert/delete/substitute on a
/// 4-letter word too often hits a different real word (`zoom` → `room`, `heap` → `heat`).
fn fix_typo(w: &str, entity_tokens: &HashSet<String>, corrections: &mut Vec<Correction>) -> String {
    let len = w.chars().count();
    if len < MIN_TYPO_LEN
        || in_lexicon(w)
        || STOPWORDS.contains(&w)
        || entity_tokens.contains(w)
        || entity_tokens.iter().any(|t| t.starts_with(w))
        || w.chars().any(|c| c.is_ascii_digit())
    {
        return w.to_string();
    }
    let fixed = if len == MIN_TYPO_LEN {
        lexicon().filter(|c| is_adjacent_transposition(c, w)).min()
    } else {
        closest(w, lexicon(), MIN_TYPO_LEN)
    };
    match fixed {
        Some(fixed) => {
            corrections.push(Correction {
                from: w.to_string(),
                to: fixed.to_string(),
            });
            fixed.to_string()
        }
        None => w.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------------------
// Intent classification
// ---------------------------------------------------------------------------------------------------------

struct WordAnalysis {
    need: Option<u64>,
    idle_for: Option<f64>,
    kinds: Vec<GroupKind>,
    content: Vec<String>,
    scores: Vec<(Intent, f64)>,
    /// The user said "idle"/"unused" (→ `state:idle` on reclaim).
    said_idle: bool,
}

fn has(words: &[String], set: &[&str]) -> bool {
    words.iter().any(|w| set.contains(&w.as_str()))
}

fn analyze_words(words: &[String]) -> WordAnalysis {
    // `m` is MiB or minutes: minutes when the words talk about time ("idle for 30m"), bytes when they talk
    // about room ("room for 500m", "can I load 500m").
    let time_words = has(words, &["idle", "since", "ago", "older", "unused"]);
    let room_words = has(words, HEADROOM_VERBS) || has(words, HEADROOM_NOUNS);
    let prefer_time = time_words || (has(words, &["for"]) && !room_words);
    let mut need = None;
    let mut idle_for = None;
    let mut rest: Vec<String> = Vec::new();
    for w in words {
        match quantity(w, prefer_time) {
            Some(Quantity::Bytes(b)) => need = Some(b),
            Some(Quantity::Duration(d)) => idle_for = Some(d),
            None => rest.push(w.clone()),
        }
    }
    let metric = if has(&rest, GPU_WORDS) {
        Some(Metric::Gpu)
    } else if has(&rest, CPU_WORDS) {
        Some(Metric::Cpu)
    } else if has(&rest, MEM_WORDS) {
        Some(Metric::Mem)
    } else {
        None
    };
    let mut kinds: Vec<GroupKind> = Vec::new();
    for w in &rest {
        if KIND_WORDS.contains(&w.as_str()) {
            if let Some(k) = kind_synonym(w) {
                if !kinds.contains(&k) {
                    kinds.push(k);
                }
            }
        }
    }
    let content: Vec<String> = rest
        .iter()
        .filter(|w| !STOPWORDS.contains(&w.as_str()) && !in_lexicon(w))
        .cloned()
        .collect();
    // Words that carry meaning (not stopwords); used to decide "metric only" / "view only".
    let meaningful: Vec<&String> = rest.iter().filter(|w| !STOPWORDS.contains(&w.as_str())).collect();

    let mut scores: Vec<(Intent, f64)> = Vec::new();
    // Headroom.
    let headroom_verb = has(&rest, HEADROOM_VERBS);
    let headroom_noun = has(&rest, HEADROOM_NOUNS);
    if need.is_some() {
        let s = if headroom_verb || headroom_noun {
            0.95
        } else if meaningful.is_empty() {
            0.85
        } else {
            0.7
        };
        scores.push((Intent::Headroom, s));
    } else if headroom_noun {
        scores.push((Intent::Headroom, 0.75));
    }
    // Explain.
    if has(&rest, EXPLAIN_STRONG) {
        scores.push((Intent::Explain, 0.9));
    } else if has(&rest, EXPLAIN_WEAK) {
        scores.push((Intent::Explain, 0.75));
    }
    // Rank by metric.
    let rank_metric = metric.unwrap_or(Metric::Mem);
    if has(&rest, RANK_WORDS) {
        scores.push((Intent::RankBy(rank_metric), 0.9));
    } else if metric.is_some()
        && meaningful.iter().all(|w| {
            MEM_WORDS.contains(&w.as_str())
                || GPU_WORDS.contains(&w.as_str())
                || CPU_WORDS.contains(&w.as_str())
        })
    {
        scores.push((Intent::RankBy(rank_metric), 0.8));
    } else if metric.is_some() {
        scores.push((Intent::RankBy(rank_metric), 0.55));
    }
    // Reclaim.
    if has(&rest, RECLAIM_STRONG) {
        scores.push((Intent::Reclaim, 0.9));
    } else if has(&rest, &["idle", "unused"]) {
        scores.push((Intent::Reclaim, 0.8));
    } else if has(&rest, &["free"]) && has(&rest, &["up"]) || has(&rest, &["kill", "stop"]) {
        scores.push((Intent::Reclaim, 0.75));
    } else if has(&rest, &["free"]) {
        scores.push((Intent::Reclaim, 0.5));
        if need.is_none() {
            scores.push((Intent::Headroom, 0.55));
        }
    }
    // Navigate.
    if let Some(v) = rest.iter().find_map(|w| nav_word(w)) {
        let only = meaningful.len() == 1;
        scores.push((Intent::Navigate(v), if only { 0.8 } else { 0.45 }));
    }
    WordAnalysis {
        need,
        idle_for,
        kinds,
        content,
        scores,
        said_idle: has(&rest, &["idle", "unused"]),
    }
}

fn kind_filter(kinds: &[GroupKind]) -> Option<Expr> {
    (!kinds.is_empty()).then(|| {
        Expr::Term(Term::Field {
            key: FieldKey::Kind,
            values: kinds.iter().map(|k| k.alias().to_string()).collect(),
        })
    })
}

fn state_filter(v: &str) -> Expr {
    Expr::Term(Term::Field {
        key: FieldKey::State,
        values: vec![v.to_string()],
    })
}

fn fmt_need(b: u64) -> String {
    if b >= GIB && b.is_multiple_of(GIB) {
        format!("{} GiB", b / GIB)
    } else if b >= 1_000_000_000 && b.is_multiple_of(1_000_000_000) {
        format!("{} GB", b / 1_000_000_000)
    } else if b >= MIB && b.is_multiple_of(MIB) {
        format!("{} MiB", b / MIB)
    } else if b >= 1_000_000 && b.is_multiple_of(1_000_000) {
        format!("{} MB", b / 1_000_000)
    } else {
        crate::headline::format_amount(b, crate::units::UnitSystem::Si)
    }
}

/// An intent word at or above this score ("why", "slow", "stop", "hogs") outranks plain entity linking.
const STRONG_INTENT: f64 = 0.75;
/// How far below a strong intent the plain "go to <entity>" interpretation is placed.
const STRONG_INTENT_LEAD: f64 = 0.2;

/// Intents that act on the linked entities (their label names the top target).
fn targets_intent(i: &Intent) -> bool {
    matches!(i, Intent::Explain | Intent::Reclaim | Intent::RankBy(_))
}

fn describe(intent: &Intent, filter: Option<&Expr>, need: Option<u64>, target_name: Option<&str>) -> String {
    let base = match intent {
        Intent::Find => match (target_name, filter) {
            (Some(n), _) => format!("go to {n}"),
            (None, Some(f)) => format!("filter {f}"),
            (None, None) => "find".into(),
        },
        Intent::Headroom => match need {
            Some(b) => format!("can {} fit?", fmt_need(b)),
            None => "check headroom".into(),
        },
        other => match target_name {
            Some(n) => format!("{} · {n}", other.label()),
            None => other.label(),
        },
    };
    match (intent, filter) {
        (Intent::Find, _) | (_, None) => base,
        (_, Some(f)) => format!("{base} ({f})"),
    }
}

fn finish(mut u: Understanding, mut interps: Vec<Interpretation>) -> Understanding {
    interps.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    // Deduplicate by label, keep best 3.
    let mut seen = HashSet::new();
    interps.retain(|i| seen.insert(i.label.clone()));
    interps.truncate(3);
    if let Some(best) = interps.first() {
        u.intent = best.intent.clone();
        u.filter = best.filter.clone();
        u.sort = best.sort;
        u.need_bytes = best.need_bytes.or(u.need_bytes);
        let second = interps.get(1).map(|i| i.score).unwrap_or(0.0);
        u.confidence = if best.score - second < 0.1 {
            best.score.min(0.45)
        } else {
            best.score
        };
    }
    u.alternatives = if u.confidence < LOW_CONFIDENCE {
        interps.iter().skip(1).map(|i| i.intent.clone()).collect()
    } else {
        Vec::new()
    };
    u.interpretations = interps;
    u
}

fn fallback_interpretations() -> Vec<Interpretation> {
    [
        (Intent::RankBy(Metric::Mem), 0.2),
        (Intent::Explain, 0.19),
        (Intent::Reclaim, 0.18),
    ]
    .into_iter()
    .map(|(intent, score)| Interpretation {
        label: intent.label(),
        sort: match intent {
            Intent::RankBy(m) => Some(m),
            _ => None,
        },
        intent,
        filter: None,
        targets: Vec::new(),
        need_bytes: None,
        score,
    })
    .collect()
}

/// Link-derived confidence for a Find interpretation. Entities sharing a display name (e.g. four
/// "Claude Code" sessions) count as one: the margin is measured against the best differently named one.
fn find_confidence(targets: &[(String, f64)], vocab: &Vocabulary) -> f64 {
    let name = |id: &str| {
        vocab
            .entities
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.name.to_lowercase())
            .unwrap_or_else(|| id.to_string())
    };
    let Some((top_id, a)) = targets.first() else {
        return 0.0;
    };
    let top_name = name(top_id);
    match targets.iter().skip(1).find(|(id, _)| name(id) != top_name) {
        // A single candidate: confidence follows match quality (gappy fuzzy matches stay ambiguous).
        None if *a >= 0.9 => 0.9,
        None if *a >= 0.6 => 0.7,
        None => 0.45,
        Some((_, b)) => {
            let margin = a - b;
            if margin >= 0.25 {
                0.9
            } else if margin >= 0.1 {
                0.7
            } else {
                0.4
            }
        }
    }
}

fn plain_interpretations(
    wa: &WordAnalysis,
    extra_filter: Option<Expr>,
    vocab: &Vocabulary,
) -> (Vec<Interpretation>, Vec<(String, f64)>) {
    let targets = link_entities(&wa.content, vocab);
    let name_of = |id: &str| vocab.entities.iter().find(|e| e.id == id).map(|e| e.name.clone());
    let mut interps = Vec::new();
    for (intent, score) in &wa.scores {
        let mut parts: Vec<Expr> = extra_filter.iter().cloned().collect();
        if *intent == Intent::Reclaim {
            if let Some(d) = wa.idle_for {
                parts.push(Expr::Term(Term::Cmp {
                    metric: Metric::Idle,
                    op: CmpOp::Ge,
                    value: d,
                }));
            } else if wa.said_idle {
                parts.push(state_filter("idle"));
            }
        }
        if !matches!(intent, Intent::Navigate(_) | Intent::Headroom | Intent::Explain) {
            if let Some(k) = kind_filter(&wa.kinds) {
                parts.push(k);
            }
        }
        let filter = Expr::and_all(parts);
        let sort = match intent {
            Intent::RankBy(m) => Some(*m),
            _ => None,
        };
        let about = if targets_intent(intent) {
            targets.first().and_then(|(id, _)| name_of(id))
        } else {
            None
        };
        interps.push(Interpretation {
            label: describe(intent, filter.as_ref(), wa.need, about.as_deref()),
            intent: intent.clone(),
            filter,
            sort,
            targets: if matches!(intent, Intent::Find) {
                Vec::new()
            } else {
                targets.iter().take(3).cloned().collect()
            },
            need_bytes: if matches!(intent, Intent::Headroom) {
                wa.need
            } else {
                None
            },
            score: *score,
        });
    }
    // Find: one interpretation per top linked entity. When a strong intent word is present ("why is
    // sd-server slow", "stop chrome") that intent already carries the targets, so plain "go to" ranks below it
    // instead of tying with it (a tie would make every such query ambiguous).
    let strong_intent = wa
        .scores
        .iter()
        .filter(|(i, s)| targets_intent(i) && *s >= STRONG_INTENT)
        .map(|(_, s)| *s)
        .fold(0.0, f64::max);
    let mut fc = find_confidence(&targets, vocab);
    if strong_intent > 0.0 {
        fc = fc.min(strong_intent - STRONG_INTENT_LEAD);
    }
    for (rank, (id, _)) in targets.iter().take(3).enumerate() {
        let name = name_of(id).unwrap_or_else(|| id.clone());
        interps.push(Interpretation {
            intent: Intent::Find,
            filter: extra_filter.clone(),
            sort: None,
            targets: targets.iter().skip(rank).take(3).cloned().collect(),
            need_bytes: None,
            label: describe(&Intent::Find, None, None, Some(&name)),
            score: if rank == 0 {
                fc
            } else {
                (fc - 0.1 * rank as f64).clamp(0.05, 0.44)
            },
        });
    }
    // Kind words alone ("agents", "daemons") → Find kind:…
    if !wa.kinds.is_empty() {
        let mut parts: Vec<Expr> = extra_filter.iter().cloned().collect();
        parts.extend(kind_filter(&wa.kinds));
        let filter = Expr::and_all(parts);
        interps.push(Interpretation {
            label: describe(&Intent::Find, filter.as_ref(), None, None),
            intent: Intent::Find,
            filter,
            sort: None,
            targets: Vec::new(),
            need_bytes: None,
            score: 0.65,
        });
    }
    (interps, targets)
}

const COMMANDS: &[&str] = &[
    "reclaim",
    "headroom",
    "fit",
    "why",
    "pin",
    "unpin",
    "mute",
    "unmute",
    "rename",
    "mode",
    "sort",
    "save",
    "view",
    "filter",
    "help",
    "quit",
    "less",
    "watch",
    "stop",
    "suspend",
    "resume",
    "models",
    "sandboxes",
    "processes",
    "timeline",
    "home",
    "settings",
    "theme",
    "clear",
    "compare",
    "undo",
];

fn understand_command(cmd: &str, vocab: &Vocabulary) -> Understanding {
    let mut u = Understanding::empty();
    let mut parts = cmd.split_whitespace().map(str::to_string);
    let raw_name = parts.next().unwrap_or_default().to_lowercase();
    let args: Vec<String> = parts.collect();
    let (name, known) = if COMMANDS.contains(&raw_name.as_str()) {
        (raw_name.clone(), true)
    } else if let Some(fixed) = (raw_name.chars().count() >= 3)
        .then(|| closest(&raw_name, COMMANDS.iter().copied(), 3))
        .flatten()
    {
        u.corrections.push(Correction {
            from: raw_name.clone(),
            to: fixed.to_string(),
        });
        (fixed.to_string(), true)
    } else {
        (raw_name.clone(), false)
    };
    u.normalized = std::iter::once(format!(":{name}"))
        .chain(args.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    let joined_args = normalize_raw_words(&args.join(" "));
    let link_args = |a: &[String]| link_entities(&normalize_raw_words(&a.join(" ")), vocab);
    let intent = match name.as_str() {
        "reclaim" => Intent::Reclaim,
        "headroom" | "fit" => {
            u.need_bytes = joined_args.iter().find_map(|w| match quantity(w, false) {
                Some(Quantity::Bytes(b)) => Some(b),
                _ => None,
            });
            // A bare number is not guessed at (GB? GiB?): ask for a unit instead of silently ignoring it.
            if u.need_bytes.is_none() {
                if let Some(n) = joined_args
                    .iter()
                    .find(|w| w.parse::<f64>().is_ok_and(|x| x.is_finite()))
                {
                    u.error = Some(format!("add a unit to {n}, e.g. {n}G or {n}GB"));
                }
            }
            Intent::Headroom
        }
        "why" => Intent::Explain,
        "sort" => {
            let m = args.first().and_then(|a| Metric::parse(a)).unwrap_or(Metric::Mem);
            u.sort = Some(m);
            Intent::RankBy(m)
        }
        "view" => args
            .first()
            .and_then(|a| View::parse(a))
            .map(Intent::Navigate)
            .unwrap_or(Intent::Find),
        "models" | "sandboxes" | "processes" | "timeline" | "home" => {
            Intent::Navigate(View::parse(&name).unwrap_or(View::Home))
        }
        "filter" => {
            match parse_filter_corrected(&args.join(" ")) {
                Ok((f, c)) => {
                    u.filter = Some(f);
                    u.corrections.extend(c);
                }
                Err(e) => u.error = Some(e.to_string()),
            }
            Intent::Find
        }
        "pin" | "unpin" | "mute" | "unmute" | "watch" | "stop" | "suspend" | "resume" | "less" => {
            u.targets = link_args(&args);
            Intent::Find
        }
        "rename" => {
            // `:rename <entity…> <alias>`: the last word is the new name.
            if args.len() >= 2 {
                u.targets = link_args(&args[..args.len() - 1]);
            }
            Intent::Find
        }
        _ => Intent::Find,
    };
    u.intent = intent.clone();
    u.command = Some(Command { name, args });
    u.confidence = if !known {
        0.3
    } else if u.corrections.is_empty() {
        1.0
    } else {
        0.9
    };
    if !known {
        let mut sugg: Vec<(&str, f64)> = COMMANDS
            .iter()
            .filter_map(|c| fuzzy_match(&raw_name, c).map(|s| (*c, s)))
            .collect();
        sugg.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(b.0))
        });
        u.interpretations = sugg
            .into_iter()
            .take(3)
            .map(|(c, s)| Interpretation {
                intent: Intent::Find,
                filter: None,
                sort: None,
                targets: Vec::new(),
                need_bytes: None,
                label: format!(":{c}"),
                score: (s * 0.4).min(0.4),
            })
            .collect();
    } else {
        u.interpretations = vec![Interpretation {
            intent,
            filter: u.filter.clone(),
            sort: u.sort,
            targets: u.targets.clone(),
            need_bytes: u.need_bytes,
            label: format!(":{}", u.command.as_ref().map(|c| c.name.as_str()).unwrap_or("")),
            score: u.confidence,
        }];
    }
    u
}

fn looks_structured(toks: &[Tok]) -> bool {
    toks.iter().any(|t| match t {
        Tok::LParen | Tok::RParen | Tok::Or | Tok::Not | Tok::Neg | Tok::Lit(_) => true,
        Tok::Atom(a) => split_key_op(a).is_some(),
    })
}

/// Understands any input: `:command`, structured filter, plain words, or a mix of filter terms and words.
pub fn understand(input: &str, vocab: &Vocabulary) -> Understanding {
    let input = input.trim();
    if let Some(cmd) = input.strip_prefix(':') {
        return understand_command(cmd, vocab);
    }
    let mut u = Understanding::empty();
    if input.is_empty() {
        u.confidence = 1.0;
        return u;
    }
    let entity_tokens = vocab.tokens();

    // Structured (or mixed) input. A filter that looks structured but doesn't parse is reported as an
    // error (the palette shows it inline) instead of being guessed at as plain words.
    match tokenize(input) {
        Ok(toks) if looks_structured(&toks) => {
            let mut corrections = Vec::new();
            match parse_tokens(toks, &mut corrections) {
                Ok(expr) => return understand_structured(expr, corrections, vocab, &entity_tokens),
                Err(e) => {
                    u.error = Some(e.to_string());
                    u.corrections = corrections;
                    u.normalized = input.to_lowercase();
                    return u;
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            u.error = Some(e.to_string());
            u.normalized = input.to_lowercase();
            return u;
        }
    }

    // Plain words.
    let raw = normalize_raw_words(input);
    let words: Vec<String> = raw
        .iter()
        .map(|w| fix_typo(w, &entity_tokens, &mut u.corrections))
        .collect();
    u.normalized = words.join(" ");
    let wa = analyze_words(&words);
    u.need_bytes = wa.need;
    let (interps, targets) = plain_interpretations(&wa, None, vocab);
    u.targets = targets;
    if interps.is_empty() {
        u.confidence = 0.2;
        u.interpretations = fallback_interpretations();
        u.alternatives = u.interpretations.iter().map(|i| i.intent.clone()).collect();
        u
    } else {
        finish(u, interps)
    }
}

fn understand_structured(
    expr: Expr,
    corrections: Vec<Correction>,
    vocab: &Vocabulary,
    entity_tokens: &HashSet<String>,
) -> Understanding {
    let mut u = Understanding::empty();
    u.corrections = corrections;
    // Split a top-level conjunction into structured parts and plain words. A phrase (a quoted word with
    // spaces or punctuation, e.g. `"Claude Code"`) stays one literal filter term; it still links entities.
    let is_phrase = |x: &str| {
        !x.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    let mut phrases: Vec<String> = Vec::new();
    let (structured, words): (Vec<Expr>, Vec<String>) = match expr {
        Expr::And(parts) => {
            let mut s = Vec::new();
            let mut w = Vec::new();
            for p in parts {
                match p {
                    Expr::Term(Term::Word(x)) if !is_phrase(&x) => w.push(x),
                    Expr::Term(Term::Word(x)) => {
                        phrases.push(x.clone());
                        s.push(Expr::Term(Term::Word(x)));
                    }
                    other => s.push(other),
                }
            }
            (s, w)
        }
        Expr::Term(Term::Word(x)) if !is_phrase(&x) => (Vec::new(), vec![x]),
        Expr::Term(Term::Word(x)) => {
            phrases.push(x.clone());
            (vec![Expr::Term(Term::Word(x))], Vec::new())
        }
        other => (vec![other], Vec::new()),
    };
    let norm_words: Vec<String> = normalize_raw_words(&words.join(" "))
        .iter()
        .map(|w| fix_typo(w, entity_tokens, &mut u.corrections))
        .collect();
    let wa = analyze_words(&norm_words);
    // Content words stay in the filter as name/state matches (a mixed query like `chrome mem>1G`).
    let mut parts = structured;
    for w in &wa.content {
        parts.push(Expr::Term(Term::Word(w.clone())));
    }
    let filter = Expr::and_all(parts);
    let intent_words: Vec<String> = norm_words
        .iter()
        .filter(|w| !wa.content.contains(w))
        .cloned()
        .collect();
    let filter_text = filter.as_ref().map(|f| f.to_string()).unwrap_or_default();
    u.normalized = format!("{} {filter_text}", intent_words.join(" "))
        .trim()
        .to_string();
    let link_words: Vec<String> = wa.content.iter().chain(phrases.iter()).cloned().collect();
    u.targets = link_entities(&link_words, vocab);
    u.need_bytes = wa.need;
    let metrics = filter.as_ref().map(|f| f.metrics()).unwrap_or_default();
    let default_sort = (metrics.len() == 1).then(|| metrics[0]);
    let mut interps: Vec<Interpretation> = wa
        .scores
        .iter()
        .map(|(intent, score)| Interpretation {
            label: describe(intent, filter.as_ref(), wa.need, None),
            intent: intent.clone(),
            filter: filter.clone(),
            sort: match intent {
                Intent::RankBy(m) => Some(*m),
                _ => default_sort,
            },
            targets: u.targets.iter().take(3).cloned().collect(),
            need_bytes: wa.need,
            score: *score,
        })
        .collect();
    interps.push(Interpretation {
        label: describe(&Intent::Find, filter.as_ref(), None, None),
        intent: Intent::Find,
        filter: filter.clone(),
        sort: default_sort,
        targets: u.targets.iter().take(3).cloned().collect(),
        need_bytes: None,
        // A parsed filter is unambiguous; intent words (if any) win over plain filtering.
        score: if wa.scores.is_empty() { 1.0 } else { 0.6 },
    });
    let mut u = finish(u, interps);
    // With intent words present the filter still applies to that intent.
    if u.filter.is_none() {
        u.filter = filter;
    }
    if u.sort.is_none() {
        u.sort = default_sort;
    }
    // Structured parts are explicit, so confidence stays high unless words were ambiguous.
    if wa.scores.is_empty() {
        u.confidence = 1.0;
    }
    u
}

// ---------------------------------------------------------------------------------------------------------
// Learned completion (UX §5.2)
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionKind {
    PastQuery,
    Entity,
    Command,
    Key,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub text: String,
    pub kind: SuggestionKind,
    pub score: f64,
}

const FILTER_KEYS: &[&str] = &[
    "kind:", "owner:", "name:", "state:", "sandbox:", "mem>", "gpu>", "cpu>", "idle>",
];

/// Palette completion: past queries (frecency-ranked, `past` = (query, frecency)), entity names (frecency
/// boosted), commands after `:`, and filter keys. Best first, at most `limit`.
pub fn suggest(input: &str, past: &[(String, f64)], vocab: &Vocabulary, limit: usize) -> Vec<Suggestion> {
    let q = input.trim();
    let mut out: Vec<Suggestion> = Vec::new();
    let push = |out: &mut Vec<Suggestion>, text: String, kind, score: f64| {
        if !out.iter().any(|s| s.text == text) && text != q {
            out.push(Suggestion { text, kind, score });
        }
    };
    if let Some(c) = q.strip_prefix(':') {
        for cmd in COMMANDS {
            if let Some(s) = fuzzy_match(c, cmd) {
                if c.is_empty() || s > 0.3 {
                    push(&mut out, format!(":{cmd}"), SuggestionKind::Command, 1.0 + s);
                }
            }
        }
    }
    for (text, fr) in past {
        let s = if q.is_empty() {
            Some(0.5)
        } else if text.to_lowercase().starts_with(&q.to_lowercase()) {
            Some(1.0)
        } else {
            fuzzy_match(q, text)
        };
        if let Some(s) = s.filter(|s| *s >= MIN_LINK_SCORE) {
            push(
                &mut out,
                text.clone(),
                SuggestionKind::PastQuery,
                s + affinity(*fr),
            );
        }
    }
    if !q.is_empty() && !q.starts_with(':') {
        let last = q.split_whitespace().last().unwrap_or("");
        for (id, s) in link_entities(&[last.to_string()], vocab).into_iter().take(limit) {
            if let Some(e) = vocab.entities.iter().find(|e| e.id == id) {
                push(&mut out, e.name.clone(), SuggestionKind::Entity, s);
            }
        }
        if last.chars().all(|c| c.is_ascii_alphabetic()) && !last.is_empty() {
            for k in FILTER_KEYS {
                if k.starts_with(&last.to_ascii_lowercase()) {
                    push(&mut out, (*k).to_string(), SuggestionKind::Key, 0.4);
                }
            }
        }
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.text.cmp(&b.text))
    });
    out.truncate(limit);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::GIB;

    fn view(kind: GroupKind, name: &str, mem: u64, gpu: u64) -> EntityView {
        EntityView {
            kind,
            name: name.into(),
            mem: Some(mem),
            gpu: Some(gpu),
            ..Default::default()
        }
    }

    #[test]
    fn grammar_example() {
        let e = parse_filter("kind:model,agent or gpu>1G -muted").unwrap();
        let m = view(GroupKind::ModelServer, "sd-server", 10 * GIB, 0);
        let c = view(GroupKind::App, "Chrome", GIB, 2 * GIB);
        let mut muted = view(GroupKind::App, "Chrome", GIB, 2 * GIB);
        muted.state.push("muted".into());
        let x = view(GroupKind::App, "Slack", GIB, 0);
        assert!(eval(&e, &m));
        assert!(eval(&e, &c));
        assert!(!eval(&e, &muted));
        assert!(!eval(&e, &x));
        assert_eq!(e.to_string(), "kind:model,agent or gpu>1G -muted");
        assert_eq!(parse_filter(&e.to_string()).unwrap(), e);
    }

    #[test]
    fn structured_filters() {
        let e = parse_filter("mem>2G kind:daemon idle>30m").unwrap();
        let mut v = view(GroupKind::BuildDaemon, "GradleDaemon", 3 * GIB, 0);
        v.idle_s = Some(5 * 3600);
        assert!(eval(&e, &v));
        v.idle_s = Some(60);
        assert!(!eval(&e, &v));
        assert!(eval(&parse_filter("not (kind:app or kind:system)").unwrap(), &v));
        assert!(eval(&parse_filter("-(kind:app or kind:system)").unwrap(), &v));
        assert!(eval(&parse_filter(r#"name:"Gradle""#).unwrap(), &v));
        assert!(eval(&parse_filter("name:grad*").unwrap(), &v));
        assert!(!eval(&parse_filter("name:kot*").unwrap(), &v));
        assert!(matches!(parse_filter("foo:bar"), Err(QueryError::UnknownKey(_))));
        assert!(matches!(
            parse_filter("kind:nope"),
            Err(QueryError::BadValue { .. })
        ));
        assert!(matches!(parse_filter("(kind:app"), Err(QueryError::Parens)));
        assert!(matches!(parse_filter("kind:app)"), Err(QueryError::Parens)));
        assert!(matches!(parse_filter("\"open"), Err(QueryError::Quote)));
        assert!(matches!(parse_filter("mem>2"), Err(QueryError::BadValue { .. })));
        assert!(matches!(parse_filter("mem:2G"), Err(QueryError::BadValue { .. })));
        assert!(matches!(
            parse_filter("name>2G"),
            Err(QueryError::BadValue { .. })
        ));
        assert!(matches!(parse_filter("a or"), Err(QueryError::Empty)));
    }

    #[test]
    fn spaced_and_alias_forms() {
        let a = parse_filter("mem > 2G").unwrap();
        let b = parse_filter("mem>2G").unwrap();
        let c = parse_filter("mem:>2G").unwrap();
        let d = parse_filter("memory >2g").unwrap();
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(c, d);
        assert_eq!(
            parse_filter("kind:models,daemons").unwrap().to_string(),
            "kind:model,daemon"
        );
        assert_eq!(parse_filter("is:orphaned").unwrap().to_string(), "state:orphan");
        assert_eq!(parse_filter("cpu>=50%").unwrap().to_string(), "cpu>=50");
        assert_eq!(parse_filter("idle>1h30m").unwrap().to_string(), "idle>90m");
        let (e, c) = parse_filter_corrected("knid:modle").unwrap();
        assert_eq!(e.to_string(), "kind:model");
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn unavailable_metric_never_matches() {
        let mut v = view(GroupKind::App, "x", 0, 0);
        v.gpu = None;
        assert!(!eval(&parse_filter("gpu<1G").unwrap(), &v));
        assert!(!eval(&parse_filter("gpu>=0").unwrap(), &v));
        assert!(eval(&parse_filter("-gpu>1G").unwrap(), &v));
    }

    #[test]
    fn sandbox_and_owner() {
        let mut v = view(GroupKind::Sandbox, "Claude VM", GIB, 0);
        v.sandbox = Some("Claude VM".into());
        v.add_state("runtime:virtualization.framework");
        v.owner = Some("Claude Code".into());
        assert!(eval(&parse_filter("sandbox:*").unwrap(), &v));
        assert!(eval(&parse_filter("sandbox:virtualization*").unwrap(), &v));
        assert!(eval(&parse_filter(r#"owner:"Claude Code""#).unwrap(), &v));
        assert!(!eval(&parse_filter("owner:codex").unwrap(), &v));
        let app = view(GroupKind::App, "Chrome", GIB, 0);
        assert!(!eval(&parse_filter("sandbox:*").unwrap(), &app));
    }

    #[test]
    fn plain_words_intents() {
        let v = Vocabulary::default();
        assert_eq!(understand("gpu hogs", &v).intent, Intent::RankBy(Metric::Gpu));
        assert_eq!(
            understand("what's eating memory", &v).intent,
            Intent::RankBy(Metric::Mem)
        );
        assert_eq!(understand("why slow", &v).intent, Intent::Explain);
        let u = understand("idle stuff", &v);
        assert_eq!(u.intent, Intent::Reclaim);
        assert_eq!(u.filter.unwrap().to_string(), "state:idle");
        assert_eq!(understand("models", &v).intent, Intent::Navigate(View::Models));
        let u = understand("can I load 13g", &v);
        assert_eq!(u.intent, Intent::Headroom);
        assert_eq!(u.need_bytes, Some(13 * GIB));
        let u = understand("can I load 13 GB", &v);
        assert_eq!(u.need_bytes, Some(13_000_000_000));
        let u = understand("throtle", &v);
        assert_eq!(u.intent, Intent::Explain, "typo corrected");
        assert_eq!(u.corrections[0].to, "throttle");
        let u = understand("mem>2G", &v);
        assert!(u.filter.is_some());
        let u = understand(":headroom 13G", &v);
        assert_eq!(u.intent, Intent::Headroom);
        assert_eq!(u.need_bytes, Some(13 * GIB));
        let u = understand("idle daemons", &v);
        assert_eq!(u.intent, Intent::Reclaim);
        assert_eq!(u.filter.unwrap().to_string(), "state:idle kind:daemon");
        let u = understand("idle for 2h", &v);
        assert_eq!(u.filter.unwrap().to_string(), "idle>=2h");
    }

    #[test]
    fn mixed_words_and_filters() {
        let v = Vocabulary::default();
        let u = understand("gpu hogs kind:model", &v);
        assert_eq!(u.intent, Intent::RankBy(Metric::Gpu));
        assert_eq!(u.filter.unwrap().to_string(), "kind:model");
        let u = understand("chrome mem>1G", &v);
        assert_eq!(u.intent, Intent::Find);
        assert_eq!(u.filter.unwrap().to_string(), "mem>1G chrome");
    }

    #[test]
    fn commands() {
        let vocab = Vocabulary {
            entities: vec![VocabEntity {
                id: "model:sd".into(),
                name: "sd-server".into(),
                ..Default::default()
            }],
        };
        let u = understand(":pin sd-server", &vocab);
        assert_eq!(u.command.as_ref().unwrap().name, "pin");
        assert_eq!(u.targets[0].0, "model:sd");
        let u = understand(":reclam", &vocab);
        assert_eq!(u.intent, Intent::Reclaim);
        assert_eq!(u.confidence, 0.9);
        let u = understand(":sort gpu", &vocab);
        assert_eq!(u.intent, Intent::RankBy(Metric::Gpu));
        let u = understand(":zzzz", &vocab);
        assert!(u.is_ambiguous());
        let u = understand(":mode pressure", &vocab);
        assert_eq!(u.command.unwrap().args, vec!["pressure".to_string()]);
    }

    #[test]
    fn cl_prefers_frecent_claude() {
        let vocab = Vocabulary {
            entities: vec![
                VocabEntity {
                    id: "pid:42".into(),
                    name: "clang".into(),
                    ..Default::default()
                },
                VocabEntity {
                    id: "agent:claude".into(),
                    name: "Claude Code".into(),
                    frecency: 12.0,
                    ..Default::default()
                },
            ],
        };
        let u = understand("cl", &vocab);
        assert_eq!(u.intent, Intent::Find);
        assert_eq!(u.targets[0].0, "agent:claude");
        // Typing the full name of the other entity wins over frecency.
        let u = understand("clang", &vocab);
        assert_eq!(u.targets[0].0, "pid:42");
    }

    #[test]
    fn fuzzy_scoring() {
        assert!(fuzzy_score("cl", "clang").is_some());
        assert!(fuzzy_score("xz", "clang").is_none());
        assert_eq!(fuzzy_score("", "x"), Some(0.0));
        // Word-start matches beat mid-word matches.
        let a = fuzzy_score("cc", "Claude Code").unwrap();
        let b = fuzzy_score("cc", "accept").unwrap();
        assert!(a > b, "{a} {b}");
        assert!(fuzzy_match("sd", "sd-server").unwrap() > 0.95);
        assert!(fuzzy_match("gd", "GradleDaemon").unwrap() > fuzzy_match("gd", "bigdata").unwrap());
        // Best alignment, not leftmost: "code" should hit the word "Code".
        assert!(fuzzy_match("code", "Claude Code").unwrap() > 0.95);
    }

    #[test]
    fn dl_distance() {
        assert_eq!(damerau_levenshtein("throttle", "throtle"), 1);
        assert_eq!(damerau_levenshtein("ab", "ba"), 1);
        assert_eq!(damerau_levenshtein("", "abc"), 3);
        assert_eq!(damerau_levenshtein("kitten", "sitting"), 3);
        assert_eq!(damerau_levenshtein("daemons", "daemnos"), 1);
    }

    #[test]
    fn typo_never_rewrites_entity_names() {
        let vocab = Vocabulary {
            entities: vec![VocabEntity {
                id: "app:slot".into(),
                name: "Slot".into(),
                ..Default::default()
            }],
        };
        // "slot" is DL 1 from "slow" but it's a known entity token.
        let u = understand("slot", &vocab);
        assert_eq!(u.intent, Intent::Find);
        assert!(u.corrections.is_empty());
        assert_eq!(u.targets[0].0, "app:slot");
    }

    #[test]
    fn suggestions() {
        let vocab = Vocabulary {
            entities: vec![VocabEntity {
                id: "agent:c".into(),
                name: "Claude Code".into(),
                frecency: 3.0,
                ..Default::default()
            }],
        };
        let past = vec![("gpu hogs".to_string(), 4.0), ("kind:model".to_string(), 1.0)];
        let s = suggest("gp", &past, &vocab, 5);
        assert_eq!(s[0].text, "gpu hogs");
        let s = suggest("cla", &past, &vocab, 5);
        assert!(s.iter().any(|x| x.text == "Claude Code"));
        let s = suggest(":rec", &past, &vocab, 3);
        assert_eq!(s[0].text, ":reclaim");
        let s = suggest("ki", &past, &vocab, 5);
        assert!(s.iter().any(|x| x.text == "kind:"));
    }
}

//! Attribution (SPEC §7): group every process under the agent session, app, sandbox, model server, build
//! daemon or system component that owns it. Pure: works over a [`Snapshot`], compiled rules and lineage data.
//!
//! ## Signals, in the order they are tried for each process
//! 0. **oomtop itself** — its own process tree forms the `oomtop` group, except that an `oomtop mcp` child
//!    stays inside the agent session that launched it (and is excluded from that session's reclaim gain).
//! 1. **Adapter truth** — pids that adapters already mapped (`Snapshot::model_servers[].pids`,
//!    `Snapshot::sandboxes[].host_pids`) when attribution runs after an adapter probe.
//! 2. **cgroup path** (Linux) — container / VM scopes (docker, podman, containerd, CRI-O, kubepods, LXC,
//!    libvirt/QEMU machines) form sandbox groups.
//! 3. **Rule roots** — an identity match (`match.exe|name|script|cmdline|cwd|bundle|cgroup`, OR'd) makes the
//!    process a group root. A root merges into a same-rule parent (`ollama serve` → `ollama runner`), and a
//!    root with `session_key` is keyed by the session id it inherits from its marker-carrying descendants,
//!    so a session's id is `agent:<hash12>` and survives restarts of child processes.
//! 4. **Session markers** — a process carrying an allowlisted marker (`CLAUDECODE`, hashed
//!    `CLAUDE_CODE_SESSION_ID`) joins the live session with that id, even after being re-parented.
//! 5. **Responsible pid** (macOS) — XPC services / helpers join the app that is responsible for them (the
//!    `com.apple.Virtualization.VirtualMachine` XPC → Claude.app). Ignored when the responsible process is a
//!    terminal or the process was launched from a shell (macOS makes the terminal "responsible" for every
//!    command typed into it).
//! 6. **Ancestry** — the ppid chain. Terminals, interactive shells and `sudo`-style launchers are
//!    *boundaries*: a command typed into a shell becomes its own root unless the shell itself belongs to a
//!    rule-defined group (e.g. an agent session), so the terminal app does not swallow everything run in it;
//!    terminal plumbing (`login`, interactive shells, `tmux`, `sudo`) stays in the terminal's group.
//!    `launchd` / `systemd` / `init` are subreapers, never owners.
//! 7. **Lineage journal** — a re-parented process (recorded ppid > 1, now 1 / a subreaper / a reused pid)
//!    rejoins its recorded group while that group is alive; otherwise it becomes an **orphan root** whose
//!    `owner_group` names the group that spawned it.
//! 8. **Dead-session markers** — like 7 for marker carriers whose session root has exited.
//! 9. **systemd unit / app scope** (Linux) — top-level processes of the same `.service` / `app-*.scope` share
//!    a group.
//! 10. **Heuristic** — the process is a top-level root: apps are keyed by bundle name (`app:google-chrome`),
//!     system daemons by name, everything else by name + pid.
//!
//! Every process lands in exactly one group; groups are sorted by footprint desc; group ids are stable
//! across samples for the same root. Identity is always `(pid, start_time)`: a parent whose start time is
//! after its child's is a reused pid and is not treated as the parent.
//!
//! ## Rule semantics
//! `[[group]]` in `rules.d/*.toml` (SPEC §15): within `match`, **fields are OR'd** (any identity field
//! matching makes the process a group root) and values within a field are OR'd. `match.env` is not an
//! identity match: it groups processes carrying those marker keys into the session (by `session_key`) of the
//! rule's root. `exclude` vetoes a match. Higher `priority` wins; ties → first rule. Globs are case-sensitive;
//! `match.exe` is tried against the executable path, its basename and argv[0] (self-updating CLIs such as
//! Claude Code run from a versioned file, so argv[0] is the stable name).

use crate::actions::{is_protected_process, ProtectContext};
use crate::fingerprint;
use crate::measured::{sum_bytes, sum_f64};
use crate::model::{
    AttributionSignal, Confidence, Group, GroupKind, GroupTotals, Member, ModelServerKind, ProcId, Process,
    SandboxKind, Snapshot,
};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use thiserror::Error;

/// Group id of oomtop's own processes.
pub const SELF_GROUP_ID: &str = "oomtop";
/// Ancestry walks stop after this many hops (cycle / pathological-depth guard).
const MAX_DEPTH: usize = 256;

// ---------------------------------------------------------------------------------------------------------
// Rules
// ---------------------------------------------------------------------------------------------------------

/// Identity match fields of a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct MatchSpec {
    /// Globs against the executable path, its basename and argv[0] (e.g. "claude", "**/bin/sd-server").
    pub exe: Vec<String>,
    /// Globs against the short process name (comm).
    pub name: Vec<String>,
    /// Globs against any argv element after argv[0] (node/python scripts, java main classes).
    pub script: Vec<String>,
    /// Regexes against the space-joined command line.
    pub cmdline: Vec<String>,
    /// Marker keys (must be in the allowlist); groups carriers into the rule's session. Not an identity match.
    pub env: Vec<String>,
    /// Globs against cwd.
    pub cwd: Vec<String>,
    /// Globs against the macOS bundle id.
    pub bundle: Vec<String>,
    /// Globs against the Linux cgroup path.
    pub cgroup: Vec<String>,
}

impl MatchSpec {
    fn has_identity(&self) -> bool {
        !(self.exe.is_empty()
            && self.name.is_empty()
            && self.script.is_empty()
            && self.cmdline.is_empty()
            && self.cwd.is_empty()
            && self.bundle.is_empty()
            && self.cgroup.is_empty())
    }
}

/// One detection rule (TOML `[[group]]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rule {
    /// Optional stable id ("builtin:claude-code"); defaults to the label slug.
    pub id: Option<String>,
    pub kind: GroupKind,
    pub label: String,
    #[serde(rename = "match")]
    pub matcher: MatchSpec,
    pub exclude: Option<MatchSpec>,
    /// "env:KEY" → one group per distinct (hashed) marker value.
    pub session_key: Option<String>,
    pub priority: i32,
    /// Matching processes are protected (never offered for actions).
    pub protected: bool,
}

impl Default for Rule {
    fn default() -> Self {
        Rule {
            id: None,
            kind: GroupKind::Other,
            label: String::new(),
            matcher: MatchSpec::default(),
            exclude: None,
            session_key: None,
            priority: 0,
            protected: false,
        }
    }
}

impl Rule {
    pub fn rule_id(&self) -> String {
        self.id.clone().unwrap_or_else(|| slug(&self.label))
    }

    /// The marker key named by `session_key = "env:KEY"`.
    pub fn session_env_key(&self) -> Option<&str> {
        self.session_key
            .as_deref()
            .and_then(|k| k.strip_prefix("env:"))
            .map(str::trim)
            .filter(|k| !k.is_empty())
    }
}

/// An ordered rule set (built-in rules first, then user rules; user rules usually carry higher priority).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RuleSet {
    #[serde(rename = "group")]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RuleError {
    #[error("rule {rule:?}: bad glob {pattern:?}: {msg}")]
    Glob {
        rule: String,
        pattern: String,
        msg: String,
    },
    #[error("rule {rule:?}: bad regex {pattern:?}: {msg}")]
    Regex {
        rule: String,
        pattern: String,
        msg: String,
    },
    #[error("rule {rule:?}: empty label")]
    EmptyLabel { rule: String },
    #[error("rule {rule:?}: session_key {key:?} must look like \"env:KEY\"")]
    SessionKey { rule: String, key: String },
}

#[derive(Debug, Clone)]
struct CompiledSpec {
    exe: GlobSet,
    name: GlobSet,
    script: GlobSet,
    cmdline: Vec<Regex>,
    cwd: GlobSet,
    bundle: GlobSet,
    cgroup: GlobSet,
    env: Vec<String>,
    has_identity: bool,
}

#[derive(Debug, Clone)]
struct CompiledRule {
    rule: Rule,
    id: String,
    spec: CompiledSpec,
    exclude: Option<CompiledSpec>,
}

/// All rules' globs for one field in a single [`GlobSet`] (glob index → rule index), so a process is
/// matched once per field instead of once per rule.
#[derive(Debug, Clone, Default)]
struct GlobIndex {
    set: GlobSet,
    owner: Vec<usize>,
}

impl GlobIndex {
    fn build(set: &RuleSet, pick: impl Fn(&MatchSpec) -> &Vec<String>) -> Result<Self, RuleError> {
        let mut b = GlobSetBuilder::new();
        let mut owner = Vec::new();
        for (i, r) in set.rules.iter().enumerate() {
            for pat in pick(&r.matcher) {
                let g = GlobBuilder::new(pat)
                    .case_insensitive(false)
                    .literal_separator(false)
                    .build()
                    .map_err(|e| RuleError::Glob {
                        rule: r.rule_id(),
                        pattern: pat.clone(),
                        msg: e.to_string(),
                    })?;
                b.add(g);
                owner.push(i);
            }
        }
        let set = b.build().map_err(|e| RuleError::Glob {
            rule: "(all rules)".into(),
            pattern: String::new(),
            msg: e.to_string(),
        })?;
        Ok(GlobIndex { set, owner })
    }

    fn is_empty(&self) -> bool {
        self.owner.is_empty()
    }

    fn hits(&self, value: &str, buf: &mut Vec<usize>, cand: &mut [bool]) {
        if self.is_empty() || value.is_empty() {
            return;
        }
        self.set.matches_into(value, buf);
        for &g in buf.iter() {
            cand[self.owner[g]] = true;
        }
    }
}

#[derive(Debug, Clone)]
struct RegexIndex {
    set: regex::RegexSet,
    owner: Vec<usize>,
}

impl Default for RegexIndex {
    fn default() -> Self {
        RegexIndex {
            set: regex::RegexSet::empty(),
            owner: Vec::new(),
        }
    }
}

/// Most memo entries kept before the memo is reset (a long session sees many short-lived processes).
const MEMO_CAP: usize = 16_384;

/// Signature of the identity fields that rule matching and fingerprinting read (exe, name, argv, cwd,
/// bundle id, cgroup). Equal signatures give equal results, so results are memoized by it.
fn identity_sig(p: &Process) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.exe.hash(&mut h);
    p.name.hash(&mut h);
    p.cmdline.hash(&mut h);
    p.cwd.hash(&mut h);
    p.bundle_id.hash(&mut h);
    p.cgroup.hash(&mut h);
    h.finish()
}

/// Memo of pure per-process results — the root rule match and the group fingerprint — keyed by
/// [`identity_sig`]. A process's identity rarely changes between samples, so re-matching ~700 processes
/// against every glob and regex each refresh is wasted work (SPEC §14 CPU budget). Interior mutability
/// keeps `match_root(&self)` unchanged; a poisoned lock just disables the memo.
#[derive(Debug, Default)]
struct Memo {
    roots: HashMap<u64, Option<usize>>,
    fingerprints: HashMap<(u64, GroupKind), String>,
    /// [`heuristic_kind_label`] also reads the uid and pid.
    heuristics: HashMap<(u64, Option<u32>, u32), (GroupKind, String)>,
}

#[derive(Debug, Default)]
struct MemoCell(std::sync::Mutex<Memo>);

impl Clone for MemoCell {
    fn clone(&self) -> Self {
        MemoCell::default()
    }
}

/// Rules compiled for matching.
#[derive(Debug, Clone, Default)]
pub struct CompiledRules {
    memo: MemoCell,
    rules: Vec<CompiledRule>,
    exe: GlobIndex,
    name: GlobIndex,
    script: GlobIndex,
    cwd: GlobIndex,
    bundle: GlobIndex,
    cgroup: GlobIndex,
    cmdline: RegexIndex,
}

fn glob_set(rule: &str, pats: &[String]) -> Result<GlobSet, RuleError> {
    let mut b = GlobSetBuilder::new();
    for p in pats {
        let g: Glob = GlobBuilder::new(p)
            .case_insensitive(false)
            .literal_separator(false)
            .build()
            .map_err(|e| RuleError::Glob {
                rule: rule.to_string(),
                pattern: p.clone(),
                msg: e.to_string(),
            })?;
        b.add(g);
    }
    b.build().map_err(|e| RuleError::Glob {
        rule: rule.to_string(),
        pattern: String::new(),
        msg: e.to_string(),
    })
}

fn compile_spec(rule: &str, s: &MatchSpec) -> Result<CompiledSpec, RuleError> {
    let cmdline = s
        .cmdline
        .iter()
        .map(|p| {
            Regex::new(p).map_err(|e| RuleError::Regex {
                rule: rule.to_string(),
                pattern: p.clone(),
                msg: e.to_string(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CompiledSpec {
        exe: glob_set(rule, &s.exe)?,
        name: glob_set(rule, &s.name)?,
        script: glob_set(rule, &s.script)?,
        cmdline,
        cwd: glob_set(rule, &s.cwd)?,
        bundle: glob_set(rule, &s.bundle)?,
        cgroup: glob_set(rule, &s.cgroup)?,
        env: s.env.clone(),
        has_identity: s.has_identity(),
    })
}

fn basename(path: &str) -> &str {
    let p = path.trim_end_matches('/');
    p.rsplit('/').next().unwrap_or(p)
}

/// Per-process strings computed once for matching against every rule.
struct ProcView<'a> {
    p: &'a Process,
    exe_base: &'a str,
    argv0: Option<&'a str>,
    joined: String,
}

impl<'a> ProcView<'a> {
    fn new(p: &'a Process) -> Self {
        ProcView {
            p,
            exe_base: basename(&p.exe),
            argv0: p.cmdline.first().map(String::as_str).filter(|a| !a.is_empty()),
            joined: p.cmdline.join(" "),
        }
    }
}

impl CompiledSpec {
    fn identity_match(&self, v: &ProcView<'_>) -> bool {
        if !self.has_identity {
            return false;
        }
        let p = v.p;
        if !self.exe.is_empty() {
            if !p.exe.is_empty() && (self.exe.is_match(&p.exe) || self.exe.is_match(v.exe_base)) {
                return true;
            }
            if let Some(a0) = v.argv0 {
                if self.exe.is_match(a0) || self.exe.is_match(basename(a0)) {
                    return true;
                }
            }
        }
        if !p.name.is_empty() && self.name.is_match(&p.name) {
            return true;
        }
        if !self.script.is_empty() && p.cmdline.iter().skip(1).any(|a| self.script.is_match(a)) {
            return true;
        }
        if !self.cmdline.is_empty() && self.cmdline.iter().any(|r| r.is_match(&v.joined)) {
            return true;
        }
        if let Some(cwd) = &p.cwd {
            if self.cwd.is_match(cwd) {
                return true;
            }
        }
        if let Some(b) = &p.bundle_id {
            if self.bundle.is_match(b) {
                return true;
            }
        }
        if let Some(cg) = &p.cgroup {
            if self.cgroup.is_match(cg) {
                return true;
            }
        }
        false
    }

    fn env_match(&self, p: &Process) -> bool {
        !self.env.is_empty() && self.env.iter().any(|k| p.markers.keys.iter().any(|m| m == k))
    }
}

/// A rule match for one process.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleMatch<'a> {
    pub index: usize,
    pub rule: &'a Rule,
}

impl CompiledRules {
    pub fn compile(set: &RuleSet) -> Result<CompiledRules, RuleError> {
        let mut rules = Vec::with_capacity(set.rules.len());
        for r in &set.rules {
            let id = r.rule_id();
            if r.label.trim().is_empty() {
                return Err(RuleError::EmptyLabel { rule: id });
            }
            if let Some(k) = &r.session_key {
                if r.session_env_key().is_none() {
                    return Err(RuleError::SessionKey {
                        rule: id,
                        key: k.clone(),
                    });
                }
            }
            let spec = compile_spec(&id, &r.matcher)?;
            let exclude = r.exclude.as_ref().map(|e| compile_spec(&id, e)).transpose()?;
            rules.push(CompiledRule {
                rule: r.clone(),
                id,
                spec,
                exclude,
            });
        }
        let mut pats = Vec::new();
        let mut owner = Vec::new();
        for (i, r) in set.rules.iter().enumerate() {
            for p in &r.matcher.cmdline {
                pats.push(p.clone());
                owner.push(i);
            }
        }
        let cmdline = RegexIndex {
            set: regex::RegexSet::new(&pats).map_err(|e| RuleError::Regex {
                rule: "(all rules)".into(),
                pattern: String::new(),
                msg: e.to_string(),
            })?,
            owner,
        };
        Ok(CompiledRules {
            memo: MemoCell::default(),
            rules,
            exe: GlobIndex::build(set, |m| &m.exe)?,
            name: GlobIndex::build(set, |m| &m.name)?,
            script: GlobIndex::build(set, |m| &m.script)?,
            cwd: GlobIndex::build(set, |m| &m.cwd)?,
            bundle: GlobIndex::build(set, |m| &m.bundle)?,
            cgroup: GlobIndex::build(set, |m| &m.cgroup)?,
            cmdline,
        })
    }

    /// Rules whose identity fields match (same semantics as matching each rule's spec in turn).
    fn identity_candidates(&self, v: &ProcView<'_>) -> Vec<bool> {
        let mut cand = vec![false; self.rules.len()];
        let mut buf = Vec::new();
        let p = v.p;
        if !p.exe.is_empty() {
            self.exe.hits(&p.exe, &mut buf, &mut cand);
            self.exe.hits(v.exe_base, &mut buf, &mut cand);
        }
        if let Some(a0) = v.argv0 {
            self.exe.hits(a0, &mut buf, &mut cand);
            self.exe.hits(basename(a0), &mut buf, &mut cand);
        }
        self.name.hits(&p.name, &mut buf, &mut cand);
        if !self.script.is_empty() {
            for a in p.cmdline.iter().skip(1) {
                self.script.hits(a, &mut buf, &mut cand);
            }
        }
        if !self.cmdline.owner.is_empty() {
            for i in self.cmdline.set.matches(&v.joined).iter() {
                cand[self.cmdline.owner[i]] = true;
            }
        }
        if let Some(c) = &p.cwd {
            self.cwd.hits(c, &mut buf, &mut cand);
        }
        if let Some(b) = &p.bundle_id {
            self.bundle.hits(b, &mut buf, &mut cand);
        }
        if let Some(c) = &p.cgroup {
            self.cgroup.hits(c, &mut buf, &mut cand);
        }
        cand
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rule at `index` (as returned in [`RuleMatch::index`]).
    pub fn rule(&self, index: usize) -> Option<&Rule> {
        self.rules.get(index).map(|r| &r.rule)
    }

    fn best<'a>(
        &'a self,
        v: &ProcView<'_>,
        f: impl Fn(&CompiledSpec, &ProcView<'_>) -> bool,
        precomputed: Option<&[bool]>,
    ) -> Option<RuleMatch<'a>> {
        let mut best: Option<RuleMatch<'a>> = None;
        for (i, r) in self.rules.iter().enumerate() {
            let hit = match precomputed {
                Some(c) => c[i] && r.spec.has_identity,
                None => f(&r.spec, v),
            };
            if !hit {
                continue;
            }
            if r.exclude.as_ref().map(|e| e.identity_match(v)).unwrap_or(false) {
                continue;
            }
            if best
                .as_ref()
                .map(|b| r.rule.priority > b.rule.priority)
                .unwrap_or(true)
            {
                best = Some(RuleMatch {
                    index: i,
                    rule: &r.rule,
                });
            }
        }
        best
    }

    /// Best identity match (process becomes a group root). Memoized by the process's identity fields.
    pub fn match_root(&self, p: &Process) -> Option<RuleMatch<'_>> {
        let sig = identity_sig(p);
        if let Ok(m) = self.memo.0.lock() {
            if let Some(hit) = m.roots.get(&sig) {
                return hit.map(|index| RuleMatch {
                    index,
                    rule: &self.rules[index].rule,
                });
            }
        }
        let found = self.match_root_uncached(p);
        if let Ok(mut m) = self.memo.0.lock() {
            if m.roots.len() >= MEMO_CAP {
                m.roots.clear();
            }
            m.roots.insert(sig, found.as_ref().map(|r| r.index));
        }
        found
    }

    fn match_root_uncached(&self, p: &Process) -> Option<RuleMatch<'_>> {
        let v = ProcView::new(p);
        let cand = self.identity_candidates(&v);
        self.best(&v, |_, _| false, Some(&cand))
    }

    /// [`heuristic_kind_label`], memoized by the process's identity fields, uid and pid.
    fn heuristic_kind_label(&self, p: &Process) -> (GroupKind, String) {
        let key = (identity_sig(p), p.uid, p.id.pid);
        if let Ok(m) = self.memo.0.lock() {
            if let Some(v) = m.heuristics.get(&key) {
                return v.clone();
            }
        }
        let v = heuristic_kind_label(p);
        if let Ok(mut m) = self.memo.0.lock() {
            if m.heuristics.len() >= MEMO_CAP {
                m.heuristics.clear();
            }
            m.heuristics.insert(key, v.clone());
        }
        v
    }

    /// [`fingerprint::group_fingerprint`], memoized by kind + the root's identity fields.
    fn group_fingerprint(&self, kind: GroupKind, root: &Process) -> String {
        let key = (identity_sig(root), kind);
        if let Ok(m) = self.memo.0.lock() {
            if let Some(fp) = m.fingerprints.get(&key) {
                return fp.clone();
            }
        }
        let fp = fingerprint::group_fingerprint(kind, root);
        if let Ok(mut m) = self.memo.0.lock() {
            if m.fingerprints.len() >= MEMO_CAP {
                m.fingerprints.clear();
            }
            m.fingerprints.insert(key, fp.clone());
        }
        fp
    }

    /// Reference implementation of [`Self::match_root`] (one rule at a time), kept for equivalence tests.
    #[cfg(test)]
    fn match_root_naive(&self, p: &Process) -> Option<RuleMatch<'_>> {
        self.best(&ProcView::new(p), |s, v| s.identity_match(v), None)
    }

    /// Best marker (env) match (process joins a session of the rule).
    pub fn match_marker(&self, p: &Process) -> Option<RuleMatch<'_>> {
        if p.markers.keys.is_empty() {
            return None;
        }
        self.best(&ProcView::new(p), |s, v| s.env_match(v.p), None)
    }

    fn is_protected_rule(&self, id: &str) -> bool {
        self.rules.iter().any(|r| r.id == id && r.rule.protected)
    }
}

// ---------------------------------------------------------------------------------------------------------
// Lineage, context
// ---------------------------------------------------------------------------------------------------------

/// Lineage journal entry (SPEC §6.2), stored by `oomtop-state`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LineageEntry {
    pub id: ProcId,
    pub ppid: Option<u32>,
    pub group_id: String,
    pub group_kind: GroupKind,
    pub group_label: String,
    pub group_fingerprint: String,
    /// Hashed session id.
    pub session_id: Option<String>,
    /// true if spawned (directly or transitively) by an agent session or tool.
    pub spawned_by_agent: bool,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub last_active_ms: u64,
}

/// Inputs besides the snapshot.
#[derive(Debug, Clone, Copy)]
pub struct AttributionContext<'a> {
    pub rules: &'a CompiledRules,
    pub lineage: &'a BTreeMap<ProcId, LineageEntry>,
    pub protect: &'a ProtectContext,
}

/// Lower-case, dash-separated slug.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

// ---------------------------------------------------------------------------------------------------------
// Process classification (terminals, shells, subreapers, app bundles)
// ---------------------------------------------------------------------------------------------------------

const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "oksh", "loksh", "tcsh", "csh", "ash", "yash", "nu",
    "elvish", "xonsh", "pwsh",
];

/// Terminal emulators, multiplexers and remote-login daemons (process / executable names).
const TERMINALS: &[&str] = &[
    "Terminal",
    "iTerm2",
    "iTerm",
    "ghostty",
    "Ghostty",
    "kitty",
    "alacritty",
    "Alacritty",
    "wezterm",
    "wezterm-gui",
    "wezterm-mux-server",
    "WezTerm",
    "Hyper",
    "Tabby",
    "rio",
    "gnome-terminal-server",
    "gnome-terminal-",
    "gnome-terminal",
    "kgx",
    "ptyxis",
    "ptyxis-agent",
    "konsole",
    "xterm",
    "uxterm",
    "urxvt",
    "rxvt",
    "foot",
    "footclient",
    "tilix",
    "terminator",
    "xfce4-terminal",
    "lxterminal",
    "mate-terminal",
    "qterminal",
    "st",
    "contour",
    "tmux",
    "tmux: server",
    "screen",
    "SCREEN",
    "zellij",
    "dtach",
    "abduco",
    "sshd",
    "sshd-session",
    "login",
    "mosh-server",
];

/// macOS terminal app bundle names (the part before `.app/`).
const TERMINAL_APPS: &[&str] = &[
    "Terminal",
    "iTerm",
    "iTerm2",
    "Ghostty",
    "kitty",
    "Alacritty",
    "WezTerm",
    "Warp",
    "Hyper",
    "Tabby",
    "Rio",
];

/// Launchers that exec a command and stay around as its parent.
const TRANSPARENT: &[&str] = &[
    "sudo",
    "doas",
    "su",
    "caffeinate",
    "script",
    "nohup",
    "timeout",
    "nice",
    "ionice",
    "taskpolicy",
];

/// Process reapers: their children are top-level processes, never their members.
const SUBREAPERS: &[&str] = &["launchd", "systemd", "init"];

fn names_of(p: &Process) -> [&str; 3] {
    [
        p.name.trim_start_matches('-'),
        basename(&p.exe),
        p.cmdline
            .first()
            .map(|a| basename(a).trim_start_matches('-'))
            .unwrap_or(""),
    ]
}

fn name_in(p: &Process, list: &[&str]) -> bool {
    names_of(p).iter().any(|n| !n.is_empty() && list.contains(n))
}

/// Bundle markers in an executable path. Chrome's updater runs the app from a temporary
/// `…/code_sign_clone/…/Google Chrome.app.bundle/Contents/MacOS/Google Chrome` copy.
const BUNDLE_MARKERS: [&str; 2] = [".app/", ".app.bundle/"];

fn bundle_in(path: &str) -> Option<&str> {
    let i = BUNDLE_MARKERS.iter().filter_map(|m| path.find(m)).min()?;
    let name = basename(&path[..i]);
    (!name.is_empty()).then_some(name)
}

/// The outermost `.app` bundle name of the executable (`Google Chrome` for a Chrome helper). Falls back
/// to `argv[0]` when the executable runs from a non-bundle copy.
pub fn app_bundle_name(p: &Process) -> Option<&str> {
    bundle_in(&p.exe).or_else(|| p.cmdline.first().and_then(|a| bundle_in(a)))
}

/// True for the main executable of an app bundle (`X.app/Contents/MacOS/X`), not a nested helper.
fn is_app_main(p: &Process) -> bool {
    let n: usize = BUNDLE_MARKERS.iter().map(|m| p.exe.matches(m).count()).sum();
    n == 1
        && BUNDLE_MARKERS
            .iter()
            .any(|m| p.exe.contains(&format!("{m}Contents/MacOS/")))
}

/// Terminal emulators, multiplexers, sshd / login.
pub fn is_terminal(p: &Process) -> bool {
    name_in(p, TERMINALS)
        || (is_app_main(p)
            && app_bundle_name(p)
                .map(|a| TERMINAL_APPS.contains(&a))
                .unwrap_or(false))
}

/// Any shell process (interactive or not).
pub fn is_shell(p: &Process) -> bool {
    name_in(p, SHELLS)
}

/// A shell waiting for user commands: no `-c`, no script argument (only flags such as `-l`, `-i`).
pub fn is_interactive_shell(p: &Process) -> bool {
    if !is_shell(p) {
        return false;
    }
    p.cmdline.iter().skip(1).all(|a| {
        if a == "-c" || a == "-s" || !a.starts_with('-') {
            return false;
        }
        // short flag clusters such as `-lc`
        !(a.len() > 1 && !a.starts_with("--") && a[1..].contains('c'))
    })
}

fn is_subreaper(p: &Process) -> bool {
    p.id.pid <= 1 || name_in(p, SUBREAPERS)
}

/// A process whose children start their own groups (unless it sits in a rule-defined group).
pub fn is_boundary(p: &Process) -> bool {
    is_terminal(p) || is_interactive_shell(p) || name_in(p, TRANSPARENT)
}

/// Virtualization.framework guest processes: host footprint under-counts them (SPEC §5).
pub fn is_vm_lower_bound(p: &Process) -> bool {
    p.name.contains("com.apple.Virtualization.VirtualMachine")
        || p.exe.contains("com.apple.Virtualization.VirtualMachine")
}

/// Heuristic kind + label for a top-level process with no rule.
pub fn heuristic_kind_label(p: &Process) -> (GroupKind, String) {
    if let Some(app) = app_bundle_name(p) {
        return (GroupKind::App, app.to_string());
    }
    let exe = p.exe.as_str();
    let system_prefixes = [
        "/System/",
        "/usr/libexec/",
        "/usr/sbin/",
        "/sbin/",
        "/usr/lib/systemd/",
        "/lib/systemd/",
    ];
    let short = if p.name.is_empty() {
        basename(exe).to_string()
    } else {
        p.name.clone()
    };
    // Terminals are user apps even when installed under /usr/libexec (gnome-terminal-server): commands
    // typed into them need a non-system owner.
    let system_path = system_prefixes.iter().any(|pre| exe.starts_with(pre)) && !is_terminal(p);
    if system_path || p.uid == Some(0) || p.id.pid <= 1 {
        return (GroupKind::System, short);
    }
    // Interpreted programs are named after what they run ("studio.py", "@scope/pkg", "GradleDaemon").
    let head = fingerprint::template_head(p);
    let label = if head.is_empty() || head.starts_with('<') || head.contains(" -c") {
        short
    } else {
        head
    };
    (
        GroupKind::Other,
        if label.is_empty() {
            format!("pid {}", p.id.pid)
        } else {
            label
        },
    )
}

// ---------------------------------------------------------------------------------------------------------
// cgroups (Linux)
// ---------------------------------------------------------------------------------------------------------

/// What a Linux cgroup v2 path says about a process's owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum CgroupScope {
    /// Container runtime scope (docker, podman, containerd, cri-o, kubepods, lxc, nspawn). `id` ≤ 12 chars.
    Container { runtime: String, id: String },
    /// libvirt / QEMU machine scope.
    Vm { name: String },
    /// A systemd service unit; `system` = under `system.slice`.
    Service { unit: String, system: bool },
    /// A desktop app scope (`app-gnome-firefox-1234.scope`, `snap.firefox.firefox-….scope`).
    App { app: String },
}

fn unescape_systemd(s: &str) -> String {
    s.replace("\\x2d", "-").replace("\\x20", " ")
}

fn is_hex_id(s: &str) -> bool {
    s.len() >= 12 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn short_id(s: &str) -> String {
    s.chars().take(12).collect()
}

fn strip_instance_suffix(s: &str) -> &str {
    // trailing "-<pid>" / "-<uuid>" / "-<hex>"
    let mut cur = s;
    loop {
        let Some((head, tail)) = cur.rsplit_once('-') else {
            return cur;
        };
        let instance = !tail.is_empty()
            && tail.bytes().all(|b| b.is_ascii_hexdigit())
            && (tail.bytes().any(|b| b.is_ascii_digit()));
        if instance && !head.is_empty() {
            cur = head;
        } else {
            return cur;
        }
    }
}

/// Parses a cgroup v2 path (as read from `/proc/<pid>/cgroup`, `0::` prefix optional).
pub fn parse_cgroup(path: &str) -> Option<CgroupScope> {
    let path = path.trim().trim_start_matches("0::");
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    // Containers and VMs anywhere in the path.
    for (i, seg) in segs.iter().enumerate() {
        let seg = *seg;
        let scope = seg.strip_suffix(".scope");
        if let Some(s) = scope {
            for (prefix, runtime) in [
                ("docker-", "docker"),
                ("libpod-conmon-", "podman"),
                ("libpod-", "podman"),
                ("cri-containerd-", "containerd"),
                ("crio-conmon-", "cri-o"),
                ("crio-", "cri-o"),
                ("containerd-", "containerd"),
            ] {
                if let Some(id) = s.strip_prefix(prefix) {
                    if is_hex_id(id) {
                        return Some(CgroupScope::Container {
                            runtime: runtime.into(),
                            id: short_id(id),
                        });
                    }
                }
            }
            if let Some(m) = s.strip_prefix("machine-") {
                let m = unescape_systemd(m);
                if let Some(rest) = m.strip_prefix("qemu-") {
                    // "qemu-<n>-<name>"
                    let name = rest.split_once('-').map(|(_, n)| n).unwrap_or(rest);
                    return Some(CgroupScope::Vm {
                        name: name.to_string(),
                    });
                }
                return Some(CgroupScope::Container {
                    runtime: "nspawn".into(),
                    id: m,
                });
            }
        }
        if let Some(name) = seg.strip_prefix("lxc.payload.") {
            return Some(CgroupScope::Container {
                runtime: "lxc".into(),
                id: name.to_string(),
            });
        }
        if (seg == "docker" || seg.starts_with("kubepods") || seg.starts_with("pod")) && i + 1 < segs.len() {
            let next = segs[i + 1];
            if is_hex_id(next) {
                let runtime = if seg == "docker" { "docker" } else { "kubernetes" };
                return Some(CgroupScope::Container {
                    runtime: runtime.into(),
                    id: short_id(next),
                });
            }
        }
    }
    let last = *segs.last()?;
    if last.ends_with("-spawn")
        || last.starts_with("vte-spawn-")
        || last.starts_with("tmux-spawn-")
        || last.starts_with("session-")
        || last == "init.scope"
    {
        return None;
    }
    if let Some(unit) = last.strip_suffix(".service") {
        if unit.starts_with("user@") || unit.is_empty() {
            return None;
        }
        let system = segs.first() == Some(&"system.slice");
        let unit = unescape_systemd(unit);
        let unit = unit.split('@').next().unwrap_or(&unit).to_string();
        return Some(CgroupScope::Service { unit, system });
    }
    let scope = last.strip_suffix(".scope")?;
    let raw = if let Some(rest) = scope.strip_prefix("app-") {
        let rest = unescape_systemd(rest);
        let rest = strip_instance_suffix(&rest).to_string();
        let rest = [
            "gnome-",
            "kde-",
            "xfce-",
            "flatpak-",
            "glib-",
            "dbus-:1.",
            "systemd-",
            "niri-",
            "sway-",
            "hyprland-",
        ]
        .iter()
        .find_map(|pre| rest.strip_prefix(pre).map(str::to_string))
        .unwrap_or(rest);
        rest
    } else {
        let rest = scope.strip_prefix("snap.")?;
        rest.split('.').next().unwrap_or(rest).to_string()
    };
    // reverse-DNS app ids → last component ("org.mozilla.firefox" → "firefox")
    let app = if raw.contains('.') && !raw.ends_with('.') {
        raw.rsplit('.').next().unwrap_or(&raw).to_string()
    } else {
        raw
    };
    if app.is_empty()
        || TERMINALS.contains(&app.as_str())
        || TERMINAL_APPS.contains(&app.as_str())
        || app.eq_ignore_ascii_case("terminal")
    {
        return None;
    }
    Some(CgroupScope::App { app })
}

// ---------------------------------------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Slot {
    group: usize,
    via: AttributionSignal,
    confidence: Confidence,
}

#[derive(Debug, Clone, Default)]
struct Meta {
    /// Rule/adapter/cgroup/self groups keep shell-launched descendants.
    sticky: bool,
    /// Root is a terminal: never a responsible-pid owner.
    launcher_root: bool,
}

#[derive(Debug, Clone)]
struct AdapterHint {
    key: String,
    kind: GroupKind,
    label: String,
    matched_by: String,
}

fn model_server_label(k: ModelServerKind) -> &'static str {
    match k {
        ModelServerKind::Ollama => "Ollama",
        ModelServerKind::LlamaCpp => "llama.cpp server",
        ModelServerKind::SdCpp => "sd-server",
        ModelServerKind::Vllm => "vLLM",
        ModelServerKind::LmStudio => "LM Studio",
        ModelServerKind::Mlx => "MLX-LM server",
        ModelServerKind::Generic => "model server",
    }
}

/// Group key of a session: `<alias>:<first 12 hex of the hashed session id>`. A session id that is not
/// hash-shaped (e.g. from an old journal or a hand-made fixture) is hashed first, so a raw id never becomes
/// part of a group id (ids are exported) and slicing never splits a multi-byte character.
fn session_group_key(kind: GroupKind, sid: &str) -> String {
    let hashed = sid.len() >= 12 && sid.bytes().all(|b| b.is_ascii_hexdigit());
    let h: String = if hashed {
        sid.chars().take(12).collect()
    } else {
        crate::redact::hash_marker(sid).chars().take(12).collect()
    };
    format!("{}:{h}", kind.alias())
}

struct Builder<'a> {
    snap: &'a Snapshot,
    rules: &'a CompiledRules,
    lineage: &'a BTreeMap<ProcId, LineageEntry>,
    by_pid: HashMap<u32, usize>,
    by_id: HashMap<ProcId, usize>,
    groups: Vec<Group>,
    meta: Vec<Meta>,
    key_to_group: HashMap<String, usize>,
    assigned: Vec<Option<Slot>>,
    visiting: Vec<bool>,
    root_rule: Vec<Option<usize>>,
    /// Session id of each session-keyed rule root.
    root_sid: Vec<Option<String>>,
    adapter: Vec<Option<AdapterHint>>,
    /// Session group keys that have a live rule root in this snapshot.
    live_sessions: HashSet<String>,
    /// oomtop's own uid: label-keyed app groups of other (non-root) users get a `@u<uid>` suffix, so
    /// another logged-in user's Chrome never merges with ours (fast user switching).
    self_uid: Option<u32>,
}

impl<'a> Builder<'a> {
    /// `""`, or `@u<uid>` for a process of another non-root user.
    fn user_suffix(&self, p: &Process) -> String {
        match (p.uid, self.self_uid) {
            (Some(u), Some(me)) if u != me && u != 0 => format!("@u{u}"),
            _ => String::new(),
        }
    }

    fn proc(&self, i: usize) -> &'a Process {
        &self.snap.processes[i]
    }

    fn proc_by_id(&self, id: ProcId) -> Option<&'a Process> {
        self.by_id.get(&id).map(|&i| &self.snap.processes[i])
    }

    fn ensure_group(&mut self, key: &str, kind: GroupKind, label: &str, root: Option<ProcId>) -> usize {
        if let Some(&i) = self.key_to_group.get(key) {
            if self.groups[i].root.is_none() {
                self.groups[i].root = root;
            }
            return i;
        }
        let i = self.groups.len();
        self.groups.push(Group {
            id: key.to_string(),
            kind,
            label: label.to_string(),
            root,
            ..Default::default()
        });
        self.meta.push(Meta::default());
        self.key_to_group.insert(key.to_string(), i);
        i
    }

    fn set_root_meta(&mut self, g: usize) {
        let root = self.groups[g].root.and_then(|r| self.proc_by_id(r));
        self.meta[g].launcher_root = root.map(is_terminal).unwrap_or(false);
    }

    /// Index of the live parent (not a subreaper, not a reused pid).
    fn parent_index(&self, i: usize) -> Option<usize> {
        let p = self.proc(i);
        let pp = p.ppid.filter(|&pp| pp > 1 && pp != p.id.pid)?;
        let pi = *self.by_pid.get(&pp)?;
        let parent = self.proc(pi);
        if pi == i || is_subreaper(parent) {
            return None;
        }
        if parent.id.start_time > 0 && p.id.start_time > 0 && parent.id.start_time > p.id.start_time {
            return None; // pid reused after our real parent exited
        }
        Some(pi)
    }

    fn resolve(&mut self, i: usize, depth: usize) -> Option<usize> {
        if let Some(s) = self.assigned[i] {
            return Some(s.group);
        }
        if self.visiting[i] || depth > MAX_DEPTH {
            return None;
        }
        self.visiting[i] = true;
        let slot = self.decide(i, depth);
        self.visiting[i] = false;
        self.assigned[i] = Some(slot);
        Some(slot.group)
    }

    fn marker_session(&self, i: usize) -> Option<(String, usize)> {
        let p = self.proc(i);
        let sid = p.markers.session_id.as_ref()?;
        let m = self.rules.match_marker(p)?;
        let key = m.rule.session_env_key()?;
        if !p.markers.keys.iter().any(|k| k == key) {
            return None;
        }
        Some((session_group_key(m.rule.kind, sid), m.index))
    }

    fn decide(&mut self, i: usize, depth: usize) -> Slot {
        let p = self.proc(i);

        // 0. oomtop itself
        if Some(p.id.pid) == self.snap.self_pid {
            if let Some(pg) = self.parent_index(i).and_then(|pi| self.resolve(pi, depth + 1)) {
                if self.groups[pg].kind == GroupKind::AgentSession {
                    return Slot {
                        group: pg,
                        via: AttributionSignal::Ancestry,
                        confidence: Confidence::High,
                    };
                }
            }
            let g = self.ensure_group(SELF_GROUP_ID, GroupKind::Other, "oomtop", Some(p.id));
            self.groups[g].matched_by = Some("self".into());
            self.meta[g].sticky = true;
            return Slot {
                group: g,
                via: AttributionSignal::Rule,
                confidence: Confidence::High,
            };
        }

        // 1. adapter truth (rule roots keep their rule key so ids stay stable with/without adapter data)
        if self.root_rule[i].is_none() {
            if let Some(h) = self.adapter[i].clone() {
                let g = self.ensure_group(&h.key, h.kind, &h.label, Some(p.id));
                self.groups[g].matched_by.get_or_insert(h.matched_by);
                self.meta[g].sticky = true;
                return Slot {
                    group: g,
                    via: AttributionSignal::Adapter,
                    confidence: Confidence::High,
                };
            }
        }

        // 2. container / VM cgroup
        if let Some(scope) = p.cgroup.as_deref().and_then(parse_cgroup) {
            let hit = match scope {
                CgroupScope::Container { runtime, id } => Some((
                    format!("sandbox:{runtime}-{}", slug(&id)),
                    format!("{runtime} {id}"),
                    format!("cgroup:{runtime}"),
                )),
                CgroupScope::Vm { name } => Some((
                    format!("sandbox:vm-{}", slug(&name)),
                    format!("VM {name}"),
                    "cgroup:machine".to_string(),
                )),
                _ => None,
            };
            if let Some((key, label, by)) = hit {
                let g = self.ensure_group(&key, GroupKind::Sandbox, &label, Some(p.id));
                self.groups[g].matched_by.get_or_insert(by);
                self.meta[g].sticky = true;
                return Slot {
                    group: g,
                    via: AttributionSignal::Cgroup,
                    confidence: Confidence::High,
                };
            }
        }

        // 3. rule root
        if let Some(r) = self.root_rule[i] {
            return self.rule_root(i, r, depth);
        }

        // 4. live session marker — unless the parent belongs to a different rule/adapter-defined group (a
        //    Gradle worker under its daemon, a model server's workers): those inherit the agent's env but
        //    are owned, and freed, by their own root.
        let marker = self.marker_session(i);
        if let Some((key, ri)) = &marker {
            let parent_group = self
                .parent_index(i)
                .and_then(|pi| self.resolve(pi, depth + 1))
                .filter(|&pg| {
                    let g = &self.groups[pg];
                    self.meta[pg].sticky
                        && g.id != *key
                        && g.id != SELF_GROUP_ID
                        && g.kind != GroupKind::AgentSession
                        && g.kind != GroupKind::System
                });
            if let Some(pg) = parent_group {
                return Slot {
                    group: pg,
                    via: AttributionSignal::Ancestry,
                    confidence: Confidence::High,
                };
            }
            if self.live_sessions.contains(key) {
                let rule = &self.rules.rules[*ri];
                let (kind, label, rid) = (rule.rule.kind, rule.rule.label.clone(), rule.id.clone());
                let g = self.ensure_group(key, kind, &label, None);
                self.groups[g].matched_by.get_or_insert(rid);
                self.meta[g].sticky = true;
                return Slot {
                    group: g,
                    via: AttributionSignal::Marker,
                    confidence: Confidence::High,
                };
            }
        }

        // 5. responsible pid (macOS)
        if let Some(g) = self.responsible_group(i, depth) {
            return Slot {
                group: g,
                via: AttributionSignal::Responsible,
                confidence: Confidence::High,
            };
        }

        // 6. ancestry
        if let Some(pi) = self.parent_index(i) {
            if let Some(pg) = self.resolve(pi, depth + 1) {
                let parent = self.proc(pi);
                // A command typed into a shell/terminal is its own root; terminal plumbing (login, shells,
                // tmux, sudo) stays with the terminal.
                if is_boundary(parent) && !self.meta[pg].sticky && !is_boundary(p) {
                    let owner =
                        (self.groups[pg].kind != GroupKind::System).then(|| self.groups[pg].id.clone());
                    return self.heuristic_root(i, owner);
                }
                let parent_high = self.assigned[pi]
                    .map(|s| s.confidence == Confidence::High)
                    .unwrap_or(false);
                return Slot {
                    group: pg,
                    via: AttributionSignal::Ancestry,
                    confidence: if parent_high {
                        Confidence::High
                    } else {
                        Confidence::Medium
                    },
                };
            }
        }

        // --- top-level from here on (parent exited, is a subreaper, or unknown) ---

        // 7. lineage journal
        if let Some(le) = self.lineage.get(&p.id).filter(|le| !le.group_id.is_empty()) {
            if reparented(p, le, self.snap, &self.by_pid) {
                // Once an orphan is its own root the journal records *that* group; its owner is then the
                // recorded session (or the marker's), never itself.
                let recorded_self = le.group_id == orphan_identity(p).2;
                let (target, kind, label) = if recorded_self {
                    let sid_key = le
                        .session_id
                        .as_deref()
                        .map(|sid| session_group_key(GroupKind::AgentSession, sid))
                        .or_else(|| marker.as_ref().map(|(k, _)| k.clone()));
                    (sid_key, GroupKind::AgentSession, String::new())
                } else {
                    (Some(le.group_id.clone()), le.group_kind, le.group_label.clone())
                };
                let agent = le.spawned_by_agent || p.markers.session_id.is_some();
                if let Some(t) = target {
                    // A recorded non-session group is alive only if its root predates this process (group
                    // keys contain pids; a younger root is a reused pid, not the group that spawned it).
                    let live = self.live_sessions.contains(&t)
                        || self
                            .key_to_group
                            .get(&t)
                            .and_then(|&g| self.groups[g].root)
                            .and_then(|r| self.proc_by_id(r))
                            .map(|r| r.id.start_time <= p.id.start_time)
                            .unwrap_or(false);
                    if live {
                        let label = if label.is_empty() { t.clone() } else { label };
                        let g = self.ensure_group(&t, kind, &label, None);
                        return Slot {
                            group: g,
                            via: AttributionSignal::Lineage,
                            confidence: Confidence::Medium,
                        };
                    }
                    return self.orphan_root(i, Some(t), AttributionSignal::Lineage, agent);
                }
                if agent {
                    return self.orphan_root(i, None, AttributionSignal::Lineage, true);
                }
            }
        }

        // 8. marker of a session whose root has exited
        if let Some((key, _)) = marker {
            return self.orphan_root(i, Some(key), AttributionSignal::Marker, true);
        }

        // 9. systemd unit / desktop app scope
        if let Some(scope) = p.cgroup.as_deref().and_then(parse_cgroup) {
            let hit = match scope {
                CgroupScope::Service { unit, system } => {
                    let kind = if system || p.uid == Some(0) {
                        GroupKind::System
                    } else {
                        GroupKind::Other
                    };
                    Some((format!("{}:{}", kind.alias(), slug(&unit)), kind, unit))
                }
                CgroupScope::App { app } => Some((format!("app:{}", slug(&app)), GroupKind::App, app)),
                _ => None,
            };
            if let Some((key, kind, label)) = hit {
                let g = self.ensure_group(&key, kind, &label, Some(p.id));
                self.groups[g]
                    .matched_by
                    .get_or_insert_with(|| "cgroup:unit".into());
                self.set_root_meta(g);
                return Slot {
                    group: g,
                    via: AttributionSignal::Cgroup,
                    confidence: Confidence::Medium,
                };
            }
        }

        // 10. heuristic top-level root
        self.heuristic_root(i, None)
    }

    fn rule_root(&mut self, i: usize, r: usize, depth: usize) -> Slot {
        let p = self.proc(i);
        let cr = &self.rules.rules[r];
        let (kind, label, rid) = (cr.rule.kind, cr.rule.label.clone(), cr.id.clone());
        let sid = self.root_sid[i].clone();
        let key = match (&sid, kind) {
            (Some(sid), _) => session_group_key(kind, sid),
            // apps and system components are one group per label (all Firefox processes are "Firefox")
            (None, GroupKind::App) => format!("app:{}{}", slug(&label), self.user_suffix(p)),
            (None, GroupKind::System) => format!("{}:{}", kind.alias(), slug(&label)),
            (None, _) => format!("{}:{}:{}", kind.alias(), rid, p.id.pid),
        };
        let via = if self.adapter[i].is_some() {
            AttributionSignal::Adapter
        } else {
            AttributionSignal::Rule
        };
        let pi = self.parent_index(i);
        let pg = pi.and_then(|pi| self.resolve(pi, depth + 1));
        if let Some(pg) = pg {
            let g = &self.groups[pg];
            if g.matched_by.as_deref() == Some(rid.as_str())
                && g.kind == kind
                && (sid.is_none() || g.id == key)
            {
                return Slot {
                    group: pg,
                    via: AttributionSignal::Ancestry,
                    confidence: Confidence::High,
                };
            }
        }
        let g = self.ensure_group(&key, kind, &label, Some(p.id));
        {
            let grp = &mut self.groups[g];
            grp.kind = kind;
            grp.label = label;
            grp.matched_by = Some(rid);
            if grp.root.is_none() {
                grp.root = Some(p.id);
            }
        }
        self.meta[g].sticky = true;
        self.set_root_meta(g);
        if self.groups[g].owner_group.is_none() {
            // the group it was launched from (a terminal, an IDE, another session) — never a system component
            let parent_owner = match pg {
                Some(pg) if pg != g && self.groups[pg].kind != GroupKind::System => {
                    Some(self.groups[pg].id.clone())
                }
                _ => None,
            };
            let owner = parent_owner
                .or_else(|| self.marker_session(i).map(|(k, _)| k).filter(|k| *k != key))
                .or_else(|| {
                    self.lineage
                        .get(&p.id)
                        .map(|le| le.group_id.clone())
                        .filter(|id| !id.is_empty() && *id != key)
                });
            self.groups[g].owner_group = owner;
        }
        Slot {
            group: g,
            via,
            confidence: Confidence::High,
        }
    }

    fn responsible_group(&mut self, i: usize, depth: usize) -> Option<usize> {
        let p = self.proc(i);
        let rp = p.responsible_pid.filter(|&rp| rp != p.id.pid && rp > 1)?;
        let ri = *self.by_pid.get(&rp)?;
        let resp = self.proc(ri);
        if ri == i || is_terminal(resp) || is_shell(resp) || is_subreaper(resp) {
            return None;
        }
        // Launched from a shell or terminal before reaching the responsible app? Then ancestry decides.
        let mut cur = self.parent_index(i);
        let mut hops = 0;
        while let Some(ci) = cur {
            if ci == ri {
                break;
            }
            if is_boundary(self.proc(ci)) || hops > 64 {
                return None;
            }
            hops += 1;
            cur = self.parent_index(ci);
        }
        let g = self.resolve(ri, depth + 1)?;
        (!self.meta[g].launcher_root).then_some(g)
    }

    /// A top-level root for process `i` (apps merge by bundle name, system daemons by name).
    fn heuristic_root(&mut self, i: usize, owner: Option<String>) -> Slot {
        let p = self.proc(i);
        let (kind, base) = self.rules.heuristic_kind_label(p);
        let suffix = self.user_suffix(p);
        let label = if kind == GroupKind::App && !suffix.is_empty() {
            // Another user's copy of an app: say so (uid only — user names are not exported).
            format!("{base} (uid {})", p.uid.unwrap_or_default())
        } else {
            base.clone()
        };
        let key = match kind {
            GroupKind::App => format!("app:{}{suffix}", slug(&base)),
            GroupKind::System => format!("system:{}", slug(&label)),
            _ => format!("{}:{}:{}", kind.alias(), slug(&label), p.id.pid),
        };
        let g = self.ensure_group(&key, kind, &label, Some(p.id));
        // Prefer the app's main executable as the root (actions quit the app, never a helper).
        if kind == GroupKind::App && is_app_main(p) {
            let cur = self.groups[g].root.and_then(|r| self.proc_by_id(r));
            if cur.map(|c| !is_app_main(c)).unwrap_or(true) {
                self.groups[g].root = Some(p.id);
            }
        }
        if self.groups[g].owner_group.is_none() {
            self.groups[g].owner_group = owner;
        }
        self.set_root_meta(g);
        Slot {
            group: g,
            via: AttributionSignal::Heuristic,
            confidence: Confidence::Low,
        }
    }

    /// Orphan roots are their own groups (never merged into an app by name) owned by the exited group.
    /// `agent_spawned` sets the preliminary `orphan` flag (the age rule is applied by [`crate::idle`]).
    fn orphan_root(
        &mut self,
        i: usize,
        owner: Option<String>,
        via: AttributionSignal,
        agent_spawned: bool,
    ) -> Slot {
        let p = self.proc(i);
        let (kind, label, key) = orphan_identity(p);
        let g = self.ensure_group(&key, kind, &label, Some(p.id));
        self.groups[g].owner_group = owner.filter(|o| *o != key);
        self.groups[g].orphan = agent_spawned;
        self.meta[g].sticky = true;
        self.set_root_meta(g);
        Slot {
            group: g,
            via,
            confidence: Confidence::Medium,
        }
    }
}

/// Kind, label and group key of a process that is (or would be) an orphan root: its own group, keyed by pid
/// (orphans are never merged into an app group by name).
fn orphan_identity(p: &Process) -> (GroupKind, String, String) {
    let (kind, label) = heuristic_kind_label(p);
    let kind = if kind == GroupKind::System {
        GroupKind::Other
    } else {
        kind
    };
    let key = format!("{}:{}:{}", kind.alias(), slug(&label), p.id.pid);
    (kind, label, key)
}

/// True if a process was re-parented since the journal recorded it: its recorded parent (> 1) is gone
/// (the ppid changed to 1 / a subreaper, or the recorded pid now belongs to a younger process).
fn reparented(p: &Process, le: &LineageEntry, s: &Snapshot, by_pid: &HashMap<u32, usize>) -> bool {
    let Some(orig) = le.ppid.filter(|&pp| pp > 1) else {
        return false;
    };
    let parent = |pid: u32| by_pid.get(&pid).map(|&i| &s.processes[i]);
    let orig_alive = parent(orig)
        .map(|pp| pp.id.start_time <= p.id.start_time)
        .unwrap_or(false);
    if p.ppid == Some(orig) {
        return !orig_alive;
    }
    match p.ppid {
        None | Some(0) | Some(1) => true,
        Some(now) => parent(now).map(is_subreaper).unwrap_or(true),
    }
}

/// Session ids for session-keyed rule roots: from the earliest marker-carrying descendant whose nearest
/// rule-root ancestor is this root, else the root's own marker when no ancestor runs the same rule (a
/// nested agent inherits its parent's marker, which is not its own session).
fn session_ids(
    s: &Snapshot,
    rules: &CompiledRules,
    root_rule: &[Option<usize>],
    by_pid: &HashMap<u32, usize>,
) -> Vec<Option<String>> {
    let n = s.processes.len();
    let mut best: Vec<Option<(u64, u32, String)>> = vec![None; n];
    let parent_of = |i: usize| -> Option<usize> {
        let p = &s.processes[i];
        let pp = p.ppid.filter(|&pp| pp > 1 && pp != p.id.pid)?;
        let pi = *by_pid.get(&pp)?;
        (pi != i && !is_subreaper(&s.processes[pi])).then_some(pi)
    };
    for (ci, c) in s.processes.iter().enumerate() {
        let Some(sid) = &c.markers.session_id else {
            continue;
        };
        let mut cur = parent_of(ci);
        let mut hops = 0;
        while let Some(k) = cur {
            if hops > 64 {
                break;
            }
            if let Some(r) = root_rule[k] {
                if let Some(key) = rules.rules[r].rule.session_env_key() {
                    if c.markers.keys.iter().any(|m| m == key) {
                        let cand = (c.id.start_time, c.id.pid, sid.clone());
                        if best[k]
                            .as_ref()
                            .map(|b| (cand.0, cand.1) < (b.0, b.1))
                            .unwrap_or(true)
                        {
                            best[k] = Some(cand);
                        }
                    }
                }
                break;
            }
            hops += 1;
            cur = parent_of(k);
        }
    }
    (0..n)
        .map(|i| {
            let r = root_rule[i]?;
            let rule = &rules.rules[r].rule;
            let key = rule.session_env_key()?;
            if let Some((_, _, sid)) = &best[i] {
                return Some(sid.clone());
            }
            let p = &s.processes[i];
            let own = p
                .markers
                .session_id
                .clone()
                .filter(|_| p.markers.keys.iter().any(|k| k == key))?;
            // nested under the same rule → the marker is the parent session's
            let mut cur = parent_of(i);
            let mut hops = 0;
            while let Some(k) = cur {
                if hops > 64 {
                    break;
                }
                if root_rule[k] == Some(r) {
                    return None;
                }
                hops += 1;
                cur = parent_of(k);
            }
            Some(own)
        })
        .collect()
}

fn adapter_hints(s: &Snapshot) -> HashMap<ProcId, AdapterHint> {
    let mut out = HashMap::new();
    for ms in &s.model_servers {
        let label = model_server_label(ms.kind);
        let id = if ms.id.is_empty() {
            slug(label)
        } else {
            slug(&ms.id)
        };
        for pid in &ms.pids {
            out.entry(*pid).or_insert_with(|| AdapterHint {
                key: format!("model:{id}"),
                kind: GroupKind::ModelServer,
                label: label.to_string(),
                matched_by: format!("adapter:{}", ms.id),
            });
        }
    }
    for sb in &s.sandboxes {
        // App-owned VMs (Claude desktop, Docker Desktop) belong to their app via the responsible pid.
        if sb.kind == SandboxKind::Vm && sb.runtime.to_ascii_lowercase().contains("virtualization") {
            continue;
        }
        let label = if sb.label.is_empty() {
            sb.runtime.clone()
        } else {
            sb.label.clone()
        };
        for pid in &sb.host_pids {
            out.entry(*pid).or_insert_with(|| AdapterHint {
                key: format!("sandbox:{}", slug(&sb.id)),
                kind: GroupKind::Sandbox,
                label: label.clone(),
                matched_by: format!("adapter:{}", sb.runtime),
            });
        }
    }
    out
}

/// oomtop's own pid plus all its descendants.
fn self_tree(s: &Snapshot) -> HashSet<ProcId> {
    let mut out = HashSet::new();
    let Some(me) = s.self_pid.and_then(|pid| s.process_by_pid(pid)) else {
        return out;
    };
    let mut children: HashMap<u32, Vec<&Process>> = HashMap::new();
    for p in &s.processes {
        if let Some(pp) = p.ppid {
            children.entry(pp).or_default().push(p);
        }
    }
    let mut stack = vec![me];
    while let Some(p) = stack.pop() {
        if !out.insert(p.id) || out.len() > 4096 {
            continue;
        }
        if let Some(kids) = children.get(&p.id.pid) {
            stack.extend(
                kids.iter()
                    .filter(|k| k.id.start_time >= p.id.start_time)
                    .copied(),
            );
        }
    }
    out
}

/// Groups all processes of the snapshot. Totals, reclaim gain, fingerprint, protection, `lower_bound` and
/// `owner_group` are filled; `orphan` is a preliminary flag (agent-spawned root whose spawning group is gone)
/// that [`crate::idle::IdleTracker::apply`] re-decides with the age rule, together with `idle` / `idle_for_s`.
pub fn attribute(snapshot: &Snapshot, ctx: &AttributionContext<'_>) -> Vec<Group> {
    let n = snapshot.processes.len();
    // pid → index of the newest process with that pid (pid reuse inside one listing keeps both entries).
    let mut by_pid: HashMap<u32, usize> = HashMap::with_capacity(n);
    for (i, p) in snapshot.processes.iter().enumerate() {
        match by_pid.get(&p.id.pid) {
            Some(&j) if snapshot.processes[j].id.start_time >= p.id.start_time => {}
            _ => {
                by_pid.insert(p.id.pid, i);
            }
        }
    }
    let by_id: HashMap<ProcId, usize> = snapshot
        .processes
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id, i))
        .collect();
    let root_rule: Vec<Option<usize>> = snapshot
        .processes
        .iter()
        .map(|p| ctx.rules.match_root(p).map(|m| m.index))
        .collect();
    let root_sid = session_ids(snapshot, ctx.rules, &root_rule, &by_pid);
    let live_sessions: HashSet<String> = root_rule
        .iter()
        .zip(&root_sid)
        .filter_map(|(r, sid)| Some(session_group_key(ctx.rules.rules[(*r)?].rule.kind, sid.as_ref()?)))
        .collect();
    let hints = adapter_hints(snapshot);
    let adapter = snapshot
        .processes
        .iter()
        .map(|p| hints.get(&p.id).cloned())
        .collect();
    let mut b = Builder {
        snap: snapshot,
        rules: ctx.rules,
        lineage: ctx.lineage,
        by_pid,
        by_id: by_id.clone(),
        groups: Vec::new(),
        meta: Vec::new(),
        key_to_group: HashMap::new(),
        assigned: vec![None; n],
        visiting: vec![false; n],
        root_rule,
        root_sid,
        adapter,
        live_sessions,
        self_uid: ctx.protect.self_uid,
    };
    // Resolve in (start_time, pid) order so parents and group roots usually resolve first.
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| (snapshot.processes[i].id.start_time, snapshot.processes[i].id.pid));
    for &i in &order {
        if b.resolve(i, 0).is_none() {
            // Unreachable in practice (resolve only fails inside a cycle); never leave a process out.
            let slot = b.heuristic_root(i, None);
            b.assigned[i] = Some(slot);
        }
    }
    let Builder {
        mut groups, assigned, ..
    } = b;
    for &i in &order {
        if let Some(s) = assigned[i] {
            groups[s.group].members.push(Member {
                id: snapshot.processes[i].id,
                confidence: s.confidence,
                via: s.via,
            });
        }
    }
    groups.retain(|g| !g.members.is_empty());
    let self_ids = self_tree(snapshot);
    for g in &mut groups {
        if g.root
            .map(|r| !g.members.iter().any(|m| m.id == r))
            .unwrap_or(true)
        {
            g.root = g.members.first().map(|m| m.id);
        }
        finish_group(g, snapshot, ctx, &self_ids, &by_id);
    }
    link_by_project(&mut groups, snapshot, &by_id);
    groups.sort_by(|a, b| {
        b.totals
            .footprint
            .value
            .unwrap_or(0)
            .cmp(&a.totals.footprint.value.unwrap_or(0))
            .then_with(|| a.id.cmp(&b.id))
    });
    groups
}

/// The per-sample numbers of a group — totals, swap gain, reclaim gain — from its member processes.
fn group_numbers<'a>(
    g: &mut Group,
    members: &[&Process],
    self_ids: &HashSet<ProcId>,
    reclaim_model: &crate::headroom::ReclaimModel,
    lookup: impl Fn(ProcId) -> Option<&'a Process> + Copy,
) {
    let mut footprint = sum_bytes(
        members.iter().map(|p| &p.mem.footprint_or_pss),
        "Σ footprint_or_pss",
    );
    if g.lower_bound && footprint.value.is_some() {
        footprint = footprint.into_estimate();
        footprint.source = "Σ footprint_or_pss (lower bound: VM memory under-counted)".into();
    }
    g.totals = GroupTotals {
        footprint,
        resident: sum_bytes(members.iter().map(|p| &p.mem.resident), "Σ resident"),
        gpu: sum_bytes(members.iter().map(|p| &p.mem.gpu), "Σ gpu"),
        swapped: sum_bytes(members.iter().map(|p| &p.mem.swapped), "Σ swapped"),
        cpu_pct: sum_f64(members.iter().map(|p| &p.cpu_pct), "Σ cpu"),
        process_count: members.len() as u32,
    };
    g.swap_gain = g.totals.swapped.clone().into_estimate();
    // An `oomtop mcp` child inside an agent session is not part of what stopping that session frees.
    if !g.is_self && g.members.iter().any(|m| self_ids.contains(&m.id)) {
        let mut others = g.clone();
        others.members.retain(|m| !self_ids.contains(&m.id));
        g.reclaim_gain = crate::headroom::group_reclaim_gain_by(&others, reclaim_model, lookup);
    } else {
        g.reclaim_gain = crate::headroom::group_reclaim_gain_by(g, reclaim_model, lookup);
    }
}

/// Signature of every input group **membership** depends on: each process's identity fields, parent,
/// responsible pid, uid and markers, the adapter truth (model servers, sandboxes), the lineage entries of
/// the listed processes (without their timestamps) and the protection context. Two samples with the same
/// signature attribute to the same groups; only the per-sample numbers differ (see [`refresh_numbers`]).
/// On a quiet machine the process set is unchanged between most refreshes, so a frontend can skip the
/// whole attribution (SPEC §14 CPU budget).
pub fn membership_signature(snapshot: &Snapshot, ctx: &AttributionContext<'_>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    snapshot.host.os.hash(&mut h);
    snapshot.self_pid.hash(&mut h);
    snapshot.processes.len().hash(&mut h);
    for p in &snapshot.processes {
        p.id.hash(&mut h);
        p.ppid.hash(&mut h);
        p.responsible_pid.hash(&mut h);
        p.uid.hash(&mut h);
        identity_sig(p).hash(&mut h);
        p.markers.session_id.hash(&mut h);
        p.markers.agent.hash(&mut h);
        p.markers.keys.hash(&mut h);
        if let Some(le) = ctx.lineage.get(&p.id) {
            le.ppid.hash(&mut h);
            le.group_id.hash(&mut h);
            le.group_kind.hash(&mut h);
            le.group_label.hash(&mut h);
            le.group_fingerprint.hash(&mut h);
            le.session_id.hash(&mut h);
            le.spawned_by_agent.hash(&mut h);
        } else {
            0u8.hash(&mut h);
        }
    }
    for m in &snapshot.model_servers {
        m.id.hash(&mut h);
        m.kind.hash(&mut h);
        m.pids.hash(&mut h);
    }
    for b in &snapshot.sandboxes {
        b.id.hash(&mut h);
        b.kind.hash(&mut h);
        b.runtime.hash(&mut h);
        b.label.hash(&mut h);
        b.host_pids.hash(&mut h);
        b.footprint_lower_bound.hash(&mut h);
        b.configured_mem.value.hash(&mut h);
    }
    let pc = ctx.protect;
    pc.self_pid.hash(&mut h);
    pc.self_uid.hash(&mut h);
    pc.ancestor_pids.hash(&mut h);
    pc.protected_names.hash(&mut h);
    pc.caller_group.hash(&mut h);
    h.finish()
}

/// Recomputes the per-sample numbers (totals, swap gain, reclaim gain) of `groups` — the result of an
/// earlier [`attribute`] with the same [`membership_signature`] — from `snapshot`, and re-sorts them by
/// footprint like [`attribute`] does. Membership, ids, labels, fingerprints and flags are kept.
pub fn refresh_numbers(groups: &mut [Group], snapshot: &Snapshot) {
    let by_id: HashMap<ProcId, usize> = snapshot
        .processes
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id, i))
        .collect();
    let lookup = |id: ProcId| by_id.get(&id).map(|&i| &snapshot.processes[i]);
    let self_ids = self_tree(snapshot);
    let model = crate::headroom::ReclaimModel::from_snapshot(snapshot);
    for g in groups.iter_mut() {
        let members: Vec<&Process> = g.members.iter().filter_map(|m| lookup(m.id)).collect();
        group_numbers(g, &members, &self_ids, &model, lookup);
    }
    groups.sort_by(|a, b| {
        b.totals
            .footprint
            .value
            .unwrap_or(0)
            .cmp(&a.totals.footprint.value.unwrap_or(0))
            .then_with(|| a.id.cmp(&b.id))
    });
}

fn finish_group(
    g: &mut Group,
    snapshot: &Snapshot,
    ctx: &AttributionContext<'_>,
    self_ids: &HashSet<ProcId>,
    by_id: &HashMap<ProcId, usize>,
) {
    let lookup = |id: ProcId| by_id.get(&id).map(|&i| &snapshot.processes[i]);
    // root first, then by start time (members were pushed in (start_time, pid) order)
    if let Some(r) = g.root {
        if let Some(pos) = g.members.iter().position(|m| m.id == r) {
            let m = g.members.remove(pos);
            g.members.insert(0, m);
        }
    }
    let members: Vec<&Process> = g.members.iter().filter_map(|m| lookup(m.id)).collect();
    let root = g.root.and_then(lookup);
    let reclaim_model = crate::headroom::ReclaimModel::from_snapshot(snapshot);

    // lower bound: Virtualization.framework guests, or an adapter-reported under-count
    let sandbox = snapshot
        .sandboxes
        .iter()
        .find(|sb| sb.footprint_lower_bound && g.members.iter().any(|m| sb.host_pids.contains(&m.id)));
    g.lower_bound = g.lower_bound || members.iter().any(|p| is_vm_lower_bound(p)) || sandbox.is_some();
    if g.configured_mem.is_none() {
        g.configured_mem = sandbox.and_then(|sb| sb.configured_mem.value);
    }

    g.is_self = g.id == SELF_GROUP_ID;
    group_numbers(g, &members, self_ids, &reclaim_model, lookup);

    // Group confidence = how sure we are about the root's identity.
    let root_member = g.root.and_then(|r| g.members.iter().find(|m| m.id == r));
    g.confidence = match root_member.map(|m| (m.via, m.confidence)) {
        Some((AttributionSignal::Heuristic, _)) if g.kind == GroupKind::App => Confidence::Medium,
        Some((_, c)) => c,
        None => g.members.iter().map(|m| m.confidence).max().unwrap_or_default(),
    };
    if g.matched_by.is_some()
        && g.confidence < Confidence::High
        && g.matched_by.as_deref() != Some("cgroup:unit")
    {
        g.confidence = Confidence::High;
    }

    if let Some(r) = root {
        if g.fingerprint.is_empty() {
            g.fingerprint = ctx.rules.group_fingerprint(g.kind, r);
        }
    }
    let rule_protected = g
        .matched_by
        .as_deref()
        .map(|id| ctx.rules.is_protected_rule(id))
        .unwrap_or(false);
    g.protected = g.protected
        || rule_protected
        || g.is_self
        || g.kind == GroupKind::System
        || ctx.protect.caller_group.as_deref() == Some(g.id.as_str())
        || root
            .map(|r| is_protected_process(r, snapshot, ctx.protect))
            .unwrap_or(false);
}

/// Rules & heuristics, lowest priority (SPEC §7): a user-launched root whose cwd is inside exactly one live
/// agent session's project is linked to it via `owner_group` (membership is not changed; an owner that is
/// merely the terminal it was typed into is replaced).
fn link_by_project(groups: &mut [Group], s: &Snapshot, by_id: &HashMap<ProcId, usize>) {
    let lookup = |id: ProcId| by_id.get(&id).map(|&i| &s.processes[i]);
    let sessions: Vec<(String, String)> = groups
        .iter()
        .filter(|g| g.kind == GroupKind::AgentSession)
        .filter_map(|g| {
            let root = lookup(g.root?)?;
            let cwd = fingerprint::project_root(root)?;
            Some((cwd.to_string(), g.id.clone()))
        })
        .collect();
    if sessions.is_empty() {
        return;
    }
    // groups rooted in a terminal / multiplexer: "launched from iTerm" is less informative than the project
    let launchers: HashSet<String> = groups
        .iter()
        .filter(|g| g.root.and_then(lookup).map(is_terminal).unwrap_or(false))
        .map(|g| g.id.clone())
        .collect();
    for g in groups.iter_mut() {
        let replaceable = g
            .owner_group
            .as_ref()
            .map(|o| launchers.contains(o))
            .unwrap_or(true);
        if g.kind != GroupKind::Other || !replaceable || g.matched_by.is_some() || g.orphan {
            continue;
        }
        let Some(cwd) = g.root.and_then(lookup).and_then(fingerprint::project_root) else {
            continue;
        };
        let mut hits: Vec<&(String, String)> = sessions
            .iter()
            .filter(|(root, _)| {
                cwd == root
                    || cwd
                        .strip_prefix(root.as_str())
                        .map(|r| r.starts_with('/'))
                        .unwrap_or(false)
            })
            .collect();
        hits.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));
        match hits.as_slice() {
            [only] => g.owner_group = Some(only.1.clone()),
            [a, b, ..] if a.0.len() > b.0.len() => g.owner_group = Some(a.1.clone()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Markers, Measured};

    pub(crate) fn proc(pid: u32, ppid: u32, name: &str, exe: &str, fp: u64) -> Process {
        Process {
            id: ProcId::new(pid, 1000 + pid as u64),
            ppid: Some(ppid),
            name: name.into(),
            exe: exe.into(),
            cmdline: vec![exe.into()],
            uid: Some(501),
            mem: crate::model::MemBreakdown {
                footprint_or_pss: Measured::exact(fp, "t"),
                resident: Measured::exact(fp / 2, "t"),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn rules() -> CompiledRules {
        let set: RuleSet = RuleSet {
            rules: vec![
                Rule {
                    kind: GroupKind::AgentSession,
                    label: "Claude Code".into(),
                    matcher: MatchSpec {
                        exe: vec!["claude".into()],
                        env: vec!["CLAUDECODE".into()],
                        ..Default::default()
                    },
                    session_key: Some("env:CLAUDE_CODE_SESSION_ID".into()),
                    ..Default::default()
                },
                Rule {
                    kind: GroupKind::BuildDaemon,
                    label: "GradleDaemon".into(),
                    matcher: MatchSpec {
                        script: vec!["org.gradle.launcher.daemon.*".into()],
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ],
        };
        CompiledRules::compile(&set).unwrap()
    }

    fn run(s: &Snapshot, lineage: &BTreeMap<ProcId, LineageEntry>) -> Vec<Group> {
        let r = rules();
        let protect = ProtectContext::default();
        attribute(
            s,
            &AttributionContext {
                rules: &r,
                lineage,
                protect: &protect,
            },
        )
    }

    fn sess(sid: &str) -> Markers {
        Markers {
            session_id: Some(sid.into()),
            agent: Some("claude-code".into()),
            keys: vec!["CLAUDECODE".into(), "CLAUDE_CODE_SESSION_ID".into()],
        }
    }

    #[test]
    fn sessions_ancestry_and_markers() {
        let mut s = Snapshot::default();
        // The agent CLI itself carries no marker; only its children do.
        let claude = proc(100, 50, "claude", "/opt/bin/claude", 400);
        let desktop = proc(
            300,
            1,
            "Claude",
            "/Applications/Claude.app/Contents/MacOS/Claude",
            50,
        );
        let zsh = proc(50, 1, "zsh", "/bin/zsh", 10);
        let mut child = proc(101, 100, "node", "/usr/bin/node", 100);
        child.markers = sess("abcdef0123456789");
        // re-parented to launchd but still carries the marker
        let mut orphan = proc(102, 1, "chrome", "/x/chrome", 1000);
        orphan.markers = sess("abcdef0123456789");
        let mut gradle = proc(200, 1, "java", "/usr/bin/java", 3000);
        gradle.cmdline = vec![
            "java".into(),
            "org.gradle.launcher.daemon.bootstrap.GradleDaemon".into(),
        ];
        gradle.markers = sess("abcdef0123456789");
        s.processes = vec![zsh, claude, child, orphan, gradle, desktop];
        let groups = run(&s, &BTreeMap::new());
        let agent = groups.iter().find(|g| g.kind == GroupKind::AgentSession).unwrap();
        assert_eq!(agent.members.len(), 3, "{groups:#?}");
        assert_eq!(agent.totals.footprint.value, Some(1500));
        assert_eq!(agent.id, "agent:abcdef012345");
        assert_eq!(agent.root.map(|r| r.pid), Some(100));
        let daemon = groups.iter().find(|g| g.kind == GroupKind::BuildDaemon).unwrap();
        assert_eq!(daemon.members.len(), 1);
        assert_eq!(daemon.owner_group.as_deref(), Some("agent:abcdef012345"));
        assert!(!daemon.fingerprint.is_empty());
        let app = groups.iter().find(|g| g.label == "Claude").unwrap();
        assert_eq!(
            app.kind,
            GroupKind::App,
            "desktop app is not Claude Code (case-sensitive exe)"
        );
        let zsh_group = groups.iter().find(|g| g.label == "zsh").unwrap();
        assert_eq!(zsh_group.members.len(), 1);
        // largest first
        assert_eq!(groups[0].kind, GroupKind::BuildDaemon);
    }

    #[test]
    fn lineage_rejoins_live_group_and_orphans_dead_one() {
        let mut s = Snapshot::default();
        let orphan = proc(500, 1, "chrome", "/x/chrome", 100);
        let stale = proc(
            600,
            1,
            "Claude",
            "/Applications/Claude.app/Contents/MacOS/Claude",
            100,
        );
        s.processes = vec![orphan.clone(), stale.clone()];
        let mut lineage = BTreeMap::new();
        let le = |id: ProcId, ppid: u32| LineageEntry {
            id,
            ppid: Some(ppid),
            group_id: "agent:abc".into(),
            group_kind: GroupKind::AgentSession,
            group_label: "Claude Code".into(),
            spawned_by_agent: true,
            ..Default::default()
        };
        lineage.insert(orphan.id, le(orphan.id, 77));
        lineage.insert(stale.id, le(stale.id, 1));
        let groups = run(&s, &lineage);
        // the session is gone → the re-parented process is its own (orphan) root owned by the session
        let g = groups.iter().find(|g| g.root == Some(orphan.id)).unwrap();
        assert_eq!(g.members[0].via, AttributionSignal::Lineage);
        assert_eq!(g.owner_group.as_deref(), Some("agent:abc"));
        assert_eq!(g.members.len(), 1);
        // ppid was already 1 when recorded → not re-parented → heuristic app
        assert!(groups
            .iter()
            .any(|g| g.label == "Claude" && g.kind == GroupKind::App));
    }

    #[test]
    fn orphan_identity_is_stable_across_samples() {
        // sample 1: session gone, marker carrier → orphan root owned by the dead session
        let mut s = Snapshot::default();
        let mut o = proc(700, 1, "node", "/usr/bin/node", 100);
        o.markers = sess("feedfacefeedface");
        s.processes = vec![o.clone()];
        let g1 = run(&s, &BTreeMap::new());
        assert_eq!(g1[0].owner_group.as_deref(), Some("agent:feedfacefeed"));
        assert!(g1[0].orphan);
        // sample 2: the engine journaled the orphan's *own* group id (+ original ppid and session id)
        let mut lineage = BTreeMap::new();
        lineage.insert(
            o.id,
            LineageEntry {
                id: o.id,
                ppid: Some(650),
                group_id: g1[0].id.clone(),
                group_kind: g1[0].kind,
                session_id: Some("feedfacefeedface".into()),
                spawned_by_agent: true,
                ..Default::default()
            },
        );
        let g2 = run(&s, &lineage);
        assert_eq!(g2[0].id, g1[0].id);
        assert_eq!(
            g2[0].owner_group.as_deref(),
            Some("agent:feedfacefeed"),
            "never its own owner"
        );
        assert!(g2[0].orphan);
        // …and without the marker the journal alone still knows the session
        s.processes[0].markers = Markers::default();
        let g3 = run(&s, &lineage);
        assert_eq!(g3[0].id, g1[0].id);
        assert_eq!(g3[0].owner_group.as_deref(), Some("agent:feedfacefeed"));
        assert_eq!(g3[0].members[0].via, AttributionSignal::Lineage);
    }

    #[test]
    fn workers_of_a_rule_root_stay_with_it_despite_markers() {
        // `./gradlew build` in a session → daemon (ppid 1) → worker; both inherit the session env
        let mut s = Snapshot::default();
        let claude = proc(100, 50, "claude", "/opt/bin/claude", 400);
        let mut mcp = proc(101, 100, "node", "/usr/bin/node", 50);
        mcp.markers = sess("abcdef0123456789");
        let mut daemon = proc(200, 1, "java", "/usr/bin/java", 3000);
        daemon.cmdline = vec![
            "java".into(),
            "org.gradle.launcher.daemon.bootstrap.GradleDaemon".into(),
        ];
        daemon.markers = sess("abcdef0123456789");
        let mut worker = proc(201, 200, "java", "/usr/bin/java", 500);
        worker.markers = sess("abcdef0123456789");
        s.processes = vec![claude, mcp, daemon, worker];
        let groups = run(&s, &BTreeMap::new());
        let d = groups.iter().find(|g| g.kind == GroupKind::BuildDaemon).unwrap();
        let pids: Vec<u32> = d.members.iter().map(|m| m.id.pid).collect();
        assert_eq!(pids, vec![200, 201], "{groups:#?}");
        assert_eq!(d.members[1].via, AttributionSignal::Ancestry);
        assert_eq!(d.owner_group.as_deref(), Some("agent:abcdef012345"));
        let a = groups.iter().find(|g| g.kind == GroupKind::AgentSession).unwrap();
        assert_eq!(a.members.len(), 2);
    }

    #[test]
    fn raw_or_non_ascii_session_ids_never_panic_or_leak() {
        let mut s = Snapshot::default();
        let claude = proc(100, 50, "claude", "/opt/bin/claude", 400);
        let mut child = proc(101, 100, "node", "/usr/bin/node", 50);
        // byte 12 falls inside 'ä' (a byte-index slice would panic)
        child.markers = sess("aääääääääää-raw");
        s.processes = vec![claude, child];
        let groups = run(&s, &BTreeMap::new());
        let a = groups.iter().find(|g| g.kind == GroupKind::AgentSession).unwrap();
        assert!(a.id.is_ascii() && !a.id.contains("raw"), "{}", a.id);
        assert_eq!(a.id.len(), "agent:".len() + 12);
        assert_eq!(a.members.len(), 2);
        // a hashed id keeps its prefix (ids stay stable with the existing journal)
        assert_eq!(
            session_group_key(GroupKind::AgentSession, "abcdef0123456789"),
            "agent:abcdef012345"
        );
    }

    #[test]
    fn lineage_never_rejoins_a_pid_reused_group() {
        // journal: pid 900 (started at 1900) was spawned by group "other:start-sh:800"; pid 800 now belongs
        // to a *younger* process with the same label → not the group that spawned it
        let mut s = Snapshot::default();
        let mut reused = proc(800, 1, "start.sh", "/bin/start.sh", 10);
        reused.id.start_time = 5000;
        let child = proc(900, 1, "node", "/usr/bin/node", 100);
        // an older XPC-style process that names the reused pid as responsible makes it resolve first
        let mut early = proc(850, 1, "helper", "/opt/helper", 1);
        early.responsible_pid = Some(800);
        s.processes = vec![reused.clone(), child.clone(), early];
        let groups = run(&s, &BTreeMap::new());
        let key = groups
            .iter()
            .find(|g| g.root == Some(reused.id))
            .unwrap()
            .id
            .clone();
        let mut lineage = BTreeMap::new();
        lineage.insert(
            child.id,
            LineageEntry {
                id: child.id,
                ppid: Some(800),
                group_id: key.clone(),
                group_kind: GroupKind::Other,
                group_label: "start.sh".into(),
                ..Default::default()
            },
        );
        let groups = run(&s, &lineage);
        let g = groups
            .iter()
            .find(|g| g.members.iter().any(|m| m.id == child.id))
            .unwrap();
        assert_ne!(g.id, key, "{groups:#?}");
        assert_eq!(g.root, Some(child.id));
        assert_eq!(g.owner_group.as_deref(), Some(key.as_str()));
    }

    #[test]
    fn libexec_terminal_is_not_a_system_component() {
        let t = proc(
            600,
            400,
            "gnome-terminal-",
            "/usr/libexec/gnome-terminal-server",
            60,
        );
        assert_eq!(heuristic_kind_label(&t).0, GroupKind::Other);
        let d = proc(601, 1, "polkitd", "/usr/libexec/polkitd", 6);
        assert_eq!(heuristic_kind_label(&d).0, GroupKind::System);
    }

    #[test]
    fn bad_rules_are_errors() {
        let set = RuleSet {
            rules: vec![Rule {
                label: "x".into(),
                matcher: MatchSpec {
                    cmdline: vec!["(".into()],
                    ..Default::default()
                },
                ..Default::default()
            }],
        };
        assert!(matches!(
            CompiledRules::compile(&set),
            Err(RuleError::Regex { .. })
        ));
        let set = RuleSet {
            rules: vec![Rule::default()],
        };
        assert!(matches!(
            CompiledRules::compile(&set),
            Err(RuleError::EmptyLabel { .. })
        ));
        let set = RuleSet {
            rules: vec![Rule {
                label: "x".into(),
                session_key: Some("CLAUDE_CODE_SESSION_ID".into()),
                ..Default::default()
            }],
        };
        assert!(matches!(
            CompiledRules::compile(&set),
            Err(RuleError::SessionKey { .. })
        ));
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Claude Code"), "claude-code");
        assert_eq!(slug("  sd-server · qwen "), "sd-server-qwen");
    }

    #[test]
    fn cgroups() {
        let c = |p: &str| parse_cgroup(p);
        assert_eq!(
            c("0::/system.slice/docker-3f2a9c01e2b4aabbccddeeff00112233445566778899aabbccddeeff00112233.scope"),
            Some(CgroupScope::Container {
                runtime: "docker".into(),
                id: "3f2a9c01e2b4".into()
            })
        );
        assert_eq!(
            c("/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-0123456789abcdef0123.scope/container"),
            Some(CgroupScope::Container {
                runtime: "podman".into(),
                id: "0123456789ab".into()
            })
        );
        assert_eq!(
            c("/kubepods/burstable/pod1234-5678/0123456789abcdef0123456789abcdef"),
            Some(CgroupScope::Container {
                runtime: "kubernetes".into(),
                id: "0123456789ab".into()
            })
        );
        assert_eq!(
            c("/machine.slice/machine-qemu\\x2d1\\x2dubuntu.scope/libvirt/emulator"),
            Some(CgroupScope::Vm {
                name: "ubuntu".into()
            })
        );
        assert_eq!(
            c("/system.slice/ollama.service"),
            Some(CgroupScope::Service {
                unit: "ollama".into(),
                system: true
            })
        );
        assert_eq!(
            c("/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-firefox-4242.scope"),
            Some(CgroupScope::App {
                app: "firefox".into()
            })
        );
        assert_eq!(
            c("/user.slice/user-1000.slice/user@1000.service/app.slice/app-flatpak-org.mozilla.firefox-4242.scope"),
            Some(CgroupScope::App { app: "firefox".into() })
        );
        assert_eq!(c("/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.gnome.Terminal.slice/vte-spawn-1a2b.scope"), None);
        assert_eq!(
            c("/user.slice/user-1000.slice/user@1000.service/app.slice/app-kitty-4242.scope"),
            None
        );
        assert_eq!(c("/user.slice/user-1000.slice/session-2.scope"), None);
        assert_eq!(
            c("/user.slice/user-1000.slice/user@1000.service/init.scope"),
            None
        );
    }

    #[test]
    fn indexed_matching_equals_rule_by_rule() {
        let set: RuleSet = serde_json::from_str(
            r#"{ "group": [
              { "id": "a", "kind": "agent_session", "label": "A", "match": { "exe": ["claude", "**/versions/*"], "name": ["claude"], "script": ["**/@anthropic-ai/claude-code/**"], "env": ["CLAUDECODE"] }, "session_key": "env:CLAUDE_CODE_SESSION_ID" },
              { "id": "b", "kind": "model_server", "label": "B", "match": { "cmdline": ["vllm\\s+serve", "mlx_lm[._]server"] } },
              { "id": "c", "kind": "app", "label": "C", "match": { "exe": ["firefox"] }, "exclude": { "exe": ["**/*.app/**"] } },
              { "id": "d", "kind": "other", "label": "D", "match": { "cwd": ["/srv/**"], "bundle": ["com.example.*"], "cgroup": ["/system.slice/x*.service"] } },
              { "id": "e", "kind": "build_daemon", "label": "E", "priority": 5, "match": { "script": ["*.js"] } },
              { "id": "f", "kind": "other", "label": "F only env", "match": { "env": ["CODEX_SANDBOX"] } }
            ] }"#,
        )
        .unwrap();
        let r = CompiledRules::compile(&set).unwrap();
        let mk = |exe: &str, argv: &[&str]| Process {
            exe: exe.into(),
            name: basename(exe).into(),
            cmdline: argv.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let mut cases = vec![
            mk("/opt/bin/claude", &["claude"]),
            mk("/u/.local/share/claude/versions/2.1.3", &["claude"]),
            mk("", &["claude", "-p", "x"]),
            mk(
                "/usr/bin/node",
                &["node", "/l/node_modules/@anthropic-ai/claude-code/cli.js"],
            ),
            mk("/usr/bin/python3", &["python3", "-m", "vllm", "serve", "m"]),
            mk("/usr/bin/python3", &["python", "-m", "mlx_lm.server"]),
            mk("/usr/lib/firefox/firefox", &["firefox"]),
            mk("/Applications/Firefox.app/Contents/MacOS/firefox", &["firefox"]),
            mk("/usr/bin/node", &["node", "server.js"]),
            mk("/bin/zsh", &["-zsh"]),
            mk("/x/tool", &[]),
        ];
        let mut d = mk("/x/y", &["y"]);
        d.cwd = Some("/srv/app".into());
        cases.push(d.clone());
        d.cwd = None;
        d.bundle_id = Some("com.example.tool".into());
        cases.push(d.clone());
        d.bundle_id = None;
        d.cgroup = Some("/system.slice/xyz.service".into());
        cases.push(d);
        for p in &cases {
            let a = r.match_root(p).map(|m| m.index);
            let b = r.match_root_naive(p).map(|m| m.index);
            assert_eq!(a, b, "{:?}", p.cmdline);
        }
        assert_eq!(
            r.match_root(&cases[3]).map(|m| m.index),
            Some(4),
            "priority 5 script rule wins"
        );
        assert_eq!(r.match_root(&cases[7]), None, "excluded .app");
        assert_eq!(r.match_root(&cases[11]).map(|m| m.rule.label.as_str()), Some("D"));
    }

    #[test]
    fn shells_and_terminals() {
        let mut p = proc(1, 0, "zsh", "/bin/zsh", 0);
        assert!(is_interactive_shell(&p));
        p.cmdline = vec!["-zsh".into()];
        p.name = "-zsh".into();
        assert!(is_interactive_shell(&p));
        p.cmdline = vec!["/bin/zsh".into(), "-c".into(), "ls".into()];
        assert!(!is_interactive_shell(&p));
        p.cmdline = vec!["bash".into(), "-lc".into(), "ls".into()];
        assert!(!is_interactive_shell(&p));
        p.cmdline = vec!["bash".into(), "start.sh".into()];
        assert!(!is_interactive_shell(&p));
        let t = proc(2, 1, "iTerm2", "/Applications/iTerm.app/Contents/MacOS/iTerm2", 0);
        assert!(is_terminal(&t));
        let w = proc(3, 1, "stable", "/Applications/Warp.app/Contents/MacOS/stable", 0);
        assert!(is_terminal(&w));
        let c = proc(
            4,
            1,
            "Google Chrome",
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            0,
        );
        assert!(!is_terminal(&c));
    }

    #[test]
    fn another_users_app_is_its_own_group() {
        // Fast user switching: a second logged-in user runs Chrome too (seen on the M5 Air).
        let exe = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
        let mine = proc(10, 1, "Google Chrome", exe, 100);
        let mut theirs = proc(20, 1, "Google Chrome", exe, 100);
        theirs.uid = Some(502);
        let s = Snapshot {
            processes: vec![mine, theirs],
            ..Default::default()
        };
        let r = rules();
        let protect = ProtectContext {
            self_uid: Some(501),
            ..Default::default()
        };
        let groups = attribute(
            &s,
            &AttributionContext {
                rules: &r,
                lineage: &BTreeMap::new(),
                protect: &protect,
            },
        );
        let ids: Vec<&str> = groups.iter().map(|g| g.id.as_str()).collect();
        assert!(ids.contains(&"app:google-chrome"), "{ids:?}");
        assert!(ids.contains(&"app:google-chrome@u502"), "{ids:?}");
        let mine = groups.iter().find(|g| g.id == "app:google-chrome").unwrap();
        assert!(!mine.protected, "our own Chrome stays actionable");
        // Without a known self uid (pure tests, fixtures) keys are unchanged.
        let groups = run(&s, &BTreeMap::new());
        assert_eq!(groups.len(), 1);
    }

    #[test]
    fn chrome_code_sign_clone_is_still_the_app() {
        // Chrome's updater runs the browser from a temporary `.app.bundle` copy (seen on the M5 Air).
        let mut c = proc(
            5,
            1,
            "Google Chrome",
            "/private/var/folders/xx/X/com.google.Chrome.code_sign_clone/code_sign_clone.AbCd/Google Chrome.app.bundle/Contents/MacOS/Google Chrome",
            0,
        );
        assert_eq!(app_bundle_name(&c), Some("Google Chrome"));
        assert!(is_app_main(&c));
        assert_eq!(heuristic_kind_label(&c).0, GroupKind::App);
        // Unknown exe, argv[0] still names the bundle.
        c.exe = String::new();
        c.cmdline = vec!["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into()];
        assert_eq!(app_bundle_name(&c), Some("Google Chrome"));
    }
}

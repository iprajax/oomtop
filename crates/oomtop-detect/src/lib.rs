//! # oomtop-detect
//!
//! Detection rule sets (SPEC §7, §15): the embedded built-in rules plus user rules from `rules.d/*.toml`,
//! compiled into [`oomtop_core::attribution::CompiledRules`]. File I/O lives here so core stays pure.
//!
//! On top of core attribution the [`Detector`] applies host-shape corrections that rules cannot express:
//!
//! 1. **Desktop shells are launchers, not owners.** Children of `gnome-shell`, `plasmashell`, `sway`, … are
//!    treated as top-level processes, so a Linux desktop does not collapse into one protected "desktop shell"
//!    group. (Subreapers such as `launchd` / `systemd --user` are already never owners in core.) Lineage
//!    entries recording the shell as the original parent are hidden so these processes are not mistaken for
//!    re-parented orphans.
//! 2. **The nearest owner wins.** When a group root (e.g. the `codex` agent inside ChatGPT.app) sits
//!    between a process and its responsible app, the responsible pid is ignored so the process stays in
//!    that nearer group (the agent session) instead of the app. (Terminals are already ignored by core.)
//! 3. **Orphaned Virtualization.framework VMs** (no usable responsible pid) become a `sandbox` group instead
//!    of a protected `system` group (their binary lives under `/System/`).

mod rules;

pub use rules::{
    builtin_rules, lint_rules, load_rules_dir, merge_user_rules, parse_rules, RuleLoadError, RuleWarning,
    MAX_RULE_FILE_BYTES, USER_PRIORITY_BOOST,
};

use oomtop_core::actions::{is_protected_process, ProtectContext};
use oomtop_core::attribution::{
    attribute, AttributionContext, CompiledRules, LineageEntry, Rule, RuleError, RuleSet,
};
use oomtop_core::fingerprint::process_fingerprint;
use oomtop_core::{Confidence, Group, GroupKind, ProcId, Process, Snapshot};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// Process names whose children are *launched*, not owned (Linux desktop shells / session managers).
/// Subreapers (`launchd`, `systemd`, `init`) are handled by core attribution and are not listed here.
pub const LAUNCHER_NAMES: &[&str] = &[
    "gnome-shell",
    "gnome-session-binary",
    "gnome-session-service",
    "plasmashell",
    "kwin_wayland",
    "kwin_x11",
    "ksmserver",
    "krunner",
    "xfce4-session",
    "xfce4-panel",
    "lxsession",
    "lxqt-session",
    "mate-session",
    "cinnamon-session",
    "sway",
    "Hyprland",
    "i3",
    "niri",
    "labwc",
    "river",
    "wayfire",
    "cosmic-comp",
    "cosmic-session",
];

/// Label given to a Virtualization.framework VM whose owning app is unknown.
pub const ORPHAN_VM_LABEL: &str = "Virtualization VM";

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// True for processes whose children should not be attributed to them (see crate docs).
pub fn is_launcher(p: &Process) -> bool {
    p.id.pid > 1 && (LAUNCHER_NAMES.contains(&p.name.as_str()) || LAUNCHER_NAMES.contains(&basename(&p.exe)))
}

/// True for the macOS Virtualization.framework VM XPC service (the process holding a guest's memory).
/// `name` may be truncated by the kernel (`com.apple.Virtualization.Virtua`), so both are checked.
pub fn is_virtualization_vm(p: &Process) -> bool {
    basename(&p.exe).starts_with("com.apple.Virtualization.VirtualMachine")
        || p.name.starts_with("com.apple.Virtualization.Virtu")
}

/// Compiled built-in + user rules, ready to attribute snapshots.
#[derive(Debug)]
pub struct Detector {
    rules: RuleSet,
    compiled: CompiledRules,
    /// The last attribution and its membership signature: an unchanged process set (the common case
    /// between refreshes) reuses it and only recomputes the numbers (SPEC §14 CPU budget).
    last: std::sync::Mutex<Option<(u64, Vec<Group>)>>,
}

impl Clone for Detector {
    fn clone(&self) -> Self {
        Detector {
            rules: self.rules.clone(),
            compiled: self.compiled.clone(),
            last: std::sync::Mutex::new(None),
        }
    }
}

impl Detector {
    pub fn new(rules: RuleSet) -> Result<Self, RuleError> {
        let compiled = CompiledRules::compile(&rules)?;
        Ok(Detector {
            rules,
            compiled,
            last: std::sync::Mutex::new(None),
        })
    }

    /// Built-in rules only.
    pub fn builtin() -> Self {
        let rules = builtin_rules();
        let compiled = CompiledRules::compile(&rules).unwrap_or_default();
        Detector {
            rules,
            compiled,
            last: std::sync::Mutex::new(None),
        }
    }

    /// Built-in rules plus user rules from `dir` (e.g. `~/.config/oomtop/rules.d`). User rules get
    /// [`USER_PRIORITY_BOOST`]; a user rule whose `id` equals a built-in id replaces that built-in. Bad files
    /// are reported and skipped; a missing directory is not an error.
    pub fn with_user_rules(dir: Option<&Path>) -> (Self, Vec<RuleLoadError>) {
        let builtin = builtin_rules();
        let (user, mut errors) = match dir {
            Some(d) => load_rules_dir(d),
            None => (RuleSet::default(), Vec::new()),
        };
        let merged = merge_user_rules(builtin, user);
        match Detector::new(merged) {
            Ok(d) => (d, errors),
            Err(source) => {
                // Unreachable in practice: every user file was compiled on its own before merging.
                errors.push(RuleLoadError::Compile {
                    path: "rules".into(),
                    source,
                });
                (Detector::builtin(), errors)
            }
        }
    }

    pub fn rules(&self) -> &RuleSet {
        &self.rules
    }

    pub fn compiled(&self) -> &CompiledRules {
        &self.compiled
    }

    /// Warnings for the loaded rules (e.g. marker keys missing from the privacy allowlist).
    pub fn lint(&self, allowlist: &[String]) -> Vec<RuleWarning> {
        lint_rules(&self.rules, allowlist)
    }

    /// The rule that makes `p` a group root, if any (for `why`/`doctor` explanations).
    pub fn matching_rule(&self, p: &Process) -> Option<&Rule> {
        self.compiled.match_root(p).map(|m| m.rule)
    }

    /// The rule whose session `p` joins through its markers, if any.
    pub fn matching_marker_rule(&self, p: &Process) -> Option<&Rule> {
        self.compiled.match_marker(p).map(|m| m.rule)
    }

    /// Attributes all processes of `snapshot` to groups (SPEC §7), with the launcher and orphan-VM
    /// corrections described in the crate docs. Every process lands in exactly one group.
    pub fn attribute(
        &self,
        snapshot: &Snapshot,
        lineage: &BTreeMap<ProcId, LineageEntry>,
        protect: &ProtectContext,
    ) -> Vec<Group> {
        // The signature is taken over the *raw* inputs: `prepare` is a pure function of them (and of the
        // rules), so equal raw inputs mean an equal prepared snapshot — and an unchanged membership skips
        // `prepare` and its snapshot clone entirely (SPEC §14 CPU budget). The per-sample numbers don't
        // depend on what `prepare` rewrites (launcher parents, demoted responsible pids).
        let raw_ctx = AttributionContext {
            rules: &self.compiled,
            lineage,
            protect,
        };
        let sig = oomtop_core::attribution::membership_signature(snapshot, &raw_ctx);
        if let Ok(last) = self.last.lock() {
            if let Some((_, groups)) = last.as_ref().filter(|(s, _)| *s == sig) {
                let mut groups = groups.clone();
                oomtop_core::attribution::refresh_numbers(&mut groups, snapshot);
                return groups;
            }
        }
        let (snap, lin) = self.prepare(snapshot, lineage);
        let ctx = AttributionContext {
            rules: &self.compiled,
            lineage: &lin,
            protect,
        };
        let mut groups = attribute(&snap, &ctx);
        fix_orphan_vms(&mut groups, snapshot, protect);
        if let Ok(mut last) = self.last.lock() {
            *last = Some((sig, groups.clone()));
        }
        groups
    }
}

impl Default for Detector {
    fn default() -> Self {
        Detector::builtin()
    }
}

impl Detector {
    /// Working copy of the inputs for core attribution (cloned only when something changes):
    ///
    /// - children of launchers are re-parented to 1; lineage entries of processes that were *always* direct
    ///   children of the launcher are dropped so they are not mistaken for re-parented orphans (entries
    ///   recording a different original parent are kept);
    /// - a responsible pid is ignored when a group-root ancestor (e.g. the `codex` CLI inside ChatGPT.app)
    ///   sits between the process and its responsible process: the nearest owner wins and ancestry
    ///   attributes the process instead.
    fn prepare<'a>(
        &self,
        snapshot: &'a Snapshot,
        lineage: &'a BTreeMap<ProcId, LineageEntry>,
    ) -> (Cow<'a, Snapshot>, Cow<'a, BTreeMap<ProcId, LineageEntry>>) {
        let procs = &snapshot.processes;
        let by_pid: HashMap<u32, usize> = procs.iter().enumerate().map(|(i, p)| (p.id.pid, i)).collect();
        let launcher: Vec<bool> = procs.iter().map(is_launcher).collect();
        let detached: Vec<usize> = procs
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.ppid
                    .filter(|&pp| pp != p.id.pid)
                    .and_then(|pp| by_pid.get(&pp))
                    .is_some_and(|&k| launcher[k])
            })
            .map(|(i, _)| i)
            .collect();
        let mut is_root: Vec<Option<bool>> = vec![None; procs.len()];
        let mut demoted = Vec::new();
        for (i, p) in procs.iter().enumerate() {
            let Some(r) = p.responsible_pid.filter(|&r| r != p.id.pid && r > 1) else {
                continue;
            };
            let mut cur = p.ppid;
            for _ in 0..64 {
                let Some(pp) = cur.filter(|&pp| pp > 1 && pp != r) else {
                    break;
                };
                let Some(&k) = by_pid.get(&pp) else { break };
                if launcher[k] || k == i {
                    break;
                }
                let root = *is_root[k].get_or_insert_with(|| self.compiled.match_root(&procs[k]).is_some());
                if root {
                    demoted.push(i);
                    break;
                }
                cur = procs[k].ppid;
            }
        }
        if detached.is_empty() && demoted.is_empty() {
            return (Cow::Borrowed(snapshot), Cow::Borrowed(lineage));
        }
        let mut snap = snapshot.clone();
        for &i in &demoted {
            snap.processes[i].responsible_pid = None;
        }
        let mut drop_lineage = Vec::new();
        for &i in &detached {
            let p = &mut snap.processes[i];
            if let Some(le) = lineage.get(&p.id) {
                if le.ppid.is_none() || le.ppid == p.ppid {
                    drop_lineage.push(p.id);
                }
            }
            p.ppid = Some(1);
        }
        let lin = if drop_lineage.is_empty() {
            Cow::Borrowed(lineage)
        } else {
            let mut l = lineage.clone();
            for id in drop_lineage {
                l.remove(&id);
            }
            Cow::Owned(l)
        };
        (Cow::Owned(snap), lin)
    }
}

/// A Virtualization.framework VM that fell through to the `system` heuristic (no responsible app found)
/// becomes a sandbox group. VMs attributed to an app are left alone.
fn fix_orphan_vms(groups: &mut [Group], snapshot: &Snapshot, protect: &ProtectContext) {
    for g in groups.iter_mut() {
        if g.kind != GroupKind::System || g.matched_by.is_some() {
            continue;
        }
        let Some(root) = g.root.and_then(|r| snapshot.process(r)) else {
            continue;
        };
        if !is_virtualization_vm(root) {
            continue;
        }
        g.kind = GroupKind::Sandbox;
        g.label = ORPHAN_VM_LABEL.to_string();
        g.id = format!("sandbox:virtualization-vm:{}", root.id.pid);
        g.fingerprint = process_fingerprint(GroupKind::Sandbox, root);
        g.lower_bound = true;
        g.confidence = g.confidence.max(Confidence::Medium);
        g.protected = is_protected_process(root, snapshot, protect);
    }
}

#[cfg(test)]
mod tests;

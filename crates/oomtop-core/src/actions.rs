//! Action planning and protection rules (SPEC §7 "actions target group roots", §13 safety). Pure: the
//! frontends build an [`ActionPlan`]; an [`Actuator`] (implemented in the CLI from `oomtop-collect::signal`
//! and `oomtop-adapters` gentle actions) executes it after explicit confirmation.
//!
//! Order: adapter-graceful action first → SIGTERM → SIGKILL only on a second explicit confirmation.
//! `(pid, start_time)` is re-verified immediately before any signal. Suspend frees no memory and never
//! appears in reclaim plans.

use crate::model::{Group, GroupKind, ProcId, Process, Snapshot};
use serde::{Deserialize, Serialize};

/// Process names that are always protected.
pub const BUILTIN_PROTECTED: &[&str] = &[
    "kernel_task",
    "launchd",
    "init",
    "systemd",
    "kthreadd",
    "WindowServer",
    "loginwindow",
    "Finder",
    "Dock",
    "SystemUIServer",
    "coreaudiod",
    "sshd",
    "Xorg",
    "Xwayland",
    "gnome-shell",
    "kwin_wayland",
    "plasmashell",
];

/// Context for protection decisions.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProtectContext {
    pub self_pid: Option<u32>,
    pub self_uid: Option<u32>,
    /// Ancestors of oomtop (its shell, terminal, and for MCP the hosting agent), never offered.
    pub ancestor_pids: Vec<u32>,
    /// Extra names from config `protected.names`.
    pub protected_names: Vec<String>,
    /// Group id of the agent session hosting an MCP call (excluded from its reclaim candidates).
    pub caller_group: Option<String>,
}

/// True for kernel threads, pid ≤ 1, built-in/user protected names, oomtop's own ancestry, and processes
/// owned by another user.
pub fn is_protected_process(p: &Process, _snapshot: &Snapshot, ctx: &ProtectContext) -> bool {
    if p.id.pid <= 1 {
        return true;
    }
    if Some(p.id.pid) == ctx.self_pid || ctx.ancestor_pids.contains(&p.id.pid) {
        return true;
    }
    // Linux kernel threads have no exe and no cmdline.
    if p.exe.is_empty() && p.cmdline.is_empty() && p.ppid == Some(2) {
        return true;
    }
    if let (Some(uid), Some(me)) = (p.uid, ctx.self_uid) {
        if uid != me {
            return true;
        }
    }
    let name = p.name.as_str();
    BUILTIN_PROTECTED.contains(&name) || ctx.protected_names.iter().any(|n| n == name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    /// Adapter-graceful action (e.g. Ollama unload, `lms unload`).
    Graceful,
    /// SIGTERM.
    #[default]
    Terminate,
    /// SIGKILL — only after a second explicit confirmation.
    Kill,
    /// SIGSTOP — CPU/GPU/thermal relief only; frees no memory.
    Suspend,
    /// SIGCONT.
    Resume,
}

/// One step of a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ActionTarget {
    pub group_id: String,
    pub label: String,
    /// Group root to signal (identity re-verified before signalling).
    pub root: ProcId,
    pub kind: ActionKind,
    pub expected_gain: Option<u64>,
    /// Graceful action description when `kind == Graceful`, e.g. "ollama unload llama3:8b".
    pub graceful: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ActionPlan {
    pub targets: Vec<ActionTarget>,
    /// Must be set by the frontend only after the user confirmed exactly this plan.
    pub confirmed: bool,
    /// Required in addition to `confirmed` for `Kill`.
    pub confirmed_kill: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ActionOutcome {
    pub target: ActionTarget,
    pub ok: bool,
    pub message: String,
    /// Re-measured gain after the action ("freed 5.6 GB, est. 5.9").
    pub measured_gain: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum Refusal {
    #[error("group {0} not found")]
    NotFound(String),
    #[error("{0} is protected")]
    Protected(String),
    #[error("{0} has no root process")]
    NoRoot(String),
    #[error("{0} is the calling agent's own session")]
    CallerSession(String),
    #[error("{0} is oomtop itself")]
    SelfGroup(String),
}

/// Plans an action on a group root. Helpers are never targeted individually (SPEC §7).
pub fn plan_group(group: &Group, kind: ActionKind, ctx: &ProtectContext) -> Result<ActionTarget, Refusal> {
    if group.is_self {
        return Err(Refusal::SelfGroup(group.label.clone()));
    }
    if group.protected || group.kind == GroupKind::System {
        return Err(Refusal::Protected(group.label.clone()));
    }
    if ctx.caller_group.as_deref() == Some(group.id.as_str()) {
        return Err(Refusal::CallerSession(group.label.clone()));
    }
    let root = group.root.ok_or_else(|| Refusal::NoRoot(group.label.clone()))?;
    if Some(root.pid) == ctx.self_pid || ctx.ancestor_pids.contains(&root.pid) {
        return Err(Refusal::Protected(group.label.clone()));
    }
    let expected_gain = match kind {
        ActionKind::Suspend | ActionKind::Resume => None,
        _ => group.reclaim_gain.value,
    };
    Ok(ActionTarget {
        group_id: group.id.clone(),
        label: group.label.clone(),
        root,
        kind,
        expected_gain,
        graceful: None,
    })
}

/// Plans stopping (SIGTERM) the given groups; unknown or refused groups are returned separately. A group
/// whose root process is missing from `snapshot` or protected per [`is_protected_process`] is refused.
pub fn plan_stop(
    snapshot: &Snapshot,
    group_ids: &[String],
    ctx: &ProtectContext,
) -> (ActionPlan, Vec<Refusal>) {
    let mut plan = ActionPlan::default();
    let mut refused = Vec::new();
    for id in group_ids {
        match snapshot.group(id) {
            None => refused.push(Refusal::NotFound(id.clone())),
            Some(g) => match plan_group(g, ActionKind::Terminate, ctx) {
                // The root process itself must be in the sample and pass the protect rules (other
                // users, pid ≤ 1, built-in / configured names) — not only the group flags.
                Ok(t) => match snapshot.process(t.root) {
                    Some(root) if !is_protected_process(root, snapshot, ctx) => plan.targets.push(t),
                    Some(_) => refused.push(Refusal::Protected(g.label.clone())),
                    None => refused.push(Refusal::NoRoot(g.label.clone())),
                },
                Err(r) => refused.push(r),
            },
        }
    }
    (plan, refused)
}

/// Executes confirmed plans (implemented outside core).
pub trait Actuator: Send {
    fn execute(&mut self, plan: &ActionPlan) -> Vec<ActionOutcome>;

    /// Graceful (adapter) actions that free a group's memory without a signal, described for the user
    /// (e.g. "ollama: unload llama3:8b"). Empty when the group has none (SPEC §10, §13: try these first).
    fn graceful_for(&self, _group_id: &str) -> Vec<String> {
        Vec::new()
    }

    /// Refreshes per-snapshot state (which model servers currently offer a graceful unload).
    fn observe(&mut self, _snapshot: &Snapshot) {}
}

/// An actuator that refuses everything (used by read-only frontends and tests).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopActuator;

impl Actuator for NoopActuator {
    fn execute(&mut self, plan: &ActionPlan) -> Vec<ActionOutcome> {
        plan.targets
            .iter()
            .map(|t| ActionOutcome {
                target: t.clone(),
                ok: false,
                message: "actions are disabled".into(),
                measured_gain: None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Measured;

    #[test]
    fn refuses_protected_and_self() {
        let ctx = ProtectContext {
            self_pid: Some(10),
            caller_group: Some("agent:x".into()),
            ..Default::default()
        };
        let mut g = Group {
            id: "daemon:gradle:5".into(),
            label: "GradleDaemon".into(),
            kind: GroupKind::BuildDaemon,
            root: Some(ProcId::new(5, 1)),
            reclaim_gain: Measured::estimate(3, "t"),
            ..Default::default()
        };
        let t = plan_group(&g, ActionKind::Terminate, &ctx).unwrap();
        assert_eq!(t.expected_gain, Some(3));
        assert_eq!(
            plan_group(&g, ActionKind::Suspend, &ctx).unwrap().expected_gain,
            None
        );
        g.protected = true;
        assert!(matches!(
            plan_group(&g, ActionKind::Terminate, &ctx),
            Err(Refusal::Protected(_))
        ));
        g.protected = false;
        g.id = "agent:x".into();
        assert!(matches!(
            plan_group(&g, ActionKind::Terminate, &ctx),
            Err(Refusal::CallerSession(_))
        ));
    }

    #[test]
    fn protected_processes() {
        let s = Snapshot::default();
        let ctx = ProtectContext {
            self_uid: Some(501),
            ..Default::default()
        };
        let mut p = Process {
            id: ProcId::new(300, 1),
            name: "WindowServer".into(),
            uid: Some(501),
            ..Default::default()
        };
        assert!(is_protected_process(&p, &s, &ctx));
        p.name = "java".into();
        assert!(!is_protected_process(&p, &s, &ctx));
        p.uid = Some(0);
        assert!(is_protected_process(&p, &s, &ctx));
    }
}

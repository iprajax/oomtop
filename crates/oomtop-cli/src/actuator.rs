//! Executes confirmed plans (SPEC §13): refuses unconfirmed plans; SIGKILL only with the second
//! confirmation; every signal goes through `oomtop_collect::signal::send`, which re-verifies
//! `(pid, start_time)` immediately before signalling and refuses pid ≤ 1 and oomtop itself.
//! Graceful targets run the model server's own unload action (SPEC §10) registered with
//! [`SignalActuator::register_gentle`]; no signal is sent for them.

use oomtop_adapters::{execute_gentle, gentle_actions, GentleAction};
use oomtop_collect::signal::{send, Signal};
use oomtop_core::actions::{ActionKind, ActionOutcome, ActionPlan, Actuator};
use oomtop_core::Snapshot;
use std::collections::BTreeMap;
use std::time::Duration;

/// Timeout for one gentle adapter action (a local HTTP call or `lms unload`).
pub const GENTLE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub struct SignalActuator {
    /// Group id → the adapter actions that free its memory without stopping it.
    gentle: BTreeMap<String, Vec<GentleAction>>,
}

impl SignalActuator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the gentle actions of every model server in the snapshot, keyed by its group.
    pub fn register_gentle(&mut self, s: &Snapshot) {
        self.gentle.clear();
        for ms in &s.model_servers {
            let Some(g) = &ms.group_id else { continue };
            let acts = gentle_actions(ms);
            if !acts.is_empty() {
                self.gentle.entry(g.clone()).or_default().extend(acts);
            }
        }
    }

    /// The registered gentle actions of a group.
    pub fn gentle_for(&self, group_id: &str) -> &[GentleAction] {
        self.gentle.get(group_id).map(Vec::as_slice).unwrap_or(&[])
    }

    fn run_gentle(&self, group_id: &str) -> Result<String, String> {
        let acts = self.gentle_for(group_id);
        if acts.is_empty() {
            return Err("no graceful adapter action available for this group".into());
        }
        let mut done = Vec::new();
        for a in acts {
            execute_gentle(a, GENTLE_TIMEOUT).map_err(|e| format!("{}: {e}", a.describe()))?;
            done.push(a.describe());
        }
        Ok(done.join("; "))
    }
}

impl Actuator for SignalActuator {
    fn graceful_for(&self, group_id: &str) -> Vec<String> {
        self.gentle_for(group_id).iter().map(|a| a.describe()).collect()
    }

    fn observe(&mut self, snapshot: &Snapshot) {
        self.register_gentle(snapshot);
    }

    fn execute(&mut self, plan: &ActionPlan) -> Vec<ActionOutcome> {
        plan.targets
            .iter()
            .map(|t| {
                let res: Result<String, String> = if !plan.confirmed {
                    Err("not confirmed".into())
                } else {
                    match t.kind {
                        ActionKind::Terminate => send(t.root, Signal::Term)
                            .map(|_| "sent SIGTERM".into())
                            .map_err(|e| e.to_string()),
                        ActionKind::Kill if plan.confirmed_kill => send(t.root, Signal::Kill)
                            .map(|_| "sent SIGKILL".into())
                            .map_err(|e| e.to_string()),
                        ActionKind::Kill => Err("SIGKILL needs a second confirmation".into()),
                        ActionKind::Suspend => send(t.root, Signal::Stop)
                            .map(|_| "suspended (SIGSTOP); frees no memory".into())
                            .map_err(|e| e.to_string()),
                        ActionKind::Resume => send(t.root, Signal::Cont)
                            .map(|_| "resumed (SIGCONT)".into())
                            .map_err(|e| e.to_string()),
                        ActionKind::Graceful => self.run_gentle(&t.group_id),
                    }
                };
                match res {
                    Ok(m) => ActionOutcome {
                        target: t.clone(),
                        ok: true,
                        message: m,
                        measured_gain: None,
                    },
                    Err(m) => ActionOutcome {
                        target: t.clone(),
                        ok: false,
                        message: m,
                        measured_gain: None,
                    },
                }
            })
            .collect()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use oomtop_core::actions::ActionTarget;
    use oomtop_core::ProcId;

    /// A `sleep 600` this test spawned; killed and reaped even if an assertion fails.
    struct Sleeper(std::process::Child);

    impl Sleeper {
        fn spawn() -> Self {
            Sleeper(std::process::Command::new("sleep").arg("600").spawn().unwrap())
        }
        fn id(&self) -> ProcId {
            let pid = self.0.id();
            ProcId::new(pid, oomtop_collect::signal::process_start_time_ms(pid).unwrap())
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn refuses_unconfirmed_and_kills_only_twice_confirmed() {
        // Only signals a child this test spawned.
        let child = Sleeper::spawn();
        let t = ActionTarget {
            root: child.id(),
            kind: ActionKind::Kill,
            label: "sleep".into(),
            ..Default::default()
        };
        let mut a = SignalActuator::new();
        let mut plan = ActionPlan {
            targets: vec![t.clone()],
            confirmed: false,
            confirmed_kill: false,
        };
        assert!(!a.execute(&plan)[0].ok);
        plan.confirmed = true;
        assert!(!a.execute(&plan)[0].ok, "kill needs second confirmation");
        plan.targets[0].kind = ActionKind::Terminate;
        let out = a.execute(&plan);
        assert!(out[0].ok, "{:?}", out[0]);
    }

    #[test]
    fn stale_identity_is_refused() {
        // A pid with the wrong start time must never be signalled (pid reuse guard).
        let child = Sleeper::spawn();
        let id = child.id();
        let plan = ActionPlan {
            targets: vec![ActionTarget {
                root: ProcId::new(id.pid, id.start_time.saturating_sub(60_000)),
                kind: ActionKind::Terminate,
                ..Default::default()
            }],
            confirmed: true,
            confirmed_kill: false,
        };
        let out = SignalActuator::new().execute(&plan);
        assert!(!out[0].ok, "{:?}", out[0]);
        assert!(oomtop_collect::signal::is_alive(id));
    }

    #[test]
    fn graceful_without_registered_action_is_an_error_not_a_signal() {
        let plan = ActionPlan {
            targets: vec![ActionTarget {
                group_id: "model:ollama".into(),
                kind: ActionKind::Graceful,
                root: ProcId::new(999_999, 1),
                ..Default::default()
            }],
            confirmed: true,
            confirmed_kill: false,
        };
        let out = SignalActuator::new().execute(&plan);
        assert!(!out[0].ok);
        assert!(out[0].message.contains("no graceful"));
    }
}

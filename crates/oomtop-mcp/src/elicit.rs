//! MCP elicitation for `reclaim` (SPEC §12.3, §13): oomtop — not the host — asks the user to confirm the
//! exact list of targets and gains, and acts only on an explicit accept.
//!
//! The request uses a flat schema with one required boolean `confirm` (default `false`). oomtop acts only when
//! the response has `action == "accept"` **and** `content.confirm == true`; `decline`, `cancel`, an accept
//! without the box ticked, a timeout, an error or a closed connection all mean "do nothing".

use crate::server::amount;
use oomtop_core::actions::ActionPlan;
use oomtop_core::redact::redact_text;
use oomtop_core::units::{format_duration, UnitSystem};
use oomtop_core::Snapshot;
use serde_json::{json, Value};

pub const METHOD: &str = "elicitation/create";

/// What the user answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// `action == "accept"` with `confirm == true`.
    Accepted,
    /// `action == "decline"`.
    Declined,
    /// `action == "cancel"` (dismissed).
    Cancelled,
    /// Accepted without confirming, or an unknown action; the string says why.
    NotConfirmed(String),
}

impl Answer {
    /// The raw `action` value reported back to the agent.
    pub fn user_action(&self) -> &'static str {
        match self {
            Answer::Accepted => "accept",
            Answer::Declined => "decline",
            Answer::Cancelled => "cancel",
            Answer::NotConfirmed(_) => "accept_unconfirmed",
        }
    }
}

/// Builds the human message listing every target, its pid, idle time and estimated gain.
pub fn message(plan: &ActionPlan, snapshot: &Snapshot, headroom: Option<i64>, units: UnitSystem) -> String {
    let n = plan.targets.len();
    let mut m = format!(
        "oomtop: an agent asks to stop {n} group{} to free memory.\n\n",
        if n == 1 { "" } else { "s" }
    );
    let mut total = 0u64;
    for t in &plan.targets {
        let g = snapshot.group(&t.group_id);
        let state = match g {
            Some(g) if g.orphan => "orphan".to_string(),
            Some(g) if g.idle => match g.idle_for_s {
                Some(s) => format!("idle {}", format_duration(s)),
                None => "idle".to_string(),
            },
            Some(_) => "ACTIVE".to_string(),
            None => "unknown".to_string(),
        };
        let procs = g.map(|g| g.totals.process_count).unwrap_or(0);
        let gain = t
            .expected_gain
            .map(|b| {
                total += b;
                format!("est. +{} RAM", amount(units, b))
            })
            .unwrap_or_else(|| "gain unknown".to_string());
        let action = match &t.graceful {
            Some(g) => format!("graceful: {g} (no signal)"),
            None => "SIGTERM".to_string(),
        };
        let note = crate::server::guard_note(snapshot, &t.group_id)
            .map(|n| format!(" — {n}"))
            .unwrap_or_default();
        m.push_str(&format!(
            "  - {} [{}] pid {}, {} process{}, {}, {}, {}{}\n",
            redact_text(&t.label),
            t.group_id,
            t.root.pid,
            procs,
            if procs == 1 { "" } else { "es" },
            state,
            gain,
            action,
            note
        ));
    }
    m.push_str(&format!(
        "\nModel servers with an unload action are unloaded (no signal); every other group root gets SIGTERM \
         (graceful stop, never SIGKILL). Estimated total: {}.",
        amount(units, total)
    ));
    if let Some(h) = headroom {
        let sign = if h < 0 { "-" } else { "" };
        m.push_str(&format!(
            " Headroom now: {sign}{}.",
            amount(units, h.unsigned_abs())
        ));
    }
    m.push_str("\nTick \"confirm\" and accept only if you want these stopped now.");
    m
}

/// `elicitation/create` params (MCP 2025-06-18): message + a flat primitive-only schema.
pub fn request_params(
    plan: &ActionPlan,
    snapshot: &Snapshot,
    headroom: Option<i64>,
    units: UnitSystem,
) -> Value {
    let n = plan.targets.len();
    json!({
        "message": message(plan, snapshot, headroom, units),
        "requestedSchema": {
            "type": "object",
            "properties": {
                "confirm": {
                    "type": "boolean",
                    "title": format!("Stop {n} group{}", if n == 1 { "" } else { "s" }),
                    "description": "Unload or SIGTERM exactly the groups listed above.",
                    "default": false
                }
            },
            "required": ["confirm"]
        }
    })
}

/// Interprets an `elicitation/create` result.
pub fn parse_response(result: &Value) -> Answer {
    match result.get("action").and_then(Value::as_str) {
        Some("accept") => match result.pointer("/content/confirm").and_then(Value::as_bool) {
            Some(true) => Answer::Accepted,
            Some(false) => Answer::NotConfirmed("accepted with confirm = false".into()),
            None => Answer::NotConfirmed("accepted without a confirm value".into()),
        },
        Some("decline") => Answer::Declined,
        Some("cancel") => Answer::Cancelled,
        Some(other) => Answer::NotConfirmed(format!("unknown elicitation action {other:?}")),
        None => Answer::NotConfirmed("elicitation result has no action".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::actions::ActionTarget;
    use oomtop_core::ProcId;

    #[test]
    fn parses_answers_strictly() {
        assert_eq!(
            parse_response(&json!({"action":"accept","content":{"confirm":true}})),
            Answer::Accepted
        );
        assert!(matches!(
            parse_response(&json!({"action":"accept","content":{"confirm":false}})),
            Answer::NotConfirmed(_)
        ));
        assert!(matches!(
            parse_response(&json!({"action":"accept"})),
            Answer::NotConfirmed(_)
        ));
        assert!(matches!(
            parse_response(&json!({"action":"accept","content":{"confirm":"true"}})),
            Answer::NotConfirmed(_)
        ));
        assert_eq!(parse_response(&json!({"action":"decline"})), Answer::Declined);
        assert_eq!(parse_response(&json!({"action":"cancel"})), Answer::Cancelled);
        assert!(matches!(
            parse_response(&json!({"action":"yes"})),
            Answer::NotConfirmed(_)
        ));
        assert!(matches!(parse_response(&json!({})), Answer::NotConfirmed(_)));
    }

    #[test]
    fn message_lists_targets_and_gains() {
        let plan = ActionPlan {
            targets: vec![ActionTarget {
                group_id: "daemon:gradle:5".into(),
                label: "GradleDaemon".into(),
                root: ProcId::new(5, 1),
                expected_gain: Some(3 << 30),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = request_params(&plan, &Snapshot::default(), Some(-(1 << 30)), UnitSystem::Iec);
        let m = p["message"].as_str().unwrap();
        assert!(m.contains("GradleDaemon [daemon:gradle:5] pid 5"), "{m}");
        assert!(m.contains("SIGTERM") && m.contains("Headroom now: -"), "{m}");
        assert_eq!(p["requestedSchema"]["required"][0], "confirm");
        assert_eq!(p["requestedSchema"]["properties"]["confirm"]["type"], "boolean");
    }
}

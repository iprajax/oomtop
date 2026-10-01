//! Regression tests for the spec audit: label redaction on every export path, root-process protection
//! for `reclaim` (before asking and again after the user confirms), honest statuses after an accept, and
//! consistent units in the text an agent reads.

mod support;

use oomtop_core::actions::{ActionOutcome, ActionPlan, Actuator};
use oomtop_core::{GroupKind, ProcId};
use oomtop_mcp::{tool_definitions, IoConfig};
use serde_json::{json, Value};
use support::*;

/// A token `redact_text` recognises anywhere in a string.
const SECRET: &str = "sk-ant-api03-verysecretvalue1234567890abcdef";

fn start(opts: oomtop_mcp::McpOptions, elicitation: bool) -> FakeClient {
    let mut c = FakeClient::start(opts, IoConfig::default());
    c.handshake(elicitation);
    c
}

fn reclaim(c: &mut FakeClient, id: u64, ids: &[&str]) {
    c.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
        "params":{"name":"reclaim","arguments":{"group_ids":ids}}}));
}

fn accept(c: &mut FakeClient) -> Value {
    let req = c.recv();
    assert_eq!(req["method"], "elicitation/create", "{req}");
    c.send(json!({"jsonrpc":"2.0","id":req["id"],"result":{"action":"accept","content":{"confirm":true}}}));
    req
}

fn refused_reason<'a>(sc: &'a Value, group: &str) -> &'a str {
    sc["refused"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["group_id"] == group)
        .and_then(|x| x["reason"].as_str())
        .unwrap_or_else(|| panic!("{group} not refused: {sc}"))
}

#[test]
fn secret_labels_never_leave_through_any_tool() {
    // Group, sandbox, victim and model labels derived from command lines carry a secret.
    let mut s = fixture();
    for g in &mut s.groups {
        if g.id == GRADLE {
            g.label = format!("GradleDaemon --token={SECRET}");
        }
        if g.id == SD {
            g.label = format!("sd-server {SECRET}");
        }
    }
    s.sandboxes[0].label = format!("vm API_KEY={SECRET}");
    s.oom.likely_victim = Some(oomtop_core::Victim {
        id: ProcId::new(400, 1_400),
        name: format!("sd-server {SECRET}"),
        ..Default::default()
    });
    let rec = Recorder::default();
    let mut opts = options(true, Some(rec.clone()));
    opts.provider = Box::new(oomtop_core::provider::StaticProvider::new(s));
    let mut c = start(opts, true);
    let tools = tool_definitions(true);
    let mut all = String::new();
    for (i, (tool, args)) in [
        ("get_headroom", json!({})),
        // 13 GiB needs the build daemons: their labels appear in the reclaim list and the reason.
        ("can_fit", json!({"size": "13G"})),
        ("top_consumers", json!({})),
        ("list_groups", json!({})),
        ("list_model_servers", json!({})),
        ("list_sandboxes", json!({})),
        ("explain_slowdown", json!({})),
        ("suggest_reclaim", json!({})),
    ]
    .into_iter()
    .enumerate()
    {
        let r = c.call(i as u64 + 1, tool, args);
        assert_conforms(&tools, tool, &r["result"]);
        all.push_str(&r.to_string());
    }
    // The elicitation message and the refusals of reclaim, too.
    reclaim(&mut c, 20, &[GRADLE, SD]);
    let req = accept(&mut c);
    all.push_str(&req.to_string());
    let r = c.recv();
    all.push_str(&r.to_string());
    assert!(!all.contains(SECRET), "a secret label leaked:\n{all}");
    assert!(all.contains("GradleDaemon"), "labels stay readable");
    // Ids are not redacted, so the reclaim still targeted the right group.
    assert_eq!(rec.plans()[0].targets[0].group_id, GRADLE);
    assert!(!format!("{:?}", rec.plans()).contains(SECRET));
    c.finish();
}

#[test]
fn reclaim_refuses_protected_root_processes() {
    // Groups that are not flagged protected, but whose root process is: another user's process, a built-in
    // protected name, a name from the user's `protected.names`, and a root missing from the sample.
    let mut s = fixture();
    let template = s.groups.iter().find(|g| g.id == GRADLE).unwrap().clone();
    let proc_template = s.processes.iter().find(|p| p.id.pid == 300).unwrap().clone();
    let mut add = |id: &str, pid: u32, name: &str, uid: u32, in_sample: bool| {
        let pid_id = ProcId::new(pid, 1_000 + pid as u64);
        let mut g = template.clone();
        g.id = id.into();
        g.label = name.into();
        g.kind = GroupKind::BuildDaemon;
        g.root = Some(pid_id);
        g.members[0].id = pid_id;
        s.groups.push(g);
        if in_sample {
            let mut p = proc_template.clone();
            p.id = pid_id;
            p.name = name.into();
            p.uid = Some(uid);
            s.processes.push(p);
        }
    };
    add("daemon:rootowned", 700, "cupsd", 0, true);
    add("daemon:windowserver", 701, "WindowServer", 501, true);
    add("daemon:mine-but-listed", 702, "ollama", 501, true);
    add("daemon:ghost", 703, "ghostd", 501, false);
    let rec = Recorder::default();
    let mut opts = options(true, Some(rec.clone()));
    opts.provider = Box::new(oomtop_core::provider::StaticProvider::new(s));
    opts.protect.protected_names = vec!["ollama".into()];
    let mut c = start(opts, true);
    let ids = [
        "daemon:rootowned",
        "daemon:windowserver",
        "daemon:mine-but-listed",
        "daemon:ghost",
        KOTLIN,
    ];
    reclaim(&mut c, 1, &ids);
    let req = accept(&mut c);
    let msg = req["params"]["message"].as_str().unwrap();
    for bad in ["cupsd", "WindowServer", "ollama", "ghostd"] {
        assert!(!msg.contains(bad), "{bad} offered to the user: {msg}");
    }
    let r = c.recv();
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["status"], "done", "{sc}");
    for id in &ids[..3] {
        assert!(refused_reason(sc, id).contains("protected"), "{id}: {sc}");
    }
    assert!(refused_reason(sc, "daemon:ghost").contains("not in the current sample"));
    let plans = rec.plans();
    assert_eq!(plans.len(), 1);
    let targets: Vec<&str> = plans[0].targets.iter().map(|t| t.group_id.as_str()).collect();
    assert_eq!(targets, [KOTLIN], "only the unprotected daemon is signalled");
    c.finish();
}

#[test]
fn a_target_protected_after_confirmation_is_not_signalled() {
    let first = fixture();
    let mut second = fixture();
    for g in &mut second.groups {
        if g.id == GRADLE {
            g.protected = true; // e.g. the user added it to protected.names meanwhile
        }
    }
    let (prov, _) = Scripted::new(vec![first, second]);
    let rec = Recorder::default();
    let mut opts = options(true, Some(rec.clone()));
    opts.provider = Box::new(prov);
    let mut c = start(opts, true);
    reclaim(&mut c, 1, &[GRADLE]);
    let req = accept(&mut c);
    assert!(req["params"]["message"]
        .as_str()
        .unwrap()
        .contains("GradleDaemon"));
    let r = c.recv();
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["status"], "nothing_to_do", "{sc}");
    assert_eq!(sc["executed"], false);
    assert!(refused_reason(sc, GRADLE).contains("protected"));
    assert!(rec.plans().is_empty(), "nothing executed");
    c.finish();
}

/// Reports every signal as failed.
struct Failing;

impl Actuator for Failing {
    fn execute(&mut self, plan: &ActionPlan) -> Vec<ActionOutcome> {
        plan.targets
            .iter()
            .map(|t| ActionOutcome {
                target: t.clone(),
                ok: false,
                message: "kill: operation not permitted".into(),
                measured_gain: None,
            })
            .collect()
    }
}

#[test]
fn failed_signals_are_reported_as_failed_not_unconfirmed() {
    let mut opts = options(true, None);
    opts.actuator = Some(Box::new(Failing));
    let mut c = start(opts, true);
    reclaim(&mut c, 1, &[GRADLE]);
    accept(&mut c);
    let r = c.recv();
    assert_conforms(&tool_definitions(true), "reclaim", &r["result"]);
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["status"], "failed", "{sc}");
    assert_eq!(sc["executed"], false);
    assert_eq!(sc["user_action"], "accept");
    assert!(sc["summary"]
        .as_str()
        .unwrap()
        .contains("operation not permitted"));
    c.finish();
}

#[test]
fn summaries_use_the_configured_units_like_the_cli() {
    // Default units are IEC with one decimal — what `oomtop headroom --need 13G` prints — so an agent that
    // asked for 13G is told "13.0 GiB", never "14 GB", and no sentence mixes "GiB" with "5.4G".
    let mut c = start(options(false, None), false);
    let h = c.call(1, "get_headroom", json!({}));
    let fit = c.call(2, "can_fit", json!({"size": "13G", "label": "qwen-image"}));
    let sr = c.call(3, "suggest_reclaim", json!({}));
    for r in [&h, &fit, &sr] {
        let s = r["result"]["structuredContent"]["summary"].as_str().unwrap();
        assert!(s.contains(" GiB") || s.contains(" MiB"), "IEC units: {s}");
        assert!(
            !s.contains(" GB") && !s.contains(" MB") && !regex_like_short_unit(s),
            "{s}"
        );
    }
    let sc = &fit["result"]["structuredContent"];
    let s = sc["summary"].as_str().unwrap().to_string();
    assert!(
        s.starts_with("Yes after reclaim: 13.0 GiB for qwen-image fits if GradleDaemon, KotlinCompileDaemon"),
        "{s}"
    );
    // The core's reason is still there for tools that parse it, in the same units.
    let reason = sc["reason"].as_str().unwrap();
    assert!(
        reason.contains(" GiB") && !regex_like_short_unit(reason),
        "{reason}"
    );
    c.finish();

    // A "no": the same sentence the CLI prints after "doesn't fit —".
    let mut c = start(options(false, None), false);
    let fit = c.call(1, "can_fit", json!({"size": "40G"}));
    let sc = &fit["result"]["structuredContent"];
    let s = sc["summary"].as_str().unwrap();
    let reason = sc["reason"].as_str().unwrap();
    assert!(s.starts_with("No: 40.0 GiB does not fit — short by "), "{s}");
    assert!(s.contains(reason), "summary {s:?} carries the reason {reason:?}");
    c.finish();

    // `format.memory_units = "si"`: every amount in GB.
    let mut o = options(false, None);
    o.units = oomtop_core::units::UnitSystem::Si;
    let mut c = start(o, false);
    let fit = c.call(1, "can_fit", json!({"size": "13G", "label": "qwen-image"}));
    let s = fit["result"]["structuredContent"]["summary"].as_str().unwrap();
    assert!(s.starts_with("Yes after reclaim: 14.0 GB for qwen-image"), "{s}");
    assert!(!s.contains("GiB"), "{s}");
    c.finish();
}

/// True if `s` contains a binary short unit like "5.4G" (digit immediately followed by K/M/G/T).
fn regex_like_short_unit(s: &str) -> bool {
    let b = s.as_bytes();
    b.windows(2)
        .any(|w| w[0].is_ascii_digit() && matches!(w[1], b'K' | b'M' | b'G' | b'T'))
}

/// An actuator that records plans and offers a graceful unload for sd-server (like the CLI's SignalActuator
/// with a model server that has an unload API).
#[derive(Clone, Default)]
struct GentleRecorder(Recorder);

impl Actuator for GentleRecorder {
    fn execute(&mut self, plan: &ActionPlan) -> Vec<ActionOutcome> {
        self.0.execute(plan)
    }
    fn graceful_for(&self, group_id: &str) -> Vec<String> {
        if group_id == SD {
            vec!["sd-server: unload qwen-image".into()]
        } else {
            Vec::new()
        }
    }
}

fn with_snapshot(s: oomtop_core::Snapshot, actuator: Box<dyn Actuator>) -> oomtop_mcp::McpOptions {
    let mut o = options(true, None);
    o.provider = Box::new(oomtop_core::provider::StaticProvider::new(s));
    o.actuator = Some(actuator);
    o
}

#[test]
fn reclaim_refuses_groups_that_are_not_reclaim_candidates() {
    // Chrome is an active app, sd-server is busy-ish (not idle): `oomtop reclaim` would offer neither, so MCP
    // must not either — and nothing is asked.
    let rec = Recorder::default();
    let mut c = start(options(true, Some(rec.clone())), true);
    reclaim(&mut c, 1, &[CHROME, SD]);
    let r = c.recv();
    let sc = structured(&r).clone();
    assert_eq!(sc["status"], "nothing_to_do", "{sc}");
    assert!(
        refused_reason(&sc, CHROME).contains("not a reclaim candidate"),
        "{sc}"
    );
    assert!(
        refused_reason(&sc, SD).contains("not a reclaim candidate"),
        "{sc}"
    );
    assert!(rec.plans().is_empty());
    c.finish();
}

#[test]
fn reclaim_unloads_model_servers_gracefully_and_refuses_busy_ones() {
    // An idle sd-server with an unload action: planned as Graceful, said so in the confirmation.
    let mut s = fixture();
    for g in &mut s.groups {
        if g.id == SD {
            g.idle = true;
            g.idle_for_s = Some(3600);
            g.totals.cpu_pct = oomtop_core::Measured::exact(0.0, "t");
        }
    }
    s.model_servers[0].busy = oomtop_core::Measured::exact(false, "sd-server /jobs");
    let rec = GentleRecorder::default();
    let mut c = start(with_snapshot(s.clone(), Box::new(rec.clone())), true);
    reclaim(&mut c, 1, &[SD]);
    let req = accept(&mut c);
    let msg = req["params"]["message"].as_str().unwrap();
    assert!(
        msg.contains("graceful: sd-server: unload qwen-image (no signal)"),
        "{msg}"
    );
    let r = c.recv();
    let sc = structured(&r).clone();
    assert_eq!(sc["targets"][0]["action"], "graceful", "{sc}");
    let plans = rec.0.plans();
    assert_eq!(plans.len(), 1);
    assert_eq!(
        plans[0].targets[0].kind,
        oomtop_core::actions::ActionKind::Graceful
    );
    c.finish();

    // The same server running a job, without an unload action: refused, never signalled.
    let mut busy = s;
    busy.model_servers[0].busy = oomtop_core::Measured::exact(true, "sd-server /jobs");
    let rec = Recorder::default();
    let mut c = start(with_snapshot(busy, Box::new(rec.clone())), true);
    reclaim(&mut c, 2, &[SD]);
    let r = c.recv();
    let sc = structured(&r).clone();
    assert!(refused_reason(&sc, SD).contains("busy"), "{sc}");
    assert!(rec.plans().is_empty());
    c.finish();
}

fn structured(r: &Value) -> &Value {
    assert_eq!(r["result"]["isError"], false, "{r}");
    &r["result"]["structuredContent"]
}

#[test]
fn get_headroom_summary_states_the_headroom_can_fit_uses() {
    // The UX headline alone ("All good — 10.0 GiB free") is available-now; an agent reading only the
    // summary must also read the headroom after the safety margin — the number `can_fit` decides with.
    let mut c = start(options(false, None), false);
    let h = c.call(1, "get_headroom", json!({}));
    let sc = structured(&h).clone();
    let s = sc["summary"].as_str().unwrap();
    let room = sc["headroom"]["headroom"].as_i64().unwrap();
    let avail = sc["headroom"]["available_now"]["value"].as_u64().unwrap();
    let margin = sc["headroom"]["safety_margin"].as_u64().unwrap();
    let fmt = |b: u64| oomtop_core::units::format_bytes(b, oomtop_core::units::UnitSystem::Iec, 1);
    let want = format!(
        "Headroom {} ({} available minus {} safety margin).",
        fmt(room as u64),
        fmt(avail),
        fmt(margin)
    );
    assert!(s.ends_with(&want), "{s}");
    let fit = c.call(2, "can_fit", json!({"size": "1G"}));
    let fs = structured(&fit)["summary"].as_str().unwrap().to_string();
    assert!(
        fs.contains(&format!("headroom {}", fmt(room as u64))),
        "{fs} vs {s}"
    );
    c.finish();
}

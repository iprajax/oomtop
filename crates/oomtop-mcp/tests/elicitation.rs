//! `reclaim` with `--allow-actions`: the elicitation round-trip with a fake MCP client over pipes.
//! Accept → act (SIGTERM plan, confirmed); decline / cancel / unconfirmed / timeout / error → nothing.

mod support;

use oomtop_core::actions::{ActionOutcome, ActionPlan, Actuator};
use oomtop_core::history::History;
use oomtop_core::provider::SnapshotProvider;
use oomtop_core::{ProcId, Snapshot};
use oomtop_mcp::{IoConfig, McpOptions};
use serde_json::{json, Value};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

fn start(elicitation: bool, rec: &Recorder) -> FakeClient {
    let mut c = FakeClient::start(options(true, Some(rec.clone())), IoConfig::default());
    c.handshake(elicitation);
    c
}

fn call_reclaim(c: &mut FakeClient, id: u64, ids: &[&str]) {
    c.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
        "params":{"name":"reclaim","arguments":{"group_ids":ids}}}));
}

/// Receives the server's elicitation/create request and checks its shape.
fn expect_elicitation(c: &mut FakeClient) -> Value {
    let req = c.recv();
    assert_eq!(req["method"], "elicitation/create", "{req}");
    assert!(req["id"].is_string(), "server request ids are strings: {req}");
    let schema = &req["params"]["requestedSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["confirm"]["type"], "boolean");
    assert_eq!(schema["required"], json!(["confirm"]));
    req
}

fn answer(c: &mut FakeClient, req: &Value, result: Value) {
    c.send(json!({"jsonrpc":"2.0","id":req["id"],"result":result}));
}

fn structured(r: &Value) -> &Value {
    assert_eq!(r["result"]["isError"], false, "{r}");
    &r["result"]["structuredContent"]
}

#[test]
fn accept_executes_exactly_the_confirmed_plan() {
    let rec = Recorder::default();
    let mut c = start(true, &rec);
    call_reclaim(&mut c, 7, &[GRADLE, KOTLIN, CALLER_GROUP, SYSTEM, "nope:1"]);
    let req = expect_elicitation(&mut c);
    let msg = req["params"]["message"].as_str().unwrap();
    assert!(msg.contains("GradleDaemon [daemon:gradle] pid 300"), "{msg}");
    assert!(
        msg.contains("KotlinCompileDaemon [daemon:kotlin] pid 301"),
        "{msg}"
    );
    assert!(msg.contains("idle 3h"), "{msg}");
    assert!(msg.contains("SIGTERM") && msg.contains("never SIGKILL"), "{msg}");
    assert!(!msg.contains(CALLER_GROUP), "caller never offered: {msg}");
    assert!(!msg.contains("launchd"), "protected never offered: {msg}");
    assert!(rec.plans().is_empty(), "nothing executed before the answer");

    answer(
        &mut c,
        &req,
        json!({"action":"accept","content":{"confirm":true}}),
    );
    let r = c.recv();
    assert_eq!(r["id"], 7);
    let sc = structured(&r);
    assert_eq!(sc["status"], "done", "{sc}");
    assert_eq!(sc["executed"], true);
    assert_eq!(sc["user_action"], "accept");
    assert_eq!(sc["expected_gain"], 2900 * MIB + 2800 * MIB);
    assert_eq!(sc["command"], Value::Null);
    let refused: Vec<(&str, &str)> = sc["refused"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["group_id"].as_str().unwrap(), x["reason"].as_str().unwrap()))
        .collect();
    assert_eq!(refused.len(), 3, "{refused:?}");
    assert!(refused
        .iter()
        .any(|(g, r)| *g == CALLER_GROUP && r.contains("calling agent")));
    assert!(refused
        .iter()
        .any(|(g, r)| *g == SYSTEM && r.contains("protected")));
    assert!(refused
        .iter()
        .any(|(g, r)| *g == "nope:1" && r.contains("not found")));
    assert_eq!(sc["outcomes"].as_array().unwrap().len(), 2);

    let plans = rec.plans();
    assert_eq!(plans.len(), 1);
    let p = &plans[0];
    assert!(p.confirmed && !p.confirmed_kill);
    let roots: Vec<ProcId> = p.targets.iter().map(|t| t.root).collect();
    assert_eq!(roots, [ProcId::new(300, 1_300), ProcId::new(301, 1_301)]);
    assert!(p
        .targets
        .iter()
        .all(|t| t.kind == oomtop_core::actions::ActionKind::Terminate));
    c.finish();
}

#[test]
fn decline_cancel_and_unconfirmed_do_nothing() {
    for (result, status, action) in [
        (json!({"action":"decline"}), "declined", "decline"),
        (json!({"action":"cancel"}), "cancelled", "cancel"),
        (
            json!({"action":"accept","content":{"confirm":false}}),
            "not_confirmed",
            "accept_unconfirmed",
        ),
        (json!({"action":"accept"}), "not_confirmed", "accept_unconfirmed"),
        (json!({"action":"maybe"}), "not_confirmed", "accept_unconfirmed"),
    ] {
        let rec = Recorder::default();
        let mut c = start(true, &rec);
        call_reclaim(&mut c, 1, &[GRADLE]);
        let req = expect_elicitation(&mut c);
        answer(&mut c, &req, result.clone());
        let r = c.recv();
        assert_eq!(r["id"], 1);
        let sc = structured(&r);
        assert_eq!(sc["status"], status, "{result} → {sc}");
        assert_eq!(sc["executed"], false);
        assert_eq!(sc["user_action"], action);
        assert_eq!(sc["command"], format!("oomtop reclaim --groups {GRADLE}"));
        assert!(rec.plans().is_empty(), "{result}: nothing may execute");
        c.finish();
    }
}

#[test]
fn without_elicitation_capability_returns_the_command() {
    let rec = Recorder::default();
    let mut c = start(false, &rec);
    call_reclaim(&mut c, 3, &[GRADLE, KOTLIN]);
    // The very next message is the tool result: no elicitation request was sent.
    let r = c.recv();
    assert_eq!(r["id"], 3, "{r}");
    let sc = structured(&r);
    assert_eq!(sc["status"], "needs_user");
    assert_eq!(sc["executed"], false);
    assert_eq!(
        sc["command"],
        format!("oomtop reclaim --groups {GRADLE},{KOTLIN}")
    );
    assert!(sc["summary"]
        .as_str()
        .unwrap()
        .contains("does not support elicitation"));
    assert_eq!(sc["targets"].as_array().unwrap().len(), 2);
    assert!(rec.plans().is_empty());
    c.finish();
}

#[test]
fn older_protocol_never_elicits() {
    let rec = Recorder::default();
    let mut c = FakeClient::start(options(true, Some(rec.clone())), IoConfig::default());
    c.request(
        0,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{"elicitation":{}}}),
    );
    call_reclaim(&mut c, 1, &[GRADLE]);
    let r = c.recv();
    assert_eq!(r["id"], 1);
    assert_eq!(structured(&r)["status"], "needs_user");
    assert!(rec.plans().is_empty());
    c.finish();
}

#[test]
fn caller_session_only_means_nothing_to_do() {
    let rec = Recorder::default();
    let mut c = start(true, &rec);
    call_reclaim(&mut c, 4, &[CALLER_GROUP]);
    let r = c.recv();
    assert_eq!(r["id"], 4, "no elicitation for an empty plan: {r}");
    let sc = structured(&r);
    assert_eq!(sc["status"], "nothing_to_do");
    assert_eq!(sc["refused"][0]["group_id"], CALLER_GROUP);
    assert_eq!(sc["command"], Value::Null);
    assert!(rec.plans().is_empty());
    // Invalid inputs are tool errors.
    for (id, args) in [
        (5, json!({})),
        (6, json!({"group_ids": []})),
        (7, json!({"group_ids": [1]})),
        (8, json!({"group_ids": "daemon:gradle"})),
    ] {
        c.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"reclaim","arguments":args}}));
        let r = c.recv();
        assert_eq!(r["result"]["isError"], true, "{r}");
    }
    c.finish();
}

#[test]
fn ping_is_answered_and_other_requests_queue_during_elicitation() {
    let rec = Recorder::default();
    let mut c = start(true, &rec);
    call_reclaim(&mut c, 10, &[GRADLE]);
    let req = expect_elicitation(&mut c);
    c.send(json!({"jsonrpc":"2.0","id":50,"method":"ping"}));
    let pong = c.recv();
    assert_eq!(pong["id"], 50);
    assert_eq!(pong["result"], json!({}));
    c.send(json!({"jsonrpc":"2.0","id":51,"method":"tools/list"}));
    c.send(json!({"jsonrpc":"2.0","id":"late","result":{"action":"accept","content":{"confirm":true}}}));
    answer(
        &mut c,
        &req,
        json!({"action":"accept","content":{"confirm":true}}),
    );
    let r = c.recv();
    assert_eq!(r["id"], 10, "the reclaim result comes first: {r}");
    assert_eq!(structured(&r)["status"], "done");
    let l = c.recv();
    assert_eq!(l["id"], 51, "queued request answered after: {l}");
    assert!(l["result"]["tools"].is_array());
    assert_eq!(rec.plans().len(), 1);
    c.finish();
}

#[test]
fn cancelled_call_gets_no_response_and_does_nothing() {
    let rec = Recorder::default();
    let mut c = start(true, &rec);
    call_reclaim(&mut c, 20, &[GRADLE]);
    let req = expect_elicitation(&mut c);
    c.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":20,"reason":"user pressed esc"}}));
    // A late answer to the abandoned elicitation is ignored.
    answer(
        &mut c,
        &req,
        json!({"action":"accept","content":{"confirm":true}}),
    );
    let r = c.request(21, "ping", json!({}));
    assert_eq!(
        r["result"],
        json!({}),
        "next message is the ping reply, not a reclaim result"
    );
    assert!(rec.plans().is_empty());
    c.finish();
}

#[test]
fn queued_request_cancelled_during_elicitation_is_dropped() {
    let rec = Recorder::default();
    let mut c = start(true, &rec);
    call_reclaim(&mut c, 30, &[GRADLE]);
    let req = expect_elicitation(&mut c);
    c.send(json!({"jsonrpc":"2.0","id":31,"method":"tools/list"}));
    c.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":31}}));
    answer(&mut c, &req, json!({"action":"decline"}));
    assert_eq!(c.recv()["id"], 30);
    assert_eq!(c.request(32, "ping", json!({}))["id"], 32, "31 was dropped");
    c.finish();
}

#[test]
fn timeout_and_client_error_do_nothing() {
    let rec = Recorder::default();
    let mut c = FakeClient::start(
        options(true, Some(rec.clone())),
        IoConfig {
            elicitation_timeout: Duration::from_millis(150),
        },
    );
    c.handshake(true);
    call_reclaim(&mut c, 40, &[GRADLE]);
    let _req = expect_elicitation(&mut c);
    let r = c.recv(); // no answer: the server gives up
    assert_eq!(r["id"], 40);
    let sc = structured(&r);
    assert_eq!(sc["status"], "not_confirmed");
    assert!(sc["summary"].as_str().unwrap().contains("no answer"), "{sc}");

    call_reclaim(&mut c, 41, &[GRADLE]);
    let req = expect_elicitation(&mut c);
    c.send(
        json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32601,"message":"elicitation unsupported"}}),
    );
    let r = c.recv();
    assert_eq!(r["id"], 41);
    assert_eq!(structured(&r)["status"], "not_confirmed");
    assert!(rec.plans().is_empty());
    c.finish();
}

#[test]
fn eof_during_elicitation_ends_cleanly_without_acting() {
    let rec = Recorder::default();
    let mut c = start(true, &rec);
    call_reclaim(&mut c, 45, &[GRADLE]);
    let _ = expect_elicitation(&mut c);
    // Closing stdin: the server answers the pending call (not confirmed) and exits.
    c.close_stdin();
    let r = c.recv();
    c.finish();
    assert_eq!(r["id"], 45);
    assert_eq!(r["result"]["structuredContent"]["status"], "not_confirmed");
    assert!(rec.plans().is_empty());
}

#[test]
fn target_replaced_after_confirmation_is_skipped() {
    // Sample 1 (plan + elicitation) sees the daemons; sample 2 (re-verify) sees GradleDaemon's pid reused.
    let first = fixture();
    let mut second = fixture();
    for p in &mut second.processes {
        if p.id.pid == 300 {
            p.id.start_time = 9_999;
        }
    }
    for g in &mut second.groups {
        if g.id == GRADLE {
            g.root = Some(ProcId::new(300, 9_999));
        }
    }
    let (prov, calls) = Scripted::new(vec![first, second]);
    let rec = Recorder::default();
    let mut opts = options(true, Some(rec.clone()));
    opts.provider = Box::new(prov);
    let mut c = FakeClient::start(opts, IoConfig::default());
    c.handshake(true);
    call_reclaim(&mut c, 50, &[GRADLE, KOTLIN]);
    let req = expect_elicitation(&mut c);
    answer(
        &mut c,
        &req,
        json!({"action":"accept","content":{"confirm":true}}),
    );
    let r = c.recv();
    let sc = structured(&r);
    assert_eq!(sc["status"], "partial", "{sc}");
    assert!(sc["refused"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["group_id"] == GRADLE && x["reason"].as_str().unwrap().contains("changed")));
    let plans = rec.plans();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].targets.len(), 1);
    assert_eq!(plans[0].targets[0].group_id, KOTLIN);
    assert!(
        *calls.lock().unwrap() >= 3,
        "plan, re-verify and re-measure samples"
    );
    c.finish();
}

#[test]
fn no_actuator_means_unavailable() {
    let mut opts = options(true, None);
    opts.actuator = None;
    let mut c = FakeClient::start(opts, IoConfig::default());
    c.handshake(true);
    call_reclaim(&mut c, 60, &[GRADLE]);
    let r = c.recv();
    assert_eq!(r["id"], 60, "no elicitation without an actuator: {r}");
    assert_eq!(structured(&r)["status"], "unavailable");
    c.finish();
}

// ---------------------------------------------------------------------------------------------------------
// End to end with a real child process (spawned here; nothing else is ever signalled)
// ---------------------------------------------------------------------------------------------------------

struct ChildProvider {
    child: Arc<Mutex<Child>>,
    base: Snapshot,
    history: History,
}

impl SnapshotProvider for ChildProvider {
    fn snapshot(&mut self) -> Snapshot {
        let mut s = self.base.clone();
        let pid = self.child.lock().unwrap().id();
        let exited = self.child.lock().unwrap().try_wait().ok().flatten().is_some();
        if exited {
            s.processes.retain(|p| p.id.pid != pid);
            s.memory.available.value = Some(12 * GIB); // the daemon's memory came back
        }
        s
    }
    fn history(&self) -> &History {
        &self.history
    }
}

/// Sends SIGTERM with `kill(1)` to our own child only.
struct ChildKiller {
    child_pid: u32,
    seen: Arc<Mutex<Vec<ActionPlan>>>,
}

impl Actuator for ChildKiller {
    fn execute(&mut self, plan: &ActionPlan) -> Vec<ActionOutcome> {
        self.seen.lock().unwrap().push(plan.clone());
        plan.targets
            .iter()
            .map(|t| {
                assert_eq!(t.root.pid, self.child_pid, "only our own child may be signalled");
                let ok = Command::new("kill")
                    .args(["-TERM", &t.root.pid.to_string()])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                ActionOutcome {
                    target: t.clone(),
                    ok,
                    message: if ok {
                        "SIGTERM sent".into()
                    } else {
                        "kill failed".into()
                    },
                    measured_gain: None,
                }
            })
            .collect()
    }
}

#[test]
fn accepted_reclaim_stops_a_real_child_process() {
    let child = Command::new("sleep").arg("600").spawn().expect("spawn sleep");
    let pid = child.id();
    let child = Arc::new(Mutex::new(child));
    // If an assertion fails, never leave our `sleep 600` behind (it is our own child; nothing else).
    struct Reap(Arc<Mutex<Child>>);
    impl Drop for Reap {
        fn drop(&mut self) {
            if let Ok(mut c) = self.0.lock() {
                if c.try_wait().ok().flatten().is_none() {
                    let _ = c.kill();
                    let _ = c.wait();
                }
            }
        }
    }
    let _reap = Reap(child.clone());
    let mut base = fixture();
    let st = 1_234_567u64;
    let mut p = base.processes[4].clone(); // a java-like process record, re-pointed at our child
    p.id = ProcId::new(pid, st);
    p.name = "sleep".into();
    base.processes.push(p);
    let mut g = base.group(GRADLE).unwrap().clone();
    g.id = "daemon:sleep".into();
    g.label = "sleep 600".into();
    g.root = Some(ProcId::new(pid, st));
    g.members[0].id = ProcId::new(pid, st);
    base.groups.push(g);

    let seen = Arc::new(Mutex::new(Vec::new()));
    let opts = McpOptions {
        provider: Box::new(ChildProvider {
            child: child.clone(),
            base,
            history: History::default(),
        }),
        actuator: Some(Box::new(ChildKiller {
            child_pid: pid,
            seen: seen.clone(),
        })),
        allow_actions: true,
        protect: protect(),
        model_estimator: None,
        units: Default::default(),
    };
    let mut c = FakeClient::start(opts, IoConfig::default());
    c.handshake(true);
    call_reclaim(&mut c, 70, &["daemon:sleep"]);
    let req = expect_elicitation(&mut c);
    assert!(req["params"]["message"]
        .as_str()
        .unwrap()
        .contains(&format!("pid {pid}")));
    // Still running while the user decides.
    assert!(child.lock().unwrap().try_wait().unwrap().is_none());
    answer(
        &mut c,
        &req,
        json!({"action":"accept","content":{"confirm":true}}),
    );
    let r = c.recv();
    let sc = structured(&r).clone();
    // The child really got SIGTERM.
    let status = child.lock().unwrap().wait().unwrap();
    assert!(!status.success(), "sleep was terminated by a signal: {status:?}");
    assert_eq!(sc["status"], "done", "{sc}");
    assert_eq!(sc["executed"], true);
    assert_eq!(seen.lock().unwrap().len(), 1);
    c.finish();
}

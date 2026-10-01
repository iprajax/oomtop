//! Scripted stdio sessions: initialize → initialized → tools/list → tools/call, over the real line transport.

mod support;

use oomtop_mcp::{serve_io, tool_definitions, IoConfig, PROTOCOL_VERSION};
use serde_json::{json, Value};
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};
use support::*;

#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs a whole scripted session (all input up front, EOF at the end) and returns the output lines.
fn run_script(lines: &[String], allow_actions: bool) -> Vec<Value> {
    let input = lines.join("\n") + "\n";
    let out = SharedBuf::default();
    serve_io(
        options(allow_actions, Some(Recorder::default())),
        Cursor::new(input.into_bytes()),
        out.clone(),
        &IoConfig::default(),
    )
    .expect("session ends cleanly at EOF");
    let bytes = out.0.lock().unwrap().clone();
    String::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

#[test]
fn scripted_initialize_list_call_can_fit() {
    let script = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"script","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"can_fit","arguments":{"size":"13G","label":"qwen-image"}}}),
        json!({"jsonrpc":"2.0","id":4,"method":"ping"}),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>();
    let out = run_script(&script, false);
    // The notification gets no response: 4 responses for 4 requests, in order.
    assert_eq!(out.len(), 4, "{out:#?}");
    for (r, id) in out.iter().zip([1, 2, 3, 4]) {
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], id);
    }
    let init = &out[0]["result"];
    assert_eq!(init["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(init["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(init["serverInfo"]["name"], "oomtop");
    assert!(init["instructions"].as_str().unwrap().contains("can_fit"));

    let tools = out[1]["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "get_headroom",
            "can_fit",
            "top_consumers",
            "list_groups",
            "list_model_servers",
            "list_sandboxes",
            "explain_slowdown",
            "suggest_reclaim"
        ]
    );
    for t in tools {
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{t}");
        assert_eq!(t["annotations"]["destructiveHint"], false, "{t}");
        assert_eq!(t["inputSchema"]["type"], "object");
        assert_eq!(t["outputSchema"]["type"], "object");
        assert!(t["title"].is_string() && t["description"].is_string());
    }

    // 10 GiB available − max(1.5 GiB, 8 % of 24 GiB) ≈ 8.1 GiB headroom: 13 GiB needs the idle daemons.
    let r = &out[2]["result"];
    assert_eq!(r["isError"], false, "{r}");
    assert_conforms(tools, "can_fit", r);
    let sc = &r["structuredContent"];
    assert_eq!(sc["fit"]["answer"], "yes_after_reclaim", "{sc}");
    assert_eq!(sc["exit_code"], 3);
    assert_eq!(sc["valid_for_s"], 10);
    assert_eq!(sc["advisory"], true);
    assert_eq!(sc["need"]["bytes"], 13 * GIB);
    assert_eq!(sc["need_source"], "size");
    let picked: Vec<&str> = sc["fit"]["reclaim"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["group_id"].as_str().unwrap())
        .collect();
    assert!(picked.contains(&GRADLE) && picked.contains(&KOTLIN), "{picked:?}");
    assert!(
        !picked.contains(&CALLER_GROUP),
        "never reclaim the calling agent's session: {picked:?}"
    );
    assert_eq!(out[3]["result"], json!({}));
}

#[test]
fn can_fit_answers_yes_and_no_with_exit_codes() {
    let mut c = FakeClient::start(options(false, None), IoConfig::default());
    c.handshake(false);
    let tools = tool_definitions(false);
    let yes = c.call(1, "can_fit", json!({"bytes": GIB}));
    assert_conforms(&tools, "can_fit", &yes["result"]);
    assert_eq!(yes["result"]["structuredContent"]["fit"]["answer"], "yes");
    assert_eq!(yes["result"]["structuredContent"]["exit_code"], 0);
    let no = c.call(2, "can_fit", json!({"size": "40G"}));
    assert_conforms(&tools, "can_fit", &no["result"]);
    assert_eq!(no["result"]["structuredContent"]["fit"]["answer"], "no");
    assert_eq!(no["result"]["structuredContent"]["exit_code"], 4);
    assert!(
        no["result"]["structuredContent"]["fit"]["shortfall"]
            .as_u64()
            .unwrap()
            > 0
    );
    // model_path goes through the injected (header-only) estimator: 8 GiB.
    let m = c.call(3, "can_fit", json!({"model_path": "/models/qwen.gguf"}));
    assert_conforms(&tools, "can_fit", &m["result"]);
    let sc = &m["result"]["structuredContent"];
    assert_eq!(sc["need"]["bytes"], 8 * GIB);
    assert_eq!(sc["need"]["label"], "qwen.gguf");
    assert_eq!(sc["need_source"], "model_path");
    // Input errors are tool errors the model can read and fix.
    for (id, args) in [
        (4, json!({})),
        (5, json!({"bytes": 1, "size": "1G"})),
        (6, json!({"bytes": -5})),
        (7, json!({"size": "lots"})),
        (8, json!({"model_path": "/etc/hosts"})),
        (9, json!({"bytes": 1, "surprise": true})),
    ] {
        let r = c.call(id, "can_fit", args.clone());
        assert_eq!(r["result"]["isError"], true, "{args} → {r}");
        assert!(r["result"]["content"][0]["text"].is_string());
    }
    c.finish();
}

#[test]
fn every_tool_conforms_to_its_output_schema_and_is_redacted() {
    let mut c = FakeClient::start(options(true, Some(Recorder::default())), IoConfig::default());
    c.handshake(false);
    let list = c.request(1, "tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap().clone();
    assert!(tools.iter().any(|t| t["name"] == "reclaim"));
    let calls = [
        ("get_headroom", json!({})),
        ("can_fit", json!({"bytes": 13 * GIB, "gpu_resident": true})),
        ("top_consumers", json!({})),
        ("top_consumers", json!({"by": "gpu", "n": 3})),
        ("top_consumers", json!({"by": "cpu", "n": 100})),
        ("list_groups", json!({})),
        ("list_groups", json!({"kind": "daemon"})),
        ("list_groups", json!({"kind": "agent_session", "limit": 1})),
        ("list_model_servers", json!({})),
        ("list_sandboxes", json!({})),
        ("explain_slowdown", json!({})),
        ("suggest_reclaim", json!({})),
        ("reclaim", json!({"group_ids": [GRADLE]})),
    ];
    let mut all_text = String::new();
    for (i, (tool, args)) in calls.iter().enumerate() {
        let r = c.call(10 + i as u64, tool, args.clone());
        assert!(r.get("error").is_none(), "{tool}: {r}");
        assert_eq!(r["result"]["isError"], false, "{tool}: {r}");
        assert_conforms(&tools, tool, &r["result"]);
        all_text.push_str(&r.to_string());
    }
    assert!(!all_text.contains("s3cr3t"), "command-line secret leaked");
    assert!(!all_text.contains("hunter2"), "endpoint credential leaked");
    c.finish();
}

#[test]
fn tool_results_have_the_expected_content() {
    let mut c = FakeClient::start(options(false, None), IoConfig::default());
    c.handshake(false);

    let h = c.call(1, "get_headroom", json!({}));
    let sc = &h["result"]["structuredContent"];
    assert_eq!(sc["headroom"]["available_now"]["value"], 10 * GIB);
    assert_eq!(sc["headroom"]["available_now"]["source"], "vm_statistics64");
    assert!(sc["headroom"]["headroom"].as_i64().unwrap() > 0);
    assert_eq!(sc["swap"]["used"]["value"], 2 * GIB);
    assert!(sc["summary"].as_str().unwrap().len() > 5);

    let top = c.call(2, "top_consumers", json!({"n": 2}));
    let g = &top["result"]["structuredContent"]["groups"];
    assert_eq!(g.as_array().unwrap().len(), 2);
    assert_eq!(g[0]["id"], SD);
    assert_eq!(g[0]["footprint"]["value"], 10 * GIB);
    assert!(g[0].get("members").is_none(), "summaries omit member lists");

    let gpu = c.call(3, "top_consumers", json!({"by": "gpu"}));
    let g = gpu["result"]["structuredContent"]["groups"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(g.len(), 1, "only groups holding GPU memory");
    assert_eq!(g[0]["id"], SD);

    let daemons = c.call(4, "list_groups", json!({"kind": "daemon"}));
    let sc = &daemons["result"]["structuredContent"];
    assert_eq!(sc["total"], 2);
    assert_eq!(sc["kind"], "build_daemon");
    let limited = c.call(5, "list_groups", json!({"limit": 3}));
    assert_eq!(limited["result"]["structuredContent"]["truncated"], true);
    assert_eq!(limited["result"]["structuredContent"]["returned"], 3);
    let caller_row = c.call(6, "list_groups", json!({"kind": "agent"}));
    let rows = caller_row["result"]["structuredContent"]["groups"]
        .as_array()
        .unwrap()
        .clone();
    let me = rows.iter().find(|r| r["id"] == CALLER_GROUP).unwrap();
    assert_eq!(me["is_caller"], true);
    assert_eq!(me["reclaim_candidate"], false);

    let ms = c.call(7, "list_model_servers", json!({}));
    let sc = &ms["result"]["structuredContent"];
    assert_eq!(sc["count"], 1);
    assert_eq!(sc["model_servers"][0]["pids"], json!([400]));
    assert_eq!(sc["model_servers"][0]["group"]["id"], SD);
    assert!(sc["model_servers"][0]["endpoint"]
        .as_str()
        .unwrap()
        .contains("127.0.0.1"));

    let sb = c.call(8, "list_sandboxes", json!({}));
    let sc = &sb["result"]["structuredContent"];
    assert_eq!(sc["sandboxes"][0]["host_cost"]["value"], GIB);
    assert_eq!(
        sc["sandboxes"][0]["host_cost"]["quality"], "estimate",
        "VM cost is a lower bound"
    );
    assert_eq!(sc["sandboxes"][0]["group"]["id"], VM);

    let why = c.call(9, "explain_slowdown", json!({}));
    assert!(why["result"]["structuredContent"]["causes"].is_array());

    let sr = c.call(10, "suggest_reclaim", json!({}));
    let sc = &sr["result"]["structuredContent"];
    let ids: Vec<&str> = sc["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["group_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [GRADLE, KOTLIN, OTHER_AGENT],
        "largest gain first, caller excluded"
    );
    assert!(sc["excluded"].as_array().unwrap().contains(&json!(CALLER_GROUP)));
    assert_eq!(sc["side_effects"], false);
    assert_eq!(
        sc["command"],
        format!("oomtop reclaim --groups {GRADLE},{KOTLIN},{OTHER_AGENT}")
    );
    assert_eq!(
        sc["total_gain"],
        2900 * MIB + 2800 * MIB + 380 * MIB,
        "RAM gain only"
    );
    c.finish();
}

#[test]
fn protocol_errors_and_lifecycle() {
    let mut c = FakeClient::start(options(false, None), IoConfig::default());
    // Before initialize: only ping and initialize are served.
    let r = c.request(1, "tools/list", json!({}));
    assert_eq!(r["error"]["code"], -32600, "{r}");
    let r = c.request(2, "ping", json!({}));
    assert_eq!(r["result"], json!({}));
    // Old protocol versions are echoed; unknown ones get ours.
    let r = c.request(
        3,
        "initialize",
        json!({"protocolVersion":"1999-01-01","capabilities":{"elicitation":{}}}),
    );
    assert_eq!(r["result"]["protocolVersion"], PROTOCOL_VERSION);
    let r = c.request(4, "initialize", json!({"protocolVersion":"2025-06-18"}));
    assert_eq!(r["error"]["code"], -32600, "second initialize is refused: {r}");
    c.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    // Unknown method / tool, reclaim not registered without --allow-actions.
    assert_eq!(c.request(5, "resources/list", json!({}))["error"]["code"], -32601);
    let r = c.request(6, "tools/call", json!({"name":"nope"}));
    assert_eq!(r["error"]["code"], -32602);
    let r = c.request(
        7,
        "tools/call",
        json!({"name":"reclaim","arguments":{"group_ids":[GRADLE]}}),
    );
    assert_eq!(
        r["error"]["code"], -32602,
        "reclaim is not registered read-only: {r}"
    );
    let r = c.request(8, "tools/call", json!({"arguments":{}}));
    assert_eq!(r["error"]["code"], -32602);
    let r = c.request(9, "tools/call", json!({"name":"get_headroom","arguments":[1]}));
    assert_eq!(r["error"]["code"], -32602);
    let r = c.request(
        10,
        "tools/call",
        json!({"name":"top_consumers","arguments":{"by":"ram"}}),
    );
    assert_eq!(r["result"]["isError"], true);
    let r = c.request(
        11,
        "tools/call",
        json!({"name":"list_groups","arguments":{"kind":"spaceship"}}),
    );
    assert_eq!(r["result"]["isError"], true);
    // Garbage lines, batches and stray responses.
    c.send_raw("{not json");
    let r = c.recv();
    assert_eq!(r["error"]["code"], -32700);
    assert_eq!(r["id"], Value::Null);
    c.send_raw("[{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"ping\"}]");
    assert_eq!(c.recv()["error"]["code"], -32600);
    c.send_raw("");
    c.send(json!({"jsonrpc":"2.0","id":"x","result":{}})); // response to nothing: ignored
    c.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":99}}));
    let r = c.request(13, "ping", json!({}));
    assert_eq!(r["result"], json!({}), "stray messages produced no output");
    c.finish();
}

#[test]
fn old_protocol_version_is_echoed() {
    let mut c = FakeClient::start(options(false, None), IoConfig::default());
    let r = c.request(
        1,
        "initialize",
        json!({"protocolVersion":"2025-03-26","capabilities":{}}),
    );
    assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
    c.finish();
}

#[test]
fn answers_fast_on_a_large_snapshot() {
    // SPEC §14: a call answers in < 400 ms (live sampling adds ~250 ms); the MCP layer itself must be cheap.
    let mut s = fixture();
    for i in 0..700u32 {
        let mut p = s.processes[1].clone();
        p.id = oomtop_core::ProcId::new(10_000 + i, 1);
        s.processes.push(p);
    }
    for i in 0..150u32 {
        let mut g = s.groups[3].clone();
        g.id = format!("app:bulk{i}");
        s.groups.push(g);
    }
    let mut opts = options(false, None);
    opts.provider = Box::new(oomtop_core::provider::StaticProvider::new(s));
    let mut sv = oomtop_mcp::Server::new(opts);
    sv.handle(
        &json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
    );
    for (i, tool) in ["get_headroom", "suggest_reclaim", "list_groups", "top_consumers"]
        .iter()
        .enumerate()
    {
        // Best of three: several builds share this fanless machine, so one slow run is scheduling noise,
        // not MCP cost. A real regression is slow every time.
        let mut best = u128::MAX;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let r = sv
                .handle(&json!({"jsonrpc":"2.0","id":i+1,"method":"tools/call","params":{"name":tool}}))
                .unwrap();
            best = best.min(t.elapsed().as_millis());
            assert_eq!(r["result"]["isError"], false);
        }
        assert!(best < 150, "{tool} took {best} ms (best of 3)");
    }
}

#[test]
fn samples_only_on_tool_calls() {
    let (prov, calls) = Scripted::new(vec![fixture()]);
    let mut opts = options(false, None);
    opts.provider = Box::new(prov);
    let mut c = FakeClient::start(opts, IoConfig::default());
    c.handshake(false);
    c.request(1, "tools/list", json!({}));
    c.request(2, "ping", json!({}));
    assert_eq!(*calls.lock().unwrap(), 0, "no sampling while idle");
    c.call(3, "get_headroom", json!({}));
    assert_eq!(*calls.lock().unwrap(), 1, "exactly one sample per call");
    c.call(4, "can_fit", json!({"size": "1G"}));
    assert_eq!(*calls.lock().unwrap(), 2);
    c.finish();
}

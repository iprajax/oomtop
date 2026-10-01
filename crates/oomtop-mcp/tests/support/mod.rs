//! Shared fixtures for the MCP integration tests: a snapshot modelled on the M5 Air acceptance machine,
//! a recording actuator, a fake MCP client over OS pipes, and a small JSON-Schema subset validator.
#![allow(dead_code)]

use oomtop_core::actions::{ActionOutcome, ActionPlan, Actuator, ProtectContext};
use oomtop_core::history::History;
use oomtop_core::provider::{SnapshotProvider, StaticProvider};
use oomtop_core::{
    Group, GroupKind, GroupTotals, Measured, Member, ModelServer, ModelServerKind, ProcId, Process, Sandbox,
    SandboxKind, Snapshot,
};
use oomtop_mcp::{serve_io, IoConfig, McpOptions};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub const GIB: u64 = 1 << 30;
pub const MIB: u64 = 1 << 20;

/// pid of the MCP server process in the fixture; its parent (100) is the calling agent.
pub const MCP_PID: u32 = 200;
pub const CALLER_GROUP: &str = "agent:caller000000";
pub const OTHER_AGENT: &str = "agent:other0000000";
pub const GRADLE: &str = "daemon:gradle";
pub const KOTLIN: &str = "daemon:kotlin";
pub const SD: &str = "model:sd-server";
pub const CHROME: &str = "app:google-chrome";
pub const VM: &str = "sandbox:vm";
pub const SYSTEM: &str = "system:launchd";

fn proc(pid: u32, ppid: u32, name: &str, cmd: &[&str], footprint: u64) -> Process {
    let mut p = Process {
        id: ProcId::new(pid, 1_000 + pid as u64),
        ppid: Some(ppid),
        name: name.into(),
        exe: format!("/usr/bin/{name}"),
        cmdline: cmd.iter().map(|s| s.to_string()).collect(),
        uid: Some(501),
        cpu_pct: Measured::exact(0.1, "test"),
        ..Default::default()
    };
    p.mem.footprint_or_pss = Measured::exact(footprint, "proc_pid_rusage.ri_phys_footprint");
    p.mem.resident = Measured::exact(footprint, "test");
    p
}

#[allow(clippy::too_many_arguments)]
fn group(
    id: &str,
    kind: GroupKind,
    label: &str,
    root: u32,
    members: &[u32],
    footprint: u64,
    gain: Option<u64>,
    idle: bool,
) -> Group {
    Group {
        id: id.into(),
        kind,
        label: label.into(),
        root: Some(ProcId::new(root, 1_000 + root as u64)),
        members: members
            .iter()
            .map(|p| Member {
                id: ProcId::new(*p, 1_000 + *p as u64),
                ..Default::default()
            })
            .collect(),
        totals: GroupTotals {
            footprint: Measured::exact(footprint, "Σ footprint"),
            resident: Measured::exact(footprint, "Σ resident"),
            cpu_pct: Measured::exact(if idle { 0.0 } else { 12.5 }, "Σ cpu"),
            gpu: Measured::exact(0, "test"),
            process_count: members.len() as u32,
            ..Default::default()
        },
        reclaim_gain: gain
            .map(|g| Measured::estimate(g, "private resident"))
            .unwrap_or_default(),
        idle,
        idle_for_s: idle.then_some(3 * 3600),
        ..Default::default()
    }
}

/// The acceptance machine: 24 GB, sd-server holding ~10 GB of Metal, two idle build daemons, Chrome, a VM,
/// the calling agent session (root 100 → MCP server 200) and a second agent session.
pub fn fixture() -> Snapshot {
    let mut s = Snapshot {
        taken_at_ms: 1_790_000_000_000,
        self_pid: Some(MCP_PID),
        ..Default::default()
    };
    s.host.mem_total = 24 * GIB;
    s.host.unified_memory = true;
    s.memory.total = Measured::exact(24 * GIB, "hw.memsize");
    s.memory.available = Measured::exact(10 * GIB, "vm_statistics64");
    s.memory.swap_used = Measured::exact(2 * GIB, "vm.swapusage");
    s.memory.swap_total = Measured::exact(4 * GIB, "vm.swapusage");
    s.memory.pressure = Measured::exact(oomtop_core::PressureLevel::Normal, "memorystatus");
    s.processes = vec![
        proc(1, 0, "launchd", &["/sbin/launchd"], 20 * MIB),
        proc(100, 1, "claude", &["claude"], 400 * MIB),
        proc(MCP_PID, 100, "oomtop", &["oomtop", "mcp"], 12 * MIB),
        proc(150, 1, "claude", &["claude", "--resume"], 380 * MIB),
        proc(300, 1, "java", &["java", "GradleDaemon"], 3 * GIB),
        proc(301, 1, "java", &["java", "KotlinCompileDaemon"], 3 * GIB),
        proc(
            400,
            1,
            "sd-server",
            &["sd-server", "--api-key", "s3cr3t", "--listen-port", "7861"],
            10 * GIB,
        ),
        proc(500, 1, "Google Chrome", &["Google Chrome"], 2 * GIB),
        proc(
            600,
            1,
            "com.apple.Virtualization.VirtualMachine",
            &["com.apple.Virtualization.VirtualMachine"],
            GIB,
        ),
    ];
    let mut caller = group(
        CALLER_GROUP,
        GroupKind::AgentSession,
        "Claude Code",
        100,
        &[100, MCP_PID],
        412 * MIB,
        Some(400 * MIB),
        true,
    );
    // Contrived: the caller's session is flagged orphan so it *would* be a reclaim candidate.
    caller.orphan = true;
    let mut other = group(
        OTHER_AGENT,
        GroupKind::AgentSession,
        "Claude Code",
        150,
        &[150],
        380 * MIB,
        Some(380 * MIB),
        true,
    );
    other.orphan = true;
    let mut sd = group(
        SD,
        GroupKind::ModelServer,
        "sd-server",
        400,
        &[400],
        10 * GIB,
        Some(9 * GIB),
        false,
    );
    sd.totals.gpu = Measured::exact(9 * GIB + 900 * MIB, "ioreg.AGXAccelerator");
    let mut vm = group(VM, GroupKind::Sandbox, "Claude VM", 600, &[600], GIB, None, false);
    vm.lower_bound = true;
    let mut sys = group(
        SYSTEM,
        GroupKind::System,
        "launchd",
        1,
        &[1],
        20 * MIB,
        None,
        false,
    );
    sys.protected = true;
    s.groups = vec![
        sd,
        group(
            GRADLE,
            GroupKind::BuildDaemon,
            "GradleDaemon",
            300,
            &[300],
            3 * GIB,
            Some(2900 * MIB),
            true,
        ),
        group(
            KOTLIN,
            GroupKind::BuildDaemon,
            "KotlinCompileDaemon",
            301,
            &[301],
            3 * GIB,
            Some(2800 * MIB),
            true,
        ),
        group(
            CHROME,
            GroupKind::App,
            "Google Chrome",
            500,
            &[500],
            2 * GIB,
            Some(2 * GIB),
            false,
        ),
        vm,
        caller,
        other,
        sys,
    ];
    s.model_servers = vec![ModelServer {
        id: "sd-server:7861".into(),
        kind: ModelServerKind::SdCpp,
        endpoint: Some("http://user:hunter2@127.0.0.1:7861".into()),
        pids: vec![ProcId::new(400, 1_400)],
        group_id: Some(SD.into()),
        ..Default::default()
    }];
    s.sandboxes = vec![Sandbox {
        id: "vm:claude".into(),
        kind: SandboxKind::Vm,
        runtime: "virtualization.framework".into(),
        label: "Claude VM".into(),
        host_pids: vec![ProcId::new(600, 1_600)],
        configured_mem: Measured::exact(4 * GIB, "vm config"),
        footprint_lower_bound: true,
        ..Default::default()
    }];
    s
}

/// Protection context as the CLI builds it for `oomtop mcp` (ancestors of the MCP process).
pub fn protect() -> ProtectContext {
    ProtectContext {
        self_pid: Some(MCP_PID),
        self_uid: Some(501),
        ancestor_pids: vec![100],
        ..Default::default()
    }
}

/// Records every plan it is asked to execute; reports success.
#[derive(Clone, Default)]
pub struct Recorder(pub Arc<Mutex<Vec<ActionPlan>>>);

impl Recorder {
    pub fn plans(&self) -> Vec<ActionPlan> {
        self.0.lock().unwrap().clone()
    }
}

impl Actuator for Recorder {
    fn execute(&mut self, plan: &ActionPlan) -> Vec<ActionOutcome> {
        self.0.lock().unwrap().push(plan.clone());
        plan.targets
            .iter()
            .map(|t| ActionOutcome {
                target: t.clone(),
                ok: true,
                message: "SIGTERM sent".into(),
                measured_gain: None,
            })
            .collect()
    }
}

/// A provider that serves snapshots in order, repeating the last one, and counts calls.
pub struct Scripted {
    pub snaps: Vec<Snapshot>,
    pub calls: Arc<Mutex<usize>>,
    pub history: History,
}

impl Scripted {
    pub fn new(snaps: Vec<Snapshot>) -> (Self, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0));
        (
            Scripted {
                snaps,
                calls: calls.clone(),
                history: History::default(),
            },
            calls,
        )
    }
}

impl SnapshotProvider for Scripted {
    fn snapshot(&mut self) -> Snapshot {
        let mut n = self.calls.lock().unwrap();
        let s = self
            .snaps
            .get(*n)
            .or(self.snaps.last())
            .cloned()
            .unwrap_or_default();
        *n += 1;
        s
    }
    fn history(&self) -> &History {
        &self.history
    }
}

pub fn options(allow_actions: bool, actuator: Option<Recorder>) -> McpOptions {
    McpOptions {
        provider: Box::new(StaticProvider::new(fixture())),
        actuator: actuator.map(|a| Box::new(a) as Box<dyn Actuator>),
        allow_actions,
        protect: protect(),
        model_estimator: Some(Box::new(|p: &str| {
            if p.ends_with(".gguf") {
                Ok(8 * GIB)
            } else {
                Err("not a model file".into())
            }
        })),
        units: Default::default(),
    }
}

/// A fake MCP client connected to `serve_io` through OS pipes.
pub struct FakeClient {
    to_server: Option<std::io::PipeWriter>,
    from_server: BufReader<std::io::PipeReader>,
    server: Option<JoinHandle<Result<(), oomtop_mcp::McpError>>>,
}

impl FakeClient {
    pub fn start(opts: McpOptions, cfg: IoConfig) -> Self {
        let (srv_in_r, srv_in_w) = std::io::pipe().unwrap();
        let (srv_out_r, srv_out_w) = std::io::pipe().unwrap();
        let server = std::thread::spawn(move || serve_io(opts, BufReader::new(srv_in_r), srv_out_w, &cfg));
        FakeClient {
            to_server: Some(srv_in_w),
            from_server: BufReader::new(srv_out_r),
            server: Some(server),
        }
    }

    pub fn send(&mut self, v: Value) {
        let w = self.to_server.as_mut().expect("stdin open");
        writeln!(w, "{v}").unwrap();
        w.flush().unwrap();
    }

    pub fn send_raw(&mut self, line: &str) {
        let w = self.to_server.as_mut().expect("stdin open");
        writeln!(w, "{line}").unwrap();
        w.flush().unwrap();
    }

    /// Next message from the server (blocks; panics after EOF).
    pub fn recv(&mut self) -> Value {
        let mut line = String::new();
        let n = self.from_server.read_line(&mut line).unwrap();
        assert!(n > 0, "server closed stdout");
        assert!(line.ends_with('\n'), "one message per line: {line:?}");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad json from server: {e}: {line}"))
    }

    pub fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let r = self.recv();
        assert_eq!(r["id"], json!(id), "{r}");
        r
    }

    /// initialize (with or without the elicitation capability) + notifications/initialized.
    pub fn handshake(&mut self, elicitation: bool) -> Value {
        let caps = if elicitation {
            json!({ "elicitation": {} })
        } else {
            json!({})
        };
        let r = self.request(
            0,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": caps,
                "clientInfo": { "name": "fake-client", "version": "0.0.1" }
            }),
        );
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        r
    }

    pub fn call(&mut self, id: u64, tool: &str, args: Value) -> Value {
        self.send(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":args}}),
        );
        id_response(self, id)
    }

    /// Closes the server's stdin (EOF) without waiting.
    pub fn close_stdin(&mut self) {
        drop(self.to_server.take());
    }

    /// Closes the server's stdin and waits for it to exit cleanly.
    pub fn finish(mut self) {
        drop(self.to_server.take());
        let mut rest = String::new();
        let _ = std::io::Read::read_to_string(&mut self.from_server, &mut rest);
        assert!(rest.trim().is_empty(), "unexpected trailing output: {rest}");
        self.server
            .take()
            .unwrap()
            .join()
            .expect("server thread panicked")
            .expect("server returned an error");
    }
}

fn id_response(c: &mut FakeClient, id: u64) -> Value {
    let r = c.recv();
    assert_eq!(r["id"], json!(id), "expected the response to {id}, got {r}");
    r
}

/// Waits (bounded) for the server thread to finish, for tests that close the pipe early.
pub fn wait_a_bit() {
    std::thread::sleep(Duration::from_millis(20));
}

// ---------------------------------------------------------------------------------------------------------
// JSON-Schema subset validator (type, properties, required, items, enum, minimum, maximum)
// ---------------------------------------------------------------------------------------------------------

fn type_ok(ty: &str, v: &Value) -> bool {
    match ty {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        other => panic!("validator: unsupported type {other}"),
    }
}

/// Validates `v` against `schema`; returns every violation with its JSON path.
pub fn validate(schema: &Value, v: &Value, path: &str, errors: &mut Vec<String>) {
    if let Some(t) = schema.get("type") {
        let ok = match t {
            Value::String(s) => type_ok(s, v),
            Value::Array(a) => a.iter().any(|t| type_ok(t.as_str().unwrap(), v)),
            _ => panic!("validator: bad type {t}"),
        };
        if !ok {
            errors.push(format!("{path}: expected {t}, got {v}"));
            return;
        }
    }
    if let Some(e) = schema.get("enum").and_then(Value::as_array) {
        if !e.contains(v) {
            errors.push(format!("{path}: {v} not in {e:?}"));
        }
    }
    if let (Some(min), Some(n)) = (schema.get("minimum").and_then(Value::as_f64), v.as_f64()) {
        if n < min {
            errors.push(format!("{path}: {n} < minimum {min}"));
        }
    }
    if let (Some(max), Some(n)) = (schema.get("maximum").and_then(Value::as_f64), v.as_f64()) {
        if n > max {
            errors.push(format!("{path}: {n} > maximum {max}"));
        }
    }
    if let Some(o) = v.as_object() {
        if let Some(req) = schema.get("required").and_then(Value::as_array) {
            for r in req {
                let r = r.as_str().unwrap();
                if !o.contains_key(r) {
                    errors.push(format!("{path}: missing required {r:?}"));
                }
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (k, sub) in props {
                if let Some(x) = o.get(k) {
                    validate(sub, x, &format!("{path}.{k}"), errors);
                }
            }
        }
    }
    if let (Some(items), Some(a)) = (schema.get("items"), v.as_array()) {
        for (i, x) in a.iter().enumerate() {
            validate(items, x, &format!("{path}[{i}]"), errors);
        }
    }
}

/// Asserts that a tool result conforms to the tool's declared outputSchema.
pub fn assert_conforms(tools: &[Value], tool: &str, result: &Value) {
    let def = tools
        .iter()
        .find(|t| t["name"] == tool)
        .unwrap_or_else(|| panic!("tool {tool} not listed"));
    let sc = &result["structuredContent"];
    assert!(sc.is_object(), "{tool}: structuredContent missing: {result}");
    let mut errors = Vec::new();
    validate(&def["outputSchema"], sc, tool, &mut errors);
    assert!(
        errors.is_empty(),
        "{tool} output violates its schema:\n{}",
        errors.join("\n")
    );
    // Text copy for older clients is the same JSON.
    let text = result["content"][0]["text"].as_str().expect("text content");
    let parsed: Value = serde_json::from_str(text).expect("text content is JSON");
    assert_eq!(&parsed, sc, "{tool}: text copy differs from structuredContent");
}

//! A fixture-backed MCP server for interop testing with real MCP clients (e.g. the MCP Python SDK or the
//! MCP Inspector) without touching this machine: the snapshot is synthetic and the actuator only records
//! what it would do (to stderr). Nothing is ever signalled.
//!
//! ```sh
//! cargo run -p oomtop-mcp --example fixture_server -- [--allow-actions]
//! ```

use oomtop_core::actions::{ActionOutcome, ActionPlan, Actuator, ProtectContext};
use oomtop_core::provider::StaticProvider;
use oomtop_core::{Group, GroupKind, GroupTotals, Measured, Member, ProcId, Process, Snapshot};
use oomtop_mcp::{serve_stdio, McpOptions};

const GIB: u64 = 1 << 30;

/// Prints the plan it was given and reports success without signalling anything.
struct DryRun;

impl Actuator for DryRun {
    fn execute(&mut self, plan: &ActionPlan) -> Vec<ActionOutcome> {
        plan.targets
            .iter()
            .map(|t| {
                eprintln!(
                    "fixture_server: would SIGTERM {} (pid {}, confirmed={})",
                    t.group_id, t.root.pid, plan.confirmed
                );
                ActionOutcome {
                    target: t.clone(),
                    ok: true,
                    message: "dry run: nothing signalled".into(),
                    measured_gain: None,
                }
            })
            .collect()
    }
}

fn group(id: &str, kind: GroupKind, label: &str, pid: u32, fp: u64, idle: bool) -> Group {
    Group {
        id: id.into(),
        kind,
        label: label.into(),
        root: Some(ProcId::new(pid, 1)),
        members: vec![Member {
            id: ProcId::new(pid, 1),
            ..Default::default()
        }],
        totals: GroupTotals {
            footprint: Measured::exact(fp, "fixture"),
            cpu_pct: Measured::exact(0.0, "fixture"),
            process_count: 1,
            ..Default::default()
        },
        reclaim_gain: Measured::estimate(fp * 9 / 10, "fixture"),
        idle,
        idle_for_s: idle.then_some(3 * 3600),
        ..Default::default()
    }
}

fn snapshot() -> Snapshot {
    let mut s = Snapshot {
        taken_at_ms: 1_790_000_000_000,
        self_pid: Some(20),
        ..Default::default()
    };
    s.host.mem_total = 24 * GIB;
    s.memory.total = Measured::exact(24 * GIB, "fixture");
    s.memory.available = Measured::exact(10 * GIB, "fixture");
    s.memory.swap_used = Measured::exact(GIB, "fixture");
    s.memory.swap_total = Measured::exact(4 * GIB, "fixture");
    s.processes = [(10, 1), (20, 10), (300, 1), (301, 1), (400, 1)]
        .into_iter()
        .map(|(pid, ppid)| Process {
            id: ProcId::new(pid, 1),
            ppid: Some(ppid),
            uid: Some(501),
            ..Default::default()
        })
        .collect();
    s.groups = vec![
        group(
            "model:sd-server",
            GroupKind::ModelServer,
            "sd-server",
            400,
            10 * GIB,
            false,
        ),
        group(
            "daemon:gradle",
            GroupKind::BuildDaemon,
            "GradleDaemon",
            300,
            3 * GIB,
            true,
        ),
        group(
            "daemon:kotlin",
            GroupKind::BuildDaemon,
            "KotlinCompileDaemon",
            301,
            3 * GIB,
            true,
        ),
        group(
            "agent:caller",
            GroupKind::AgentSession,
            "Claude Code",
            10,
            GIB / 2,
            true,
        ),
    ];
    s.groups[3].members.push(Member {
        id: ProcId::new(20, 1),
        ..Default::default()
    });
    s
}

fn main() {
    let allow_actions = std::env::args().any(|a| a == "--allow-actions");
    let opts = McpOptions {
        provider: Box::new(StaticProvider::new(snapshot())),
        actuator: Some(Box::new(DryRun)),
        allow_actions,
        protect: ProtectContext {
            self_pid: Some(20),
            self_uid: Some(501),
            ..Default::default()
        },
        model_estimator: None,
        units: Default::default(),
    };
    if let Err(e) = serve_stdio(opts) {
        eprintln!("fixture_server: {e}");
        std::process::exit(1);
    }
}

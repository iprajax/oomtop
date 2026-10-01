//! Shared fixtures for the attribution golden / acceptance tests (`tests/attr_*.rs`).
//!
//! The rule set mirrors the relevant subset of `oomtop-detect/src/builtin.toml` (core cannot depend on
//! detect). Machines are modelled on the development Mac of SPEC §17 (four Claude Code sessions, Claude
//! desktop + its Virtualization.framework VM, idle Gradle/Kotlin daemons, the Metal-backed sd-server) and on
//! a typical Linux desktop (systemd --user, GNOME app scopes, docker, tmux).

#![allow(dead_code)]

use oomtop_core::actions::ProtectContext;
use oomtop_core::attribution::{attribute, AttributionContext, CompiledRules, LineageEntry, RuleSet};
use oomtop_core::idle::{IdleConfig, IdleTracker};
use oomtop_core::measured::sum_bytes;
use oomtop_core::redact::hash_marker;
use oomtop_core::{Group, Markers, Measured, MemBreakdown, ProcId, Process, Snapshot};
use serde::Serialize;
use std::collections::BTreeMap;

pub const GIB: u64 = 1 << 30;
pub const MIB: u64 = 1 << 20;
/// "now" of every fixture: 2026-09-29 12:00:00 UTC.
pub const NOW_MS: u64 = 1_790_000_000_000;
pub const MIN_MS: u64 = 60_000;

pub const RULES_JSON: &str = r#"{ "group": [
  { "id": "builtin:claude-code", "kind": "agent_session", "label": "Claude Code",
    "match": { "exe": ["claude", "**/.local/share/claude/versions/*"], "name": ["claude"],
               "script": ["**/@anthropic-ai/claude-code/**"],
               "env": ["CLAUDECODE", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_ENTRYPOINT"] },
    "session_key": "env:CLAUDE_CODE_SESSION_ID" },
  { "id": "builtin:codex", "kind": "agent_session", "label": "Codex",
    "match": { "exe": ["codex"], "script": ["**/@openai/codex/**"], "env": ["CODEX_SANDBOX", "CODEX_SESSION_ID"] },
    "session_key": "env:CODEX_SESSION_ID" },
  { "id": "builtin:ollama", "kind": "model_server", "label": "Ollama", "match": { "exe": ["ollama"] } },
  { "id": "builtin:sd-cpp", "kind": "model_server", "label": "sd-server", "match": { "exe": ["sd-server"] } },
  { "id": "builtin:gradle-daemon", "kind": "build_daemon", "label": "GradleDaemon",
    "match": { "script": ["org.gradle.launcher.daemon.bootstrap.GradleDaemon"] } },
  { "id": "builtin:kotlin-daemon", "kind": "build_daemon", "label": "KotlinCompileDaemon",
    "match": { "script": ["org.jetbrains.kotlin.daemon.KotlinCompileDaemon"] } },
  { "id": "builtin:tsserver", "kind": "build_daemon", "label": "tsserver", "match": { "script": ["**/tsserver.js"] } },
  { "id": "builtin:firefox", "kind": "app", "label": "Firefox",
    "match": { "exe": ["firefox", "firefox-bin"] }, "exclude": { "exe": ["**/*.app/**"] } },
  { "id": "builtin:kernel-task", "kind": "system", "label": "kernel_task", "match": { "name": ["kernel_task"] }, "protected": true },
  { "id": "builtin:launchd", "kind": "system", "label": "launchd", "match": { "name": ["launchd"] }, "protected": true },
  { "id": "builtin:window-server", "kind": "system", "label": "WindowServer", "match": { "name": ["WindowServer"] }, "protected": true },
  { "id": "builtin:systemd", "kind": "system", "label": "systemd", "match": { "name": ["systemd"] }, "protected": true }
] }"#;

pub fn rules() -> CompiledRules {
    let set: RuleSet = serde_json::from_str(RULES_JSON).expect("rules json");
    CompiledRules::compile(&set).expect("rules compile")
}

/// A Claude Code session marker set, as `markers_from_env` would build it.
pub fn claude_markers(session: &str) -> Markers {
    Markers {
        session_id: Some(hash_marker(session)),
        agent: Some("claude-code".into()),
        keys: vec![
            "CLAUDECODE".into(),
            "CLAUDE_CODE_ENTRYPOINT".into(),
            "CLAUDE_CODE_SESSION_ID".into(),
        ],
    }
}

/// Fluent process builder.
#[derive(Clone)]
pub struct P(pub Process);

impl P {
    /// `age_min` = minutes since the process started (start_time = NOW − age).
    pub fn new(pid: u32, ppid: u32, age_min: u64, exe: &str) -> P {
        let name = exe.rsplit('/').next().unwrap_or(exe).to_string();
        P(Process {
            id: ProcId::new(pid, NOW_MS - age_min * MIN_MS),
            ppid: Some(ppid),
            name,
            exe: exe.into(),
            cmdline: vec![exe.into()],
            uid: Some(501),
            cwd: Some("/".into()),
            cpu_pct: Measured::exact(0.0, "proc_pid_rusage.ri_user_time+ri_system_time"),
            ..Default::default()
        })
    }
    pub fn name(mut self, n: &str) -> P {
        self.0.name = n.into();
        self
    }
    pub fn argv(mut self, a: &[&str]) -> P {
        self.0.cmdline = a.iter().map(|s| s.to_string()).collect();
        self
    }
    pub fn cwd(mut self, c: &str) -> P {
        self.0.cwd = Some(c.into());
        self
    }
    pub fn uid(mut self, u: u32) -> P {
        self.0.uid = Some(u);
        self
    }
    pub fn cpu(mut self, c: f64) -> P {
        self.0.cpu_pct = Measured::exact(c, "proc_pid_rusage.ri_user_time+ri_system_time");
        self
    }
    /// footprint and resident bytes.
    pub fn mem(mut self, footprint: u64, resident: u64) -> P {
        self.0.mem = MemBreakdown {
            resident: Measured::exact(resident, "proc_pid_rusage.ri_resident_size"),
            footprint_or_pss: Measured::exact(footprint, "proc_pid_rusage.ri_phys_footprint"),
            non_resident_est: Measured::estimate(
                footprint.saturating_sub(resident),
                "max(0, footprint − resident)",
            ),
            ..Default::default()
        };
        self
    }
    pub fn markers(mut self, m: Markers) -> P {
        self.0.markers = m;
        self
    }
    pub fn responsible(mut self, pid: u32) -> P {
        self.0.responsible_pid = Some(pid);
        self
    }
    pub fn cgroup(mut self, cg: &str) -> P {
        self.0.cgroup = Some(cg.into());
        self
    }
    pub fn build(self) -> Process {
        self.0
    }
}

/// Lineage entry: `pid` was first seen as a child of `ppid` in `group`.
pub fn lineage(p: &Process, ppid: u32, group: &str, label: &str, agent: bool) -> LineageEntry {
    LineageEntry {
        id: p.id,
        ppid: Some(ppid),
        group_id: group.into(),
        group_kind: if group.starts_with("agent:") {
            oomtop_core::GroupKind::AgentSession
        } else {
            oomtop_core::GroupKind::Other
        },
        group_label: label.into(),
        spawned_by_agent: agent,
        first_seen_ms: p.id.start_time,
        last_seen_ms: NOW_MS - MIN_MS,
        last_active_ms: p.id.start_time,
        ..Default::default()
    }
}

pub fn protect_for(s: &Snapshot) -> ProtectContext {
    let me = s.self_pid.and_then(|p| s.process_by_pid(p));
    let mut ancestors = Vec::new();
    let mut cur = me.and_then(|p| p.ppid);
    while let Some(pid) = cur.filter(|p| *p > 1 && ancestors.len() < 64) {
        ancestors.push(pid);
        cur = s.process_by_pid(pid).and_then(|p| p.ppid);
    }
    ProtectContext {
        self_pid: s.self_pid,
        self_uid: me.and_then(|p| p.uid),
        ancestor_pids: ancestors,
        ..Default::default()
    }
}

/// The engine pipeline for one snapshot: attribution, then idle/orphan with a tracker that has watched the
/// machine for `watched_min` minutes (two observations, all CPU values as in the fixture).
pub fn enrich(mut s: Snapshot, lineage: &BTreeMap<ProcId, LineageEntry>, watched_min: u64) -> Snapshot {
    let r = rules();
    let protect = protect_for(&s);
    s.groups = attribute(
        &s,
        &AttributionContext {
            rules: &r,
            lineage,
            protect: &protect,
        },
    );
    let cfg = IdleConfig::default();
    let mut t = IdleTracker::new();
    t.seed(lineage);
    let mut earlier = s.clone();
    earlier.taken_at_ms = s.taken_at_ms - watched_min * MIN_MS;
    // only processes that already existed back then
    earlier
        .processes
        .retain(|p| p.id.start_time <= earlier.taken_at_ms);
    t.observe(&earlier, &cfg);
    t.observe(&s, &cfg);
    t.apply(&mut s, lineage, &cfg);
    s
}

/// Compact, reviewable view of a group for golden files.
#[derive(Serialize, Debug)]
pub struct GroupView {
    pub id: String,
    pub kind: &'static str,
    pub label: String,
    pub root: Option<u32>,
    pub confidence: String,
    pub footprint_mib: Option<u64>,
    pub members: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<&'static str>,
}

pub fn view(s: &Snapshot) -> Vec<GroupView> {
    s.groups
        .iter()
        .map(|g| {
            let mut flags = Vec::new();
            for (on, f) in [
                (g.idle, "idle"),
                (g.orphan, "orphan"),
                (g.protected, "protected"),
                (g.is_self, "self"),
                (g.lower_bound, "lower_bound"),
            ] {
                if on {
                    flags.push(f);
                }
            }
            GroupView {
                id: g.id.clone(),
                kind: g.kind.alias(),
                label: g.label.clone(),
                root: g.root.map(|r| r.pid),
                confidence: format!("{:?}", g.confidence).to_lowercase(),
                footprint_mib: g.totals.footprint.value.map(|v| v / MIB),
                members: g
                    .members
                    .iter()
                    .map(|m| format!("{} {:?}/{:?}", m.id.pid, m.via, m.confidence).to_lowercase())
                    .collect(),
                owner: g.owner_group.clone(),
                flags,
            }
        })
        .collect()
}

pub fn group<'a>(s: &'a Snapshot, id: &str) -> &'a Group {
    s.group(id).unwrap_or_else(|| {
        panic!(
            "no group {id}; have {:?}",
            s.groups.iter().map(|g| &g.id).collect::<Vec<_>>()
        )
    })
}

pub fn sid12(session: &str) -> String {
    hash_marker(session)[..12].to_string()
}

pub fn total_footprint(s: &Snapshot) -> u64 {
    sum_bytes(s.processes.iter().map(|p| &p.mem.footprint_or_pss), "t")
        .value
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------------------------
// The development Mac (SPEC §17 real-world acceptance fixture)
// ---------------------------------------------------------------------------------------------------------

pub const SESSIONS: [&str; 4] = [
    "0b7e7a52-0000-4000-8000-000000000001",
    "0b7e7a52-0000-4000-8000-000000000002",
    "0b7e7a52-0000-4000-8000-000000000003",
    "0b7e7a52-0000-4000-8000-000000000004",
];
pub const SELF_PID: u32 = 4120;
pub const VM_PID: u32 = 889;
pub const DESKTOP_PID: u32 = 880;

/// Four Claude Code sessions in iTerm2 (one hosting `oomtop mcp`), Claude desktop + VM, idle Gradle and
/// Kotlin daemons, the qwen studio (bash start.sh → sd-server + python), Chrome, system daemons.
pub fn mac_machine() -> (Snapshot, BTreeMap<ProcId, LineageEntry>) {
    let home = "/Users/dev";
    let claude_exe = format!("{home}/.local/share/claude/versions/2.1.3");
    let mut v: Vec<Process> = vec![
        P::new(0, 0, 900, "").name("kernel_task").uid(0).mem(GIB, GIB).cpu(3.0).build(),
        P::new(1, 0, 900, "/sbin/launchd").name("launchd").uid(0).mem(30 * MIB, 20 * MIB).build(),
        P::new(150, 1, 900, "/System/Library/PrivateFrameworks/SkyLight.framework/Resources/WindowServer")
            .name("WindowServer")
            .uid(88)
            .mem(900 * MIB, 400 * MIB)
            .cpu(12.0)
            .build(),
        P::new(160, 1, 30, "/System/Library/Frameworks/CoreServices.framework/Frameworks/Metadata.framework/Versions/A/Support/mdworker_shared")
            .name("mdworker_shared")
            .mem(20 * MIB, 15 * MIB)
            .build(),
        P::new(161, 1, 20, "/System/Library/Frameworks/CoreServices.framework/Frameworks/Metadata.framework/Versions/A/Support/mdworker_shared")
            .name("mdworker_shared")
            .mem(18 * MIB, 14 * MIB)
            .build(),
        // iTerm2
        P::new(400, 1, 600, "/Applications/iTerm.app/Contents/MacOS/iTerm2").mem(300 * MIB, 250 * MIB).cpu(1.0).build(),
    ];
    // four terminal tabs: login → -zsh → claude
    for (k, sess) in SESSIONS.iter().enumerate() {
        let base = 410 + 10 * k as u32;
        let age = 300 - 60 * k as u64;
        let project = format!("{home}/code/proj{}", k + 1);
        v.push(
            P::new(base, 400, age, "/usr/bin/login")
                .uid(0)
                .argv(&["login", "-fpl", "dev", "/bin/zsh"])
                .responsible(400)
                .mem(3 * MIB, 2 * MIB)
                .build(),
        );
        v.push(
            P::new(base + 1, base, age, "/bin/zsh")
                .name("-zsh")
                .argv(&["-zsh"])
                .responsible(400)
                .cwd(&project)
                .mem(6 * MIB, 5 * MIB)
                .build(),
        );
        v.push(
            P::new(base + 2, base + 1, age - 1, &claude_exe)
                .name("claude")
                .argv(&["claude"])
                .responsible(400)
                .cwd(&project)
                .mem(420 * MIB, 380 * MIB)
                .cpu(if k == 0 { 25.0 } else { 0.8 })
                .build(),
        );
        // an MCP server child (node) carrying the session marker
        v.push(
            P::new(base + 3, base + 2, age - 1, "/opt/homebrew/bin/node")
                .argv(&["node", "/opt/homebrew/lib/node_modules/@playwright/mcp/cli.js"])
                .responsible(400)
                .cwd(&project)
                .markers(claude_markers(sess))
                .mem(90 * MIB, 80 * MIB)
                .build(),
        );
    }
    let [s1, s2, s3, _s4] = SESSIONS;
    // session 1 hosts `oomtop mcp` (this process) → stays in the session, excluded from its reclaim gain
    v.push(
        P::new(SELF_PID, 412, 5, "/opt/homebrew/bin/oomtop")
            .argv(&["oomtop", "mcp"])
            .responsible(400)
            .cwd(&format!("{home}/code/proj1"))
            .markers(claude_markers(s1))
            .mem(12 * MIB, 11 * MIB)
            .cpu(0.6)
            .build(),
    );
    // session 2 started a dev server with `&` via its shell; the shell exited → re-parented to launchd
    v.push(
        P::new(5001, 1, 200, "/opt/homebrew/bin/node")
            .argv(&[
                "node",
                "/Users/dev/code/proj2/node_modules/next/dist/bin/next",
                "dev",
                "-p",
                "3000",
            ])
            .cwd(&format!("{home}/code/proj2"))
            .markers(claude_markers(s2))
            .mem(700 * MIB, 650 * MIB)
            .cpu(1.5)
            .build(),
    );
    v.push(
        P::new(5002, 5001, 200, "/opt/homebrew/bin/node")
            .argv(&[
                "node",
                "/Users/dev/code/proj2/node_modules/next/dist/server/lib/start-server.js",
            ])
            .cwd(&format!("{home}/code/proj2"))
            .markers(claude_markers(s2))
            .mem(400 * MIB, 380 * MIB)
            .build(),
    );
    // session 3: Chrome for Testing launched by the playwright MCP, re-parented, carries markers
    v.push(
        P::new(5101, 1, 150, "/Users/dev/Library/Caches/ms-playwright/chromium-1200/chrome-mac/Chromium.app/Contents/MacOS/Chromium")
            .markers(claude_markers(s3))
            .responsible(400)
            .mem(350 * MIB, 300 * MIB)
            .build(),
    );
    v.push(
        P::new(5102, 5101, 150, "/Users/dev/Library/Caches/ms-playwright/chromium-1200/chrome-mac/Chromium.app/Contents/Frameworks/Chromium Framework.framework/Helpers/Chromium Helper (Renderer).app/Contents/MacOS/Chromium Helper (Renderer)")
            .markers(claude_markers(s3))
            .responsible(400)
            .mem(200 * MIB, 180 * MIB)
            .build(),
    );
    // session 4: a watcher spawned without the marker env (env -i), later re-parented; only the journal knows
    let watcher = P::new(5201, 1, 100, "/opt/homebrew/bin/fswatch")
        .argv(&["fswatch", "-r", "/Users/dev/code/proj4/src"])
        .cwd(&format!("{home}/code/proj4"))
        .mem(8 * MIB, 7 * MIB)
        .build();
    v.push(watcher.clone());
    // a dead session's leftover: tsc --watch whose session (and CLI) exited 3 h ago
    let dead = "0b7e7a52-dead-4000-8000-00000000dead";
    v.push(
        P::new(5301, 1, 240, "/opt/homebrew/bin/node")
            .argv(&[
                "node",
                "/Users/dev/code/old/node_modules/typescript/bin/tsc",
                "--watch",
            ])
            .cwd(&format!("{home}/code/old"))
            .markers(claude_markers(dead))
            .mem(300 * MIB, 290 * MIB)
            .build(),
    );
    // idle Gradle + Kotlin daemons (spawned by session 1's `./gradlew build`, daemonized → ppid 1)
    v.push(
        P::new(
            781,
            1,
            240,
            "/opt/homebrew/Cellar/openjdk@17/17.0.20.1/libexec/openjdk.jdk/Contents/Home/bin/java",
        )
        .argv(&[
            "/opt/homebrew/Cellar/openjdk@17/17.0.20.1/libexec/openjdk.jdk/Contents/Home/bin/java",
            "--add-opens=java.base/java.util=ALL-UNNAMED",
            "-Xmx4g",
            "-cp",
            "/Users/dev/.gradle/wrapper/dists/gradle-9.8.0-bin/lib/gradle-daemon-main-9.8.0.jar",
            "org.gradle.launcher.daemon.bootstrap.GradleDaemon",
            "9.8.0",
        ])
        .cwd(&format!("{home}/.gradle/daemon/9.8.0"))
        .markers(claude_markers(s1))
        .mem(3_100 * MIB, 2_950 * MIB)
        .build(),
    );
    v.push(
        P::new(782, 1, 238, "/opt/homebrew/Cellar/openjdk@17/17.0.20.1/libexec/openjdk.jdk/Contents/Home/bin/java")
            .argv(&[
                "/opt/homebrew/Cellar/openjdk@17/17.0.20.1/libexec/openjdk.jdk/Contents/Home/bin/java",
                "-cp",
                "/Users/dev/.gradle/caches/modules-2/files-2.1/org.jetbrains.kotlin/kotlin-compiler-embeddable/2.2.0/kotlin-compiler-embeddable-2.2.0.jar",
                "-Xmx3g",
                "org.jetbrains.kotlin.daemon.KotlinCompileDaemon",
                "--daemon-runFilesPath",
                "/Users/dev/Library/Application Support/kotlin/daemon",
            ])
            .markers(claude_markers(s1))
            .mem(2_900 * MIB, 2_780 * MIB)
            .build(),
    );
    // Claude desktop + helpers + VM (VM is an XPC service: ppid launchd, responsible = Claude.app)
    v.push(
        P::new(
            DESKTOP_PID,
            1,
            500,
            "/Applications/Claude.app/Contents/MacOS/Claude",
        )
        .mem(450 * MIB, 400 * MIB)
        .cpu(2.0)
        .build(),
    );
    v.push(
        P::new(881, DESKTOP_PID, 500, "/Applications/Claude.app/Contents/Frameworks/Claude Helper (Renderer).app/Contents/MacOS/Claude Helper (Renderer)")
            .responsible(DESKTOP_PID)
            .mem(380 * MIB, 350 * MIB)
            .build(),
    );
    v.push(
        P::new(
            882,
            DESKTOP_PID,
            500,
            "/Applications/Claude.app/Contents/Frameworks/Claude Helper.app/Contents/MacOS/Claude Helper",
        )
        .responsible(DESKTOP_PID)
        .mem(120 * MIB, 110 * MIB)
        .build(),
    );
    v.push(
        P::new(883, 1, 500, "/Applications/Claude.app/Contents/Frameworks/Electron Framework.framework/Helpers/chrome_crashpad_handler")
            .responsible(DESKTOP_PID)
            .mem(4 * MIB, 3 * MIB)
            .build(),
    );
    v.push(
        P::new(VM_PID, 1, 499, "/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine")
            .responsible(DESKTOP_PID)
            .mem(1_500 * MIB, 900 * MIB)
            .cpu(4.0)
            .build(),
    );
    // qwen studio: started from the 4th tab's shell with ./start.sh → sd-server (Metal, ~10 GB) + python UI
    v.push(
        P::new(900, 441, 60, "/bin/bash")
            .argv(&["/bin/bash", "./start.sh"])
            .cwd(&format!("{home}/rnd/qwen-image-studio"))
            .responsible(400)
            .mem(2 * MIB, 2 * MIB)
            .build(),
    );
    v.push(
        P::new(901, 900, 60, "/Users/dev/rnd/qwen-image-studio/bin/sd-server")
            .argv(&[
                "/Users/dev/rnd/qwen-image-studio/bin/sd-server",
                "--listen-port",
                "7861",
                "--diffusion-model",
                "/Users/dev/models/qwen-image-edit-2509-Q4_K_M.gguf",
            ])
            .cwd(&format!("{home}/rnd/qwen-image-studio"))
            .responsible(400)
            .mem(10_200 * MIB, 310 * MIB)
            .cpu(85.0)
            .build(),
    );
    v.push(
        P::new(902, 900, 60, "/opt/homebrew/bin/python3.12")
            .argv(&["python3", "studio.py", "--port", "7860"])
            .cwd(&format!("{home}/rnd/qwen-image-studio"))
            .responsible(400)
            .mem(160 * MIB, 150 * MIB)
            .build(),
    );
    // Google Chrome + helpers
    v.push(
        P::new(
            700,
            1,
            480,
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )
        .mem(600 * MIB, 550 * MIB)
        .cpu(3.0)
        .build(),
    );
    v.push(
        P::new(701, 700, 480, "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/140.0/Helpers/Google Chrome Helper (Renderer).app/Contents/MacOS/Google Chrome Helper (Renderer)")
            .responsible(700)
            .mem(900 * MIB, 850 * MIB)
            .build(),
    );
    v.push(
        P::new(702, 700, 480, "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/140.0/Helpers/Google Chrome Helper (GPU).app/Contents/MacOS/Google Chrome Helper (GPU)")
            .responsible(700)
            .mem(300 * MIB, 280 * MIB)
            .build(),
    );
    let mut lin = BTreeMap::new();
    lin.insert(
        watcher.id,
        lineage(
            &watcher,
            443,
            &format!("agent:{}", sid12(SESSIONS[3])),
            "Claude Code",
            true,
        ),
    );
    let s = Snapshot {
        taken_at_ms: NOW_MS,
        processes: v,
        self_pid: Some(SELF_PID),
        host: oomtop_core::HostInfo {
            hostname: "dev-mac".into(),
            os: oomtop_core::OsKind::Macos,
            mem_total: 24 * GIB,
            ..Default::default()
        },
        ..Default::default()
    };
    (s, lin)
}

// ---------------------------------------------------------------------------------------------------------
// A Linux desktop
// ---------------------------------------------------------------------------------------------------------

pub fn linux_machine() -> (Snapshot, BTreeMap<ProcId, LineageEntry>) {
    let user = "/user.slice/user-1000.slice/user@1000.service";
    let v: Vec<Process> = vec![
        P::new(1, 0, 900, "/usr/lib/systemd/systemd")
            .name("systemd")
            .uid(0)
            .cgroup("/init.scope")
            .mem(12 * MIB, 10 * MIB)
            .build(),
        P::new(2, 0, 900, "").name("kthreadd").uid(0).build(),
        P::new(3, 2, 900, "").name("rcu_gp").uid(0).build(),
        P::new(4, 2, 900, "")
            .name("kworker/0:0H-events_highpri")
            .uid(0)
            .build(),
        P::new(300, 1, 900, "/usr/sbin/dockerd")
            .uid(0)
            .cgroup("/system.slice/docker.service")
            .mem(90 * MIB, 80 * MIB)
            .build(),
        P::new(310, 1, 800, "/usr/bin/containerd-shim-runc-v2")
            .uid(0)
            .cgroup("/system.slice/containerd.service")
            .mem(10 * MIB, 9 * MIB)
            .build(),
        // a container: postgres (init + worker), cgroup scope carries the container id
        P::new(311, 310, 800, "/usr/lib/postgresql/16/bin/postgres")
            .uid(999)
            .cgroup(
                "/system.slice/docker-4b1d6c9e0f2a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a4b3c.scope",
            )
            .mem(250 * MIB, 240 * MIB)
            .build(),
        P::new(312, 311, 800, "/usr/lib/postgresql/16/bin/postgres")
            .uid(999)
            .cgroup(
                "/system.slice/docker-4b1d6c9e0f2a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b9c8d7e6f5a4b3c.scope",
            )
            .mem(80 * MIB, 70 * MIB)
            .build(),
        P::new(400, 1, 900, "/usr/lib/systemd/systemd")
            .name("systemd")
            .argv(&["/usr/lib/systemd/systemd", "--user"])
            .cgroup(&format!("{user}/init.scope"))
            .mem(10 * MIB, 9 * MIB)
            .build(),
        // ollama as a user service
        P::new(450, 400, 700, "/usr/local/bin/ollama")
            .argv(&["/usr/local/bin/ollama", "serve"])
            .cgroup(&format!("{user}/app.slice/ollama.service"))
            .mem(300 * MIB, 280 * MIB)
            .build(),
        P::new(451, 450, 30, "/usr/local/bin/ollama")
            .argv(&[
                "/usr/local/bin/ollama",
                "runner",
                "--model",
                "/home/dev/.ollama/models/blobs/sha256-6a0746a1ec1a",
                "--port",
                "39113",
            ])
            .cgroup(&format!("{user}/app.slice/ollama.service"))
            .mem(5_200 * MIB, 5_100 * MIB)
            .cpu(40.0)
            .build(),
        // Firefox from the dock (app scope) — two processes; content process re-parented to systemd --user
        P::new(500, 400, 300, "/usr/lib/firefox/firefox")
            .cgroup(&format!("{user}/app.slice/app-gnome-firefox-4242.scope"))
            .mem(800 * MIB, 700 * MIB)
            .cpu(4.0)
            .build(),
        P::new(501, 500, 300, "/usr/lib/firefox/firefox")
            .argv(&[
                "/usr/lib/firefox/firefox",
                "-contentproc",
                "-childID",
                "1",
                "-isForBrowser",
                "tab",
            ])
            .cgroup(&format!("{user}/app.slice/app-gnome-firefox-4242.scope"))
            .mem(400 * MIB, 380 * MIB)
            .build(),
        // Slack in its app scope, no rule → cgroup app scope fallback
        P::new(520, 400, 300, "/usr/lib/slack/slack")
            .cgroup(&format!("{user}/app.slice/app-gnome-slack-5151.scope"))
            .mem(500 * MIB, 450 * MIB)
            .build(),
        P::new(521, 400, 299, "/usr/lib/slack/slack")
            .argv(&["/usr/lib/slack/slack", "--type=zygote"])
            .cgroup(&format!("{user}/app.slice/app-gnome-slack-5151.scope"))
            .mem(90 * MIB, 80 * MIB)
            .build(),
        // GNOME Terminal server → bash (vte-spawn) → claude (node install) → tsserver child
        P::new(600, 400, 400, "/usr/libexec/gnome-terminal-server")
            .name("gnome-terminal-")
            .cgroup(&format!(
                "{user}/app.slice/app-org.gnome.Terminal.slice/gnome-terminal-server.service"
            ))
            .mem(60 * MIB, 55 * MIB)
            .build(),
        P::new(601, 600, 400, "/usr/bin/bash")
            .argv(&["bash"])
            .cgroup(&format!(
                "{user}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-8c7e.scope"
            ))
            .cwd("/home/dev/code/api")
            .mem(5 * MIB, 5 * MIB)
            .build(),
        P::new(602, 601, 120, "/usr/bin/node")
            .argv(&[
                "node",
                "/home/dev/.npm-global/lib/node_modules/@anthropic-ai/claude-code/cli.js",
            ])
            .cgroup(&format!(
                "{user}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-8c7e.scope"
            ))
            .cwd("/home/dev/code/api")
            .mem(380 * MIB, 360 * MIB)
            .cpu(6.0)
            .build(),
        P::new(603, 602, 119, "/usr/bin/node")
            .argv(&[
                "node",
                "/home/dev/code/api/node_modules/typescript/lib/tsserver.js",
                "--useInferredProjectPerProjectRoot",
            ])
            .cgroup(&format!(
                "{user}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-8c7e.scope"
            ))
            .cwd("/home/dev/code/api")
            .markers(claude_markers("linux-session-1"))
            .mem(450 * MIB, 440 * MIB)
            .build(),
        P::new(604, 602, 119, "/usr/bin/bash")
            .argv(&["/usr/bin/bash", "-c", "-l", "cargo test"])
            .cgroup(&format!(
                "{user}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-8c7e.scope"
            ))
            .cwd("/home/dev/code/api")
            .markers(claude_markers("linux-session-1"))
            .mem(4 * MIB, 4 * MIB)
            .build(),
        P::new(605, 604, 1, "/home/dev/.cargo/bin/cargo")
            .argv(&["cargo", "test"])
            .cgroup(&format!(
                "{user}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-8c7e.scope"
            ))
            .cwd("/home/dev/code/api")
            .markers(claude_markers("linux-session-1"))
            .mem(120 * MIB, 110 * MIB)
            .cpu(95.0)
            .build(),
        // user runs a script from another tab in the same project (no marker): linked to the session by cwd
        P::new(610, 600, 50, "/usr/bin/bash")
            .argv(&["bash"])
            .cgroup(&format!(
                "{user}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-9d1f.scope"
            ))
            .cwd("/home/dev/code/api")
            .mem(5 * MIB, 5 * MIB)
            .build(),
        P::new(611, 610, 40, "/usr/bin/python3")
            .argv(&["python3", "scripts/load_test.py", "--rps", "200"])
            .cgroup(&format!(
                "{user}/app.slice/app-org.gnome.Terminal.slice/vte-spawn-9d1f.scope"
            ))
            .cwd("/home/dev/code/api/scripts")
            .mem(90 * MIB, 85 * MIB)
            .cpu(30.0)
            .build(),
        // tmux server (daemonized) → shell → sudo → htop (root)
        P::new(700, 400, 600, "/usr/bin/tmux")
            .argv(&["tmux", "new", "-d"])
            .name("tmux: server")
            .cgroup(&format!("{user}/app.slice/tmux-spawn-11aa.scope"))
            .mem(6 * MIB, 5 * MIB)
            .build(),
        P::new(701, 700, 600, "/usr/bin/zsh")
            .argv(&["-zsh"])
            .name("zsh")
            .cgroup(&format!("{user}/app.slice/tmux-spawn-11aa.scope"))
            .mem(7 * MIB, 6 * MIB)
            .build(),
        P::new(702, 701, 10, "/usr/bin/sudo")
            .argv(&["sudo", "htop"])
            .uid(0)
            .cgroup(&format!("{user}/app.slice/tmux-spawn-11aa.scope"))
            .mem(4 * MIB, 4 * MIB)
            .build(),
        P::new(703, 702, 10, "/usr/bin/htop")
            .uid(0)
            .cgroup(&format!("{user}/app.slice/tmux-spawn-11aa.scope"))
            .mem(6 * MIB, 5 * MIB)
            .cpu(2.0)
            .build(),
        // oomtop TUI in the tmux shell: its own `oomtop` group
        P::new(4120, 701, 2, "/home/dev/.cargo/bin/oomtop")
            .cgroup(&format!("{user}/app.slice/tmux-spawn-11aa.scope"))
            .mem(14 * MIB, 13 * MIB)
            .cpu(0.8)
            .build(),
    ];
    let s = Snapshot {
        taken_at_ms: NOW_MS,
        processes: v,
        self_pid: Some(4120),
        host: oomtop_core::HostInfo {
            hostname: "dev-linux".into(),
            os: oomtop_core::OsKind::Linux,
            mem_total: 32 * GIB,
            ..Default::default()
        },
        ..Default::default()
    };
    (s, BTreeMap::new())
}

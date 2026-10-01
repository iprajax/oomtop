//! Attribution over the real capture of the development Mac (`fixtures/macos/m5-air-agents.json`, SPEC §17
//! acceptance fixtures). The raw `mac_procs` records are converted here directly (only the fields rules
//! need), so this test does not depend on the collector crate.

use oomtop_core::actions::ProtectContext;
use oomtop_core::{GroupKind, Markers, Measured, MemBreakdown, ProcId, Process, Snapshot};
use oomtop_detect::Detector;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/macos/m5-air-agents.json")
}

fn load_frame0() -> Option<Snapshot> {
    let text = std::fs::read_to_string(fixture_path()).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let frame = v.get("frames")?.get(0)?.as_array()?;
    let procs = frame
        .iter()
        .find(|s| s["payload"]["type"] == "mac_procs")?
        .get("payload")?
        .get("procs")?
        .as_array()?;
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let u = |v: &Value| v.as_u64();
    let processes = procs
        .iter()
        .map(|r| {
            let start = u(&r["start_tvsec"]).unwrap_or(0) * 1000 + u(&r["start_tvusec"]).unwrap_or(0) / 1000;
            let rus = &r["rusage"];
            let fp = u(&rus["phys_footprint"]);
            Process {
                id: ProcId::new(u(&r["pid"]).unwrap_or(0) as u32, start),
                ppid: u(&r["ppid"]).map(|x| x as u32),
                responsible_pid: u(&r["responsible_pid"]).map(|x| x as u32),
                name: s(&r["name"]),
                exe: s(&r["path"]),
                cmdline: r["argv"]
                    .as_array()
                    .map(|a| a.iter().map(s).collect())
                    .unwrap_or_default(),
                uid: u(&r["uid"]).map(|x| x as u32),
                mem: MemBreakdown {
                    footprint_or_pss: match fp {
                        Some(b) => Measured::exact(b, "proc_pid_rusage.ri_phys_footprint"),
                        None => Measured::unavailable("proc_pid_rusage", "other user"),
                    },
                    ..Default::default()
                },
                markers: serde_json::from_value::<Markers>(r["markers"].clone()).unwrap_or_default(),
                ..Default::default()
            }
        })
        .collect();
    Some(Snapshot {
        processes,
        ..Default::default()
    })
}

#[test]
fn m5_air_agents_fixture() {
    let Some(snap) = load_frame0() else {
        eprintln!("fixture missing: {}", fixture_path().display());
        return;
    };
    let d = Detector::builtin();
    let protect = ProtectContext {
        self_uid: Some(501),
        ..Default::default()
    };
    let groups = d.attribute(&snap, &BTreeMap::new(), &protect);

    // Every process in exactly one group.
    let members: usize = groups.iter().map(|g| g.members.len()).sum();
    assert_eq!(members, snap.processes.len());

    // Acceptance #3: the Virtualization.framework VM belongs to Claude.app (responsible pid).
    let vm = snap
        .processes
        .iter()
        .find(|p| oomtop_detect::is_virtualization_vm(p))
        .expect("VM in fixture");
    let vm_group = snap_group(&groups, vm.id).expect("vm attributed");
    assert_eq!(
        (vm_group.kind, vm_group.label.as_str()),
        (GroupKind::App, "Claude")
    );

    // Acceptance #2: idle Gradle + Kotlin daemons are build daemons of their own.
    for label in ["GradleDaemon", "KotlinCompileDaemon"] {
        let g = groups
            .iter()
            .find(|g| g.label == label)
            .unwrap_or_else(|| panic!("{label} missing"));
        assert_eq!(g.kind, GroupKind::BuildDaemon);
        assert!(!g.protected);
    }

    // Acceptance #4 (shape): each `claude` CLI process roots an agent session.
    let claude_roots: Vec<&Process> = snap.processes.iter().filter(|p| p.name == "claude.exe").collect();
    assert!(claude_roots.len() >= 3);
    for c in &claude_roots {
        let g = snap_group(&groups, c.id).unwrap();
        assert_eq!(g.kind, GroupKind::AgentSession, "{}", g.id);
        assert_eq!(g.label, "Claude Code");
        // …and every descendant that is not itself a group root (tool shells, caffeinate, `head`) stays
        // in that session instead of going to Terminal.app through its responsible pid.
        for child in descendants(&snap, c.id.pid) {
            if d.matching_rule(child).is_some() {
                continue;
            }
            let cg = snap_group(&groups, child.id).unwrap();
            assert_eq!(cg.id, g.id, "{} ({}) left its session", child.name, child.id.pid);
        }
    }
    let terminal = groups.iter().find(|g| g.label == "Terminal").unwrap();
    assert!(
        terminal.members.iter().all(|m| {
            let p = snap.process(m.id).unwrap();
            matches!(p.name.as_str(), "Terminal" | "login" | "zsh")
        }),
        "Terminal holds only the terminal, login and interactive shells"
    );

    // Codex (inside the ChatGPT app) is an agent session owned by the app.
    let codex = groups.iter().find(|g| g.label == "Codex").expect("codex session");
    assert_eq!(codex.kind, GroupKind::AgentSession);
    assert!(codex.owner_group.is_some());

    // This user's Chrome is one app group (browser + helpers), even though it runs from a code-sign clone
    // (`Google Chrome.app.bundle`). The second logged-in user's Chrome is a separate, protected group
    // keyed by uid, so its memory is never offered for reclaim or merged with ours.
    let chrome: Vec<_> = groups
        .iter()
        .filter(|g| g.label.starts_with("Google Chrome") && g.kind == GroupKind::App)
        .collect();
    let ids: Vec<&str> = chrome.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
    let mine = chrome
        .iter()
        .find(|g| g.id == "app:google-chrome")
        .expect("{ids:?}");
    assert!(mine.members.len() > 5);
    assert!(!mine.protected);
    let theirs = chrome
        .iter()
        .find(|g| g.id.starts_with("app:google-chrome@u"))
        .expect("{ids:?}");
    assert!(theirs.protected, "another user's app is protected");

    // Golden: the non-trivial groups (kind, label, member count, rule) of the real machine.
    let mut summary: Vec<String> = groups
        .iter()
        .filter(|g| g.members.len() > 1 || g.matched_by.is_some() || g.kind != GroupKind::System)
        .filter(|g| g.kind != GroupKind::Other)
        .map(|g| {
            format!(
                "{:<14} {:<28} n={:<3} rule={}",
                g.kind.as_str(),
                g.label,
                g.members.len(),
                g.matched_by.as_deref().unwrap_or("-")
            )
        })
        .collect();
    summary.sort();
    insta::assert_snapshot!("m5_air_agents_groups", summary.join("\n"));

    // Cost on a real 773-process machine (SPEC §14): report, and fail only on a gross regression.
    let n = 10;
    let t = std::time::Instant::now();
    for _ in 0..n {
        std::hint::black_box(d.attribute(&snap, &BTreeMap::new(), &protect));
    }
    let per = t.elapsed() / n;
    eprintln!(
        "attribution of {} processes: {per:?} per sample",
        snap.processes.len()
    );
    assert!(per < std::time::Duration::from_millis(250), "{per:?}");
}

fn descendants(s: &Snapshot, pid: u32) -> Vec<&Process> {
    let mut out = Vec::new();
    let mut stack = vec![pid];
    while let Some(pp) = stack.pop() {
        for p in s
            .processes
            .iter()
            .filter(|p| p.ppid == Some(pp) && p.id.pid != pp)
        {
            out.push(p);
            stack.push(p.id.pid);
        }
    }
    out
}

fn snap_group(groups: &[oomtop_core::Group], id: ProcId) -> Option<&oomtop_core::Group> {
    groups.iter().find(|g| g.members.iter().any(|m| m.id == id))
}

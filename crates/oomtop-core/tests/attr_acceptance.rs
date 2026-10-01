//! SPEC §17 acceptance scenarios for attribution, idle/orphan and `why` (#2, #3, #4, #5), plus the §7
//! invariants (every process in exactly one group, (pid, start_time) identity, self group, actions on roots).

mod attr_support;

use attr_support::*;
use oomtop_core::actions::{plan_group, ActionKind};
use oomtop_core::can_fit::reclaim_candidates;
use oomtop_core::headroom::is_reclaim_candidate;
use oomtop_core::history::{History, HistoryPoint};
use oomtop_core::throttle::throttle_factor;
use oomtop_core::why::{explain, CauseKind};
use oomtop_core::{
    AttributionSignal, ClusterFreq, Confidence, GroupKind, Measured, PressureLevel, ThermalPressure,
};
use std::collections::HashSet;

fn mac() -> oomtop_core::Snapshot {
    let (s, lin) = mac_machine();
    enrich(s, &lin, 45)
}

#[test]
fn every_process_in_exactly_one_group() {
    for s in [mac(), {
        let (s, l) = linux_machine();
        enrich(s, &l, 45)
    }] {
        let mut seen = HashSet::new();
        for g in &s.groups {
            assert!(!g.members.is_empty(), "{}", g.id);
            assert!(g.root.is_some(), "every group has a root: {}", g.id);
            assert_eq!(g.members[0].id, g.root.unwrap(), "root listed first in {}", g.id);
            for m in &g.members {
                assert!(seen.insert(m.id), "{:?} in two groups", m.id);
            }
        }
        assert_eq!(seen.len(), s.processes.len());
        let ids: HashSet<&str> = s.groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids.len(), s.groups.len(), "group ids unique");
        // sorted by footprint desc
        let fps: Vec<u64> = s
            .groups
            .iter()
            .map(|g| g.totals.footprint.value.unwrap_or(0))
            .collect();
        assert!(fps.windows(2).all(|w| w[0] >= w[1]));
        assert_eq!(fps.iter().sum::<u64>(), total_footprint(&s), "no double counting");
    }
}

/// Acceptance #1 shape: the Metal-backed model server is ≈10 GB in its group, not its RSS.
#[test]
fn acceptance_1_metal_model_server_counts_footprint() {
    let s = mac();
    let g = s
        .groups
        .iter()
        .find(|g| g.kind == GroupKind::ModelServer)
        .unwrap();
    assert_eq!(g.label, "sd-server");
    assert_eq!(s.groups[0].id, g.id, "largest group first");
    assert!(g.totals.footprint.value.unwrap() >= 10_000 * MIB);
    assert!(g.totals.resident.value.unwrap() < 400 * MIB);
    assert_eq!(
        g.owner_group.as_deref(),
        Some("other:start-sh:900"),
        "started by ./start.sh"
    );
}

/// Acceptance #2: idle Gradle + Kotlin daemons appear as build_daemon, idle, top of Reclaim, gain ≈ footprint.
#[test]
fn acceptance_2_idle_build_daemons_top_reclaim() {
    let s = mac();
    let gradle = group(&s, "daemon:builtin:gradle-daemon:781");
    let kotlin = group(&s, "daemon:builtin:kotlin-daemon:782");
    for g in [gradle, kotlin] {
        assert_eq!(g.kind, GroupKind::BuildDaemon);
        assert!(g.idle, "{} idle", g.label);
        assert!(g.idle_for_s.unwrap() >= 30 * 60);
        assert_eq!(g.confidence, Confidence::High);
        assert!(!g.orphan, "its session is alive");
        assert_eq!(
            g.owner_group.as_deref(),
            Some(format!("agent:{}", sid12(SESSIONS[0])).as_str())
        );
        assert!(is_reclaim_candidate(g));
        let gain = g.reclaim_gain.value.unwrap() as f64;
        let fp = g.totals.footprint.value.unwrap() as f64;
        assert!((gain - fp).abs() / fp <= 0.15, "gain {gain} vs footprint {fp}");
    }
    assert_eq!(gradle.label, "GradleDaemon");
    assert_eq!(kotlin.label, "KotlinCompileDaemon");
    let top: Vec<String> = reclaim_candidates(&s)
        .into_iter()
        .take(2)
        .map(|c| c.group_id)
        .collect();
    assert_eq!(
        top,
        [
            "daemon:builtin:gradle-daemon:781",
            "daemon:builtin:kotlin-daemon:782"
        ]
    );
}

/// Acceptance #3: the Virtualization.framework VM is attributed to the app that launched it (responsible
/// pid) and the group is marked as a lower bound; the action targets the app, never the VM.
#[test]
fn acceptance_3_vm_belongs_to_app_and_is_lower_bound() {
    let s = mac();
    let app = group(&s, "app:claude");
    assert_eq!(app.kind, GroupKind::App);
    assert_eq!(app.root.unwrap().pid, DESKTOP_PID);
    let vm = app
        .members
        .iter()
        .find(|m| m.id.pid == VM_PID)
        .expect("VM in Claude app group");
    assert_eq!(vm.via, AttributionSignal::Responsible);
    assert_eq!(vm.confidence, Confidence::High);
    assert!(app.lower_bound);
    assert_eq!(app.totals.footprint.quality, oomtop_core::Quality::Estimate);
    assert!(app.totals.footprint.source.contains("lower bound"));
    // helpers, crashpad (ppid 1) and VM all in the one app group
    let pids: HashSet<u32> = app.members.iter().map(|m| m.id.pid).collect();
    assert_eq!(pids, HashSet::from([880, 881, 882, 883, VM_PID]));
    let t = plan_group(app, ActionKind::Terminate, &protect_for(&s)).unwrap();
    assert_eq!(t.root.pid, DESKTOP_PID, "quit the app, not the VM");
}

/// Acceptance #4: four agent sessions → four groups keyed by session marker, children nested, including
/// children re-parented to launchd (by marker, and by lineage journal when the env had no marker).
#[test]
fn acceptance_4_four_sessions_with_reparented_children() {
    let s = mac();
    let agents: Vec<_> = s
        .groups
        .iter()
        .filter(|g| g.kind == GroupKind::AgentSession)
        .collect();
    assert_eq!(agents.len(), 4, "{:#?}", view(&s));
    let pids = |sess: &str| -> Vec<u32> {
        let g = group(&s, &format!("agent:{}", sid12(sess)));
        assert_eq!(g.label, "Claude Code");
        let mut v: Vec<u32> = g.members.iter().map(|m| m.id.pid).collect();
        v.sort();
        v
    };
    assert_eq!(pids(SESSIONS[0]), vec![412, 413, SELF_PID]);
    assert_eq!(pids(SESSIONS[1]), vec![422, 423, 5001, 5002]);
    assert_eq!(pids(SESSIONS[2]), vec![432, 433, 5101, 5102]);
    assert_eq!(pids(SESSIONS[3]), vec![442, 443, 5201]);
    let s2 = group(&s, &format!("agent:{}", sid12(SESSIONS[1])));
    let next = s2.members.iter().find(|m| m.id.pid == 5001).unwrap();
    assert_eq!(next.via, AttributionSignal::Marker);
    let s4 = group(&s, &format!("agent:{}", sid12(SESSIONS[3])));
    let watcher = s4.members.iter().find(|m| m.id.pid == 5201).unwrap();
    assert_eq!(watcher.via, AttributionSignal::Lineage);
    assert_eq!(watcher.confidence, Confidence::Medium);
    // ids are stable: same snapshot 10 s later with new pids for children → same group ids
    let (mut later, lin) = mac_machine();
    later.taken_at_ms += 10_000;
    later.processes.retain(|p| p.id.pid != 413);
    let later = enrich(later, &lin, 45);
    for sess in SESSIONS {
        assert!(later.group(&format!("agent:{}", sid12(sess))).is_some());
    }
}

#[test]
fn self_group_and_mcp_child() {
    // `oomtop mcp` inside a session: stays in the session, not reclaimable, session protected (it hosts us)
    let s = mac();
    assert!(s.group("oomtop").is_none());
    let s1 = group(&s, &format!("agent:{}", sid12(SESSIONS[0])));
    assert!(!s1.is_self);
    assert!(
        s1.protected,
        "the agent session hosting this process is protected"
    );
    let with_self = s1.totals.footprint.value.unwrap();
    let gain = s1.reclaim_gain.value.unwrap();
    assert!(
        gain < with_self - 11 * MIB,
        "self excluded from reclaim gain ({gain} vs {with_self})"
    );
    // TUI from a shell: its own `oomtop` group
    let (l, lin) = linux_machine();
    let l = enrich(l, &lin, 45);
    let me = group(&l, "oomtop");
    assert!(me.is_self && me.protected);
    assert!(!is_reclaim_candidate(me));
    assert_eq!(me.members.len(), 1);
}

#[test]
fn dead_session_leftover_is_an_orphan_root() {
    let s = mac();
    let g = group(&s, "other:typescript-tsc:5301");
    assert!(g.orphan);
    assert_eq!(g.root.unwrap().pid, 5301);
    assert!(g.owner_group.as_deref().unwrap().starts_with("agent:"));
    assert!(
        s.group(g.owner_group.as_deref().unwrap()).is_none(),
        "owner session exited"
    );
    assert!(is_reclaim_candidate(g));
    let t = plan_group(g, ActionKind::Terminate, &protect_for(&s)).unwrap();
    assert_eq!(t.root.pid, 5301, "orphans are their own roots");
}

#[test]
fn terminal_does_not_swallow_shell_commands() {
    let s = mac();
    let iterm = group(&s, "app:iterm");
    let pids: HashSet<u32> = iterm.members.iter().map(|m| m.id.pid).collect();
    // iTerm2 + 4 × (login, -zsh): terminal plumbing only
    assert_eq!(pids.len(), 9, "{pids:?}");
    assert!(iterm.protected, "oomtop's own terminal");
    let studio = group(&s, "other:start-sh:900");
    assert_eq!(
        studio.members.iter().map(|m| m.id.pid).collect::<Vec<_>>(),
        vec![900, 902]
    );
    assert!(!studio.protected);
}

#[test]
fn pid_reuse_never_merges() {
    let (mut s, lin) = mac_machine();
    // pid 5001's real parent exited; a *younger* process now holds its ppid (pid reuse)
    let p = s.processes.iter_mut().find(|p| p.id.pid == 5002).unwrap();
    p.markers = Default::default();
    p.ppid = Some(9999);
    s.processes
        .push(P::new(9999, 1, 1, "/usr/bin/newcomer").mem(MIB, MIB).build());
    let s = enrich(s, &lin, 45);
    let newcomer = s.group_of(s.process_by_pid(9999).unwrap().id).unwrap();
    assert!(
        !newcomer.members.iter().any(|m| m.id.pid == 5002),
        "reused ppid is not a parent"
    );
}

#[test]
fn linux_cgroups_units_and_boundaries() {
    let (s, lin) = linux_machine();
    let s = enrich(s, &lin, 45);
    // container by cgroup scope
    let pg = group(&s, "sandbox:docker-4b1d6c9e0f2a");
    assert_eq!(pg.kind, GroupKind::Sandbox);
    assert_eq!(pg.members.len(), 2);
    assert!(pg.members.iter().all(|m| m.via == AttributionSignal::Cgroup));
    // ollama serve + runner merged (same rule), children of systemd --user are top-level
    let ol = s.groups.iter().find(|g| g.label == "Ollama").unwrap();
    assert_eq!(ol.members.len(), 2);
    assert_eq!(ol.root.unwrap().pid, 450);
    // Firefox by rule, app key without pid; Slack by app scope
    assert_eq!(group(&s, "app:firefox").members.len(), 2);
    let slack = group(&s, "app:slack");
    assert_eq!(slack.members.len(), 2);
    assert!(slack.members.iter().all(|m| m.via == AttributionSignal::Cgroup));
    // kernel threads under kthreadd
    assert_eq!(group(&s, "system:kthreadd").members.len(), 3);
    // Claude Code (node install) session with its tool children; `bash -c` does not cut ancestry
    let agent = group(&s, &format!("agent:{}", sid12("linux-session-1")));
    let mut pids: Vec<u32> = agent.members.iter().map(|m| m.id.pid).collect();
    pids.sort();
    assert_eq!(pids, vec![602, 604, 605]);
    // the session's tsserver is a build daemon root of its own, owned by the session
    let ts = group(&s, "daemon:builtin:tsserver:603");
    assert_eq!(ts.owner_group.as_deref(), Some(agent.id.as_str()));
    // user-launched script in the same project → own root, linked by cwd
    let lt = s
        .groups
        .iter()
        .find(|g| g.label == "scripts/load_test.py" || g.label == "load_test.py")
        .unwrap();
    assert_eq!(lt.owner_group.as_deref(), Some(agent.id.as_str()));
    assert!(!lt.orphan);
    // tmux plumbing stays together; `sudo htop` → htop is its own (root-owned, protected) group
    let tmux = s
        .groups
        .iter()
        .find(|g| g.root.map(|r| r.pid) == Some(700))
        .unwrap();
    let tp: HashSet<u32> = tmux.members.iter().map(|m| m.id.pid).collect();
    assert_eq!(tp, HashSet::from([700, 701, 702]));
    let htop = s.group_of(s.process_by_pid(703).unwrap().id).unwrap();
    assert_eq!(htop.members.len(), 1);
    assert!(htop.protected);
}

// ---------------------------------------------------------------------------------------------------------
// Acceptance #5: `why` during a thermally throttled run vs idle
// ---------------------------------------------------------------------------------------------------------

fn swap_history(growing: bool) -> History {
    let mut h = History::default();
    for i in 0..=30u64 {
        let swap = if growing { 2 * GIB + i * 60 * MIB } else { 2 * GIB };
        h.push(HistoryPoint {
            t_ms: NOW_MS - (30 - i) * 10_000,
            swap_used: Some(swap),
            swap_total: Some(6 * GIB),
            ..Default::default()
        });
    }
    h
}

pub fn throttled_snapshot() -> oomtop_core::Snapshot {
    let mut s = mac();
    s.thermal.clusters = vec![
        ClusterFreq {
            name: "P-cluster".into(),
            cur_mhz: 1_900.0,
            max_mhz: 4_400.0,
            active_pct: 97.0,
        },
        ClusterFreq {
            name: "E-cluster".into(),
            cur_mhz: 2_000.0,
            max_mhz: 2_900.0,
            active_pct: 60.0,
        },
        ClusterFreq {
            name: "GPU".into(),
            cur_mhz: 600.0,
            max_mhz: 1_430.0,
            active_pct: 99.0,
        },
    ];
    s.thermal.throttle_factor = throttle_factor(&s.thermal.clusters);
    s.thermal.pressure = Measured::exact(
        ThermalPressure::Heavy,
        "notify:com.apple.system.thermalpressurelevel",
    );
    s.thermal.on_battery = Measured::exact(true, "IOPSCopyPowerSourcesInfo");
    s.thermal.battery_pct = Measured::exact(29.0, "IOPSCopyPowerSourcesInfo");
    s.thermal.low_power_mode = Measured::exact(false, "NSProcessInfo.isLowPowerModeEnabled");
    s.memory.swap_in_per_min = Measured::exact(600 * MIB, "vm_statistics64.swapins Δ");
    s.memory.swap_out_per_min = Measured::exact(350 * MIB, "vm_statistics64.swapouts Δ");
    s.memory.swap_used = Measured::exact(2 * GIB + 1_800 * MIB, "sysctl vm.swapusage");
    s.memory.swap_total = Measured::exact(6 * GIB, "sysctl vm.swapusage");
    s.memory.pressure = Measured::exact(PressureLevel::Warn, "kern.memorystatus_vm_pressure_level");
    s
}

#[test]
fn acceptance_5_throttled_run_reports_throttle_and_swap() {
    let s = throttled_snapshot();
    let causes = explain(&s, &swap_history(true));
    let kinds: Vec<CauseKind> = causes.iter().map(|c| c.kind).collect();
    assert!(kinds.contains(&CauseKind::ThermalThrottle), "{kinds:?}");
    assert!(kinds.contains(&CauseKind::SwapStorm), "{kinds:?}");
    assert!(kinds.contains(&CauseKind::LowBattery), "{kinds:?}");
    let swap = causes.iter().find(|c| c.kind == CauseKind::SwapStorm).unwrap();
    assert!(
        swap.evidence[0].starts_with("swap in +600M/min"),
        "{:?}",
        swap.evidence
    );
    assert!(swap.evidence[0].ends_with("for 5m"), "{:?}", swap.evidence);
    assert!(
        swap.fix.as_deref().unwrap().contains("2 idle build daemons"),
        "{:?}",
        swap.fix
    );
    let th = causes
        .iter()
        .find(|c| c.kind == CauseKind::ThermalThrottle)
        .unwrap();
    assert!(
        th.evidence
            .iter()
            .any(|e| e == "GPU at 42% of max clock (99% busy) with thermal pressure heavy"),
        "{:?}",
        th.evidence
    );
    assert!(th.evidence.iter().any(|e| e == "on battery at 29%"));
    assert!(causes.windows(2).all(|w| w[0].score >= w[1].score));
    insta::assert_yaml_snapshot!("why_throttled_run", causes);
}

#[test]
fn acceptance_5_idle_reports_nothing_throttle_related() {
    let mut s = throttled_snapshot();
    // same hot, low-battery, Low-Power machine — but idle: clusters parked at low clocks
    for c in &mut s.thermal.clusters {
        c.active_pct = 3.0;
        c.cur_mhz = 600.0;
    }
    s.thermal.throttle_factor = throttle_factor(&s.thermal.clusters);
    assert_eq!(s.thermal.throttle_factor.unavailable_reason(), Some("idle"));
    s.thermal.low_power_mode = Measured::exact(true, "NSProcessInfo.isLowPowerModeEnabled");
    s.cpu.total_pct = Measured::exact(4.0, "host_processor_info");
    s.memory.swap_in_per_min = Measured::exact(0, "vm_statistics64.swapins Δ");
    s.memory.swap_out_per_min = Measured::exact(0, "vm_statistics64.swapouts Δ");
    s.memory.pressure = Measured::exact(PressureLevel::Normal, "kern.memorystatus_vm_pressure_level");
    let causes = explain(&s, &swap_history(false));
    assert!(
        causes.iter().all(|c| !c.kind.is_throttle_related()),
        "{causes:#?}"
    );
    assert!(causes.is_empty(), "{causes:#?}");
}

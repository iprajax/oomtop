use super::*;

const DAY: u64 = 86_400_000;
const HL: f64 = 7.0 * 86400.0;

fn entry(pid: u32, active: u64, agent: bool) -> LineageEntry {
    LineageEntry {
        id: ProcId::new(pid, 1000),
        ppid: Some(1),
        group_id: "agent:abc".into(),
        group_kind: GroupKind::AgentSession,
        group_label: "Claude Code".into(),
        group_fingerprint: "fp".into(),
        session_id: Some("3f2a9c01aa55bb66".into()),
        spawned_by_agent: agent,
        first_seen_ms: 10,
        last_seen_ms: active,
        last_active_ms: active,
    }
}

#[test]
fn lineage_roundtrip_and_merge() {
    let d = tempfile::tempdir().unwrap();
    let mut db = StateDb::open(&d.path().join("x/state.db")).unwrap();
    assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    let mut first = entry(5, 100, true);
    first.ppid = Some(4242);
    db.record_lineage(&[first]).unwrap();
    db.flush().unwrap();
    // later sample: re-parented to launchd, attributed heuristically — journal keeps the agent attribution
    let mut later = entry(5, 200, false);
    later.group_id = "other:chrome:5".into();
    later.first_seen_ms = 150;
    later.session_id = None;
    db.record_lineage(&[later.clone()]).unwrap();
    for flushed in [false, true] {
        if flushed {
            db.flush().unwrap();
            assert_eq!(db.pending_len(), 0);
        }
        let m = db
            .lineage_for(&[ProcId::new(5, 1000), ProcId::new(6, 1)])
            .unwrap();
        assert_eq!(m.len(), 1);
        let e = &m[&ProcId::new(5, 1000)];
        assert_eq!(e.group_id, "agent:abc", "flushed={flushed}");
        assert!(e.spawned_by_agent);
        assert_eq!(e.ppid, Some(4242), "original ppid kept");
        assert_eq!(e.first_seen_ms, 10);
        assert_eq!(e.last_active_ms, 200);
        assert_eq!(e.session_id.as_deref(), Some("3f2a9c01aa55bb66"));
    }
    assert_eq!(db.lineage_since(150).unwrap().len(), 1);
    assert_eq!(db.lineage_since(250).unwrap().len(), 0);
    assert_eq!(db.prune_lineage(300).unwrap(), 1);
    assert!(db.lineage_all().unwrap().is_empty());
}

#[test]
fn merge_matches_sql_upsert() {
    // the in-memory merge and the SQL upsert must agree for every combination
    for (old_agent, new_agent) in [(false, false), (false, true), (true, false), (true, true)] {
        for (old_ppid, new_ppid) in [(Some(1), Some(77)), (Some(55), Some(1)), (None, Some(9))] {
            let mut old = entry(9, 100, old_agent);
            old.ppid = old_ppid;
            old.group_id = "old".into();
            let mut new = entry(9, 90, new_agent);
            new.ppid = new_ppid;
            new.group_id = "new".into();
            new.first_seen_ms = 50;
            new.last_active_ms = 300;
            new.session_id = None;
            let expected = merge_lineage(&old, &new);
            let mut db = StateDb::open_in_memory().unwrap();
            db.set_batch_interval(Duration::ZERO);
            db.record_lineage(&[old]).unwrap();
            db.record_lineage(&[new]).unwrap();
            let got = &db.lineage_all().unwrap()[&ProcId::new(9, 1000)];
            assert_eq!(
                got, &expected,
                "agent {old_agent}/{new_agent} ppid {old_ppid:?}/{new_ppid:?}"
            );
        }
    }
}

#[test]
fn session_ids_are_hashed_before_storage() {
    let mut db = StateDb::open_in_memory().unwrap();
    let mut e = entry(7, 10, true);
    e.session_id = Some("9d1c-raw-session-uuid".into());
    db.record_lineage(&[e]).unwrap();
    db.flush().unwrap();
    let got = db.lineage_all().unwrap()[&ProcId::new(7, 1000)]
        .session_id
        .clone()
        .unwrap();
    assert_eq!(got, oomtop_core::redact::hash_marker("9d1c-raw-session-uuid"));
    assert!(is_hashed(&got));
    let raw: String = db
        .conn
        .query_row("SELECT session_id FROM lineage", [], |r| r.get(0))
        .unwrap();
    assert!(!raw.contains("raw-session"));
}

#[test]
fn profile_frecency_reset() {
    let mut db = StateDb::open_in_memory().unwrap();
    let now = 100 * DAY;
    db.upsert_entity("sd", "sd-server · qwen-image-studio", now)
        .unwrap();
    for i in 0..5 {
        db.record_event("sd", EventKind::Select, now - i * DAY, Some("working"))
            .unwrap();
    }
    db.set_pinned("claude", true, now).unwrap();
    db.set_muted("chrome", Some(now + DAY), now).unwrap();
    db.rename("sd", Some("studio"), now).unwrap();
    let f = db.frecency(now, HL).unwrap();
    assert!(f["sd"] > 3.0 && f["sd"] < 5.0);
    assert!((f["claude"] - 3.0).abs() < 1e-9);
    assert!(f["chrome"] < 0.0);
    let ents = db.entities().unwrap();
    let sd = ents.iter().find(|e| e.fingerprint == "sd").unwrap();
    assert_eq!(sd.alias.as_deref(), Some("studio"));
    assert_eq!(sd.name(), "studio");
    assert_eq!(sd.display_name, "sd-server · qwen-image-studio");
    assert_eq!(db.pinned().unwrap(), vec!["claude".to_string()]);
    assert!(db.muted(now).unwrap().contains("chrome"));
    assert!(!db.muted(now + 2 * DAY).unwrap().contains("chrome"));
    assert_eq!(db.aliases().unwrap()["sd"], "studio");
    db.record_query("mem>2G", now).unwrap();
    db.record_query("mem>2G", now).unwrap();
    db.record_query("gpu hogs", now - 30 * DAY).unwrap();
    let q = db.top_queries(5, now, HL).unwrap();
    assert_eq!(q[0].0, "mem>2G");
    let export = db.export_profile(now, HL).unwrap();
    assert!(export["entities"].as_array().unwrap().len() >= 3);
    // UX §11 test 7: reset returns ranking to salience-only defaults (no affinity left), lineage kept
    db.record_lineage(&[entry(1, now, true)]).unwrap();
    db.reset_profile().unwrap();
    assert!(db.frecency(now, HL).unwrap().is_empty());
    assert!(db.entities().unwrap().is_empty());
    assert!(db.top_queries(5, now, HL).unwrap().is_empty());
    assert_eq!(db.lineage_all().unwrap().len(), 1);
}

#[test]
fn ux_stats_hit_at_3() {
    let mut db = StateDb::open_in_memory().unwrap();
    let served = vec!["a".to_string(), "b".into(), "c".into(), "d".into()];
    for (sel, typed) in [("a", false), ("c", false), ("d", false), ("d", true)] {
        db.log_impression(&Impression {
            at_ms: 10,
            served: served.clone(),
            selected: Some(sel.into()),
            keystrokes: 2,
            typed,
            reformulated: typed,
            dismissed: false,
        })
        .unwrap();
    }
    let s = db.ux_stats(0).unwrap();
    assert_eq!(s.impressions, 4);
    assert_eq!(s.hit_at_3, Some(0.5));
    assert_eq!(s.reformulation_rate, Some(1.0));
    assert_eq!(s.mean_keystrokes, Some(2.0));
    db.flush().unwrap();
    assert_eq!(db.ux_stats(0).unwrap(), s, "same after flush");
    db.set_machine_profile(&serde_json::json!({"ram": 24}), 5)
        .unwrap();
    assert_eq!(db.machine_profile().unwrap().unwrap().1, 5);
    assert!(db.machine_profile_stale(5 + DAY).unwrap());
    assert!(!db.machine_profile_stale(6).unwrap());
    db.prune(RETENTION_MS + 11).unwrap();
    assert_eq!(db.ux_stats(0).unwrap().impressions, 0);
}

#[test]
fn ux11_test2_five_sessions_make_sd_server_yours() {
    // After 5 sessions of opening sd-server first, it is the top affinity entity (→ "Your things") and
    // Hit@3 ≥ 0.8 for it.
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("state.db");
    let start = 50 * DAY;
    let mut served = vec![
        "chrome".to_string(),
        "gradle".into(),
        "claude".into(),
        "sd".into(),
    ];
    for session in 0..5u64 {
        let mut db = StateDb::open(&path).unwrap();
        let now = start + session * DAY;
        // the served list: ranking lifts sd once it has affinity
        let fr = db.frecency(now, HL).unwrap();
        served.sort_by(|a, b| {
            fr.get(b)
                .unwrap_or(&0.0)
                .partial_cmp(fr.get(a).unwrap_or(&0.0))
                .unwrap()
        });
        db.upsert_entity("sd", "sd-server · qwen-image-studio", now)
            .unwrap();
        db.record_event("sd", EventKind::Select, now, Some("working"))
            .unwrap();
        db.log_impression(&Impression {
            at_ms: now,
            served: served.clone(),
            selected: Some("sd".into()),
            keystrokes: if session == 0 { 4 } else { 1 },
            ..Default::default()
        })
        .unwrap();
        // dropped at the end of the session → flushed on exit
    }
    let db = StateDb::open(&path).unwrap();
    let now = start + 5 * DAY;
    let fr = db.frecency(now, HL).unwrap();
    let top = fr.iter().max_by(|a, b| a.1.partial_cmp(b.1).unwrap()).unwrap();
    assert_eq!(top.0, "sd");
    let s = db.ux_stats(0).unwrap();
    assert_eq!(s.selections, 5);
    assert!(s.hit_at_3.unwrap() >= 0.8, "{s:?}");
}

/// UX §11 test 2 end to end: the served list each session is the **real ranker's** output
/// (`group_candidates_with` → `rank`) with affinity from the store's frecency, and "Your things" comes from
/// `your_things_from_profile`. sd-server is deliberately not among the three largest groups, so only
/// learning can bring it into the top 3.
#[test]
fn ux11_test2_end_to_end_through_the_ranker_and_your_things() {
    use oomtop_core::headline::your_things_from_profile;
    use oomtop_core::modes::Mode;
    use oomtop_core::ranking::{
        affinity_from_frecency, group_candidates_with, rank, RankContext, RankWeights,
        DEFAULT_DIVERSITY_LAMBDA,
    };
    use oomtop_core::{Group, GroupTotals, Measured, Snapshot};
    use std::collections::HashMap;

    const GB: u64 = 1_000_000_000;
    let group = |id: &str, kind: GroupKind, label: &str, fp: &str, bytes: u64| Group {
        id: id.into(),
        kind,
        label: label.into(),
        fingerprint: fp.into(),
        totals: GroupTotals {
            footprint: Measured::exact(bytes, "t"),
            process_count: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut s = Snapshot {
        taken_at_ms: 50 * DAY,
        ..Default::default()
    };
    s.groups = vec![
        group(
            "daemon:kotlin",
            GroupKind::BuildDaemon,
            "KotlinCompileDaemon",
            "fp-kotlin",
            3 * GB,
        ),
        group(
            "app:google-chrome",
            GroupKind::App,
            "Google Chrome",
            "fp-chrome",
            29 * GB / 10,
        ),
        group(
            "daemon:gradle",
            GroupKind::BuildDaemon,
            "GradleDaemon",
            "fp-gradle",
            28 * GB / 10,
        ),
        group(
            "agent:3f2a",
            GroupKind::AgentSession,
            "Claude Code",
            "fp-claude",
            400_000_000,
        ),
        group(
            "model:sd-server",
            GroupKind::ModelServer,
            "sd-server",
            "fp-sd",
            900_000_000,
        ),
        group(
            "other:clang:9",
            GroupKind::Other,
            "clang",
            "fp-clang",
            120_000_000,
        ),
    ];
    let fp_of: HashMap<String, String> = s
        .groups
        .iter()
        .map(|g| (g.id.clone(), g.fingerprint.clone()))
        .collect();

    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("state.db");
    let start = 50 * DAY;
    let mut first_positions = Vec::new();
    for session in 0..5u64 {
        let mut db = StateDb::open(&path).unwrap();
        let now = start + session * DAY;
        let affinity: HashMap<String, f64> = db
            .frecency(now, HL)
            .unwrap()
            .into_iter()
            .map(|(fp, f)| (fp, affinity_from_frecency(f)))
            .collect();
        let ctx = RankContext {
            mode: Mode::Calm,
            affinity: Some(&affinity),
            ..Default::default()
        };
        let cands = group_candidates_with(&s, &ctx);
        let served: Vec<String> = rank(&cands, &RankWeights::default(), DEFAULT_DIVERSITY_LAMBDA)
            .into_iter()
            .map(|(id, _)| fp_of[&id].clone())
            .collect();
        first_positions.push(served.iter().position(|f| f == "fp-sd").unwrap());
        db.upsert_entity("fp-sd", "sd-server", now).unwrap();
        db.record_event("fp-sd", EventKind::Select, now, Some("calm"))
            .unwrap();
        db.log_impression(&Impression {
            at_ms: now,
            served,
            selected: Some("fp-sd".into()),
            keystrokes: 1,
            ..Default::default()
        })
        .unwrap();
    }
    assert!(
        first_positions[0] >= 3,
        "cold: sd-server is not top-3 by size ({first_positions:?})"
    );
    let db = StateDb::open(&path).unwrap();
    let now = start + 5 * DAY;
    let affinity: HashMap<String, f64> = db
        .frecency(now, HL)
        .unwrap()
        .into_iter()
        .map(|(fp, f)| (fp, affinity_from_frecency(f)))
        .collect();
    let chips = your_things_from_profile(&s, &[], &affinity);
    assert!(
        chips.iter().any(|c| c.contains("sd-server")),
        "sd-server is in Your things: {chips:?}"
    );
    let stats = db.ux_stats(0).unwrap();
    assert_eq!(stats.selections, 5);
    assert!(
        stats.hit_at_3.unwrap() >= 0.8,
        "Hit@3 from the ranker's served lists {first_positions:?}: {stats:?}"
    );
}

#[test]
fn batching_defers_writes_and_drop_flushes() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("state.db");
    {
        let mut db = StateDb::open(&path).unwrap();
        db.record_event("sd", EventKind::Select, 1000, None).unwrap();
        db.record_lineage(&[entry(3, 1000, false)]).unwrap();
        db.upsert_entity("sd", "sd-server", 1000).unwrap();
        db.record_query("gpu hogs", 1000).unwrap();
        assert_eq!(db.pending_len(), 4, "queued, not written");
        // a second connection (another oomtop instance) does not see queued rows yet
        let other = StateDb::open(&path).unwrap();
        assert!(other.frecency(2000, HL).unwrap().is_empty());
        assert!(other.lineage_all().unwrap().is_empty());
        // but the writer sees its own writes
        assert_eq!(db.frecency(2000, HL).unwrap().len(), 1);
        assert_eq!(db.entities().unwrap()[0].display_name, "sd-server");
    }
    let db = StateDb::open(&path).unwrap();
    assert_eq!(db.frecency(2000, HL).unwrap().len(), 1, "flushed on drop");
    assert_eq!(db.lineage_all().unwrap().len(), 1);
    assert_eq!(db.top_queries(3, 2000, HL).unwrap()[0].0, "gpu hogs");
    // write-through mode
    let mut db = StateDb::open_in_memory().unwrap();
    db.set_batch_interval(Duration::ZERO);
    db.record_event("x", EventKind::Select, 1, None).unwrap();
    assert_eq!(db.pending_len(), 0);
}

#[test]
fn no_learn_records_nothing_personal() {
    let mut db = StateDb::open_in_memory().unwrap();
    db.record_event("queued", EventKind::Select, 5, None).unwrap();
    db.set_learning(false);
    assert!(!db.learning());
    assert_eq!(db.pending_len(), 0, "queued learning signals are dropped");
    db.record_event("sd", EventKind::Select, 10, None).unwrap();
    db.upsert_entity("sd", "sd-server", 10).unwrap();
    db.record_query("mem>2G", 10).unwrap();
    db.log_impression(&Impression {
        at_ms: 10,
        selected: Some("sd".into()),
        ..Default::default()
    })
    .unwrap();
    db.set_pinned("claude", true, 10).unwrap();
    db.record_lineage(&[entry(2, 10, true)]).unwrap();
    db.flush().unwrap();
    assert!(
        db.frecency(20, HL).unwrap().is_empty(),
        "no events, not even the pin event"
    );
    assert_eq!(db.ux_stats(0).unwrap().impressions, 0);
    assert!(db.top_queries(5, 20, HL).unwrap().is_empty());
    assert_eq!(
        db.pinned().unwrap(),
        vec!["claude".to_string()],
        "explicit pin kept"
    );
    assert_eq!(
        db.lineage_all().unwrap().len(),
        1,
        "lineage is operational, still recorded"
    );
}

#[test]
fn log_is_capped_by_size_and_age() {
    let mut db = StateDb::open_in_memory().unwrap();
    db.set_batch_interval(Duration::from_secs(3600));
    let now = 40 * DAY;
    let served: Vec<String> = (0..10).map(|i| format!("{:016x}", i * 7919)).collect();
    for i in 0..4000u64 {
        db.log_impression(&Impression {
            at_ms: now - DAY + i,
            served: served.clone(),
            selected: Some(served[0].clone()),
            ..Default::default()
        })
        .unwrap();
        db.record_event(&served[1], EventKind::Select, now - DAY + i, Some("calm"))
            .unwrap();
    }
    // old rows beyond retention
    db.record_event("old", EventKind::Select, now - 31 * DAY, None)
        .unwrap();
    db.record_event("muted-old", EventKind::Mute, now - 31 * DAY, None)
        .unwrap();
    db.flush().unwrap();
    let before = db.log_bytes().unwrap();
    assert!(before > 600_000, "{before}");
    let cap = 256 * 1024;
    let rep = db.prune_with(now, RETENTION_MS, cap).unwrap();
    assert!(
        db.log_bytes().unwrap() <= cap,
        "{} > {cap}",
        db.log_bytes().unwrap()
    );
    assert!(rep.impressions > 0);
    let f = db.frecency(now, HL).unwrap();
    assert!(!f.contains_key("old"), "positive events past retention are gone");
    assert!(
        f.contains_key("muted-old"),
        "negative signals decay on their own half-life"
    );
    // the newest impressions survive
    let newest: i64 = db
        .conn
        .query_row("SELECT MAX(at_ms) FROM impression", [], |r| r.get(0))
        .unwrap();
    assert_eq!(newest as u64, now - DAY + 3999);
    // default cap is 5 MB
    assert_eq!(LOG_CAP_BYTES, 5 * 1024 * 1024);
}

#[test]
fn queries_are_redacted() {
    let mut db = StateDb::open_in_memory().unwrap();
    db.record_query("name:x --token=abcd1234 mem>2G", 1).unwrap();
    let q = db.top_queries(1, 1, HL).unwrap();
    assert!(!q[0].0.contains("abcd1234"), "{}", q[0].0);
    assert!(q[0].0.contains("mem>2G"));
    db.record_query("   ", 1).unwrap();
    assert_eq!(db.top_queries(5, 1, HL).unwrap().len(), 1);
}

#[test]
fn newer_schema_is_refused_and_file_is_private() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("state.db");
    drop(StateDb::open(&path).unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    {
        let c = Connection::open(&path).unwrap();
        c.execute_batch("PRAGMA user_version = 99;").unwrap();
    }
    let e = StateDb::open(&path).unwrap_err();
    assert!(matches!(e, StateError::NewerSchema { found: 99, .. }), "{e}");
}

#[test]
fn default_path_is_xdg() {
    let p = default_path();
    assert!(p.ends_with("oomtop/state.db"), "{}", p.display());
}

fn golden_db(now: u64) -> StateDb {
    let mut db = StateDb::open_in_memory().unwrap();
    db.upsert_entity("a1b2c3d4e5f60718", "sd-server · qwen-image-studio", now - 3 * DAY)
        .unwrap();
    db.upsert_entity("0f1e2d3c4b5a6978", "Claude Code", now - 2 * DAY)
        .unwrap();
    db.upsert_entity("99aa88bb77cc66dd", "build daemons", now - DAY)
        .unwrap();
    for i in 0..3 {
        db.record_event(
            "a1b2c3d4e5f60718",
            EventKind::Select,
            now - i * DAY,
            Some("working"),
        )
        .unwrap();
    }
    db.record_event("0f1e2d3c4b5a6978", EventKind::SearchSelect, now - DAY, None)
        .unwrap();
    db.set_pinned("a1b2c3d4e5f60718", true, now - DAY).unwrap();
    db.set_muted("99aa88bb77cc66dd", Some(now + 30 * DAY), now)
        .unwrap();
    db.rename("a1b2c3d4e5f60718", Some("studio"), now).unwrap();
    db.record_query("gpu hogs", now - DAY).unwrap();
    db.record_query("cl", now).unwrap();
    db.record_query("cl", now).unwrap();
    for (sel, typed) in [("a1b2c3d4e5f60718", false), ("0f1e2d3c4b5a6978", true)] {
        db.log_impression(&Impression {
            at_ms: now - DAY,
            served: vec!["99aa88bb77cc66dd".into(), "a1b2c3d4e5f60718".into()],
            selected: Some(sel.into()),
            keystrokes: 3,
            typed,
            reformulated: false,
            dismissed: false,
        })
        .unwrap();
    }
    db.set_machine_profile(
        &serde_json::json!({"ram_gb": 24, "unified_memory": true, "fanless": true, "roles": ["local-llm", "agent-heavy", "jvm"]}),
        now,
    )
    .unwrap();
    db
}

fn round_floats(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Number(n) if n.is_f64() => {
            let f = (n.as_f64().unwrap() * 1e6).round() / 1e6;
            *v = serde_json::json!(f);
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(round_floats),
        serde_json::Value::Object(o) => o.values_mut().for_each(round_floats),
        _ => {}
    }
}

#[test]
fn export_golden() {
    let now = 200 * DAY;
    let db = golden_db(now);
    let mut v = db.export_profile(now, HL).unwrap();
    round_floats(&mut v);
    insta::assert_json_snapshot!("profile_export", v);
}

#[test]
fn show_golden() {
    let now = 200 * DAY;
    let db = golden_db(now);
    let s = db.summary(now, HL).unwrap();
    assert_eq!(s.pinned.len(), 1);
    assert_eq!(s.pinned[0].name, "studio");
    assert_eq!(s.muted[0].name, "build daemons");
    assert_eq!(s.top[0].name, "studio");
    insta::assert_snapshot!("profile_show", s.render_text(now));
}

#[cfg(unix)]
#[test]
fn wal_shm_and_dir_are_private_too() {
    // regression: the db was chmod-ed after SQLite created it (a 0644 window) and the -wal/-shm files, which
    // hold the same data, kept the umask default
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().join("state/oomtop");
    let path = dir.join("state.db");
    let mut db = StateDb::open(&path).unwrap();
    db.set_batch_interval(Duration::ZERO);
    db.upsert_entity("fp", "sd-server", 1).unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    for suffix in ["-wal", "-shm"] {
        let mut p = path.as_os_str().to_owned();
        p.push(suffix);
        let p = PathBuf::from(p);
        assert!(p.exists(), "{} exists while open (WAL mode)", p.display());
        assert_eq!(mode(&p), 0o600, "{}", p.display());
    }
    assert_eq!(mode(&dir), 0o700);
    drop(db);
    // an existing world-readable store is tightened on open
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    drop(StateDb::open(&path).unwrap());
    assert_eq!(mode(&path), 0o600);
}

#[test]
fn failed_flush_keeps_the_batch() {
    // regression: the queue was taken before the transaction, so an error (SQLITE_BUSY from another oomtop,
    // disk full) silently dropped up to 30 s of lineage and learning
    let mut db = StateDb::open_in_memory().unwrap();
    db.set_batch_interval(Duration::from_secs(3600));
    db.record_lineage(&[entry(7, 100, true)]).unwrap();
    db.record_query("gpu hogs", 5).unwrap();
    db.conn
        .execute_batch("ALTER TABLE query RENAME TO query_gone;")
        .unwrap();
    assert!(db.flush().is_err());
    assert_eq!(db.pending_len(), 2, "batch is kept for the next flush");
    // nothing half-written: the transaction rolled back the lineage insert too
    let n: i64 = db
        .conn
        .query_row("SELECT COUNT(*) FROM lineage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    db.conn
        .execute_batch("ALTER TABLE query_gone RENAME TO query;")
        .unwrap();
    db.flush().unwrap();
    assert_eq!(db.pending_len(), 0);
    assert_eq!(db.lineage_all().unwrap().len(), 1);
    assert_eq!(db.top_queries(5, 5, HL).unwrap()[0].0, "gpu hogs");
}

#[test]
fn prune_converges_quickly_far_over_cap() {
    let mut db = StateDb::open_in_memory().unwrap();
    db.set_batch_interval(Duration::from_secs(3600));
    let now = 40 * DAY;
    let served: Vec<String> = (0..10).map(|i| format!("{:016x}", i)).collect();
    for i in 0..4000u64 {
        db.log_impression(&Impression {
            at_ms: now - DAY + i,
            served: served.clone(),
            ..Default::default()
        })
        .unwrap();
    }
    db.flush().unwrap();
    let cap = 16 * 1024;
    let start = Instant::now();
    let rep = db.prune_with(now, RETENTION_MS, cap).unwrap();
    assert!(db.log_bytes().unwrap() <= cap);
    // mean-size batching deletes roughly the overflow, not the whole log
    let left = db.count("impression").unwrap();
    assert!(
        left > 0 && rep.impressions > 3500,
        "left {left}, deleted {}",
        rep.impressions
    );
    assert!(start.elapsed() < Duration::from_secs(2), "{:?}", start.elapsed());
}

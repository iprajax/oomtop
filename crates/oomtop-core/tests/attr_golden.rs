//! Golden files (insta) for attribution decisions, fingerprint templates and export redaction.
//! Review changes with `cargo insta review` (or `INSTA_UPDATE=always` + `git diff`).

mod attr_support;

use attr_support::*;
use oomtop_core::fingerprint::{cmdline_template, display_name, process_template};
use oomtop_core::redact::{redact_cmdline, redact_snapshot};
use serde::Serialize;

#[test]
fn golden_macos_motivating_machine() {
    let (s, lin) = mac_machine();
    let s = enrich(s, &lin, 45);
    insta::assert_yaml_snapshot!("attribution_macos", view(&s));
}

#[test]
fn golden_linux_desktop() {
    let (s, lin) = linux_machine();
    let s = enrich(s, &lin, 45);
    insta::assert_yaml_snapshot!("attribution_linux", view(&s));
}

#[derive(Serialize)]
struct TemplateRow {
    pid: u32,
    template: String,
    display: String,
}

#[test]
fn golden_templates() {
    let (mac, _) = mac_machine();
    let (linux, _) = linux_machine();
    let rows: Vec<TemplateRow> = mac
        .processes
        .iter()
        .chain(linux.processes.iter())
        .map(|p| TemplateRow {
            pid: p.id.pid,
            template: process_template(p),
            display: display_name(p),
        })
        .collect();
    insta::assert_yaml_snapshot!("templates", rows);
}

#[test]
fn fingerprints_are_stable_across_restarts_ports_and_versions() {
    let (a, lin) = mac_machine();
    let a = enrich(a, &lin, 45);
    // the same machine after a reboot: new pids/start times, sd-server on another port, Gradle 9.8.1
    let (mut b, _) = mac_machine();
    for p in &mut b.processes {
        p.id.start_time += 86_400_000;
        for arg in &mut p.cmdline {
            *arg = arg.replace("7861", "7870").replace("9.8.0", "9.8.1");
        }
    }
    b.taken_at_ms += 86_400_000;
    let b = enrich(b, &Default::default(), 45);
    for label in [
        "sd-server",
        "GradleDaemon",
        "KotlinCompileDaemon",
        "Claude",
        "Google Chrome",
        "start.sh",
    ] {
        let fa = &a.groups.iter().find(|g| g.label == label).unwrap().fingerprint;
        let fb = &b.groups.iter().find(|g| g.label == label).unwrap().fingerprint;
        assert_eq!(fa, fb, "{label}");
        assert_eq!(fa.len(), 16);
    }
    // four sessions in four projects → four distinct session entities, stable across restarts
    let fps = |s: &oomtop_core::Snapshot| {
        let mut v: Vec<String> = s
            .groups
            .iter()
            .filter(|g| g.kind == oomtop_core::GroupKind::AgentSession)
            .map(|g| g.fingerprint.clone())
            .collect();
        v.sort();
        v
    };
    assert_eq!(fps(&a).len(), 4);
    assert_eq!(fps(&a), fps(&b));
    let mut d = fps(&a);
    d.dedup();
    assert_eq!(d.len(), 4);
}

#[test]
fn secrets_never_reach_templates_or_exports() {
    let argv: Vec<String> = [
        "/usr/local/bin/server",
        "--api-key",
        "sk-proj-AAAAAAAAAAAAAAAAAAAAAAAA",
        "--token=ghp_abcdefghijklmnopqrstuvwxyz0123",
        "OPENAI_API_KEY=sk-live-secret-value-123456",
        "--db",
        "postgres://admin:hunter2@127.0.0.1:5432/app",
        "--webhook",
        "https://hooks.example.com/x?sig=abcdef0123456789&x=1",
        "-H",
        "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.c2lnbmF0dXJlZ29lc2hlcmU",
        "--session-id",
        "2f1c3a4b-1111-2222-3333-444455556666",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let secrets = [
        "sk-proj-AAAA",
        "ghp_abc",
        "sk-live-secret",
        "hunter2",
        "sig=abcdef",
        "eyJhbGci",
        "2f1c3a4b-1111",
    ];
    let t = cmdline_template(&argv, None);
    let r = redact_cmdline(&argv).join(" ");
    for s in secrets {
        assert!(!t.contains(s), "template leaks {s}: {t}");
        assert!(!r.contains(s), "export leaks {s}: {r}");
    }
    insta::assert_yaml_snapshot!("redaction", (t, redact_cmdline(&argv)));

    // whole-snapshot export: argv redacted, marker values only as hashes
    let (mut s, lin) = mac_machine();
    s.processes[5].cmdline = argv.clone();
    let s = enrich(s, &lin, 45);
    let json = serde_json::to_string(&redact_snapshot(&s)).unwrap();
    for sec in secrets {
        assert!(!json.contains(sec), "export leaks {sec}");
    }
    for sess in SESSIONS {
        assert!(!json.contains(sess), "raw session id leaked");
    }
}

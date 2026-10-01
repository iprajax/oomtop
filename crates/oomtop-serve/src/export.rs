//! Export redaction (SPEC §13): every JSON body and the /why causes are built from this view.
//!
//! `oomtop_core::redact::redact_snapshot` covers process command lines, exe/cwd paths, model files and
//! endpoints. Human labels are derived from command lines too (group labels, sandbox labels, the likely
//! OOM victim, loaded model names, job labels), and they flow into the headline summary and `why`
//! evidence — so they are redacted here too (defence in depth; idempotent with the core redaction).

use oomtop_core::redact::{redact_snapshot, redact_text};
use oomtop_core::Snapshot;

/// A redacted copy of `s` that is safe to export. Idempotent. Ids, pids and numbers are unchanged.
pub fn export_snapshot(s: &Snapshot) -> Snapshot {
    let mut out = redact_snapshot(s);
    for p in &mut out.processes {
        p.name = redact_text(&p.name);
    }
    for g in &mut out.groups {
        g.label = redact_text(&g.label);
    }
    for b in &mut out.sandboxes {
        b.label = redact_text(&b.label);
    }
    for m in &mut out.model_servers {
        for model in &mut m.models {
            model.name = redact_text(&model.name);
        }
        if let Some(p) = &mut m.progress {
            p.label = redact_text(&p.label);
        }
    }
    if let Some(v) = &mut out.oom.likely_victim {
        v.name = redact_text(&v.name);
    }
    for k in &mut out.oom.recent_kills {
        if let Some(n) = &mut k.victim_name {
            *n = redact_text(n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oomtop_core::{Group, LoadedModel, ModelServer, Sandbox, Victim};

    #[test]
    fn labels_are_redacted_and_the_view_is_idempotent() {
        let secret = "sk-ant-api03-verysecretvalue1234567890abcdef";
        let mut s = Snapshot::default();
        s.groups.push(Group {
            id: "other:train:7".into(),
            label: format!("train.py --token={secret}"),
            ..Default::default()
        });
        s.sandboxes.push(Sandbox {
            label: format!("API_KEY={secret}"),
            ..Default::default()
        });
        s.model_servers.push(ModelServer {
            models: vec![LoadedModel {
                name: format!("https://u:{secret}@hf.example/m"),
                ..Default::default()
            }],
            ..Default::default()
        });
        s.oom.likely_victim = Some(Victim {
            name: format!("--api-key={secret}"),
            ..Default::default()
        });
        let e = export_snapshot(&s);
        let text = serde_json::to_string(&e).unwrap();
        assert!(!text.contains(secret), "{text}");
        assert_eq!(e.groups[0].id, "other:train:7", "ids are untouched");
        assert!(e.groups[0].label.starts_with("train.py"), "{}", e.groups[0].label);
        assert_eq!(export_snapshot(&e), e, "idempotent");
    }
}

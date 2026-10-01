//! Fixture capture tool (`tools/capture`, SPEC §17): records `RawSample` frames from this machine into a
//! fixture file with secrets redacted.
//!
//! ```text
//! cargo run -p oomtop-collect --example capture -- fixtures/macos/m5-air.json --frames 3 --interval-ms 2000 \
//!     --name m5-air --description "M5 Air, 4 agent sessions" --note "thermal: nominal" --note "battery: 80%"
//! ```
//! Redaction: argv through `oomtop_core::redact::redact_cmdline`; home directories → `/Users/<user>` /
//! `/home/<user>`; hostname → `<host>`. Environments are never captured (Sources store hashed markers only).

use oomtop_collect::raw::{RawPayload, RawSample};
use oomtop_collect::replay::{fixture_to_json, Fixture, FixtureMeta};
use oomtop_collect::{Sampler, SamplerOptions};
use oomtop_core::redact::redact_cmdline;
use std::time::Duration;

fn scrub_home(s: &str) -> String {
    let mut out = s.to_string();
    for prefix in ["/Users/", "/home/", "-Users-", "-home-"] {
        let mut result = String::new();
        let mut rest = out.as_str();
        while let Some(i) = rest.find(prefix) {
            result.push_str(&rest[..i + prefix.len()]);
            let after = &rest[i + prefix.len()..];
            let sep = if prefix.starts_with('-') { '-' } else { '/' };
            let end = after.find(sep).unwrap_or(after.len());
            let user = &after[..end];
            result.push_str(if user == "Shared" { user } else { "<user>" });
            rest = &after[end..];
        }
        result.push_str(rest);
        out = result;
    }
    out
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "ksh"];

/// Fixture-grade argv redaction: secrets (core), shell `-c` scripts, very long args, home paths (also the
/// dash-encoded `-Users-<name>-` form used by agent project dirs).
fn redact_argv(argv: &[String]) -> Vec<String> {
    let is_shell = argv
        .first()
        .map(|a| SHELLS.contains(&a.rsplit('/').next().unwrap_or(a).trim_start_matches('-')))
        .unwrap_or(false);
    let mut out = Vec::with_capacity(argv.len());
    let mut script_next = false;
    for a in redact_cmdline(argv) {
        if script_next {
            out.push("<script>".to_string());
            script_next = false;
            continue;
        }
        script_next =
            is_shell && (a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.ends_with('c')));
        let a = scrub_home(&a);
        out.push(if a.len() > 200 {
            "<long-arg>".to_string()
        } else {
            a
        });
    }
    out
}

fn redact_sample(raw: &mut RawSample) {
    match &mut raw.payload {
        RawPayload::MacProcs(ps) => {
            for p in &mut ps.procs {
                p.path = scrub_home(&p.path);
                if let Some(a) = &mut p.argv {
                    *a = redact_argv(a);
                }
            }
        }
        RawPayload::MacHost(h) => {
            if h.sysctl_str.contains_key("kern.hostname") {
                h.sysctl_str.insert("kern.hostname".into(), "<host>".into());
            }
        }
        RawPayload::LinuxFiles(f) => {
            let keys: Vec<String> = f.files.keys().cloned().collect();
            for k in keys {
                let Some(v) = f.files.get_mut(&k) else { continue };
                if k.ends_with("/cmdline") {
                    let argv: Vec<String> = v
                        .split('\0')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    *v = redact_argv(&argv).join("\0");
                } else if k.ends_with("/exe") || k.ends_with("/cwd") {
                    *v = scrub_home(v);
                } else if k == "sys/kernel/hostname" {
                    *v = "<host>\n".into();
                }
            }
        }
        RawPayload::Unavailable { .. } => {}
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out = None;
    let mut frames = 2usize;
    let mut interval = 2000u64;
    let mut meta = FixtureMeta {
        os: std::env::consts::OS.into(),
        ..Default::default()
    };
    let mut i = 0;
    while i < args.len() {
        let val = || args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--frames" => {
                frames = val().parse().unwrap_or(2);
                i += 1;
            }
            "--interval-ms" => {
                interval = val().parse().unwrap_or(2000);
                i += 1;
            }
            "--name" => {
                meta.name = val();
                i += 1;
            }
            "--description" => {
                meta.description = val();
                i += 1;
            }
            "--note" => {
                meta.notes.push(val());
                i += 1;
            }
            p => out = Some(p.to_string()),
        }
        i += 1;
    }
    let Some(out) = out else {
        eprintln!("usage: capture <out.json> [--frames N] [--interval-ms MS] [--name N] [--description D] [--note N]…");
        std::process::exit(2);
    };
    let mut sampler = Sampler::platform_default(&SamplerOptions::default());
    let mut fx = Fixture {
        meta,
        frames: Vec::new(),
    };
    for f in 0..frames {
        sampler.sample_all();
        let mut frame = sampler.last_raw();
        for r in &mut frame {
            redact_sample(r);
        }
        fx.meta.captured_at_ms = fx
            .meta
            .captured_at_ms
            .max(frame.iter().map(|r| r.taken_at_ms).max().unwrap_or(0));
        fx.frames.push(frame);
        if f + 1 < frames {
            std::thread::sleep(Duration::from_millis(interval));
        }
    }
    if let Err(e) = std::fs::write(&out, fixture_to_json(&fx)) {
        eprintln!("capture: writing {out}: {e}");
        std::process::exit(1);
    }
    let procs: usize = fx
        .frames
        .last()
        .map(|f| {
            f.iter()
                .map(|r| match &r.payload {
                    RawPayload::MacProcs(p) => p.procs.len(),
                    RawPayload::LinuxFiles(l) => l.files.keys().filter(|k| k.ends_with("/stat")).count(),
                    _ => 0,
                })
                .sum()
        })
        .unwrap_or(0);
    eprintln!(
        "capture: wrote {out} ({} frames, {procs} processes in the last frame)",
        fx.frames.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_scripts_and_long_args() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            redact_argv(&v(&["/bin/zsh", "-c", "echo secret"])),
            v(&["/bin/zsh", "-c", "<script>"])
        );
        assert_eq!(redact_argv(&v(&["-zsh"])), v(&["-zsh"]));
        assert_eq!(
            redact_argv(&v(&["node", &"x".repeat(300)])),
            v(&["node", "<long-arg>"])
        );
        assert_eq!(
            scrub_home("/Users/<user>/.claude/projects/-Users-bob-ws-x"),
            "/Users/<user>/.claude/projects/-Users-<user>-ws-x"
        );
    }

    #[test]
    fn scrubs_home_dirs() {
        assert_eq!(
            scrub_home("/Users/alex/x/Users/bob/y"),
            "/Users/<user>/x/Users/<user>/y"
        );
        assert_eq!(scrub_home("/home/alice"), "/home/<user>");
        assert_eq!(scrub_home("/Users/Shared/z"), "/Users/Shared/z");
    }
}

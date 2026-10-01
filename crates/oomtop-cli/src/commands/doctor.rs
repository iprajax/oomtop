//! `oomtop doctor` (SPEC §12.2, UX §12.10): every Source's status and why, terminal capabilities (probed
//! live when interactive, else inferred from the environment and labelled as such), config files and
//! errors, rules, state store and privileges.

use super::{json_pretty, out, Ctx};
use crate::termprobe::{self, ModeState, Probe};
use anyhow::Result;
use oomtop_core::provider::SnapshotProvider;
use oomtop_core::SourceStatus;
use oomtop_tui::caps::TermCaps;
use serde::Serialize;

/// What each source provides (prefix match on the source name).
const PROVIDES: &[(&str, &str)] = &[
    (
        "macos.host",
        "RAM, vm_statistics64, compressor, swap, memorystatus level",
    ),
    (
        "macos.procs",
        "per-process phys_footprint, CPU, lineage (proc_pid_rusage)",
    ),
    ("macos.thermal", "thermal pressure level, Low Power Mode"),
    ("macos.power", "battery, AC adapter"),
    (
        "macos.ioreport",
        "GPU/ANE residency, frequency, package power (IOReport)",
    ),
    ("macos.ioaccel", "GPU model, cores, in-use memory (IOAccelerator)"),
    ("macos.jetsam", "recent jetsam kills (DiagnosticReports)"),
    ("linux.host", "meminfo, PSI, vmstat, swap"),
    ("linux.psi", "pressure stall information (memory/cpu/io)"),
    (
        "linux.markers",
        "agent session markers (allowlisted env keys, hashed)",
    ),
    ("linux.cpufreq", "CPU frequency per core (throttle detection)"),
    (
        "linux.procs",
        "per-process PSS/SwapPss (smaps_rollup), CPU, oom_score",
    ),
    ("linux.cgroup", "cgroup v2 memory limits and events"),
    (
        "linux.oom",
        "kernel OOM kills, systemd-oomd / earlyoom thresholds",
    ),
    (
        "linux.nvml",
        "NVIDIA GPU memory, utilization, clocks, throttle reasons (NVML)",
    ),
    ("linux.drm", "per-process GPU memory (drm fdinfo)"),
    ("linux.gpu", "GPU memory and utilization"),
    ("linux.thermal", "thermal zones, trip points, CPU frequency"),
    ("linux.powercap", "RAPL package power"),
    ("linux.power_supply", "battery, AC adapter"),
    ("adapter.", "model-server API (loaded models, busy, queue)"),
    ("sandbox.", "containers / VMs"),
];

/// Sources a live sampler on each OS is expected to report (SPEC §5/§9). Any that is absent from the
/// snapshot's `source_status` is shown as `missing`, so a collector this build lacks is visible, not silent.
pub const EXPECTED_MACOS: &[&str] = &[
    "macos.host",
    "macos.procs",
    "macos.thermal",
    "macos.power",
    "macos.ioreport",
    "macos.ioaccel",
    "macos.jetsam",
];
pub const EXPECTED_LINUX: &[&str] = &[
    "linux.host",
    "linux.procs",
    "linux.psi",
    "linux.cgroup",
    "linux.oom",
    // NVML reports under its own name only where an NVIDIA driver exists; elsewhere `linux.gpu` already
    // says "no NVML device", so it is not expected (a `missing` row on every non-NVIDIA box was noise).
    "linux.gpu",
    "linux.thermal",
    "linux.cpufreq",
    "linux.powercap",
    "linux.power_supply",
];

fn expected_for(os: oomtop_core::OsKind) -> &'static [&'static str] {
    match os {
        oomtop_core::OsKind::Macos => EXPECTED_MACOS,
        oomtop_core::OsKind::Linux => EXPECTED_LINUX,
        _ => &[],
    }
}

/// Rows for every reported source, plus `missing` rows for expected sources that weren't reported (live
/// only: a fixture records whatever its machine had).
fn source_rows(s: &oomtop_core::Snapshot, live: bool) -> Vec<SourceRow> {
    let mut rows: Vec<SourceRow> = s.source_status.iter().map(|(k, v)| status_row(k, v)).collect();
    if live {
        for name in expected_for(s.host.os) {
            if !s.source_status.contains_key(*name) {
                rows.push(SourceRow {
                    name: name.to_string(),
                    status: "missing",
                    reason: Some("not reported by this build's sampler".into()),
                    provides: provides(name),
                    hint: Some("this collector isn't part of this build (or found nothing to report); what it provides shows as unavailable"),
                });
            }
        }
    }
    rows
}

fn provides(name: &str) -> &'static str {
    PROVIDES
        .iter()
        .find(|(p, _)| name.starts_with(p))
        .map(|(_, d)| *d)
        .unwrap_or("")
}

/// A fix hint for an unavailable/partial reason.
pub fn hint(name: &str, reason: &str) -> Option<&'static str> {
    let r = reason.to_ascii_lowercase();
    let needs_root = [
        "permission",
        "eperm",
        "not permitted",
        "eacces",
        "need root",
        "needs root",
    ];
    if needs_root.iter().any(|k| r.contains(k)) {
        return Some("some data needs root: `sudo oomtop doctor` shows what extra detail it would add");
    }
    if name.contains("nvml") || r.contains("libnvidia-ml") {
        return Some("no NVIDIA driver/GPU (or a musl build, which cannot dlopen NVML: use the glibc build)");
    }
    if name.starts_with("adapter.")
        && (r.contains("refused") || r.contains("timed out") || r.contains("timeout"))
    {
        return Some(
            "the model server's API isn't reachable on loopback; check adapters.ports, or --offline",
        );
    }
    if r.contains("offline") {
        return Some("adapter HTTP probes are off (--offline or adapters.enabled = false)");
    }
    if r.contains("two samples") || r.contains("initializing") {
        return Some("a rate needs two readings: it appears after the next refresh (live TUI / ndjson)");
    }
    if r.contains("time budget") {
        return Some("skipped to stay inside the 50 ms per-sample budget; it is retried every refresh");
    }
    if (r.contains("ioreport") || name.contains("ioreport"))
        && (r.contains("symbol") || r.contains("loadable") || r.contains("channel"))
    {
        return Some(
            "IOReport symbols not found on this macOS; GPU/power stay unavailable, everything else works",
        );
    }
    if r.contains("cgroup") && r.contains("v1") {
        return Some("cgroup v1 hosts report host totals only; cgroup v2 is needed for limits");
    }
    if r.contains("idle") {
        return Some("measured only under load (nothing is busy right now)");
    }
    None
}

#[derive(Debug, Serialize)]
struct SourceRow {
    name: String,
    status: &'static str,
    reason: Option<String>,
    provides: &'static str,
    hint: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct Capability {
    name: &'static str,
    /// `None` = unknown.
    supported: Option<bool>,
    /// "probe" (the terminal answered), "environment" (inferred from TERM/COLORTERM/TERM_PROGRAM),
    /// "not probed".
    via: &'static str,
    detail: String,
}

fn mode(
    p: Option<&Probe>,
    get: impl Fn(&Probe) -> Option<ModeState>,
    env_guess: Option<bool>,
) -> (Option<bool>, &'static str) {
    match p {
        // Not asked (e.g. Apple Terminal, which prints DECRQM instead of answering): keep the guess.
        Some(p) if !p.modes_queried && get(p).is_none() => (env_guess, "environment"),
        Some(p) if p.da1 => (Some(get(p).is_some_and(|m| m.supported())), "probe"),
        Some(p) => match get(p) {
            Some(m) => (Some(m.supported()), "probe"),
            None => (env_guess, "environment"),
        },
        None => (env_guess, "environment"),
    }
}

fn capabilities(caps: &TermCaps, probe: Option<&Probe>, tried: bool) -> Vec<Capability> {
    let mut v = Vec::new();
    let env_truecolor = caps.colorterm == "truecolor" || caps.colorterm == "24bit";
    let env_detail = if caps.colorterm.is_empty() {
        "COLORTERM unset (set COLORTERM=truecolor if your terminal supports 24-bit color)".to_string()
    } else {
        format!("COLORTERM={}", caps.colorterm)
    };
    let (truecolor, tc_via, tc_detail) = match probe.and_then(|p| p.truecolor) {
        Some(yes) => (
            yes,
            "probe",
            format!(
                "DECRQSS {} the 24-bit test color · {env_detail}",
                if yes { "kept" } else { "downsampled" }
            ),
        ),
        None => (env_truecolor, "environment", env_detail),
    };
    v.push(Capability {
        name: "truecolor",
        supported: Some(truecolor),
        via: tc_via,
        detail: if tc_via == "probe" && truecolor && !env_truecolor {
            format!("{tc_detail} (export COLORTERM=truecolor so other programs know too)")
        } else {
            tc_detail
        },
    });
    v.push(Capability {
        name: "color depth",
        supported: Some(caps.depth != oomtop_tui::style::ColorDepth::None),
        via: "environment",
        detail: format!(
            "{}{}",
            caps.depth.as_str(),
            if caps.no_color { " (NO_COLOR set)" } else { "" }
        ),
    });
    let kitty_env = caps.term.contains("kitty")
        || matches!(caps.term_program.as_str(), "WezTerm" | "ghostty")
        || caps.term.contains("ghostty");
    let (kk, kk_via) = match probe {
        Some(p) if p.kitty_keyboard.is_some() => (Some(true), "probe"),
        Some(p) if p.da1 && p.modes_queried => (Some(false), "probe"),
        _ => (Some(kitty_env), "environment"),
    };
    v.push(Capability {
        name: "kitty keyboard",
        supported: kk,
        via: kk_via,
        detail: match (probe, probe.and_then(|p| p.kitty_keyboard)) {
            (_, Some(f)) => format!("CSI ? u answered, flags {f}"),
            (Some(p), None) if p.modes_queried => {
                "CSI ? u not answered: modifier-only keys fall back to legacy encoding".into()
            }
            (Some(_), None) => format!(
                "not asked (TERM_PROGRAM={} prints this query instead of answering); guessed from TERM",
                caps.term_program
            ),
            (None, None) => format!(
                "guessed from TERM={} TERM_PROGRAM={}",
                if caps.term.is_empty() { "-" } else { &caps.term },
                if caps.term_program.is_empty() {
                    "-"
                } else {
                    &caps.term_program
                }
            ),
        },
    });
    let (sync, sync_via) = mode(probe, |p| p.sync_output, Some(caps.sync_output));
    v.push(Capability {
        name: "synchronized output",
        supported: sync,
        via: sync_via,
        detail: "DEC mode 2026 (tear-free redraws)".into(),
    });
    v.push(Capability {
        name: "OSC 8 hyperlinks",
        supported: Some(caps.osc8),
        via: "environment",
        detail: if caps.term_program.is_empty() {
            format!("TERM={} (no query exists for OSC 8)", caps.term)
        } else {
            format!("TERM_PROGRAM={} (no query exists for OSC 8)", caps.term_program)
        },
    });
    let bg = probe.and_then(|p| p.background);
    v.push(Capability {
        name: "OSC 11 background",
        supported: match probe {
            Some(p) if p.da1 || bg.is_some() => Some(bg.is_some()),
            _ => None,
        },
        via: if probe.is_some() { "probe" } else { "not probed" },
        detail: match bg {
            Some(rgb) => format!(
                "{} → {} variant",
                probe.and_then(|p| p.background_hex()).unwrap_or_default(),
                oomtop_config::theme::Variant::for_background(rgb).as_str()
            ),
            None if !tried => {
                "not a terminal (or --no-probe): light/dark follows appearance.appearance".into()
            }
            None => {
                "no answer: set appearance.appearance = \"light\" or \"dark\" to pick theme variants".into()
            }
        },
    });
    let (mouse, mouse_via) = mode(probe, |p| p.sgr_mouse, Some(caps.mouse));
    v.push(Capability {
        name: "mouse (SGR 1006)",
        supported: mouse,
        via: mouse_via,
        detail: "click/scroll in lists (general.mouse)".into(),
    });
    let (focus, focus_via) = mode(probe, |p| p.focus_events, None);
    v.push(Capability {
        name: "focus events",
        supported: focus,
        via: focus_via,
        detail: "pause heavy sampling while unfocused".into(),
    });
    v.push(Capability {
        name: "unicode",
        supported: Some(caps.unicode),
        via: "environment",
        detail: "UTF-8 locale (otherwise ASCII glyphs)".into(),
    });
    v
}

fn status_row(name: &str, st: &SourceStatus) -> SourceRow {
    let (status, reason) = match st {
        SourceStatus::Available => ("available", None),
        SourceStatus::Partial(r) => ("partial", Some(r.clone())),
        SourceStatus::Unavailable(r) => ("unavailable", Some(r.clone())),
    };
    SourceRow {
        name: name.to_string(),
        status,
        hint: reason.as_deref().and_then(|r| hint(name, r)),
        reason,
        provides: provides(name),
    }
}

fn uid() -> Option<u32> {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions.
        Some(unsafe { libc::getuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

pub fn run(ctx: &Ctx, json: bool, no_probe: bool) -> Result<i32> {
    let mut e = ctx.engine(true)?;
    let s = e.snapshot();
    let rule_warnings = e.rule_warnings(&ctx.config().privacy.marker_allowlist);
    let caps = oomtop_tui::caps::detect(ctx.config().appearance.color);
    let tried = !no_probe && termprobe::interactive();
    let probe = if tried { termprobe::probe() } else { None };
    let capabilities = capabilities(&caps, probe.as_ref(), tried);
    let sources = source_rows(&s, e.is_live());
    let state_path = oomtop_state::default_path();
    let state_bytes = std::fs::metadata(&state_path).map(|m| m.len()).ok();
    let paths = ctx.paths();
    let root = uid() == Some(0);
    let mut config_errors: Vec<String> = ctx.loaded.errors.iter().map(|e| e.to_string()).collect();
    let report = oomtop_config::validate::validate_all(&ctx.opts);
    for err in report.errors {
        let s = err.to_string();
        if !config_errors.contains(&s) {
            config_errors.push(s);
        }
    }

    if json {
        out(&json_pretty(&serde_json::json!({
            "version": oomtop_core::VERSION,
            "host": s.host,
            "replay": ctx.global.replay,
            "processes": s.processes.len(),
            "groups": s.groups.len(),
            "model_servers": s.model_servers.len(),
            "sandboxes": s.sandboxes.len(),
            "sources": sources,
            "terminal": {
                "term": caps.term,
                "colorterm": caps.colorterm,
                "term_program": caps.term_program,
                "tmux": caps.tmux,
                "ssh": caps.ssh,
                "dumb": caps.dumb,
                "no_color": caps.no_color,
                "clicolor_force": caps.clicolor_force,
                "probed": probe.is_some(),
                "probe": probe,
                "capabilities": capabilities,
            },
            "config": {
                "dir": paths.dir,
                "files": ctx.loaded.files,
                "errors": config_errors,
            },
            "rules": { "dir": paths.rules_d, "errors": e.rule_errors, "warnings": rule_warnings },
            "state": { "path": state_path, "bytes": state_bytes, "learning": !ctx.no_learn },
            "privileges": { "uid": uid(), "root": root },
        }))?);
        return Ok(0);
    }

    let h = &s.host;
    out(&format!("oomtop {} — doctor\n\n", oomtop_core::VERSION));
    if let Some(r) = &ctx.global.replay {
        out(&format!("Replaying {}\n", r.display()));
    }
    // No empty "· ·" field when neither a model nor a CPU brand is known (arm64 Linux has no "model name").
    let model = h
        .model
        .clone()
        .or_else(|| h.cpu_brand.clone())
        .filter(|m| !m.trim().is_empty())
        .map(|m| format!("{m} · "))
        .unwrap_or_default();
    out(&format!(
        "Host       {} · {} {} · {model}{} logical cores · {} RAM{}{}\n",
        h.hostname,
        h.os_version,
        h.arch,
        h.cores_logical,
        ctx.fmt(h.mem_total),
        if h.unified_memory { " (unified)" } else { "" },
        if h.fanless == Some(true) {
            " · fanless"
        } else {
            ""
        }
    ));
    out(&format!(
        "Sampled    {} processes → {} groups · {} model servers · {} sandboxes\n",
        s.processes.len(),
        s.groups.len(),
        s.model_servers.len(),
        s.sandboxes.len()
    ));
    out(&format!(
        "Privileges uid {}{}\n\nSources\n",
        uid().map(|u| u.to_string()).unwrap_or_else(|| "?".into()),
        if root {
            " (root: extra detail, actions unchanged)"
        } else {
            " (no root needed; other users' processes show partial data)"
        }
    ));
    for r in &sources {
        let mark = match r.status {
            "available" => "ok  ",
            "partial" => "part",
            "missing" => "??  ",
            _ => "--  ",
        };
        out(&format!("  {mark} {:<22} {}\n", r.name, r.provides));
        if let Some(reason) = &r.reason {
            out(&format!("       {:<22} {}: {reason}\n", "", r.status));
        }
        if let Some(hint) = r.hint {
            out(&format!("       {:<22} → {hint}\n", ""));
        }
    }
    out(&format!(
        "\nTerminal   TERM={} COLORTERM={}{}{}{}\n",
        if caps.term.is_empty() { "-" } else { &caps.term },
        if caps.colorterm.is_empty() {
            "-"
        } else {
            &caps.colorterm
        },
        if caps.term_program.is_empty() {
            String::new()
        } else {
            format!(" TERM_PROGRAM={}", caps.term_program)
        },
        if caps.tmux { " · tmux/screen" } else { "" },
        if caps.ssh { " · SSH" } else { "" }
    ));
    for c in &capabilities {
        let yn = match c.supported {
            Some(true) => "yes",
            Some(false) => "no ",
            None => "?  ",
        };
        out(&format!("  {yn} {:<20} {} ({})\n", c.name, c.detail, c.via));
    }
    if !tried {
        out("  (not a terminal or --no-probe: capabilities inferred from the environment)\n");
    } else if caps.tmux && probe.as_ref().is_some_and(|p| p.background.is_none()) {
        out("  hint: in tmux, `set -g allow-passthrough on` lets queries reach the outer terminal\n");
    }
    out(&format!("\nConfig     {}\n", tilde(&paths.dir)));
    if ctx.loaded.files.is_empty() {
        out("  defaults only (run `oomtop config init`)\n");
    }
    for f in &ctx.loaded.files {
        out(&format!("  read {}\n", tilde(f)));
    }
    for err in &config_errors {
        out(&format!("  error {err}\n"));
    }
    out(&format!("Rules      {}\n", tilde(&paths.rules_d)));
    for err in &e.rule_errors {
        out(&format!("  error {err}\n"));
    }
    for w in &rule_warnings {
        out(&format!("  warning {w}\n"));
    }
    out(&format!(
        "State      {}{}{}\n",
        tilde(&state_path),
        state_bytes
            .map(|b| format!(" ({})", ctx.fmt(b)))
            .unwrap_or_else(|| " (not created yet)".into()),
        if ctx.no_learn { " · learning off" } else { "" }
    ));
    Ok(0)
}

/// `$HOME/x` → `~/x` (shorter, and keeps the user name out of shared screenshots).
fn tilde(p: &std::path::Path) -> String {
    match std::env::var_os("HOME").map(std::path::PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() => match p.strip_prefix(&home) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        },
        _ => p.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_and_descriptions() {
        assert!(hint("linux.nvml", "libnvidia-ml.so.1 not loadable").is_some());
        assert!(hint("macos.procs", "Operation not permitted")
            .unwrap()
            .contains("root"));
        assert!(hint("macos.host", "all good").is_none());
        assert!(provides("adapter.ollama").contains("model-server"));
        assert_eq!(provides("mystery"), "");
    }

    #[test]
    fn probe_answers_win_over_environment() {
        let caps = oomtop_tui::caps::detect_from(&[], oomtop_config::model::ColorMode::Auto);
        let p = crate::termprobe::parse_replies(b"\x1b[?1u\x1b[?2026;2$y\x1b[?1;2c");
        let c = capabilities(&caps, Some(&p), true);
        let get = |n: &str| c.iter().find(|x| x.name == n).unwrap();
        assert_eq!(get("kitty keyboard").supported, Some(true));
        assert_eq!(get("kitty keyboard").via, "probe");
        assert_eq!(get("synchronized output").supported, Some(true));
        assert_eq!(
            get("mouse (SGR 1006)").supported,
            Some(false),
            "DA1 answered, 1006 not"
        );
        assert_eq!(get("OSC 11 background").supported, Some(false));
        let c = capabilities(&caps, None, false);
        let get = |n: &str| c.iter().find(|x| x.name == n).unwrap();
        assert_eq!(get("kitty keyboard").via, "environment");
        assert!(get("kitty keyboard").detail.starts_with("guessed from TERM="));
        assert_eq!(get("OSC 11 background").supported, None);
        assert_eq!(get("truecolor").via, "environment");
    }

    /// Apple Terminal is asked only OSC 11 + DA1: mode support stays the environment guess instead of
    /// "no" (it was reported as lacking mouse support only because it never answers DECRQM).
    #[test]
    fn unasked_modes_fall_back_to_the_environment() {
        let env = [("TERM_PROGRAM".to_string(), "Apple_Terminal".to_string())];
        let caps = oomtop_tui::caps::detect_from(&env, oomtop_config::model::ColorMode::Auto);
        let mut p = crate::termprobe::parse_replies(b"\x1b]11;rgb:0d0d/0c0c/0b0b\x1b\\\x1b[?1;2c");
        p.modes_queried = false;
        let c = capabilities(&caps, Some(&p), true);
        let get = |n: &str| c.iter().find(|x| x.name == n).unwrap();
        assert_eq!(get("mouse (SGR 1006)").via, "environment");
        assert_eq!(get("mouse (SGR 1006)").supported, Some(caps.mouse));
        assert_eq!(get("kitty keyboard").via, "environment");
        assert!(
            get("kitty keyboard").detail.contains("not asked"),
            "{}",
            get("kitty keyboard").detail
        );
        assert_eq!(get("OSC 11 background").supported, Some(true));
    }

    #[test]
    fn truecolor_probe_overrides_colorterm() {
        let env = [("COLORTERM".to_string(), "truecolor".to_string())];
        let caps = oomtop_tui::caps::detect_from(&env, oomtop_config::model::ColorMode::Auto);
        // COLORTERM claims truecolor, the terminal downsampled the test color: the answer wins.
        let p = crate::termprobe::parse_replies(b"\x1bP1$r38;5;16m\x1b\\\x1b[?1;2c");
        let c = capabilities(&caps, Some(&p), true);
        let tc = c.iter().find(|x| x.name == "truecolor").unwrap();
        assert_eq!((tc.supported, tc.via), (Some(false), "probe"));
        // No COLORTERM, but the terminal kept the color: yes, with a hint to export COLORTERM.
        let caps = oomtop_tui::caps::detect_from(&[], oomtop_config::model::ColorMode::Auto);
        let p = crate::termprobe::parse_replies(b"\x1bP1$r38:2:1:2:3m\x1b\\\x1b[?1;2c");
        let c = capabilities(&caps, Some(&p), true);
        let tc = c.iter().find(|x| x.name == "truecolor").unwrap();
        assert_eq!((tc.supported, tc.via), (Some(true), "probe"));
        assert!(tc.detail.contains("export COLORTERM=truecolor"), "{}", tc.detail);
    }

    #[test]
    fn expected_sources_that_were_not_reported_are_listed_as_missing() {
        let mut s = oomtop_core::Snapshot::default();
        s.host.os = oomtop_core::OsKind::Macos;
        s.source_status
            .insert("macos.host".into(), SourceStatus::Available);
        s.source_status.insert(
            "macos.ioreport".into(),
            SourceStatus::Unavailable("IOReport symbols not found".into()),
        );
        let rows = source_rows(&s, true);
        let get = |n: &str| rows.iter().find(|r| r.name == n).unwrap();
        assert_eq!(get("macos.host").status, "available");
        assert_eq!(get("macos.ioreport").status, "unavailable");
        assert!(get("macos.ioreport").hint.is_some());
        assert_eq!(get("macos.procs").status, "missing");
        assert_eq!(get("macos.thermal").status, "missing");
        assert_eq!(rows.len(), EXPECTED_MACOS.len(), "no duplicates");
        // A replayed fixture reports what its machine had, nothing more.
        assert_eq!(source_rows(&s, false).len(), 2);
        // Linux without an NVIDIA driver: NVML is covered by `linux.gpu`, not a `missing` row of its own.
        let mut l = oomtop_core::Snapshot::default();
        l.host.os = oomtop_core::OsKind::Linux;
        l.source_status.insert(
            "linux.gpu".into(),
            SourceStatus::Unavailable("no GPU in /sys/class/drm and no NVML device".into()),
        );
        let rows = source_rows(&l, true);
        assert!(
            rows.iter().all(|r| r.name != "linux.nvml"),
            "{:?}",
            rows.iter().map(|r| &r.name).collect::<Vec<_>>()
        );
        assert_eq!(rows.len(), EXPECTED_LINUX.len());
        // Every expected source has a description.
        for n in EXPECTED_MACOS.iter().chain(EXPECTED_LINUX) {
            assert!(!provides(n).is_empty(), "{n} has no description");
        }
    }
}

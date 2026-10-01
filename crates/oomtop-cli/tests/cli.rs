//! End-to-end tests of the `oomtop` binary (std::process, assert_cmd style). Each test gets private
//! `XDG_CONFIG_HOME` / `XDG_STATE_HOME` directories, so nothing touches the developer's real config or state.
//! Replay tests use the recorded fixtures and run on every CI OS; `live_*` tests sample the machine running
//! the tests (read-only: nothing here signals a process it did not spawn).

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

mod common;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn mac_fixture() -> String {
    root()
        .join("fixtures/macos/m5-air-agents.json")
        .to_string_lossy()
        .into_owned()
}

fn linux_fixture() -> String {
    root()
        .join("fixtures/linux/nvidia-oom-trend.json")
        .to_string_lossy()
        .into_owned()
}

struct Env {
    dir: tempfile::TempDir,
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    fn json(&self) -> Value {
        serde_json::from_str(&self.out)
            .unwrap_or_else(|e| panic!("not JSON ({e}):\n{}\n{}", self.out, self.err))
    }
}

impl Env {
    fn new() -> Self {
        Env {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn config_dir(&self) -> PathBuf {
        self.dir.path().join("config/oomtop")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_oomtop"));
        c.env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_STATE_HOME", self.dir.path().join("state"))
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .env_remove("VISUAL")
            .env_remove("EDITOR")
            .stdin(Stdio::null());
        for (k, _) in std::env::vars() {
            if k.starts_with("OOMTOP_") {
                c.env_remove(k);
            }
        }
        c
    }

    fn output(out: Output) -> Run {
        Run {
            code: out.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&out.stdout).into_owned(),
            err: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Runs against this machine.
    fn live(&self, args: &[&str]) -> Run {
        Self::output(self.cmd().arg("--offline").args(args).output().unwrap())
    }

    /// Runs against a fixture.
    fn replay_with(&self, fixture: &str, args: &[&str]) -> Run {
        Self::output(
            self.cmd()
                .args(["--replay", fixture, "--offline"])
                .args(args)
                .output()
                .unwrap(),
        )
    }

    fn replay(&self, args: &[&str]) -> Run {
        self.replay_with(&mac_fixture(), args)
    }

    fn plain(&self, args: &[&str]) -> Run {
        Self::output(self.cmd().args(args).output().unwrap())
    }
}

// ----- basics ------------------------------------------------------------------------------------------

#[test]
fn version_and_help() {
    let e = Env::new();
    let r = e.plain(&["--version"]);
    assert_eq!(r.code, 0);
    assert_eq!(r.out.trim(), format!("oomtop {}", env!("CARGO_PKG_VERSION")));
    let r = e.plain(&["--help"]);
    assert_eq!(r.code, 0);
    // No placeholder URLs in user-facing help until the repository exists.
    assert!(!r.out.contains("OWNER"), "placeholder in --help:\n{}", r.out);
    for sub in [
        "headroom", "why", "reclaim", "json", "ndjson", "serve", "mcp", "doctor", "config", "theme", "keys",
        "profile",
    ] {
        assert!(r.out.contains(sub), "help lists {sub}:\n{}", r.out);
    }
    for flag in [
        "--plain",
        "--ascii",
        "--no-learn",
        "--config",
        "--replay",
        "--set",
    ] {
        assert!(r.out.contains(flag), "help lists {flag}");
    }
    assert!(r.out.contains("Exit codes"));
}

#[test]
fn usage_errors_exit_2() {
    let e = Env::new();
    for args in [
        &["headroom", "--need", "banana"][..],
        &["headroom", "--need", "1G", "--model", "x.gguf"],
        &["headroom", "--ctx", "4096"],
        &["ndjson", "--interval", "never"],
        &["--color", "purple", "json"],
        &["config", "schema", "bogus"],
        &["config", "set", "appearance.theme", "x", "--layer", "nowhere"],
        &["keys", "list", "--preset", "nano"],
        &["--set", "novalue", "json"],
        &["frobnicate"],
    ] {
        let r = e.replay(args);
        assert_eq!(r.code, 2, "{args:?} → {}\n{}{}", r.code, r.out, r.err);
    }
}

// ----- headroom (SPEC §8, acceptance #6) --------------------------------------------------------------

#[test]
fn headroom_exit_codes_follow_the_answer() {
    let e = Env::new();
    // The fixture: ~5.3 GiB headroom, ~5.8 GiB reclaimable (idle Gradle + Kotlin daemons).
    let r = e.replay(&["headroom", "--need", "1G", "--json"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    let v = r.json();
    assert_eq!(v["fit"]["answer"], "yes");
    assert_eq!(v["valid_for_s"], 10);
    assert_eq!(
        v["expires_at_ms"].as_u64().unwrap(),
        v["as_of_ms"].as_u64().unwrap() + 10_000
    );

    let r = e.replay(&["headroom", "--need", "8G", "--json"]);
    assert_eq!(r.code, 3, "{}{}", r.out, r.err);
    let v = r.json();
    assert_eq!(v["fit"]["answer"], "yes_after_reclaim");
    let reclaim = v["fit"]["reclaim"].as_array().unwrap();
    assert!(!reclaim.is_empty());
    assert!(reclaim.iter().all(|c| c["kind"] == "build_daemon"), "{reclaim:?}");

    let r = e.replay(&["headroom", "--need", "13G"]);
    assert_eq!(r.code, 4, "{}{}", r.out, r.err);
    assert!(r.out.starts_with("No: "), "{}", r.out);

    let r = e.replay(&["headroom", "--need", "8G"]);
    assert_eq!(r.code, 3);
    assert!(r.out.contains("Run: oomtop reclaim --groups "), "{}", r.out);
}

#[test]
fn headroom_without_need_reports_the_budget() {
    let e = Env::new();
    let r = e.replay(&["headroom"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("headroom"), "{}", r.out);
    assert!(r.out.contains("safety margin"));
    let v = e.replay(&["headroom", "--json"]).json();
    assert!(v["headroom"]["safety_margin"].as_u64().unwrap() > 0);
    assert!(v["headroom"]["available_now"]["value"].as_u64().unwrap() > 0);
    assert_eq!(v["headroom"]["available_now"]["quality"], "exact");
}

/// A minimal GGUF v3 header: `general.architecture = "llama"` + llama dims (the core parser reads only this).
fn write_gguf(path: &Path) {
    fn s(buf: &mut Vec<u8>, v: &str) {
        buf.extend_from_slice(&(v.len() as u64).to_le_bytes());
        buf.extend_from_slice(v.as_bytes());
    }
    let kv_u32: &[(&str, u32)] = &[
        ("llama.block_count", 32),
        ("llama.embedding_length", 4096),
        ("llama.attention.head_count", 32),
        ("llama.attention.head_count_kv", 8),
        ("llama.context_length", 8192),
    ];
    let mut b = Vec::new();
    b.extend_from_slice(b"GGUF");
    b.extend_from_slice(&3u32.to_le_bytes());
    b.extend_from_slice(&0u64.to_le_bytes()); // tensors
    b.extend_from_slice(&((kv_u32.len() + 1) as u64).to_le_bytes());
    s(&mut b, "general.architecture");
    b.extend_from_slice(&8u32.to_le_bytes()); // string
    s(&mut b, "llama");
    for (k, v) in kv_u32 {
        s(&mut b, k);
        b.extend_from_slice(&4u32.to_le_bytes()); // uint32
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.resize(b.len() + 64 * 1024, 0); // "weights"
    std::fs::write(path, b).unwrap();
}

#[test]
fn headroom_for_a_model_file_reads_headers_and_scales_with_context() {
    let e = Env::new();
    let model = e.dir.path().join("tiny-llama.gguf");
    write_gguf(&model);
    let m = model.to_str().unwrap();
    let small = e.replay(&["headroom", "--model", m, "--ctx", "2048", "--json"]);
    assert_eq!(small.code, 0, "{}{}", small.out, small.err);
    let small = small.json();
    assert_eq!(small["estimate"]["fallback"], false, "{small}");
    let big = e
        .replay(&[
            "headroom",
            "--model",
            m,
            "--ctx",
            "8192",
            "--kv-type",
            "f16",
            "--json",
        ])
        .json();
    let (kv_s, kv_b) = (
        small["estimate"]["kv"].as_u64().unwrap(),
        big["estimate"]["kv"].as_u64().unwrap(),
    );
    assert!(
        kv_s > 0 && kv_b == 4 * kv_s,
        "KV grows linearly with ctx: {kv_s} vs {kv_b}"
    );
    let q8 = e
        .replay(&[
            "headroom",
            "--model",
            m,
            "--ctx",
            "8192",
            "--kv-type",
            "q8_0",
            "--json",
        ])
        .json();
    assert!(q8["estimate"]["kv"].as_u64().unwrap() < kv_b);
    let text = e.replay(&["headroom", "--model", m, "--ctx", "2048"]);
    assert!(text.out.starts_with("Estimate: "), "{}", text.out);
    assert!(text.out.contains("Yes"), "{}", text.out);
    let r = e.replay(&["headroom", "--model", "/nonexistent/model.gguf"]);
    assert_eq!(r.code, 1, "missing file is an error, not a usage error");
    let r = e.replay(&["headroom", "--model", m, "--kv-type", "q9"]);
    assert_eq!(r.code, 2);
}

// ----- json / ndjson (SPEC §5, §13) --------------------------------------------------------------------

#[test]
fn json_snapshot_is_attributed_and_redacted() {
    let e = Env::new();
    let r = e.replay(&["json"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let v = r.json();
    assert_eq!(v["schema_version"], 1);
    let groups = v["groups"].as_array().unwrap();
    let kinds: Vec<&str> = groups.iter().filter_map(|g| g["kind"].as_str()).collect();
    assert!(kinds.contains(&"build_daemon"), "Gradle/Kotlin daemons detected");
    assert!(kinds.contains(&"agent_session"), "agent sessions detected");
    assert!(
        groups
            .iter()
            .any(|g| g["kind"] == "app" && g["lower_bound"] == true),
        "Virtualization.framework VM is a lower bound (acceptance #3)"
    );
    let members: usize = groups
        .iter()
        .map(|g| g["members"].as_array().unwrap().len())
        .sum();
    assert_eq!(
        members,
        v["processes"].as_array().unwrap().len(),
        "each process in exactly one group"
    );
    // Every measured number carries a source and quality.
    assert!(v["memory"]["available"]["source"]
        .as_str()
        .is_some_and(|s| !s.is_empty()));
    // No environments, no secrets in argv.
    let re = regex_lite_secret();
    for p in v["processes"].as_array().unwrap() {
        assert!(p.get("env").is_none() && p.get("environ").is_none());
        for a in p["cmdline"].as_array().into_iter().flatten() {
            let a = a.as_str().unwrap_or_default();
            assert!(!re(a), "unredacted secret-looking arg {a:?}");
        }
    }
    let compact = e.replay(&["json", "--compact"]);
    assert_eq!(compact.out.lines().count(), 1);
}

/// `--token=abc`, `api_key=…`, `password=…` with a non-redacted value.
fn regex_lite_secret() -> impl Fn(&str) -> bool {
    |a: &str| {
        let l = a.to_ascii_lowercase();
        ["token=", "api_key=", "api-key=", "password=", "secret="]
            .iter()
            .any(|k| {
                l.find(k).is_some_and(|i| {
                    let v = &a[i + k.len()..];
                    !v.is_empty() && !v.contains("redacted") && !v.starts_with('<') && !v.starts_with('*')
                })
            })
    }
}

#[test]
fn ndjson_streams_one_snapshot_per_line() {
    let e = Env::new();
    let started = Instant::now();
    let r = e.replay(&["ndjson", "--interval", "250ms", "--count", "3"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let lines: Vec<&str> = r.out.lines().collect();
    assert_eq!(lines.len(), 3);
    for l in &lines {
        let v: Value = serde_json::from_str(l).unwrap();
        assert_eq!(v["schema_version"], 1);
    }
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "sleeps between snapshots"
    );
}

#[test]
fn ndjson_replay_shows_the_oom_forecast_building_up() {
    // Linux fixture: a job leaks ~1.5 GiB / 10 s until the kernel OOM-kills it (acceptance #7 shape).
    let e = Env::new();
    let r = e.replay_with(
        &linux_fixture(),
        &["ndjson", "--interval", "250ms", "--count", "16"],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let snaps: Vec<Value> = r.out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(snaps.len(), 16);
    assert!(snaps[0]["oom"]["forecast"].is_null(), "never on the first sample");
    assert!(
        snaps.iter().any(|s| !s["oom"]["forecast"].is_null()),
        "forecast appears once the trend is established"
    );
    assert_eq!(snaps[0]["host"]["os"], "linux");
}

#[test]
fn ndjson_replay_forecast_clears_right_after_the_oom_kill() {
    // The frame after the kernel's OOM kill has swap and memory back: no "swap full soon" ETA there.
    let e = Env::new();
    let r = e.replay_with(
        &linux_fixture(),
        &["ndjson", "--interval", "250ms", "--count", "17"],
    );
    assert_eq!(r.code, 0, "{}", r.err);
    let snaps: Vec<Value> = r.out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let killed = snaps
        .iter()
        .position(|s| s["oom"]["recent_kills"].as_array().is_some_and(|k| !k.is_empty()))
        .expect("the fixture records the OOM kill");
    assert!(
        snaps[..killed].iter().any(|s| !s["oom"]["forecast"].is_null()),
        "the leak was forecast before the kill"
    );
    for (i, s) in snaps.iter().enumerate().skip(killed) {
        assert!(
            s["oom"]["forecast"].is_null(),
            "frame {i} after the kill still forecasts: {}",
            s["oom"]["forecast"]
        );
    }
}

// ----- why / reclaim -----------------------------------------------------------------------------------

#[test]
fn why_reports_causes_or_nothing() {
    let e = Env::new();
    let r = e.replay(&["why", "--json"]);
    assert_eq!(r.code, 0);
    assert!(r.json().is_array());
    let r = e.replay(&["why"]);
    assert_eq!(r.code, 0);
    assert!(!r.out.trim().is_empty());
}

#[test]
fn why_shows_swap_growth_next_to_gpu_throttling() {
    // SPEC §17 #5 shape on the synthetic NVIDIA fixture: the first frame has no swap rates (they need two
    // samples), so `why` takes a second one and the swap storm is listed with the throttled GPU.
    let e = Env::new();
    let r = e.replay_with(&linux_fixture(), &["why"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("Swapping heavily"), "{}", r.out);
    assert!(r.out.contains("throttled"), "{}", r.out);
}

#[test]
fn reclaim_dry_run_lists_idle_daemons_with_estimates() {
    let e = Env::new();
    let r = e.replay(&["reclaim", "--dry-run"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("GradleDaemon"), "{}", r.out);
    assert!(r.out.contains("KotlinCompileDaemon"), "{}", r.out);
    assert!(r.out.contains("SIGTERM pid"));
    let v = e.replay(&["reclaim", "--dry-run", "--json"]).json();
    assert_eq!(v["dry_run"], true);
    let c = v["candidates"].as_array().unwrap();
    assert!(c.len() >= 2);
    let total: u64 = c.iter().map(|x| x["gain"].as_u64().unwrap()).sum();
    assert_eq!(v["total_gain"].as_u64().unwrap(), total);
    // Largest first.
    let gains: Vec<u64> = c.iter().map(|x| x["gain"].as_u64().unwrap()).collect();
    assert!(gains.windows(2).all(|w| w[0] >= w[1]), "{gains:?}");
}

#[test]
fn reclaim_never_acts_without_confirmation_or_on_a_replay() {
    let e = Env::new();
    let r = e.replay(&["reclaim"]);
    assert_eq!(r.code, 1, "no TTY, no --yes → refuse: {}{}", r.out, r.err);
    assert!(
        r.err.contains("refusing to stop processes without confirmation"),
        "{}",
        r.err
    );
    let r = e.replay(&["reclaim", "--yes"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("nothing executed"), "{}", r.out);
    let v = e
        .replay(&["reclaim", "--dry-run", "--json", "--groups", "nope,system:kernel"])
        .json();
    assert!(v["candidates"].as_array().unwrap().is_empty());
    let skipped = v["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 2);
    assert!(skipped[0]["reason"].as_str().unwrap().contains("no such group"));
}

// ----- doctor / plain ----------------------------------------------------------------------------------

#[test]
fn doctor_reports_sources_and_terminal_capabilities() {
    let e = Env::new();
    let r = e.replay(&["doctor"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("macos.procs"));
    assert!(r.out.contains("Terminal"));
    let v = e.replay(&["doctor", "--json"]).json();
    let caps: Vec<&str> = v["terminal"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for want in [
        "truecolor",
        "kitty keyboard",
        "synchronized output",
        "OSC 8 hyperlinks",
        "OSC 11 background",
        "mouse (SGR 1006)",
    ] {
        assert!(caps.contains(&want), "{want} in {caps:?}");
    }
    assert_eq!(
        v["terminal"]["probed"], false,
        "stdout is a pipe: never query the terminal"
    );
    let sources = v["sources"].as_array().unwrap();
    assert!(sources
        .iter()
        .all(|s| s["status"].is_string() && s["name"].is_string()));
    assert!(sources
        .iter()
        .any(|s| s["status"] == "partial" && s["reason"].is_string()));
}

#[test]
fn plain_mode_prints_a_linear_summary() {
    let e = Env::new();
    let r = e.replay(&["--plain"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(!r.out.contains('\x1b'), "no escape codes in --plain");
    assert!(r.out.lines().count() > 3, "{}", r.out);
}

// ----- config (UX §12.1, §12.9) ------------------------------------------------------------------------

#[test]
fn config_lifecycle_init_set_print_validate_unset() {
    let e = Env::new();
    let r = e.plain(&["config", "init", "--print"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("theme"), "{}", r.out);
    let r = e.plain(&["config", "init"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(e.config_dir().join("config.toml").is_file());
    assert!(e.config_dir().join("schemas/oomtop-config.schema.json").is_file());
    assert_eq!(e.plain(&["config", "init"]).code, 1, "no silent overwrite");
    assert_eq!(e.plain(&["config", "init", "--force"]).code, 0);

    // A comment the user wrote survives `config set`.
    let main = e.config_dir().join("config.toml");
    let mut text = std::fs::read_to_string(&main).unwrap();
    text.push_str("\n# my note: keep me\n");
    std::fs::write(&main, &text).unwrap();
    assert_eq!(e.plain(&["config", "set", "appearance.theme", "ember"]).code, 0);
    assert!(std::fs::read_to_string(&main)
        .unwrap()
        .contains("# my note: keep me"));

    assert_eq!(
        e.plain(&["config", "set", "format.decimals", "2", "--layer", "host"])
            .code,
        0
    );
    assert_eq!(
        e.plain(&[
            "config",
            "set",
            "appearance.theme",
            "mint",
            "--layer",
            "dropin:work"
        ])
        .code,
        0
    );
    let r = e.plain(&["config", "print", "--origin", "appearance.theme"]);
    assert_eq!(
        r.out.trim(),
        "appearance.theme = \"mint\"    # work.toml:2",
        "{}",
        r.out
    );
    let r = e.plain(&["config", "print", "--origin", "format.decimals"]);
    assert!(r.out.contains("# host-"), "{}", r.out);

    // Precedence: env beats files, flags beat env.
    let r = Env::output(
        e.cmd()
            .env("OOMTOP_APPEARANCE__THEME", "sand")
            .args(["config", "print", "--origin", "appearance.theme"])
            .output()
            .unwrap(),
    );
    assert!(
        r.out.contains("\"sand\"") && r.out.contains("env OOMTOP_APPEARANCE__THEME"),
        "{}",
        r.out
    );
    let r = Env::output(
        e.cmd()
            .env("OOMTOP_APPEARANCE__THEME", "sand")
            .args([
                "--theme",
                "coral",
                "config",
                "print",
                "--origin",
                "appearance.theme",
            ])
            .output()
            .unwrap(),
    );
    assert!(
        r.out.contains("\"coral\"") && r.out.contains("# flag"),
        "{}",
        r.out
    );

    // Validation errors carry file:line and fail; a bad drop-in doesn't break the rest.
    let r = e.plain(&["config", "set", "general.refresh_ms", "\"fast\""]);
    assert_eq!(r.code, 1);
    assert!(r.err.contains("config.toml:"), "{}", r.err);
    assert_eq!(e.plain(&["config", "validate"]).code, 0);
    std::fs::write(
        e.config_dir().join("config.d/zz-bad.toml"),
        "[general]\nrefresh_ms = \"x\"\n",
    )
    .unwrap();
    let r = e.plain(&["config", "validate"]);
    assert_eq!(r.code, 1);
    assert!(r.out.contains("zz-bad.toml:2"), "{}", r.out);
    let r = e.plain(&["config", "print", "--effective", "appearance.theme"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("mint"), "last good values kept: {}", r.out);
    std::fs::remove_file(e.config_dir().join("config.d/zz-bad.toml")).unwrap();

    assert_eq!(
        e.plain(&["config", "unset", "appearance.theme", "--layer", "dropin:work"])
            .code,
        0
    );
    let r = e.plain(&["config", "print", "--origin", "appearance.theme"]);
    assert!(
        r.out.contains("\"ember\"") && r.out.contains("config.toml:"),
        "{}",
        r.out
    );
    let r = e.plain(&["config", "print", "--effective", "no.such.key"]);
    assert_eq!(r.code, 2);
}

#[test]
fn config_flag_moves_the_config_directory() {
    let e = Env::new();
    let other = e.dir.path().join("elsewhere/my.toml");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, "[appearance]\ntheme = \"mono\"\n").unwrap();
    let o = other.to_str().unwrap();
    let r = e.plain(&["--config", o, "config", "print", "--origin", "appearance.theme"]);
    assert!(
        r.out.contains("\"mono\"") && r.out.contains("my.toml:2"),
        "{}",
        r.out
    );
    assert_eq!(
        e.plain(&["--config", o, "config", "set", "appearance.theme", "sand"])
            .code,
        0
    );
    assert!(std::fs::read_to_string(&other).unwrap().contains("sand"));
    assert!(
        !e.config_dir().join("config.toml").exists(),
        "default file untouched"
    );
}

#[test]
fn config_schema_prints_and_writes() {
    let e = Env::new();
    for kind in ["config", "theme", "keymap", "layout", "rules"] {
        let r = e.plain(&["config", "schema", kind]);
        assert_eq!(r.code, 0, "{kind}");
        let v: Value = serde_json::from_str(&r.out).unwrap();
        assert!(v.is_object());
    }
    let d = e.dir.path().join("schemas");
    let r = e.plain(&["config", "schema", "--write", d.to_str().unwrap()]);
    assert_eq!(r.code, 0);
    assert!(d.join(".taplo.toml").is_file());
    assert!(d.join("oomtop-rules.schema.json").is_file());
}

// ----- themes (UX §12.3–12.4) --------------------------------------------------------------------------

const GRUVBOX: &str = "scheme: \"Gruvbox dark, medium\"\nauthor: \"morhetz\"\nbase00: \"282828\"\nbase01: \"3c3836\"\nbase02: \"504945\"\nbase03: \"665c54\"\nbase04: \"bdae93\"\nbase05: \"d5c4a1\"\nbase06: \"ebdbb2\"\nbase07: \"fbf1c7\"\nbase08: \"fb4934\"\nbase09: \"fe8019\"\nbase0A: \"fabd2f\"\nbase0B: \"b8bb26\"\nbase0C: \"8ec07c\"\nbase0D: \"83a598\"\nbase0E: \"d3869b\"\nbase0F: \"d65d0e\"\n";

#[test]
fn builtin_themes_list_and_check() {
    let e = Env::new();
    let r = e.plain(&["theme", "list"]);
    assert_eq!(r.code, 0);
    for t in [
        "terminal",
        "none",
        "mono",
        "ember",
        "mint",
        "sand",
        "coral",
        "high-contrast",
        "colorblind",
    ] {
        assert!(r.out.contains(t), "{t}");
        let c = e.plain(&["theme", "check", t]);
        assert_eq!(c.code, 0, "{t}: {}{}", c.out, c.err);
    }
    assert!(r
        .out
        .lines()
        .any(|l| l.starts_with("terminal") && l.contains("current")));
    assert_eq!(e.plain(&["theme", "check", "nope"]).code, 1);
}

#[test]
fn theme_import_export_round_trip() {
    let e = Env::new();
    let scheme = e.dir.path().join("gruvbox.yaml");
    std::fs::write(&scheme, GRUVBOX).unwrap();
    let s = scheme.to_str().unwrap();
    let r = e.plain(&["theme", "import", s, "--name", "gruv"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert!(e.config_dir().join("themes/gruv.toml").is_file());
    assert_eq!(
        e.plain(&["theme", "import", s, "--name", "gruv"]).code,
        1,
        "no silent overwrite"
    );
    assert_eq!(
        e.plain(&["theme", "import", s, "--name", "gruv", "--force"]).code,
        0
    );
    assert!(e.plain(&["theme", "list"]).out.contains("gruv"));
    let c = e.plain(&["theme", "check", "gruv"]);
    assert_eq!(c.code, 0, "{}", c.out);

    // export → file → it is a valid, loadable user theme
    let out = e.config_dir().join("themes/gruv-copy.toml");
    let r = e.plain(&["theme", "export", "gruv", "-o", out.to_str().unwrap()]);
    assert_eq!(r.code, 0, "{}", r.err);
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("[ui]") && text.contains("[mem]"), "{text}");
    let c = e.plain(&["theme", "check", "gruv-copy"]);
    assert_eq!(c.code, 0, "{}", c.out);
    let p = e.plain(&["theme", "import", s, "--print"]);
    assert!(p.out.contains("name = "));
}

#[test]
fn theme_preview_honors_depth_and_no_color() {
    let e = Env::new();
    let r = e.plain(&["--color", "16", "theme", "preview", "terminal"]);
    assert_eq!(r.code, 0);
    assert!(r.out.contains("\x1b["), "colored preview");
    assert!(
        !r.out.contains("38;2;") && !r.out.contains("48;"),
        "16 colors only, never a background"
    );
    for tok in [
        "ui.accent",
        "state.crit",
        "mem.gpu",
        "kind.agent",
        "text.headline",
    ] {
        assert!(r.out.contains(tok), "{tok}");
    }
    let r = Env::output(
        e.cmd()
            .env("NO_COLOR", "1")
            .args(["theme", "preview", "ember"])
            .output()
            .unwrap(),
    );
    assert!(
        !has_color_sgr(&r.out),
        "NO_COLOR → no colors (attributes are fine)"
    );
    let r = e.plain(&[
        "--color",
        "truecolor",
        "theme",
        "preview",
        "ember",
        "--variant",
        "light",
    ]);
    assert!(r.out.contains("38;2;"), "truecolor theme at truecolor depth");
}

/// True if any SGR sequence sets a foreground/background color.
fn has_color_sgr(s: &str) -> bool {
    s.split("\x1b[")
        .skip(1)
        .filter_map(|c| c.split_once('m'))
        .flat_map(|(p, _)| {
            p.split(';')
                .filter_map(|x| x.parse::<u32>().ok())
                .collect::<Vec<_>>()
        })
        .any(|p| (30..=49).contains(&p) || (90..=107).contains(&p))
}

// ----- keys --------------------------------------------------------------------------------------------

#[test]
fn keymap_presets_list_without_conflicts() {
    let e = Env::new();
    for p in ["default", "vim", "emacs", "htop"] {
        let r = e.plain(&["keys", "list", "--conflicts", "--preset", p]);
        assert_eq!(r.code, 0, "{p}: {}{}", r.out, r.err);
        assert!(
            r.out.starts_with(&format!("no conflicts (preset {p}, ")) && r.out.contains("bindings checked"),
            "{p}: {}",
            r.out
        );
        assert_eq!(r.out.lines().count(), 1, "{p}: {}", r.out);
        let r = e.plain(&["keys", "list", "--preset", p]);
        assert!(r.out.contains("[global]"), "{p}");
    }
}

// ----- profile (UX §6) ---------------------------------------------------------------------------------

#[test]
fn profile_is_built_locally_and_resettable() {
    let e = Env::new();
    let r = e.plain(&["profile", "show"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.contains("not profiled yet"));

    // --no-learn: a live run doesn't build the profile.
    assert_eq!(e.live(&["--no-learn", "json", "--compact"]).code, 0);
    assert!(e.plain(&["profile", "show"]).out.contains("not profiled yet"));
    let r = Env::output(
        e.cmd()
            .env("OOMTOP_NO_LEARN", "1")
            .args(["--offline", "json", "--compact"])
            .output()
            .unwrap(),
    );
    assert_eq!(r.code, 0);
    assert!(e.plain(&["profile", "show"]).out.contains("not profiled yet"));

    // A normal live run builds it.
    assert_eq!(e.live(&["json", "--compact"]).code, 0);
    let v = e.plain(&["profile", "show", "--json"]).json();
    let mp = &v["machine_profile"];
    assert!(mp["hardware"]["ram"].as_u64().unwrap() > 0, "{v}");
    assert!(mp["roles"].is_object());
    let exp = e.plain(&["profile", "export"]).json();
    assert_eq!(exp["schema_version"], 1);
    assert!(
        !exp.to_string().contains("cmdline"),
        "no command lines in the export"
    );
    let st = e.plain(&["profile", "stats", "--json"]).json();
    assert_eq!(st["impressions"], 0);

    let r = e.plain(&["profile", "reset"]);
    assert_eq!(r.code, 1, "no TTY → refuse without --yes");
    assert_eq!(e.plain(&["profile", "reset", "--yes"]).code, 0);
    assert!(e.plain(&["profile", "show"]).out.contains("not profiled yet"));
}

#[test]
fn replay_never_writes_state() {
    let e = Env::new();
    assert_eq!(e.replay(&["json", "--compact"]).code, 0);
    assert!(!e.dir.path().join("state/oomtop/state.db").exists());
}

// ----- serve / mcp -------------------------------------------------------------------------------------

fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(s, "GET {path} HTTP/1.0\r\nHost: {addr}\r\n\r\n").unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let buf = String::from_utf8_lossy(&raw).into_owned();
    let code = buf
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = buf
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (code, body)
}

#[test]
fn serve_answers_http_on_loopback() {
    let e = Env::new();
    let child = e
        .cmd()
        .args([
            "--replay",
            &mac_fixture(),
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--max-requests",
            "4",
        ])
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = common::Reaped::new(child);
    let mut err = BufReader::new(child.child().stderr.take().unwrap());
    let mut line = String::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let addr = loop {
        line.clear();
        if err.read_line(&mut line).unwrap() == 0 || Instant::now() > deadline {
            panic!("serve did not start: {line}");
        }
        if let Some(rest) = line.split("http://").nth(1) {
            break rest.split('/').next().unwrap().to_string();
        }
    };
    assert!(addr.starts_with("127.0.0.1:"), "{addr}");
    let (code, body) = http_get(&addr, "/healthz");
    assert_eq!(code, 200);
    assert!(body.contains("ok"));
    let (code, body) = http_get(&addr, "/metrics");
    assert_eq!(code, 200);
    assert!(body.contains("oomtop_memory_total_bytes"), "{body}");
    let (code, body) = http_get(&addr, "/snapshot");
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["schema_version"], 1);
    let (code, _) = http_get(&addr, "/headroom");
    assert_eq!(code, 200);
    let status = child.wait().unwrap();
    assert!(status.success(), "exits after --max-requests");
}

fn mcp_session(e: &Env, extra: &[&str], calls: &[Value]) -> Vec<Value> {
    let mut c = e.cmd();
    c.args(["--replay", &mac_fixture(), "--offline", "mcp"])
        .args(extra);
    mcp_session_cmd(c, calls)
}

fn mcp_session_cmd(mut c: Command, calls: &[Value]) -> Vec<Value> {
    let mut child = common::Reaped::new(
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    {
        let mut stdin = child.child().stdin.take().unwrap();
        let init = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}});
        writeln!(stdin, "{init}").unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        for c in calls {
            writeln!(stdin, "{c}").unwrap();
        }
    } // EOF ends the session
    let out = child.into_inner().wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// `oomtop headroom --need 13G` and MCP `can_fit {size: "13G"}` say the same amounts in the same units
/// (the configured `format.memory_units`), and neither mixes "GiB" with a short "4.5G".
#[test]
fn cli_and_mcp_can_fit_agree_on_amounts_and_units() {
    let e = Env::new();
    for (set, unit) in [(None, "GiB"), (Some("format.memory_units=si"), "GB")] {
        let mut args = vec![];
        if let Some(kv) = set {
            args.extend(["--set", kv]);
        }
        let mut cli_args = args.clone();
        cli_args.extend(["headroom", "--need", "13G"]);
        let r = e.replay(&cli_args);
        assert_eq!(r.code, 4, "{}{}", r.out, r.err);
        let first = r.out.lines().next().unwrap();
        let want_need = if unit == "GiB" { "13.0 GiB" } else { "14.0 GB" };
        let prefix = format!("No: {want_need} doesn't fit — ");
        assert!(first.starts_with(&prefix), "{first}");
        let reason = first[prefix.len()..].trim_end_matches('.');
        assert!(reason.contains(&format!(" {unit}")), "{first}");
        assert!(
            !first
                .as_bytes()
                .windows(2)
                .any(|w| w[0].is_ascii_digit() && matches!(w[1], b'K' | b'M' | b'G' | b'T')),
            "short unit mixed in: {first}"
        );

        let mut mcp_args: Vec<&str> = args.clone();
        mcp_args.push("mcp");
        let mut c = e.cmd();
        c.args(["--replay", &mac_fixture(), "--offline"]).args(&mcp_args);
        let resp = mcp_session_cmd(
            c,
            &[
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"can_fit","arguments":{"size":"13G"}}}),
            ],
        );
        let sc = &resp.iter().find(|r| r["id"] == 1).unwrap()["result"]["structuredContent"];
        let summary = sc["summary"].as_str().unwrap();
        // A cold MCP call samples twice, so on a replay it answers from the next frame (2 s later): the
        // numbers may differ by that frame's change; the sentence, the units and the precision may not.
        let shape = |t: &str| -> String {
            t.chars()
                .map(|ch| if ch.is_ascii_digit() { '#' } else { ch })
                .collect()
        };
        let mcp_reason = sc["reason"].as_str().unwrap();
        assert_eq!(
            shape(mcp_reason),
            shape(reason),
            "MCP {summary:?} vs CLI {first:?}"
        );
        assert!(
            summary.starts_with(&format!("No: {want_need} does not fit — {mcp_reason}")),
            "MCP {summary:?} vs CLI {first:?}"
        );
    }
}

#[test]
fn mcp_answers_can_fit_over_stdio() {
    let e = Env::new();
    let resp = mcp_session(
        &e,
        &[],
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"can_fit","arguments":{"bytes": 8u64 << 30}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_headroom","arguments":{}}}),
        ],
    );
    let by_id = |id: u64| {
        resp.iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("id {id}: {resp:?}"))
    };
    assert_eq!(by_id(0)["result"]["protocolVersion"], "2025-06-18");
    let tools: Vec<&str> = by_id(1)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"can_fit") && tools.contains(&"get_headroom"));
    assert!(!tools.contains(&"reclaim"), "read-only by default");
    assert_eq!(
        by_id(2)["result"]["structuredContent"]["fit"]["answer"],
        "yes_after_reclaim"
    );
    assert!(by_id(3)["result"]["structuredContent"].is_object());

    let resp = mcp_session(
        &e,
        &["--allow-actions"],
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
    );
    let tools = resp.iter().find(|r| r["id"] == 1).unwrap()["result"]["tools"].to_string();
    assert!(
        tools.contains("\"reclaim\""),
        "--allow-actions registers reclaim: {tools}"
    );
}

// ----- live (this machine; read-only) ------------------------------------------------------------------

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn live_snapshot_and_headroom_on_this_machine() {
    let e = Env::new();
    let r = e.live(&["json", "--compact"]);
    assert_eq!(r.code, 0, "{}", r.err);
    let v = r.json();
    assert!(!v["processes"].as_array().unwrap().is_empty());
    let me = v["self_pid"].as_u64().unwrap();
    assert!(v["processes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["id"]["pid"].as_u64() == Some(me)));
    // A measurable answer: yes (0), yes after reclaim (3), or no (4) on a machine already below its safety
    // margin. Never "couldn't measure" (1) or a usage error (2).
    let r = e.live(&["headroom", "--need", "1K"]);
    assert!(
        matches!(r.code, 0 | 3 | 4),
        "1 KiB gets an answer: {}{}",
        r.out,
        r.err
    );
    let first = r.out.lines().next().unwrap_or_default();
    assert!(first.starts_with("Yes") || first.starts_with("No"), "{}", r.out);
    let r = e.live(&["doctor", "--no-probe"]);
    assert_eq!(r.code, 0, "{}", r.err);
    // Every source this OS is expected to have is listed: reported (with a status) or explicitly missing.
    let v = e.live(&["doctor", "--json", "--no-probe"]).json();
    let names: Vec<String> = v["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    let expected: &[&str] = if cfg!(target_os = "macos") {
        &[
            "macos.host",
            "macos.procs",
            "macos.thermal",
            "macos.power",
            "macos.ioreport",
        ]
    } else {
        &[
            "linux.host",
            "linux.procs",
            "linux.psi",
            "linux.gpu",
            "linux.thermal",
        ]
    };
    for want in expected {
        assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
    }
    assert!(v["sources"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["status"] == "available" || s["reason"].is_string()));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn ndjson_discard_samples_without_output() {
    // The perf job's steady-state probe: the TUI's pipeline, no JSON.
    let e = Env::new();
    let started = Instant::now();
    let r = e.live(&["ndjson", "--discard", "--count", "2", "--interval", "250ms"]);
    assert_eq!(r.code, 0, "{}", r.err);
    assert!(r.out.is_empty(), "{}", r.out);
    assert!(
        started.elapsed() >= Duration::from_millis(250),
        "honours the interval"
    );
}

/// SPEC §14/§21: on macOS the binary runs on jemalloc with immediate purging (the TUI's RSS budget depends
/// on it). `confirm_conf` makes jemalloc print the options it parsed, including the compiled-in string.
#[cfg(target_os = "macos")]
#[test]
fn macos_allocator_purges_freed_pages_immediately() {
    let out = Command::new(env!("CARGO_BIN_EXE_oomtop"))
        .arg("--version")
        .env("_RJEM_MALLOC_CONF", "confirm_conf:true")
        .output()
        .unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    for opt in [
        "narenas:1",
        "tcache:false",
        "dirty_decay_ms:0",
        "muzzy_decay_ms:0",
    ] {
        assert!(
            err.contains(&format!("Set conf value: {opt}")),
            "{opt} not applied:\n{err}"
        );
    }
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("oomtop "));
}

/// A panicking test still kills and reaps its child (the guard used by the serve / MCP / pty tests).
#[test]
fn reaped_children_die_with_a_panicking_test() {
    let child = Command::new("sleep").arg("600").spawn().unwrap();
    let pid = child.id() as libc::pid_t;
    let r = std::panic::catch_unwind(move || {
        let _g = common::Reaped::new(child);
        panic!("assertion failed mid-test");
    });
    assert!(r.is_err());
    // SAFETY: signal 0 only checks existence; the pid was reaped, so ESRCH is expected.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    assert!(!alive, "child {pid} survived the panic");
}

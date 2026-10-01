# oomtop — crate contracts (M0)

This is what parallel builders code against. **Signatures listed here and every public type in
`crates/oomtop-core/src/model.rs` are frozen: additive changes only.** New struct fields must be `Option<_>` or
carry a serde default (every model struct is `#[serde(default)]` + `Default`). A breaking change needs a
SPEC/UX update in the same change and a note in the builder's report.

Dependency direction (SPEC §6): `core` ← `collect`, `detect`, `adapters`, `config`, `state` ← frontends
(`tui`, `mcp`, `serve`) ← `cli`. Frontends never touch OS APIs; they receive a `SnapshotProvider` (and an
optional `Actuator`) from the CLI. Synchronous design everywhere: **no tokio / async**.

Global invariants
- `oomtop-core` performs **no I/O** (no fs, net, env, clock, process). Everything is a pure function.
- Every measured number is `Measured<T> { value: Option<T>, source: String, quality }`; unavailable values are
  `value: None` + `Quality::Unavailable(reason)` and are never rendered as zero.
- Bytes are `u64`; times are ms since the Unix epoch (`*_ms`) or seconds (`*_s`).
- Process identity is `ProcId { pid, start_time }` (`start_time` = ms since epoch; macOS
  `pbi_start_tvsec*1000 + usec/1000`, Linux `btime*1000 + starttime*1000/CLK_TCK`). `collect::signal`
  re-verifies it before any signal.
- Live snapshots hold raw argv (rules + adapters need it). **Every export path** (json, ndjson, serve, MCP)
  calls `oomtop_core::redact::redact_snapshot`. Environments are never stored: Sources convert allowlisted keys
  into `Markers` (keys + hashed session id) and drop everything else without copying.
- Collectors never panic on missing/changed OS data; they return `SourceError::Unavailable(reason)` or a
  partial sample, honoring the per-source budget (≤ 50 ms; the process listing gets 4×, see SPEC §21 §14).
- Default theme `terminal` never paints a background; no blue/purple in built-in themes (checked by
  `theme::check`); meaning is never carried by color alone.
- Actions: explicit confirmation always; group roots only; SIGKILL only with `ActionPlan::confirmed_kill`;
  suspend never counts as reclaim; the calling agent's session is never a target.

---

## oomtop-core (pure) — owner: core/logic builders (split by module)

`lib.rs` declares every module; builders edit module files only. `pub use model::*;`
`pub const VERSION: &str`.

### model.rs — SPEC §5 — FROZEN
`SCHEMA_VERSION = 1`, `type Bytes = u64`,
`Quality { Exact, Estimate, Unavailable(String) }` (serde: `"exact"`, `"estimate"`, `{"unavailable": ".."}`),
`Measured<T> { value, source, quality }`, `SourceStatus { Available, Partial(String), Unavailable(String) }`,
`OsKind`, `HostInfo`, `PressureLevel { Normal, Warn, Critical }`, `Psi`, `HostMemory` (total, available, free,
cached, wired, compressed, compressed_logical, app, swap_used, swap_total, swap_in_per_min, swap_out_per_min,
pressure, memorystatus_level, psi, own_cgroup_limit, swap_limit (macOS swap ceiling, additive)), `HostCpu`, `OomKiller { Kernel, SystemdOomd, Earlyoom,
Jetsam }`, `ThresholdMetric`, `OomThreshold`, `ForecastTarget`, `Forecast { target, killer, eta_s, confidence,
rate_per_min, window_s }`, `Victim`, `OomKill`, `Oom { killer, killers, thresholds, forecast, likely_victim,
recent_kills }`, `GpuVendor`, `Accelerator` (incl. `gpu_budget`), `ThermalPressure { Nominal, Moderate, Heavy,
Trapping, Sleeping }`, `ClusterFreq`, `TempSensor`, `Thermal` (pressure, throttle_factor, low_power_mode,
on_battery, battery_pct, adapter_watts, package_power_w, clusters, temps, trip_point_hit), `ProcId`,
`MemBreakdown { resident, footprint_or_pss, gpu, compressed, swapped, non_resident_est }`, `DiskIo`, `ProcState`,
`Markers { session_id, agent, keys }`, `Process`, `GroupKind { AgentSession, App, ModelServer, Sandbox,
BuildDaemon, System, Other }` (+ `alias()`, `as_str()`, `parse()`; serde accepts short aliases),
`Confidence`, `AttributionSignal`, `Member`, `GroupTotals`, `Group`, `ModelServerKind`, `Device`,
`LoadedModel`, `JobProgress`, `ModelServer`, `SandboxKind`, `SandboxLimits`, `Sandbox`,
`Snapshot { schema_version, taken_at_ms, host, memory, cpu, oom, accelerators, thermal, processes, groups,
model_servers, sandboxes, source_status: BTreeMap<String, SourceStatus>, self_pid }` with
`process(ProcId)`, `process_by_pid(u32)`, `group(&str)`, `group_of(ProcId)`.

### measured.rs
`Quality::{rank, is_available, worst, short_label}`; `Measured::{exact, estimate, unavailable, get,
is_available, map, into_estimate, unavailable_reason, value_or}`;
`sum_bytes(iter, source) -> Measured<u64>`, `sum_f64(iter, source) -> Measured<f64>` (partial sums → `Estimate`).

### units.rs — UX §5.2, §12.5
`parse_bytes(&str) -> Result<u64, UnitError>` (K/M/G/T and KiB.. = 1024ⁿ; KB/MB/GB/TB = 1000ⁿ),
`parse_duration_s(&str) -> Result<f64, UnitError>` ("500ms", "30m", "1h30m"), `format_bytes(u64, UnitSystem,
decimals)`, `format_bytes_short(u64)` ("9.9G"), `format_bytes_signed`, `format_duration(u64)` ("3h40m"),
`format_pct`. `KIB/MIB/GIB/TIB`.

### headroom.rs — SPEC §8.1
`HeadroomConfig { min_margin, margin_pct, pressure_boost_pct, margin_override }` (default 1.5 GiB / 8 % / +50 %),
`Headroom { available_now, safety_margin, headroom: Option<i64>, pressure, swap_growing, reclaimable, gpu:
Vec<GpuHeadroom>, as_of_ms }`, `compute(&Snapshot, &HeadroomConfig) -> Headroom`,
`safety_margin(total, pressured, &cfg) -> u64`, `swap_growing(&Snapshot) -> bool`,
`is_reclaim_candidate(&Group) -> bool` (build daemons, orphans, idle model servers; never protected/self),
`process_reclaim_gain(&Process, compressor_ratio) -> Measured<u64>`, `group_reclaim_gain(&Group, &Snapshot)`,
`compressor_ratio(&Snapshot) -> Option<f64>`.
Invariant: reclaim gain is RAM freed (always `Estimate`), swap freed is separate (`Group::swap_gain`).

### can_fit.rs — SPEC §8.2
`EXIT_YES=0, EXIT_ERROR=1, EXIT_USAGE=2, EXIT_YES_AFTER_RECLAIM=3, EXIT_NO=4`, `VALID_FOR_S=10`,
`Need { bytes, gpu_bytes, label }`, `ReclaimCandidate`, `Fit { Yes, YesAfterReclaim{reclaim, gain},
No{shortfall} }` (serde tag `answer`), `CanFitAnswer`, `reclaim_candidates(&Snapshot) -> Vec<ReclaimCandidate>`
(largest gain first), `can_fit(&Need, &Headroom, &[ReclaimCandidate]) -> CanFitAnswer` (greedy),
`exit_code(&Fit) -> i32`. Advisory only. Additive (round 2): `CanFitAnswer.{host_shortfall, reclaimable}` (serde
default 0) and `reason_with(&CanFitAnswer, &dyn Fn(u64) -> String) -> String`, which renders the one-line reason
in a frontend's units; the stored `reason` uses IEC with one decimal.

### model_estimate.rs — SPEC §8.2
`parse_gguf_header(&[u8]) -> Result<GgufMeta, GgufError>` (v2/v3; arrays skipped; truncated buffers →
`truncated = true`), `GgufMeta::{architecture, n_layers, n_heads, n_kv_heads, embedding_length,
context_length, head_dim}`, `KvParams { ctx, kv_type_bytes, n_parallel }`, `ModelEstimate { weights, kv,
overhead, need, fallback, ctx, note }`, `estimate_llm(file_size, Option<&GgufMeta>, &KvParams)`,
`estimate_unknown(file_size)` (1.2×), `estimate_diffusion(&[u64], w, h)` (activation table to be calibrated),
`overhead_for(weights)`.

### history.rs / forecast.rs — SPEC §6.2, §8.3
`History::{new(retention_ms), default() /*10 min*/, push, push_snapshot, len, is_empty, iter, last, window,
series(window_ms, f) -> Vec<(t_s, y)>, group_series(id)}`, `HistoryPoint` (additive: `swap_limit`,
`group_keys` — groups are stored as `group_key(id)` hashes; `push` converts `groups`), `ProcPoint`, `group_key`.
`linear_fit(&[(f64,f64)]) -> Option<LinearFit>`, `eta_to_threshold(points, threshold, rising)`,
`forecast_oom(&History, &Oom) -> Option<Forecast>`. Rules: R² ≥ 0.6, ETA < 30 min, ≥ 2 min of data, ≥ 8 points;
never on a single spike.

### attribution.rs — SPEC §7
Rule file format (TOML `[[group]]`): `id?`, `kind`, `label`, `match.{exe,name,script,cmdline,env,cwd,bundle,
cgroup}`, `exclude?`, `session_key? = "env:KEY"`, `priority`, `protected`. **Fields inside `match` are OR'd**;
`env` is not an identity match (it groups marker carriers into the rule's session). Globs are
case-sensitive. Types: `MatchSpec`, `Rule`, `RuleSet { group: Vec<Rule> }`, `RuleError`,
`CompiledRules::{compile(&RuleSet), match_root(&Process), match_marker(&Process), len, is_empty}`,
`LineageEntry { id, ppid, group_id, group_kind, group_label, group_fingerprint, session_id, spawned_by_agent,
first_seen_ms, last_seen_ms, last_active_ms }`, `AttributionContext { rules, lineage, protect }`,
`attribute(&Snapshot, &AttributionContext) -> Vec<Group>`, `slug`, `heuristic_kind_label`; additive:
`membership_signature(&Snapshot, &AttributionContext) -> u64` and `refresh_numbers(&mut [Group], &Snapshot)`
(the detector reuses the last attribution while the signature is unchanged and only refreshes totals/gains);
`CompiledRules` memoizes rule matches, heuristic labels and fingerprints by the process's identity fields.
Order: rule root (merging into a same-rule ancestor or the session group) → session marker → responsible pid →
ancestry → lineage (only if re-parented: recorded ppid > 1, now 1) → heuristic top-level root (apps keyed by
bundle name, e.g. `app:google-chrome`). Agent roots inherit the session id of their marker-carrying
descendants, so a session's group id is `agent:<hash12>` and survives restarts. Every process lands in exactly
one group; groups are sorted by footprint desc. Group ids are stable across samples for the same root.

### idle.rs — SPEC §7
`IdleConfig { cpu_pct_threshold: 0.5, idle_after_s: 1800, orphan_age_s: 600 }`,
`IdleTracker::{new, seed(&lineage), observe(&Snapshot, &cfg), last_active_ms(ProcId), idle_for(ProcId, now) ->
Measured<u64>, apply(&mut Snapshot, &lineage, &cfg)}`. Without history, idle is "≥ observed window" (`Estimate`).

### fingerprint.rs — UX §4
`cmdline_template(&[String], project_root) -> String`, `normalize_value`, `fingerprint(kind, exe_basename,
template, project_root, bundle_id) -> String` (16 hex), `process_fingerprint(kind, &Process)`,
`process_template`, `project_root(&Process)`, `display_name(&Process)`. Secrets are placeholder-ized first.

### throttle.rs / why.rs — SPEC §9
`throttle_factor(&[ClusterFreq]) -> Measured<f64>` (busy units only: active ≥ 50 %; else
`Unavailable("idle")`), `is_throttled(&Thermal)`, `speed_label(f)`, `BUSY_PCT`, `THROTTLED_BELOW = 0.8`.
`CauseKind`, `Cause { kind, score, title, evidence, fix }`, `explain(&Snapshot, &History) -> Vec<Cause>`
(sorted by score desc; nothing throttle-related when idle).

### redact.rs — SPEC §13
`REDACTED`, `DEFAULT_MARKER_ALLOWLIST`, `default_allowlist()`, `is_allowlisted(key, &[String])` (trailing `*`
= prefix), `hash_marker(&str) -> String`, `markers_from_env(pairs, allowlist) -> Markers`,
`is_secret_flag`, `redact_text`, `redact_cmdline(&[String]) -> Vec<String>`,
`redact_snapshot(&Snapshot) -> Snapshot`.

### actions.rs — SPEC §7, §13
`BUILTIN_PROTECTED`, `ProtectContext { self_pid, self_uid, ancestor_pids, protected_names, caller_group }`,
`is_protected_process(&Process, &Snapshot, &ProtectContext)`, `ActionKind { Graceful, Terminate, Kill,
Suspend, Resume }`, `ActionTarget`, `ActionPlan { targets, confirmed, confirmed_kill }`, `ActionOutcome`,
`Refusal`, `plan_group(&Group, ActionKind, &ProtectContext) -> Result<ActionTarget, Refusal>`,
`plan_stop(&Snapshot, &[String], &ProtectContext) -> (ActionPlan, Vec<Refusal>)`,
`trait Actuator: Send { fn execute(&mut self, &ActionPlan) -> Vec<ActionOutcome>; fn graceful_for(&self,
group_id) -> Vec<String> { empty } fn observe(&mut self, &Snapshot) {} }`, `NoopActuator`.

### query.rs — UX §5.2
`parse_filter(&str) -> Result<Expr, QueryError>`, `parse_term`, `Expr { And, Or, Not, Term }`,
`Term { Field{key, values}, Cmp{metric, op, value}, Word }`, `FieldKey`, `Metric { Mem, Gpu, Cpu, Idle }`,
`CmpOp`, `EntityView` (+ `from_group(&Group, &Snapshot)`), `eval(&Expr, &EntityView) -> bool`,
`Intent { Find, RankBy(Metric), Explain, Headroom, Reclaim, Navigate(View) }`, `View`, `Command`,
`VocabEntity`, `Vocabulary`, `Understanding`, `understand(&str, &Vocabulary) -> Understanding`,
`link_entities`, `fuzzy_score`, `damerau_levenshtein`.

### ranking.rs — UX §5.4
`RankWeights` (1.0/0.6/0.5/3.0/1.0), `Candidate`, `ScoreParts`, `score`, `frecency(&[(age_s, w)],
half_life_s)`, `affinity_from_frecency`, `group_salience/actionability/noise`, `group_candidates(&Snapshot,
Mode, &affinity_by_fingerprint, &muted_fingerprints)`, `rank(&[Candidate], &RankWeights, λ) -> Vec<(id,
ScoreParts)>`, `RankState::{new, apply(&[(id, score)], selected, margin) -> Vec<RankedRow>}`,
`MOVE_AFTER_REFRESHES = 2`, `explain_rank(position, &ScoreParts) -> String`.

### headline.rs / modes.rs — UX §9, §3
`HeadlineInput`, `Headline { text, action_key, mode }`, `input_from(&Snapshot, &Headroom, Mode)`,
`render(&HeadlineInput) -> Headline`. `Mode { Calm, Pressure, Throttle, Working, Leftovers }`,
`ModeSignals` (+ `desired()`), `signals(&Snapshot, &Headroom)`, `ModeMachine::{new, current, pin, is_pinned,
update(now_ms, &ModeSignals)}` (first sample adopted; enter 10 s, leave 30 s).

### provider.rs — frontend contract
`trait SnapshotProvider: Send { fn snapshot(&mut self) -> Snapshot; fn snapshot_now(&mut self) -> Snapshot
{ self.snapshot() } fn history(&self) -> &History; fn headroom_config(&self) -> HeadroomConfig { default } }`, `StaticProvider::new(Snapshot)`,
`SequenceProvider::new(Vec<Snapshot>)`.

---

## oomtop-collect — owner: collector builders (macOS / Linux)

`trait Source: Send { fn name(&self) -> &'static str; fn read(&mut self, budget: Duration) -> Result<RawSample,
SourceError>; }`, `SourceError { Unavailable(String), Timeout(u64), Io(String) }`, `now_ms()`.
`raw::{RawSample { source, taken_at_ms, read_us, payload }, RawPayload { MacHost, MacProcs, LinuxFiles,
Unavailable{reason} } (serde tag "type"), MacHostRaw, MacProcsRaw, MacProcRaw, MacRusage, MacSwap,
LinuxFilesRaw, names::{MACOS_HOST, MACOS_PROCS, LINUX_HOST, LINUX_PROCS}}`.
`decode::{decode(&RawSample, prev: Option<&RawSample>) -> PartialSnapshot, PartialSnapshot { host, memory, cpu,
oom, accelerators, thermal, processes, status } + apply(self, &mut Snapshot) + unavailable(src, reason)}`;
pure decoders `decode::macos::{decode_host, decode_procs, start_time_ms}`, `decode::linux::{decode_files,
parse_kv_kb, parse_psi, parse_stat, start_time_ms}` compile on every OS.
`sampler::{Sampler::{new(sources, Cadence, budget), platform_default(&SamplerOptions), sample_once,
sample_all, last_raw, source_names}, Tier { Host, Procs, Expensive }, Cadence (1 s / 2 s / 10 s),
SamplerOptions}` — failing sources back off exponentially (≤ 60 s) and report `source_status`.
`replay::{Fixture { meta, frames: Vec<Vec<RawSample>> }, FixtureMeta, load_fixture(&Path), fixture_to_json,
replay(&Fixture) -> Vec<Snapshot>}`.
`signal::{Signal { Term, Kill, Stop, Cont }, SignalError, process_start_time_ms(pid), is_alive(ProcId),
send(ProcId, Signal)}` — the only code that signals; refuses pid ≤ 1 and oomtop itself.
`macos` (cfg macos): `MacHostSource`, `MacProcSource::new(allowlist)`, `sysctl_int`, `sysctl_string`,
`timebase`, `list_pids`, `basic_info` (PROC_PIDTBSDINFO, falls back to `kinfo_proc` for other users),
`kinfo`, `start_time_ms`, `parse_procargs2`. `linux` (cfg unix; std::fs over a root, default `/proc`):
`LinuxHostSource::new(root)`, `LinuxProcSource::new(root, allowlist)`, `clk_tck`, `page_size`,
`SMAPS_PER_SAMPLE = 64` (rotation).
Example `capture` (`cargo run -p oomtop-collect --example capture -- out.json …`) writes redacted fixtures.

## oomtop-detect — owner: attribution builder

`builtin_rules() -> RuleSet` (from `src/builtin.toml`), `parse_rules(&str, origin) -> Result<RuleSet,
RuleLoadError>`, `load_rules_dir(&Path) -> (RuleSet, Vec<RuleLoadError>)` (alphabetical, bad files skipped),
`USER_PRIORITY_BOOST = 100`, `Detector::{new(RuleSet) -> Result<_, RuleError>, builtin(), with_user_rules(Option<&Path>)
-> (Detector, Vec<RuleLoadError>), rules(), compiled(), attribute(&Snapshot, &lineage, &ProtectContext) ->
Vec<Group>}`.

## oomtop-adapters — owner: model-server / sandbox builders

Loopback only (`ensure_loopback(url)`), only for processes that exist, `DEFAULT_TIMEOUT = 200 ms`.
`ProbeOptions { timeout, http, ports }`, `Probe { model_servers, sandboxes, status }`,
`probe(&Snapshot, &ProbeOptions) -> Probe`, `apply(&mut Snapshot, Probe)` (sets lists, marks VM groups
`lower_bound`), `AdapterError`.
`models::{classify(&Process) -> Option<ModelServerKind>, kind_key, arg_value, argv_model_files,
parse_ollama_ps, detect_model_servers(&Snapshot, &ProbeOptions, &mut status), GentleAction { OllamaUnload,
LmsUnload } + describe(), gentle_actions(&ModelServer), execute_gentle(&GentleAction, timeout)}`.
`sandboxes::{classify(&Process), detect_sandboxes(&Snapshot)}`.
`model_files::{estimate_model_file(&Path, &KvParams) -> Result<ModelEstimate, AdapterError>,
GGUF_HEADER_BYTES}` — headers only, never weights.

## oomtop-config — owner: config/theming builder

`Config` (sections: general, thresholds, headroom, protected, models, adapters, privacy, appearance, format,
keys, layout, personalization, serve, mcp, aliases, views; all `deny_unknown_fields` + defaults) with
`headroom_config()`, `idle_config()`; appearance enums `AppearanceMode, ColorMode, Background, Glyphs, Borders,
Density, Motion, Sparklines, MemoryUnits, CpuFormat, TimeFormat` (+ `as_str()`).
`layered::{LoadOptions { config_path, config_dir, system_dir, hostname, env, flags }, load_layered(&LoadOptions)
-> Loaded, Loaded { config, origins, files, errors } + effective() -> Vec<EffectiveEntry>, origin(key),
set_runtime(key, value), Origin { Default, File{path, line}, Env(key), Flag, Runtime } (Display "config.toml:12"),
leaves, find_line, parse_value}`. Precedence exactly UX §12.1.3; an invalid layer is skipped with file:line.
`load_default()`. `ConfigError { path, line, message }` (Display "file:line: msg").
`paths::{config_dir, state_dir, system_dir, expand_tilde, short_hostname, ConfigPaths}` (XDG on both OSes).
`write::{set_value(&Path, key, raw_value), set_in_text, validate_text, edit_value}` — toml_edit,
comment/order preserving, validated before writing.
`template::{init_template(), json_schema()}`; `docs::{DOCS, doc_for}` (a test fails if a setting lacks a doc).
`theme::{TOKENS, BUILTIN_THEMES, Style, Theme, ThemeError, ThemeIssue, valid_color, parse_theme, terminal_theme,
none_theme, builtin_theme, list_themes, load_theme(name, user_dir) (inherits + @refs resolved, missing tokens
from terminal), check, hex_rgb, contrast}`.
`keymap::{ACTIONS, PRESETS, Keymap { preset, contexts } + apply_user/action/keys_for/conflicts, Conflict,
preset(name), load_keymap(preset, user_file)}`.
`watch::{watch(paths, WatchMode, on_change) -> ConfigWatcher, WatchMode { Native, Poll(Duration) }, DEBOUNCE}`.

## oomtop-state — owner: personalization builder

`StateDb::{open(&Path), open_default(), open_in_memory(), path(), schema_version(), record_lineage(&[LineageEntry]),
lineage_for(&[ProcId]), lineage_all(), prune_lineage(before_ms), upsert_entity(fp, display_name, now),
record_event(fp, EventKind, at_ms, mode), set_pinned, set_muted(fp, until_ms), rename(fp, alias), entities(),
frecency(now_ms, half_life_s) -> HashMap<fp, f64>, record_query, top_queries, log_impression(&Impression),
ux_stats(since_ms) -> UxStats, set_machine_profile, machine_profile, reset_profile() (keeps lineage),
export_profile(now, half_life), prune(now)}`, `default_path()` (`$XDG_STATE_HOME/oomtop/state.db`),
`EventKind { Select, SearchSelect, Action, Pin, Mute, Less }` (+ weights), `EntityRecord`, `Impression`,
`UxStats`, `SCHEMA_VERSION`, `RETENTION_MS` (30 d), `NEGATIVE_HALF_LIFE_S` (30 d), `StateError`.
Lineage upsert keeps `first_seen`, the original ppid (> 1) and the agent attribution once set.

## oomtop-tui — owner: TUI builder

`TuiOptions { provider, actuator, config, theme, keymap, state, protect, plain, ascii, no_learn }`,
`run(TuiOptions) -> Result<(), TuiError>` (plain summary when `--plain`, `TERM=dumb` or not a TTY),
`plain_summary(&Snapshot, &Headroom, headline) -> String`, `key_string(KeyCode, KeyModifiers)`.
`app::{App::{new(&Config), update(Snapshot, &History), selected_group, on_raw_key, on_action}, View, Effect,
Input}`; `render::{draw(&mut Frame, &App, &Palette, &Glyphs), fit, Glyphs, UNICODE, ASCII}`;
`style::{Palette::{new(Theme, ColorDepth, paint_background), token(name)}, ColorDepth, parse_color}`;
`caps::{TermCaps, detect(ColorMode), detect_from(env, mode)}`. Snapshot tests (insta) at 60/80/120 columns.
Use `ratatui::crossterm` (same crossterm 0.29 as the workspace).

## oomtop-mcp — owner: MCP builder

`McpOptions { provider, actuator, allow_actions, protect, model_estimator, units }` (`units: UnitSystem`, added in
round 2: every summary in the configured `format.memory_units`, like the CLI; `Default` = IEC), `serve_stdio(McpOptions)`, `type ModelEstimator = Box<dyn Fn(&str) -> Result<u64, String> + Send>`,
`Server::{new, handle(&Value) -> Option<Value>}`, `tool_definitions(allow_actions)`, `PROTOCOL_VERSION =
"2025-06-18"`. Newline-delimited JSON-RPC over stdio. Tools: get_headroom, can_fit, top_consumers, list_groups,
list_model_servers, list_sandboxes, explain_slowdown, suggest_reclaim (all `readOnlyHint`); `reclaim` only with
`allow_actions` — the elicitation round-trip is M3; until then it executes nothing and returns the command.
Results carry `structuredContent` and a text copy; snapshots are redacted.

## oomtop-serve — owner: MCP/serve builder

`ServeOptions { listen, provider, refresh, max_requests, on_ready }`, `serve_http(ServeOptions)`,
`route(path, &Snapshot, &HeadroomConfig) -> (status, content_type, body)`, `prometheus_text(&Snapshot, &Headroom)`,
`DEFAULT_LISTEN = "127.0.0.1:9469"`. Endpoints `/healthz`, `/snapshot` (redacted), `/headroom`, `/metrics`.
Per-group series are aggregated by `(kind, label)`; no per-pid series.

## oomtop-cli — owner: CLI/integration builder

Binary `oomtop` (`src/main.rs` → `oomtop_cli::run(args) -> i32`). Global flags: `--config`, `--no-learn`,
`--plain`, `--ascii`, `--theme`, `--color`, `--offline`, `--replay FIXTURE`, `--set KEY=VALUE`.
Subcommands: `headroom [--need SIZE | --model PATH] [--ctx N] [--json]`, `why [--json]`,
`reclaim [--dry-run] [--yes] [--groups a,b] [--json]`, `json`, `ndjson [--interval 2s] [--count N]`,
`serve [--listen]`, `mcp [--allow-actions]`, `doctor [--json]`,
`config init [--force] [--print] | edit [--host] | print [--effective] [--origin] | validate |
set KEY VALUE [--layer user|host|dropin:<name>] | schema`, `theme list | preview | check | import | export`,
`keys list [--conflicts] [--preset]`, `profile show | export | reset [--yes] | stats`.
`engine::Engine::{live(&Config, Option<StateDb>, on_demand, offline), replay(&Config, &Path), enrich(Snapshot),
flush(), self_group(&Snapshot), state_mut()}` implements `SnapshotProvider` (on-demand = two samples 150 ms
apart, or one when the previous call's sample is ≤ 30 s old; `snapshot_now` forces every source); `actuator::SignalActuator` implements `Actuator`. Exit codes: 0 ok/yes · 1 error · 2 usage ·
3 yes after reclaim · 4 no.

---

## Checks every builder runs

```
export PATH=/opt/homebrew/opt/rustup/bin:$PATH CARGO_BUILD_JOBS=2
cargo fmt -p <crate>
cargo clippy -p <crate> --all-targets -- -D warnings
cargo test -p <crate>
```

---

## Additive APIs after M0 (v0.1.0 integration, 2026-09-30)

All additive; no frozen signature or `model.rs` type changed. Listed so the next builder knows they exist.

- **core** — `headroom::{MacVmPages, macos_available_now, linux_available_now, memorystatus_bytes,
  unified_budget_estimate, discrete_gpu_margin, gpu_headroom, sub_i64, is_exact, swap_growing_in_history,
  ReclaimModel, reclaim_gain_with, group_swap_gain, group_gpu_gain}`, `Headroom.unified_memory` (serde default);
  `can_fit::{can_fit_snapshot, answer_exit_code, Need::from_estimate}`; `forecast::{forecast_oom_with,
  MAX_STALE_S}`; `model_estimate::{MAX_LAYERS, GgufMeta::kv_heads_sum}`; `ranking::{CONTEXT_AFFINITY_WEIGHT,
  RankContext.mode_affinity, RankFacts.opens_in_mode}`; `query::name_tokens`; `attribution::app_bundle_name`.
  `actions::plan_stop` now also refuses groups whose root process is missing or protected.
- **collect** — `decode::macos_extras` (pure, every OS; re-exported as `macos::extras`): `decode_host_full`,
  `decode_extras`, `apply_extras`, `fanless_model`, `keys`, `status_keys`, `JetsamRecord`; `decode()` applies
  it to every `MacHost` sample and `linux::enrich` to every `LinuxFiles` sample (`linux::decode_full` is now an
  alias of `decode`). macOS: `MacHostSource::with_extras`, `HostExtras`, `ioreport::*`, `jetsam::JetsamScanner`,
  `sensors`, `groundtruth`, `kinfo_all` (one `KERN_PROC_ALL` per sample). Linux: `enrich`, `replay_full`,
  `gpu`, `oom`, `smaps`, `nvml` modules.
- **detect** — `Detector::{lint(allowlist) -> Vec<RuleWarning>, matching_rule, matching_marker_rule}`.
- **adapters** — `http::connect_unix`, `docker::valid_container_id`, `models::{argv_model_source, stop_guard,
  StopGuard}`, `sandboxes::{detect_seatbelt, seatbelt_check, MAX_SEATBELT_CHECKS, LIVE_BUDGET}`,
  `weights::scan_procfs_within`, `lm_studio::find_lms_in`, `model_index::{scan, mark_loaded, IndexOptions,
  ModelIndex, ModelFile, DuplicateSet}`, `ProberConfig` (engine budget 300 ms, ≤ 16 stats).
- **config** — `theme::{check_structure, valid_theme_name, xterm256_rgb, Variant::{resolve, from_colorfgbg}}`,
  `watch::MIN_POLL`.
- **tui** — `run_with(TuiOptions, TuiExtras)`, `TuiExtras { loaded, load_options, paths, watch, background,
  disk_models }`, `DiskModel` (+ `loaded_by(&Snapshot)`), `App.disk_models`, `columns`, `load_active_layout`,
  `auto_variant`, `wanted_variant`, `App::{set_keymap, key_for, set_layout, reclaim_all_targets}`,
  `Effect::Layout`, `HomeSort::{Gpu, Idle, Reclaim}`.
- **mcp / serve** — `export_snapshot` (core redaction + model/job label redaction), `serve::{headroom_json,
  route_with}`; reclaim status `failed`.
- **cli** — `Engine::{lineage_len, rule_warnings}`, `LINEAGE_EVICT_MS`; `termprobe::{Probe.truecolor,
  truecolor_from_decrqss, skip_probe_for}`; subcommand `oomtop models`.

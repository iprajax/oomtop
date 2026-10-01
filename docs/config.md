# Configuring oomtop

oomtop works with no configuration at all. Everything below is optional, lives in plain TOML files, and can be
edited by hand, with `oomtop config set`, or from the in-app settings screen (`,`). Edits preserve your comments
and ordering.

## Files

```
~/.config/oomtop/                 ($XDG_CONFIG_HOME/oomtop on both Linux and macOS)
  config.toml                     main settings — `oomtop config init` writes it fully commented
  config.d/*.toml                 drop-ins, applied alphabetically after config.toml
  config.d/host-<hostname>.toml   per-machine overrides (applied last among files)
  themes/<name>.toml              your themes (a file named like a built-in overrides it)
  keymap.toml                     key remapping on top of a preset
  layouts/<name>.toml             panel/column layouts
  rules.d/*.toml                  your detection rules (see below)
  schemas/*.schema.json           JSON Schemas written by `config init` (editor completion via taplo)
~/.local/state/oomtop/state.db    local profile + lineage journal ($XDG_STATE_HOME); never needed to reproduce your setup
/etc/oomtop/config.toml, config.d/  system-wide defaults (lowest file layer)
```

`--config PATH` (or `OOMTOP_CONFIG=PATH`) replaces `config.toml`; drop-ins, themes, keymap and rules are then read
from that file's directory.

All files are live-reloaded on save (debounced 200 ms). An invalid file is skipped with a `file:line` message and
the last good values stay in effect.

## Precedence (later wins)

```
built-in defaults → /etc/oomtop/ → config.toml → config.d/*.toml → config.d/host-<hostname>.toml
  → OOMTOP_* environment variables → command-line flags → runtime toggles (not saved unless you save them)
```

See exactly where every value comes from:

```console
$ oomtop config print --origin appearance
appearance.theme = "mint"    # work.toml:2
appearance.color = "auto"    # default
...
```

Environment variables mirror keys with `__` for nesting: `OOMTOP_APPEARANCE__THEME=ember`,
`OOMTOP_GENERAL__REFRESH_MS=1000`. Also `OOMTOP_CONFIG=/path/config.toml` and `OOMTOP_NO_LEARN=1`.
Flags: `--theme NAME`, `--color auto|truecolor|256|16|none`, `--ascii`, `--no-learn`, and `--set KEY=VALUE` for
anything (repeatable).

## Commands

| Command | What it does |
|---|---|
| `oomtop config init [--force] [--print]` | write a fully commented `config.toml` (every option at its default) + JSON Schemas + `.taplo.toml` |
| `oomtop config edit [--host]` | open `config.toml` (or this machine's `host-<hostname>.toml`) in `$VISUAL`/`$EDITOR`, then validate |
| `oomtop config print [--effective] [--origin] [KEY]` | the merged config, or one line per setting with the layer that set it |
| `oomtop config validate` | check every file (config layers, themes, keymap, layouts, rules) with `file:line` errors; exit 1 on errors |
| `oomtop config set KEY VALUE [--layer user\|host\|dropin:NAME]` | comment-preserving edit, validated before writing |
| `oomtop config unset KEY [--layer …]` | remove a key from a layer (lower layers / defaults apply again) |
| `oomtop config schema [config\|theme\|keymap\|layout\|rules] [--write DIR]` | print a JSON Schema, or write them all |

Values are TOML literals; bare words are strings: `oomtop config set appearance.theme ember`,
`oomtop config set headroom.min_margin 2G`, `oomtop config set protected.names '["postgres", "Xcode"]'`.

## Recipes

**Bigger safety margin on a fanless laptop** (headroom answers are advisory; the margin keeps them honest):

```toml
# ~/.config/oomtop/config.d/host-air.toml
[headroom]
min_margin = "3G"
margin_pct = 12.0
```

**Never offer my database for reclaim:** `oomtop config set protected.names '["postgres"]'`.

**Ollama on a non-default port:** `oomtop config set adapters.ports '{ ollama = 11500 }'`.

**No learning at all:** `oomtop config set personalization.learn false` (or run with `--no-learn`). Existing
learned data can be removed with `oomtop profile reset`.

**A machine-readable pipe without redaction** (local use only; `serve` and MCP always redact):
`oomtop --set privacy.redact_exports=false ndjson`.

## Detection rules (`rules.d/*.toml`)

Rules teach oomtop to group your own tools. Fields inside `match` are OR'd; globs are case-sensitive.

```toml
[[group]]
kind = "agent_session"                              # agent_session | app | model_server | sandbox | build_daemon | system | other
label = "My agent"
match.exe = ["my-agent"]                            # native binary
match.script = ["**/my-agent/dist/cli.js"]          # interpreter-launched (exe = node/python)
match.env = ["MY_AGENT_SESSION"]                    # marker key; must also be in privacy.marker_allowlist
session_key = "env:MY_AGENT_SESSION"                # one group per session (the value is hashed)
priority = 10
protected = false
```

User rules get a priority boost over the built-in ones. `oomtop config validate` checks them;
`oomtop doctor` lists any rule that failed to load.

## Every setting

Generated from the same source as `oomtop config init --print` (defaults shown; run that command for the full,
commented file).

| Setting | Default | What it does |
|---|---|---|
| `general.refresh_ms` | `2000` | per-process refresh in ms (TUI, serve) |
| `general.host_refresh_ms` | `1000` | host stats refresh in ms |
| `general.expensive_refresh_ms` | `10000` | expensive reads (smaps rotation, adapters) in ms |
| `general.mouse` | `true` | enable mouse support in the TUI |
| `general.live_reload` | `true` | reload config, themes, keymap and layouts on save (debounced 200 ms) |
| `general.watch_poll_ms` | `0` | 0 = native file events; > 0 = poll every N ms (network filesystems) |
| `general.other_users` | `true` | show other users' processes (`false` = only yours, like htop's user filter; host totals stay whole-machine) |
| `thresholds.idle_cpu_pct` | `0.5` | per-core CPU % below which a process counts as inactive |
| `thresholds.idle_after_s` | `1800` | seconds without CPU/disk activity before a process is idle |
| `thresholds.orphan_age_s` | `600` | minimum age in seconds before a leftover counts as an orphan |
| `headroom.min_margin` | `"1.5G"` | minimum safety margin (e.g. "1.5G") |
| `headroom.margin_pct` | `8.0` | safety margin as % of RAM (the larger of this and min_margin wins) |
| `headroom.pressure_boost_pct` | `50.0` | extra margin % when under pressure or swap is growing |
| `headroom.margin_override` | `""` | fixed safety margin (e.g. "2G"); empty = computed |
| `protected.names` | `[]` | extra process names never offered for stop/suspend |
| `models.folders` | `["~/.cache/huggingface", "~/.ollama/models", "~/.lmstudio/models"]` | folders indexed for model files |
| `adapters.enabled` | `true` | probe local model-server APIs (127.0.0.1 only) |
| `adapters.timeout_ms` | `200` | adapter HTTP timeout in ms |
| `adapters.ports` | `{}` | Port overrides per server kind |
| `privacy.marker_allowlist` | `["CLAUDECODE", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_ENTRYPOINT"…` | environment keys read as session markers (values hashed, never stored) |
| `privacy.redact_exports` | `true` | redact command lines in json/ndjson output (serve and MCP always redact) |
| `appearance.theme` | `"terminal"` | terminal (default) \| mono \| ember \| mint \| sand \| coral \| high-contrast \| colorblind \| none \| <user theme> |
| `appearance.appearance` | `"auto"` | auto \| dark \| light |
| `appearance.color` | `"auto"` | auto \| truecolor \| 256 \| 16 \| none |
| `appearance.background` | `"transparent"` | transparent (never paint) \| theme (paint the theme surface) |
| `appearance.glyphs` | `"unicode"` | unicode \| nerd \| ascii |
| `appearance.borders` | `"rounded"` | rounded \| plain \| thick \| double \| none |
| `appearance.density` | `"comfortable"` | comfortable \| compact |
| `appearance.motion` | `"marks"` | marks (one-refresh change markers) \| none |
| `appearance.sparklines` | `"braille"` | braille \| blocks \| none |
| `appearance.header` | `["headline", "memory", "swap", "accelerators", "thermal"]` | header rows in display order |
| `format.memory_units` | `"iec"` | iec (GiB) \| si (GB) |
| `format.decimals` | `1` | decimals for memory values |
| `format.cpu` | `"per-core"` | per-core (100% = 1 core) \| total (100% = machine) |
| `format.time` | `"relative"` | relative ("idle 5h") \| clock |
| `keys.preset` | `"default"` | default \| vim \| emacs \| htop |
| `layout.name` | `""` | layout from layouts/<name>.toml; empty = built-in adaptive layout |
| `personalization.learn` | `true` | learn from what you open, pin and search (local only) |
| `personalization.half_life_days` | `7.0` | frecency half-life in days |
| `personalization.retention_days` | `30` | days the local impression/selection log is kept |
| `personalization.log_max_mb` | `5` | size cap of the local impression/selection log in MB |
| `personalization.weights.salience` | `1.0` | ranking weight: share of memory/GPU/CPU |
| `personalization.weights.affinity` | `0.6` | ranking weight: your frecency for the entity |
| `personalization.weights.actionability` | `0.5` | ranking weight: reclaimable bytes / fixable cause |
| `personalization.weights.query` | `3.0` | ranking weight: active query match |
| `personalization.weights.noise` | `1.0` | ranking weight: muted / system noise penalty |
| `serve.listen` | `"127.0.0.1:9469"` | HTTP listen address for `oomtop serve` (127.0.0.1 unless changed) |
| `mcp.allow_actions` | `false` | register the elicitation-gated MCP reclaim tool |
| `aliases` | `{}` | query aliases, e.g. llm = "kind:model,agent" |
| `views` | `[]` | saved views: [[views]] name/query/layout |
| `general` | — | Sampling cadence and general behavior |
| `models` | — | Model files on disk (SPEC §10) |
| `format` | — | Number and time formatting (UX §12.5) |
| `serve` | — | `oomtop serve` HTTP endpoint (SPEC §12.2) |
| `mcp` | — | `oomtop mcp` server for agents (SPEC §12.3) |

## Privacy

Command lines are redacted in every export (`json`, `ndjson`, `serve`, MCP): `--token=…`, `KEY=…` pairs and URLs
with credentials become `<redacted>`. Environments are read only for the allowlisted marker keys above, are hashed,
and are never stored or exported. Nothing leaves the machine: no telemetry, and adapters only talk to
127.0.0.1.

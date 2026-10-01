# oomtop command reference

`oomtop --help` and `oomtop <command> --help` print the same information from the binary itself.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok · for `headroom`: **yes**, it fits now |
| 1 | error (or `headroom`: couldn't measure; `config validate` / `theme check`: problems found) |
| 2 | usage error (bad flag or value, e.g. `--need banana`) |
| 3 | `headroom`: **yes after reclaim** — fits if the listed idle groups are stopped |
| 4 | `headroom`: **no** — doesn't fit even after reclaim |

## Global flags (any position)

| Flag | Effect |
|---|---|
| `--config PATH` | use PATH instead of `~/.config/oomtop/config.toml` (also `OOMTOP_CONFIG`); drop-ins, themes, keymap and rules are read from its directory |
| `--no-learn` | don't record learning signals or refresh the machine profile (also `OOMTOP_NO_LEARN=1`) |
| `--plain` | linear, screen-reader-friendly output instead of the TUI (automatic for `TERM=dumb` and pipes) |
| `--ascii` | ASCII glyphs only |
| `--theme NAME` | theme for this run |
| `--color auto\|truecolor\|256\|16\|none` | color depth for this run (`NO_COLOR` always wins) |
| `--offline` | no HTTP calls to local model-server APIs (process-level detection only) |
| `--replay FIXTURE` | run against a recorded machine (`fixtures/…json`); state is not written and nothing is ever signalled |
| `--set KEY=VALUE` | override any setting for this run (repeatable), e.g. `--set appearance.theme=none` |

## Commands

### `oomtop` — the TUI

Ranked groups (agent sessions, apps, model servers, sandboxes, build daemons), true-memory bar, swap trend and
forecast, pressure, thermal/throttle badge. Views `1`–`6`, `/` filter, `x` stop, `z` suspend, `?` help,
`,` settings. See [UX.md](../UX.md) §7 for keys; `oomtop keys list` for the effective bindings.

### `oomtop headroom [--need SIZE | --model PATH] [--gpu SIZE] [--json]`

"Can I load this now?" Without a need, prints the budget: available memory, safety margin, headroom, GPU/Metal
budget, pressure, swap trend and forecast.

| Option | |
|---|---|
| `--need SIZE` | host RAM needed: `13G`, `500M`, `1.5GiB` (K/M/G/T and KiB… are 1024ⁿ; KB/MB/GB are 1000ⁿ) |
| `--model PATH` | a `.gguf` file or a safetensors/MLX directory; only headers and `config.json` are read |
| `--ctx N` | context length for the KV-cache estimate (default: the model's, capped at 8192) |
| `--kv-type TYPE` | KV cache element type: `f16` (default), `q8_0`, `q4_0`, … |
| `--parallel N` | parallel sequences (llama.cpp `-np`, `OLLAMA_NUM_PARALLEL`) |
| `--offload auto\|gpu\|cpu` | `auto`: GPU-resident on Apple Silicon with a Metal budget, host RAM otherwise |
| `--gpu SIZE` | memory that must also fit the GPU budget (discrete GPUs) |
| `--json` | the full answer: `fit` (`answer`: `yes` / `yes_after_reclaim` / `no`), `need`, `headroom`, `reason`, `notes`, `as_of_ms`, `valid_for_s`, `expires_at_ms`, and `estimate` for `--model` |

The answer is advisory and valid for about 10 s; the calling session is never proposed for reclaim.

### `oomtop why [--json]`

Ranked causes of slowness or memory pressure with evidence and a fix: imminent OOM, swap storm, memory pressure, thermal
throttling and GPU throttling (only when something is busy), Low Power Mode, low battery, CPU saturation. Lists sources that couldn't
be measured.

### `oomtop reclaim [--dry-run] [--yes] [--groups a,b] [--wait 5s] [--json]`

Idle build daemons, orphans and idle model servers, largest estimated gain first.

1. Lists each group, its estimated RAM gain (and swap freed, separately), idle time and the planned action:
   the model server's own unload (Ollama `keep_alive: 0`, `lms unload`) when available, otherwise SIGTERM to the
   group root.
2. Asks `[y/N]`. `--yes` confirms exactly the listed plan; without a terminal and without `--yes` it refuses.
3. Waits up to `--wait` for the groups to exit, then offers SIGKILL for survivors — only after a second,
   interactive confirmation (`--yes` never escalates).
4. Reports the memory actually freed.

Never targeted: oomtop itself, its terminal/shell ancestry, the calling agent's session, protected names
(`protected.names`), system groups, and anything owned by another user. Suspend is not reclaim.

### `oomtop models [--json] [--no-hash]`

Model files on disk (SPEC §10): the Hugging Face cache (`$HF_HUB_CACHE`), Ollama (`$OLLAMA_MODELS`,
`~/.ollama/models`), LM Studio (`~/.lmstudio/models`), the llama.cpp cache and every folder in `models.folders`.
Largest first, with format, last use (atime, coarse on `noatime` mounts), duplicates (same size and the same hash
of the first and last MiB) with the space a single copy would save, and which model server or process has each
file loaded right now. Only directory entries are read, plus 2 MiB of each duplicate candidate (`--no-hash` skips
that). `--json` prints the index with home-relative (`~/…`) paths.

### `oomtop json [--compact]` · `oomtop ndjson [--interval 2s] [--count N]`

A snapshot (schema version 1) as JSON, or one per interval as newline-delimited JSON (minimum interval 250 ms).
Every measured number is `{ "value", "source", "quality" }`; unavailable values are `null` with the reason,
never 0. Command lines are redacted; environments are never included.

### `oomtop serve [--listen 127.0.0.1:9469]`

HTTP: `/snapshot` (redacted JSON), `/headroom`, `/metrics` (Prometheus: host series and per-group series labelled
by `kind` and `label`; no per-pid series), `/healthz`. Binds loopback unless told otherwise, with a warning.

### `oomtop mcp [--allow-actions]`

MCP server over stdio for agents. See [mcp.md](mcp.md).

### `oomtop doctor [--json] [--no-probe]`

Host, every source's status (`available` / `partial` / `unavailable`) with the reason and a hint — plus
`missing` for a source this OS normally has that the running build didn't report at all — terminal capabilities
(truecolor, Kitty keyboard protocol, synchronized output, OSC 8 hyperlinks, OSC 11 background color, SGR mouse,
focus events), config files and errors, rules, state store, privileges. When run in a terminal it queries the
terminal directly (OSC 11, `CSI ? u`, DECRQM, a DECRQSS read-back of a 24-bit test color, then DA1; bounded to
200 ms, never on `TERM=dumb`/`linux`) and says which answers came from the terminal and which from the
environment.

### `oomtop config …`

`init [--force] [--print]` · `edit [--host]` · `print [--effective] [--origin] [KEY]` · `validate` ·
`set KEY VALUE [--layer user|host|dropin:NAME]` · `unset KEY [--layer …]` · `schema [KIND] [--write DIR]`.
See [config.md](config.md).

### `oomtop theme …`

`list` · `preview [NAME] [--variant dark|light]` · `check NAME` · `import FILE [--name N] [--force] [--print]` ·
`export NAME [-o FILE]`. See [themes.md](themes.md).

### `oomtop keys list [--conflicts] [--preset default|vim|emacs|htop]`

Effective key bindings per context (preset plus `keymap.toml`); `--conflicts` prints only conflicting bindings and
exits 1 if there are any, or one line — `no conflicts (preset default, 46 bindings checked)` — and exits 0.

### `oomtop profile show [--json] | export | reset [--yes] | stats [--json]`

The local profile (UX §6): machine facts, detected roles (local LLM, diffusion, agent-heavy, JVM/Android, web dev,
containers) and what they adapt, plus remembered entities with their frecency. `export` prints everything learned
as JSON (no command lines, no environments). `reset` forgets it (the lineage journal is kept). `stats` shows local
UX metrics (Hit@3, keystrokes to target, reformulation and dismiss rates). Stored in
`$XDG_STATE_HOME/oomtop/state.db`; nothing leaves the machine.

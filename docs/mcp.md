# oomtop for agents (MCP)

`oomtop mcp` is a Model Context Protocol server over stdio. It lets a coding agent ask **"can I load this
now?"** before it pulls a 13 GB model or starts a heavy build, and see what is eating the machine — with the same
numbers the TUI shows (true footprint/PSS, GPU/Metal, compressed and swapped memory, attribution to agent
sessions, apps, sandboxes and model servers).

It is **read-only by default**. There is no network listener: the agent starts it as a child process and talks
JSON-RPC over stdin/stdout. It samples only when a tool is called, so it costs no CPU while idle.

## Setup

### Claude Code

```console
$ claude mcp add oomtop -- oomtop mcp
$ claude mcp add --scope user oomtop -- oomtop mcp      # for every project
$ claude mcp list
```

Then ask: *"Before loading qwen-image, check with oomtop whether it fits."*

If `oomtop` is not on the `PATH` Claude Code sees, use the absolute path (`which oomtop`), e.g.
`claude mcp add oomtop -- /opt/homebrew/bin/oomtop mcp`.

### Other clients

Claude Desktop (`claude_desktop_config.json`), Cursor (`.cursor/mcp.json`), Gemini CLI (`settings.json`):

```json
{
  "mcpServers": {
    "oomtop": { "command": "/opt/homebrew/bin/oomtop", "args": ["mcp"] }
  }
}
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.oomtop]
command = "oomtop"
args = ["mcp"]
```

## Tools

All tools return `structuredContent` (with an `outputSchema`) plus a JSON text copy. Snapshots are redacted
(command-line secrets removed; environments never included).

| Tool | Arguments | Returns |
|---|---|---|
| `get_headroom` | — | available memory, safety margin, headroom, GPU/Metal budget, pressure, swap trend, OOM forecast |
| `can_fit` | one of `bytes`, `size` (`"13G"`), `model_path`; optional `gpu_resident`, `label` | `yes` / `yes_after_reclaim` (which groups, estimated gain) / `no` (shortfall), valid ~10 s |
| `top_consumers` | `by`: `footprint` \| `gpu` \| `cpu`, `n` | the largest groups |
| `list_groups` | `kind`?, `limit`? | agent sessions, apps, model servers, sandboxes, build daemons… with members and totals |
| `list_model_servers` | — | detected servers (Ollama, llama.cpp, sd.cpp, vLLM, LM Studio, MLX) and loaded models |
| `list_sandboxes` | — | containers and VMs with their host cost |
| `explain_slowdown` | — | ranked causes (swap storm, memory pressure, thermal throttling, low power…) with evidence |
| `suggest_reclaim` | — | idle build daemons, orphans and idle model servers with estimated gains; **no side effects** |
| `reclaim` | `group_ids` | only with `--allow-actions`; see below |

`can_fit` is advisory: another process can take the memory between the answer and the load. Answers carry
`as_of_ms`, `valid_for_s` (10) and `expires_at_ms`; call again right before loading. The calling agent's own
session is never proposed for reclaim.

Every `summary` and `reason` uses the configured `format.memory_units` (IEC by default, one decimal) — the same
sentence `oomtop headroom` prints: `size: "13G"` is echoed as "13.0 GiB", never "14 GB", and a sentence never mixes
"GiB" with a short "4.5G". `get_headroom`'s summary is the TUI headline (available now, e.g. "All good — 4.8 GiB
free.") followed by the headroom `can_fit` decides with: "Headroom 2.9 GiB (4.8 GiB available minus 1.9 GiB safety
margin)." — plan against the headroom, not the free figure. Byte fields (`need.bytes`, `host_shortfall`, `reclaimable`, …) are integers.

## Allowing actions

```console
$ claude mcp add oomtop -- oomtop mcp --allow-actions
```

(or `mcp.allow_actions = true` in `config.toml`). This registers `reclaim` (annotated `destructiveHint`). Even
then, oomtop asks **you**, not the agent: it sends an MCP *elicitation* with the exact list of groups, their pids
and estimated gains, and stops them (SIGTERM; never SIGKILL) only if you accept. If the
client doesn't support elicitation, nothing is executed and the tool returns the `oomtop reclaim --groups …`
command for you to run yourself. Only what `oomtop reclaim` would offer can be targeted — idle build daemons,
orphans and idle model servers (`suggest_reclaim` lists them); an active app or another agent's live session is
refused with a reason. Model servers with an unload API are unloaded gently (no signal); a server whose job is
running is refused, and "idle not confirmed" is spelled out in the confirmation. Protected processes, other
users' processes, oomtop itself and the calling agent's session are never targets.

## Try it by hand

```console
$ printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"me","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"can_fit","arguments":{"size":"13G"}}}' \
  | oomtop mcp
```

Add `--replay fixtures/macos/m5-air-agents.json` to try it against a recorded machine instead of yours.

## HTTP instead of MCP

For dashboards and scripts, `oomtop serve` exposes the same data over HTTP on 127.0.0.1:9469: `/snapshot`
(redacted JSON), `/headroom`, `/metrics` (Prometheus; host series plus per-group series labelled by `kind` and
`label`, no per-pid series) and `/healthz`. For shell scripts, `oomtop headroom --need 13G` answers with its exit
code: 0 yes · 3 yes after reclaim · 4 no · 1 couldn't measure.

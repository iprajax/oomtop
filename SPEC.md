# oomtop — specification

> **See the OOM coming.** Who's eating your machine: agents, models, sandboxes.

Status: draft v0.2 · 2026-09-29 · Linux + macOS

### What changed in v0.2 (and why)
- **OOM forecast is now a first-class feature** (§8.3) — the tagline promised it; v0.1 had no section for it.
- **Memory taxonomy corrected against this Mac** (§5): per-process *compressed* needs task ports on macOS (`top` is
  setuid root with `com.apple.system-task-ports.read`), so unprivileged oomtop shows it host-level only; per-process
  *swapped* is not attributable on macOS at all; Virtualization.framework VMs are under-counted by footprint.
- **Headroom corrected** (§8): macOS "available" no longer counts anonymous inactive pages; Metal GPU budget
  (`recommendedMaxWorkingSetSize` / `iogpu.wired_limit_mb`) added; reclaim gain is RAM freed, not footprint;
  model estimate no longer multiplies an already-quantized file by a quant factor.
- **Sampling model defined** (§6.2): no daemon; MCP samples on demand; a shared lineage journal makes orphans and
  idle times survive restarts. Added `oomtop-config` and `oomtop-state` crates so `oomtop-core` stays pure.
- **Collectors split into Source (I/O) + decode (pure)** so macOS syscalls can be recorded and replayed (§6.1, §17).
- **Throttle factor only under load** (§9) — Apple Silicon down-clocks at idle; v0.1 would have reported idle as
  throttling. Low Power Mode and public thermal-pressure APIs added.
- **Safety**: act on group roots, prefer adapter-graceful actions (e.g. Ollama unload), re-verify `(pid, start_time)`
  before signalling, MCP `reclaim` confirms via MCP *elicitation* instead of trusting the host (§12.3, §13).
- **Packaging fix**: fully static musl binaries cannot `dlopen` NVML → glibc build is the primary Linux artifact (§16).
- **Key conflicts removed** (`k` was both "up" and "stop"; `p`/`m` were both tabs and pin/mute) — see UX §7.
- **Milestones re-ordered** so the agent-facing MCP ships right after headroom, before personalization (§18).
- Open questions resolved where evidence allowed (§19).

---

## 1. Problem

Local AI work — coding agents, local LLM / diffusion servers, sandboxes, browsers driven by agents — puts a machine
under memory, GPU and thermal pressure, and the classic tools can't explain it. Evidence from a single session on a
24 GB M5 MacBook Air running an image-generation server and four agent sessions:

| Question | What htop showed | What was true |
|---|---|---|
| Who holds the memory? | `sd-server` not near the top; RES ≈ small | **9.9 GB** in Metal/GPU allocations (only visible in macOS *footprint*) |
| Are the Java processes big? | RES ≈ 0.5 GB | **~3 GB each** — mostly compressed/swapped (Gradle + Kotlin daemons idle for 5 h) |
| What is that VM? | `com.apple.Virtualization.VirtualMachine` | The Claude desktop app's sandbox VM (`claudevm.bundle`) |
| Why did generation get 2–3.5× slower? | nothing | Thermal throttling + low battery + 6–7.5 GB swap — `pmset` reported "no thermal warning" |
| Which session spawned what? | flat process list | 4 agent sessions, headless Chrome, benchmark runs, orphaned daemons |

Stopping the idle daemons and the model server moved free memory from **36 % → 84 %**. Nothing on screen had
pointed at them, and nothing warned that the machine was heading for swap exhaustion.

## 2. Goals / non-goals

**Goals** (v1 core = 1, 2, 3, 7, 8; then 4, 5, 6)
1. **True memory accounting** per process and per group, with the source and quality of every number.
2. **Attribution**: group every process under the agent session, app, sandbox, model server or system component that owns it.
3. **Headroom & OOM forecast**: *"can I load X GB / this model now without swapping?"*, *what to stop to make room*,
   and *"at this rate you hit swap exhaustion / the OOM killer in ~6 min — and it will pick X"*.
4. **Explain slowdowns**: thermal/power throttling, Low Power Mode, memory pressure, swap storms — with a cause.
5. **Model-server awareness**: loaded models, weights vs KV/working memory, throughput, queue.
6. **Sandbox/VM/container awareness**: host cost attributed to who started them.
7. **Idle-cost & orphan reclaim**: find processes that cost memory and do nothing; reclaim safely.
8. **Agent-native**: an MCP server so agents can check headroom before starting heavy work.
9. **Linux + macOS** from day one, no root required by default (root adds detail, never required).

**Platforms:** macOS 13+ on Apple Silicon is primary (Intel Macs: best effort, no IOReport GPU/power). Linux: kernel
≥ 5.10 baseline (smaps_rollup, PSI, cgroup v2); accelerator features detect keys at runtime instead of gating on
kernel versions.

**Non-goals (v1)**
- Token/cost tracking of cloud LLM APIs — **agtop** does this well.
- Fleet/cluster monitoring (Kubernetes, multi-host). Single machine only.
- Windows (the Source trait stays portable).
- Kernel modules / eBPF as a requirement.
- A background daemon or launch agent (every frontend samples for itself; §6.2).
- Auto-killing or auto-suspending anything without explicit user confirmation.

## 3. Users & scenarios

- **Local-LLM developer** on a Mac: *"Can I load the 13 GB Q8 model now?"* → `oomtop headroom --need 13G` → "No: 9.1 GB
  available after safety margin. Stopping *GradleDaemon* (≈2.9 GB freed, idle 5 h) and *KotlinCompileDaemon*
  (≈3.0 GB, idle 7 h) → Yes." Scripts use the exit code: `oomtop headroom --need 13G && ./start.sh`.
- **Agent-heavy engineer**: *"What did my agent sessions leave behind?"* → the Leftovers card shows orphans (headless
  Chrome from an ended session, a benchmark server) with one-key reclaim.
- **Anyone near the edge**: swap growing +400 MB/min → headline *"Swap full in ~6 min at this rate — sd-server
  (9.9 GB) is the largest; 2 idle daemons could free 5.9 GB."*
- **Linux workstation with NVIDIA/AMD**: `oomtop why` → "GPU at power limit (throttle reason: SW power cap); 3.1 GB
  swapped by `vllm` workers; memory PSI some=38 %; systemd-oomd kills at 60 % for 30 s."
- **An agent** (Claude Code, Codex…) calls MCP `can_fit({bytes: 13e9})` before launching a model, and
  `suggest_reclaim()` to ask the user for permission to stop idle hogs.

## 4. Landscape (researched 2026-09) and positioning

| Tool | Strength | Gap oomtop fills |
|---|---|---|
| **agtop** (Rust; Linux/macOS/Windows/FreeBSD) | ~20 agent matchers, tokens/cost, context %, disk I/O, TCP, NVIDIA GPU, detects local model servers | No container/VM attribution, no power, RSS-based memory, NVIDIA-only GPU |
| **agentop / agenttop** (npm) | Session status dashboards (thinking/tool/waiting, branch, model) | Little resource data |
| **aitop** (Python, GitLab) | Mixed-vendor GPU/NPU telemetry, 50+ AI framework detection | Linux-first; Apple GPU inventory only; no agent sessions |
| **macmon / mactop** | Sudoless Apple GPU/ANE/power/temps (IOReport) | No attribution to processes/agents |
| **gpu-mcp-server** | GPU metrics over MCP | NVIDIA only; no memory/headroom |
| **amux** | Headroom check before admitting its own workers | Only for its own workers |
| htop / btop / Activity Monitor | General process view | RSS lies on Apple Silicon; no groups, sandboxes, models, headroom, forecast |

**Positioning:** oomtop is the *resource attribution + headroom* layer. It does not compete on token/cost tracking.

## 5. Concepts & data model

Every measured value is `Measured<T> { value, source, quality }` where `quality = exact | estimate |
unavailable(reason)`. The UI and every export show quality; nothing unavailable is rendered as zero.

```
Host ─┬─ Memory (total, available, free, cached, wired, compressed, swap used/total, pressure level, PSI,
      │          own_cgroup_limit?, swap_limit? (macOS: how far swap can grow))
      ├─ Oom (killer: kernel|systemd_oomd|earlyoom|jetsam, thresholds, forecast{target, eta_s, confidence},
      │       likely_victim?, recent_kills[])
      ├─ Accelerators[] (vendor, util %, mem used/total, gpu_budget (Metal working-set limit), power W, temp °C,
      │                  clock, throttle reasons)
      ├─ Thermal (pressure: nominal|moderate|heavy|trapping|sleeping, throttle_factor 0..1 | n/a,
      │           low_power_mode, on_battery, battery %)
      ├─ Processes[] ─ id=(pid, start_time), ppid, responsible_pid, exe, cmdline (redacted), cwd, user, cpu %,
      │                mem{resident, footprint_or_pss, gpu, compressed, swapped, non_resident_est},
      │                disk io, state, idle_for, markers{session_id?, agent?}
      ├─ Groups[]    ─ id, kind, label, fingerprint, root, members[], totals, reclaim_gain, confidence,
      │                owner_group, orphan, idle
      ├─ ModelServers[] ─ kind, endpoint, models[{name, file, weights_bytes, kv_bytes, device}], tok_s|s_per_step, queue
      └─ Sandboxes[] ─ kind, id, label, host_pids[], configured_mem, guest_mem?, limits, started_by_group
```

**Group kinds** (short alias used in queries, themes and rules in brackets): `agent_session` [agent] (Claude Code,
Codex, Cursor, Aider, Gemini, Goose…), `app` [app], `model_server` [model] (Ollama, llama.cpp, sd.cpp, vLLM,
LM Studio, MLX), `sandbox` [sandbox] (container/VM/microVM/seatbelt), `build_daemon` [daemon] (Gradle, Kotlin,
Bazel, sccache, tsserver…), `system` [system], `other` [other].

**Memory taxonomy (the headline feature).** Verified on this Mac 2026-09-29 where marked ✓.

| Field | Linux source | macOS source (unprivileged) |
|---|---|---|
| `resident` | `/proc/<pid>/statm` | `ri_resident_size` from `proc_pid_rusage` |
| `footprint_or_pss` | `Pss` from `/proc/<pid>/smaps_rollup` (rotating, §6.2); between reads `RSS − Shared` as *estimate* | `ri_phys_footprint` from `proc_pid_rusage` — includes Metal/IOAccelerator and compressed; same-user processes only; ✓ matches `top` MEM within 0.2 % |
| `gpu` | NVML per-process used memory; `/proc/<pid>/fdinfo` `drm-memory-*` / `drm-resident-*` (amdgpu, i915, xe) | inside footprint; per-process split is a spike (§19 Q3); host-level GPU memory in use from IOAccelerator |
| `compressed` | host-level only (zswap/zram) | host-level from `vm_statistics64`; per-process needs a task port (✓ `top` is setuid root + `com.apple.system-task-ports.read`) → `unavailable(needs root)` unless run with sudo |
| `swapped` | `SwapPss` from smaps_rollup | not attributable per process (the compressor swaps segments, not a process's pages); host-level only |
| `non_resident_est` | — | `max(0, footprint − resident)` labeled "compressed or swapped (est.)" |

Group totals use `footprint_or_pss` (PSS never double-counts shared pages; footprint excludes shared file-backed
pages and matches Activity Monitor).

**Known under-count:** Virtualization.framework VMs. ✓ The Claude desktop VM showed footprint 1.5 GB while `top`
reported 2.7 GB compressed for it. Sandbox groups therefore also show the VM's *configured* memory and mark the
footprint as a lower bound until §19 Q2 is resolved.

## 6. Architecture

Single binary, **Rust** workspace:

```
oomtop/
  crates/
    oomtop-core/        data model, units, Measured<T>, attribution matching, headroom, forecast, throttle, query,
                        ranking — pure, no I/O, fully unit-tested
    oomtop-collect/     Sources (I/O) + decoders (pure) — one per OS data source
      linux/            procfs, smaps_rollup, cgroup v2, PSI, vmstat, hwmon, powercap(RAPL), cpufreq, NVML (dlopen),
                        drm fdinfo, systemd-oomd/earlyoom config
      macos/            libproc (proc_pid_rusage, proc_pidinfo), sysctl (KERN_PROCARGS2, memorystatus, iogpu),
                        vm_statistics64, IOReport (dlsym), IOKit power sources, thermal-pressure notify key,
                        responsible pid, JetsamEvent reports
    oomtop-detect/      detection rule set (built-in + user TOML) compiled for oomtop-core's matcher
    oomtop-adapters/    local model-server & sandbox APIs (Ollama, llama.cpp, sd.cpp, vLLM, LM Studio, Docker/Podman,
                        Firecracker) — localhost only, 200 ms timeouts
    oomtop-config/      layered config, rules/themes/keymaps loading, file watching, JSON Schema, toml_edit writes
    oomtop-state/       SQLite (WAL): lineage journal, profile/personalization store, impression log
    oomtop-tui/         ratatui frontend
    oomtop-mcp/         MCP server (stdio)
    oomtop-serve/       HTTP JSON + Prometheus /metrics
    oomtop-cli/         main binary `oomtop`: subcommands, wiring
```

Dependency direction: `core` ← `collect`, `detect`, `adapters`, `config`, `state` ← frontends (`tui`, `mcp`,
`serve`) ← `cli`. Frontends never touch OS APIs directly.

### 6.1 Collectors = Source + decode
```rust
trait Source   { fn read(&mut self, budget: Duration) -> Result<RawSample, SourceError>; }  // I/O only
fn decode(raw: &RawSample, prev: Option<&RawSample>) -> Partial<Snapshot>;              // pure
```
`RawSample` is serde-serializable (file contents on Linux; syscall result structs on macOS). The fixture tool
records `RawSample`s; tests replay them through `decode` on any OS. Each Source reports `available |
partial(reason) | unavailable(reason)`, honors a ≤ 50 ms budget and backs off when slow or failing; missing or
changed OS data yields `unavailable`, never a panic.

### 6.2 Sampling model
- **Tiers:** host stats every 1 s; per-process basics every 2 s; expensive reads every 5–10 s or on demand.
- **smaps_rollup rotation (Linux):** reading it walks page tables and takes the mmap lock, so read it for the top 64
  processes by RSS every 5 s and round-robin the rest within 60 s; values between reads are marked *estimate*.
- **First frame:** memory values from the first sample (< 300 ms); CPU % needs a delta, so a second sample is taken
  at +500 ms.
- **No daemon.** The TUI and `serve` sample continuously while running. `oomtop mcp` samples **on each call**
  (two samples 150 ms apart, or one when the previous call is ≤ 30 s old), so four agents with four MCP servers cost nothing when idle.
- **Lineage journal** (`oomtop-state`): whenever oomtop samples, it records `(pid, start_time) → ppid, group,
  markers, last_active`. It lets any later instance know that a daemon now parented to `launchd`/`init` was spawned
  by an agent session, and for how long it has been idle. Without journal data, `idle_for` is "≥ observed window".
- **History:** in-memory ring buffer of the last 10 min, compact (per group full resolution; per process u32 KiB
  footprint + u16 CPU permille for the top 200), for sparklines, trends and the forecast.
- **Output schema** (`json`, `ndjson`, `serve`, MCP) is versioned (`schema_version: 1`); bytes are integers;
  every measured field carries `source` and `quality`.

## 7. Attribution

Signals, strongest first:
1. **Adapter truth** — sandbox runtime / model-server APIs map their own pids.
2. **cgroup path** (Linux) — docker/podman/systemd scopes, `user@.service/app-*.scope`.
3. **Responsible process** (macOS) — helpers/XPC services map to the app that launched them (e.g. the
   `Virtualization.VirtualMachine` XPC → Claude.app).
4. **Session markers** — an allowlist of environment keys read from the process's own environment (Linux
   `/proc/<pid>/environ`, macOS `sysctl KERN_PROCARGS2`, same-user only). ✓ Claude Code children carry
   `CLAUDECODE=1` and `CLAUDE_CODE_SESSION_ID`, which groups a session's processes even after re-parenting.
   The same environment also holds `CLAUDE_CODE_MESSAGING_TOKEN` — hence **allowlist only**: non-listed keys are
   skipped without being copied, and marker values are never stored or exported (session ids are hashed).
5. **Ancestry** — ppid chain to the nearest known group root (agent CLI, app bundle, terminal), plus the lineage
   journal for processes whose parent has exited.
6. **Rules & heuristics** — cwd under a session's project, command-line patterns, well-known daemons (GradleDaemon,
   KotlinCompileDaemon, tsserver, sccache, language servers).

Each membership carries a **confidence** (high/medium/low) shown in the UI. Process identity is `(pid, start_time)`
everywhere, so pid reuse never merges two processes. oomtop's own processes form an `oomtop` group; an
`oomtop mcp` child stays inside its agent session but is excluded from that session's reclaim candidates.

**Orphans:** a process whose group root has exited, that was spawned by an agent session or tool (lineage journal or
session marker), and that is older than 10 min (configurable). **Idle:** CPU < 0.5 % of one core and no disk I/O
for ≥ 30 min (configurable). **Idle cost** = reclaim gain of idle members (§8.1); the Reclaim view sorts by it.

**Actions target group roots.** Helpers of an app (renderers, XPC services, the Claude desktop VM) are never offered
individually; the action is "quit <app>". Build daemons and orphans are their own roots.

## 8. Headroom & OOM forecast

### 8.1 Headroom
```
available_now  Linux: MemAvailable, capped by (memory.max − memory.current) of oomtop's own cgroup when set
               macOS: (free − speculative) + external/file-backed (⊇ speculative) + purgeable pages from
                      vm_statistics64 — speculative pages are counted once (free_count includes them and so
                      does external_page_count) — cross-checked against kern.memorystatus_level (% available);
                      anonymous inactive pages are NOT counted — reclaiming them means compression
safety_margin  max(1.5 GB, 8 % of RAM); +50 % when pressure ≥ warn or swap is growing; configurable
headroom       available_now − safety_margin
reclaim_gain   per group: RAM freed if stopped = private resident (+ compressed share when known);
               swap freed is reported separately; always quality=estimate
reclaimable    Σ reclaim_gain of groups oomtop may suggest (build daemons, orphans, idle model servers)
```
After a reclaim, oomtop re-measures and reports the actual gain next to the estimate ("freed 5.6 GB, est. 5.9").

**GPU budget.** On Apple Silicon, Metal allocations are capped by the device's `recommendedMaxWorkingSetSize`
(overridable via `sysctl iogpu.wired_limit_mb`; ✓ readable, `0` = OS default). A GPU-resident model must fit
**both** host headroom and the GPU budget minus GPU memory in use. On discrete GPUs, check VRAM free per device and
host RAM separately.

### 8.2 can_fit
`can_fit(need)` → `yes | yes_after_reclaim(list, gain) | no(shortfall)` with `as_of` and `valid_for_s = 10`.
It is **advisory**: two agents can both get `yes` and both load (see §19 Q1).
CLI exit codes: `0` yes · `3` yes after reclaim · `4` no · `1` error · `2` usage.

**Model-load estimate** (read only the GGUF header / safetensors index, never the weights):
```
need     = weights + kv + overhead
weights  = Σ tensor file sizes (files are already quantized — no quant multiplier)
kv       = 2 × n_layers × n_kv_heads × head_dim × ctx × bytes(kv_type) × n_parallel
overhead = max(512 MB, 5 % of weights)          # compute buffers
```
Diffusion (sd.cpp): Σ all component files (diffusion model, text encoders, VAE) + activation estimate by
resolution from a table calibrated on the qwen-image-studio fixture. Without metadata: 1.2 × file size, `estimate`.

### 8.3 OOM forecast
- **Which killer applies:** Linux — kernel OOM killer always; `systemd-oomd` when active (reads its config and unit
  `ManagedOOM*` settings; defaults: swap used > 90 %, memory pressure > 60 % for 30 s); `earlyoom` when running
  (thresholds from argv). macOS — jetsam (memorystatus) and the "out of application memory" state when compressor +
  swap are exhausted.
- **Forecast:** linear fit of the last 2–5 min of available memory, swap used and PSI against the nearest applicable
  threshold. Shown only when the trend is consistent (R² ≥ 0.6) and ETA < 30 min; otherwise "stable". Never an
  alarm on a single spike.
- **Likely victim:** Linux — highest `/proc/<pid>/oom_score` (readable unprivileged). macOS — jetsam bands are not
  readable without root, so: largest footprint among background apps, labeled as a heuristic.
- **Recent kills:** Linux — cgroup `memory.events` `oom_kill` counters and the oomd journal where readable; macOS —
  `JetsamEvent-*.ips` in `/Library/Logs/DiagnosticReports` (✓ readable by admin users via `_analyticsusers`).

## 9. Throttling & "why is it slow"

- **macOS:** thermal pressure via the public notify key `com.apple.system.thermalpressurelevel`
  (`OSThermalNotification.h`); **Low Power Mode** (`NSProcessInfo.isLowPowerModeEnabled`) as an explicit cause;
  battery and adapter wattage via IOKit power sources; CPU/GPU residency, frequency and package power from IOReport;
  memory pressure level. `pmset -g therm` is not used (reports nothing on Apple Silicon).
- **Linux:** `cpufreq` current vs max, thermal zones/trip points, hwmon temps, RAPL power (often root-only via
  powercap permissions — degrade), NVML clock throttle reasons, amdgpu `gpu_metrics`, PSI `/proc/pressure/*`.
- **Throttle factor** (0..1) = observed ÷ max frequency per cluster/GPU, computed **only while that unit is busy**
  (active residency ≥ 50 %); otherwise `n/a (idle)`. Blended by utilization; shown as "running at 45 % speed".
- **Swap storm:** swap-ins/outs per minute (`vm_statistics64` swapins/swapouts; Linux `/proc/vmstat`
  `pswpin/pswpout`).
- `oomtop why` ranks likely causes with evidence lines ("swap in +600 MB/min for 5 min", "GPU at 42 % of max clock
  with thermal pressure *heavy*", "Low Power Mode on", "on battery at 29 %").

## 10. Model-server adapters

Adapters probe `127.0.0.1` only, only when a matching process exists (no port scanning), with 200 ms timeouts.

| Server | Discovery | Data | Gentle action |
|---|---|---|---|
| Ollama | process + `:11434` | `/api/ps` (model, size, size_vram), `/api/tags` | unload model (`keep_alive: 0`) without stopping the server |
| llama.cpp server | process + `--port` | `/metrics`, `/slots`, `/props` | SIGTERM |
| stable-diffusion.cpp (`sd-server`) | process + `--listen-port` | `/sdcpp/v1/capabilities`, job/queue endpoints, weight files from argv | SIGTERM (refuse while a job runs unless confirmed) |
| vLLM | process + port | Prometheus `/metrics` (KV cache usage, running/waiting requests, tok/s) | SIGTERM |
| LM Studio | app + local API | `/api/v0/models` (loaded state) | `lms unload` |
| MLX / MLX-LM | process patterns (`mlx_lm.server`) | process-level only | SIGTERM |
| Generic | any process mapping `*.gguf/*.safetensors` (open files / `/proc/<pid>/maps`) | weight files + sizes | SIGTERM |

**Model files on disk:** index `~/.cache/huggingface`, `~/.ollama/models`, the LM Studio models dir and user-added
folders; show size, last-used, duplicates (same size + hash of first/last MiB), and which server has it loaded.

## 11. Sandboxes / VMs / containers

| Kind | Linux | macOS |
|---|---|---|
| Containers | Docker/Podman API (stats), cgroup v2 accounting | Docker Desktop / OrbStack / Colima: host cost = their VM process; per-container via Docker API |
| VMs | libvirt/QEMU processes, Firecracker socket API, Cloud Hypervisor | Virtualization.framework XPC processes (via responsible pid), UTM/Parallels |
| Process sandboxes | bubblewrap, firejail, nsjail (namespaces + markers), gVisor `runsc` | Seatbelt (`sandbox-exec`) markers |

Host-side cost is always shown; VM groups show configured memory next to footprint (§5 under-count). App-owned VMs
(Claude desktop, Docker Desktop) belong to their app's group and are reclaimed by quitting the app.

## 12. Interfaces

Full UX — adaptive modes, personalization, layouts, keys, visual system, themes, settings — is in
**[UX.md](UX.md)**. Summary:

### 12.1 TUI (`oomtop`)
htop-style meter block (per-core CPU bars tagged P/E, `Mem[…]` segmented by oomtop's memory truth, `Swp[…]`,
`GPU[…]`; Tasks / Load average / Uptime on the right), then the headline sentence, true-memory legend (apps · GPU/Metal · compressed · wired · free), swap trend + forecast,
pressure, thermal/throttle badge, accelerator mini-bars. Views: `1` Home (ranked groups) · `2` Processes · `3` Models ·
`4` Sandboxes · `5` Reclaim · `6` Timeline. Keys per UX §7 (`x` stop, `z` suspend, `/` filter, `?` help), plus an htop-style F1–F10 bar on the last line in every preset.

### 12.2 CLI
```
oomtop                                   # TUI
oomtop headroom [--need 13G | --model path.gguf] [--json]    # exit 0 yes · 3 after reclaim · 4 no
oomtop why [--json]                      # ranked causes of slowness / pressure
oomtop reclaim [--dry-run] [--yes]       # idle/orphan groups; stops only with confirmation (or --yes)
oomtop json | oomtop ndjson [--interval 2s]
oomtop serve [--listen 127.0.0.1:9469]   # HTTP JSON + Prometheus /metrics
oomtop mcp [--allow-actions]             # MCP server over stdio
oomtop doctor                            # detected capabilities, unavailable sources and why
oomtop config … | theme … | keys … | profile …   # see UX §12.9, §6
```
Prometheus: host series plus per-group series labeled by `kind` and `label`; no per-pid series by default
(cardinality).

### 12.3 MCP tools
Read-only by default, annotated `readOnlyHint`; structured results with an `outputSchema`.
- `get_headroom()` → available, safety margin, GPU budget, pressure, swap trend, OOM forecast
- `can_fit({bytes? , model_path?})` → yes / yes_after_reclaim(list, gain) / no(shortfall)
- `top_consumers({by: "footprint"|"gpu"|"cpu", n})`
- `list_groups({kind?})`, `list_model_servers()`, `list_sandboxes()`
- `explain_slowdown()` → ranked causes with evidence
- `suggest_reclaim()` → candidates with estimated gain; **no side effects**
- `reclaim({group_ids})` — `destructiveHint`; registered **only** with `--allow-actions`. oomtop itself asks the
  user through MCP **elicitation** (exact list, gains, SIGTERM) and acts only on an explicit accept. If the client
  doesn't support elicitation, the tool performs nothing and returns the `oomtop reclaim` command for the user.
  Never targets the calling agent's own session.

## 13. Safety, privacy, permissions

- No root by default; features needing privileges show *unavailable (needs X)*. Running with `sudo` is supported for
  extra detail (per-process compressed on macOS, other users' processes); it doesn't widen actions (below).
- **Actions:** confirmation always; group roots only (§7); adapter-graceful action first (§10); then SIGTERM;
  SIGKILL only on an explicit second confirmation. `(pid, start_time)` is re-verified immediately before any signal.
- **Protected:** kernel threads, pid 1/launchd/systemd, WindowServer, loginwindow, the terminal and shell running
  oomtop, the agent session hosting an MCP call, anything owned by another user, plus a user list in config.
- **Suspend (`z`, SIGSTOP):** offered as a CPU/GPU/thermal relief only — it frees no memory, so it never appears in
  Reclaim or `can_fit` plans.
- **Privacy:** command lines redacted in exports (`--token=…`, `KEY=…`, URLs with credentials); environments are
  read only for allowlisted marker keys and never stored or exported; fingerprints hash placeholder-ized templates.
- `serve` binds 127.0.0.1 unless `--listen` says otherwise; MCP runs over stdio.
- No telemetry, no network calls except to local adapter endpoints.

## 14. Performance budget

- ≤ 1 % of one core at the default refresh (2 s) with ~700 processes; ≤ 40 MB resident incl. SQLite and history.
- Per Source ≤ 50 ms per sample (hard cap); UX layer ≤ 5 ms per frame.
- First frame < 300 ms (memory); CPU % within 1 s.
- `oomtop mcp` idle cost: zero CPU between calls; a call answers in < 400 ms.
- Measured by a bench on this Mac with thermal pressure and battery state recorded alongside the numbers —
  the whole process over ≥ 11 min (`tools/groundtruth/soak.py`, headless and TUI), not a pipeline microbench.
  The process listing may use 4× the per-source budget (§21). Latest numbers and status: §21.

## 15. Configuration

Full design — layered precedence, themes/tokens, layouts, keymaps, in-app settings, `oomtop config|theme` CLI,
terminal compatibility — is in **[UX.md §12](UX.md#12-appearance-themes--settings--the-terminal-way)**. Summary:

`~/.config/oomtop/config.toml` — refresh rates, idle/orphan thresholds, safety margin, protected names, model folders,
adapter endpoints, marker-key allowlist. `~/.config/oomtop/rules.d/*.toml` — user detection rules:
```toml
[[group]]
kind = "agent_session"
label = "Claude Code"
match.exe = ["claude"]                              # native binary
match.script = ["**/@anthropic-ai/claude-code/**"]  # node-based installs (exe = node)
match.env = ["CLAUDECODE"]                          # marker key must be in the allowlist
session_key = "env:CLAUDE_CODE_SESSION_ID"          # one group per session (value is hashed)
```

## 16. Packaging

- **Linux:** primary artifact is glibc-linked against an old baseline (glibc 2.28, built with `cargo-zigbuild`) so
  NVML can be `dlopen`ed; a fully static **musl** variant is also published without NVML (musl static binaries
  can't load glibc shared libraries). x86_64 + aarch64.
- **macOS:** arm64 + x86_64 universal, signed + notarized. IOReport symbols are resolved at runtime (`dlsym`); if they
  disappear in a future macOS, GPU/power degrade to `unavailable` and everything else still works. The macOS binary
  uses jemalloc (statically linked, BSD-2-Clause — its notice ships with release archives) so RSS stays within §14
  (§21); Linux uses the system allocator.
- Channels: GitHub Releases + install script (checksum verified), Homebrew (tap → homebrew-core), `cargo install
  oomtop-cli` (the crate is `oomtop-cli`; the binary is `oomtop`), `.deb`/`.rpm`; AUR/Nix later. Names on GitHub, crates.io, npm and PyPI are reserved by the owner.

## 17. Testing

- **Fixture tests:** recorded `RawSample`s (Linux `/proc` subsets, smaps_rollup, cgroup files, NVML/drm fdinfo;
  macOS `proc_pid_rusage`/sysctl/vm_statistics64/IOReport results) replayed through decoders — no hardware in CI.
  Captured by `tools/capture` with secrets redacted.
- **Golden tests** (insta) for attribution, headroom, can_fit, forecast and `why` decisions.
- **Ground-truth checks** (`tools/groundtruth`, run on real machines, not CI): compare against `top -l 1 -stats
  pid,mem,cmprs,command`, `vm_stat`, `sysctl vm.swapusage`, `memory_pressure` (macOS) and smaps/`free` (Linux);
  target delta ≤ 5 %.
- **CI matrix:** ubuntu-latest (x86_64), ubuntu-24.04-arm, macos-latest (arm64); nightly perf check against §14.
- **Acceptance scenario (from the motivating session):**
  1. A Metal-backed model server holding ~10 GB shows ≈10 GB in its group (not its RSS).
  2. Idle Gradle + Kotlin daemons (~3 GB footprint each) appear as `build_daemon`, idle, top of Reclaim, with a gain
     estimate within 15 % of the measured gain after stopping them.
  3. `com.apple.Virtualization.VirtualMachine` is attributed to the app that launched it and marked as a lower bound.
  4. Four agent sessions appear as four groups (by session marker) with their child processes nested — including
     children re-parented to launchd.
  5. `oomtop why` during a thermally throttled run reports throttling and swap as causes, and reports nothing
     throttle-related while idle.
  6. `oomtop headroom --need 13G` gives the correct yes / yes-after-reclaim / no answer and exit code.
  7. With swap growing steadily, the forecast appears with an ETA within ±30 % of actual, and never on a single spike.

## 18. Milestones

| M | Scope | Exit criteria |
|---|---|---|
| M0 | Workspace skeleton; CI; Source/decode harness; `tools/capture` + first fixture of this Mac | `cargo build`/`test` green on Linux + macOS in CI; `oomtop --version` |
| M1 | True memory + host header + Processes view; config layering; semantic tokens + `terminal`/`none` themes; capability detection; `doctor`; `json`; spike: per-process Metal split (deferred, §21) | acceptance #1; footprint/PSS within 5 % of ground truth |
| M2 | Attribution (markers, ancestry, lineage journal, rules) + Home/groups view + idle/orphans + Reclaim (dry-run, confirm) + headroom/`can_fit` (bytes) + OOM forecast; entity fingerprints | acceptance #2, #3, #4, #6 (bytes), #7 |
| M3 | MCP (read-only tools, then elicitation-gated `reclaim`) + `serve` (JSON/Prometheus) | an agent calls `can_fit` end to end on this Mac |
| M4 | Accelerators + thermal/throttle + Low Power Mode + `why`; situation modes | acceptance #5 on Apple Silicon + an NVIDIA box |
| M5 | Model-server adapters + gentle actions + model-aware `can_fit` + model files index | acceptance #6 with `--model` |
| M6 | Sandboxes (Docker/Podman/OrbStack, Virtualization.framework, Firecracker) | container & VM groups with host cost |
| M7 | Personalization: profile store, frecency, query understanding, ranker, headline/why, local UX metrics | UX §11 tests 1–7, 13 |
| M8 | Theme library, base16/terminal-theme import, layouts/columns/keymaps, settings screen with comment-preserving save | UX §11 tests 8–12 |
| M9 | Packaging, docs, 1.0 | brew/cargo/deb installs; signed + notarized macOS binary |

## 19. Open questions

1. **Cross-agent reservations** — `can_fit` is advisory; a `reserve({bytes, ttl})` lease shared via `oomtop-state`
   would stop two agents loading at once. Recommended: experiment after M3.
2. **VM memory on macOS** — footprint under-counts Virtualization.framework guests (§5). Spike in M6: configured
   size vs host compressor deltas vs sudo task ports.
3. **Per-process Metal split** — can IOAccelerator user clients in the IOKit registry attribute GPU memory per pid
   without root? Spike deferred past M1 (§21); until then `gpu` is folded into footprint on macOS.
4. **agtop NDJSON import** — recommended: not before 1.0.
5. **Guest agent** for per-process detail inside VMs — recommended: no for v1.

Resolved: **license = MIT** (owner decision 2026-09-29); drm fdinfo kernel minimum (detect keys at runtime); IOReport (ship it, resolved via `dlsym`,
degrades cleanly); suspend (CPU/thermal relief only, never counted as reclaim).

## 20. Research sources

- agtop — https://github.com/mbrassey/agtop
- agentop — https://www.npmjs.com/package/agentop (repo: https://github.com/ktamas77/agentop)
- aitop (GitLab) — https://gitlab.com/CochainComplex/aitop
- macmon — https://github.com/Byron/macmon · mactop — https://github.com/metaspartan/mactop
- gpu-mcp-server — https://github.com/pmady/gpu-mcp-server
- amux headroom admission — https://amux.io/guides/ai-agent-server-sizing/
- Agent sandboxing landscape — https://northflank.com/blog/how-to-sandbox-ai-agents
- MCP elicitation — https://modelcontextprotocol.io/specification/2025-06-18/client/elicitation
- MCP tool annotations — https://modelcontextprotocol.io/specification/2025-06-18/server/tools
- Metal `recommendedMaxWorkingSetSize` — https://developer.apple.com/documentation/metal/mtldevice/recommendedmaxworkingsetsize
- Thermal state — https://developer.apple.com/documentation/foundation/processinfo/thermalstate
- systemd-oomd defaults — https://www.freedesktop.org/software/systemd/man/latest/oomd.conf.html
- DRM client usage stats (fdinfo) — https://docs.kernel.org/gpu/drm-usage-stats.html
- smaps_rollup / PSS — https://docs.kernel.org/filesystems/proc.html · PSI — https://docs.kernel.org/accounting/psi.html
- Ollama `keep_alive` unload — https://github.com/ollama/ollama/blob/main/docs/api.md
- Local verification on this Mac (2026-09-29): `top -l 1 -stats pid,mem,cmprs,command`; `proc_pid_rusage` via
  ctypes; `codesign -d --entitlements - /usr/bin/top`; `sysctl iogpu.wired_limit_mb kern.memorystatus_level`;
  child env of a Claude Code session.
- Name checks: crates.io, registry.npmjs.org, pypi.org, formulae.brew.sh, sources.debian.org, GitHub search
  (`oomtop`: free everywhere, 1 unrelated repo; `rtop`, `gtop`, `agtop`, `aitop`, `agentop`, `psitop`, `swaptop` taken)

## 21. Implementation decisions (v0.1.0 build, 2026-09-30)

What the first full build of M0–M8 decided where this spec was silent or had to bend, grouped by section. Each item
names the reason; the code is the reference for details.

**§5 / §6 collectors**
- macOS extras (thermal pressure, Low Power Mode, power sources, IOAccelerator GPU memory/utilization, IOReport
  residency/energy with the `pmgr` DVFS tables, JetsamEvent reports) travel in `MacHostRaw`'s existing
  `sysctl`/`sysctl_str` maps under namespaced keys (`notify.*`, `iops.*`, `ioaccel.*`, `ioreport.*`, `dvfs.*`,
  `jetsam.*`, `unavailable.<part>`), so older fixtures replay as `unavailable`. The pure decoder lives in
  `decode/macos_extras.rs` and runs on every OS; `decode()` applies it to every `MacHost` sample.
- Linux enrichment (cgroup limits, OOM killers, thermal zones/hwmon/RAPL/battery, NVML/amdgpu/i915 accelerators,
  per-process GPU from DRM fdinfo, disk I/O, model files) is applied by `decode()` for every `LinuxFiles` sample.
  `gpu_metrics` v2.x activity is read as centi-% and socket power as mW (header semantics; hwmon preferred when
  present); v3.0 temperature is centi-°C.
- The IOReport subscription is created on a helper thread and takes its baseline sample there, so the first
  read after it is ready already has a delta (one-shot commands sample twice, 150 ms apart). The subscription's
  out-dictionary is intentionally never released (ownership undocumented; a one-time leak beats an over-release).
- macOS process listing: one `sysctl(KERN_PROC_ALL)` per sample provides identity for other users' processes
  (PROC_PIDTBSDINFO is refused for them); `proc_pid_rusage`/`PROC_PIDTASKINFO` are not retried for a process
  once refused with EPERM for its uid, and the responsible pid is cached per `(pid, start_time)`. This halved the
  collector's cost on a two-user Mac (§14).
- `iogpu.wired_limit_mb = 0` means "OS default": the GPU budget is then an `estimate` of 2/3 of RAM up to 36 GiB,
  3/4 above. `hw.perflevel*` and `iogpu.wired_limit_mb` are optional (Apple Silicon only) and never degrade
  `macos.host` when absent.
- The fanless model table is hard-coded (`Mac17,3` verified; older Air ids from Apple's lists); unknown models
  report `fanless = null`, not `false`.

**§7 attribution**
- Order actually implemented: adapter truth → rule root (merging into a same-rule ancestor or the session group) →
  session marker (before responsible pid) → responsible pid → ancestry with launcher boundaries (terminals,
  interactive shells, transparent launchers start new groups) → lineage (re-parented only) → heuristic root.
- Adapter truth needs attributed groups to probe, so the engine carries the previous probe's pid mapping into the
  next attribution and re-attributes once when a freshly probed server/sandbox pid sits in a group of another
  kind (first sample, or a server that just appeared).
- A live-session marker does not override ancestry when the parent is in a rule, adapter or sandbox group (not an
  agent/system/self group): such workers belong to their daemon, so stopping the daemon frees them.
- Label-keyed app groups are per user: another non-root user's copy of an app (fast user switching) is
  `app:<slug>@u<uid>`, labeled "<App> (uid N)", and protected. Chrome's updater runs the browser from a temporary
  `…/code_sign_clone/…/Google Chrome.app.bundle/…` copy; `.app.bundle/` counts as a bundle and `argv[0]` is the
  fallback when the executable path is not a bundle.
- Orphan roots are keyed by pid with `owner_group` set; App/System rule roots use label-only keys; session ids
  that are not hash-shaped are hashed before taking 12 characters.
- Idle: a process that never had a measurable CPU sample is `unavailable("no CPU data")`, not "≥ observed
  window" — otherwise other users' processes (no CPU data without root) would all look idle. The first frame
  shows idle as unknown until the second sample.

**§8 headroom / forecast**
- Busy build daemons (≥ 10 % CPU and not idle) are not offered for reclaim. Discrete GPUs keep a VRAM margin of
  `max(256 MiB, 5 %)`. An exact GPU budget of 0 counts as not set.
- Forecast: earliest ETA across 2–5 min windows with a step/halves guard against single spikes; PSI `some`
  (not `full`) for the memory-pressure series; systemd-oomd's swap rule is "swap used ≥ limit **and** available
  ≤ (100 − limit) % of RAM" when RAM size is known (`forecast_oom_with`, used by the engine). A metric without a
  sample in the last 30 s is not forecast.
- macOS: jetsam hosts add swap files on demand, so `swap_total` is not a limit. The ceiling is
  `HostMemory.swap_limit` = swap used + free space on the swap volume (`statfs /System/Volumes/VM`, an
  `estimate`): jetsam kills when swap can't grow. The swap-exhaustion forecast runs against it; with ample disk
  (hundreds of GB on this Mac) the ETA is far beyond 30 min and the forecast stays "stable", which is the honest
  answer. Acceptance #7 has a macOS golden (`forecast_acceptance_7_steady_macos`, nearly full disk). The
  compressor-segment limit (`vm.compressor.segment.limit`, jetsam's "compressor space shortage") is not
  forecast yet: on this Mac it sits far above any reachable swap.
- The step guard rejects a single step in **either** direction larger than half of the fitted change, and a
  newest value far off the fitted line; the ETA is measured from the newest actual value. So a drop against the
  trend (an OOM kill, a stopped hog, a reclaim) clears the ETA on the next sample (golden
  `forecast_rise_then_drop`, replay test on `nvidia-oom-trend`).
- Model estimate: default context `min(model ctx, 8192)`; `block_count > 65536` is treated as corrupt metadata
  (1.2× fallback); GGUF string values decode lossily. The diffusion activation table has one calibration point
  (256² warm-up on the studio); larger resolutions extrapolate.

**§9 why**
- `oomtop why` takes a second sample when the first has no swap in/out rates (they need two samples; a replay's
  first frame has none), so a swap storm shows next to throttling.
- "Thermal throttling" requires thermal evidence (pressure ≥ moderate or a trip point); slow clocks without it
  are "Clocks reduced" (plug in / pause heavy work / check the power profile), and Low Power Mode alone is its own
  cause. Linux `cpuinfo_max_freq` includes single-core boost, so an all-core load can read slightly below 0.8.
- Round 3: an evidence line that only repeats its cause's title is dropped ("Clocks reduced — running at 35%
  speed" is no longer followed by "· running at 35% speed"; "Low Power Mode is on" not by "· Low Power Mode on").

**§10 adapters**
- llama.cpp tok/s is a counter delta between probes; vLLM weights come from the HF cache; `gentle_actions` only
  when the server's API answered; `stop_guard` answers `Unknown` when "idle" is only a CPU heuristic (a Metal/CUDA
  job can run with an idle CPU). The CLI reclaim skips busy servers unless named with `--groups`, and the TUI
  prompt says when a job runs, requests are queued, or idle is unconfirmed.
- sd-server exposes job state only per job id; without the ids from its parent the busy signal is heuristic.
- `oomtop models` and the TUI Models view ("On disk") show the model-file index: size, last use (atime), duplicate
  sets (same size + hash of the first and last MiB) and which server or process has each file loaded.

**§11 sandboxes**
- Seatbelt detection uses libSystem's `sandbox_check` (exported, not in the public headers — a private-API class
  like IOReport, see §19); it is isolated in `sandboxes::seatbelt_check`.
- Docker Desktop for Linux's own qemu VM currently shows as a separate "QEMU VM" group (needs a real install to
  find its exclude path).

**§12 interfaces**
- HTTP paths are `/api/snapshot`, `/api/headroom`, `/api/why`, `/metrics`, `/healthz` (the unprefixed paths stay
  as aliases). MCP `reclaim` statuses include `failed` (accepted, signals sent, none succeeded) and
  `nothing_to_do`.
- Units (round 2): every human sentence — CLI, TUI, MCP `summary`/`reason`, the headline — renders amounts with
  the configured `format.memory_units` (IEC by default, one decimal), so `can_fit {size: "13G"}` echoes
  "13.0 GiB" exactly like `oomtop headroom --need 13G` (it used to say "14 GB", SI, while the CLI mixed "GiB" with
  the core's short "4.5G"). The core stores its `reason` in IEC and exposes `can_fit::reason_with(answer, fmt)` so
  each frontend re-renders the sentence in its own units; `CanFitAnswer` gained `host_shortfall` and
  `reclaimable` (additive) for that, and `McpOptions` gained `units` (the CLI passes the config's).
- `oomtop --help` and the Cargo metadata carry no placeholder URL: `repository`/`homepage` are added when the
  GitHub repo exists (the `OWNER` placeholder stays only in the not-yet-live install script, Homebrew template
  and README install section, which say so).
- MCP `get_headroom`'s `summary` (round 3) is the UX headline — which reports *available now* ("All good — 4.8 GiB
  free.") — followed by the headroom `can_fit` and `oomtop headroom` decide with: "Headroom 2.9 GiB (4.8 GiB
  available minus 1.9 GiB safety margin)." An agent that reads only the summary no longer plans against memory
  the margin keeps back. Headroom notes stored by the core (memorystatus, own-cgroup cap) use IEC with one decimal
  like `CanFitAnswer::reason` ("13.9 GiB", not the table form "13.9G").
- `oomtop keys list --conflicts` prints `no conflicts (preset …, N bindings checked)` when clean.
- `oomtop doctor` lists expected-but-missing sources as `missing` (live runs only) and rule-file lint warnings.
  `linux.nvml` is not expected: it reports under its own name only where an NVIDIA driver exists, and
  `linux.gpu` already says "no NVML device" elsewhere (the first real-Linux run showed a `missing` row on a box
  without NVIDIA). The host line skips an empty model; Linux reads the model from DMI `product_name` or the
  device tree (`/sys/firmware/devicetree/base/model`), skipping firmware placeholders.
- MCP `reclaim` targets only what `oomtop reclaim` would offer (`reclaim_candidates` minus the caller's session):
  an active app or another agent's live session is refused with a reason, so the fallback
  `oomtop reclaim --groups …` command always works for what was accepted. Model servers with an adapter unload
  action are unloaded gently (no signal, SPEC §13); a model server whose job is running (`stop_guard` busy) is
  refused; "idle not confirmed" is said in the elicitation message and in `targets[].note`.
- MCP on-demand sampling reuses the previous call's sample as the CPU baseline when it is ≤ 30 s old (one
  sample per call, CPU averaged since that call); otherwise two samples 150 ms apart.
- The TUI's second sample (500 ms after start, and 600 ms after an action) forces every source
  (`SnapshotProvider::snapshot_now`), so per-process CPU shows within a second instead of at the next 2 s tick.

**§13 privacy / safety**
- Redaction also covers inline secret flags, ALL-CAPS/secret-named assignments, UUIDs and high-entropy words
  inside strings, and process names, group/sandbox labels, victim names, loaded-model names and job labels in
  exports — one view, `redact_snapshot`, for json, ndjson, serve and MCP. Secret flags are matched after
  lowercasing and dropping `-`/`_` by suffix (`--accessToken`, `--clientSecret`, `--passphrase`, `--apiKey` …),
  `user:password` after `-u`/`--user`/`--proxy-user` keeps the user name only, and `mysql -pSECRET` is redacted —
  both as separate argv elements and inside one shell-string argument (`zsh -c 'curl -u bob:pw …; mysql -p… db'`,
  verified live in `oomtop json`). Round 3: the MySQL rule runs before the secret-flag rule, so a password that
  contains a secret word (`-pSECRET`, `-pmytoken`) no longer reads as a secret *flag* that also takes the next
  word (the database name); a quote or backtick may precede `mysql` inside a string (`zsh -c 'mysql -p… db'` as
  one argument used to leak); and in an argv a `-p…` after a MySQL client word anywhere earlier in the same
  command (`sudo mariadb …`, `zsh -c mysql -pX db` split into words) is a password, until `;`/`&&`/`|`.
  Free-text secrets with no recognizable key, format or entropy cannot be detected; exports rely on the
  secret-flag list for those. `privacy.redact_exports = false` affects json/ndjson only (serve and MCP always
  redact).
- `plan_stop` refuses a group whose root process is missing from the sample or protected per
  `is_protected_process`; the CLI, TUI and MCP paths check the root process as well. The TUI offers SIGKILL only
  after a sample taken after the SIGTERM still shows the target.

**§14 performance (whole process, release build, M5 Air, 2026-09-30 05:16–05:50 IST, ~711 processes / 438
groups, AC power, battery 100 % charged, Low Power Mode off, thermal pressure nominal, load average 0.9–1.7 from
other agents' builds; `tools/groundtruth/soak.py`, exact child rusage + `ps` samples every 30 s)**
- Measured as the whole process (every thread: sampler, IOReport, adapters, render, JSON), over 11.5 min so the
  10-minute history ring is full — not the `pipeline_cost` example, whose collect + enrich number (~0.6 %)
  left out the rest of the process and was reported as "budget met" by mistake.
- Headless `ndjson --interval 2s --discard`: **CPU 1.10 % of one core** steady (audit before these fixes:
  1.3–1.9 %); **RSS plateau 30.6 MB** (before: 40.9 MB, growing ~1.1 MB/min until the ring filled).
- TUI in a 120×30 pty: **CPU 1.37 %** (1.12 % in a quieter 40 s window); **RSS plateau 45.4 MB** (before: 53.8 MB
  and still growing at 11 min); **physical footprint 19.8 MB**, flat. RSS counts ~25 MB of shared dyld-cache
  pages (AppKit/Foundation/IOKit text mapped by the TUI's frameworks) that no other process would free; by the
  project's own memory truth (footprint, SPEC §5) the TUI is at half the budget, by RSS it is over.
- Round-1 budget status (superseded below): CPU not met (≈1.1 % vs 1 %); RSS met headless, not met for the TUI.
- **Round 2 (2026-09-30 07:29–07:41 IST, release build, 740 processes / 464 groups, on battery 87→84 %, Low Power
  Mode off, thermal pressure nominal, load average 1.8–3.1 from other agents' builds; soak.py, 691 s):**
  TUI in a 120×30 pty **0.80 %** of one core steady (0.82 % whole run), **RSS plateau 44.4 MB**, footprint
  19.2 MB; headless `ndjson --interval 2s --discard` **0.66 %** steady (0.68 % whole run), **RSS plateau 31.5 MB**,
  footprint 13.3 MB. Same-load A/B against the round-1 binary (240 s, all four started together): TUI 0.95 → 0.83 %,
  headless 0.81 → 0.74 %, TUI RSS −1.3 to −2.7 MB.
- **Budget status (round 2): CPU met in this run for both (0.80 % TUI, 0.66 % headless; earlier runs of the same
  code under other loads read 0.7–0.9 %, and the round-1 gate read 1.02 % headless, so the margin is ~10–20 %, not
  large); RSS met headless (31.5 MB), not met for the TUI by RSS (44.4 MB vs 40), met by footprint (19.2 MB).**
  Why the TUI's RSS stays over: `footprint -w` shows ~10 MB of MALLOC_SMALL *reclaimable* pages (freed, kept by
  the allocator for reuse; counted in RSS, not in footprint) against ~4 MB headless — RSS tracks the heap's peak
  while a refresh builds a new snapshot and the TUI's rows next to the live ones, not what is in use (the live
  heap is flat at ~11 MB after the second sample; `heap` node counts at 2 s and 10 s: 89 184 vs 88 710). Tried
  and measured: `malloc_zone_pressure_relief` every 30 s (no RSS change: the pages are already reclaimable),
  mimalloc as the global allocator (worse: RSS 43.9 vs 42.6 MB, footprint 29 vs 17 MB at 240 s), watcher off (no
  change) — all reverted. Closing the gap needs either the owner's sign-off to state the §14 memory budget as
  footprint (the project's own memory truth, §5), or fewer transient allocations per refresh (the `Cow<'static,
  str>` source strings below, a model change).
- **Round 3 (2026-09-30 08:42–08:54 IST, release build, ~740 processes, on battery 67→65 %, Low Power Mode off,
  thermal pressure nominal, load average 1.6–2.2 incl. a Linux VM running the test suite; soak.py, 691 s):
  macOS binary on jemalloc** (`tikv-jemallocator`, `narenas:1,tcache:false,dirty_decay_ms:0,muzzy_decay_ms:0`,
  macOS only, `crates/oomtop-cli/src/main.rs`). TUI in a 120×30 pty **RSS plateau 30.3 MB** (max 33.1 MB),
  footprint 18.9 MB, **CPU 0.91 %** steady; headless **RSS 24.0 MB** (max 25.5 MB), footprint 13.9 MB,
  **CPU 0.76 %**. Same-time A/B soaks (690 s, all variants started together) against the round-2 binary:
  TUI RSS 45.0 → 28.8 MB, footprint 19.3 → 19.0 MB, CPU 0.93 → 0.91 % (first run) / 0.78 % vs 0.79 % with
  thread caches; headless RSS 30.0 → 23.0 MB, CPU 0.64 → 0.57 %. jemalloc's default 10 s decay (tried at 1 s:
  TUI RSS 35.4 MB, footprint 23.9 MB) keeps freed pages dirty; purging immediately returns them with
  `mmap(MAP_FIXED)` overlays, which macOS drops from RSS at once — libmalloc's `MADV_FREE_REUSABLE` pages leave the
  footprint but stay resident, which was the whole gap. Thread caches added ~1.5 MB and no CPU benefit. Linux keeps
  the system allocator (glibc returns freed pages differently; measured below). `_RJEM_MALLOC_CONF` in the
  environment overrides the options (test `macos_allocator_purges_freed_pages_immediately` pins them). The
  x86_64-apple-darwin cross build (jemalloc built by `cc` for the target) and the `lipo` universal binary were
  built and run (under Rosetta) on this Mac.
- **Budget status (round 3): met on this Mac for both by RSS and by footprint — TUI 30.3 MB, headless 24.0 MB
  (≤ 40 MB); CPU 0.91 % TUI / 0.76 % headless (≤ 1 %, margin ~10 % under a loaded machine).** The first frame
  and MCP numbers below are unchanged.
- Linux (Ubuntu 24.04 aarch64 guest, 2 vCPU / 2.8 GiB, ~100 processes, release build, soak.py 240 s): TUI RSS
  16.9 MB, 0.56 % of one core; headless RSS 12.3 MB, 0.56 %. A small VM, not a desktop's 700 processes.
- Round-2 changes: the TUI keeps a flag instead of the whole previous snapshot (only `groups`/`oom` are compared
  for insights, and the old snapshot's processes are freed before the new rows are built); timeline frames share
  interned `Arc<str>` ids/labels instead of a `String` pair per row per frame; the detector checks the membership
  signature on the raw inputs before `prepare` (a cache hit no longer clones the snapshot); IOReport is read every
  other refresh (`IOREPORT_REFRESH` 3.9 s: residency/energy are ~4 s averages; a reading without a delta is never
  re-used, so one-shot commands keep their second-sample delta).
- What changed: history stores group keys (FNV-1a u64) instead of a `String` per group per point and keeps the
  top 50 processes (was 200); the detector reuses the last attribution while the membership signature (process
  identities, parents, markers, adapter truth, lineage, protection) is unchanged and only refreshes totals and
  gains; rule matches, heuristic labels and fingerprints are memoized by the process's identity fields; the
  process source re-reads `proc_pid_rusage` of a process that used no CPU between its last two reads only every
  3rd refresh (CPU deltas use the real per-read interval, `MacRusage.read_ms`), thread counts every 5th, and the
  full `pbi_name` once per identity (again after an exec); the sampler hands its process list out instead of
  deep-copying it every refresh (re-derived from the raw samples on a partial run); lineage fields are assigned
  only when they change; O(n²) lookups in reclaim gain and lineage recording are gone.
- Tried and reverted: subscribing IOReport to only the 4 kept Energy Model channels measured **more** CPU
  (0.89 % vs 1.09 % in a same-time A/B), so the whole groups stay subscribed.
- What is left (profile, 60 s): IOReport sampling ~25 % of the main thread, kinfo/rusage syscalls ~15 %, decode +
  drop of ~700 `Process` values ~20 % — every `Measured` carries an owned `source: String` and an owned
  `Unavailable(String)`, so each process costs ~20 small allocations per refresh; making those `Cow<'static,
  str>` is the next lever but changes the frozen model (a breaking change, not done here).
- First frame on a terminal that never answers queries: **221–230 ms** (was ~2 s: crossterm's own Kitty query
  waited 2 s for DA1; the probe's answer now replaces it, the probe runs on a helper thread during the first
  sample, and its cap is 200 ms). Terminals that answer: 30–45 ms. Answers that arrive after the cap (slow SSH)
  are dropped by the TUI instead of read as keys (UX §14; pty test `pty_late_replies`, 0–1500 ms delays).
- MCP: a call ≤ 30 s after the previous one reuses its sample as the CPU baseline and answers in 7–15 ms; a
  cold call takes two samples 150 ms apart (~210 ms; was 283–323 ms). Idle between calls: 0 CPU.
- The process listing gets 4× the per-source budget (200 ms, `SamplerOptions`/`Sampler::run`), a documented
  exception to the 50 ms hard cap: one listing of ~700 processes takes 10–25 ms here, 50 ms is reached only
  on a loaded or throttled machine, and a truncated listing (`truncated = true`) would drop whole groups from
  the view. Host sources keep the 50 ms cap.
- Reliability fix found while measuring on battery: `Cf::owned` built its wrapper with `then_some(Cf(p))`, so a
  null "Copy" result was released — `CFRelease(NULL)` traps. On battery power
  `IOPSCopyExternalPowerAdapterDetails` returns null, so **every live command crashed with SIGTRAP when the
  Mac was unplugged**. Fixed with a regression test (`null_copy_results_are_none_and_never_released`); the
  collector tests now pass on battery.

**§17 / §18 status (what is verified, and how)**
- Verified by fixtures/goldens and on this Mac: acceptance #3, #4, #6, #7 (Linux and macOS goldens; replay
  test after an OOM kill), UX §11 tests 1–13 (test 2 end to end through the real ranker and "Your things";
  test 13 with a live-shaped macOS snapshot: jetsam killer, swap ceiling).
- Not yet verified live, and why: **#1** (sd-server's Metal footprint in its group) — the studio was not
  running during the build and is only started by its owner; covered by a synthetic snapshot until a
  `m5-air-studio` fixture is captured with `tools/capture/capture-macos.sh` while it runs. **#2** "gain within
  15 % of the measured gain after stopping" — stopping the Gradle/Kotlin daemons needs the owner's confirmation;
  the classification and "top of Reclaim" parts hold live. **#5** — reproduced only on synthetic Apple Silicon
  and NVIDIA fixtures; a hot-Air run with the studio's turbo edit and an NVIDIA box are still to do.
- **Real Linux (round 3):** the whole suite runs on Linux in a Lima guest on this Mac (`tools/linux-vm/`: Ubuntu
  24.04, `vz`, no host sudo): `cargo fmt --check`, `clippy --workspace --all-targets --locked -D warnings` and
  `cargo test --workspace --locked` are green for **aarch64-unknown-linux-gnu natively and
  x86_64-unknown-linux-gnu under Rosetta — 689 passed, 0 failed, 1 ignored on each** (macOS: 718 passed, 3
  ignored). The first run found two things the x86_64-only cross-lint could not: `c_char` is `u8` on aarch64
  Linux (clippy `unnecessary_cast` in the NVML name decoder — now `to_ne_bytes`), and the install-script test
  assumed the test binary's arch instead of `uname -m`. Live on the guest: `oomtop doctor` (every Linux source
  reports; no GPU/thermal/cpufreq/RAPL in a VM, each `unavailable` with its reason), `headroom`, `why`, `json`,
  `--plain`, and `tools/groundtruth/groundtruth.py`: host metrics within 0.3 % of `/proc/meminfo`/`free -b`, a
  400 MiB test process's PSS equal to `smaps_rollup` (−0.0 %). A release `aarch64-unknown-linux-gnu` archive
  with `SHA256SUMS`, installed through `packaging/install.sh` (`OOMTOP_BASE_URL=file://…`, `--gnu`), verified
  its checksum and ran. Not covered by the guest: real GPUs (NVML, amdgpu, i915), thermal zones/RAPL/cpufreq,
  cgroup memory limits under systemd-oomd pressure, and x86_64 *hardware* (Rosetta runs the binaries).
- M0's exit ("green **in CI** on Linux + macOS") and M9's (published brew/cargo/deb installs, signed + notarized
  binary, 1.0) still need the owner's GitHub repo, name reservations and signing secrets:
  `.github/workflows/{ci,release}.yml` have never run on GitHub. What CI would run now passes locally on macOS
  arm64 and both Linux arches (above). The `OWNER` placeholder is gone from `--help` and Cargo metadata; it stays
  only in the not-yet-live install script, Homebrew template and README install lines until the repo exists
  (the owner's GitHub name is unknown to the build and is not guessed).
- Acceptance #7 on macOS: shown by the nearly-full-disk golden only (live on Linux it is the kernel/oomd path the
  Linux goldens cover; the guest was not driven to OOM either). With ample free space on the VM volume the
  swap ceiling is hundreds of GB away and steady swap growth honestly gives no ETA; the compressor-segment limit
  (jetsam's "compressor space shortage") is not forecast. Showing #7 live here would mean filling this Mac's
  memory on purpose, which the build does not do on a shared machine.
- `rust-toolchain.toml` pins `channel = "stable"` rather than the MSRV: MSRV 1.88 (ratatui 0.30's floor) is
  declared in `Cargo.toml` `rust-version` and enforced by the CI `msrv` job, while contributors build with
  current stable (pinning 1.88 locally would make every build download and use an old compiler).

**§19 open questions touched**
- Q3 (per-process Metal split) is **deferred past M1** (not attempted): host GPU memory in use comes from
  IOAccelerator, and per-process Metal memory stays folded into `ri_phys_footprint` (which is what acceptance #1
  needs). Revisit with the studio running (IOAccelerator user clients per pid).
- `sandbox_check` joins IOReport as a runtime-resolved private-API dependency.

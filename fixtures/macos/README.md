# macOS fixtures

Recorded `RawSample` frames from real Macs, replayed through the pure decoders on **any** OS (SPEC §6.1, §17).

| File | Machine | Notes |
|---|---|---|
| `m5-air-agents.json` | MacBook Air M5 (Mac17,3), 24 GB, fanless, macOS 26.6 | M0 capture (no thermal/power/GPU extras): several Claude Code sessions, Codex, ChatGPT, Chrome, Claude desktop + its Virtualization.framework VM, idle Gradle + Kotlin daemons; qwen-image-studio not running; a second user logged in (their processes have no rusage) |
| `m5-air-baseline.json` + `m5-air-baseline.groundtruth.txt` | same Mac, 2026-09-30 01:19 IST | 3 frames 1.5 s apart, **with extras** (thermal pressure, Low Power Mode, battery/adapter, IOAccelerator GPU memory, IOReport CPU/GPU residency + energy, a JetsamEvent from 2026-09-29 17:13); on AC, battery 100 %, thermal nominal, LPM off; same workload as above, studio not running. Ground truth (`vm_stat`, `vm.swapusage`, `memory_pressure -Q`, `top -o mem`) taken right after the last frame |

## Format

A fixture is one JSON document (`oomtop_collect::replay::Fixture`):

```json
{
  "meta": { "name": "m5-air-baseline", "os": "macos", "captured_at_ms": 1790711357132,
            "description": "…", "notes": ["power source: AC Power; battery: 100%; charged", "…"] },
  "frames": [
    [ { "source": "macos.host",  "taken_at_ms": …, "read_us": …, "payload": { "type": "mac_host",  … } },
      { "source": "macos.procs", "taken_at_ms": …, "read_us": …, "payload": { "type": "mac_procs", … } } ],
    [ … next sampler tick … ]
  ]
}
```

- `mac_host` (`MacHostRaw`): integer sysctls (`hw.memsize`, `vm.pagesize`, `kern.memorystatus_level`,
  `kern.memorystatus_vm_pressure_level`, `iogpu.wired_limit_mb`, `hw.ncpu`, `hw.perflevel{0,1}.logicalcpu`),
  string sysctls (`hw.model`, `machdep.cpu.brand_string`, `kern.osproductversion`, `kern.osversion`,
  `hw.perflevel{0,1}.name`, `kern.hostname` → `<host>`), `host_statistics64(HOST_VM_INFO64)` counters by field
  name, `vm.swapusage`, CPU ticks, load average, boot time.
- `mac_procs` (`MacProcsRaw`): per process pid, ppid, uid, start time, status, comm/name, path,
  `proc_pid_rusage(RUSAGE_INFO_V4)` subset (`None` for other users' processes, with the reason),
  argv (same-user only, redacted), allowlisted markers (keys + hashed session id), responsible pid; plus the
  Mach timebase (numer/denom) for CPU time conversion.

### Host extras (namespaced keys inside `mac_host`)

Until `raw.rs` grows typed fields, the host source carries the M4 data in `MacHostRaw`'s existing maps under
these keys (decoded by `oomtop_collect::macos::extras`, which is pure and OS-independent). Older fixtures
without them decode to `unavailable`.

| key | map | source |
|---|---|---|
| `notify.com.apple.system.thermalpressurelevel` | sysctl | `notify_get_state` (0 nominal, 1 moderate, 2 heavy, 3 trapping, 4 sleeping) |
| `nsprocessinfo.thermalState`, `nsprocessinfo.isLowPowerModeEnabled` | sysctl | `NSProcessInfo` via the ObjC runtime |
| `iops.providing_ac`, `iops.providing` | sysctl, str | `IOPSGetProvidingPowerSourceType` |
| `iops.battery.{present,current_capacity,max_capacity,is_charging,time_to_empty_min,on_ac}` | sysctl | `IOPSCopyPowerSourcesInfo` (InternalBattery) |
| `iops.adapter.watts` | sysctl | `IOPSCopyExternalPowerAdapterDetails` |
| `ioaccel.<PerformanceStatistics key>`, `ioaccel.gpu-core-count`, `ioaccel.model` | sysctl, str | IORegistry `IOAccelerator` (`In use system memory`, `Device Utilization %`, …) |
| `ioreport.window_us`, `ioreport.energy_nj.{CPU Energy,GPU Energy,ANE,DRAM}` | sysctl | IOReport "Energy Model" delta |
| `ioreport.residency.{cpu.ECPU,cpu.PCPU,gpu.GPUPH}` | str | IOReport performance-state residency delta, ordered `STATE:ticks,…` |
| `dvfs.voltage-states{1-sram,5-sram,9}` | str | IORegistry `pmgr` DVFS tables, MHz per state (E-cluster, P-cluster, GPU) |
| `jetsam.<JetsamEvent-*.ips>` | str | JSON `{at_ms, largest_process, killed:[{name,pid,reason}]}` (names only) |
| `unavailable.<thermal|low_power|power|ioaccel|ioreport|dvfs|jetsam>` | str | why that part could not be read (`initializing` / `needs two samples` for IOReport's first frames) |

## Capture

```
tools/capture/capture-macos.sh <name> --description "<workload>"
# FRAMES=3 INTERVAL_MS=1500 by default; IOReport data appears from the 3rd frame (async init + first delta)
```

It builds and runs `cargo run -p oomtop-collect --example capture` (redaction below), adds context notes
(power source, battery, Low Power Mode, `memory_pressure`), then records `<name>.groundtruth.txt` right after
the last frame. Replay tests (`oomtop_collect::macos::groundtruth`, `macos::replay_tests`) compare the decoded
fixture with that ground truth (target ≤ 5 %).

Redaction (before writing): argv through `oomtop_core::redact::redact_cmdline`, shell `-c` scripts → `<script>`,
args > 200 chars → `<long-arg>`, home directories (`/Users/<name>`, `-Users-<name>-`) → `<user>`, hostname →
`<host>`; the script also replaces the user/host name in the ground truth (case-insensitive) and, if the user,
host or full name, an email or a token-like string (`sk-ant-`, `ghp_`, `xox?-`, `AKIA…`, `Bearer …`) is left in
either file, renames both to `*.REJECTED` and fails. Environments are never captured.
**Review a new fixture before committing it**
(`grep -i` for your user name, tokens, emails).

## Ground truth at capture time (m5-air-baseline)

Live comparison on the same Mac (`cargo test -p oomtop-collect -- --ignored groundtruth_live --nocapture`,
2026-09-30): footprint of the 8 same-user processes in `top`'s top 10 within **0.00–0.09 %**; `vm_stat`
free/wired/file-backed/compressor/anonymous and `vm.swapusage` within **0.00–0.02 %**; `memorystatus_level`
equal to `memory_pressure`'s free %. WindowServer and the second user's Chrome helper have no footprint
without root (`unavailable`, by design, SPEC §5).

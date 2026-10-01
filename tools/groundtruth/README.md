# tools/groundtruth

Compares oomtop with the operating system's own memory tools (SPEC §17). Target: every host metric and every
process above 16 MiB within **±5 %**. Runs on real machines, not in CI (CI runs `perf_budget.py`).

| Platform | oomtop metric | Ground truth |
|---|---|---|
| macOS | `memory.total` | `sysctl hw.memsize` |
| macOS | `memory.free`, `wired`, `compressed`, `compressed_logical`, `cached` | `vm_stat` (free, wired down, occupied by / stored in compressor, file-backed) |
| macOS | `memory.swap_used`, `swap_total` | `sysctl vm.swapusage` |
| macOS | `memorystatus_level` | `memory_pressure -Q` (free %) |
| macOS | per-process `footprint_or_pss` | `top -l 1 -o mem -stats pid,mem,cmprs,command` (MEM = `phys_footprint`) |
| Linux | `memory.total`, `available`, `free`, `swap_*` | `/proc/meminfo`, plus a second reading from `free -b` (rows marked `#free`) |
| Linux | per-process `footprint_or_pss`, `swapped` | `/proc/<pid>/smaps_rollup` `Pss`, `SwapPss` |

## Run

```console
$ cargo build --release -p oomtop-cli
$ uv run tools/groundtruth/groundtruth.py                # this machine (stdlib only; python3 works too)
$ uv run tools/groundtruth/groundtruth.py --top 40 --strict --json
$ uv run tools/groundtruth/groundtruth.py --selftest       # parser checks for both OSes (no oomtop binary needed)
$ uv run tools/groundtruth/groundtruth.py \
    --replay fixtures/macos/m5-air-baseline.json \
    --recorded fixtures/macos/m5-air-baseline.groundtruth.txt   # deterministic: fixture vs recorded tool output
```

The oomtop snapshot is taken first and the tools right after, so fast-moving values (free pages) drift a little
between the two reads; re-run before chasing a small delta. Processes of other users show `n/a` without root
(macOS `proc_pid_rusage` needs it). Battery and memory-pressure state are printed with every report.

Example (MacBook Air M5, 24 GB, 2026-09-30, on AC, 100 % battery):

```
metric                          oomtop     truth    delta
memory.total                     24.0G     24.0G    +0.0%
memory.free                     489.5M    473.1M    +3.5%
memory.wired                      2.6G      2.6G    +0.0%
memory.compressed                 9.0G      9.0G    +0.0%
memory.swap_used                  2.0G      2.0G    +0.0%
processes > 16 MiB within ±5 %: 13/13 · median |delta| 0.1%
```

## perf_budget.py

Checks a `/usr/bin/time -l` (macOS) or `-v` (GNU) report of `oomtop ndjson --interval 2s --count N --discard`
against SPEC §14 (≤ 1 % of one core, ≤ 40 MB RSS); used by the nightly `perf` CI job. `--discard` (hidden) runs
the TUI's sampling pipeline without serializing JSON. Fails above 2× budget, warns above budget; `--report-only`
never fails.

To see *where* the CPU goes, the bench example splits one sample into collector, enrichment (attribution, idle,
adapters, history, forecast, lineage) and JSON, and also reports whole-process CPU (background threads):

```console
$ cargo run --release -p oomtop-cli --example pipeline_cost -- 10
steady state per 2 s sample (761 processes): collect 8.9 ms + enrich 3.4 ms = 12.3 ms CPU → 0.61 % of one core …
```

Numbers on a fanless Mac swing 2–3× with thermal state and with other work competing for the performance
cores; compare runs taken back to back, not across sessions.

## soak.py

The SPEC §14 verdict: runs the real binary for a long window (default 660 s — long enough for the 10-minute
history ring to fill) and reports whole-process CPU from the child's exact rusage (whole run, and steady state
after a warm-up from `ps` cputime deltas), RSS every 30 s, `ru_maxrss`, and on macOS the physical footprint
(`vmmap --summary`, what oomtop itself reports as memory). `--pty COLSxROWS` runs the TUI on a pseudo-terminal
that never answers terminal queries (the slow start-up case) and reports how much it wrote. It stops only the
child it started (`q`, then SIGTERM). Point `XDG_CONFIG_HOME` / `XDG_STATE_HOME` at a scratch dir so a soak
doesn't touch your real profile.

```console
$ uv run tools/groundtruth/soak.py --seconds 690 -- target/release/oomtop ndjson --interval 2s --count 100000 --discard
$ uv run tools/groundtruth/soak.py --seconds 690 --pty 120x30 -- target/release/oomtop
```

`pipeline_cost` above is for finding where time goes; it leaves out background threads' share of rendering,
adapters and JSON and must not be used as the budget verdict.

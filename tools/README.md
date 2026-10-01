# tools

| Tool | How to run | Purpose |
|---|---|---|
| capture | `cargo run -p oomtop-collect --example capture -- <out.json> [--frames N] [--interval-ms MS] [--name] [--description] [--note]` | Record redacted `RawSample` fixtures of this machine into `fixtures/<os>/` (SPEC §17). See `fixtures/macos/README.md`. |
| groundtruth | `uv run tools/groundtruth/groundtruth.py [--top N] [--strict] [--json]` (see `tools/groundtruth/README.md`) | Compare oomtop against `top -l 1 -stats pid,mem,cmprs,command`, `vm_stat`, `sysctl vm.swapusage`, `memory_pressure` (macOS) and smaps/`free` (Linux); target delta ≤ 5 %. |
| linux-vm | `brew install lima && limactl start --name=oomtop-linux --tty=false tools/linux-vm/oomtop-linux.yaml`, then `tools/linux-vm/check.sh` | What CI runs (fmt, clippy `-D warnings`, tests) on real Linux from a Mac: Ubuntu 24.04 aarch64 natively and x86_64 under Rosetta, in a disposable guest (no host sudo). `oomtop` itself can be tried live in the guest, e.g. `limactl shell oomtop-linux -- bash -lc '~/target/aarch64-unknown-linux-gnu/debug/oomtop doctor'`. |
| perf_budget | `uv run tools/groundtruth/perf_budget.py` | SPEC §14 check: CPU % of one core at the 2 s refresh and peak RSS of `oomtop ndjson --discard`. |

Replay any fixture through the full pipeline: `oomtop --replay fixtures/macos/m5-air-agents.json --offline <command>`.

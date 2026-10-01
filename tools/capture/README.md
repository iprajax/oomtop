# tools/capture (macOS)

Records a redacted fixture of this Mac plus the OS ground truth, for replay tests (SPEC §17).

```
tools/capture/capture-macos.sh m5-air-studio --description "qwen-image-studio running, 4 agent sessions"
# → fixtures/macos/m5-air-studio.json + fixtures/macos/m5-air-studio.groundtruth.txt
```

- Frames come from `cargo run -p oomtop-collect --example capture` (the platform sampler: `macos.host` with
  thermal/power/GPU/IOReport/jetsam extras, `macos.procs`), redacted before writing. `FRAMES` (default 3) and
  `INTERVAL_MS` (default 1500): IOReport residency/energy appear from the 3rd frame (async subscribe, then the
  first delta).
- Ground truth, taken right after the last frame: `vm_stat`, `sysctl vm.swapusage`, `memory_pressure -Q`,
  `top -l 1 -o mem -n 30 -stats pid,mem,cmprs,command`. User and host names are replaced (case-insensitive); if the
  user/host/full name, an email or a token-like string remains in either file, both are renamed to `*.REJECTED`
  and the script fails (so a leaking capture cannot be committed by accident).
- Read-only: it runs `cargo`, `top`, `vm_stat`, `sysctl`, `memory_pressure`, `pmset -g` and `sw_vers` only.

Checks that use the output:

```
cargo test -p oomtop-collect macos::                       # replay tests incl. fixture-vs-ground-truth (≤ 5 %)
cargo test -p oomtop-collect -- --ignored groundtruth_live --nocapture   # live comparison on this Mac
```

Record thermal state and battery with every capture (the script adds them as fixture notes); on a fanless Air
numbers drift when hot or on battery.

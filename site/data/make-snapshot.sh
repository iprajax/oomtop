#!/bin/sh
# Regenerates site/data/snapshot.json from the recorded M5 Air fixture.
# Only aggregate group data is kept (label, kind, footprint, flags): no pids, command lines, paths or hostnames.
#
#   cargo build --release -p oomtop-cli && sh site/data/make-snapshot.sh
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
bin="${OOMTOP_BIN:-$root/target/release/oomtop}"
fixture="$root/fixtures/macos/m5-air-agents.json"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

"$bin" --replay "$fixture" json > "$tmp/snap.json"
"$bin" --replay "$fixture" reclaim --dry-run --json > "$tmp/reclaim.json"
# headroom at a size that needs reclaim, so the plan (greedy, largest gain first) is in the answer
"$bin" --replay "$fixture" headroom --need 1T --json > "$tmp/headroom.json" || true

jq -n --slurpfile s "$tmp/snap.json" --slurpfile r "$tmp/reclaim.json" --slurpfile h "$tmp/headroom.json" '
  ($s[0]) as $s | ($r[0]) as $r | ($h[0]) as $h |
  ($r.candidates | map({key: .group_id, value: .gain}) | from_entries) as $gain |
  {
    source: "oomtop --replay fixtures/macos/m5-air-agents.json json (schema_version \($s.schema_version))",
    note: "Recorded on a 24 GB MacBook Air M5 with four agent sessions, Gradle/Kotlin daemons and apps running. Groups only; no pids, command lines or paths.",
    taken_at_ms: $s.taken_at_ms,
    host: {
      os: $s.host.os_version, arch: $s.host.arch, model: $s.host.model, cpu: $s.host.cpu_brand,
      cores_performance: $s.host.cores_performance, cores_efficiency: $s.host.cores_efficiency,
      unified_memory: $s.host.unified_memory, fanless: $s.host.fanless
    },
    memory: {
      total: $s.memory.total.value, available: $s.memory.available.value, app: $s.memory.app.value,
      wired: $s.memory.wired.value, compressed: $s.memory.compressed.value, cached: $s.memory.cached.value,
      free: $s.memory.free.value, swap_used: $s.memory.swap_used.value, swap_total: $s.memory.swap_total.value,
      pressure: $s.memory.pressure.value
    },
    headroom: {
      bytes: $h.headroom,
      reclaimable: $r.total_gain,
      reclaim: [$r.candidates[] | {label, kind, gain}] | sort_by(-.gain),
      valid_for_s: $h.valid_for_s
    },
    groups: [
      $s.groups[]
      | select((.totals.footprint.value // 0) > 0)
      | {
          label, kind,
          footprint: .totals.footprint.value,
          idle, orphan,
          reclaimable: ($gain[.id] != null)
        }
      + (if $gain[.id] != null then {gain: $gain[.id]} else {} end)
    ] | sort_by(-.footprint)
  }' > "$root/site/data/snapshot.json"

echo "wrote site/data/snapshot.json ($(jq '.groups|length' "$root/site/data/snapshot.json") groups)"

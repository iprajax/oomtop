#!/bin/zsh
# tools/capture/capture-macos.sh — record a redacted macOS fixture plus the OS ground truth (SPEC §17).
#
#   tools/capture/capture-macos.sh <name> [--description "…"] [extra capture args…]
#
# Writes:
#   fixtures/macos/<name>.json              RawSample frames (macos.host incl. thermal/power/GPU/IOReport/jetsam
#                                           extras, macos.procs), redacted by the capture example
#   fixtures/macos/<name>.groundtruth.txt   `vm_stat`, `sysctl vm.swapusage`, `memory_pressure -Q` and
#                                           `top -l 1 -o mem -stats pid,mem,cmprs,command`, taken right after
#                                           the last frame; replayed by
#                                           `oomtop_collect::macos::groundtruth` tests (target delta ≤ 5 %)
#
# Env: FRAMES (default 3), INTERVAL_MS (default 1500), WORKSPACE (cargo workspace to build from; default: repo).
# Read-only: it spawns only `cargo`, `top`, `vm_stat`, `sysctl`, `memory_pressure` and `pmset -g`, and never
# signals other processes. Review the output before committing (the script greps for your user/host names).
set -euo pipefail

root=${0:A:h:h:h}
ws=${WORKSPACE:-$root}
name=${1:-}
if [[ -z "$name" || "$name" == -* ]]; then
  print -u2 "usage: $0 <name> [--description TEXT] [capture args…]"
  exit 2
fi
shift
desc="macOS capture"
extra=()
while (( $# )); do
  case $1 in
    --description) desc=$2; shift 2 ;;
    *) extra+=("$1"); shift ;;
  esac
done

export PATH=/opt/homebrew/opt/rustup/bin:$PATH
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
out=$root/fixtures/macos/$name.json
gt=$root/fixtures/macos/$name.groundtruth.txt
mkdir -p ${out:h}

# Context notes (thermal/battery matter for every number, CLAUDE.md "Verification habits").
batt=$(pmset -g batt 2>/dev/null | awk -F'\t' 'NR==2 {print $2}' | cut -d';' -f1-2 | tr -d '\n')
src=$(pmset -g batt 2>/dev/null | head -1 | sed "s/.*'\(.*\)'.*/\1/")
lpm=$(pmset -g 2>/dev/null | awk '/lowpowermode/ {print $2}')
mp=$(memory_pressure -Q 2>/dev/null | tail -1)
model=$(sysctl -n hw.model)
os=$(sw_vers -productVersion)

cargo build --release --manifest-path "$ws/Cargo.toml" -p oomtop-collect --example capture
cargo run --release --quiet --manifest-path "$ws/Cargo.toml" -p oomtop-collect --example capture -- "$out" \
  --frames "${FRAMES:-3}" --interval-ms "${INTERVAL_MS:-1500}" --name "$name" \
  --description "$desc ($model, macOS $os)" \
  --note "power source: ${src:-unknown}; battery: ${batt:-n/a}" \
  --note "low power mode: ${lpm:-unknown}" \
  --note "memory_pressure: ${mp:-n/a}" \
  --note "ground truth: $name.groundtruth.txt (taken right after the last frame)" \
  "${extra[@]}"

# Ground truth immediately after the last frame: fast-moving host counters first, top last.
{
  print "## vm_stat"; vm_stat
  print "## swapusage"; sysctl vm.swapusage
  print "## memory_pressure"; memory_pressure -Q
  print "## top"; top -l 1 -o mem -n 30 -stats pid,mem,cmprs,command | sed -n '/^PID/,$p'
} > "$gt"

# Redaction: user and host names must not appear (process names in `top` are the only free text).
# Matching is case-insensitive and literal (-F: names with regex characters are not patterns).
user=$(id -un); host=$(hostname -s); full=$(id -F 2>/dev/null || true)
esc() { print -r -- "$1" | sed -e 's/[][\/.*^$&]/\\&/g'; }
sed -i '' -e "s/$(esc "$user")/<user>/gI" -e "s/$(esc "$host")/<host>/gI" "$gt"
leaks=()
for f in "$out" "$gt"; do
  if grep -q -i -F -e "$user" -e "$host" "$f" \
     || { [[ -n "$full" ]] && grep -q -i -F -e "$full" "$f"; } \
     || grep -q -E '(sk-ant-|ghp_[A-Za-z0-9]{20}|xox[bp]-|AKIA[0-9A-Z]{16}|Bearer [A-Za-z0-9._-]{16})' "$f" \
     || grep -q -E '[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+\.[A-Za-z]{2,}([^A-Za-z]|$)' <(grep -v -E '@(openai|anthropic-ai|[a-z0-9-]+-bundled)' "$f"); then
    leaks+=("$f")
  fi
done
if (( ${#leaks} )); then
  # Moved aside so a leaking capture can't be committed by accident.
  for f in "$out" "$gt"; do [[ -e "$f" ]] && mv "$f" "$f.REJECTED"; done
  print -u2 "capture: REJECTED: user/host/full name, an email or a token-like string appears in: ${leaks[*]}"
  print -u2 "capture: inspect the .REJECTED file(s) (grep -n -i), fix redaction, re-run; never commit them"
  exit 1
fi
print -u2 "capture: wrote $out and $gt"

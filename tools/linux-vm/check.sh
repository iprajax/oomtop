#!/bin/sh
# Runs what CI runs (fmt, clippy -D warnings, tests) inside the Lima guest from oomtop-linux.yaml:
# aarch64-unknown-linux-gnu natively, then x86_64-unknown-linux-gnu under Rosetta. Exit 1 on any failure.
set -eu
VM="${OOMTOP_VM:-oomtop-linux}"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
limactl shell --workdir "$REPO" "$VM" -- bash -lc '
set -u
export PATH=$HOME/.cargo/bin:$PATH CARGO_TARGET_DIR=$HOME/target CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-2}
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc
export CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar
fail=0
uname -srm; cargo --version
cargo fmt --all --check || fail=1
for t in aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
  # x86_64 test binaries find their libc/libgcc_s here (arm64 processes skip the wrong-arch libraries)
  if [ "$t" = x86_64-unknown-linux-gnu ]; then export LD_LIBRARY_PATH=/usr/x86_64-linux-gnu/lib; fi
  echo "== $t: clippy"
  cargo clippy --workspace --all-targets --locked --target "$t" -- -D warnings || fail=1
  echo "== $t: test"
  cargo test --workspace --locked --no-fail-fast --target "$t" > "$HOME/test-$t.log" 2>&1 || fail=1
  grep -E "^test result|FAILED|panicked" "$HOME/test-$t.log" | grep -v "^test result: ok" || true
  awk -v t="$t" "/^test result/ {p+=\$4; f+=\$6; i+=\$8} END {print t \": passed\", p, \"failed\", f, \"ignored\", i}" "$HOME/test-$t.log"
done
exit $fail
'

#!/bin/sh
# Package one built oomtop binary as a release archive: dist/oomtop-<version>-<target>.tar.gz containing
#   oomtop-<version>-<target>/{oomtop, LICENSE, README.md, docs/*.md[, THIRD-PARTY-NOTICES.md]}
# Used by .github/workflows/release.yml and for local dry runs, so CI and a laptop produce the same layout
# (the layout is a contract: packaging/install.sh and the Homebrew formula read <dir>/oomtop and docs/).
#
#   tools/release/package.sh --version 0.1.0 --target universal-apple-darwin --bin dist/oomtop [--out dist]
#
# Archives are normalized (sorted entries, uid/gid 0, fixed mtime from SOURCE_DATE_EPOCH or the binary's own
# mtime) so two runs over the same binary produce byte-identical tarballs on GNU tar and bsdtar alike.
set -eu

VERSION=""
TARGET=""
BIN=""
OUT="dist"

die() { printf 'package.sh: error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift 2 ;;
    --target) TARGET="${2:-}"; shift 2 ;;
    --bin) BIN="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[ -n "$VERSION" ] || die "--version is required"
[ -n "$TARGET" ] || die "--target is required"
[ -f "$BIN" ] || die "--bin '$BIN' is not a file"
case "$VERSION" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) die "bad version '$VERSION' (want X.Y.Z, no leading v)" ;;
esac

root=$(cd "$(dirname "$0")/../.." && pwd)
name="oomtop-$VERSION-$TARGET"
mkdir -p "$OUT"
out=$(cd "$OUT" && pwd)
stage=$(mktemp -d 2>/dev/null || mktemp -d -t oomtop-pkg)
trap 'rm -rf "$stage"' EXIT INT TERM

mkdir -p "$stage/$name/docs"
cp "$BIN" "$stage/$name/oomtop"
chmod 0755 "$stage/$name/oomtop"
cp "$root/LICENSE" "$root/README.md" "$stage/$name/"
# Only the Markdown docs: the README's GIFs (docs/media, ~5 MB) would make every archive several times bigger.
for f in "$root"/docs/*.md; do
  cp "$f" "$stage/$name/docs/"
done
case "$TARGET" in
  # The macOS binary statically links jemalloc (BSD-2-Clause) as its allocator: ship its notice (SPEC §21).
  *apple-darwin) cp "$root/packaging/THIRD-PARTY-NOTICES.md" "$stage/$name/" ;;
esac
find "$stage/$name" -type f ! -name oomtop -exec chmod 0644 {} +
find "$stage/$name" -type d -exec chmod 0755 {} +

# Fixed timestamp for reproducible archives.
if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
  epoch="$SOURCE_DATE_EPOCH"
elif stat -c %Y "$BIN" >/dev/null 2>&1; then
  epoch=$(stat -c %Y "$BIN")
else
  epoch=$(stat -f %m "$BIN")
fi

archive="$out/$name.tar.gz"
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
  tar -C "$stage" --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$epoch" \
    -cf - "$name" | gzip -n -9 > "$archive"
else
  # bsdtar (macOS): no --sort; feed a sorted file list instead. touch -t needs local time, so go through date.
  stamp=$(date -r "$epoch" +%Y%m%d%H%M.%S)
  find "$stage/$name" -exec touch -h -t "$stamp" {} +
  (cd "$stage" && find "$name" -print | LC_ALL=C sort) > "$stage/.list"
  tar -C "$stage" --uid 0 --gid 0 --uname '' --gname '' --no-recursion --no-mac-metadata \
    -cf - -T "$stage/.list" | gzip -n -9 > "$archive"
fi

printf '%s\n' "$archive"

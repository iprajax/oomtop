#!/bin/sh
# oomtop installer — downloads a release archive, verifies its SHA-256 against the release's SHA256SUMS,
# and installs the binary. No sudo: installs to ~/.local/bin unless told otherwise.
#
#   curl -fsSL https://github.com/iprajax/oomtop/releases/latest/download/install.sh | sh
#   sh install.sh --version 0.1.0 --to /usr/local/bin --musl
#
# Environment: OOMTOP_VERSION, OOMTOP_INSTALL_DIR, OOMTOP_FLAVOR (gnu|musl), OOMTOP_REPO (owner/name),
# OOMTOP_BASE_URL (mirror; must contain <tag>/<archive> and <tag>/SHA256SUMS).
set -eu

REPO="${OOMTOP_REPO:-iprajax/oomtop}"
VERSION="${OOMTOP_VERSION:-}"
DEST="${OOMTOP_INSTALL_DIR:-$HOME/.local/bin}"
FLAVOR="${OOMTOP_FLAVOR:-}"
BASE_URL="${OOMTOP_BASE_URL:-}"
DRY_RUN=0

say() { printf 'oomtop-install: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

usage() {
  cat <<EOF
Usage: install.sh [--version X.Y.Z] [--to DIR] [--musl|--gnu] [--dry-run]

  --version X.Y.Z   release to install (default: latest)
  --to DIR          install directory (default: ~/.local/bin)
  --musl            Linux: fully static build (no NVIDIA/NVML support)
  --gnu             Linux: glibc >= 2.28 build with NVML (default when glibc is new enough)
  --dry-run         print what would be downloaded and installed
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2 ;;
    --to) [ $# -ge 2 ] || die "--to needs a value"; DEST="$2"; shift 2 ;;
    --musl) FLAVOR=musl; shift ;;
    --gnu) FLAVOR=gnu; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown option: $1" ;;
  esac
done

need() { command -v "$1" >/dev/null 2>&1 || die "'$1' is required"; }
need uname
need tar
need mkdir

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --proto '=https,file' --retry 3 -o "$2" "$1"; }
  latest_tag() { curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" | sed 's#.*/tag/##'; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
  latest_tag() { wget -q -S --spider "https://github.com/$REPO/releases/latest" 2>&1 | sed -n 's#.*Location: .*/tag/##p' | tail -1 | tr -d '\r'; }
else
  die "curl or wget is required"
fi

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | sed 's/.*= //'
  else die "no SHA-256 tool (sha256sum, shasum or openssl) found; refusing to install unverified"
  fi
}

# glibc version as "major.minor", empty if not glibc
glibc_version() {
  if command -v getconf >/dev/null 2>&1; then
    v=$(getconf GNU_LIBC_VERSION 2>/dev/null | sed -n 's/^glibc //p')
    [ -n "$v" ] && { echo "$v"; return; }
  fi
  ldd --version 2>/dev/null | head -1 | grep -i -e glibc -e 'gnu libc' | sed 's/.* //' || true
}

version_ge() { # $1 >= $2 (major.minor)
  a1=${1%%.*}; b1=${2%%.*}
  a2=${1#*.}; a2=${a2%%.*}; b2=${2#*.}; b2=${b2%%.*}
  [ "$a1" -gt "$b1" ] || { [ "$a1" -eq "$b1" ] && [ "$a2" -ge "$b2" ]; }
}

os=$(uname -s)
arch=$(uname -m)
case "$arch" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "unsupported architecture: $arch (x86_64 and aarch64 are published)" ;;
esac

case "$os" in
  Darwin) target="universal-apple-darwin" ;;
  Linux)
    if [ -z "$FLAVOR" ]; then
      gv=$(glibc_version)
      if [ -n "$gv" ] && version_ge "$gv" "2.28"; then FLAVOR=gnu; else FLAVOR=musl; fi
    fi
    case "$FLAVOR" in
      gnu|musl) ;;
      *) die "OOMTOP_FLAVOR must be gnu or musl" ;;
    esac
    target="$arch-unknown-linux-$FLAVOR"
    ;;
  *) die "unsupported OS: $os (Linux and macOS are published)" ;;
esac

if [ -z "$VERSION" ]; then
  tag=$(latest_tag) || die "could not resolve the latest release of $REPO"
  [ -n "$tag" ] || die "could not resolve the latest release of $REPO"
else
  tag="v${VERSION#v}"
fi
VERSION="${tag#v}"
archive="oomtop-$VERSION-$target.tar.gz"
base="${BASE_URL:-https://github.com/$REPO/releases/download}/$tag"
# A checksum fetched over plain HTTP proves nothing: only HTTPS (or a local file:// mirror for testing).
case "$base" in
  https://*|file://*) ;;
  *) die "refusing to download over a non-HTTPS URL: $base" ;;
esac
case "$tag" in
  v[0-9]*) ;;
  *) die "unexpected release tag: $tag" ;;
esac

say "installing oomtop $VERSION ($target) into $DEST"
if [ "$DRY_RUN" -eq 1 ]; then
  say "would download $base/$archive and $base/SHA256SUMS"
  exit 0
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t oomtop)
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || die "download failed: $base/SHA256SUMS"
fetch "$base/$archive" "$tmp/$archive" || die "download failed: $base/$archive"

expected=$(awk -v f="$archive" '{ n=$2; sub(/^\*/, "", n); if (n == f) print $1 }' "$tmp/SHA256SUMS")
[ -n "$expected" ] || die "$archive is not listed in SHA256SUMS"
actual=$(sha256 "$tmp/$archive")
if [ "$expected" != "$actual" ]; then
  die "checksum mismatch for $archive (expected $expected, got $actual); nothing installed"
fi
say "checksum ok ($actual)"

tar -xzf "$tmp/$archive" -C "$tmp"
src="$tmp/oomtop-$VERSION-$target/oomtop"
[ -f "$src" ] || die "archive does not contain oomtop-$VERSION-$target/oomtop"

mkdir -p "$DEST"
if command -v install >/dev/null 2>&1; then
  install -m 0755 "$src" "$DEST/oomtop"
else
  cp "$src" "$DEST/oomtop" && chmod 0755 "$DEST/oomtop"
fi
say "installed $DEST/oomtop"

case ":$PATH:" in
  *":$DEST:"*) ;;
  *) say "note: $DEST is not on your PATH; add: export PATH=\"$DEST:\$PATH\"" ;;
esac
if [ "$os" = "Linux" ] && [ "$FLAVOR" = "musl" ]; then
  say "note: the static musl build can't load NVML; NVIDIA GPU data shows as unavailable (use --gnu on glibc >= 2.28)"
fi
say "next: oomtop doctor   ·   MCP: claude mcp add oomtop -- oomtop mcp"

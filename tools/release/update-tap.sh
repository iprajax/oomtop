#!/bin/sh
# Publish the Homebrew formula for a released version to the tap repository (iprajax/homebrew-oomtop).
#
#   tools/release/update-tap.sh 0.1.0                  # render, verify, brew style, commit + push Formula/oomtop.rb
#   tools/release/update-tap.sh 0.1.0 --dry-run        # render + verify only; prints the formula, pushes nothing
#   tools/release/update-tap.sh 0.1.0 --sums dist/SHA256SUMS --url-base file://$PWD/dist --dry-run   # offline
#
# Steps: (1) fetch SHA256SUMS from the GitHub release v<version> (or --sums), (2) render
# packaging/homebrew/oomtop.rb.tmpl with packaging/homebrew/render.py, (3) download the three archives the
# formula points at and check them against SHA256SUMS (skip with --no-verify), (4) `brew style` the formula
# when brew is installed, (5) clone the tap, write Formula/oomtop.rb (+ a README on first use), commit, push.
#
# Auth for the push, first match wins: $HOMEBREW_TAP_TOKEN or $GH_TOKEN (sent as an HTTP header, never written
# to disk), else `gh auth` (as git credential helper) when gh is logged in, else your normal git credentials.
# Options: --repo OWNER/NAME (default iprajax/oomtop), --tap OWNER/NAME (default iprajax/homebrew-oomtop),
#          --branch NAME (default main), --out FILE (also write the rendered formula there).
set -eu

REPO="iprajax/oomtop"
TAP="iprajax/homebrew-oomtop"
BRANCH="main"
VERSION=""
SUMS=""
URL_BASE=""
OUT=""
VERIFY=1
DRY_RUN=0

say() { printf 'update-tap: %s\n' "$*" >&2; }
die() { say "error: $*"; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --repo) REPO="${2:?}"; shift 2 ;;
    --tap) TAP="${2:?}"; shift 2 ;;
    --branch) BRANCH="${2:?}"; shift 2 ;;
    --sums) SUMS="${2:?}"; shift 2 ;;
    --url-base) URL_BASE="${2:?}"; shift 2 ;;
    --out) OUT="${2:?}"; shift 2 ;;
    --no-verify) VERIFY=0; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    -*) die "unknown option: $1" ;;
    *) [ -z "$VERSION" ] || die "only one version, got '$VERSION' and '$1'"; VERSION="$1"; shift ;;
  esac
done
VERSION="${VERSION#v}"
[ -n "$VERSION" ] || die "usage: update-tap.sh <version> [--dry-run] (see --help)"
case "$VERSION" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) die "bad version '$VERSION'" ;;
esac
case "$REPO$TAP" in
  *[!A-Za-z0-9._/-]*) die "bad --repo/--tap" ;;
esac

root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d 2>/dev/null || mktemp -d -t oomtop-tap)
trap 'rm -rf "$work"' EXIT INT TERM
release="https://github.com/$REPO/releases/download/v$VERSION"
[ -n "$URL_BASE" ] || URL_BASE="$release"

command -v curl >/dev/null 2>&1 || die "curl is required"
fetch() { curl -fsSL --proto '=https,file' --retry 3 -o "$2" "$1"; }
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# 1. checksums
if [ -n "$SUMS" ]; then
  cp "$SUMS" "$work/SHA256SUMS"
else
  say "fetching $release/SHA256SUMS"
  fetch "$release/SHA256SUMS" "$work/SHA256SUMS" || die "no SHA256SUMS at $release (is v$VERSION published, not a draft?)"
fi

# 2. render (stdlib-only Python; uv when present per project convention, else python3 >= 3.8)
formula="$work/oomtop.rb"
if command -v uv >/dev/null 2>&1; then
  py="uv run --no-project --quiet python"
else
  py="python3"
fi
$py "$root/packaging/homebrew/render.py" --version "$VERSION" --sums "$work/SHA256SUMS" \
  --template "$root/packaging/homebrew/oomtop.rb.tmpl" --url-base "$URL_BASE" > "$formula"
say "rendered formula for $VERSION ($URL_BASE)"

# 3. every URL in the formula must serve exactly the bytes SHA256SUMS lists
if [ "$VERIFY" -eq 1 ]; then
  for target in universal-apple-darwin x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu; do
    name="oomtop-$VERSION-$target.tar.gz"
    want=$(awk -v f="$name" '{ n=$2; sub(/^\*/, "", n); if (n == f) print $1 }' "$work/SHA256SUMS")
    [ -n "$want" ] || die "$name missing from SHA256SUMS"
    fetch "$URL_BASE/$name" "$work/$name" || die "download failed: $URL_BASE/$name"
    got=$(sha256 "$work/$name")
    [ "$want" = "$got" ] || die "checksum mismatch for $name: SHA256SUMS $want, download $got"
    tar -tzf "$work/$name" "oomtop-$VERSION-$target/oomtop" >/dev/null || die "$name lacks oomtop-$VERSION-$target/oomtop"
    say "verified $name ($got)"
    rm -f "$work/$name"
  done
fi

# 4. Homebrew's own style rules (rubocop); the tap's CI would reject what fails here
if command -v brew >/dev/null 2>&1; then
  # brew only applies the formula rule set to files inside a tap, so lint through a throwaway tap directory
  # (a tap is just Library/Taps/<user>/homebrew-<name>); removed again right after.
  check_user="oomtop-check"
  check_name="style$$"
  check_dir="$(brew --repository)/Library/Taps/$check_user/homebrew-$check_name"
  mkdir -p "$check_dir/Formula"
  cp "$formula" "$check_dir/Formula/oomtop.rb"
  rc=0
  HOMEBREW_NO_AUTO_UPDATE=1 brew style --formula "$check_user/$check_name/oomtop" >&2 || rc=$?
  rm -rf "$check_dir"
  rmdir "$(dirname "$check_dir")" 2>/dev/null || true
  [ "$rc" -eq 0 ] || die "brew style failed on the rendered formula"
  say "brew style: no offenses"
else
  say "brew not installed: skipping brew style"
fi

[ -z "$OUT" ] || cp "$formula" "$OUT"
if [ "$DRY_RUN" -eq 1 ]; then
  cat "$formula"
  say "dry run: not pushing to $TAP"
  exit 0
fi

# 5. commit to the tap
command -v git >/dev/null 2>&1 || die "git is required"
token="${HOMEBREW_TAP_TOKEN:-${GH_TOKEN:-}}"
if [ -n "$token" ]; then
  basic=$(printf 'x-access-token:%s' "$token" | base64 | tr -d '\n')
  set -- -c credential.helper= -c "http.https://github.com/.extraheader=AUTHORIZATION: basic $basic"
elif command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
  set -- -c credential.helper= -c 'credential.https://github.com.helper=!gh auth git-credential'
else
  set --
fi
tap_url="${OOMTOP_TAP_URL:-https://github.com/$TAP.git}"   # override: tests against a local bare repo
say "cloning $tap_url"
git "$@" clone --quiet "$tap_url" "$work/tap" 2>"$work/clone.err" || {
  cat "$work/clone.err" >&2
  die "cannot clone $tap_url: create the (public, empty is fine) repository first: gh repo create $TAP --public"
}
cd "$work/tap"
git checkout --quiet -B "$BRANCH" 2>/dev/null || true
if git rev-parse --verify --quiet "origin/$BRANCH" >/dev/null; then
  git reset --quiet --hard "origin/$BRANCH"
fi

mkdir -p Formula
cp "$formula" Formula/oomtop.rb
if [ ! -f README.md ]; then
  cat > README.md <<EOF
# homebrew-oomtop

Homebrew tap for [oomtop](https://github.com/$REPO): see the OOM coming.

\`\`\`sh
brew install ${TAP%%/*}/oomtop/oomtop
\`\`\`

\`Formula/oomtop.rb\` is generated from \`packaging/homebrew/oomtop.rb.tmpl\` in the main repository by
\`tools/release/update-tap.sh\` (or the release workflow) on every release; edit the template there, not here.
EOF
fi
git add Formula/oomtop.rb README.md
if git diff --cached --quiet; then
  say "tap already has oomtop $VERSION; nothing to push"
  exit 0
fi
if [ -z "$(git config user.email || true)" ]; then
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    git config user.name "github-actions[bot]"
    git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
  else
    die "set git user.name/user.email first"
  fi
fi
git commit --quiet -m "oomtop $VERSION" -m "Rendered from $REPO packaging/homebrew/oomtop.rb.tmpl against $release/SHA256SUMS."
git "$@" push --quiet origin "HEAD:$BRANCH"
say "pushed Formula/oomtop.rb ($VERSION) to $TAP@$BRANCH"
say "test: brew update && brew install ${TAP%%/*}/oomtop/oomtop && brew test oomtop"

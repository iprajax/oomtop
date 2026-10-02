#!/bin/sh
# Compose the GitHub Release body for a version and print it to stdout.
#
#   tools/release/release-notes.sh --version 0.1.0 [--changelog CHANGELOG.md] [--signed true|false]
#                                  [--notarized true|false] [--repo iprajax/oomtop]
#
# The body is: the CHANGELOG.md section for the version (Keep a Changelog style "## [0.1.0] - date", or
# "## 0.1.0" / "## v0.1.0"), then install lines, then how to verify (SHA256SUMS + build provenance), then the
# macOS signing state. Exit status 0 = changelog section found; 3 = not found (the body is still printed, and
# the release job then adds `--generate-notes` so GitHub fills in the commit list).
set -eu

VERSION=""
CHANGELOG="CHANGELOG.md"
SIGNED="false"
NOTARIZED="false"
REPO="${GITHUB_REPOSITORY:-iprajax/oomtop}"

die() { printf 'release-notes.sh: error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift 2 ;;
    --changelog) CHANGELOG="${2:-}"; shift 2 ;;
    --signed) SIGNED="${2:-}"; shift 2 ;;
    --notarized) NOTARIZED="${2:-}"; shift 2 ;;
    --repo) REPO="${2:-}"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done
VERSION="${VERSION#v}"
[ -n "$VERSION" ] || die "--version is required"

section=""
if [ -f "$CHANGELOG" ]; then
  # From the version's heading (exclusive) to the next "## " heading. Link-reference lines ("[0.1.0]: url")
  # at the bottom of a Keep a Changelog file are dropped.
  section=$(awk -v v="$VERSION" '
    /^## / {
      if (found) exit
      h = $0
      sub(/^## +/, "", h); sub(/^\[/, "", h); sub(/^v/, "", h)
      split(h, w, /[] \t]/)
      if (w[1] == v) { found = 1; next }
    }
    found && !/^\[[^]]+\]: / { print }
  ' "$CHANGELOG" | sed -e '/./,$!d')   # strip leading blank lines
  # Relative links ("[SPEC.md](SPEC.md)") would resolve against /releases/ on GitHub: pin them to the tag.
  section=$(printf '%s\n' "$section" | REPO="$REPO" TAG="v$VERSION" \
    perl -pe 's{\]\((?!https?:|mailto:|#)([^)\s]+)\)}{](https://github.com/$ENV{REPO}/blob/$ENV{TAG}/$1)}g')
fi

found=3
if [ -n "$(printf '%s' "$section" | tr -d '[:space:]')" ]; then
  found=0
  printf '%s\n\n' "$section"
fi

cat <<EOF
## Install

\`\`\`sh
brew install iprajax/oomtop/oomtop                                             # macOS / Linux (Homebrew tap)
curl -fsSL https://github.com/$REPO/releases/download/v$VERSION/install.sh | sh   # checksum-verified, no sudo
cargo install oomtop-cli                                                        # from source (binary: oomtop)
\`\`\`

Debian/Ubuntu: \`sudo apt install ./oomtop_${VERSION}-1_amd64.deb\` · Fedora/RHEL: \`sudo dnf install ./oomtop-${VERSION}-1.x86_64.rpm\`
(arm64 / aarch64 packages are attached too). Linux \`gnu\` builds need glibc >= 2.28 and load NVML for NVIDIA GPUs;
\`musl\` builds are fully static and report NVIDIA GPU data as unavailable.

## Verify

\`\`\`sh
shasum -a 256 -c SHA256SUMS --ignore-missing                      # or: sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify oomtop-$VERSION-<target>.tar.gz --repo $REPO   # SLSA build provenance from this workflow
\`\`\`

EOF

if [ "$SIGNED" = "true" ] && [ "$NOTARIZED" = "true" ]; then
  echo "macOS: the universal binary is signed with a Developer ID (hardened runtime) and notarized by Apple."
elif [ "$SIGNED" = "true" ]; then
  echo "macOS: the universal binary is signed with a Developer ID (hardened runtime) but **not notarized** in this release."
else
  cat <<'EOF'
**macOS: this release is not Developer ID signed or notarized** (the binary carries an ad-hoc signature only;
Apple signing secrets were not configured). Homebrew and `curl | sh` installs are unaffected (no quarantine
flag). If you downloaded the archive in a browser, clear the flag once: `xattr -d com.apple.quarantine oomtop`.
EOF
fi

exit "$found"

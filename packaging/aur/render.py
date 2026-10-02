# /// script
# requires-python = ">=3.9"
# ///
"""Render the AUR package oomtop-bin (PKGBUILD + .SRCINFO) for a release.

    uv run packaging/aur/render.py --version 0.1.0 --sums dist/SHA256SUMS --out dist/aur/oomtop-bin

The checksums come from the release's SHA256SUMS (fails if an archive is missing). .SRCINFO is produced by
sourcing the rendered PKGBUILD in bash, as `makepkg --printsrcinfo` does, so the two can't drift (makepkg isn't
available on macOS; on Arch, `makepkg --printsrcinfo` gives an equivalent file).
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

# bash: print every .SRCINFO field from the sourced PKGBUILD
DUMP = r"""
set -e
source "$1"
out() { local k=$1; shift; local v; for v in "$@"; do printf '\t%s = %s\n' "$k" "$v"; done; }
printf 'pkgbase = %s\n' "$pkgname"
out pkgdesc "$pkgdesc"
out pkgver "$pkgver"
out pkgrel "$pkgrel"
out url "$url"
out arch "${arch[@]}"
out license "${license[@]}"
out depends "${depends[@]}"
out optdepends "${optdepends[@]}"
out provides "${provides[@]}"
out conflicts "${conflicts[@]}"
out options "${options[@]}"
for a in "${arch[@]}"; do
  eval "src=(\"\${source_$a[@]}\")"; eval "sums=(\"\${sha256sums_$a[@]}\")"
  out "source_$a" "${src[@]}"
  out "sha256sums_$a" "${sums[@]}"
done
printf '\npkgname = %s\n' "$pkgname"
"""


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--version", required=True)
    ap.add_argument("--sums", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    a = ap.parse_args()
    if not re.fullmatch(r"\d+\.\d+\.\d+", a.version):
        # pacman versions can't contain '-'; pre-releases need a pkgver mapping first
        sys.exit(f"error: AUR pkgver must be X.Y.Z, got {a.version!r}")

    sums = {}
    for line in a.sums.read_text().splitlines():
        parts = line.split()
        if len(parts) == 2:
            sums[parts[1].lstrip("*")] = parts[0].lower()

    def sha(arch: str) -> str:
        name = f"oomtop-{a.version}-{arch}-unknown-linux-gnu.tar.gz"
        if name not in sums:
            sys.exit(f"error: {name} not in {a.sums}")
        return sums[name]

    text = (
        (HERE / "PKGBUILD.in")
        .read_text()
        .replace("@VERSION@", a.version)
        .replace("@SHA256_X86_64@", sha("x86_64"))
        .replace("@SHA256_AARCH64@", sha("aarch64"))
    )
    assert "@" not in re.sub(r"<[^>]*>", "", text), "unrendered placeholder"
    a.out.mkdir(parents=True, exist_ok=True)
    pkgbuild = a.out / "PKGBUILD"
    pkgbuild.write_text(text)
    srcinfo = subprocess.run(["bash", "-c", DUMP, "dump", str(pkgbuild)], check=True, capture_output=True, text=True)
    (a.out / ".SRCINFO").write_text(srcinfo.stdout)
    print(f"wrote {pkgbuild} and {a.out / '.SRCINFO'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

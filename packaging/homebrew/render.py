#!/usr/bin/env python3
"""Render the Homebrew formula from the template and a SHA256SUMS file.

    python3 packaging/homebrew/render.py --version 0.1.0 --sums dist/SHA256SUMS \
        --template packaging/homebrew/oomtop.rb.tmpl > oomtop.rb

Locally: `uv run packaging/homebrew/render.py ...` (stdlib only). Fails if any checksum is missing, so a
formula can never point at an artifact that wasn't built.
"""

import argparse
import re
import sys
from pathlib import Path

ARTIFACTS = {
    "@SHA_MACOS@": "oomtop-{v}-universal-apple-darwin.tar.gz",
    "@SHA_LINUX_X86_64@": "oomtop-{v}-x86_64-unknown-linux-gnu.tar.gz",
    "@SHA_LINUX_AARCH64@": "oomtop-{v}-aarch64-unknown-linux-gnu.tar.gz",
}


def parse_sums(text: str) -> dict:
    sums = {}
    for line in text.splitlines():
        m = re.match(r"^([0-9a-f]{64})\s+\*?(\S+)$", line.strip())
        if m:
            sums[m.group(2)] = m.group(1)
    return sums


def render(template: str, version: str, sums: dict) -> str:
    if not re.fullmatch(r"\d+\.\d+\.\d+([-.+][0-9A-Za-z.-]+)?", version):
        raise SystemExit(f"bad version {version!r}")
    out = template.replace("@VERSION@", version)
    for placeholder, pattern in ARTIFACTS.items():
        name = pattern.format(v=version)
        if name not in sums:
            raise SystemExit(f"missing checksum for {name}")
        out = out.replace(placeholder, sums[name])
    left = re.findall(r"@[A-Z_0-9]+@", out)
    if left:
        raise SystemExit(f"unrendered placeholders: {sorted(set(left))}")
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--version", required=True)
    ap.add_argument("--sums", required=True, type=Path)
    ap.add_argument("--template", required=True, type=Path)
    a = ap.parse_args()
    sys.stdout.write(render(a.template.read_text(), a.version, parse_sums(a.sums.read_text())))


if __name__ == "__main__":
    main()

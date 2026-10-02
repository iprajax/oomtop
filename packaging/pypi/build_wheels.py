# /// script
# requires-python = ">=3.9"
# ///
"""Build one PyPI wheel per release target from the GitHub release archives.

    uv run packaging/pypi/build_wheels.py --dist dist --version 0.1.0 --out dist/wheels

`--dist` must hold the release's oomtop-<v>-<target>.tar.gz files and SHA256SUMS (as produced by release.yml, or
`gh release download v<v> -p '*.tar.gz' -p SHA256SUMS -D dist`). Every archive is verified against SHA256SUMS
before its binary goes into a wheel; a missing archive or a mismatch is an error, never a skipped wheel.
"""

import argparse
import hashlib
import os
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent

# release target -> wheel platform tag (glibc 2.28 baseline matches the zigbuild target in release.yml)
TARGETS = {
    "x86_64-unknown-linux-gnu": "manylinux_2_28_x86_64",
    "aarch64-unknown-linux-gnu": "manylinux_2_28_aarch64",
    "x86_64-unknown-linux-musl": "musllinux_1_2_x86_64",
    "aarch64-unknown-linux-musl": "musllinux_1_2_aarch64",
    "universal-apple-darwin": "macosx_11_0_universal2",
}


def sha256(p: Path) -> str:
    h = hashlib.sha256()
    with p.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def load_sums(p: Path) -> dict:
    sums = {}
    for line in p.read_text().splitlines():
        parts = line.split()
        if len(parts) == 2:
            sums[parts[1].lstrip("*")] = parts[0].lower()
    return sums


def run(cmd, **kw):
    print("+", " ".join(str(c) for c in cmd), flush=True)
    subprocess.run(cmd, check=True, **kw)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dist", required=True, type=Path)
    ap.add_argument("--version", required=True)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--targets", default=",".join(TARGETS), help="comma-separated subset (default: all)")
    a = ap.parse_args()

    sums = load_sums(a.dist / "SHA256SUMS")
    a.out.mkdir(parents=True, exist_ok=True)
    stage = HERE / "build" / "bin"
    shutil.copy(REPO / "LICENSE", HERE / "LICENSE")

    for target in a.targets.split(","):
        tag = TARGETS[target]
        name = f"oomtop-{a.version}-{target}"
        archive = a.dist / f"{name}.tar.gz"
        if not archive.is_file():
            sys.exit(f"error: missing {archive}")
        want = sums.get(archive.name)
        got = sha256(archive)
        if want != got:
            sys.exit(f"error: checksum mismatch for {archive.name}: SHA256SUMS={want} actual={got}")
        print(f"{archive.name}: sha256 ok")

        if stage.exists():
            shutil.rmtree(stage)
        stage.mkdir(parents=True)
        with tarfile.open(archive) as t:
            member = t.getmember(f"{name}/oomtop")
            src = t.extractfile(member)
            assert src is not None
            (stage / "oomtop").write_bytes(src.read())
        os.chmod(stage / "oomtop", 0o755)

        tmp = HERE / "build" / "wheel"
        if tmp.exists():
            shutil.rmtree(tmp)
        env = dict(os.environ, OOMTOP_PY_VERSION=a.version)
        run(["uv", "build", "--wheel", "--out-dir", str(tmp), str(HERE)], env=env)
        (whl,) = tmp.glob("*.whl")
        # py3-none-any -> py3-none-<platform>: the binary is platform-specific, the Python shim is not
        run(
            ["uvx", "--from", "wheel>=0.43", "wheel", "tags", "--remove",
             "--python-tag", "py3", "--abi-tag", "none", "--platform-tag", tag, str(whl)]
        )
        (tagged,) = tmp.glob("*.whl")
        shutil.move(str(tagged), a.out / tagged.name)
        print(f"-> {a.out / tagged.name}")

    shutil.rmtree(HERE / "build", ignore_errors=True)
    (HERE / "LICENSE").unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

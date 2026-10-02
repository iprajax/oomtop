# oomtop (PyPI)

**See the OOM coming.** A terminal resource monitor for local AI work: true memory accounting (GPU/Metal,
compressed, swapped), attribution of every process to the agent session, app, sandbox or model server that owns
it, headroom checks and an MCP server for agents. Linux and macOS.

```console
$ uvx oomtop                     # run without installing
$ uv tool install oomtop         # or: pipx install oomtop
$ oomtop headroom --need 13G
```

Each wheel carries the native binary from the matching [GitHub release](https://github.com/iprajax/oomtop/releases),
verified against the release's `SHA256SUMS` when the wheel was built. Wheels: `manylinux_2_28` (x86_64, aarch64;
NVIDIA via NVML), `musllinux_1_2` (static, no NVML), `macosx_11_0_universal2`. Nothing is downloaded at install or
run time. There is no sdist; on other platforms use `cargo install oomtop-cli --locked`.

Docs and source: <https://github.com/iprajax/oomtop>. MIT licensed.

<!-- mcp-name: io.github.iprajax/oomtop -->

## Building (maintainers)

```console
$ gh release download v0.1.0 -R iprajax/oomtop -p '*.tar.gz' -p SHA256SUMS -D dist
$ uv run packaging/pypi/build_wheels.py --dist dist --version 0.1.0 --out dist/wheels
$ uvx twine check dist/wheels/*.whl
```

`.github/workflows/publish.yml` does this on every published release and uploads with PyPI trusted publishing.

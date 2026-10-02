# oomtop (npm)

**See the OOM coming.** A terminal resource monitor for local AI work: true memory accounting (GPU/Metal,
compressed, swapped), attribution of every process to the agent session, app, sandbox or model server that owns
it, headroom checks and an MCP server for agents. Linux and macOS.

```console
$ npx oomtop                 # TUI
$ npx oomtop headroom --need 13G
$ npm i -g oomtop && oomtop mcp
```

This package contains no native code. On install, or on first run if install scripts are disabled, it downloads
the matching archive from the [GitHub release](https://github.com/iprajax/oomtop/releases) for this exact version,
**verifies its SHA-256 against the release's `SHA256SUMS`** (and refuses to run anything that doesn't match), and
caches the binary.

| Variable | Effect |
|---|---|
| `OOMTOP_BINARY_PATH` | use an existing binary (Homebrew, cargo) instead of downloading one |
| `OOMTOP_BASE_URL` | mirror holding `v<version>/<archive>` and `v<version>/SHA256SUMS` |
| `OOMTOP_FLAVOR` | Linux: `gnu` (glibc ≥ 2.28, NVIDIA via NVML) or `musl` (static, no NVML) |
| `OOMTOP_CACHE_DIR` | cache location used when the package directory is read-only |

Docs, other install methods and source: <https://github.com/iprajax/oomtop>. MIT licensed.

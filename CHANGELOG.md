# Changelog

All notable changes to oomtop are documented here. The format follows
[Keep a Changelog 1.1](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Before 1.0, a minor version (0.x.0) may break
compatibility; patch versions never do. What counts as the public interface is listed in
[docs/RELEASING.md](docs/RELEASING.md#what-semver-covers).

Each entry goes under `Unreleased` in the PR that makes the change, in one of: **Added**, **Changed**,
**Deprecated**, **Removed**, **Fixed**, **Security**.

## [Unreleased]

### Added

- Project governance: [CONTRIBUTING](CONTRIBUTING.md), [Code of Conduct](CODE_OF_CONDUCT.md),
  [security policy](SECURITY.md), [governance](GOVERNANCE.md), [maintainers](MAINTAINERS.md),
  [release process](docs/RELEASING.md) and [roadmap](docs/ROADMAP.md); issue forms and a PR template.
- CI: `cargo deny` (licenses, advisories, bans, sources) and `typos` jobs; Dependabot for Cargo and GitHub
  Actions.

## [0.1.0] - Unreleased

First public release: milestones M0 to M8 of [SPEC.md](https://github.com/iprajax/oomtop/blob/main/SPEC.md) §18. macOS (Apple Silicon first) and Linux
(x86_64, aarch64), no root required.

### Added

**Memory that tells the truth**
- Per-process memory from macOS `phys_footprint` (`proc_pid_rusage`, includes Metal allocations) and Linux
  PSS/SwapPss (`smaps_rollup`), plus GPU memory via NVML (loaded at runtime) and DRM fdinfo (AMD, Intel). RSS is
  never shown alone as "memory used", and every number carries its source and quality (`exact`, `estimate`,
  `unavailable(reason)`).
- Host memory split by what really holds RAM: apps, GPU/Metal, compressed, wired, cache; swap with the macOS
  swap ceiling (free space on the VM volume).
- Apple Silicon GPU, power and thermal data through IOReport and IOKit, without sudo, resolved at runtime so it
  degrades to `unavailable` if the API changes.

**Attribution: things, not PIDs**
- Every process is grouped into the agent session (Claude Code, Codex, Cursor, Aider, Gemini, Goose, ...), app,
  model server (Ollama, llama.cpp, sd.cpp, vLLM, LM Studio, MLX), sandbox (Virtualization.framework VMs,
  Docker/OrbStack/Podman, Firecracker, bubblewrap, Seatbelt) or build daemon that owns it, with a confidence
  per group. Uses cgroups, macOS responsible pid, session markers, ancestry, a lineage journal and user rules
  (`rules.d/*.toml`).
- Orphans (children that outlived their session) and idle groups (cost memory, do nothing) are flagged.

**Answers**
- `oomtop headroom --need SIZE | --model FILE.gguf`: can this load now? Exit code 0 (fits), 3 (fits after
  reclaim, naming what to stop), 4 (doesn't fit, with the shortfall). Checks the Metal GPU budget on Apple
  Silicon; `--model` estimates weights, KV cache and buffers from the GGUF header alone.
- `oomtop why`: ranked causes of slowness with evidence: swap storms, memory pressure / PSI, thermal pressure,
  Low Power Mode, battery, reduced clocks. Throttling is reported only under load.
- OOM forecast: when swap or available memory trends steadily toward a killer's threshold (kernel OOM,
  systemd-oomd, earlyoom, macOS jetsam) the headline gives an ETA and the likely victim; single spikes never
  trigger it.
- `oomtop reclaim [--dry-run]`: idle build daemons, orphans and idle model servers, largest estimated gain
  first. Model servers are unloaded through their own API first; then SIGTERM; SIGKILL only on a second explicit
  yes. Protected processes are never offered.
- `oomtop models`: model files on disk with size, last use, duplicates, and who has them loaded.

**Interfaces**
- TUI with htop-style per-core meters (E/P cores on Apple Silicon), `Mem`/`Swp` meters, an answer-first
  headline, Home / Processes / Models / Sandboxes / Reclaim / Timeline views, filter queries
  (`mem>2G kind:daemon idle>30m`), command palette, "why is this here?", pin, mute, rename, watch and compare.
  F1-F10 bar and four key presets (default, vim, emacs, htop).
- `oomtop mcp`: MCP server over stdio with read-only tools (`get_headroom`, `can_fit`, `top_consumers`,
  `list_groups`, `list_model_servers`, `list_sandboxes`, `explain_slowdown`, `suggest_reclaim`); `reclaim` only
  with `--allow-actions`, and only after the user accepts an MCP elicitation. Samples per call, so idle servers
  cost nothing.
- `oomtop serve`: HTTP JSON and Prometheus `/metrics` on 127.0.0.1:9469.
- `oomtop json` / `oomtop ndjson`: redacted, versioned (`schema_version` 1) snapshots for scripts.
- `oomtop doctor`: every source as ok / partial / unavailable with the reason, plus detected terminal
  capabilities.
- `--replay FIXTURE`: run the whole pipeline against a recorded machine; nothing is ever signalled.

**Make it yours**
- Layered TOML config (defaults, `/etc/oomtop`, user, drop-ins, per-host, `OOMTOP_*` environment, flags) with `oomtop config` commands,
  comment-preserving edits, a settings screen and a JSON Schema.
- Themes: the default `terminal` theme inherits your terminal's 16 colors and never paints a background;
  opt-in `mono`, `ember`, `mint`, `sand`, `coral`, `high-contrast`, `colorblind`, `none`; import from base16,
  iTerm2, Ghostty, Kitty and Alacritty; `theme check` enforces valid tokens, WCAG contrast and no blue/purple defaults.
  `NO_COLOR`, `--plain` (screen readers) and `--ascii` are honored.
- Local personalization (frecency, roles, ranking) in a local SQLite profile; `--no-learn` turns it off and
  `oomtop profile reset` forgets it.

**Packaging**
- Release workflow for Linux glibc 2.28 baseline and fully static musl builds (x86_64, aarch64), a macOS
  universal binary (codesigned and notarized when the secrets are configured), `.deb` and `.rpm`, `SHA256SUMS`,
  a checksum-verifying `install.sh`, and a Homebrew formula rendered from the checksums.

### Security

- Command lines are redacted in every export; environments are read only for allowlisted session-marker keys,
  hashed, and never stored or exported. No telemetry; network calls go only to local adapter endpoints.
- Process identity (`pid` + start time) is re-verified immediately before any signal.

[Unreleased]: https://github.com/iprajax/oomtop/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/iprajax/oomtop/releases/tag/v0.1.0

# Contributing to oomtop

Thanks for helping. oomtop is a small project with a clear contract: [SPEC.md](SPEC.md) (system design) and
[UX.md](UX.md) (terminal UX) describe what it does, and the code follows them. This guide covers how to build it,
how to test it, and how a change gets merged. How decisions are made is in [GOVERNANCE.md](GOVERNANCE.md).

By taking part you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). Security problems go to
[SECURITY.md](SECURITY.md), never to a public issue.

## Ways to help

- **Report what oomtop gets wrong on your machine.** A bug report with `oomtop doctor` output and a ground-truth
  comparison (below) is the most useful contribution there is: oomtop is only as good as the machines it has seen.
- **Contribute a fixture.** A redacted recording of a machine we don't have (an NVIDIA box, a Linux laptop with
  an AMD GPU, a Mac under memory pressure) becomes a replay test that runs on every CI build.
- **Add an adapter or a detection rule** for a model server, agent or sandbox oomtop doesn't know yet.
- **Improve docs.** If something here was wrong or unclear, fixing it is a welcome first PR.

Not sure where to start? Look for issues labeled [`good first issue`](https://github.com/iprajax/oomtop/labels/good%20first%20issue)
or [`help wanted`](https://github.com/iprajax/oomtop/labels/help%20wanted), or ask in
[Discussions](https://github.com/iprajax/oomtop/discussions).

## Development setup

You need Rust **stable** (the workspace's MSRV is 1.88; see `rust-version` in `Cargo.toml`). No root, no
kernel modules, no GPU.

```console
$ brew install rustup && rustup-init -y            # macOS (Homebrew); Linux: https://rustup.rs
$ rustup component add clippy rustfmt               # rust-toolchain.toml asks for both
$ git clone https://github.com/iprajax/oomtop && cd oomtop
$ cargo build                                       # binary: target/debug/oomtop
$ cargo run -- doctor                               # what this machine exposes, and what it doesn't
```

Optional helpers:

```console
$ cargo install cargo-insta --locked                # review snapshot changes
$ brew install cargo-deny typos-cli actionlint vhs  # the extra CI checks, and README GIFs
```

### The checks (what CI runs)

Run these before you open a PR. CI runs the same on `ubuntu-latest`, `ubuntu-24.04-arm` and `macos-latest`,
plus an MSRV build.

```console
$ cargo fmt --all -- --check
$ cargo clippy --workspace --all-targets --locked -- -D warnings
$ cargo test --workspace --locked
$ cargo deny check                                  # licenses, advisories, bans, sources (deny.toml)
$ typos                                             # spelling (_typos.toml for real false positives)
$ actionlint                                        # only if you touched .github/workflows/
```

Clippy warnings are errors. Don't silence one with `#[allow]` unless the PR says why.

### Workspace layout

| Crate | What it is |
|---|---|
| `oomtop-core` | Pure logic, **no I/O**: data model, units, attribution, headroom, forecast, `why`, ranking |
| `oomtop-collect` | OS collectors (macOS `proc_pid_rusage`/sysctl/IOReport, Linux `/proc`/cgroup/PSI/NVML/DRM) |
| `oomtop-detect` | Agent, app, sandbox and model-server detection rules |
| `oomtop-adapters` | Model-server and sandbox adapters (127.0.0.1 only) |
| `oomtop-config` | Layered TOML config, themes, keymaps, JSON Schema |
| `oomtop-state` | Local SQLite profile store (personalization, lineage) |
| `oomtop-tui` | The terminal UI (ratatui + crossterm) |
| `oomtop-mcp` | MCP server over stdio |
| `oomtop-serve` | HTTP JSON + Prometheus `/metrics` |
| `oomtop-cli` | The `oomtop` binary |

The contract between crates is [docs/CONTRACTS.md](docs/CONTRACTS.md). Public types in
`crates/oomtop-core/src/model.rs` are frozen: additive changes only.

## Rules the code follows

These come from SPEC.md and are checked in review:

- **Every number carries its source.** Never present RSS alone as "memory used". Missing data is
  `unavailable(reason)`, never a silent zero.
- **Collectors never panic** on missing or changed OS data, report `available | partial | unavailable`, and stay
  within their per-sample time budget (50 ms; the process listing gets 200 ms, SPEC §21).
- **Safety.** Nothing is stopped without explicit confirmation. SIGTERM first; SIGKILL only on a second explicit
  yes. `(pid, start_time)` is re-verified right before any signal. Protected processes stay protected.
- **Privacy.** Command lines are redacted in every export. Environments are never stored or exported. No
  telemetry; network calls go to 127.0.0.1 adapters only.
- **Synchronous design.** No async runtime (`deny.toml` bans `tokio`).
- **Performance budget** (SPEC §14): at most 1 % of one core at the 2 s refresh, 40 MB RSS, first frame under
  300 ms. If you touch sampling or ranking, measure it and put the numbers in the PR:

  ```console
  $ cargo build --release -p oomtop-cli
  $ /usr/bin/time -l target/release/oomtop ndjson --interval 2s --count 30 --offline --discard >/dev/null 2>time.txt
  $ uv run tools/groundtruth/perf_budget.py time.txt --seconds 60     # Linux: /usr/bin/time -v
  ```

  On a fanless laptop, compare runs taken back to back and note the thermal state and battery.
- **Theme.** The default `terminal` theme never paints a background. No blue or purple in built-in themes, and
  meaning is never carried by color alone (`oomtop theme check <name>` enforces it).

## Testing

### Unit and golden tests

`oomtop-core` is pure, so most logic is tested with fixtures and golden files. Put tests next to the code
(`#[cfg(test)]`) or in the crate's `tests/` directory.

### Snapshot tests (insta)

Attribution, headroom, `can_fit`, forecast, `why` and every TUI screen are covered by
[insta](https://insta.rs) snapshots (`tests/snapshots/*.snap`). TUI screens render through ratatui's
`TestBackend` at 60, 80, 120 and 200 columns, in 16-color and `none` modes (`crates/oomtop-tui/tests/render.rs`).

When you change output on purpose:

```console
$ cargo insta test --workspace          # run tests; new/changed snapshots are written as *.snap.new
$ cargo insta review                    # accept or reject each change, one by one
```

Without cargo-insta: `INSTA_UPDATE=always cargo test -p <crate>`, then read `git diff` on the `.snap` files.

Review every changed snapshot as if it were code. A snapshot that changes for a reason the PR doesn't explain is
a bug. Never commit `*.snap.new` files (they are in `.gitignore`).

### Fixtures (recorded machines)

Every OS reading is recorded as a `RawSample` and decoded by pure code, so a recording of any machine replays on
any OS in CI. Fixtures live in `fixtures/macos/` and `fixtures/linux/`; their READMEs describe the format.

```console
$ tools/capture/capture-macos.sh m5-air-studio --description "image server running, 4 agent sessions"
#   -> fixtures/macos/m5-air-studio.json + .groundtruth.txt (user/host names redacted; the script
#      refuses to write a capture that still contains them, an email or a token-like string)
$ cargo run -p oomtop-collect --example capture -- out.json --frames 3 --interval-ms 1500   # any OS
$ oomtop --replay fixtures/macos/m5-air-agents.json --offline   # run the whole pipeline on a recording
```

Before you commit a fixture, read it. Redaction is automatic but you are the last check: no user names, host
names, emails, tokens or paths that identify you. Note the thermal state and battery in the fixture's notes.

### Ground truth

Collector changes are compared against what the OS itself reports. The target delta is 5 % or less; say the
delta in the PR.

```console
$ uv run tools/groundtruth/groundtruth.py --top 15      # vs top/vm_stat/memory_pressure (macOS), smaps/free (Linux)
```

### Linux from a Mac

`tools/linux-vm/` runs the CI checks in a disposable Ubuntu guest (Lima, no host sudo): see
[tools/README.md](tools/README.md).

### README GIFs (VHS tapes)

Every GIF in `docs/media/` is recorded from a tape in `docs/tapes/` with [VHS](https://github.com/charmbracelet/vhs),
replaying the recorded M5 Air fixture so the result is reproducible:

```console
$ cargo build --release -p oomtop-cli                   # tapes put target/release on PATH
$ vhs docs/tapes/headroom.tape                          # run from the repository root
```

Tapes share `_setup.tape` (font: Hack Nerd Font Mono) and `_replay.tape`. If your change alters what a GIF
shows, re-record it in the same PR.

## Commits

Write commit messages like the existing history: a short lowercase scope, a colon, and what changed, in the
imperative mood.

```text
collect: read SwapPss from smaps_rollup on kernels without Pss_Anon

The 4.14 format has no Pss_Anon line; fall back to Pss - Pss_File and mark the value as an estimate.
Fixes #123.
```

- Subject line: 72 characters or fewer, no trailing period. Scopes are crate names without the prefix (`core`,
  `collect`, `tui`, `mcp`, ...) or `docs`, `ci`, `packaging`, `tools`.
- Body: what and **why**. Mention the issue it fixes.
- One logical change per commit. Formatting-only changes go in their own commit.

### Sign-off (DCO)

Every commit must be signed off under the [Developer Certificate of Origin](https://developercertificate.org/)
(DCO 1.1). The sign-off is one line at the end of the commit message:

```text
Signed-off-by: Your Name <you@example.com>
```

`git commit -s` adds it for you, using `user.name` and `user.email` from your git config. By adding it you
certify that you wrote the change, or otherwise have the right to submit it under the project's license (MIT),
and that you understand the contribution is public and recorded. That is the whole agreement: there is no CLA
to sign, and you keep your copyright.

Forgot it? `git commit --amend -s` fixes the last commit; `git rebase --signoff main` fixes every commit on your
branch. Then `git push --force-with-lease` to your fork.

## Pull requests

1. For anything bigger than a bug fix, open an issue or a Discussion first, so we agree on the shape before you
   write code. Changes to SPEC.md or UX.md follow the RFC-lite process in [GOVERNANCE.md](GOVERNANCE.md).
2. Fork, create a branch from `main`, make the change, run the checks.
3. Open the PR and fill in the template. Small PRs get reviewed faster.
4. A maintainer reviews. Every PR needs one approving review from a committer or maintainer and green CI. We
   aim to give first feedback within a week.
5. We squash or rebase-merge; the PR title becomes the commit subject, so write it in commit style.

### PR checklist

- [ ] `cargo fmt`, `clippy -D warnings` and `cargo test` pass locally.
- [ ] Commits are signed off (`git commit -s`).
- [ ] Tests cover the change (unit, golden or snapshot); changed snapshots are explained.
- [ ] **If the implementation deviates from SPEC.md or UX.md, the spec is updated in the same PR, with the
      reason.** The spec is the contract.
- [ ] Docs are in sync: a new setting is in `docs/config.md` and the JSON Schema; a new collector is in SPEC
      §5/§9; a new insight or mode is in UX §3/§5; a new CLI flag is in `docs/cli.md`.
- [ ] User-visible change? Add a line under `Unreleased` in [CHANGELOG.md](CHANGELOG.md).
- [ ] Collector change: ground-truth delta stated. Sampling/ranking change: perf numbers stated.
- [ ] UI change: snapshots updated and, if a README GIF changes, the tape re-recorded.
- [ ] No secrets, personal paths or unredacted fixtures.

## Licensing

oomtop is licensed under the [MIT License](LICENSE). Contributions are accepted under the same license (inbound =
outbound), certified by your DCO sign-off. New dependencies must pass `cargo deny check`: permissive licenses
only (see `deny.toml`); anything else needs a maintainer decision first.

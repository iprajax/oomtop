# Releasing oomtop

How a version goes from `main` to every channel users install from. The release manager (today: the
maintainer in [MAINTAINERS.md](../MAINTAINERS.md)) follows this checklist top to bottom and ticks it off in the
release tracking issue.

Channels and artifacts are described in [packaging/README.md](../packaging/README.md); the build itself is
`.github/workflows/release.yml`.

## Versioning

oomtop follows [Semantic Versioning 2.0](https://semver.org/). Tags are `vX.Y.Z` (`vX.Y.Z-rc.N` for release
candidates, which are published as GitHub pre-releases and skip the Homebrew tap). All crates in the workspace
share one version (`[workspace.package] version` in `Cargo.toml`); the release workflow refuses a tag that does
not match it.

### What semver covers

The public interface, which a minor release before 1.0 (and a major release after it) may break and a patch
release never does:

- CLI commands, flags and **exit codes** (`docs/cli.md`; `headroom` 0/3/4 are relied on by scripts)
- JSON / NDJSON output (`schema_version`; additive fields are not breaking)
- MCP tool names, inputs and outputs (`docs/mcp.md`); HTTP paths and Prometheus metric names (`serve`)
- config keys, their defaults and precedence (`docs/config.md`, the JSON Schema); theme token names
- the frozen data model in `oomtop-core` (`docs/CONTRACTS.md`)

Internal crates (`oomtop-core`, `oomtop-collect`, ...) are published so `cargo install oomtop-cli` works; their
Rust APIs are not a stable interface before 1.0.

| Change | Bump (pre-1.0) | Bump (1.0 and later) |
|---|---|---|
| Bug fix, new detection rule, new adapter, docs | patch | patch |
| New command, flag, setting, MCP tool, JSON field | minor | minor |
| Removed or renamed flag/setting/tool, changed exit code or default | minor, with a **Changed**/**Removed** entry | major |

## Checklist

### 1. Prepare (a PR to `main`)

- [ ] Open a tracking issue: "Release vX.Y.Z", with this checklist pasted in.
- [ ] `main` is green in CI (ci.yml on all three OSes, MSRV, cargo-deny, typos).
- [ ] Bump the version in `Cargo.toml`: `[workspace.package] version` **and** every internal entry under
      `[workspace.dependencies]` (`oomtop-core = { path = ..., version = "X.Y.Z" }`, ...).
- [ ] `cargo update --workspace` so `Cargo.lock` carries the new version (CI builds with `--locked`).
- [ ] CHANGELOG.md: rename `## [Unreleased]` contents into `## [X.Y.Z] - YYYY-MM-DD` (today's date, ISO 8601),
      leave an empty `## [Unreleased]` above it, and update the compare links at the bottom. The release
      workflow uses this section as the release notes (`tools/release/release-notes.sh`), so write it for users.
- [ ] Docs match the release: README install section, `docs/cli.md`, `docs/config.md` (`oomtop config init`
      output), SPEC §18/§21 status.
- [ ] Run the full local check and a ground-truth pass on a real machine; record thermal state and battery:

      ```console
      $ cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings
      $ cargo test --workspace --locked
      $ cargo deny check && typos && actionlint
      $ cargo build --release -p oomtop-cli && uv run tools/groundtruth/groundtruth.py --oomtop target/release/oomtop --top 15
      $ cargo publish --workspace --dry-run --locked       # every crate packages and builds from its .crate
      ```

- [ ] Optional dry run of the whole release build: **Actions → release → Run workflow** with the tag name (or
      `gh workflow run release.yml -f tag=vX.Y.Z` after pushing the tag to a fork). It builds every artifact and
      uploads them as a workflow artifact, without creating a release.
- [ ] Merge the release PR.

### 2. Vote (optional while there is one maintainer)

Apache projects vote on every release. oomtop does too once it has three maintainers
([GOVERNANCE.md](../GOVERNANCE.md#votes)):

- [ ] Post `[VOTE] Release oomtop X.Y.Z` in Discussions (category: Announcements or General) with the commit
      SHA, the CHANGELOG section and the dry-run artifacts. Open for at least 72 hours.
- [ ] Passes with at least 3 binding +1 and more +1 than -1. Post `[RESULT][VOTE]` with the tally.

With fewer than three maintainers, the release manager's +1 after completing step 1 is the vote; say so in the
tracking issue.

### 3. Tag (this triggers the release)

```console
$ git switch main && git pull --ff-only
$ git tag -s vX.Y.Z -m "oomtop X.Y.Z"        # -s: signed tag (or -a if you have no signing key)
$ git push origin vX.Y.Z
```

`release.yml` then: checks the tag matches the workspace version → builds Linux glibc (2.28 baseline) and musl
for x86_64 and aarch64, and the macOS universal binary (codesigned and notarized when the secrets are set) →
smoke-tests each binary → builds `.deb`/`.rpm` → writes `SHA256SUMS` → attests build provenance (SLSA, Sigstore)
→ renders the Homebrew formula → publishes the GitHub Release with notes from CHANGELOG.md → updates the tap
when `HOMEBREW_TAP_TOKEN` is configured.

- [ ] The workflow is green. If a job fails, fix it on `main` and re-run; for a broken tag, delete the release
      and tag and cut `X.Y.(Z+1)` rather than moving a published tag.

### 4. Verify what was published

On a clean machine or directory, download from the release, not from your build:

```console
$ gh release download vX.Y.Z -R iprajax/oomtop -D /tmp/oomtop-vX.Y.Z && cd /tmp/oomtop-vX.Y.Z
$ shasum -a 256 -c SHA256SUMS                                     # Linux: sha256sum -c SHA256SUMS
$ gh attestation verify oomtop-X.Y.Z-universal-apple-darwin.tar.gz --repo iprajax/oomtop
$ tar xzf oomtop-X.Y.Z-universal-apple-darwin.tar.gz && ./oomtop-X.Y.Z-universal-apple-darwin/oomtop --version
$ curl -fsSL https://github.com/iprajax/oomtop/releases/download/vX.Y.Z/install.sh | sh -s -- --to /tmp/oomtop-bin
```

- [ ] Every archive in the release checks out against `SHA256SUMS`, and `gh attestation verify` passes for at
      least one archive per OS.
- [ ] macOS: `codesign -dv --verbose=2 oomtop` shows the Developer ID, and `spctl -a -vv -t install oomtop`
      accepts it (only when signing secrets were configured; the release notes say which).
- [ ] `install.sh` installs and the binary runs `oomtop doctor`.
- [ ] The release notes read well (CHANGELOG section, install lines, verify lines, signing state).

### 5. Homebrew tap

The tap is `iprajax/homebrew-oomtop` (`brew install iprajax/oomtop/oomtop`).

- [ ] If the `homebrew-tap` job ran, check the commit in the tap. Otherwise update it by hand:

      ```console
      $ tools/release/update-tap.sh X.Y.Z --dry-run      # render from the release's SHA256SUMS, verify, brew style
      $ tools/release/update-tap.sh X.Y.Z                # commit + push Formula/oomtop.rb to the tap
      ```

- [ ] `brew update && brew install iprajax/oomtop/oomtop && oomtop --version` (and `brew test oomtop`).
- [ ] homebrew-core: only after 1.0 (it requires a stable, notable project).

### 6. crates.io

Crates are published in dependency order; a crate can only be published once the crates it depends on are on
crates.io at the same version:

```text
oomtop-core
  → oomtop-adapters, oomtop-collect, oomtop-config, oomtop-detect, oomtop-serve, oomtop-state
  → oomtop-mcp (needs adapters), oomtop-tui (needs config, state)
  → oomtop-cli (needs all of them; provides the `oomtop` binary)
```

```console
$ git switch --detach vX.Y.Z
$ cargo publish --workspace --locked            # Cargo 1.90+: publishes all crates in the order above
```

or crate by crate, `cargo publish -p <crate> --locked` in that order. crates.io rate-limits **new** crates
(a burst of a few, then roughly one every 10 minutes), so the very first release of all ten crates takes a while;
re-run the command and it skips what is already published. A published version can be yanked but never
replaced, which is why this step comes after the GitHub release is verified.

- [ ] `cargo install oomtop-cli --locked --version X.Y.Z` on a clean machine installs a working `oomtop`.

### 7. Announce

- [ ] Close the tracking issue and the milestone; move unfinished items to the next one.
- [ ] Post in Discussions → Announcements: what's new (from the CHANGELOG), how to install, how to verify.
- [ ] Update [docs/ROADMAP.md](ROADMAP.md) if the release finished a milestone.
- [ ] Other channels (AUR, Nix, distro packages) as they come online; see packaging/README.md.

## Security releases

A fix for a reported vulnerability ([SECURITY.md](../SECURITY.md)) is prepared in a private GitHub security
advisory (with a temporary private fork), released as a patch version through the same checklist (no public vote
thread; the maintainers approve in the advisory), and the advisory is published together with the release. The
CHANGELOG entry goes under **Security** and credits the reporter unless they asked not to be named.

## Secrets the release uses

| Secret | Used for | Without it |
|---|---|---|
| `MACOS_CERT_P12`, `MACOS_CERT_PASSWORD`, `MACOS_SIGN_IDENTITY` | Developer ID codesign | ad-hoc signed binary; the notes say so |
| `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD` | notarization | not notarized |
| `HOMEBREW_TAP_TOKEN` | fine-grained PAT, contents:write on `iprajax/homebrew-oomtop` | update the tap by hand (step 5) |

crates.io publishing uses the release manager's own token (`cargo login`) and is not automated.

# packaging

Everything the release workflow (`.github/workflows/release.yml`) needs to publish oomtop (SPEC §16). Pushing a
tag `v*` that matches the workspace version builds, smoke-tests, checksums, attests and **publishes** the GitHub
Release; a manual `workflow_dispatch` run is a dry run that only uploads the would-be release as a workflow
artifact.

| Channel | Source | Notes |
|---|---|---|
| GitHub Releases | `release.yml` + `tools/release/package.sh` | `oomtop-<v>-<target>.tar.gz` (dir `oomtop-<v>-<target>/` with `oomtop`, `LICENSE`, `README.md`, `docs/*.md`) for `x86_64/aarch64-unknown-linux-gnu` (glibc 2.28 baseline via `cargo-zigbuild`; NVML `dlopen`ed at runtime), `x86_64/aarch64-unknown-linux-musl` (static, no NVML), `universal-apple-darwin` (lipo of arm64 + x86_64, macOS 11+; Developer ID codesign + notarization only when the secrets below are set, otherwise the linker's ad-hoc signature and a note in the release body). Archives are reproducible (sorted, uid 0, `SOURCE_DATE_EPOCH`). |
| checksums + provenance | `release.yml` | `SHA256SUMS` covers every asset; `actions/attest-build-provenance` signs SLSA provenance for every asset (`gh attestation verify <file> --repo iprajax/oomtop`). |
| install script | `install.sh` | POSIX sh; picks gnu/musl by glibc version; downloads the archive and `SHA256SUMS`, **refuses to install on a checksum mismatch**, installs to `~/.local/bin` without sudo. `OOMTOP_BASE_URL` points it at a mirror (or `file://` for testing). Attached to every release. |
| Homebrew | `homebrew/oomtop.rb.tmpl` + `homebrew/render.py` + `tools/release/update-tap.sh` | the release job renders `oomtop.rb` from the archive checksums (fails if one is missing) and attaches it; `update-tap.sh <version>` re-renders it from the published `SHA256SUMS`, re-downloads and verifies every archive, runs `brew style`, and pushes `Formula/oomtop.rb` to the tap `iprajax/homebrew-oomtop`. The release workflow does that itself when the `HOMEBREW_TAP_TOKEN` secret exists. homebrew-core after 1.0. |
| .deb / .rpm | `[package.metadata.deb]` / `[package.metadata.generate-rpm]` in `crates/oomtop-cli/Cargo.toml` | built from the glibc binaries with `cargo deb --no-build` / `cargo generate-rpm` (explicit `glibc >= 2.28` dependency, xz payload); CI installs each into Debian 10 / Rocky 8 containers (both glibc 2.28) and runs it. |
| release notes | `tools/release/release-notes.sh` | the `CHANGELOG.md` section of the version + install / verify lines + the macOS signing state; GitHub's generated notes are added when the changelog has no section for the version. |
| crates.io | `cargo install oomtop-cli` / `cargo binstall oomtop-cli` | binary name `oomtop`; binstall downloads the release archive (`[package.metadata.binstall]`). |

The macOS archive also carries `THIRD-PARTY-NOTICES.md` (jemalloc's BSD-2-Clause notice: the macOS binary links
it as its allocator, SPEC §21). A full notice set for every Rust dependency (e.g. `cargo about`) is still to do
before 1.0.

## Release secrets (all optional)

| Secret | Used for |
|---|---|
| `MACOS_CERT_P12` | base64 of the Developer ID Application certificate (.p12) |
| `MACOS_CERT_PASSWORD` | its password |
| `MACOS_SIGN_IDENTITY` | e.g. `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD` | `xcrun notarytool submit --wait` |
| `HOMEBREW_TAP_TOKEN` | fine-grained PAT with *Contents: read and write* on `iprajax/homebrew-oomtop`; enables the `homebrew-tap` job |

Without the Apple secrets the macOS steps are skipped (not failed) and the release body says the binary is not
Developer ID signed or notarized. Homebrew and `curl | sh` installs don't set the quarantine flag, so they run
as is.

## Cut a release

```console
$ git tag -a v0.1.0 -m "oomtop 0.1.0" && git push origin v0.1.0     # release.yml runs
$ gh run watch "$(gh run list --workflow release.yml --limit 1 --json databaseId -q '.[0].databaseId')"
$ gh repo create iprajax/homebrew-oomtop --public                    # once
$ tools/release/update-tap.sh 0.1.0                                  # unless HOMEBREW_TAP_TOKEN is set
$ brew install iprajax/oomtop/oomtop && brew test oomtop
```

## Test locally

```console
$ sh -n packaging/install.sh                                      # syntax
$ OOMTOP_BASE_URL=file:///path/to/fake-release sh packaging/install.sh --version 9.9.9 --to /tmp/oomtop-bin
$ tools/release/package.sh --version 0.1.0 --target universal-apple-darwin --bin /path/to/oomtop --out dist
$ uv run packaging/homebrew/render.py --version 0.1.0 --sums SHA256SUMS --template packaging/homebrew/oomtop.rb.tmpl \
    [--url-base file:///abs/dist]                                 # file:// = local brew install test
$ tools/release/update-tap.sh 0.1.0 --sums dist/SHA256SUMS --url-base file://$PWD/dist --dry-run
$ actionlint .github/workflows/release.yml
```

`brew style` only applies the formula rules inside a tap, so `update-tap.sh` lints through a throwaway tap
directory. A local `brew install` needs a tap too: `brew tap-new --no-git you/test`, render with `--url-base
file://…` into its `Formula/`, `brew install you/test/oomtop && brew test you/test/oomtop`, then `brew untap`.

The repository is `iprajax/oomtop`; the formula, install script and Cargo metadata (`repository`/`homepage` in the
workspace `Cargo.toml`) all use it.

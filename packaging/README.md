# packaging

Everything the release workflow (`.github/workflows/release.yml`) needs to publish oomtop (SPEC §16).

| Channel | Source | Notes |
|---|---|---|
| GitHub Releases | `release.yml` | `oomtop-<v>-<target>.tar.gz` for `x86_64/aarch64-unknown-linux-gnu` (glibc 2.28 baseline via `cargo-zigbuild`; NVML `dlopen`ed at runtime), `x86_64/aarch64-unknown-linux-musl` (static, no NVML), `universal-apple-darwin` (lipo of arm64 + x86_64; Developer ID codesign + notarization when the secrets below are set). `SHA256SUMS` covers every file. |
| install script | `install.sh` | POSIX sh; picks gnu/musl by glibc version; downloads the archive and `SHA256SUMS`, **refuses to install on a checksum mismatch**, installs to `~/.local/bin` without sudo. `OOMTOP_BASE_URL` points it at a mirror (or `file://` for testing). |
| Homebrew | `homebrew/oomtop.rb.tmpl` + `homebrew/render.py` | the release job renders `oomtop.rb` from `SHA256SUMS` (fails if an artifact is missing); commit it to the tap `iprajax/homebrew-oomtop`, homebrew-core after 1.0 |
| .deb / .rpm | `[package.metadata.deb]` / `[package.metadata.generate-rpm]` in `crates/oomtop-cli/Cargo.toml` | built from the glibc binaries with `cargo deb --no-build` / `cargo generate-rpm` |
| crates.io | `cargo install oomtop-cli` | binary name `oomtop` |

The macOS archive also carries `THIRD-PARTY-NOTICES.md` (jemalloc's BSD-2-Clause notice: the macOS binary links
it as its allocator, SPEC §21). A full notice set for every Rust dependency (e.g. `cargo about`) is still to do
before 1.0.

## Release secrets (optional; unsigned builds are still produced without them)

| Secret | Used for |
|---|---|
| `MACOS_CERT_P12` | base64 of the Developer ID Application certificate (.p12) |
| `MACOS_CERT_PASSWORD` | its password |
| `MACOS_SIGN_IDENTITY` | e.g. `Developer ID Application: Name (TEAMID)` |
| `APPLE_ID`, `APPLE_TEAM_ID`, `APPLE_APP_PASSWORD` | `xcrun notarytool submit --wait` |

## Test locally

```console
$ sh -n packaging/install.sh                                      # syntax
$ OOMTOP_BASE_URL=file:///path/to/fake-release sh packaging/install.sh --version 9.9.9 --to /tmp/oomtop-bin
$ uv run packaging/homebrew/render.py --version 0.1.0 --sums SHA256SUMS --template packaging/homebrew/oomtop.rb.tmpl
```

The repository is `iprajax/oomtop`; the formula, install script and Cargo metadata (`repository`/`homepage` in the
workspace `Cargo.toml`) all use it.

# Installing oomtop

oomtop is one static binary for Linux (x86_64, aarch64) and macOS (universal). Every channel below ships the
same release binary, or builds it from the same tagged source. Pick whichever your machine already uses.

```console
$ brew install iprajax/oomtop/oomtop                                                  # macOS, Linux
$ curl -fsSL https://github.com/iprajax/oomtop/releases/latest/download/install.sh | sh   # verifies SHA-256
$ cargo binstall oomtop-cli        # prebuilt;  or: cargo install oomtop-cli --locked
$ npx oomtop                       # or: npm i -g oomtop
$ uvx oomtop                       # or: uv tool install oomtop / pipx install oomtop
$ nix run github:iprajax/oomtop
$ yay -S oomtop-bin                # Arch Linux (AUR)
$ docker run --rm --pid=host ghcr.io/iprajax/oomtop json
$ mise use -g github:iprajax/oomtop
```

Then run `oomtop doctor` to see which sources this machine can measure, and why the others can't.

## Channels and status

Status as of v0.1.0 (pre-release). **live**: works today. **after first release**: works automatically once the
`v0.1.0` GitHub Release is published. **needs owner account**: also needs a one-time registry login or secret
(see [Maintainers](#maintainers-publishing-a-release)).

| Channel | Command | Ships | Status |
|---|---|---|---|
| Source | `cargo build --release -p oomtop-cli` | build from the repo (Rust ≥ 1.88) | **live** |
| Nix flake | `nix run github:iprajax/oomtop`, `nix profile install github:iprajax/oomtop` | builds the tagged source with `rustPlatform.buildRustPackage`; `nix develop` gives the dev shell | **live** once `flake.nix` is on `main` |
| GitHub Releases | [releases page](https://github.com/iprajax/oomtop/releases) | `oomtop-<v>-<target>.tar.gz`, `.deb`, `.rpm`, `SHA256SUMS` | after first release |
| install script | `curl -fsSL …/install.sh \| sh` | picks gnu/musl, verifies SHA-256, installs to `~/.local/bin` (no sudo) | after first release |
| Homebrew | `brew install iprajax/oomtop/oomtop` | release archive (tap `iprajax/homebrew-oomtop`) | after first release |
| .deb / .rpm | `sudo apt install ./oomtop_<v>_amd64.deb`, `sudo dnf install ./oomtop-<v>.x86_64.rpm` | glibc build | after first release |
| Docker / GHCR | `docker pull ghcr.io/iprajax/oomtop` | static musl image, linux/amd64 + linux/arm64, build provenance attested | after first release (pushed on the tag) |
| mise | `mise use -g github:iprajax/oomtop` (or `ubi:iprajax/oomtop`) | release archive | after first release |
| ubi | `ubi --project iprajax/oomtop --in ~/.local/bin` | release archive; add `--matching gnu` for the NVML build | after first release |
| eget | `eget iprajax/oomtop --to ~/.local/bin` | release archive; `--asset gnu` / `--asset musl` to choose | after first release |
| cargo-binstall | `cargo binstall oomtop-cli` | release archive (`[package.metadata.binstall]`), falls back to building | after first release + crates.io |
| crates.io | `cargo install oomtop-cli --locked` | builds from source | needs owner account |
| npm | `npx oomtop`, `npm i -g oomtop` | downloads the release archive, verifies it against `SHA256SUMS` | needs owner account |
| PyPI | `uvx oomtop`, `pipx install oomtop` | platform wheels carrying the release binary | needs owner account |
| AUR | `yay -S oomtop-bin` | glibc release binary | needs owner account |
| MCP Registry | `io.github.iprajax/oomtop` | lists the npm + PyPI packages as `oomtop mcp` (stdio) | after npm + PyPI |
| aqua | needs an entry in [aquaproj/aqua-registry](https://github.com/aquaproj/aqua-registry) | release archive | not started |

Not planned for v1: Windows (no collector), Snap/Flatpak (sandboxing hides the host `/proc` and other processes'
memory, which defeats the tool). Candidates after 1.0: homebrew-core, nixpkgs, Debian/Ubuntu and Fedora
repositories, conda-forge.

### Which Linux build?

| Build | Use when | GPU |
|---|---|---|
| `*-unknown-linux-gnu` (default) | glibc ≥ 2.28: RHEL/Rocky 8+, Debian 10+, Ubuntu 18.10+, Arch, Fedora | NVIDIA via NVML (`dlopen`ed from the driver at runtime), drm fdinfo |
| `*-unknown-linux-musl` | Alpine, older glibc, containers, anywhere | drm fdinfo only. Static musl can't load NVML, so NVIDIA shows as `unavailable` |

The install script and the npm package pick gnu when glibc is new enough; `cargo binstall` tries gnu, then musl. `OOMTOP_FLAVOR=musl`
(or `--musl`) forces the static build.

### Verifying a download

Every release archive is listed in the release's `SHA256SUMS`. The install script, the npm package, the PyPI
wheel builder and the AUR/Homebrew renderers all check it and refuse to continue on a mismatch. To check a
download by hand:

```console
$ sha256sum -c --ignore-missing SHA256SUMS        # macOS: shasum -a 256 -c --ignore-missing SHA256SUMS
```

The container image carries a GitHub build-provenance attestation:
`gh attestation verify oci://ghcr.io/iprajax/oomtop:0.1.0 -R iprajax/oomtop`.

## Docker

The image is a distroless static base (nonroot, no shell) plus the static musl binary. It runs `oomtop serve` by default; any non-interactive
command works too.

```console
# Prometheus /metrics, /snapshot, /headroom, /healthz on the host's loopback only
$ docker run -d --name oomtop --pid=host -p 127.0.0.1:9469:9469 ghcr.io/iprajax/oomtop

# one snapshot / a stream
$ docker run --rm --pid=host ghcr.io/iprajax/oomtop json --compact
$ docker run --rm --pid=host ghcr.io/iprajax/oomtop ndjson --interval 5s

# full per-process PSS/SwapPss for every user's processes
$ docker run -d --pid=host --user 0 --cap-add SYS_PTRACE -p 127.0.0.1:9469:9469 ghcr.io/iprajax/oomtop
```

| Flag | Why |
|---|---|
| `--pid=host` | without it, oomtop sees only the container's own processes. Host-wide memory, swap and PSI are visible either way |
| `--user 0 --cap-add SYS_PTRACE` | needed to read `/proc/<pid>/smaps_rollup` of processes owned by other users. Without it those processes get only their world-readable files, and each value is labelled with its source |
| `-p 127.0.0.1:9469:9469` | inside the container `serve` listens on `0.0.0.0` (with a warning) so the port can be published. Keep the host side on loopback unless the network is trusted |
| `--network host` | lets the model-server adapters reach Ollama / llama.cpp / LM Studio on the host's `localhost` (otherwise pass `--offline`) |

The image is for Linux hosts. On macOS, Docker runs inside a VM, so a container would measure the VM, not the Mac.
Use the native binary there.

## Maintainers: publishing a release

`release.yml` builds everything on a `vX.Y.Z` tag, publishes the GitHub Release and then dispatches `publish.yml`,
which sends that version to crates.io, npm, PyPI, the AUR and the MCP Registry. `docker.yml` pushes the image on
the same tag. Full checklist: [RELEASING.md](RELEASING.md). A channel whose credential isn't configured is skipped with a notice. A version that's
already published is skipped, so re-running is safe. A manual run (`gh workflow run publish.yml -f tag=v0.1.0`)
is a dry run by default.

| Channel | Credential (repository settings → Secrets and variables → Actions) |
|---|---|
| crates.io | secret `CARGO_REGISTRY_TOKEN`: <https://crates.io/settings/tokens>, scopes `publish-new` + `publish-update` |
| npm | secret `NPM_TOKEN`: npmjs.com → Access Tokens → Granular, read/write. Provenance is signed via OIDC |
| PyPI | variable `PYPI_TRUSTED_PUBLISHING=true`, after adding a pending trusted publisher at <https://pypi.org/manage/account/publishing/> (project `oomtop`, owner `iprajax`, repo `oomtop`, workflow `publish.yml`, environment `pypi`) |
| AUR | secret `AUR_SSH_PRIVATE_KEY`: a key added to the AUR account (<https://aur.archlinux.org/register>) |
| MCP Registry | variable `MCP_REGISTRY_PUBLISH=true`. Auth is GitHub OIDC, no secret needed |
| GHCR | none (`GITHUB_TOKEN`). After the first push, make the package public at <https://github.com/users/iprajax/packages/container/oomtop/settings> if it isn't |

crates.io publish order (each crate's internal dependencies are published before it):

```
oomtop-core
oomtop-adapters  oomtop-collect  oomtop-config  oomtop-detect  oomtop-serve  oomtop-state   (need core)
oomtop-mcp (core, adapters)      oomtop-tui (core, config, state)
oomtop-cli (all of the above)    → binary `oomtop`
```

`cargo publish --workspace` (Cargo ≥ 1.90) does this ordering itself. `cargo publish --workspace --dry-run`
verifies all ten crates against a local overlay registry before anything is on crates.io.

Local checks for each channel's packaging, none of which publish anything:

```console
$ cargo publish --workspace --dry-run --allow-dirty
$ (cd packaging/npm && npm pack --dry-run)
$ uv run packaging/pypi/build_wheels.py --dist <dir with release archives + SHA256SUMS> --version 0.1.0 --out /tmp/wheels
$ uv run packaging/aur/render.py --version 0.1.0 --sums SHA256SUMS --out /tmp/aur
$ hadolint Dockerfile && actionlint
```

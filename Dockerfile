# syntax=docker/dockerfile:1
#
# oomtop as a small Linux image (distroless static + one static binary): `oomtop serve` (HTTP JSON + Prometheus /metrics) by default, or any
# other non-interactive command (json, ndjson, headroom, why, doctor).
#
#   docker run -d --name oomtop --pid=host -p 127.0.0.1:9469:9469 ghcr.io/iprajax/oomtop
#   docker run --rm --pid=host ghcr.io/iprajax/oomtop json --compact
#
# Host visibility: without --pid=host the container sees only its own processes. Host-wide memory (meminfo,
# PSI, swap) is always visible. Per-process PSS/SwapPss (/proc/<pid>/smaps_rollup) of processes owned by other
# users needs `--user 0 --cap-add SYS_PTRACE`; without it oomtop reads only their world-readable files and
# labels each value with its source. See docs/INSTALL.md#docker.

# ---- build: static musl binary (NVML can't be dlopen'ed from static musl; GPU shows "unavailable") ----
FROM rust:1-alpine AS build
# hadolint ignore=DL3018
RUN apk add --no-cache musl-dev
WORKDIR /src
# rust-toolchain.toml is deliberately not copied: it would make rustup download clippy/rustfmt here.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p oomtop-cli \
 && cp target/release/oomtop /oomtop \
 && /oomtop --version

# ---- runtime: distroless static (CA certs, /etc/passwd, tzdata; no shell, no package manager) ----
FROM gcr.io/distroless/static-debian12:nonroot
LABEL org.opencontainers.image.title="oomtop" \
      org.opencontainers.image.description="See the OOM coming: true memory accounting, attribution and headroom for local AI work" \
      org.opencontainers.image.source="https://github.com/iprajax/oomtop" \
      org.opencontainers.image.licenses="MIT" \
      io.modelcontextprotocol.server.name="io.github.iprajax/oomtop"
COPY --from=build /oomtop /usr/local/bin/oomtop
# Containers are ephemeral: don't build a personalization profile; state goes to $HOME/.local/state (nonroot).
ENV OOMTOP_NO_LEARN=1
EXPOSE 9469
ENTRYPOINT ["/usr/local/bin/oomtop"]
# Inside the container, listen on all interfaces so -p works; publish to 127.0.0.1 on the host (see above).
CMD ["serve", "--listen", "0.0.0.0:9469"]

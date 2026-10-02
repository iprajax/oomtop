# Roadmap

Where oomtop is, what is not done yet, and what is undecided. This is a plan, not a promise: priorities change
with what users report. The detailed milestone table is [SPEC.md §18](../SPEC.md#18-milestones); open design
questions are [SPEC.md §19](../SPEC.md#19-open-questions). Ideas go to
[Discussions](https://github.com/iprajax/oomtop/discussions/categories/ideas); spec changes follow the RFC-lite
process in [GOVERNANCE.md](../GOVERNANCE.md#rfc-lite-changing-specmd-or-uxmd).

Last reviewed: 2026-10-01.

## Where we are: 0.1.0 (pre-release)

Milestones **M0 to M8 are implemented**: true memory, attribution, headroom and OOM forecast, `why`, MCP and
`serve`, model-server and sandbox adapters, personalization, themes and the settings screen. The test suite
passes on macOS (Apple Silicon) and on Linux aarch64 and x86_64 (in a local VM). What is **not** yet true:

| Gap | Why it matters | Status |
|---|---|---|
| CI has never run on GitHub | M0's exit criterion is "green in CI" | Workflows pass locally; needs the first push |
| No published release | Homebrew, `install.sh`, `cargo install` don't work yet | Release workflow and tap tooling are ready; first tag pending |
| macOS binaries not signed or notarized | Gatekeeper prompts on download | Needs a Developer ID and the signing secrets |
| Acceptance #1 (a ~10 GB Metal model server shows ≈10 GB in its group) verified only on a synthetic snapshot | It is the motivating case | Capture an `m5-air-studio` fixture while the server runs |
| Acceptance #2 (reclaim gain within 15 % of the measured gain) not measured live | Gain estimates are labeled as estimates until then | Needs a run that actually stops the idle daemons |
| Acceptance #5 (`why` reports throttling under load, nothing when idle) shown on fixtures only | Throttle detection on real hardware | A hot fanless Mac run and an NVIDIA box |
| Real GPUs on Linux (NVML, amdgpu, i915), thermal zones/RAPL, systemd-oomd pressure | The VM has none of them | Needs contributor hardware or fixtures ([CONTRIBUTING.md](../CONTRIBUTING.md#fixtures-recorded-machines)) |
| Full third-party notice set for every Rust dependency | Required for clean binary redistribution | `cargo about` (or similar) before 1.0 |

## Next: 0.2 and on the way to 1.0 (M9)

M9 is packaging, docs and 1.0. Concretely:

1. **First release.** CI green on GitHub, `v0.1.0` tagged, archives with checksums and build provenance,
   the Homebrew tap, crates.io.
2. **Close the live acceptance gaps** above (#1, #2, #5), each with a recorded fixture so it stays covered in CI.
3. **Signed and notarized macOS builds.**
4. **More machines.** Fixtures and ground-truth runs from NVIDIA, AMD and Intel GPU boxes and Linux laptops.
5. **Lower per-process cost.** Each process still carries about 20 small allocations per refresh (owned
   `source` strings in `Measured`); moving to `Cow<'static, str>` is the next lever but changes the frozen
   model, so it needs an RFC and belongs in a minor release.
6. **Compressor-space forecast on macOS** (jetsam's "compressor space shortage"), not forecast today.
7. **Channels after the first release:** `.deb`/`.rpm` repositories, AUR, Nix; homebrew-core after 1.0.

1.0 means: the public interface in [docs/RELEASING.md](RELEASING.md#what-semver-covers) is stable, every
acceptance test in SPEC §17 has been verified on real hardware, and macOS builds are signed and notarized.

## Open questions (SPEC §19)

| # | Question | Current thinking |
|---|---|---|
| 1 | **Cross-agent reservations.** `can_fit` is advisory; a `reserve({bytes, ttl})` lease shared through `oomtop-state` would stop two agents loading at once | Experiment after the first release |
| 2 | **VM memory on macOS.** Footprint under-counts Virtualization.framework guests | Spike: configured size vs host compressor deltas vs task ports (with sudo); shown as a lower bound until then |
| 3 | **Per-process Metal split.** Can IOAccelerator user clients attribute GPU memory per pid without root? | Deferred; per-process Metal stays inside `phys_footprint`, which is correct for totals |
| 4 | **agtop NDJSON import** (tokens/cost alongside memory) | Not before 1.0; token/cost tracking itself stays a non-goal |
| 5 | **Guest agent** for per-process detail inside VMs | Not for v1 |
| - | **Private macOS APIs** (IOReport, `sandbox_check`) in distributed builds | Shipped, resolved at runtime, degrade to `unavailable`; revisit if a macOS release breaks them |

Resolved: license MIT; IOReport shipped behind a runtime check; suspend (SIGSTOP) is CPU/thermal relief only and
never counts as reclaim.

## Not planned

The v1 non-goals from SPEC §2, so nobody spends time on them by accident: token or cost tracking of cloud LLM
APIs (agtop does it), fleet or cluster monitoring (single machine only), Windows (the collector trait stays
portable), kernel modules or eBPF as a requirement, a background daemon or launch agent, and auto-killing or
auto-suspending anything without explicit confirmation.

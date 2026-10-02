# Security policy

oomtop reads every process on your machine, can signal processes on your behalf, and can be driven by AI
agents over MCP. We take reports about any of that seriously.

## Reporting a vulnerability

**Do not open a public issue, discussion or pull request for a security problem.**

Report it privately through GitHub:
**[Report a vulnerability](https://github.com/iprajax/oomtop/security/advisories/new)**
(Security tab, "Report a vulnerability"). Only the maintainers can see the report.

Please include:

- the oomtop version (`oomtop --version`) and OS (`oomtop doctor` output helps; redact anything you consider
  private),
- what an attacker can do, from what starting position (another local user, a process you run, an MCP client,
  a network peer, a crafted file),
- steps to reproduce, or a proof of concept,
- whether you have shared the issue with anyone else.

If you cannot use GitHub's private reporting, contact the maintainer listed in [MAINTAINERS.md](MAINTAINERS.md)
through GitHub and ask for a private channel; do not include the details in that first message.

## What happens next

oomtop currently has one maintainer, so these are honest targets, not an SLA:

| Step | Target |
|---|---|
| Acknowledge the report | within 7 days |
| Confirm or reject it, with a severity assessment | within 14 days |
| Fix released for a confirmed issue | within 90 days, sooner for high severity |

We work on the fix in a private GitHub security advisory, credit you in the advisory and the
[CHANGELOG](CHANGELOG.md) (unless you prefer not to be named), request a CVE through GitHub when it is
warranted, and publish the advisory when the fixed release is out. Please give us the chance to ship the fix
before you disclose publicly; we will agree on a date with you.

## Supported versions

oomtop is pre-1.0. Only the latest released minor version gets security fixes.

| Version | Supported |
|---|---|
| 0.1.x (latest) | yes |
| unreleased `main` | best effort: fixes land here first |
| anything older | no; please upgrade |

After 1.0 the policy will cover the latest minor plus the previous one for at least three months.

## Scope

In scope: anything that makes oomtop do something its documentation says it never does. In particular:

**Process actions**
- Stopping or signalling a process without the explicit confirmation described in SPEC §13 (SIGTERM first;
  SIGKILL only on a second explicit yes), or signalling a **protected** process (pid 1/launchd/systemd,
  WindowServer, loginwindow, the terminal and shell running oomtop, the calling agent's session, another user's
  process, or anything on the user's protected list).
- PID reuse: signalling a process other than the one confirmed (identity is re-verified as `pid` + start time
  right before every signal).
- Any path by which oomtop gains privileges it was not started with.

**MCP server (`oomtop mcp`)**
- Without `--allow-actions` (or `mcp.allow_actions = true`) the server is read-only. Any way to stop, suspend or
  signal a process through it without that flag is in scope.
- With `--allow-actions`, `reclaim` must only run after the **user** accepts an MCP elicitation that lists the
  exact groups and pids, must never send SIGKILL, and must refuse targets that `oomtop reclaim` would not offer
  (active apps, other agents' live sessions, the caller's own session). Bypassing any of those is in scope,
  including by prompt injection through data oomtop returns (process names, command lines, model names).

**HTTP server (`oomtop serve`)**
- `serve` binds `127.0.0.1:9469` by default and is read-only. Binding a non-loopback address without an explicit
  `--listen`, exposing actions over HTTP, or leaking unredacted data from `/api/snapshot`, `/metrics` or any
  other path is in scope. (Choosing `--listen 0.0.0.0:…` yourself publishes your process list to that network;
  oomtop warns about it, and that is by design.)

**Privacy and redaction**
- Secrets reaching any export (json, ndjson, serve, MCP, fixtures written by `tools/capture`): tokens in command
  lines (`--token=…`, `KEY=…`, URLs with credentials), environment values (only allowlisted marker keys are read,
  hashed, and never stored), user or host names in captures.
- The local profile store (SQLite) holding anything beyond what UX.md says it holds.
- Any network connection other than to local adapter endpoints on 127.0.0.1. oomtop has no telemetry.

**Input handling**
- Crashes, hangs or memory exhaustion from crafted input oomtop parses: config and theme files, imported terminal
  themes, rule files, `--replay` fixtures, GGUF headers read by `headroom --model`, `/proc` contents another user
  controls, adapter HTTP responses.

**Supply chain**
- `install.sh` installing a binary without verifying its SHA-256 against `SHA256SUMS`, release artifacts that
  don't match their checksums or build provenance, or a dependency with a known vulnerability that affects
  oomtop (we track RustSec advisories with `cargo deny` in CI).

Out of scope:

- Things the running user could already do without oomtop (stopping their own processes with `kill`, reading
  their own `/proc`).
- Extra detail visible when you choose to run oomtop with `sudo` (documented in SPEC §13; it does not widen
  actions).
- macOS private-API behavior (IOReport, `sandbox_check`) changing in a new macOS release: a functional bug, not
  a vulnerability, unless it causes one of the in-scope effects above.
- Denial of service that needs the attacker to already control the oomtop process or its config files.
- Reports from automated scanners without a demonstrated impact on oomtop.

## Verifying releases

Every release publishes `SHA256SUMS` next to the archives; `install.sh` refuses to install on a mismatch. To check
a download by hand:

```console
$ shasum -a 256 -c SHA256SUMS --ignore-missing          # Linux: sha256sum -c SHA256SUMS --ignore-missing
$ gh attestation verify oomtop-<version>-<target>.tar.gz --repo iprajax/oomtop   # when the release carries build provenance
```

See [docs/RELEASING.md](docs/RELEASING.md) for how releases are built.

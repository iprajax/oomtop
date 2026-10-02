<!--
Thanks for contributing. The PR title becomes the commit subject: "scope: what changed" (see CONTRIBUTING.md).
Security fixes: don't open a public PR, see SECURITY.md.
-->

## What and why

<!-- What this changes, and the problem it solves. Link the issue or RFC: "Fixes #123", "RFC: #45". -->

## How it was tested

<!--
Commands you ran and their real output (trimmed). For collector changes: the ground-truth delta (target ≤ 5 %).
For sampling/ranking changes: CPU % and RSS (SPEC §14). For UI changes: changed snapshots or a short recording.
-->

```console

```

## Checklist

- [ ] `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings` and
      `cargo test --workspace --locked` pass locally
- [ ] Every commit is signed off (`git commit -s`, DCO)
- [ ] Tests cover the change; every changed `.snap` file is intended and explained above
- [ ] **SPEC.md / UX.md updated in this PR if the implementation deviates from them** (with the reason), or no
      deviation
- [ ] Docs in sync: `docs/cli.md`, `docs/config.md` + JSON Schema, `docs/mcp.md`, SPEC §5/§9 (collectors),
      UX §3/§5 (insights/modes), as applicable
- [ ] CHANGELOG.md `Unreleased` entry for user-visible changes
- [ ] README GIFs re-recorded (`vhs docs/tapes/<name>.tape`) if what they show changed
- [ ] Safety/privacy unchanged, or the change is called out: confirmations, protected processes, redaction,
      loopback-only network
- [ ] No secrets, personal paths or unredacted fixtures

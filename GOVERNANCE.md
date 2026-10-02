# Governance

oomtop is run the way Apache Software Foundation projects are run, scaled down to a project that starts with one
maintainer: decisions happen in the open, merit earns responsibility, and silence on a well-announced proposal
means consent. This document says who decides what, and how. It changes through the same process it describes.

## Principles

- **Community over code.** A healthy group of people who can maintain oomtop matters more than any single
  feature.
- **Open by default.** Decisions are made in public places (issues, pull requests, Discussions), so anyone can
  see why something is the way it is. The only private channel is for security reports and conduct matters.
- **The spec is the contract.** [SPEC.md](SPEC.md) and [UX.md](UX.md) define what oomtop does. Changing what
  oomtop *is* means changing those documents first, in public.
- **Evidence over opinion.** Proposals that change measurements, attribution or performance come with
  numbers from real machines or fixtures.

## Roles

| Role | Who | Can |
|---|---|---|
| **User** | Anyone who runs oomtop | Report bugs, request features, answer questions, vote informally (+1s are welcome and counted as input) |
| **Contributor** | Anyone who has had a contribution merged (code, docs, fixtures, triage, design) | Everything a user can, plus review any PR (a contributor's review is advisory) |
| **Committer** | A contributor invited by the maintainers after sustained, quality contributions | Merge PRs after review, triage issues, cast binding votes on code changes |
| **Maintainer** | Committers who share responsibility for the project as a whole (the PMC, in ASF terms) | Everything a committer can, plus binding votes on releases, new committers/maintainers, governance and security; manage the repository, secrets and release channels |

The current committers and maintainers are listed in [MAINTAINERS.md](MAINTAINERS.md).

**Becoming a committer.** There is no fixed count of PRs. A maintainer nominates someone who has shown good
judgement over time: reviews that catch real problems, changes that respect the spec, help given to other
users. Maintainers vote privately (it is a vote about a person), the nomination passes with at least one binding
+1 and no -1 (with one maintainer: that maintainer's decision), and the result is announced publicly.

**Becoming a maintainer.** A committer who has been active for a while and takes part in releases and project
direction can be nominated the same way.

**Stepping down.** Anyone can step down at any time by saying so; they are listed as emeritus in MAINTAINERS.md
and can return by asking. A committer or maintainer inactive for 12 months is asked whether they want to
continue; if there is no answer within a month they move to emeritus.

## How decisions are made

### Lazy consensus (the default)

Most decisions are made by **lazy consensus**: someone proposes something in a PR, issue or Discussion, says
what they intend to do, and does it if nobody objects within a reasonable time. Silence is consent.

- Routine changes (bug fixes, docs, tests, refactors within a crate): a PR with one approving review from a
  committer or maintainer and green CI can be merged.
- Changes with wider effect (new dependency, new collector, new CLI command, changed default): leave the PR open
  for at least **72 hours** so others can respond.

Anyone may object. An objection needs a reason and, ideally, an alternative. Objections are discussed until
there is agreement; if there is none, the question goes to a vote.

### RFC-lite: changing SPEC.md or UX.md

Changes to what oomtop *is* go through a short public proposal before code is written:

1. Open a [Discussion](https://github.com/iprajax/oomtop/discussions) in the **Ideas** category titled
   `RFC: <short name>`. Say what changes, why, what it affects (spec sections, config, CLI, MCP, data model),
   what you considered instead, and how it will be tested.
2. Leave it open for at least **7 days**. Link it from any related issue.
3. If there is agreement (lazy consensus, or a vote if there is an objection), a maintainer marks it accepted and
   the change lands as a PR that updates SPEC.md/UX.md **in the same PR as the code** (or before it).
4. Rejected or withdrawn RFCs stay in Discussions, so the reasoning is not lost.

RFC-lite is required for: changes to the frozen public model (`oomtop-core::model`, docs/CONTRACTS.md); the
JSON/NDJSON `schema_version`; MCP tool names, inputs and outputs; HTTP paths; exit codes; config keys and their
precedence; safety rules (confirmation, protected processes, signals); privacy rules (redaction, what is stored);
the default theme; and anything listed as a non-goal in SPEC §2 (for example token/cost tracking).

Small clarifications and corrections to the spec that do not change behavior are ordinary PRs.

### Votes

When consensus cannot be reached, or for the decisions listed below, a maintainer calls a vote in a public
Discussion (or, for votes about people, privately among maintainers). Votes are open for at least **72 hours**.

| Vote | Meaning |
|---|---|
| **+1** | Yes, I agree and will help where I can |
| **0** | No opinion, or "I don't object" (+0 / -0 to lean) |
| **-1** | No. On a code change, a -1 is a **veto** and must come with a technical reason; it stands until the reason is addressed or withdrawn |

Only maintainers' votes are binding on releases and on people; committers' votes are binding on code changes.
Everyone's votes are welcome and read as input.

| Decision | Passes with |
|---|---|
| Code change in dispute | lazy consensus; a justified -1 is a veto |
| Release | at least **3 binding +1** and more +1 than -1 (Apache release vote). **While the project has fewer than three maintainers**, the release manager's own +1 after the checklist in [docs/RELEASING.md](docs/RELEASING.md) is sufficient, and the vote thread is optional |
| New committer or maintainer | at least one binding +1 and no binding -1 |
| Governance change (this file) | 2/3 of binding votes cast, at least 3 binding +1 once there are 3 maintainers |
| RFC in dispute | majority of binding votes cast |

A release vote cannot be vetoed: a -1 is a reason to stop and talk, but the majority rule above decides.

## Security and conduct

Security reports follow [SECURITY.md](SECURITY.md) and are handled privately by the maintainers until a fix is
released. Conduct reports follow the [Code of Conduct](CODE_OF_CONDUCT.md); a maintainer who is the subject of a
report takes no part in handling it.

## Trademarks and the name

"oomtop" is the project's name. Forks are welcome under the MIT License; please give a fork a different name so
users are not confused about which project they are running.

## Changes to this document

Proposed through a PR, open for at least 7 days, and approved by the governance-change vote above.

# Getting help

oomtop is maintained by volunteers. Here is where to go so your question reaches someone who can answer it.

| You want to | Go to |
|---|---|
| Ask how to do something, share a setup, or ask "is this expected?" | [Discussions: Q&A](https://github.com/iprajax/oomtop/discussions/categories/q-a) |
| Propose an idea or a change to SPEC.md / UX.md | [Discussions: Ideas](https://github.com/iprajax/oomtop/discussions/categories/ideas) (see the RFC-lite process in [GOVERNANCE.md](GOVERNANCE.md)) |
| Report a bug or a wrong number | [New bug report](https://github.com/iprajax/oomtop/issues/new?template=bug_report.yml) |
| Request a feature | [New feature request](https://github.com/iprajax/oomtop/issues/new?template=feature_request.yml) |
| Report a security problem | [Private vulnerability report](https://github.com/iprajax/oomtop/security/advisories/new), never a public issue ([SECURITY.md](SECURITY.md)) |

## Before you ask

1. **Run `oomtop doctor`.** It lists every data source as ok / partial / unavailable, with the reason, and the
   terminal capabilities it detected. Many "oomtop shows n/a" questions are answered there (for example: no
   NVIDIA driver, a container without `/proc` access, a terminal that doesn't report its colors).
2. **Check the docs:** [README](README.md), [command reference](docs/cli.md), [configuration](docs/config.md),
   [themes](docs/themes.md), [MCP for agents](docs/mcp.md).
3. **Search** [issues](https://github.com/iprajax/oomtop/issues?q=is%3Aissue) and
   [Discussions](https://github.com/iprajax/oomtop/discussions); someone may have hit it already.

## Numbers look wrong?

Compare with what your OS reports, and include both in your report:

- macOS: `top -l 1 -o mem -stats pid,mem,cmprs,command`, `vm_stat`, `sysctl vm.swapusage`, `memory_pressure`
- Linux: `free -b`, `cat /proc/meminfo`, `cat /proc/<pid>/smaps_rollup`

or, from a clone of the repository, `uv run tools/groundtruth/groundtruth.py --top 15`, which does the
comparison for you. Differences above 5 % are bugs.

## What to expect

There is no paid support. Maintainers answer when they can, usually within a week. Clear reports with
`oomtop doctor` output get answered first.

#!/usr/bin/env python3
"""Check a `/usr/bin/time` report of `oomtop ndjson --interval 2s --count N` against SPEC §14.

    /usr/bin/time -l oomtop ndjson --interval 2s --count 30 --discard > /dev/null 2> time.txt   # macOS
    /usr/bin/time -v oomtop ndjson --interval 2s --count 30 --discard > /dev/null 2> time.txt   # Linux (GNU time)
    uv run tools/groundtruth/perf_budget.py time.txt --seconds 60

Budget (SPEC §14): ≤ 1 % of one core at the 2 s default refresh, ≤ 40 MB resident. `--discard` runs the TUI's
sampling pipeline without serializing JSON (plain `ndjson` also encodes ~1–2 MB of JSON per sample, which the
TUI doesn't). CPU above the budget is a warning; the job fails only above 2× budget (shared CI runners are
noisy). Exit 1 on failure, unless --report-only.
"""

import argparse
import re
import sys
from pathlib import Path

CPU_BUDGET_PCT = 1.0
RSS_BUDGET_MB = 40.0


def parse(text: str) -> dict:
    out = {}
    # GNU time -v
    m = re.search(r"User time \(seconds\):\s*([0-9.]+)", text)
    if m:
        out["user"] = float(m.group(1))
        out["sys"] = float(re.search(r"System time \(seconds\):\s*([0-9.]+)", text).group(1))
        out["rss_mb"] = int(re.search(r"Maximum resident set size \(kbytes\):\s*(\d+)", text).group(1)) / 1024
        wall = re.search(r"Elapsed \(wall clock\) time.*?:\s*(?:(\d+):)?(\d+):([0-9.]+)", text)
        if wall:
            h, mi, s = wall.groups()
            out["wall"] = int(h or 0) * 3600 + int(mi) * 60 + float(s)
        return out
    # BSD time -l
    m = re.search(r"([0-9.]+)\s+real\s+([0-9.]+)\s+user\s+([0-9.]+)\s+sys", text)
    if m:
        out["wall"], out["user"], out["sys"] = (float(x) for x in m.groups())
        rss = re.search(r"(\d+)\s+maximum resident set size", text)
        if rss:
            out["rss_mb"] = int(rss.group(1)) / (1024 * 1024)
        return out
    raise SystemExit("unrecognized /usr/bin/time output")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("report", type=Path)
    ap.add_argument("--seconds", type=float, help="expected wall time (used when the report has none)")
    ap.add_argument("--report-only", action="store_true", help="print the numbers, never fail")
    a = ap.parse_args()
    r = parse(a.report.read_text())
    wall = r.get("wall") or a.seconds
    if not wall:
        raise SystemExit("no wall time: pass --seconds")
    cpu_pct = (r["user"] + r["sys"]) / wall * 100
    rss = r.get("rss_mb")
    print(f"wall {wall:.1f}s · cpu {cpu_pct:.2f}% of one core (budget {CPU_BUDGET_PCT}%) · "
          f"max RSS {rss:.1f} MB (budget {RSS_BUDGET_MB} MB)" if rss is not None else f"cpu {cpu_pct:.2f}%")
    fail = False
    if cpu_pct > CPU_BUDGET_PCT:
        level = "error" if cpu_pct > 2 * CPU_BUDGET_PCT else "warning"
        print(f"::{level}::CPU {cpu_pct:.2f}% exceeds the {CPU_BUDGET_PCT}% budget")
        fail |= level == "error"
    if rss is not None and rss > RSS_BUDGET_MB:
        level = "error" if rss > 2 * RSS_BUDGET_MB else "warning"
        print(f"::{level}::RSS {rss:.1f} MB exceeds the {RSS_BUDGET_MB} MB budget")
        fail |= level == "error"
    return 1 if fail and not a.report_only else 0


if __name__ == "__main__":
    sys.exit(main())

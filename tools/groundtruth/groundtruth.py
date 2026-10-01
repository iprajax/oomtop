#!/usr/bin/env python3
"""Compare oomtop's numbers with the operating system's own tools (SPEC §17). Target delta ≤ 5 %.

macOS ground truth:  sysctl hw.memsize, vm_stat, sysctl vm.swapusage, memory_pressure -Q,
                     top -l 1 -o mem -stats pid,mem,cmprs,command   (MEM = phys_footprint)
Linux ground truth:  /proc/meminfo, free -b (second reading), /proc/<pid>/smaps_rollup (Pss vs footprint_or_pss,
                     SwapPss vs swapped)

Usage (stdlib only; run with uv on the dev Mac):
    uv run tools/groundtruth/groundtruth.py                       # live: this machine
    uv run tools/groundtruth/groundtruth.py --top 40 --strict      # exit 1 if a headline delta > 5 %
    uv run tools/groundtruth/groundtruth.py --json                 # machine-readable report
    uv run tools/groundtruth/groundtruth.py --selftest             # parser checks (any OS, no oomtop needed)
    # offline, deterministic: a replayed fixture vs the tool output recorded next to it
    uv run tools/groundtruth/groundtruth.py \
        --replay fixtures/macos/m5-air-baseline.json \
        --recorded fixtures/macos/m5-air-baseline.groundtruth.txt

Numbers drift between the two reads (memory is live), so each comparison records when it was taken; the
oomtop snapshot is taken first and the tools immediately after. Thermal and battery state are printed
alongside, as the project's benchmarking rules require.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

TARGET_PCT = 5.0
ROOT = Path(__file__).resolve().parents[2]


# ----- helpers ------------------------------------------------------------------------------------------


def run(cmd: list[str], timeout: float = 20.0) -> str:
    try:
        return subprocess.run(cmd, check=True, capture_output=True, text=True, timeout=timeout).stdout
    except (OSError, subprocess.SubprocessError) as e:
        return f"__error__ {e}"


def find_oomtop(explicit: str | None) -> str:
    if explicit:
        return explicit
    for p in (ROOT / "target/release/oomtop", ROOT / "target/debug/oomtop"):
        if p.is_file() and os.access(p, os.X_OK):
            return str(p)
    for d in sorted(ROOT.glob("target-*/*/oomtop")):
        if d.is_file() and os.access(d, os.X_OK):
            return str(d)
    found = shutil.which("oomtop")
    if found:
        return found
    sys.exit("oomtop binary not found: build it (cargo build --release -p oomtop-cli) or pass --oomtop PATH")


def val(measured) -> int | float | None:
    """`Measured<T>` JSON → value (None when unavailable)."""
    if isinstance(measured, dict):
        return measured.get("value")
    return measured


def pct_delta(ours: float | None, truth: float | None) -> float | None:
    if ours is None or truth is None:
        return None
    if truth == 0:
        return 0.0 if ours == 0 else 100.0
    return (ours - truth) / truth * 100.0


def fmt_bytes(b: float | None) -> str:
    if b is None:
        return "n/a"
    b = float(b)
    for unit in ("B", "K", "M", "G", "T"):
        if abs(b) < 1024 or unit == "T":
            return f"{b:.1f}{unit}" if unit != "B" else f"{int(b)}B"
        b /= 1024
    return f"{b:.1f}T"


def parse_size(s: str) -> int | None:
    """`top`/`sysctl` sizes: "7434M", "10G", "512K", "0B", "12M+", "3072.00M" (1024-based)."""
    s = s.strip().rstrip("+-")
    m = re.fullmatch(r"([0-9]*\.?[0-9]+)\s*([BKMGT]?)", s)
    if not m:
        return None
    mul = {"": 1, "B": 1, "K": 1 << 10, "M": 1 << 20, "G": 1 << 30, "T": 1 << 40}[m.group(2)]
    return int(float(m.group(1)) * mul)


# ----- macOS ----------------------------------------------------------------------------------------------


def parse_vm_stat(text: str) -> dict:
    out: dict = {}
    m = re.search(r"page size of (\d+) bytes", text)
    out["page_size"] = int(m.group(1)) if m else 4096
    for line in text.splitlines():
        m = re.match(r'^"?([^":]+)"?:\s+(\d+)\.?$', line.strip())
        if m:
            out[m.group(1).strip()] = int(m.group(2))
    return out


def parse_swapusage(text: str) -> dict:
    out = {}
    for key in ("total", "used", "free"):
        m = re.search(rf"{key} = ([0-9.]+[KMGT]?)", text)
        if m:
            out[key] = parse_size(m.group(1))
    return out


def parse_memory_pressure(text: str) -> int | None:
    m = re.search(r"System-wide memory free percentage:\s*(\d+)%", text)
    return int(m.group(1)) if m else None


def parse_top(text: str) -> dict[int, dict]:
    """PID MEM CMPRS COMMAND rows → {pid: {mem, cmprs, command}}."""
    rows: dict[int, dict] = {}
    started = False
    for line in text.splitlines():
        parts = line.split(None, 3)
        if not parts:
            continue
        if parts[0] == "PID":
            started = True
            continue
        if not started or len(parts) < 4 or not parts[0].isdigit():
            continue
        mem, cmprs = parse_size(parts[1]), parse_size(parts[2])
        if mem is not None:
            rows[int(parts[0])] = {"mem": mem, "cmprs": cmprs, "command": parts[3].strip()}
    return rows


def split_recorded(text: str) -> dict[str, str]:
    """`## top` / `## vm_stat` / `## swapusage` / `## memory_pressure` sections of a .groundtruth.txt."""
    sections: dict[str, list[str]] = {}
    cur = None
    for line in text.splitlines():
        m = re.match(r"^## (\w+)", line)
        if m:
            cur = m.group(1)
            sections[cur] = []
        elif cur:
            sections[cur].append(line)
    return {k: "\n".join(v) for k, v in sections.items()}


def macos_truth(recorded: dict[str, str] | None, top_n: int) -> dict:
    if recorded is not None:
        vm = recorded.get("vm_stat", "")
        swap = recorded.get("swapusage", "")
        mp = recorded.get("memory_pressure", "")
        top = recorded.get("top", "")
        m = re.search(r"The system has (\d+)", mp)
        memsize = int(m.group(1)) if m else None
    else:
        memsize_s = run(["sysctl", "-n", "hw.memsize"]).strip()
        memsize = int(memsize_s) if memsize_s.isdigit() else None
        vm = run(["vm_stat"])
        swap = run(["sysctl", "vm.swapusage"])
        mp = run(["memory_pressure", "-Q"])
        top = run(["top", "-l", "1", "-o", "mem", "-n", str(top_n), "-stats", "pid,mem,cmprs,command"])
    v = parse_vm_stat(vm)
    ps = v["page_size"]
    pages = lambda k: v.get(k)  # noqa: E731
    mul = lambda k: pages(k) * ps if pages(k) is not None else None  # noqa: E731
    sw = parse_swapusage(swap)
    return {
        "memory.total": memsize,
        # vm_stat already prints free_count − speculative_count as "Pages free"
        "memory.free": mul("Pages free"),
        "memory.wired": mul("Pages wired down"),
        "memory.compressed": mul("Pages occupied by compressor"),
        "memory.compressed_logical": mul("Pages stored in compressor"),
        "memory.cached": mul("File-backed pages"),
        "memory.swap_used": sw.get("used"),
        "memory.swap_total": sw.get("total"),
        "_memorystatus_free_pct": parse_memory_pressure(mp),
        "_top": parse_top(top),
        "_sources": {
            "memory.total": "sysctl hw.memsize",
            "memory.free": "vm_stat Pages free (free − speculative)",
            "memory.wired": "vm_stat wired down",
            "memory.compressed": "vm_stat occupied by compressor",
            "memory.compressed_logical": "vm_stat stored in compressor",
            "memory.cached": "vm_stat file-backed",
            "memory.swap_used": "sysctl vm.swapusage used",
            "memory.swap_total": "sysctl vm.swapusage total",
        },
    }


# ----- Linux ----------------------------------------------------------------------------------------------


def parse_meminfo(text: str) -> dict:
    out = {}
    for line in text.splitlines():
        m = re.match(r"^(\w+(?:\(\w+\))?):\s+(\d+)(?:\s+kB)?", line)
        if m:
            out[m.group(1)] = int(m.group(2)) * 1024
    return out


def parse_free(text: str) -> dict:
    out = {}
    for line in text.splitlines():
        p = line.split()
        if p and p[0] == "Mem:" and len(p) >= 7 and all(x.isdigit() for x in p[1:7]):
            # total used free shared buff/cache available
            out.update(total=int(p[1]), free=int(p[3]), available=int(p[6]))
        elif p and p[0] == "Swap:" and len(p) >= 4 and all(x.isdigit() for x in p[1:4]):
            out.update(swap_total=int(p[1]), swap_used=int(p[2]))
    return out


def smaps_rollup(pid: int) -> dict:
    try:
        text = Path(f"/proc/{pid}/smaps_rollup").read_text()
    except OSError:
        return {}
    return parse_meminfo(text)


def linux_truth() -> dict:
    mi = parse_meminfo(Path("/proc/meminfo").read_text())
    fr = parse_free(run(["free", "-b"]))
    swap_total, swap_free = mi.get("SwapTotal"), mi.get("SwapFree")
    return linux_truth_from(Path("/proc/meminfo").read_text(), run(["free", "-b"]))


def linux_truth_from(meminfo: str, free_b: str) -> dict:
    """Pure: /proc/meminfo text + `free -b` output → truth table (keys `memory.<field>[#tool]`)."""
    mi = parse_meminfo(meminfo)
    fr = parse_free(free_b)
    swap_total, swap_free = mi.get("SwapTotal"), mi.get("SwapFree")
    truth = {
        "memory.total": mi.get("MemTotal"),
        "memory.available": mi.get("MemAvailable"),
        "memory.free": mi.get("MemFree"),
        "memory.swap_total": swap_total,
        "memory.swap_used": swap_total - swap_free if swap_total is not None and swap_free is not None else None,
        "_sources": {
            "memory.total": "/proc/meminfo MemTotal",
            "memory.available": "/proc/meminfo MemAvailable",
            "memory.free": "/proc/meminfo MemFree",
            "memory.swap_total": "/proc/meminfo SwapTotal",
            "memory.swap_used": "/proc/meminfo SwapTotal − SwapFree",
        },
    }
    # `free -b` is a second, independent reading (procps); it reads the same file a moment later.
    for field, key in (("total", "memory.total#free"), ("available", "memory.available#free"),
                       ("free", "memory.free#free")):
        if field in fr:
            truth[key] = fr[field]
            truth["_sources"][key] = f"free -b {field}"
    return truth


# ----- environment (thermal / battery) ----------------------------------------------------------------


def machine_state() -> str:
    if sys.platform == "darwin":
        batt = run(["pmset", "-g", "batt"]).strip().splitlines()
        mp = parse_memory_pressure(run(["memory_pressure", "-Q"]))
        return f"battery: {batt[-1].strip() if batt else 'n/a'} · memory free {mp}%"
    parts = []
    for ps in sorted(Path("/sys/class/power_supply").glob("*")):
        cap = ps / "capacity"
        status = ps / "status"
        if cap.exists():
            parts.append(f"{ps.name} {cap.read_text().strip()}% {status.read_text().strip() if status.exists() else ''}")
    temps = []
    for z in sorted(Path("/sys/class/thermal").glob("thermal_zone*"))[:4]:
        try:
            temps.append(f"{(z / 'type').read_text().strip()} {int((z / 'temp').read_text()) / 1000:.0f}°C")
        except (OSError, ValueError):
            pass
    return " · ".join(parts + temps) or "n/a"


# ----- comparison -----------------------------------------------------------------------------------------


def snapshot(oomtop: str, replay: str | None) -> dict:
    cmd = [oomtop, "--offline", "--no-learn"]
    if replay:
        cmd += ["--replay", replay]
    cmd += ["json", "--compact"]
    out = subprocess.run(cmd, check=True, capture_output=True, text=True, timeout=60).stdout
    return json.loads(out)


def compare(snap: dict, truth: dict, top_n: int) -> dict:
    mem = snap.get("memory", {})
    rows = []
    for key, tval in truth.items():
        if key.startswith("_"):
            continue
        field = key.split(".", 1)[1].split("#", 1)[0]
        m = mem.get(field)
        ours = val(m)
        d = pct_delta(ours, tval)
        rows.append(
            {
                "metric": key,
                "oomtop": ours,
                "oomtop_source": m.get("source") if isinstance(m, dict) else None,
                "truth": tval,
                "truth_source": truth["_sources"].get(key),
                "delta_pct": d,
                "ok": d is None or abs(d) <= TARGET_PCT,
            }
        )
    procs = {p["id"]["pid"]: p for p in snap.get("processes", [])}
    per_proc = []
    if "_top" in truth:
        for pid, t in list(truth["_top"].items())[:top_n]:
            p = procs.get(pid)
            if not p:
                continue
            ours = val(p.get("mem", {}).get("footprint_or_pss"))
            per_proc.append(
                {"pid": pid, "name": p.get("name") or t["command"], "oomtop": ours, "truth": t["mem"],
                 "delta_pct": pct_delta(ours, t["mem"])}
            )
    elif sys.platform.startswith("linux"):
        ranked = sorted(
            procs.values(),
            key=lambda p: val(p.get("mem", {}).get("footprint_or_pss")) or 0,
            reverse=True,
        )[:top_n]
        for p in ranked:
            sr = smaps_rollup(p["id"]["pid"])
            if "Pss" not in sr:
                continue
            ours = val(p["mem"].get("footprint_or_pss"))
            row = {"pid": p["id"]["pid"], "name": p.get("name"), "oomtop": ours, "truth": sr["Pss"],
                   "delta_pct": pct_delta(ours, sr["Pss"])}
            if "SwapPss" in sr:
                sw = val(p["mem"].get("swapped"))
                row.update(swap_oomtop=sw, swap_truth=sr["SwapPss"], swap_delta_pct=pct_delta(sw, sr["SwapPss"]))
            per_proc.append(row)
    deltas = [abs(r["delta_pct"]) for r in per_proc if r["delta_pct"] is not None and r["truth"] > 16 << 20]
    summary = {
        "processes_compared": len(per_proc),
        "median_abs_delta_pct": statistics.median(deltas) if deltas else None,
        "within_target": sum(1 for d in deltas if d <= TARGET_PCT),
        "large_processes": len(deltas),
    }
    extra = {}
    if truth.get("_memorystatus_free_pct") is not None:
        lvl = val(mem.get("memorystatus_level"))
        extra["memorystatus_free_pct"] = {"oomtop": lvl, "truth": truth["_memorystatus_free_pct"]}
    return {"host": rows, "processes": per_proc, "process_summary": summary, "extra": extra}


def print_report(rep: dict, state: str, snap: dict, when: str) -> None:
    h = snap.get("host", {})
    print(f"oomtop ground truth · {h.get('hostname', '?')} · {h.get('os_version', '?')} · {when}")
    print(f"state: {state}\n")
    print(f"{'metric':<28}{'oomtop':>10}{'truth':>10}{'delta':>9}  ")
    for r in rep["host"]:
        d = "n/a" if r["delta_pct"] is None else f"{r['delta_pct']:+.1f}%"
        flag = "" if r["ok"] else "  > 5 %"
        print(f"{r['metric']:<28}{fmt_bytes(r['oomtop']):>10}{fmt_bytes(r['truth']):>10}{d:>9}{flag}")
        if not r["ok"]:
            print(f"    oomtop: {r['oomtop_source']}\n    truth:  {r['truth_source']}")
    for k, v in rep["extra"].items():
        print(f"{k:<28}{str(v['oomtop']):>10}{str(v['truth']):>10}")
    s = rep["process_summary"]
    if rep["processes"]:
        print(f"\nper process (footprint / PSS), top {len(rep['processes'])}:")
        print(f"{'pid':>7}  {'name':<28}{'oomtop':>10}{'truth':>10}{'delta':>9}")
        for p in rep["processes"]:
            d = "n/a" if p["delta_pct"] is None else f"{p['delta_pct']:+.1f}%"
            swap = ""
            if p.get("swap_truth"):
                sd = "n/a" if p.get("swap_delta_pct") is None else f"{p['swap_delta_pct']:+.1f}%"
                swap = f"   swap {fmt_bytes(p.get('swap_oomtop'))} vs SwapPss {fmt_bytes(p['swap_truth'])} ({sd})"
            print(f"{p['pid']:>7}  {(p['name'] or '')[:27]:<28}{fmt_bytes(p['oomtop']):>10}{fmt_bytes(p['truth']):>10}{d:>9}{swap}")
        med = s["median_abs_delta_pct"]
        print(
            f"\nprocesses > 16 MiB within ±5 %: {s['within_target']}/{s['large_processes']}"
            f" · median |delta| {'n/a' if med is None else f'{med:.1f}%'}"
        )


# ----- self-test (runs anywhere: exercises the Linux and macOS parsers on recorded text) ----------------

_MEMINFO = """MemTotal:       16303428 kB
MemFree:         1203852 kB
MemAvailable:    9876544 kB
Buffers:          204800 kB
Cached:          7340032 kB
SwapTotal:       8388604 kB
SwapFree:        7340028 kB
"""
_FREE = """               total        used        free      shared  buff/cache   available
Mem:     16694710272  6580000000  1232744448   123456789  7725000000 10113581056
Swap:     8589930496  1073741824  7516188672
"""
_SMAPS = """55d0c0000000-7ffd00000000 ---p 00000000 00:00 0                          [rollup]
Rss:              524288 kB
Pss:              409600 kB
Swap:              20480 kB
SwapPss:           10240 kB
"""
_VM_STAT = """Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                                5000.
Pages wired down:                        100000.
Pages occupied by compressor:            200000.
Pages stored in compressor:              600000.
File-backed pages:                        50000.
"""
_TOP = """Processes: 700 total
PID    MEM    CMPRS  COMMAND
4242   10G    0B     sd-server
77     3072M+ 512M   java
"""


def selftest() -> int:
    t = linux_truth_from(_MEMINFO, _FREE)
    assert t["memory.total"] == 16303428 * 1024, t
    assert t["memory.swap_used"] == (8388604 - 7340028) * 1024
    assert t["memory.available#free"] == 10113581056 and t["memory.free#free"] == 1232744448
    assert parse_free(_FREE)["swap_used"] == 1073741824
    assert parse_free("garbage\nMem: a b c") == {}
    sr = parse_meminfo(_SMAPS)
    assert sr["Pss"] == 409600 * 1024 and sr["SwapPss"] == 10240 * 1024, sr
    snap = {"memory": {"total": {"value": 16303428 * 1024, "source": "/proc/meminfo"},
                       "available": {"value": 10113581056, "source": "/proc/meminfo"},
                       "free": {"value": None, "source": "x", "quality": {"unavailable": "test"}}}}
    rep = compare(snap, t, 10)
    by = {r["metric"]: r for r in rep["host"]}
    assert by["memory.total"]["delta_pct"] == 0.0
    assert by["memory.available#free"]["ok"] and by["memory.available#free"]["oomtop"] == 10113581056
    assert by["memory.free"]["delta_pct"] is None, "unavailable is never compared as zero"
    v = parse_vm_stat(_VM_STAT)
    assert v["page_size"] == 16384 and v["Pages free"] == 5000
    top = parse_top(_TOP)
    assert top[4242]["mem"] == 10 << 30 and top[77]["mem"] == 3072 << 20 and top[77]["cmprs"] == 512 << 20
    assert parse_size("12M+") == 12 << 20 and parse_size("nope") is None
    assert parse_swapusage("vm.swapusage: total = 2048.00M  used = 1024.50M  free = 1023.50M")["used"] == int(1024.5 * (1 << 20))
    assert pct_delta(105, 100) == 5.0 and pct_delta(None, 1) is None
    print("groundtruth selftest: ok")
    return 0


def main() -> int:
    if "--selftest" in sys.argv[1:]:
        return selftest()
    ap = argparse.ArgumentParser(description="Compare oomtop with the OS's own memory tools (target ≤ 5 %).")
    ap.add_argument("--oomtop", help="oomtop binary (default: target/release, target/debug, PATH)")
    ap.add_argument("--top", type=int, default=25, help="processes to compare (largest first)")
    ap.add_argument("--replay", help="fixture to replay instead of sampling this machine")
    ap.add_argument("--recorded", help="recorded tool output (.groundtruth.txt) matching --replay")
    ap.add_argument("--json", action="store_true", help="print the report as JSON")
    ap.add_argument("--strict", action="store_true", help="exit 1 if a host metric misses the 5 %% target")
    a = ap.parse_args()
    if bool(a.replay) != bool(a.recorded):
        ap.error("--replay and --recorded go together")

    oomtop = find_oomtop(a.oomtop)
    t0 = time.strftime("%Y-%m-%d %H:%M:%S")
    snap = snapshot(oomtop, a.replay)
    os_kind = snap.get("host", {}).get("os")
    if a.recorded:
        recorded = split_recorded(Path(a.recorded).read_text())
        truth = macos_truth(recorded, a.top) if os_kind == "macos" else sys.exit("recorded mode supports macOS fixtures")
        state = "recorded"
    elif sys.platform == "darwin":
        truth, state = macos_truth(None, a.top), machine_state()
    elif sys.platform.startswith("linux"):
        truth, state = linux_truth(), machine_state()
    else:
        sys.exit(f"unsupported platform {platform.system()}")
    rep = compare(snap, truth, a.top)
    if a.json:
        print(json.dumps({"taken_at": t0, "state": state, **rep}, indent=2, default=str))
    else:
        print_report(rep, state, snap, t0)
    bad = [r for r in rep["host"] if not r["ok"]]
    return 1 if (a.strict and bad) else 0


if __name__ == "__main__":
    sys.exit(main())

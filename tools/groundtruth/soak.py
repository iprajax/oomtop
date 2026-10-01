#!/usr/bin/env python3
"""Whole-process soak test for SPEC §14 (CPU ≤ 1 % of one core at the 2 s refresh, RSS ≤ 40 MB).

Runs the real binary for a long window — long enough for the 10-minute history ring to fill — and reports:

- CPU of the whole process (every thread: sampler, IOReport, adapters, render, JSON) from the child's exact
  rusage (`wait4`), for the whole run and for the steady state after a warm-up (from `ps` cputime deltas);
- RSS every 30 s and the maximum (`wait4` ru_maxrss).

    uv run tools/groundtruth/soak.py --seconds 660 -- target/release/oomtop ndjson --interval 2s --count 100000 --discard
    uv run tools/groundtruth/soak.py --seconds 660 --pty 120x30 -- target/release/oomtop

`--pty COLSxROWS` runs the TUI on a pseudo-terminal (output is drained and discarded; the terminal never
answers queries, the worst case for start-up). The process is stopped with `q` (TUI) or SIGTERM (headless) —
only the child this script started. Exit 1 when a budget is exceeded (unless --report-only).
"""

import argparse
import fcntl
import os
import pty
import signal
import struct
import subprocess
import sys
import termios
import time

CPU_BUDGET_PCT = 1.0
RSS_BUDGET_MB = 40.0


def ps_sample(pid: int):
    """(rss_mb, cputime_s) of `pid` from ps, or None."""
    try:
        out = subprocess.run(["ps", "-o", "rss=,time=", "-p", str(pid)], capture_output=True, text=True).stdout
    except OSError:
        return None
    parts = out.split()
    if len(parts) < 2:
        return None
    rss_mb = int(parts[0]) / 1024
    t = parts[1]  # [[dd-]hh:]mm:ss.cc
    secs = 0.0
    for p in t.replace("-", ":").split(":"):
        secs = secs * 60 + float(p)
    return rss_mb, secs


def footprint_mb(pid: int):
    """macOS physical footprint (what oomtop itself reports as memory), from vmmap; None elsewhere."""
    if sys.platform != "darwin":
        return None
    try:
        out = subprocess.run(["vmmap", "--summary", str(pid)], capture_output=True, text=True, timeout=10).stdout
    except (OSError, subprocess.TimeoutExpired):
        return None
    for line in out.splitlines():
        if line.startswith("Physical footprint:"):
            v = line.split(":", 1)[1].strip()
            num, unit = float(v[:-1]), v[-1]
            return num * {"K": 1 / 1024, "M": 1, "G": 1024}.get(unit, 1)
    return None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--seconds", type=float, default=660)
    ap.add_argument("--warmup", type=float, default=60, help="steady state starts after this many seconds")
    ap.add_argument("--interval", type=float, default=30, help="RSS sample interval")
    ap.add_argument("--pty", help="COLSxROWS: run on a pseudo-terminal (TUI)")
    ap.add_argument("--report-only", action="store_true")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd and a.cmd[0] == "--" else a.cmd
    if not cmd:
        raise SystemExit("usage: soak.py [--pty 120x30] --seconds N -- <command>")

    master = None
    if a.pty:
        cols, rows = (int(x) for x in a.pty.lower().split("x"))
        pid, master = pty.fork()
        if pid == 0:
            os.environ.setdefault("TERM", "xterm-256color")
            os.execvp(cmd[0], cmd)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.set_blocking(master, False)
    else:
        pid = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).pid

    start = time.monotonic()
    next_sample = start
    samples = []  # (t, rss_mb, cpu_s)
    tty_bytes = 0
    steady_from = None
    while True:
        now = time.monotonic()
        if master is not None:
            try:
                while True:
                    chunk = os.read(master, 65536)
                    if not chunk:
                        break
                    tty_bytes += len(chunk)
            except (BlockingIOError, OSError):
                pass
        if now >= next_sample:
            s = ps_sample(pid)
            if s is None or s[0] == 0:
                break
            samples.append((now - start, *s))
            fp = footprint_mb(pid)
            fp_txt = f"  footprint {fp:6.1f} MB" if fp is not None else ""
            print(f"t={now - start:6.0f}s  rss {s[0]:6.1f} MB{fp_txt}  cputime {s[1]:7.2f} s", flush=True)
            if steady_from is None and now - start >= a.warmup:
                steady_from = samples[-1]
            next_sample += a.interval
        if now - start >= a.seconds:
            break
        done, _ = os.waitpid(pid, os.WNOHANG)
        if done:
            break
        time.sleep(0.2)
    last = ps_sample(pid)
    if last is not None:
        samples.append((time.monotonic() - start, *last))
    # Stop only our own child: `q` for the TUI, SIGTERM otherwise. Keep draining the pty while it exits — a
    # process blocks in exit while its terminal output is unread.
    def drain():
        if master is None:
            return
        try:
            while os.read(master, 65536):
                pass
        except (BlockingIOError, OSError):
            pass

    if master is not None:
        try:
            os.write(master, b"q")
        except OSError:
            pass
    deadline = time.monotonic() + 5.0
    ru = None
    while ru is None:
        drain()
        done, _, r = os.wait4(pid, os.WNOHANG)
        if done:
            ru = r
            break
        if time.monotonic() > deadline:
            try:
                os.kill(pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            deadline = time.monotonic() + 3600
        time.sleep(0.05)
    wall = time.monotonic() - start
    maxrss_mb = ru.ru_maxrss / (1024 * 1024) if sys.platform == "darwin" else ru.ru_maxrss / 1024
    cpu_total = ru.ru_utime + ru.ru_stime

    print(f"whole run: wall {wall:.0f}s · cpu {cpu_total:.2f}s = {cpu_total / wall * 100:.2f}% of one core · "
          f"max RSS {maxrss_mb:.1f} MB")
    if master is not None:
        print(f"terminal output: {tty_bytes / 1024:.0f} KiB ({tty_bytes / wall:.0f} B/s)")
    steady = None
    if steady_from and last and last[1] and samples[-1][0] > steady_from[0]:
        t0, _, c0 = steady_from
        t1, _, c1 = samples[-1]
        steady = (c1 - c0) / (t1 - t0) * 100
        print(f"steady state t={t0:.0f}–{t1:.0f}s: {steady:.2f}% of one core")
    cpu_pct = steady if steady is not None else cpu_total / wall * 100
    fail = False
    if cpu_pct > CPU_BUDGET_PCT:
        print(f"::warning::CPU {cpu_pct:.2f}% exceeds the {CPU_BUDGET_PCT}% budget")
        fail = True
    if maxrss_mb > RSS_BUDGET_MB:
        print(f"::warning::max RSS {maxrss_mb:.1f} MB exceeds the {RSS_BUDGET_MB} MB budget")
        fail = True
    return 1 if fail and not a.report_only else 0


if __name__ == "__main__":
    sys.exit(main())

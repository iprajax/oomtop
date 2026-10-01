# /// script
# requires-python = ">=3.11"
# ///
"""Generates the synthetic Linux fixtures (formats per the kernel docs; numbers are realistic but invented).

    uv run fixtures/linux/gen_fixtures.py

Outputs (committed, so tests never need Python):
  tree-amd/                 fake filesystem root (proc/, sys/, etc/, usr/) of a Fedora laptop:
                            Ryzen 7 7840HS (16 cpus, amd-pstate), Radeon 780M APU (card0, gpu_metrics v2.2)
                            + Radeon RX 7700S dGPU (card1, gpu_metrics v1.3), systemd-oomd with Fedora drop-ins,
                            battery discharging at 41 %, platform_profile low-power, a passive trip point hit,
                            a 12 GiB user@1000.service memory.max, llama-server on the dGPU (drm fdinfo),
                            a Claude Code session (environ markers + a secret that must never leak).
  tree-amd/links.txt        symlinks/permissions git cannot carry (applied by the tests in a temp copy).
  nvidia-oom-trend.json     replay fixture (RawSample frames) of an Ubuntu workstation: RTX 4090 via NVML
                            (`@nvml` text), earlyoom -m 5 -s 10, a python training job leaking memory:
                            MemAvailable 30 → 3 GiB and swap filling over 150 s, PSI rising, then a kernel OOM
                            kill of the job (vmstat oom_kill +1, `@oom/kills`) and recovery.
"""

from __future__ import annotations

import json
import shutil
import struct
from pathlib import Path

HERE = Path(__file__).resolve().parent
KIB = 1024
MIB = 1024 * KIB
GIB = 1024 * MIB


def w(root: Path, rel: str, text: str | bytes) -> None:
    p = root / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(text, bytes):
        p.write_bytes(text)
    else:
        p.write_text(text)


def meminfo(total_kb: int, avail_kb: int, free_kb: int, cached_kb: int, anon_kb: int,
            swap_total_kb: int, swap_free_kb: int, zswap_kb: int | None = None) -> str:
    rows = [
        ("MemTotal", total_kb), ("MemFree", free_kb), ("MemAvailable", avail_kb), ("Buffers", 412_332),
        ("Cached", cached_kb), ("SwapCached", 81_220), ("Active", anon_kb // 2 + cached_kb // 2),
        ("Inactive", anon_kb // 2 + cached_kb // 2), ("Unevictable", 214_336), ("Mlocked", 24_576),
        ("SwapTotal", swap_total_kb), ("SwapFree", swap_free_kb),
    ]
    if zswap_kb is not None:
        rows += [("Zswap", zswap_kb), ("Zswapped", zswap_kb * 3)]
    rows += [("Dirty", 2_140), ("Writeback", 0), ("AnonPages", anon_kb), ("Mapped", 1_802_220),
             ("Shmem", 912_044), ("KReclaimable", 402_112), ("Slab", 812_440), ("PageTables", 98_220),
             ("CommitLimit", total_kb // 2 + swap_total_kb), ("Committed_AS", anon_kb + 4_000_000),
             ("HugePages_Total", 0), ("Hugepagesize", 2048)]
    out = []
    for k, v in rows:
        if k.startswith("HugePages_"):
            out.append(f"{k + ':':<16}{v:>8}")
        else:
            out.append(f"{k + ':':<16}{v:>8} kB")
    return "\n".join(out) + "\n"


def proc_stat(ncpu: int, busy: list[int], idle: list[int], btime: int) -> str:
    agg_u = sum(busy)
    agg_i = sum(idle)
    lines = [f"cpu  {agg_u} 120 {agg_u // 8} {agg_i} 812 0 311 0 0 0"]
    for c in range(ncpu):
        lines.append(f"cpu{c} {busy[c]} 7 {busy[c] // 8} {idle[c]} 50 0 19 0 0 0")
    lines += ["intr 812337112 0 9 0", "ctxt 1934200112", f"btime {btime}", "processes 912331",
              "procs_running 9", "procs_blocked 0", "softirq 312003 0 1 2 3 4 5 6 7 8 9"]
    return "\n".join(lines) + "\n"


def pid_stat(pid: int, comm: str, state: str, ppid: int, utime: int, stime: int, threads: int,
             starttime: int, vsize: int, rss_pages: int) -> str:
    # 52 fields per proc(5); field 22 = starttime, 23 = vsize, 24 = rss.
    f = [str(pid), f"({comm})", state, str(ppid), str(pid), str(pid), "0", "-1", "4194560", "812", "0",
         "12", "0", str(utime), str(stime), "0", "0", "20", "0", str(threads), "0", str(starttime),
         str(vsize), str(rss_pages)] + ["0"] * 28
    return " ".join(f) + "\n"


def statm(size_pages: int, resident: int, shared: int) -> str:
    return f"{size_pages} {resident} {shared} 612 0 {resident - shared} 0\n"


def status(name: str, pid: int, ppid: int, uid: int, rss_kb: int, swap_kb: int, threads: int) -> str:
    return (f"Name:\t{name}\nUmask:\t0022\nState:\tS (sleeping)\nTgid:\t{pid}\nNgid:\t0\nPid:\t{pid}\n"
            f"PPid:\t{ppid}\nTracerPid:\t0\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t{uid}\t{uid}\t{uid}\t{uid}\n"
            f"FDSize:\t256\nVmPeak:\t{rss_kb * 2} kB\nVmSize:\t{rss_kb * 2} kB\nVmHWM:\t{rss_kb} kB\n"
            f"VmRSS:\t{rss_kb} kB\nRssAnon:\t{rss_kb * 9 // 10} kB\nRssFile:\t{rss_kb // 10} kB\n"
            f"VmSwap:\t{swap_kb} kB\nThreads:\t{threads}\n")


def smaps_rollup(rss_kb: int, pss_kb: int, swap_pss_kb: int) -> str:
    return (f"55d1c0000000-7ffd1e3fe000 ---p 00000000 00:00 0                          [rollup]\n"
            f"Rss:            {rss_kb} kB\nPss:            {pss_kb} kB\nPss_Dirty:      {pss_kb * 9 // 10} kB\n"
            f"Pss_Anon:       {pss_kb * 9 // 10} kB\nPss_File:       {pss_kb // 10} kB\nPss_Shmem:      0 kB\n"
            f"Shared_Clean:   {rss_kb - pss_kb} kB\nShared_Dirty:   0 kB\nPrivate_Clean:  1024 kB\n"
            f"Private_Dirty:  {pss_kb - 1024} kB\nReferenced:     {rss_kb} kB\nAnonymous:      {pss_kb} kB\n"
            f"LazyFree:       0 kB\nAnonHugePages:  0 kB\nShmemPmdMapped: 0 kB\nFilePmdMapped:  0 kB\n"
            f"Shared_Hugetlb: 0 kB\nPrivate_Hugetlb: 0 kB\nSwap:           {swap_pss_kb} kB\n"
            f"SwapPss:        {swap_pss_kb} kB\nLocked:         0 kB\n")


def io(rb: int, wb: int) -> str:
    return (f"rchar: {rb * 3}\nwchar: {wb * 2}\nsyscr: 81233\nsyscw: 12001\nread_bytes: {rb}\n"
            f"write_bytes: {wb}\ncancelled_write_bytes: 0\n")


def psi(some10: float, some60: float, some300: float, full10: float | None, full60: float = 0.0,
        full300: float = 0.0, total: int = 0) -> str:
    s = f"some avg10={some10:.2f} avg60={some60:.2f} avg300={some300:.2f} total={total}\n"
    if full10 is not None:
        s += f"full avg10={full10:.2f} avg60={full60:.2f} avg300={full300:.2f} total={total // 3}\n"
    return s


def vmstat(pswpin: int, pswpout: int, oom_kill: int) -> str:
    return (f"nr_free_pages 812331\nnr_zone_inactive_anon 31220\nnr_zone_active_anon 912331\n"
            f"pgpgin 81233112\npgpgout 12003311\npswpin {pswpin}\npswpout {pswpout}\npgfault 912331122\n"
            f"pgmajfault 81233\noom_kill {oom_kill}\n")


def fdinfo(driver: str, client: int, pdev: str, vram_kib: int, gtt_kib: int, gfx_ns: int) -> str:
    return (f"pos:\t0\nflags:\t02100002\nmnt_id:\t26\nino:\t1073\ndrm-driver:\t{driver}\n"
            f"drm-client-id:\t{client}\ndrm-pdev:\t{pdev}\npasid:\t32790\n"
            f"drm-memory-vram:\t{vram_kib} KiB\ndrm-memory-gtt:\t{gtt_kib} KiB\ndrm-memory-cpu:\t0 KiB\n"
            f"drm-total-vram:\t{vram_kib + 4096} KiB\ndrm-resident-vram:\t{vram_kib} KiB\n"
            f"drm-total-gtt:\t{gtt_kib} KiB\ndrm-resident-gtt:\t{gtt_kib} KiB\n"
            f"drm-engine-gfx:\t{gfx_ns} ns\ndrm-engine-compute:\t0 ns\ndrm-engine-dma:\t81233 ns\n")


def gpu_metrics_v1_3(edge: int, hotspot: int, activity: int, power_w: int, gfxclk: int, throttle: int) -> bytes:
    # struct gpu_metrics_v1_3 (kgd_pp_interface.h): sizeof = 120; offsets checked with offsetof().
    b = bytearray(120)
    struct.pack_into("<HBB", b, 0, 120, 1, 3)
    struct.pack_into("<6H", b, 4, edge, hotspot, 0xFFFF, 60, 55, 58)
    struct.pack_into("<3H", b, 16, activity, 40, 0)
    struct.pack_into("<H", b, 22, power_w)
    struct.pack_into("<Q", b, 24, 81233112)  # energy accumulator
    struct.pack_into("<Q", b, 32, 912331122)  # system clock counter
    struct.pack_into("<7H", b, 40, gfxclk - 50, 1200, 1000, 0, 0, 0, 0)
    struct.pack_into("<7H", b, 54, gfxclk, 1200, 1000, 0, 0, 0, 0)
    struct.pack_into("<I", b, 68, throttle)
    return bytes(b)


def gpu_metrics_v2_2(gfx_centi: int, activity_centi: int, socket_mw: int, gfxclk: int) -> bytes:
    # struct gpu_metrics_v2_2: sizeof = 128. Since v2.1 the timestamp follows the activity fields:
    # temperature_gfx @4, temperature_soc @6, average_gfx_activity @28 (centi-%), system_clock_counter @32,
    # average_socket_power @40 (mW), average clocks @64, current clocks @76, throttle_status @108.
    b = bytearray(128)
    struct.pack_into("<HBB", b, 0, 128, 2, 2)
    struct.pack_into("<H", b, 4, gfx_centi)
    struct.pack_into("<H", b, 6, gfx_centi - 150)
    struct.pack_into("<H", b, 28, activity_centi)
    struct.pack_into("<H", b, 30, 0)
    struct.pack_into("<Q", b, 32, 912331122)
    struct.pack_into("<H", b, 40, socket_mw)
    struct.pack_into("<6H", b, 64, gfxclk, 1400, 2800, 1600, 0, 0)
    struct.pack_into("<6H", b, 76, gfxclk, 1400, 2800, 1600, 0, 0)
    struct.pack_into("<I", b, 108, 0)
    struct.pack_into("<Q", b, 120, 0)  # indep_throttle_status
    return bytes(b)


# ---------------------------------------------------------------------------------------------------------
# tree-amd
# ---------------------------------------------------------------------------------------------------------

def gen_tree() -> None:
    root = HERE / "tree-amd"
    if root.exists():
        shutil.rmtree(root)
    btime = 1_790_000_000
    ncpu = 16
    # proc (host)
    w(root, "proc/meminfo", meminfo(31_998_332, 9_812_440, 1_012_332, 7_812_004, 17_023_112,
                                    8_388_604, 5_120_332, zswap_kb=812_332))
    w(root, "proc/vmstat", vmstat(812_331, 1_912_004, 0))
    w(root, "proc/stat", proc_stat(ncpu, [81_000 + c * 10 for c in range(ncpu)], [900_000] * ncpu, btime))
    w(root, "proc/loadavg", "11.52 9.80 6.12 13/1834 91233\n")
    w(root, "proc/pressure/memory", psi(12.40, 8.22, 3.10, 4.02, 2.51, 0.80, 81_233_112))
    w(root, "proc/pressure/cpu", psi(35.10, 30.00, 22.00, 0.00, total=912_331_122))
    w(root, "proc/pressure/io", psi(1.20, 0.80, 0.40, 0.90, 0.50, 0.20, 1_233_112))
    w(root, "proc/sys/kernel/osrelease", "6.11.4-301.fc41.x86_64\n")
    w(root, "proc/sys/kernel/hostname", "fw16\n")
    own_cg = "/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.gnome.Terminal.slice/vte-spawn-1.scope"
    w(root, "proc/self/cgroup", f"0::{own_cg}\n")

    procs = [
        # pid, comm, ppid, uid, starttime ticks, rss_kb, shared_kb, pss_kb, swap_kb, oom_score, cmdline, cg
        (1, "systemd", 0, 0, 1, 18_220, 9_000, 9_900, 0, 0, ["/usr/lib/systemd/systemd", "--switched-root"], "/init.scope"),
        (2, "kthreadd", 0, 0, 1, 0, 0, 0, 0, 0, [], "/"),
        (57, "kworker/3:1-events", 2, 0, 20, 0, 0, 0, 0, 0, [], "/"),
        (812, "systemd-oomd", 1, 991, 300, 6_120, 4_000, 2_400, 0, 0, ["/usr/lib/systemd/systemd-oomd"], "/system.slice/systemd-oomd.service"),
        (1500, "gnome-shell", 1480, 1000, 2_000, 412_332, 120_000, 330_120, 12_000, 677, ["/usr/bin/gnome-shell"], "/user.slice/user-1000.slice/user@1000.service/session.slice/org.gnome.Shell@wayland.service"),
        (2100, "claude", 2050, 1000, 90_000, 382_112, 40_000, 351_220, 0, 690, ["claude", "--resume"], own_cg),
        (2200, "llama-server", 2100, 1000, 95_000, 2_812_332, 1_900_000, 1_402_112, 0, 712, ["/usr/bin/llama-server", "-m", "/home/dev/models/qwen2.5-coder-14b-q4_k_m.gguf", "-ngl", "99", "--port", "8080", "--api-key", "sk-live-DO-NOT-LEAK"], own_cg),
        (2300, "python3", 2100, 1000, 97_000, 1_312_004, 200_000, 1_150_332, 40_000, 705, ["python3", "main.py", "--listen", "127.0.0.1"], own_cg),
        (2400, "java", 1, 1000, 60_000, 3_120_332, 80_000, 3_050_112, 210_000, 740, ["/usr/lib/jvm/java-21/bin/java", "-Xmx4g", "org.gradle.launcher.daemon.bootstrap.GradleDaemon", "8.10"], "/user.slice/user-1000.slice/user@1000.service/app.slice/gradle.scope"),
    ]
    passwd = ["root:x:0:0:root:/root:/bin/bash", "systemd-oom:x:991:991:systemd Userspace OOM Killer:/:/usr/sbin/nologin",
              "dev:x:1000:1000:Dev:/home/dev:/bin/zsh"]
    w(root, "etc/passwd", "\n".join(passwd) + "\n")
    links = []
    for pid, comm, ppid, uid, st, rss, shared, pss, swap, score, argv, cg in procs:
        d = f"proc/{pid}"
        pages = rss // 4
        w(root, f"{d}/stat", pid_stat(pid, comm, "S" if pid != 2200 else "R", ppid, 81_000 + pid, 9_000, 12,
                                      st, rss * 2048, pages))
        w(root, f"{d}/statm", statm(pages * 2, pages, shared // 4))
        w(root, f"{d}/status", status(comm[:15], pid, ppid, uid, rss, swap, 12))
        w(root, f"{d}/cmdline", "\0".join(argv) + ("\0" if argv else ""))
        w(root, f"{d}/cgroup", f"0::{cg}\n")
        w(root, f"{d}/oom_score", f"{score}\n")
        if rss:
            w(root, f"{d}/io", io(81_233_112 + pid, 12_331_004 + pid))
            w(root, f"{d}/smaps_rollup", smaps_rollup(rss, pss, swap))
            w(root, f"{d}/environ", f"PATH=/usr/bin\0HOME=/home/dev\0LANG=C.UTF-8\0")
        (root / f"{d}/fd").mkdir(parents=True, exist_ok=True)
        (root / f"{d}/fdinfo").mkdir(parents=True, exist_ok=True)
        if argv:
            links.append(f"link {d}/exe {argv[0] if argv[0].startswith('/') else '/usr/bin/' + argv[0]}")
            links.append(f"link {d}/cwd /home/dev/project")
            links.append(f"link {d}/fd/0 /dev/null")
    # Claude Code session: allowlisted markers + a secret that must never be stored.
    w(root, "proc/2100/environ", "CLAUDECODE=1\0CLAUDE_CODE_SESSION_ID=3b6f0c1e-4d2a-4c55-9d1e-8f7a2b1c0d9e\0"
                                 "CLAUDE_CODE_MESSAGING_TOKEN=tok-DO-NOT-LEAK\0PATH=/usr/bin\0")
    w(root, "proc/2200/environ", "CLAUDECODE=1\0CLAUDE_CODE_SESSION_ID=3b6f0c1e-4d2a-4c55-9d1e-8f7a2b1c0d9e\0HOME=/home/dev\0")
    # llama-server on the dGPU (card1 = renderD129): 7.5 GiB VRAM; the gguf is mapped and open.
    links += ["link proc/2200/fd/5 /dev/dri/renderD129", "link proc/2200/fd/6 /dev/dri/renderD129",
              "link proc/2200/fd/7 /home/dev/models/qwen2.5-coder-14b-q4_k_m.gguf"]
    w(root, "proc/2200/fdinfo/5", fdinfo("amdgpu", 17, "0000:03:00.0", 7_864_320, 131_072, 912_331_122_000))
    # fd 6 is a dup of the same DRM client (same client id) → must not double count.
    w(root, "proc/2200/fdinfo/6", fdinfo("amdgpu", 17, "0000:03:00.0", 7_864_320, 131_072, 912_331_122_000))
    w(root, "proc/2200/fdinfo/7", "pos:\t0\nflags:\t0100000\nmnt_id:\t31\nino:\t912331\n")
    w(root, "proc/2200/maps",
      "5581c0000000-5581c0200000 r-xp 00000000 103:02 912331 /usr/bin/llama-server\n"
      "7f1c00000000-7f1f40000000 r--s 00000000 103:02 812331 /home/dev/models/qwen2.5-coder-14b-q4_k_m.gguf\n"
      "7f1f40000000-7f1f40200000 rw-p 00000000 00:00 0 \n"
      "7ffd1e3dd000-7ffd1e3fe000 rw-p 00000000 00:00 0                          [stack]\n")
    # ComfyUI-like python on the APU (card0 = renderD128): VRAM carve-out + GTT.
    links += ["link proc/2300/fd/9 /dev/dri/renderD128"]
    w(root, "proc/2300/fdinfo/9", fdinfo("amdgpu", 21, "0000:c4:00.0", 409_600, 3_145_728, 81_233_112_000))

    # cgroup v2
    cg = "sys/fs/cgroup"
    w(root, f"{cg}/cgroup.controllers", "cpuset cpu io memory hugetlb pids rdma misc\n")
    for rel, mx, cur, ev in [
        ("user.slice", "max", 15_032_385_536, "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n"),
        ("user.slice/user-1000.slice", "max", 14_812_332_032, "low 0\nhigh 0\nmax 0\noom 0\noom_kill 0\noom_group_kill 0\n"),
        ("user.slice/user-1000.slice/user@1000.service", "12884901888", 11_811_160_064,
         "low 0\nhigh 0\nmax 12\noom 1\noom_kill 1\noom_group_kill 0\n"),
    ]:
        w(root, f"{cg}/{rel}/memory.max", f"{mx}\n")
        w(root, f"{cg}/{rel}/memory.high", "max\n")
        w(root, f"{cg}/{rel}/memory.swap.max", "max\n")
        w(root, f"{cg}/{rel}/memory.current", f"{cur}\n")
        w(root, f"{cg}/{rel}/memory.swap.current", "412331008\n")
        w(root, f"{cg}/{rel}/memory.events", ev)

    # cpufreq: amd-pstate-epp, one policy per cpu, low-power profile keeps clocks at ~1.4 of 5.14 GHz.
    for c in range(ncpu):
        d = f"sys/devices/system/cpu/cpufreq/policy{c}"
        w(root, f"{d}/cpuinfo_max_freq", "5137000\n")
        w(root, f"{d}/cpuinfo_min_freq", "400000\n")
        w(root, f"{d}/scaling_max_freq", "5137000\n")
        w(root, f"{d}/scaling_cur_freq", f"{1_400_000 + c * 10_000}\n")
        w(root, f"{d}/related_cpus", f"{c}\n")
        w(root, f"{d}/scaling_driver", "amd-pstate-epp\n")

    # thermal zones: acpitz over its passive trip (throttling), a disabled wifi zone.
    w(root, "sys/class/thermal/thermal_zone0/type", "acpitz\n")
    w(root, "sys/class/thermal/thermal_zone0/mode", "enabled\n")
    w(root, "sys/class/thermal/thermal_zone0/temp", "88000\n")
    for i, (ty, t) in enumerate([("critical", 110000), ("hot", 100000), ("passive", 85000), ("active", 60000)]):
        w(root, f"sys/class/thermal/thermal_zone0/trip_point_{i}_type", f"{ty}\n")
        w(root, f"sys/class/thermal/thermal_zone0/trip_point_{i}_temp", f"{t}\n")
    w(root, "sys/class/thermal/thermal_zone1/type", "iwlwifi_1\n")
    w(root, "sys/class/thermal/thermal_zone1/mode", "disabled\n")
    w(root, "sys/class/thermal/thermal_zone1/temp", "45000\n")
    w(root, "sys/class/thermal/cooling_device0/type", "Processor\n")

    # hwmon
    hw = "sys/class/hwmon"
    w(root, f"{hw}/hwmon0/name", "k10temp\n")
    w(root, f"{hw}/hwmon0/temp1_input", "91500\n")
    w(root, f"{hw}/hwmon0/temp1_label", "Tctl\n")
    w(root, f"{hw}/hwmon1/name", "nvme\n")
    w(root, f"{hw}/hwmon1/temp1_input", "45850\n")
    w(root, f"{hw}/hwmon1/temp1_label", "Composite\n")
    w(root, f"{hw}/hwmon1/temp1_crit", "84850\n")
    w(root, f"{hw}/hwmon2/name", "amdgpu\n")
    w(root, f"{hw}/hwmon2/temp1_input", "70000\n")
    w(root, f"{hw}/hwmon2/temp1_label", "edge\n")
    w(root, f"{hw}/hwmon2/power1_input", "18500000\n")
    w(root, f"{hw}/hwmon3/name", "amdgpu\n")
    w(root, f"{hw}/hwmon3/temp1_input", "78000\n")
    w(root, f"{hw}/hwmon3/temp1_label", "edge\n")
    w(root, f"{hw}/hwmon3/temp1_crit", "100000\n")
    w(root, f"{hw}/hwmon3/temp2_input", "95000\n")
    w(root, f"{hw}/hwmon3/temp2_label", "junction\n")
    w(root, f"{hw}/hwmon3/temp2_crit", "110000\n")
    w(root, f"{hw}/hwmon3/power1_average", "95000000\n")
    w(root, f"{hw}/hwmon3/freq1_input", "1800000000\n")
    w(root, f"{hw}/hwmon3/freq1_label", "sclk\n")

    # power supply: laptop battery discharging, AC offline, a mouse battery that must be ignored.
    ps = "sys/class/power_supply"
    w(root, f"{ps}/BAT1/type", "Battery\n")
    w(root, f"{ps}/BAT1/status", "Discharging\n")
    w(root, f"{ps}/BAT1/capacity", "41\n")
    w(root, f"{ps}/BAT1/power_now", "38120000\n")
    w(root, f"{ps}/ACAD/type", "Mains\n")
    w(root, f"{ps}/ACAD/online", "0\n")
    w(root, f"{ps}/hidpp_battery_0/type", "Battery\n")
    w(root, f"{ps}/hidpp_battery_0/scope", "Device\n")
    w(root, f"{ps}/hidpp_battery_0/capacity", "90\n")
    w(root, f"{ps}/hidpp_battery_0/status", "Discharging\n")

    # powercap: RAPL package zone; energy_uj root-only (chmod 000 applied via links.txt).
    pc = "sys/class/powercap"
    w(root, f"{pc}/intel-rapl:0/name", "package-0\n")
    w(root, f"{pc}/intel-rapl:0/max_energy_range_uj", "65532610987\n")
    w(root, f"{pc}/intel-rapl:0/energy_uj", "81233112233\n")
    w(root, f"{pc}/intel-rapl:0:0/name", "core\n")
    w(root, "sys/firmware/acpi/platform_profile", "low-power\n")
    w(root, "sys/firmware/acpi/platform_profile_choices", "low-power balanced performance\n")

    # DRM: card0 = 780M APU, card1 = RX 7700S dGPU; connectors and render nodes must be ignored.
    drm = "sys/class/drm"
    w(root, f"{drm}/card0/device/vendor", "0x1002\n")
    w(root, f"{drm}/card0/device/device", "0x15bf\n")
    w(root, f"{drm}/card0/device/uevent", "DRIVER=amdgpu\nPCI_CLASS=30000\nPCI_ID=1002:15BF\nPCI_SLOT_NAME=0000:c4:00.0\n")
    w(root, f"{drm}/card0/device/mem_info_vram_total", f"{512 * MIB}\n")
    w(root, f"{drm}/card0/device/mem_info_vram_used", f"{430 * MIB}\n")
    w(root, f"{drm}/card0/device/mem_info_gtt_total", f"{12 * GIB}\n")
    w(root, f"{drm}/card0/device/mem_info_gtt_used", f"{3 * GIB}\n")
    w(root, f"{drm}/card0/device/gpu_busy_percent", "12\n")
    w(root, f"{drm}/card0/device/pp_dpm_sclk", "0: 800Mhz\n1: 1600Mhz *\n2: 2700Mhz\n")
    w(root, f"{drm}/card0/device/gpu_metrics", gpu_metrics_v2_2(7_012, 1_200, 18_500, 1_600))
    w(root, f"{drm}/card0/device/hwmon/hwmon2/name", "amdgpu\n")
    w(root, f"{drm}/card1/device/vendor", "0x1002\n")
    w(root, f"{drm}/card1/device/device", "0x7480\n")
    w(root, f"{drm}/card1/device/product_name", "AMD Radeon RX 7700S\n")
    w(root, f"{drm}/card1/device/uevent", "DRIVER=amdgpu\nPCI_CLASS=30000\nPCI_ID=1002:7480\nPCI_SLOT_NAME=0000:03:00.0\n")
    w(root, f"{drm}/card1/device/mem_info_vram_total", f"{8 * GIB}\n")
    w(root, f"{drm}/card1/device/mem_info_vram_used", f"{7_900 * MIB}\n")
    w(root, f"{drm}/card1/device/mem_info_gtt_total", f"{15 * GIB}\n")
    w(root, f"{drm}/card1/device/gpu_busy_percent", "99\n")
    w(root, f"{drm}/card1/device/pp_dpm_sclk", "0: 500Mhz\n1: 1800Mhz *\n2: 2500Mhz\n")
    w(root, f"{drm}/card1/device/gpu_metrics", gpu_metrics_v1_3(78, 95, 99, 95, 1_800, 0x4))
    w(root, f"{drm}/card1/device/hwmon/hwmon3/name", "amdgpu\n")
    w(root, f"{drm}/card1-eDP-1/status", "connected\n")
    w(root, f"{drm}/renderD128/dev", "226:128\n")

    # systemd-oomd (Fedora defaults) + a local override of the swap limit.
    w(root, "etc/systemd/oomd.conf", "#  This file is part of systemd.\n[OOM]\n#SwapUsedLimit=90%\n"
                                     "#DefaultMemoryPressureLimit=60%\n#DefaultMemoryPressureDurationSec=30s\n")
    w(root, "etc/systemd/oomd.conf.d/50-local.conf", "[OOM]\nSwapUsedLimit=80%\n")
    w(root, "usr/lib/systemd/system/-.slice.d/10-oomd-root-slice-defaults.conf",
      "# Fedora default\n[Slice]\nManagedOOMSwap=kill\n")
    w(root, "usr/lib/systemd/system/user@.service.d/10-oomd-user-service-defaults.conf",
      "[Service]\nManagedOOMMemoryPressure=kill\nManagedOOMMemoryPressureLimit=50%\n")

    links.append("chmod000 sys/class/powercap/intel-rapl:0/energy_uj")
    w(root, "links.txt", "# Applied by the tests to a temp copy: `link <path> <target>` | `chmod000 <path>`\n"
      + "\n".join(links) + "\n")


# ---------------------------------------------------------------------------------------------------------
# nvidia-oom-trend.json
# ---------------------------------------------------------------------------------------------------------

def gen_nvidia() -> None:
    t0 = 1_790_000_000_000
    btime = 1_789_990_000
    total_kb = 65_773_040  # 64 GiB box
    swap_kb = 8_388_604
    ncpu = 16
    frames = []
    n = 16
    for i in range(n):
        t = t0 + i * 10_000
        killed = i == n - 1
        if not killed:
            avail = int((30 - 1.8 * i) * GIB / KIB)
            swap_used = int((0.5 + 0.46 * i) * GIB / KIB)
            leak_rss = int((30 + 1.5 * i) * GIB / KIB)
            some10, full10 = 1.0 + 3.1 * i, 0.2 + 1.7 * i
        else:
            avail = int(41 * GIB / KIB)
            swap_used = int(2.1 * GIB / KIB)
            leak_rss = 0
            some10, full10 = 30.0, 8.0
        busy = [1_000_000 + i * 900 * (1 if c < 12 else 0) for c in range(ncpu)]
        idle = [5_000_000 + i * (100 if c < 12 else 1000) for c in range(ncpu)]
        files = {
            "meminfo": meminfo(total_kb, avail, max(avail // 8, 200_000), max(avail // 2, 400_000),
                               total_kb - avail - 2_000_000, swap_kb, swap_kb - swap_used, zswap_kb=None),
            "vmstat": vmstat(120_000 + i * 2_000, 300_000 + i * 60_000, 1 if killed else 0),
            "stat": proc_stat(ncpu, busy, idle, btime),
            "loadavg": f"{14 + i * 0.3:.2f} 12.10 9.33 17/912 {40000 + i}\n",
            "pressure/memory": psi(some10, some10 * 0.7, some10 * 0.3, full10, full10 * 0.6, full10 * 0.2, 1000 * i),
            "pressure/cpu": psi(12.0, 10.0, 8.0, 0.0, total=9_000 * i),
            "pressure/io": psi(8.0 + i, 5.0, 2.0, 6.0 + i * 0.5, 3.0, 1.0, 3_000 * i),
            "sys/kernel/osrelease": "6.8.0-45-generic\n",
            "sys/kernel/hostname": "<host>\n",
            "driver/nvidia/version": "NVRM version: NVIDIA UNIX x86_64 Kernel Module  550.107.02\n",
            "self/cgroup": "0::/user.slice/user-1000.slice/session-3.scope\n",
            "/sys/fs/cgroup/cgroup.controllers": "cpuset cpu io memory hugetlb pids rdma misc\n",
            "/sys/fs/cgroup/user.slice/memory.max": "max\n",
            "/sys/fs/cgroup/user.slice/memory.events": f"low 0\nhigh 0\nmax 0\noom 0\noom_kill {1 if killed else 0}\n",
            "/sys/fs/cgroup/user.slice/memory.current": f"{(total_kb - avail) * KIB}\n",
            "/sys/fs/cgroup/user.slice/user-1000.slice/memory.max": "max\n",
            "/sys/fs/cgroup/user.slice/user-1000.slice/session-3.scope/memory.max": "max\n",
            "/sys/class/hwmon/hwmon0/name": "k10temp\n",
            "/sys/class/hwmon/hwmon0/temp1_input": f"{78000 + i * 250}\n",
            "/sys/class/hwmon/hwmon0/temp1_label": "Tctl\n",
            "/sys/class/drm/card0/device/vendor": "0x10de\n",
            "/sys/class/drm/card0/device/device": "0x2684\n",
            "/sys/class/drm/card0/device/uevent": "DRIVER=nvidia\nPCI_CLASS=30000\nPCI_ID=10DE:2684\nPCI_SLOT_NAME=0000:01:00.0\n",
            "@nvml": (
                "driver: 550.107.02\ndevice 0\nname: NVIDIA GeForce RTX 4090\npci: 00000000:01:00.0\n"
                f"mem_total: {25_757_220_864}\nmem_used: {22_548_578_304}\nutil_gpu: 97\npower_mw: {441_000 + i * 100}\n"
                f"power_limit_mw: 450000\ntemp_c: {80 + i // 5}\nclock_sm_mhz: {2_235 - i * 5}\nmax_clock_sm_mhz: 3105\n"
                "throttle_reasons: 0x4\n"
            ),
            "@oom/daemons": "earlyoom\t640\n",
            "640/cmdline": "/usr/bin/earlyoom\0-r\0003600\0-m\0005\0-s\00010\0--avoid\0(^|/)(init|systemd|Xorg|sshd)$\0",
        }
        for c in range(ncpu):
            files[f"/sys/devices/system/cpu/cpufreq/policy{c}/scaling_cur_freq"] = f"{5_200_000 - i * 20_000}\n"
            files[f"/sys/devices/system/cpu/cpufreq/policy{c}/cpuinfo_max_freq"] = "5453000\n"
            files[f"/sys/devices/system/cpu/cpufreq/policy{c}/related_cpus"] = f"{c}\n"
        if not killed:
            files["@oom/scan"] = (f"3100\t812331\t{600 + i * 20}\tpython3\n2900\t712331\t{380 - i}\tllama-server\n"
                                  "3300\t912331\t12\tbash\n")
        else:
            files["@oom/scan"] = "2900\t712331\t366\tllama-server\n3300\t912331\t12\tbash\n"
            files["@oom/kills"] = (f"{t}\t3100\tpython3\t/proc/vmstat:oom_kill +1; memory.events:oom_kill +1 "
                                   "(/user.slice); victim = top oom_score candidate that exited\n")
        host = {"source": "linux.host", "taken_at_ms": t, "read_us": 1_800 + i * 3,
                "payload": {"type": "linux_files", "files": files,
                            "missing": {"/sys/class/powercap/intel-rapl:0/energy_uj": "permission denied"},
                            "markers": {}, "clk_tck": 100, "page_size": 4096, "truncated": False}}
        pf: dict[str, str] = {"stat": files["stat"]}
        procs = [
            (1, "systemd", 0, 0, 3, 14_220, 9_000, 9_100, ["/sbin/init", "splash"], None),
            (640, "earlyoom", 1, 0, 812, 3_120, 2_000, 1_900, ["/usr/bin/earlyoom", "-r", "3600", "-m", "5", "-s", "10"], None),
            (2900, "llama-server", 2800, 1000, 712_331, 3_212_332, 2_600_000, 812_332,
             ["/opt/llama.cpp/llama-server", "-m", "/data/models/Qwen2.5-32B-Instruct-Q4_K_M.gguf", "-ngl", "99", "--port", "8080"],
             "/data/models/Qwen2.5-32B-Instruct-Q4_K_M.gguf"),
            (3300, "bash", 3200, 1000, 912_331, 5_120, 3_000, 2_300, ["-bash"], None),
        ]
        if not killed:
            procs.append((3100, "python3", 3300, 1000, 812_331, leak_rss, 180_000, leak_rss - 150_000,
                          ["python3", "train.py", "--epochs", "3", "--wandb-key=abc123secret"], None))
        for pid, comm, ppid, uid, st, rss, shared, pss, argv, model in procs:
            pages = rss // 4
            pf[f"{pid}/stat"] = pid_stat(pid, comm, "R" if pid == 3100 else "S", ppid, 500_000 + i * 150 * (pid == 3100),
                                         8_000, 24, st, rss * 2048, pages)
            pf[f"{pid}/statm"] = statm(pages * 2, pages, shared // 4)
            pf[f"{pid}/status"] = status(comm, pid, ppid, uid, rss, 0, 24)
            pf[f"{pid}/cmdline"] = "\0".join(argv) + "\0"
            pf[f"{pid}/cgroup"] = "0::/user.slice/user-1000.slice/session-3.scope\n" if uid else "0::/system.slice/x.service\n"
            pf[f"{pid}/oom_score"] = f"{600 + i * 20 if pid == 3100 else (380 - i if pid == 2900 else 12 if pid == 3300 else 0)}\n"
            pf[f"{pid}/exe"] = argv[0] if argv[0].startswith("/") else f"/usr/bin/{comm}"
            if uid == 1000:
                pf[f"{pid}/io"] = io(812_331_122 + i * (40 * MIB if pid == 3100 else 0), 12_331_004 + i * MIB)
                # smaps rotation: read on even frames, cached (estimate) on odd frames.
                if i % 2 == 0:
                    pf[f"{pid}/smaps_rollup"] = smaps_rollup(rss, pss, 0)
                else:
                    prev_rss = rss if pid != 3100 else int((30 + 1.5 * (i - 1)) * GIB / KIB)
                    prev_pss = prev_rss - (150_000 if pid == 3100 else rss - pss)
                    pf[f"{pid}/@smaps_cache"] = f"Pss: {prev_pss} kB\nRss: {prev_rss} kB\nAgeMs: 10000\nSwapPss: 0 kB\n"
            if model:
                pf[f"{pid}/@model_files"] = model + "\n"
        pf["@gpu/devices"] = "drm 1\nnvidia 1\ncard 0000:01:00.0 nvidia -\n"
        pf["@nvml/procs"] = "0 2900 21474836480\n" + ("" if killed else "0 3100 1073741824\n")
        pf["@users"] = "0 root\n1000 dev\n"
        markers = {}
        procs_raw = {"source": "linux.procs", "taken_at_ms": t + 3, "read_us": 9_500 + i * 11,
                     "payload": {"type": "linux_files", "files": pf, "missing": {}, "markers": markers,
                                 "clk_tck": 100, "page_size": 4096, "truncated": False}}
        frames.append([host, procs_raw])
    fixture = {
        "meta": {
            "name": "nvidia-oom-trend",
            "os": "linux",
            "captured_at_ms": t0,
            "description": "Synthetic Ubuntu 24.04 workstation (64 GiB, RTX 4090 via NVML, earlyoom -m 5 -s 10): "
                           "a python training job leaks ~1.5 GiB/10 s until the kernel OOM-kills it.",
            "notes": ["synthetic: generated by fixtures/linux/gen_fixtures.py",
                      "frames every 10 s; smaps_rollup read on even frames, cached on odd frames",
                      "last frame: kill recorded (vmstat oom_kill +1) and memory recovered"],
        },
        "frames": frames,
    }
    (HERE / "nvidia-oom-trend.json").write_text(json.dumps(fixture, indent=1, sort_keys=True) + "\n")


if __name__ == "__main__":
    gen_tree()
    gen_nvidia()
    print("wrote", HERE / "tree-amd", "and", HERE / "nvidia-oom-trend.json")

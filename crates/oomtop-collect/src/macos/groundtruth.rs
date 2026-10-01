//! Ground-truth comparison (SPEC §17): oomtop's macOS numbers vs the OS tools — `top -l 1 -o mem -stats
//! pid,mem,cmprs,command` (MEM = `phys_footprint`), `vm_stat`, `sysctl vm.swapusage`, `memory_pressure -Q`.
//! Target delta ≤ 5 %.
//!
//! The parsers are pure. `tools/capture/capture-macos.sh` records the tools' output next to a fixture
//! (`fixtures/macos/<name>.groundtruth.txt`, sections `## top`, `## vm_stat`, `## swapusage`,
//! `## memory_pressure`) so the comparison replays deterministically; the live check spawns the tools
//! itself (`cargo test -p oomtop-collect -- --ignored groundtruth_live --nocapture`).

use oomtop_core::Snapshot;
use std::collections::BTreeMap;

/// One `top` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopRow {
    pub pid: u32,
    pub mem: u64,
    pub cmprs: u64,
    pub command: String,
}

/// `"7434M"`, `"10G"`, `"512K"`, `"0B"`, `"12M+"` → bytes (1024-based, as `top` prints).
pub fn parse_top_size(s: &str) -> Option<u64> {
    let s = s.trim().trim_end_matches(['+', '-']);
    let (num, unit) = s.split_at(s.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(s.len()));
    let v: f64 = num.parse().ok()?;
    let mul = match unit {
        "" | "B" => 1.0,
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        "T" => 1024.0f64.powi(4),
        _ => return None,
    };
    Some((v * mul) as u64)
}

/// Parses `top -l 1 -stats pid,mem,cmprs,command` output (rows after the `PID` header).
pub fn parse_top(text: &str) -> Vec<TopRow> {
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("PID") && t.contains("MEM") {
            in_table = true;
            continue;
        }
        if !in_table || t.is_empty() {
            continue;
        }
        let mut it = t.split_whitespace();
        let (Some(pid), Some(mem), Some(cmprs)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Some(mem), Some(cmprs)) = (pid.parse(), parse_top_size(mem), parse_top_size(cmprs))
        else {
            continue;
        };
        rows.push(TopRow {
            pid,
            mem,
            cmprs,
            command: it.collect::<Vec<_>>().join(" "),
        });
    }
    rows
}

/// Parses `vm_stat`: `(page size, "Pages free" → value, …)`.
pub fn parse_vm_stat(text: &str) -> (u64, BTreeMap<String, u64>) {
    let mut page = 16384;
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Some(i) = line.find("page size of ") {
            if let Some(n) = line[i + 13..]
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
            {
                page = n;
            }
            continue;
        }
        let Some((k, v)) = line.rsplit_once(':') else {
            continue;
        };
        if let Ok(n) = v.trim().trim_end_matches('.').parse::<u64>() {
            out.insert(k.trim().trim_matches('"').to_string(), n);
        }
    }
    (page, out)
}

/// Parses `vm.swapusage: total = 3072.00M  used = 2079.25M  free = 992.75M  (encrypted)` → (total, used).
pub fn parse_swapusage(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        let i = text.find(&format!("{name} = "))?;
        let v = text[i + name.len() + 3..].split_whitespace().next()?;
        parse_top_size(v)
    };
    Some((field("total")?, field("used")?))
}

/// `memory_pressure -Q` → "System-wide memory free percentage: 54%" → 54.
pub fn parse_memory_pressure_free_pct(text: &str) -> Option<u8> {
    let i = text.find("free percentage:")?;
    text[i + 16..]
        .trim()
        .trim_end_matches('%')
        .split('%')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Sections of a `.groundtruth.txt` file (`## name` headers).
pub fn parse_sections(text: &str) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    let mut cur: Option<String> = None;
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("## ") {
            cur = Some(h.trim().to_string());
            out.entry(h.trim().to_string()).or_default();
            continue;
        }
        if let Some(c) = &cur {
            let e = out.entry(c.clone()).or_default();
            e.push_str(line);
            e.push('\n');
        }
    }
    out
}

/// One comparison row.
#[derive(Debug, Clone, PartialEq)]
pub struct Delta {
    pub what: String,
    pub truth: u64,
    /// `None` = oomtop has no value (e.g. another user's process without root).
    pub ours: Option<u64>,
    /// Live checks: oomtop's value read right *after* the tool (with `ours` read right before). A truth
    /// that lies between the two readings is a 0 % delta — the value moved while the tool sampled.
    pub ours_after: Option<u64>,
}

impl Delta {
    /// |ours − truth| / truth × 100 (bracketed: distance to the nearer reading, 0 when in between).
    pub fn pct(&self) -> Option<f64> {
        let t = self.truth as f64;
        let rel = |o: u64| {
            let o = o as f64;
            if t == 0.0 {
                if o == 0.0 {
                    0.0
                } else {
                    100.0
                }
            } else {
                (o - t).abs() / t * 100.0
            }
        };
        match (self.ours, self.ours_after) {
            (Some(a), Some(b)) if a.min(b) <= self.truth && self.truth <= a.max(b) => Some(0.0),
            (Some(a), Some(b)) => Some(rel(a).min(rel(b))),
            (Some(a), None) | (None, Some(a)) => Some(rel(a)),
            (None, None) => None,
        }
    }

    /// Adds the post-tool reading from `after` (same order/length as `self`).
    pub fn bracket(rows: Vec<Delta>, after: &[Delta]) -> Vec<Delta> {
        rows.into_iter()
            .zip(after)
            .map(|(mut d, a)| {
                d.ours_after = a.ours;
                d
            })
            .collect()
    }
}

/// Footprint of the `n` largest `top` rows vs the snapshot's `footprint_or_pss` (matched by pid).
pub fn compare_footprints(top: &[TopRow], snap: &Snapshot, n: usize) -> Vec<Delta> {
    top.iter()
        .take(n)
        .map(|r| Delta {
            what: format!("{} {}", r.pid, r.command),
            truth: r.mem,
            ours: snap
                .process_by_pid(r.pid)
                .and_then(|p| p.mem.footprint_or_pss.value),
            ours_after: None,
        })
        .collect()
}

/// Host numbers vs `vm_stat` / `vm.swapusage` / `memory_pressure` (bytes; memorystatus in %).
pub fn compare_host(
    vm_stat: &str,
    swapusage: Option<&str>,
    memory_pressure: Option<&str>,
    snap: &Snapshot,
) -> Vec<Delta> {
    let (page, vm) = parse_vm_stat(vm_stat);
    let m = &snap.memory;
    let pages = |k: &str| vm.get(k).map(|v| v * page);
    let mut out = Vec::new();
    let mut push = |what: &str, truth: Option<u64>, ours: Option<u64>| {
        if let Some(truth) = truth {
            out.push(Delta {
                what: what.to_string(),
                truth,
                ours,
                ours_after: None,
            });
        }
    };
    push("free (vm_stat Pages free)", pages("Pages free"), m.free.value);
    push(
        "wired (Pages wired down)",
        pages("Pages wired down"),
        m.wired.value,
    );
    push(
        "cached (File-backed pages)",
        pages("File-backed pages"),
        m.cached.value,
    );
    push(
        "compressed (Pages occupied by compressor)",
        pages("Pages occupied by compressor"),
        m.compressed.value,
    );
    push(
        "compressed_logical (Pages stored in compressor)",
        pages("Pages stored in compressor"),
        m.compressed_logical.value,
    );
    push(
        "app (Anonymous − purgeable)",
        pages("Anonymous pages").map(|a| a.saturating_sub(pages("Pages purgeable").unwrap_or(0))),
        m.app.value,
    );
    push(
        "available (free+spec+purgeable+file-backed)",
        match (
            pages("Pages free"),
            pages("Pages speculative"),
            pages("Pages purgeable"),
            pages("File-backed pages"),
        ) {
            (Some(a), Some(b), Some(c), Some(d)) => Some(a + b + c + d),
            _ => None,
        },
        m.available.value,
    );
    if let Some((total, used)) = swapusage.and_then(parse_swapusage) {
        push("swap_total (vm.swapusage)", Some(total), m.swap_total.value);
        push("swap_used (vm.swapusage)", Some(used), m.swap_used.value);
    }
    if let Some(p) = memory_pressure.and_then(parse_memory_pressure_free_pct) {
        push(
            "memorystatus_level % (memory_pressure free %)",
            Some(p as u64),
            m.memorystatus_level.value.map(|v| v as u64),
        );
    }
    out
}

/// Markdown-ish table for reports.
pub fn render(title: &str, rows: &[Delta]) -> String {
    let bracketed = rows.iter().any(|r| r.ours_after.is_some());
    let v = |x: Option<u64>| x.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into());
    let mut s = format!("{title}\n{:<50} {:>14} {:>14}", "what", "truth", "oomtop");
    if bracketed {
        s.push_str(&format!(" {:>14}", "oomtop after"));
    }
    s.push_str(&format!(" {:>8}\n", "delta"));
    for r in rows {
        s.push_str(&format!(
            "{:<50} {:>14} {:>14}",
            r.what.chars().take(50).collect::<String>(),
            r.truth,
            v(r.ours)
        ));
        if bracketed {
            s.push_str(&format!(" {:>14}", v(r.ours_after)));
        }
        let pct = r.pct().map(|p| format!("{p:.2}%")).unwrap_or_else(|| "—".into());
        s.push_str(&format!(" {pct:>8}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{load_fixture, replay};
    use std::path::PathBuf;

    const TOP: &str = "Processes: 700 total\nPID    MEM   CMPRS COMMAND         \n11368  7434M 5028M qemu-system-aarc\n88143  5166M 3088M java            \n92759  393M  48M   claude.exe\n1 12K+ 0B launchd\n";

    #[test]
    fn parsers() {
        let rows = parse_top(TOP);
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].pid, 11368);
        assert_eq!(rows[0].mem, 7434 << 20);
        assert_eq!(rows[0].cmprs, 5028 << 20);
        assert_eq!(rows[0].command, "qemu-system-aarc");
        assert_eq!(rows[3].mem, 12 << 10);
        assert_eq!(parse_top_size("10G"), Some(10 << 30));
        assert_eq!(parse_top_size("x"), None);
        let (page, vm) = parse_vm_stat(
            "Mach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:     24198.\n\"Translation faults\":  781240633.\n",
        );
        assert_eq!(page, 16384);
        assert_eq!(vm["Pages free"], 24198);
        assert_eq!(vm["Translation faults"], 781240633);
        assert_eq!(
            parse_swapusage("vm.swapusage: total = 3072.00M  used = 2079.25M  free = 992.75M  (encrypted)"),
            Some((3072 << 20, (2079.25 * 1048576.0) as u64))
        );
        assert_eq!(
            parse_memory_pressure_free_pct("The system has 1\nSystem-wide memory free percentage: 54%\n"),
            Some(54)
        );
        let s = parse_sections("## top\na\nb\n## vm_stat\nc\n");
        assert_eq!(s["top"], "a\nb\n");
        assert_eq!(s["vm_stat"], "c\n");
        let d = Delta {
            what: "x".into(),
            truth: 100,
            ours: Some(103),
            ours_after: None,
        };
        assert!((d.pct().unwrap() - 3.0).abs() < 1e-9);
        let b = Delta {
            ours: Some(90),
            ours_after: Some(110),
            ..d.clone()
        };
        assert_eq!(b.pct(), Some(0.0));
        let c = Delta {
            ours: Some(80),
            ours_after: Some(96),
            ..d
        };
        assert!((c.pct().unwrap() - 4.0).abs() < 1e-9);
    }

    fn fixture_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/macos")
    }

    /// Replays the recorded baseline fixture and compares it with the ground truth recorded right after it.
    #[test]
    fn baseline_fixture_matches_recorded_ground_truth() {
        let fx = fixture_dir().join("m5-air-baseline.json");
        let gt = fixture_dir().join("m5-air-baseline.groundtruth.txt");
        let (Ok(fixture), Ok(gt)) = (load_fixture(&fx), std::fs::read_to_string(&gt)) else {
            panic!("baseline fixture + ground truth must exist: {}", fx.display());
        };
        let snaps = replay(&fixture);
        let snap = snaps.last().expect("frames");
        let sec = parse_sections(&gt);
        let top = parse_top(sec.get("top").map(String::as_str).unwrap_or(""));
        assert!(top.len() >= 10, "top section has {} rows", top.len());
        let fp = compare_footprints(&top, snap, 10);
        let host = compare_host(
            sec.get("vm_stat").map(String::as_str).unwrap_or(""),
            sec.get("swapusage").map(String::as_str),
            sec.get("memory_pressure").map(String::as_str),
            snap,
        );
        eprintln!("{}", render("footprint, top 10 by MEM (replayed fixture)", &fp));
        eprintln!("{}", render("host (replayed fixture)", &host));
        // Every same-user process in top's top 10 is within 5 %.
        let comparable: Vec<&Delta> = fp.iter().filter(|d| d.ours.is_some()).collect();
        assert!(!comparable.is_empty());
        for d in &comparable {
            assert!(d.pct().unwrap() <= 5.0, "footprint delta too large: {d:?}");
        }
        // Stable host quantities (large and slow-moving) within 5 %.
        for key in [
            "wired",
            "compressed (",
            "compressed_logical",
            "swap_total",
            "swap_used",
            "app (",
        ] {
            if let Some(d) = host.iter().find(|d| d.what.starts_with(key)) {
                assert!(d.pct().unwrap_or(100.0) <= 5.0, "host delta too large: {d:?}");
            }
        }
    }

    /// Live comparison against the OS tools on this Mac (spawns `top`, `vm_stat`, `sysctl`, `memory_pressure`).
    #[test]
    #[ignore = "live ground truth; run explicitly"]
    fn groundtruth_live() {
        use crate::Source;
        use std::process::Command;
        use std::time::Duration;
        let run = |cmd: &str, args: &[&str]| {
            Command::new(cmd)
                .args(args)
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        };
        let mut host = super::super::MacHostSource::new();
        let mut procs = super::super::MacProcSource::default();
        let snap_of = |raws: &[&crate::raw::RawSample]| {
            let mut snap = Snapshot::default();
            for r in raws {
                crate::decode(r, None).apply(&mut snap);
            }
            snap
        };
        // Bracket every tool with our own reads right before and right after it.
        let p0 = procs.read(Duration::from_millis(200)).expect("procs");
        let top = run(
            "/usr/bin/top",
            &[
                "-l",
                "1",
                "-o",
                "mem",
                "-n",
                "25",
                "-stats",
                "pid,mem,cmprs,command",
            ],
        );
        let p1 = procs.read(Duration::from_millis(200)).expect("procs");
        let h0 = host.read(Duration::from_millis(50)).expect("host");
        let vm = run("/usr/bin/vm_stat", &[]);
        let swap = run("/usr/sbin/sysctl", &["vm.swapusage"]);
        let mp = run("/usr/bin/memory_pressure", &["-Q"]);
        let h1 = host.read(Duration::from_millis(50)).expect("host");
        let (before, after) = (snap_of(&[&h0, &p0]), snap_of(&[&h1, &p1]));
        let rows = parse_top(&top);
        let fp = Delta::bracket(
            compare_footprints(&rows, &before, 10),
            &compare_footprints(&rows, &after, 10),
        );
        let hd = Delta::bracket(
            compare_host(&vm, Some(&swap), Some(&mp), &before),
            &compare_host(&vm, Some(&swap), Some(&mp), &after),
        );
        eprintln!("{}", render("footprint, top 10 by MEM (live, bracketed)", &fp));
        eprintln!("{}", render("host (live, bracketed)", &hd));
        for d in fp.iter().filter(|d| d.ours.is_some() && d.ours_after.is_some()) {
            assert!(d.pct().unwrap() <= 5.0, "{d:?}");
        }
    }
}

//! Pure parsers for Linux `/proc` and `/sys` text formats (no I/O). Formats follow the kernel docs:
//! `Documentation/filesystems/proc.rst`, `admin-guide/cgroup-v2.rst`, `driver-api/thermal/sysfs-api.rst`,
//! `hwmon/sysfs-interface.rst`, `power/powercap/powercap.rst`, `power/power_supply_class.rst`,
//! `cpu-freq/cpufreq-stats.rst`. Every parser tolerates missing/garbled input by returning `None` / empty.

use std::collections::BTreeMap;

/// Parses a trimmed unsigned integer.
pub fn parse_u64(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// Parses a trimmed signed integer.
pub fn parse_i64(text: &str) -> Option<i64> {
    text.trim().parse().ok()
}

/// `space separated key value` files (`/proc/vmstat`, `memory.events`, `memory.stat`).
pub fn parse_space_kv(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.parse().ok()?))
        })
        .collect()
}

/// `/proc/<pid>/io` (`rchar: 123` lines). Unit-less byte counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcIo {
    pub rchar: u64,
    pub wchar: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub cancelled_write_bytes: u64,
}

pub fn parse_proc_io(text: &str) -> Option<ProcIo> {
    let mut io = ProcIo::default();
    let mut any = false;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let Some(v) = parse_u64(v) else { continue };
        any = true;
        match k.trim() {
            "rchar" => io.rchar = v,
            "wchar" => io.wchar = v,
            "read_bytes" => io.read_bytes = v,
            "write_bytes" => io.write_bytes = v,
            "cancelled_write_bytes" => io.cancelled_write_bytes = v,
            _ => {}
        }
    }
    any.then_some(io)
}

/// The cgroup v2 path from `/proc/<pid>/cgroup` (`0::/user.slice/…`). `None` on pure cgroup v1 hosts.
pub fn cgroup_v2_path(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| p.trim().to_string())
        .filter(|p| p.starts_with('/'))
}

/// All ancestors of a cgroup path including itself, root first: `/a/b` → `["", "/a", "/a/b"]`
/// (`""` = the cgroup-namespace root, i.e. `/sys/fs/cgroup` itself).
pub fn cgroup_ancestors(path: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut cur = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        // Never walk out of the cgroup mount, whatever the kernel handed us.
        if part == ".." || part == "." {
            continue;
        }
        cur.push('/');
        cur.push_str(part);
        out.push(cur.clone());
    }
    out
}

/// cgroup v2 `memory.max` / `memory.high` / `memory.swap.max`: `"max"` = unlimited (`Some(None)`).
pub fn parse_cgroup_limit(text: &str) -> Option<Option<u64>> {
    let t = text.trim();
    if t == "max" {
        Some(None)
    } else {
        t.parse().ok().map(Some)
    }
}

/// CPU list formats: `"0 1 2 3"` (cpufreq `related_cpus`), `"0-3,8-11"` (cpumask lists).
pub fn parse_cpu_list(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    for tok in text.split(|c: char| c == ',' || c.is_whitespace()) {
        if tok.is_empty() {
            continue;
        }
        match tok.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    if b >= a && b - a < 4096 {
                        out.extend(a..=b);
                    }
                }
            }
            None => {
                if let Ok(v) = tok.parse() {
                    out.push(v);
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Per-cpu `(busy, total)` jiffies from `/proc/stat` (`cpuN user nice system idle iowait irq softirq steal`).
pub fn per_cpu_ticks(stat: &str) -> BTreeMap<u32, (u64, u64)> {
    let mut out = BTreeMap::new();
    for l in stat.lines() {
        let Some(rest) = l.strip_prefix("cpu") else {
            continue;
        };
        let mut it = rest.split_whitespace();
        let Some(Ok(n)) = it.next().map(|n| n.parse::<u32>()) else {
            continue; // the aggregate "cpu " line
        };
        let v: Vec<u64> = it.filter_map(|x| x.parse().ok()).collect();
        if v.len() < 4 {
            continue;
        }
        let total: u64 = v.iter().take(8).sum();
        let idle = v[3] + v.get(4).copied().unwrap_or(0);
        out.insert(n, (total.saturating_sub(idle), total));
    }
    out
}

/// Busy % per cpu between two `/proc/stat` texts.
pub fn per_cpu_busy_pct(now: &str, prev: &str) -> BTreeMap<u32, f64> {
    let a = per_cpu_ticks(now);
    let b = per_cpu_ticks(prev);
    a.iter()
        .filter_map(|(cpu, (busy, total))| {
            let (pb, pt) = b.get(cpu)?;
            let dt = total.checked_sub(*pt)?;
            if dt == 0 {
                return None;
            }
            let db = busy.saturating_sub(*pb).min(dt);
            Some((*cpu, db as f64 / dt as f64 * 100.0))
        })
        .collect()
}

/// A thermal trip point (`trip_point_N_type` / `trip_point_N_temp`, m°C).
#[derive(Debug, Clone, PartialEq)]
pub struct Trip {
    /// `active` (fan), `passive` (throttling), `hot`, `critical`.
    pub kind: String,
    pub celsius: f64,
}

/// Millidegrees Celsius (thermal zones, hwmon `temp*_input`) → °C. Rejects absurd readings.
pub fn milli_c(text: &str) -> Option<f64> {
    let v = parse_i64(text)? as f64 / 1000.0;
    (-60.0..=250.0).contains(&v).then_some(v)
}

/// The `*` marked (current) entry and the max of an amdgpu `pp_dpm_sclk` / `pp_dpm_mclk` table:
/// `"0: 500Mhz\n1: 1800Mhz *\n2: 2500Mhz\n"` → `(Some(1800), Some(2500))`.
pub fn parse_pp_dpm(text: &str) -> (Option<f64>, Option<f64>) {
    let mut cur = None;
    let mut max: Option<f64> = None;
    for l in text.lines() {
        let Some((_, rest)) = l.split_once(':') else {
            continue;
        };
        let rest = rest.trim();
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Ok(mhz) = digits.parse::<f64>() else {
            continue;
        };
        let lower = rest.to_ascii_lowercase();
        // Some ASICs print GHz-free "Mhz", some "MHz"; values in kHz never appear here.
        if !lower.contains("mhz") {
            continue;
        }
        max = Some(max.map_or(mhz, |m| m.max(mhz)));
        if rest.ends_with('*') {
            cur = Some(mhz);
        }
    }
    (cur, max)
}

/// `KEY=VALUE` lines (`uevent`, `/etc/default/*`, systemd unit/INI files; section headers ignored unless
/// requested via [`parse_ini_section`]).
pub fn parse_env_lines(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with('#') || l.starts_with(';') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// Keys of one `[Section]` of an INI/systemd file (later assignments win).
pub fn parse_ini_section(text: &str, section: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut in_section = false;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') && t.ends_with(']') {
            in_section = &t[1..t.len() - 1] == section;
            continue;
        }
        if !in_section || t.starts_with('#') || t.starts_with(';') {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    out
}

/// systemd percent values: `"90%"`, `"90.5%"`, `"900‰"`, `"9000‱"` → percent.
pub fn parse_percent(text: &str) -> Option<f64> {
    let t = text.trim();
    let (num, div) = [('%', 1.0), ('‰', 10.0), ('‱', 100.0)]
        .into_iter()
        .find_map(|(suffix, div)| t.strip_suffix(suffix).map(|n| (n, div)))?;
    let v = num.trim().parse::<f64>().ok()? / div;
    (0.0..=100.0).contains(&v).then_some(v)
}

/// systemd time spans (`systemd.time(7)`): `"30s"`, `"1min"`, `"1min 30s"`, `"1min30s"`, `"5 min"`,
/// `"500ms"`, `"2h"`; bare numbers = seconds.
pub fn parse_timespan_s(text: &str) -> Option<f64> {
    let t = text.trim();
    let mut total = 0.0;
    let mut any = false;
    let mut rest = t;
    while !rest.is_empty() {
        let n_end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        if n_end == 0 {
            return None;
        }
        let n: f64 = rest[..n_end].parse().ok()?;
        rest = rest[n_end..].trim_start();
        let u_end = rest
            .find(|c: char| c.is_ascii_digit() || c == '.' || c.is_whitespace())
            .unwrap_or(rest.len());
        let unit = &rest[..u_end];
        rest = rest[u_end..].trim_start();
        let mul = match unit {
            "" | "s" | "sec" | "second" | "seconds" => 1.0,
            "ms" | "msec" => 0.001,
            "us" | "usec" | "µs" | "μs" => 0.000_001,
            "m" | "min" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hour" | "hours" => 3600.0,
            "d" | "day" | "days" => 86_400.0,
            "w" | "week" | "weeks" => 604_800.0,
            _ => return None,
        };
        total += n * mul;
        any = true;
    }
    any.then_some(total)
}

/// Minimal POSIX-shell-like word splitting for `EARLYOOM_ARGS="…"` (quotes, no expansion).
pub fn shell_words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut in_word = false;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            None => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// `/etc/passwd` → uid → name.
pub fn parse_passwd(text: &str) -> BTreeMap<u32, String> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let _pw = f.next()?;
            let uid = f.next()?.parse().ok()?;
            (!name.is_empty() && !name.starts_with('#')).then(|| (uid, name.to_string()))
        })
        .collect()
}

/// Extensions of model weight files (SPEC §5 `Process::model_files`).
pub const MODEL_EXTENSIONS: &[&str] = &[
    ".gguf",
    ".ggml",
    ".safetensors",
    ".onnx",
    ".ckpt",
    ".pth",
    ".mlmodel",
];

/// True for paths that look like model weights.
pub fn is_model_file(path: &str) -> bool {
    let p = path.trim_end_matches(" (deleted)").to_ascii_lowercase();
    MODEL_EXTENSIONS.iter().any(|e| p.ends_with(e))
}

/// Model files mapped by a process, from `/proc/<pid>/maps` (6th column = path). Sorted, unique.
pub fn model_files_from_maps(maps: &str) -> Vec<String> {
    let mut out: Vec<String> = maps
        .lines()
        .filter_map(|l| {
            // address perms offset dev inode path — the path may contain spaces.
            let mut it = l.splitn(6, char::is_whitespace);
            for _ in 0..5 {
                it.next()?;
            }
            let path = it.next()?.trim();
            (path.starts_with('/') && is_model_file(path)).then(|| path.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Lower-case hex encoding for binary sysfs files stored in a `RawSample` (`"hex:0a1b…"`).
pub fn to_hex(bytes: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(4 + bytes.len() * 2);
    s.push_str("hex:");
    for b in bytes {
        s.push(H[(b >> 4) as usize] as char);
        s.push(H[(b & 15) as usize] as char);
    }
    s
}

/// Inverse of [`to_hex`]; `None` for anything malformed.
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    let h = text.trim().strip_prefix("hex:")?;
    if h.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    h.as_bytes()
        .chunks(2)
        .map(|p| Some(nib(p[0])? << 4 | nib(p[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_and_cgroup() {
        let io = parse_proc_io(
            "rchar: 100\nwchar: 50\nsyscr: 1\nsyscw: 1\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n",
        )
        .unwrap();
        assert_eq!(io.read_bytes, 4096);
        assert_eq!(io.write_bytes, 8192);
        assert!(parse_proc_io("garbage").is_none());
        assert_eq!(
            cgroup_v2_path("12:memory:/x\n0::/user.slice/user-1000.slice/session-2.scope\n").as_deref(),
            Some("/user.slice/user-1000.slice/session-2.scope")
        );
        assert_eq!(cgroup_v2_path("5:cpu:/foo\n"), None);
        assert_eq!(cgroup_ancestors("/a/b"), vec!["", "/a", "/a/b"]);
        assert_eq!(cgroup_ancestors("/"), vec![""]);
        assert_eq!(cgroup_ancestors("/a/../b"), vec!["", "/a", "/a/b"]);
        assert_eq!(parse_cgroup_limit("max\n"), Some(None));
        assert_eq!(parse_cgroup_limit("1073741824\n"), Some(Some(1 << 30)));
        assert_eq!(parse_cgroup_limit("x"), None);
    }

    #[test]
    fn cpu_lists_and_busy() {
        assert_eq!(parse_cpu_list("0 1 2 3\n"), vec![0, 1, 2, 3]);
        assert_eq!(parse_cpu_list("0-2,8-9"), vec![0, 1, 2, 8, 9]);
        assert_eq!(parse_cpu_list(""), Vec::<u32>::new());
        let a = "cpu  10 0 10 80 0 0 0 0\ncpu0 10 0 10 80 0 0 0 0\ncpu1 0 0 0 100 0 0 0 0\n";
        let b = "cpu  60 0 10 130 0 0 0 0\ncpu0 60 0 10 80 0 0 0 0\ncpu1 0 0 0 150 0 0 0 0\n";
        let busy = per_cpu_busy_pct(b, a);
        assert_eq!(busy[&0], 100.0);
        assert_eq!(busy[&1], 0.0);
        assert!(!busy.contains_key(&2));
    }

    #[test]
    fn temps_and_dpm() {
        assert_eq!(milli_c("45500\n"), Some(45.5));
        assert_eq!(milli_c("-274000"), None);
        assert_eq!(milli_c("nope"), None);
        let (cur, max) = parse_pp_dpm("0: 500Mhz\n1: 1800Mhz *\n2: 2500Mhz\n");
        assert_eq!((cur, max), (Some(1800.0), Some(2500.0)));
        assert_eq!(parse_pp_dpm(""), (None, None));
    }

    #[test]
    fn config_formats() {
        let ini = "# c\n[OOM]\nSwapUsedLimit=80%\nDefaultMemoryPressureDurationSec=20s\n[Other]\nSwapUsedLimit=1%\n";
        let s = parse_ini_section(ini, "OOM");
        assert_eq!(s["SwapUsedLimit"], "80%");
        assert_eq!(parse_percent("80%"), Some(80.0));
        assert_eq!(parse_percent("905‰"), Some(90.5));
        assert_eq!(parse_percent("9000‱"), Some(90.0));
        assert_eq!(parse_percent("80"), None);
        assert_eq!(parse_timespan_s("20s"), Some(20.0));
        assert_eq!(parse_timespan_s("1min 30s"), Some(90.0));
        assert_eq!(parse_timespan_s("500ms"), Some(0.5));
        assert_eq!(parse_timespan_s("30"), Some(30.0));
        assert_eq!(parse_timespan_s("5 parsecs"), None);
        assert_eq!(parse_timespan_s("1min30s"), Some(90.0));
        assert_eq!(parse_timespan_s("5 min"), Some(300.0));
        assert_eq!(parse_timespan_s("1h 2min3s"), Some(3723.0));
        assert_eq!(parse_timespan_s(""), None);
        assert_eq!(parse_timespan_s("s"), None);
        assert_eq!(parse_timespan_s("1..2s"), None);
        let env = parse_env_lines("DRIVER=amdgpu\nPCI_SLOT_NAME=0000:03:00.0\n# x=y\nA=\"q v\"\n");
        assert_eq!(env["DRIVER"], "amdgpu");
        assert_eq!(env["A"], "q v");
        assert!(!env.contains_key("# x"));
        assert_eq!(
            shell_words(r#"-m 5,2 -s 20 --avoid '(^|/)(init|Xorg)$' -r "3600""#),
            vec![
                "-m",
                "5,2",
                "-s",
                "20",
                "--avoid",
                "(^|/)(init|Xorg)$",
                "-r",
                "3600"
            ]
        );
        let pw = parse_passwd("root:x:0:0:root:/root:/bin/bash\ndev:x:1000:1000::/home/dev:/bin/zsh\nbad\n");
        assert_eq!(pw[&1000], "dev");
        assert_eq!(pw.len(), 2);
    }

    #[test]
    fn model_files_and_hex() {
        let maps = "7f00-7f10 r--s 00000000 103:02 123 /models/qwen2.5 7b.Q4_K_M.gguf\n\
                    7f10-7f20 r-xp 00000000 103:02 124 /usr/lib/libc.so.6\n\
                    7f20-7f30 r--s 00000000 103:02 125 /models/vae.safetensors (deleted)\n\
                    7f30-7f40 rw-p 00000000 00:00 0 \n";
        assert_eq!(
            model_files_from_maps(maps),
            vec![
                "/models/qwen2.5 7b.Q4_K_M.gguf".to_string(),
                "/models/vae.safetensors (deleted)".to_string()
            ]
        );
        assert!(!is_model_file("/usr/lib/libc.so.6"));
        let bytes = [0u8, 1, 0xab, 0xff];
        let h = to_hex(&bytes);
        assert_eq!(h, "hex:0001abff");
        assert_eq!(from_hex(&h).unwrap(), bytes);
        assert!(from_hex("hex:0").is_none());
        assert!(from_hex("0001").is_none());
        assert!(from_hex("hex:zz").is_none());
    }
}

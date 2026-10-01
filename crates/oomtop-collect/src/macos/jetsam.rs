//! JetsamEvent report listing (SPEC §8.3 "recent kills"): `/Library/Logs/DiagnosticReports/JetsamEvent-*.ips`
//! is readable by admin users (group `_analyticsusers`). Only the newest reports within
//! [`MAX_AGE_MS`] are parsed, each once (cached by file name + size + mtime), and only process **names**,
//! pids and kill reasons are kept. Directory rescans are rate-limited to [`RESCAN_MS`].

use super::extras::{JetsamRecord, JetsamVictim};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const REPORT_DIR: &str = "/Library/Logs/DiagnosticReports";
/// Reports older than this are ignored.
pub const MAX_AGE_MS: u64 = 7 * 24 * 3600 * 1000;
/// Newest reports kept.
pub const MAX_REPORTS: usize = 8;
/// Directory rescan interval.
pub const RESCAN_MS: u64 = 30_000;
/// Reports larger than this are skipped (a normal one is ~0.5 MB).
pub const MAX_REPORT_BYTES: u64 = 8 << 20;

#[derive(Deserialize)]
struct Header {
    #[serde(default)]
    timestamp: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Body {
    #[serde(rename = "largestProcess")]
    largest_process: Option<String>,
    processes: Vec<BodyProc>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BodyProc {
    name: String,
    pid: Option<u32>,
    reason: Option<String>,
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Parses an `.ips` header timestamp: `"2026-09-29 17:13:21.00 +0530"` → ms since epoch.
pub fn parse_ips_timestamp(s: &str) -> Option<u64> {
    let mut parts = s.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let tz = parts.next().unwrap_or("+0000");
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let sec: f64 = t.next().unwrap_or("0").parse().ok()?;
    // Range checks keep the arithmetic below overflow-free for any input (a corrupt or future-format
    // report must never panic, SPEC §6.1).
    if !(1970..=9999).contains(&y)
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&da)
        || !(0..=23).contains(&h)
        || !(0..=59).contains(&mi)
        || !(0.0..61.0).contains(&sec)
    {
        return None;
    }
    let (sign, tzd) = match tz.as_bytes().first() {
        Some(b'-') => (-1, &tz[1..]),
        Some(b'+') => (1, &tz[1..]),
        _ => (1, tz),
    };
    if tzd.len() != 4 || !tzd.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let tzn: i64 = tzd.parse().ok()?;
    if !(0..=1400).contains(&tzn) || tzn % 100 > 59 {
        return None;
    }
    let offset_s = sign * ((tzn / 100) * 3600 + (tzn % 100) * 60);
    let secs = days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 - offset_s;
    let ms = secs as f64 * 1000.0 + sec * 1000.0;
    (ms >= 0.0).then_some(ms as u64)
}

/// Parses one report's text (two JSON documents: a one-line header, then the body).
pub fn parse_report(text: &str, mtime_ms: u64) -> Option<JetsamRecord> {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let at_ms = serde_json::from_str::<Header>(first)
        .ok()
        .and_then(|h| parse_ips_timestamp(&h.timestamp))
        .unwrap_or(mtime_ms);
    let body: Body = serde_json::from_str(rest).ok()?;
    Some(JetsamRecord {
        at_ms,
        largest_process: body.largest_process,
        killed: body
            .processes
            .into_iter()
            .filter_map(|p| {
                let reason = p.reason?;
                Some(JetsamVictim {
                    name: p.name,
                    pid: p.pid,
                    reason,
                })
            })
            .collect(),
    })
}

type CacheKey = (String, u64, u64);

/// Cached scanner of the report directory.
#[derive(Debug)]
pub struct JetsamScanner {
    dir: PathBuf,
    cache: HashMap<CacheKey, Option<JetsamRecord>>,
    last_scan: Option<Instant>,
    last: Result<Vec<(String, JetsamRecord)>, String>,
}

impl Default for JetsamScanner {
    fn default() -> Self {
        Self::new(REPORT_DIR)
    }
}

fn mtime_ms(m: &std::fs::Metadata) -> u64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl JetsamScanner {
    pub fn new(dir: impl AsRef<Path>) -> Self {
        JetsamScanner {
            dir: dir.as_ref().to_path_buf(),
            cache: HashMap::new(),
            last_scan: None,
            last: Ok(Vec::new()),
        }
    }

    /// Recent reports (file name → record), newest first. Rescans at most every [`RESCAN_MS`]; parsing
    /// stops early once `budget` is spent (the remaining files are parsed on a later call).
    pub fn scan(&mut self, budget: Duration) -> Result<Vec<(String, JetsamRecord)>, String> {
        let due = self
            .last_scan
            .map(|t| t.elapsed() >= Duration::from_millis(RESCAN_MS))
            .unwrap_or(true);
        if due {
            let start = Instant::now();
            self.last = self.rescan(start, budget);
            self.last_scan = Some(Instant::now());
        }
        self.last.clone()
    }

    fn rescan(&mut self, start: Instant, budget: Duration) -> Result<Vec<(String, JetsamRecord)>, String> {
        let rd = std::fs::read_dir(&self.dir).map_err(|e| {
            format!(
                "{} unreadable ({}; admin users only)",
                self.dir.display(),
                e.kind()
            )
        })?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut files: Vec<(String, u64, u64)> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if !(name.starts_with("JetsamEvent-") && name.ends_with(".ips")) {
                    return None;
                }
                let m = e.metadata().ok()?;
                let mt = mtime_ms(&m);
                (m.is_file() && mt + MAX_AGE_MS >= now).then_some((name, m.len(), mt))
            })
            .collect();
        files.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
        files.truncate(MAX_REPORTS);
        let mut out = Vec::new();
        let mut skipped = 0usize;
        for key in &files {
            if !self.cache.contains_key(key) {
                if start.elapsed() > budget {
                    skipped += 1;
                    continue;
                }
                let rec = if key.1 > MAX_REPORT_BYTES {
                    None
                } else {
                    std::fs::read_to_string(self.dir.join(&key.0))
                        .ok()
                        .and_then(|t| parse_report(&t, key.2))
                };
                self.cache.insert(key.clone(), rec);
            }
            if let Some(Some(r)) = self.cache.get(key) {
                out.push((key.0.clone(), r.clone()));
            }
        }
        self.cache.retain(|k, _| files.contains(k));
        if skipped > 0 {
            // Force a rescan next time so the skipped files get parsed.
            self.last_scan = None;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r#"{"bug_type":"298","timestamp":"2026-09-29 17:13:21.00 +0530","os_version":"macOS 26.6 (25G72)"}
{
  "product" : "Mac17,3",
  "largestProcess" : "sd-server",
  "processes" : [
    { "name" : "corebrightnessd", "pid" : 398, "lifetimeMax" : 472 },
    { "name" : "spotlightknowledged", "pid" : 73341, "reason" : "per-process-limit", "killDelta" : 38175 }
  ]
}"#;

    #[test]
    fn timestamps() {
        // 2026-09-29 11:43:21 UTC
        let ms = parse_ips_timestamp("2026-09-29 17:13:21.00 +0530").unwrap();
        assert_eq!(ms, 1_790_682_201_000);
        assert_eq!(parse_ips_timestamp("1970-01-01 00:00:00.50 +0000"), Some(500));
        assert_eq!(parse_ips_timestamp("garbage"), None);
        assert_eq!(parse_ips_timestamp("2026-13-01 00:00:00 +0000"), None);
        assert_eq!(
            parse_ips_timestamp("2026-09-29 17:13:21.00 -0700"),
            Some(1_790_727_201_000)
        );
    }

    /// Corrupt headers never overflow/panic (debug builds check arithmetic overflow).
    #[test]
    fn hostile_timestamps_are_rejected_not_panics() {
        for s in [
            "99999999999999999-01-01 00:00:00 +0000",
            "-99999999999999999-01-01 00:00:00 +0000",
            "2026-01-01 99999999999999999:00:00 +0000",
            "2026-01-01 -5:00:00 +0000",
            "2026-01-01 00:00:inf +0000",
            "2026-01-01 00:00:NaN +0000",
            "2026-01-01 00:00:00 +99999999999999999",
            "2026-01-01 00:00:00 +0199",
            "2026-01-01 00:00:00 ++5",
            "2026-01-01 00:00:00 +",
            "",
        ] {
            assert_eq!(parse_ips_timestamp(s), None, "{s}");
        }
        let r = parse_report(
            "{\"timestamp\":\"99999999999-01-01 00:00:00 +0000\"}\n{\"processes\":[{\"name\":\"x\",\"pid\":4294967295,\"reason\":\"r\"}]}",
            42,
        )
        .unwrap();
        assert_eq!(r.at_ms, 42);
        assert_eq!(r.killed[0].pid, Some(u32::MAX));
    }

    #[test]
    fn parses_killed_processes_only() {
        let r = parse_report(REPORT, 7).unwrap();
        assert_eq!(r.at_ms, 1_790_682_201_000);
        assert_eq!(r.largest_process.as_deref(), Some("sd-server"));
        assert_eq!(
            r.killed,
            vec![JetsamVictim {
                name: "spotlightknowledged".into(),
                pid: Some(73341),
                reason: "per-process-limit".into()
            }]
        );
        assert!(parse_report("{}\nnot json", 7).is_none());
        // Bad header → mtime.
        assert_eq!(parse_report("x\n{}", 7).unwrap().at_ms, 7);
    }

    #[test]
    fn scans_directory_with_cache() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("JetsamEvent-2026-09-29-171321.ips"), REPORT).unwrap();
        std::fs::write(dir.path().join("Other-2026.ips"), REPORT).unwrap();
        let mut s = JetsamScanner::new(dir.path());
        let v = s.scan(Duration::from_millis(50)).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, "JetsamEvent-2026-09-29-171321.ips");
        // Cached (no rescan within RESCAN_MS).
        assert_eq!(s.scan(Duration::from_millis(50)).unwrap(), v);
        let mut missing = JetsamScanner::new(dir.path().join("nope"));
        assert!(missing.scan(Duration::from_millis(50)).is_err());
    }
}

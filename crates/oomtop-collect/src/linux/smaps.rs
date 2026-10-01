//! `smaps_rollup` rotation (SPEC §6.2). Reading it walks page tables under the mmap lock, so:
//! the top [`SMAPS_PER_SAMPLE`](super::SMAPS_PER_SAMPLE) processes by RSS are re-read at least every
//! [`TOP_INTERVAL_MS`]; the rest round-robin (never-read first, then oldest) so each is read within
//! [`FULL_CYCLE_MS`]. Between reads the Source hands the decoder the cached values (`<pid>/@smaps_cache`)
//! and the decoder marks the adjusted value as an *estimate*. Pure: time is injected.

use std::collections::{HashMap, HashSet};

/// Top-N refresh period.
pub const TOP_INTERVAL_MS: u64 = 5_000;
/// Every readable process is read at least once per cycle.
pub const FULL_CYCLE_MS: u64 = 60_000;

/// `(pid, starttime ticks)`: pid reuse never inherits a cache entry.
pub type ProcKey = (u32, u64);

/// Values from the last successful read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cached {
    pub read_ms: u64,
    pub pss: u64,
    pub swap_pss: Option<u64>,
    /// `Rss` reported by smaps_rollup at read time (the decoder adjusts the estimate by ΔRSS).
    pub rss: u64,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    last_attempt_ms: u64,
    cached: Option<Cached>,
}

/// Rotation state across samples.
#[derive(Debug, Clone)]
pub struct SmapsScheduler {
    top_n: usize,
    top_interval_ms: u64,
    full_cycle_ms: u64,
    entries: HashMap<ProcKey, Entry>,
    last_plan_ms: Option<u64>,
}

impl SmapsScheduler {
    pub fn new(top_n: usize) -> Self {
        SmapsScheduler {
            top_n,
            top_interval_ms: TOP_INTERVAL_MS,
            full_cycle_ms: FULL_CYCLE_MS,
            entries: HashMap::new(),
            last_plan_ms: None,
        }
    }

    /// Chooses which processes to read now. `candidates` = `(key, rss_bytes)` of processes whose
    /// smaps_rollup is readable. Returned order: top-N due (largest first), then round-robin picks.
    pub fn plan(&mut self, now_ms: u64, candidates: &[(ProcKey, u64)]) -> Vec<ProcKey> {
        let tick = self
            .last_plan_ms
            .map(|t| now_ms.saturating_sub(t))
            .filter(|t| *t > 0)
            .unwrap_or(2_000)
            .min(self.full_cycle_ms);
        self.last_plan_ms = Some(now_ms);
        let mut sorted: Vec<&(ProcKey, u64)> = candidates.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let age = |k: &ProcKey| {
            self.entries
                .get(k)
                .map(|e| now_ms.saturating_sub(e.last_attempt_ms))
        };
        let mut out = Vec::new();
        // Top N: due when the next tick would exceed the interval.
        let (top, rest) = sorted.split_at(sorted.len().min(self.top_n));
        for (k, _) in top {
            match age(k) {
                Some(a) if a + tick / 2 < self.top_interval_ms => {}
                _ => out.push(*k),
            }
        }
        // The rest: a quota that covers everyone within the cycle.
        if !rest.is_empty() {
            let quota = (rest.len() as u64 * tick).div_ceil(self.full_cycle_ms).max(1) as usize;
            // Rank: 0 = read before and past the cycle (must go now), 1 = never read, 2 = the rest; oldest
            // first within a rank, stable by key. Overdue entries always go; never-read ones only within the
            // quota, so a cold start spreads over the first cycle instead of one huge tick.
            let mut order: Vec<(u8, u64, ProcKey)> = rest
                .iter()
                .map(|(k, _)| match age(k) {
                    None => (1, 0, *k),
                    Some(a) if a >= self.full_cycle_ms => (0, a, *k),
                    Some(a) => (2, a, *k),
                })
                .collect();
            order.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
            let overdue = order.iter().filter(|o| o.0 == 0).count();
            out.extend(order.into_iter().take(quota.max(overdue)).map(|o| o.2));
        }
        out
    }

    /// Records a read attempt (`values = None` when it failed, so it is not retried every tick).
    pub fn record(&mut self, key: ProcKey, now_ms: u64, values: Option<(u64, Option<u64>, u64)>) {
        let e = self.entries.entry(key).or_insert(Entry {
            last_attempt_ms: now_ms,
            cached: None,
        });
        e.last_attempt_ms = now_ms;
        if let Some((pss, swap_pss, rss)) = values {
            e.cached = Some(Cached {
                read_ms: now_ms,
                pss,
                swap_pss,
                rss,
            });
        }
    }

    pub fn cached(&self, key: &ProcKey) -> Option<Cached> {
        self.entries.get(key).and_then(|e| e.cached)
    }

    /// Drops state of exited processes.
    pub fn retain(&mut self, alive: &HashSet<ProcKey>) {
        self.entries.retain(|k, _| alive.contains(k));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Text stored under `<pid>/@smaps_cache`.
pub fn cache_to_text(c: &Cached, now_ms: u64) -> String {
    let mut s = format!(
        "Pss: {} kB\nRss: {} kB\nAgeMs: {}\n",
        c.pss / 1024,
        c.rss / 1024,
        now_ms.saturating_sub(c.read_ms)
    );
    if let Some(sw) = c.swap_pss {
        s.push_str(&format!("SwapPss: {} kB\n", sw / 1024));
    }
    s
}

/// The Pss estimate between reads. Growth: cached Pss plus the RSS added since the read (new pages are
/// private, so they count fully in both). Shrink (pages swapped out or freed): Pss scaled by the RSS ratio —
/// subtracting would reach 0 for a process that still holds memory. Never above the current RSS.
pub fn estimate_pss(cached_pss: u64, cached_rss: u64, rss_now: u64) -> u64 {
    let est = if rss_now >= cached_rss {
        cached_pss as u128 + (rss_now - cached_rss) as u128
    } else if cached_rss == 0 {
        0
    } else {
        cached_pss as u128 * rss_now as u128 / cached_rss as u128
    };
    est.min(rss_now as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(n: u32) -> Vec<(ProcKey, u64)> {
        (1..=n).map(|p| ((p, 100), u64::from(p) * 1_000_000)).collect()
    }

    #[test]
    fn top_every_5s_and_everyone_within_60s() {
        let mut s = SmapsScheduler::new(64);
        let c = cands(700);
        let top: HashSet<ProcKey> = c.iter().rev().take(64).map(|(k, _)| *k).collect();
        let mut last_read: HashMap<ProcKey, u64> = HashMap::new();
        let mut max_top_gap = 0;
        let mut t = 0;
        while t <= 120_000 {
            for k in s.plan(t, &c) {
                if let Some(prev) = last_read.insert(k, t) {
                    if top.contains(&k) {
                        max_top_gap = max_top_gap.max(t - prev);
                    }
                }
                s.record(k, t, Some((1, None, 1)));
            }
            if t == 60_000 {
                assert_eq!(last_read.len(), 700, "everyone read within the first cycle");
            }
            t += 2_000;
        }
        assert!(max_top_gap <= TOP_INTERVAL_MS, "top gap {max_top_gap}");
        // Nobody older than one cycle at the end.
        assert!(last_read.values().all(|v| 120_000 - v <= FULL_CYCLE_MS));
    }

    #[test]
    fn per_tick_cost_is_bounded() {
        let mut s = SmapsScheduler::new(64);
        let c = cands(700);
        let first = s.plan(0, &c);
        for k in &first {
            s.record(*k, 0, Some((1, None, 1)));
        }
        // 64 top + ⌈636 × 2 s / 60 s⌉ = 22.
        assert_eq!(first.len(), 64 + 22);
        let second = s.plan(2_000, &c);
        assert_eq!(second.len(), 22, "top not due after 2 s");
    }

    #[test]
    fn failures_are_not_retried_every_tick_and_cache_survives() {
        let mut s = SmapsScheduler::new(2);
        let c = cands(2);
        assert_eq!(s.plan(0, &c).len(), 2);
        s.record((1, 100), 0, None);
        s.record((2, 100), 0, Some((500, Some(10), 800)));
        assert!(s.plan(2_000, &c).is_empty());
        assert_eq!(s.cached(&(2, 100)).unwrap().pss, 500);
        assert!(s.cached(&(1, 100)).is_none());
        s.record((2, 100), 6_000, None);
        assert_eq!(
            s.cached(&(2, 100)).unwrap().read_ms,
            0,
            "failed read keeps old values"
        );
        s.retain(&HashSet::from([(1, 100)]));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn estimate_and_text() {
        assert_eq!(estimate_pss(500, 800, 900), 600);
        assert_eq!(
            estimate_pss(500, 800, 100),
            62,
            "shrunk (swapped out): scaled, never a false 0"
        );
        assert_eq!(estimate_pss(900, 800, 850), 850, "never above current RSS");
        assert_eq!(estimate_pss(900, 1000, 950), 855);
        assert_eq!(estimate_pss(500, 800, 0), 0);
        assert_eq!(estimate_pss(500, 0, 0), 0);
        assert_eq!(estimate_pss(u64::MAX, u64::MAX, u64::MAX - 1), u64::MAX - 1);
        let c = Cached {
            read_ms: 1_000,
            pss: 2048,
            swap_pss: Some(1024),
            rss: 4096,
        };
        assert_eq!(
            cache_to_text(&c, 4_000),
            "Pss: 2 kB\nRss: 4 kB\nAgeMs: 3000\nSwapPss: 1 kB\n"
        );
    }
}

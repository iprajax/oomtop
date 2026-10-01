//! Sampler with tiered cadence (SPEC §6.2): host stats every 1 s; per-process basics every 2 s; expensive
//! reads every 5–10 s or on demand. Failing/slow sources back off exponentially (max 60 s). Each source gets
//! the per-sample budget (≤ 50 ms, SPEC §14).

use crate::decode::decode;
use crate::raw::RawSample;
use crate::{now_ms, Source, SourceError};
use oomtop_core::{OsKind, Snapshot, SourceStatus};
use std::time::Duration;

/// Cadence tier of a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// every `Cadence::host_ms` (1 s)
    Host,
    /// every `Cadence::procs_ms` (2 s)
    Procs,
    /// every `Cadence::expensive_ms` (10 s) or on demand
    Expensive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    pub host_ms: u64,
    pub procs_ms: u64,
    pub expensive_ms: u64,
}

impl Default for Cadence {
    fn default() -> Self {
        Cadence {
            host_ms: 1000,
            procs_ms: 2000,
            expensive_ms: 10_000,
        }
    }
}

impl Cadence {
    pub fn interval_ms(&self, t: Tier) -> u64 {
        match t {
            Tier::Host => self.host_ms,
            Tier::Procs => self.procs_ms,
            Tier::Expensive => self.expensive_ms,
        }
    }
}

/// Options for [`Sampler::platform_default`].
#[derive(Debug, Clone, PartialEq)]
pub struct SamplerOptions {
    pub cadence: Cadence,
    /// Per-source budget (hard cap 50 ms for host sources; the process listing may take up to 4×).
    pub budget: Duration,
    /// Marker-key allowlist (config `privacy.marker_allowlist`).
    pub marker_allowlist: Vec<String>,
}

impl Default for SamplerOptions {
    fn default() -> Self {
        SamplerOptions {
            cadence: Cadence::default(),
            budget: Duration::from_millis(50),
            marker_allowlist: oomtop_core::redact::default_allowlist(),
        }
    }
}

struct Slot {
    source: Box<dyn Source>,
    tier: Tier,
    last_run_ms: Option<u64>,
    last: Option<RawSample>,
    /// The raw sample before `last` (decode needs both for deltas): lets the sampler re-derive this slot's
    /// part of the snapshot after handing its processes out without a copy.
    prev: Option<RawSample>,
    failures: u32,
    backoff_until_ms: u64,
}

/// Runs sources on their cadence and merges decoded partials into one snapshot.
pub struct Sampler {
    slots: Vec<Slot>,
    cadence: Cadence,
    budget: Duration,
    current: Snapshot,
    /// `current.processes` was moved into the last returned snapshot (every source ran, the common case at
    /// the default cadence); a later partial run re-derives it from the slots' raw samples.
    procs_moved: bool,
}

impl std::fmt::Debug for Sampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sampler")
            .field(
                "sources",
                &self.slots.iter().map(|s| s.source.name()).collect::<Vec<_>>(),
            )
            .field("cadence", &self.cadence)
            .finish()
    }
}

fn base_snapshot() -> Snapshot {
    Snapshot {
        host: oomtop_core::HostInfo {
            os: if cfg!(target_os = "macos") {
                OsKind::Macos
            } else if cfg!(target_os = "linux") {
                OsKind::Linux
            } else {
                OsKind::Other
            },
            arch: std::env::consts::ARCH.to_string(),
            ..Default::default()
        },
        self_pid: Some(std::process::id()),
        ..Default::default()
    }
}

impl Sampler {
    pub fn new(sources: Vec<(Box<dyn Source>, Tier)>, cadence: Cadence, budget: Duration) -> Self {
        Sampler {
            slots: sources
                .into_iter()
                .map(|(source, tier)| Slot {
                    source,
                    tier,
                    last_run_ms: None,
                    last: None,
                    prev: None,
                    failures: 0,
                    backoff_until_ms: 0,
                })
                .collect(),
            cadence,
            budget,
            current: base_snapshot(),
            procs_moved: false,
        }
    }

    /// The platform's sources (macOS: host + procs; Linux: host + procs; other: none → all unavailable).
    pub fn platform_default(opts: &SamplerOptions) -> Self {
        #[allow(unused_mut)]
        let mut sources: Vec<(Box<dyn Source>, Tier)> = Vec::new();
        #[cfg(target_os = "macos")]
        {
            sources.push((Box::new(crate::macos::MacHostSource::new()), Tier::Host));
            sources.push((
                Box::new(crate::macos::MacProcSource::new(opts.marker_allowlist.clone())),
                Tier::Procs,
            ));
        }
        #[cfg(target_os = "linux")]
        {
            sources.push((Box::new(crate::linux::LinuxHostSource::default()), Tier::Host));
            sources.push((
                Box::new(crate::linux::LinuxProcSource::new(
                    "/proc",
                    opts.marker_allowlist.clone(),
                )),
                Tier::Procs,
            ));
        }
        Sampler::new(sources, opts.cadence, opts.budget)
    }

    pub fn source_names(&self) -> Vec<&'static str> {
        self.slots.iter().map(|s| s.source.name()).collect()
    }

    /// Latest raw samples (for `tools/capture`).
    pub fn last_raw(&self) -> Vec<RawSample> {
        self.slots.iter().filter_map(|s| s.last.clone()).collect()
    }

    fn run(&mut self, force: bool) -> Snapshot {
        let now = now_ms();
        let mut all_ran = true;
        for slot in &mut self.slots {
            let interval = self.cadence.interval_ms(slot.tier);
            let due = force
                || slot
                    .last_run_ms
                    .map(|t| now.saturating_sub(t) + 5 >= interval)
                    .unwrap_or(true);
            if !due || (!force && now < slot.backoff_until_ms) {
                all_ran = false;
                if self.procs_moved {
                    // This slot's part of the snapshot left with the previous result: re-derive it (pure).
                    if let Some(raw) = &slot.last {
                        decode(raw, slot.prev.as_ref()).apply(&mut self.current);
                    }
                }
                continue;
            }
            let budget = match slot.tier {
                Tier::Procs => self.budget * 4,
                _ => self.budget,
            };
            slot.last_run_ms = Some(now);
            match slot.source.read(budget) {
                Ok(raw) => {
                    let part = decode(&raw, slot.last.as_ref());
                    part.apply(&mut self.current);
                    slot.prev = slot.last.take();
                    slot.last = Some(raw);
                    slot.failures = 0;
                    slot.backoff_until_ms = 0;
                }
                Err(e) => {
                    if self.procs_moved {
                        if let Some(raw) = &slot.last {
                            decode(raw, slot.prev.as_ref()).apply(&mut self.current);
                        }
                    }
                    slot.failures += 1;
                    let backoff = (interval << slot.failures.min(6)).min(60_000);
                    slot.backoff_until_ms = now + backoff;
                    let status = match e {
                        SourceError::Unavailable(r) => SourceStatus::Unavailable(r),
                        SourceError::Timeout(ms) => SourceStatus::Partial(format!("timed out after {ms} ms")),
                        SourceError::Io(r) => SourceStatus::Unavailable(r),
                    };
                    self.current
                        .source_status
                        .insert(slot.source.name().to_string(), status);
                }
            }
        }
        if self.slots.is_empty() {
            self.current.source_status.insert(
                "platform".into(),
                SourceStatus::Unavailable("no collectors for this OS".into()),
            );
        }
        self.current.taken_at_ms = now;
        self.current.self_pid = Some(std::process::id());
        // Every source ran: hand the process list out instead of deep-copying ~700 processes (their argv
        // and per-value source strings) every refresh (SPEC §14 CPU budget). Otherwise clone as usual.
        if all_ran && !self.slots.is_empty() {
            let procs = std::mem::take(&mut self.current.processes);
            let mut out = self.current.clone();
            out.processes = procs;
            self.procs_moved = true;
            out
        } else {
            self.procs_moved = false;
            self.current.clone()
        }
    }

    /// Runs the sources that are due (all of them on the first call) and returns the merged snapshot.
    /// Groups/model servers/sandboxes are left empty — attribution and adapters run in the caller's pipeline.
    pub fn sample_once(&mut self) -> Snapshot {
        self.run(false)
    }

    /// Runs every source now, ignoring cadence and backoff (MCP on-demand sampling).
    pub fn sample_all(&mut self) -> Snapshot {
        self.run(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::RawPayload;

    struct Failing;
    impl Source for Failing {
        fn name(&self) -> &'static str {
            "test.failing"
        }
        fn read(&mut self, _b: Duration) -> Result<RawSample, SourceError> {
            Err(SourceError::Unavailable("nope".into()))
        }
    }

    struct Fixed;
    impl Source for Fixed {
        fn name(&self) -> &'static str {
            "test.fixed"
        }
        fn read(&mut self, _b: Duration) -> Result<RawSample, SourceError> {
            Ok(RawSample {
                source: "test.fixed".into(),
                taken_at_ms: now_ms(),
                read_us: 0,
                payload: RawPayload::Unavailable {
                    reason: "fixture".into(),
                },
            })
        }
    }

    #[test]
    fn failing_sources_are_unavailable_not_panics() {
        let mut s = Sampler::new(
            vec![(Box::new(Failing), Tier::Host), (Box::new(Fixed), Tier::Procs)],
            Cadence::default(),
            Duration::from_millis(50),
        );
        let snap = s.sample_once();
        assert_eq!(
            snap.source_status.get("test.failing"),
            Some(&SourceStatus::Unavailable("nope".into()))
        );
        assert_eq!(
            snap.source_status.get("test.fixed"),
            Some(&SourceStatus::Unavailable("fixture".into()))
        );
        assert_eq!(snap.self_pid, Some(std::process::id()));
    }

    /// A process source whose listing changes on every read (so a stale copy would be visible).
    struct Procs(u32);
    impl Source for Procs {
        fn name(&self) -> &'static str {
            "test.procs"
        }
        fn read(&mut self, _b: Duration) -> Result<RawSample, SourceError> {
            self.0 += 1;
            Ok(RawSample {
                source: "macos.procs".into(),
                taken_at_ms: now_ms(),
                read_us: 0,
                payload: RawPayload::MacProcs(crate::raw::MacProcsRaw {
                    procs: (0..3)
                        .map(|i| crate::raw::MacProcRaw {
                            pid: 100 + i,
                            start_tvsec: 1,
                            name: format!("p{i}-{}", self.0),
                            argv: Some(vec![format!("p{i}"), "--flag".into()]),
                            ..Default::default()
                        })
                        .collect(),
                    timebase_numer: 1,
                    timebase_denom: 1,
                    ..Default::default()
                }),
            })
        }
    }

    #[test]
    fn processes_survive_a_partial_run_after_being_handed_out() {
        let mut s = Sampler::new(
            vec![(Box::new(Fixed), Tier::Host), (Box::new(Procs(0)), Tier::Procs)],
            Cadence {
                host_ms: 1,
                procs_ms: 60_000,
                expensive_ms: 60_000,
            },
            Duration::from_millis(50),
        );
        let first = s.sample_once(); // every source runs: processes are moved out
        assert_eq!(first.processes.len(), 3);
        std::thread::sleep(Duration::from_millis(10));
        let second = s.sample_once(); // only the host tier is due
        assert_eq!(
            second.processes, first.processes,
            "re-derived, not lost or re-read"
        );
        let third = s.sample_all(); // forced: a fresh listing
        assert_eq!(third.processes[0].name, "p0-2");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn platform_sampler_reads_memory() {
        let mut s = Sampler::platform_default(&SamplerOptions::default());
        let snap = s.sample_all();
        assert!(
            snap.memory.total.value.unwrap_or(0) > 0,
            "{:?}",
            snap.source_status
        );
        assert!(!snap.processes.is_empty());
    }
}

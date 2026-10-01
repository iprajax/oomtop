//! Pure decoders: `decode(raw, prev) -> PartialSnapshot` (SPEC §6.1). No I/O, compiled on every OS so
//! fixtures from any machine replay anywhere.

pub mod linux;
pub mod macos;
pub mod macos_extras;

use crate::raw::{RawPayload, RawSample};
use oomtop_core::{
    Accelerator, HostCpu, HostInfo, HostMemory, Oom, ProcState, Process, Snapshot, SourceStatus, TaskCounts,
    Thermal,
};
use std::collections::BTreeMap;

/// The part of a snapshot one source contributes. `None` fields are left untouched when applied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PartialSnapshot {
    pub host: Option<HostInfo>,
    pub memory: Option<HostMemory>,
    pub cpu: Option<HostCpu>,
    pub oom: Option<Oom>,
    pub accelerators: Option<Vec<Accelerator>>,
    pub thermal: Option<Thermal>,
    pub processes: Option<Vec<Process>>,
    pub status: BTreeMap<String, SourceStatus>,
}

impl PartialSnapshot {
    pub fn unavailable(source: &str, reason: impl Into<String>) -> Self {
        let mut status = BTreeMap::new();
        status.insert(source.to_string(), SourceStatus::Unavailable(reason.into()));
        PartialSnapshot {
            status,
            ..Default::default()
        }
    }

    /// Merges into `s`: present fields replace; host info merges non-empty fields.
    pub fn apply(self, s: &mut Snapshot) {
        if let Some(h) = self.host {
            merge_host(&mut s.host, h);
        }
        if let Some(m) = self.memory {
            s.memory = m;
        }
        if let Some(c) = self.cpu {
            s.cpu = c;
        }
        if let Some(o) = self.oom {
            s.oom = o;
        }
        if let Some(a) = self.accelerators {
            s.accelerators = a;
        }
        if let Some(t) = self.thermal {
            s.thermal = t;
        }
        if let Some(p) = self.processes {
            s.processes = p;
        }
        s.source_status.extend(self.status);
        s.cpu.tasks = task_counts(&s.processes);
    }
}

/// Header "Tasks" line from the process list: process count, thread sum where readable, running count.
pub fn task_counts(ps: &[Process]) -> TaskCounts {
    let mut threads = None::<u32>;
    for t in ps.iter().filter_map(|p| p.threads) {
        threads = Some(threads.unwrap_or(0).saturating_add(t));
    }
    TaskCounts {
        processes: ps.len() as u32,
        threads,
        running: ps.iter().filter(|p| p.state == ProcState::Running).count() as u32,
    }
}

fn merge_host(dst: &mut HostInfo, src: HostInfo) {
    if !src.hostname.is_empty() {
        dst.hostname = src.hostname;
    }
    if src.os != oomtop_core::OsKind::Other {
        dst.os = src.os;
    }
    if !src.os_version.is_empty() {
        dst.os_version = src.os_version;
    }
    if !src.arch.is_empty() {
        dst.arch = src.arch;
    }
    dst.model = src.model.or(dst.model.take());
    dst.cpu_brand = src.cpu_brand.or(dst.cpu_brand.take());
    if src.cores_logical > 0 {
        dst.cores_logical = src.cores_logical;
    }
    dst.cores_performance = src.cores_performance.or(dst.cores_performance);
    dst.cores_efficiency = src.cores_efficiency.or(dst.cores_efficiency);
    if src.mem_total > 0 {
        dst.mem_total = src.mem_total;
    }
    dst.unified_memory |= src.unified_memory;
    dst.fanless = src.fanless.or(dst.fanless);
    if src.page_size > 0 {
        dst.page_size = src.page_size;
    }
    dst.boot_time_ms = src.boot_time_ms.or(dst.boot_time_ms);
}

/// Decodes one raw sample; `prev` is the previous sample of the same source (for rates / CPU %).
pub fn decode(raw: &RawSample, prev: Option<&RawSample>) -> PartialSnapshot {
    let prev_payload = prev.filter(|p| p.source == raw.source && p.taken_at_ms < raw.taken_at_ms);
    let dt_ms = prev_payload.map(|p| raw.taken_at_ms - p.taken_at_ms);
    match &raw.payload {
        RawPayload::Unavailable { reason } => PartialSnapshot::unavailable(&raw.source, reason.clone()),
        RawPayload::MacHost(h) => {
            let p = prev_payload.and_then(|p| match &p.payload {
                RawPayload::MacHost(x) => Some(x),
                _ => None,
            });
            // Host counters + thermal / power / GPU / jetsam extras (all pure, any OS).
            macos_extras::decode_host_full(&raw.source, h, p, dt_ms, raw.taken_at_ms)
        }
        RawPayload::MacProcs(ps) => {
            let p = prev_payload.and_then(|p| match &p.payload {
                RawPayload::MacProcs(x) => Some(x),
                _ => None,
            });
            macos::decode_procs(&raw.source, ps, p, dt_ms)
        }
        RawPayload::LinuxFiles(f) => {
            let p = prev_payload.and_then(|p| match &p.payload {
                RawPayload::LinuxFiles(x) => Some(x),
                _ => None,
            });
            #[allow(unused_mut)]
            let mut part = linux::decode_files(&raw.source, f, p, dt_ms);
            // cgroup, OOM killers, thermal, GPU, per-process GPU/io and model-file enrichment.
            #[cfg(unix)]
            crate::linux::enrich(&raw.source, f, p, dt_ms, &mut part);
            part
        }
    }
}

#[cfg(test)]
mod task_tests {
    use super::*;

    #[test]
    fn tasks_count_processes_threads_and_running() {
        let mk = |state, threads| Process {
            state,
            threads,
            ..Default::default()
        };
        let t = task_counts(&[
            mk(ProcState::Running, Some(4)),
            mk(ProcState::Sleeping, Some(2)),
            mk(ProcState::Unknown, None),
        ]);
        assert_eq!(t.processes, 3);
        assert_eq!(t.threads, Some(6));
        assert_eq!(t.running, 1);
        assert_eq!(task_counts(&[mk(ProcState::Unknown, None)]).threads, None);

        let mut s = Snapshot::default();
        PartialSnapshot {
            processes: Some(vec![mk(ProcState::Running, Some(1))]),
            ..Default::default()
        }
        .apply(&mut s);
        // A later host-only partial replaces `cpu` but the task counts are kept in sync.
        PartialSnapshot {
            cpu: Some(HostCpu::default()),
            ..Default::default()
        }
        .apply(&mut s);
        assert_eq!(s.cpu.tasks.processes, 1);
    }
}

//! stable-diffusion.cpp `sd-server`: `/sdcpp/v1/capabilities` (reachability, model info) and job state.
//!
//! sd-server tracks jobs per id (`/sdcpp/v1/jobs/{id}` → `queued | generating | completed | failed |
//! cancelled`). A job *list* (`GET /sdcpp/v1/jobs`) is used when the build exposes one; otherwise busy is
//! inferred from CPU activity (`Estimate`). Weight files come from argv (`--diffusion-model`, `--vae`, `--llm`…).
//! A running job cannot be cancelled, so stopping the server while busy is refused ([`super::stop_guard`]).

use crate::http::get;
use crate::AdapterError;
use oomtop_core::{JobProgress, Measured, ModelServer, SourceStatus};
use std::time::Duration;

/// Job states counted as active.
pub const ACTIVE_STATES: &[&str] = &["generating", "running", "processing", "in_progress"];
pub const QUEUED_STATES: &[&str] = &["queued", "pending", "waiting"];

/// Summary of a job list.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Jobs {
    pub active: u32,
    pub queued: u32,
    pub progress: Option<JobProgress>,
}

fn u32_of(v: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|k| v.get(*k).and_then(|x| x.as_u64()))
        .map(|x| x.min(u32::MAX as u64) as u32)
}

/// Parses a job list: a bare array or `{jobs|data|items: [...]}` of `{status, step?, steps?}` objects.
pub fn parse_jobs(v: &serde_json::Value) -> Option<Jobs> {
    let arr = v.as_array().or_else(|| {
        ["jobs", "data", "items"]
            .iter()
            .find_map(|k| v.get(*k).and_then(|x| x.as_array()))
    })?;
    let mut j = Jobs::default();
    for job in arr {
        let st = job
            .get("status")
            .or_else(|| job.get("state"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ACTIVE_STATES.contains(&st.as_str()) {
            j.active += 1;
            if j.progress.is_none() {
                let done = u32_of(job, &["step", "current_step", "progress_step"]);
                let total = u32_of(job, &["steps", "total_steps", "sample_steps"]);
                if let (Some(done), Some(total)) = (done, total) {
                    j.progress = Some(JobProgress {
                        done,
                        total,
                        label: "generating".into(),
                    });
                }
            }
        } else if QUEUED_STATES.contains(&st.as_str()) {
            j.queued += 1;
        }
    }
    Some(j)
}

/// Queue information some builds put into the capabilities document.
pub fn parse_capabilities_queue(v: &serde_json::Value) -> (Option<u32>, Option<u32>) {
    let active = u32_of(v, &["active_jobs", "running_jobs", "running"]);
    let queued = u32_of(v, &["queued_jobs", "pending_jobs", "queue_length", "queue_size"]);
    let busy_flag = v.get("busy").and_then(|x| x.as_bool()).map(|b| b as u32);
    (active.or(busy_flag), queued)
}

pub(crate) fn enrich(ms: &mut ModelServer, ep: &str, timeout: Duration) -> Result<(), AdapterError> {
    let caps = get(&format!("{ep}/sdcpp/v1/capabilities"), timeout)?.json()?;
    let (cap_active, cap_queued) = parse_capabilities_queue(&caps);
    let jobs = match get(&format!("{ep}/sdcpp/v1/jobs"), timeout) {
        Ok(r) if r.is_success() => r.json().ok().as_ref().and_then(parse_jobs),
        _ => None,
    };
    match (jobs, cap_active) {
        (Some(j), _) => {
            ms.busy = Measured::exact(j.active > 0, "sd-server /sdcpp/v1/jobs");
            ms.queue = Measured::exact(j.queued, "sd-server /sdcpp/v1/jobs");
            ms.progress = j.progress;
            ms.status = SourceStatus::Available;
        }
        (None, Some(a)) => {
            ms.busy = Measured::exact(a > 0, "sd-server capabilities");
            if let Some(q) = cap_queued {
                ms.queue = Measured::exact(q, "sd-server capabilities");
            }
            ms.status = SourceStatus::Available;
        }
        (None, None) => {
            ms.queue = Measured::unavailable("sd-server", "job state is exposed per job id only");
            ms.status = SourceStatus::Partial("job list not exposed; busy inferred from CPU".into());
        }
    }
    Ok(())
}

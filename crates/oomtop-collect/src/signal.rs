//! Process signalling with identity re-verification (SPEC §13): `(pid, start_time)` is re-checked
//! immediately before any signal, so pid reuse can never hit a different process.
//!
//! This is the only place oomtop sends signals. Callers (the CLI actuator) must have explicit user
//! confirmation; SIGKILL needs a second confirmation (enforced by `oomtop_core::actions::ActionPlan`).

use oomtop_core::ProcId;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
    Stop,
    Cont,
}

impl Signal {
    fn raw(self) -> libc::c_int {
        match self {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
            Signal::Stop => libc::SIGSTOP,
            Signal::Cont => libc::SIGCONT,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SignalError {
    #[error("process {0} is gone")]
    Gone(u32),
    #[error("pid {pid} was reused (start time {found} ≠ {expected}); refusing to signal")]
    IdentityMismatch { pid: u32, expected: u64, found: u64 },
    #[error("permission denied for pid {0}")]
    Permission(u32),
    #[error("refusing to signal pid {0}")]
    Refused(u32),
    #[error("signal failed: {0}")]
    Os(String),
}

/// Start time in ms since the Unix epoch — the same value decoders put into `ProcId::start_time`.
pub fn process_start_time_ms(pid: u32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::start_time_ms(pid)
    }
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let st = crate::decode::linux::parse_stat(&stat)?;
        let sys = std::fs::read_to_string("/proc/stat").ok()?;
        let btime: u64 = sys
            .lines()
            .find_map(|l| l.strip_prefix("btime ").and_then(|v| v.trim().parse().ok()))?;
        Some(crate::decode::linux::start_time_ms(
            btime,
            st.starttime,
            crate::linux::clk_tck(),
        ))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

/// True if the process with this identity still exists.
pub fn is_alive(id: ProcId) -> bool {
    process_start_time_ms(id.pid) == Some(id.start_time)
}

/// Re-verifies identity, then sends the signal. Refuses pid ≤ 1 and oomtop's own pid.
pub fn send(id: ProcId, sig: Signal) -> Result<(), SignalError> {
    if id.pid <= 1 || id.pid == std::process::id() || id.pid > i32::MAX as u32 {
        return Err(SignalError::Refused(id.pid));
    }
    match process_start_time_ms(id.pid) {
        None => return Err(SignalError::Gone(id.pid)),
        Some(found) if found != id.start_time => {
            return Err(SignalError::IdentityMismatch {
                pid: id.pid,
                expected: id.start_time,
                found,
            })
        }
        Some(_) => {}
    }
    // SAFETY: plain kill(2) on a verified pid.
    let r = unsafe { libc::kill(id.pid as libc::pid_t, sig.raw()) };
    if r == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    Err(match e.raw_os_error() {
        Some(libc::ESRCH) => SignalError::Gone(id.pid),
        Some(libc::EPERM) => SignalError::Permission(id.pid),
        _ => SignalError::Os(e.to_string()),
    })
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn verifies_identity_before_signalling_own_child() {
        // Only ever signals a child this test spawned.
        let mut child = Command::new("sleep").arg("600").spawn().expect("spawn sleep");
        let pid = child.id();
        let st = process_start_time_ms(pid).expect("start time");
        let wrong = ProcId::new(pid, st + 1);
        assert!(matches!(
            send(wrong, Signal::Term),
            Err(SignalError::IdentityMismatch { .. })
        ));
        assert!(is_alive(ProcId::new(pid, st)));
        send(ProcId::new(pid, st), Signal::Stop).expect("stop");
        send(ProcId::new(pid, st), Signal::Cont).expect("cont");
        send(ProcId::new(pid, st), Signal::Term).expect("term");
        let status = child.wait().expect("wait");
        assert!(!status.success());
        assert!(matches!(
            send(ProcId::new(1, 0), Signal::Term),
            Err(SignalError::Refused(1))
        ));
    }
}

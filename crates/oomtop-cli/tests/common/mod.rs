//! Shared test helpers.

use std::process::{Child, ExitStatus};

/// Owns a spawned `oomtop` child and kills + reaps it on drop, so a failing assertion (or a panic
/// anywhere in the test) never leaves a server running. `oomtop serve --listen 127.0.0.1:0` children
/// leaked this way once ran for hours at ~8 % CPU each.
pub struct Reaped(Option<Child>);

#[allow(dead_code)] // not every test binary uses every helper
impl Reaped {
    pub fn new(child: Child) -> Self {
        Reaped(Some(child))
    }

    pub fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("child already taken")
    }

    /// Waits for a normal exit; the guard is disarmed only once the child is reaped.
    pub fn wait(mut self) -> std::io::Result<ExitStatus> {
        let status = self.child().wait();
        if status.is_ok() {
            self.0 = None;
        }
        status
    }

    /// Disarms the guard and hands the child back (for `wait_with_output`).
    pub fn into_inner(mut self) -> Child {
        self.0.take().expect("child already taken")
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            if matches!(c.try_wait(), Ok(None)) {
                let _ = c.kill();
            }
            let _ = c.wait();
        }
    }
}

//! Frontend-facing traits. Frontends (`tui`, `mcp`, `serve`) never touch OS APIs: they receive a
//! [`SnapshotProvider`] (and optionally an [`crate::actions::Actuator`]) built by `oomtop-cli`.

use crate::headroom::HeadroomConfig;
use crate::history::History;
use crate::model::Snapshot;

/// Produces enriched snapshots (sampled, attributed, adapters probed, idle/orphan/forecast applied).
pub trait SnapshotProvider: Send {
    /// Takes a fresh sample (the TUI/serve call this on their refresh cadence; MCP per call).
    fn snapshot(&mut self) -> Snapshot;

    /// Like [`Self::snapshot`] but samples every source now, ignoring the cadence (the TUI's second
    /// sample 500 ms after start, so CPU % appears within a second; a refresh right after an action).
    fn snapshot_now(&mut self) -> Snapshot {
        self.snapshot()
    }

    /// History ring buffer of samples taken so far (may be empty for one-shot providers).
    fn history(&self) -> &History;

    /// Headroom tunables from config.
    fn headroom_config(&self) -> HeadroomConfig {
        HeadroomConfig::default()
    }
}

/// A provider that always returns the same snapshot (tests, fixtures, `--replay`).
#[derive(Debug, Clone, Default)]
pub struct StaticProvider {
    pub snapshot: Snapshot,
    pub history: History,
    pub headroom: HeadroomConfig,
}

impl StaticProvider {
    pub fn new(snapshot: Snapshot) -> Self {
        let mut history = History::default();
        history.push_snapshot(&snapshot);
        StaticProvider {
            snapshot,
            history,
            headroom: HeadroomConfig::default(),
        }
    }
}

impl SnapshotProvider for StaticProvider {
    fn snapshot(&mut self) -> Snapshot {
        self.snapshot.clone()
    }
    fn history(&self) -> &History {
        &self.history
    }
    fn headroom_config(&self) -> HeadroomConfig {
        self.headroom.clone()
    }
}

/// A provider replaying a sequence of snapshots, then repeating the last one.
#[derive(Debug, Clone, Default)]
pub struct SequenceProvider {
    snapshots: Vec<Snapshot>,
    next: usize,
    history: History,
}

impl SequenceProvider {
    pub fn new(snapshots: Vec<Snapshot>) -> Self {
        SequenceProvider {
            snapshots,
            next: 0,
            history: History::default(),
        }
    }
}

impl SnapshotProvider for SequenceProvider {
    fn snapshot(&mut self) -> Snapshot {
        let s = match self.snapshots.get(self.next) {
            Some(s) => {
                self.next += 1;
                s.clone()
            }
            None => self.snapshots.last().cloned().unwrap_or_default(),
        };
        self.history.push_snapshot(&s);
        s
    }
    fn history(&self) -> &History {
        &self.history
    }
}

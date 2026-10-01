//! Fixture replay (SPEC §17): decode recorded `RawSample`s on any OS — no hardware in CI.
//!
//! Fixture file format (`fixtures/<os>/<name>.json`, see `fixtures/*/README.md`):
//! ```json
//! { "meta": { "name": "m5-air-studio", "os": "macos", "captured_at_ms": 0, "description": "…" },
//!   "frames": [ [ RawSample, RawSample ], [ … ] ] }
//! ```
//! One frame = the samples of one sampler tick. Replaying yields one `Snapshot` per frame, each decoded
//! with the previous sample of the same source as `prev` (so rates and CPU % work).

use crate::decode::decode;
use crate::raw::RawSample;
use oomtop_core::Snapshot;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FixtureMeta {
    pub name: String,
    pub os: String,
    pub captured_at_ms: u64,
    pub description: String,
    /// Free-form capture context (thermal state, battery, workload).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Fixture {
    pub meta: FixtureMeta,
    pub frames: Vec<Vec<RawSample>>,
}

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("reading {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("parsing {path}: {source}")]
    Parse { path: String, source: serde_json::Error },
}

/// Loads a fixture file.
pub fn load_fixture(path: &Path) -> Result<Fixture, ReplayError> {
    let text = std::fs::read_to_string(path).map_err(|source| ReplayError::Io {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| ReplayError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Serializes a fixture (pretty JSON) — used by `tools/capture`.
pub fn fixture_to_json(f: &Fixture) -> String {
    serde_json::to_string_pretty(f).unwrap_or_else(|_| "{}".to_string())
}

/// Replays frames into snapshots (pure).
pub fn replay(fixture: &Fixture) -> Vec<Snapshot> {
    let mut prev: HashMap<String, RawSample> = HashMap::new();
    let mut current = Snapshot::default();
    let mut out = Vec::with_capacity(fixture.frames.len());
    for frame in &fixture.frames {
        let mut t = current.taken_at_ms;
        for raw in frame {
            let part = decode(raw, prev.get(&raw.source));
            part.apply(&mut current);
            t = t.max(raw.taken_at_ms);
            prev.insert(raw.source.clone(), raw.clone());
        }
        current.taken_at_ms = t;
        out.push(current.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::{LinuxFilesRaw, RawPayload};

    #[test]
    fn roundtrip_and_replay() {
        let mut files = LinuxFilesRaw {
            clk_tck: 100,
            page_size: 4096,
            ..Default::default()
        };
        files
            .files
            .insert("meminfo".into(), "MemTotal: 100 kB\nMemAvailable: 40 kB\n".into());
        let f = Fixture {
            meta: FixtureMeta {
                name: "t".into(),
                os: "linux".into(),
                ..Default::default()
            },
            frames: vec![vec![RawSample {
                source: "linux.host".into(),
                taken_at_ms: 10,
                read_us: 1,
                payload: RawPayload::LinuxFiles(files),
            }]],
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.json");
        std::fs::write(&p, fixture_to_json(&f)).unwrap();
        let back = load_fixture(&p).unwrap();
        assert_eq!(back, f);
        let snaps = replay(&back);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].memory.available.value, Some(40 * 1024));
        assert_eq!(snaps[0].taken_at_ms, 10);
        assert!(load_fixture(&dir.path().join("missing.json")).is_err());
    }
}

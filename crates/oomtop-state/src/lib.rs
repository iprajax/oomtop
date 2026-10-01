//! # oomtop-state
//!
//! SQLite (WAL) store at `$XDG_STATE_HOME/oomtop/state.db` (default `~/.local/state/oomtop/`) on both OSes
//! (UX §6): the **lineage journal** (SPEC §6.2), the **profile / personalization store** (UX §5–6) and the
//! local **impression → selection log** (UX §5.6). No telemetry; nothing leaves the machine.
//!
//! **Batched writes** (UX §10): lineage, entity sightings, learning events, impressions and queries are queued
//! in memory and written in one transaction every [`BATCH_INTERVAL`] (30 s), on [`StateDb::flush`] and on drop.
//! Reads merge the queue, so callers always see their own writes. Explicit user actions (pin, mute, rename,
//! reset) are written immediately.
//!
//! **Learning off** (`--no-learn`, `personalization.learn = false`): [`StateDb::set_learning`]`(false)` drops
//! learning signals (events, impressions, queries, entity sightings). The lineage journal still works (it is
//! operational data for orphans/idle, not personalization), and explicit pins/mutes/renames are still saved
//! because the user asked for them.
//!
//! **Retention** (UX §5.6): the log is capped at 30 days and 5 MB ([`StateDb::prune`]); negative signals
//! decay with a 30-day half-life.
//!
//! **Privacy** (SPEC §13): only fingerprints (hashes of placeholder-ized templates), derived display names,
//! user aliases, hashed session ids and the user's own queries (secret-looking parts redacted) are stored —
//! never raw command lines or environments. The database file is created `0600`.

pub mod profile;

pub use profile::{EntityScore, ProfileSummary};

use oomtop_core::attribution::LineageEntry;
use oomtop_core::ranking::frecency;
use oomtop_core::{GroupKind, ProcId};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;

pub const SCHEMA_VERSION: u32 = 1;
/// Log retention (UX §5.6): 30 days.
pub const RETENTION_MS: u64 = 30 * 24 * 3600 * 1000;
/// Log size cap (UX §5.6): 5 MB.
pub const LOG_CAP_BYTES: u64 = 5 * 1024 * 1024;
/// Negative signals (mute / show less) decay slowly (UX §5.1): 30 days half-life.
pub const NEGATIVE_HALF_LIFE_S: f64 = 30.0 * 86400.0;
/// Batched writes (UX §10): every 30 s and on exit.
pub const BATCH_INTERVAL: Duration = Duration::from_secs(30);
/// Queued lineage entries that force an early flush (bounds memory).
pub const MAX_PENDING_LINEAGE: usize = 50_000;
/// Queued log rows that force an early flush.
pub const MAX_PENDING_ROWS: usize = 5_000;
/// How often a flush also prunes.
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

#[derive(Debug, Error)]
pub enum StateError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("state database schema v{found} is newer than this oomtop (v{SCHEMA_VERSION}); upgrade oomtop or move {path}")]
    NewerSchema { found: u32, path: String },
}

pub type Result<T> = std::result::Result<T, StateError>;

/// Default database path: `$XDG_STATE_HOME/oomtop/state.db` or `~/.local/state/oomtop/state.db` (a relative
/// `XDG_STATE_HOME` is ignored, per the XDG spec).
pub fn default_path() -> PathBuf {
    let base = match std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        Some(v) => v,
        None => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local/state"),
    };
    base.join("oomtop").join("state.db")
}

/// Learning signals (UX §5.1) with fixed v1 weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// Select / expand an entity (implicit positive, medium).
    Select,
    /// Filter/search that matched, then selected (strong positive).
    SearchSelect,
    /// Stop, suspend, rename (explicit positive).
    Action,
    /// Pin (explicit, persistent).
    Pin,
    /// Mute (explicit negative).
    Mute,
    /// "Show less like this" (explicit negative).
    Less,
}

impl EventKind {
    pub const ALL: [EventKind; 6] = [
        Self::Select,
        Self::SearchSelect,
        Self::Action,
        Self::Pin,
        Self::Mute,
        Self::Less,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Select => "select",
            EventKind::SearchSelect => "search_select",
            EventKind::Action => "action",
            EventKind::Pin => "pin",
            EventKind::Mute => "mute",
            EventKind::Less => "less",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }
    pub fn weight(self) -> f64 {
        match self {
            EventKind::Select => 1.0,
            EventKind::SearchSelect => 2.0,
            EventKind::Action => 2.0,
            EventKind::Pin => 3.0,
            EventKind::Mute => -2.0,
            EventKind::Less => -1.0,
        }
    }
    pub fn is_negative(self) -> bool {
        self.weight() < 0.0
    }
}

/// A remembered entity (UX §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct EntityRecord {
    pub fingerprint: String,
    pub display_name: String,
    /// User rename (`n`), usable in queries.
    pub alias: Option<String>,
    pub pinned: bool,
    pub muted_until_ms: Option<u64>,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
}

impl EntityRecord {
    /// Alias if renamed, else the derived display name.
    pub fn name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.display_name)
    }
    pub fn is_muted(&self, now_ms: u64) -> bool {
        self.muted_until_ms.is_some_and(|u| u > now_ms)
    }
}

/// One served list and what the user picked (UX §5.6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Impression {
    pub at_ms: u64,
    /// Fingerprints in served order (top N).
    pub served: Vec<String>,
    pub selected: Option<String>,
    pub keystrokes: u32,
    /// The user typed a query before selecting.
    pub typed: bool,
    /// The query was retyped (reformulation).
    pub reformulated: bool,
    /// A served insight was dismissed/muted.
    pub dismissed: bool,
}

/// `oomtop profile stats`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct UxStats {
    pub impressions: u64,
    pub selections: u64,
    /// Selected entity was in the top 3 without typing.
    pub hit_at_3: Option<f64>,
    pub mean_keystrokes: Option<f64>,
    pub reformulation_rate: Option<f64>,
    pub dismiss_rate: Option<f64>,
}

/// What [`StateDb::prune_with`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct PruneReport {
    pub impressions: u64,
    pub events: u64,
    pub queries: u64,
    pub lineage: u64,
    pub expired_mutes: u64,
}

#[derive(Debug, Default)]
struct Pending {
    lineage: BTreeMap<ProcId, LineageEntry>,
    /// fingerprint → (display name, first seen in batch, last seen in batch)
    entities: BTreeMap<String, (String, u64, u64)>,
    events: Vec<(String, EventKind, u64, Option<String>)>,
    impressions: Vec<Impression>,
    /// query → (count, last_ms)
    queries: BTreeMap<String, (u64, u64)>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.lineage.is_empty()
            && self.entities.is_empty()
            && self.events.is_empty()
            && self.impressions.is_empty()
            && self.queries.is_empty()
    }
    fn rows(&self) -> usize {
        self.entities.len() + self.events.len() + self.impressions.len() + self.queries.len()
    }
}

pub struct StateDb {
    conn: Connection,
    path: Option<PathBuf>,
    learn: bool,
    interval: Duration,
    last_flush: Instant,
    last_prune: Option<Instant>,
    pending: Pending,
}

impl std::fmt::Debug for StateDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateDb")
            .field("path", &self.path)
            .field("learn", &self.learn)
            .finish()
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS lineage (
  pid INTEGER NOT NULL, start_time INTEGER NOT NULL, ppid INTEGER,
  group_id TEXT NOT NULL, group_kind TEXT NOT NULL, group_label TEXT NOT NULL, group_fp TEXT NOT NULL,
  session_id TEXT, spawned_by_agent INTEGER NOT NULL DEFAULT 0,
  first_seen_ms INTEGER NOT NULL, last_seen_ms INTEGER NOT NULL, last_active_ms INTEGER NOT NULL,
  PRIMARY KEY (pid, start_time));
CREATE INDEX IF NOT EXISTS lineage_seen ON lineage(last_seen_ms);
CREATE TABLE IF NOT EXISTS entity (
  fingerprint TEXT PRIMARY KEY, display_name TEXT NOT NULL, alias TEXT, pinned INTEGER NOT NULL DEFAULT 0,
  muted_until_ms INTEGER, first_seen_ms INTEGER NOT NULL, last_seen_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS event (
  id INTEGER PRIMARY KEY, fingerprint TEXT NOT NULL, kind TEXT NOT NULL, weight REAL NOT NULL,
  at_ms INTEGER NOT NULL, mode TEXT);
CREATE INDEX IF NOT EXISTS event_fp ON event(fingerprint);
CREATE INDEX IF NOT EXISTS event_at ON event(at_ms);
CREATE TABLE IF NOT EXISTS impression (
  id INTEGER PRIMARY KEY, at_ms INTEGER NOT NULL, served TEXT NOT NULL, selected TEXT,
  keystrokes INTEGER NOT NULL, typed INTEGER NOT NULL, reformulated INTEGER NOT NULL, dismissed INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS impression_at ON impression(at_ms);
CREATE TABLE IF NOT EXISTS query (text TEXT PRIMARY KEY, count INTEGER NOT NULL, last_ms INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS machine_profile (id INTEGER PRIMARY KEY CHECK (id = 1), json TEXT NOT NULL, updated_ms INTEGER NOT NULL);
";

/// Creates the state directory (and missing parents) as `0700` on Unix. An existing directory is left alone.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Makes sure the database file exists with mode `0600` **before** SQLite opens it: SQLite creates the `-wal`
/// and `-shm` files with the main file's permissions, so this keeps the whole store private (SPEC §13). A
/// pre-existing file readable by group/others is tightened.
fn secure_db_file(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = std::fs::metadata(path)?;
                if meta.is_file() && meta.permissions().mode() & 0o077 != 0 {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
                }
            }
            Err(e) => return Err(e),
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn is_hashed(s: &str) -> bool {
    s.len() == 16
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Lineage upsert semantics (shared by the SQL upsert and in-memory merging): keeps `first_seen`, the
/// original ppid (> 1) and the agent attribution once set; last seen/active only move forward.
pub fn merge_lineage(old: &LineageEntry, new: &LineageEntry) -> LineageEntry {
    let keep_group = old.spawned_by_agent && !new.spawned_by_agent;
    let g = if keep_group { old } else { new };
    LineageEntry {
        id: old.id,
        ppid: match old.ppid {
            Some(p) if p > 1 => Some(p),
            _ => new.ppid,
        },
        group_id: g.group_id.clone(),
        group_kind: g.group_kind,
        group_label: g.group_label.clone(),
        group_fingerprint: g.group_fingerprint.clone(),
        session_id: new.session_id.clone().or_else(|| old.session_id.clone()),
        spawned_by_agent: old.spawned_by_agent || new.spawned_by_agent,
        first_seen_ms: old.first_seen_ms,
        last_seen_ms: old.last_seen_ms.max(new.last_seen_ms),
        last_active_ms: old.last_active_ms.max(new.last_active_ms),
    }
}

impl StateDb {
    /// Opens (creating directories and schema as needed) with WAL journaling.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir)?;
        }
        secure_db_file(path)?;
        let conn = Connection::open(path)?;
        let mut db = Self::with_conn(conn, Some(path.to_path_buf()));
        db.init()?;
        Ok(db)
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&default_path())
    }

    pub fn open_in_memory() -> Result<Self> {
        let mut db = Self::with_conn(Connection::open_in_memory()?, None);
        db.init()?;
        Ok(db)
    }

    fn with_conn(conn: Connection, path: Option<PathBuf>) -> Self {
        StateDb {
            conn,
            path,
            learn: true,
            interval: BATCH_INTERVAL,
            last_flush: Instant::now(),
            last_prune: None,
            pending: Pending::default(),
        }
    }

    fn init(&mut self) -> Result<()> {
        // auto_vacuum must be set before the first table exists; harmless afterwards.
        self.conn.execute_batch("PRAGMA auto_vacuum=INCREMENTAL;")?;
        // WAL is a no-op for in-memory databases.
        let _: String = self.conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        self.conn
            .execute_batch("PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=2000; PRAGMA foreign_keys=ON;")?;
        let found: u32 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(StateError::NewerSchema {
                found,
                path: self
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| ":memory:".into()),
            });
        }
        let tx = self.conn.transaction()?;
        tx.execute_batch(SCHEMA)?;
        // future migrations: `if found < 2 { … }`
        tx.execute(
            "INSERT INTO meta(key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![SCHEMA_VERSION.to_string()],
        )?;
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
        tx.commit()?;
        Ok(())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn schema_version(&self) -> Result<u32> {
        let v: String =
            self.conn
                .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| {
                    r.get(0)
                })?;
        Ok(v.parse().unwrap_or(0))
    }

    /// `--no-learn` / `personalization.learn = false`: stop recording learning signals.
    pub fn set_learning(&mut self, learn: bool) {
        self.learn = learn;
        if !learn {
            let lineage = std::mem::take(&mut self.pending.lineage);
            self.pending = Pending {
                lineage,
                ..Default::default()
            };
        }
    }

    pub fn learning(&self) -> bool {
        self.learn
    }

    /// Batch interval; `Duration::ZERO` writes through (useful for one-shot commands and tests).
    pub fn set_batch_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Number of queued, unwritten rows (lineage + log).
    pub fn pending_len(&self) -> usize {
        self.pending.lineage.len() + self.pending.rows()
    }

    fn after_write(&mut self) -> Result<()> {
        if self.interval.is_zero()
            || self.last_flush.elapsed() >= self.interval
            || self.pending.lineage.len() >= MAX_PENDING_LINEAGE
            || self.pending.rows() >= MAX_PENDING_ROWS
        {
            self.flush()?;
        }
        Ok(())
    }

    /// Writes everything queued in one transaction (also prunes about once an hour).
    pub fn flush(&mut self) -> Result<()> {
        self.last_flush = Instant::now();
        if self.pending.is_empty() {
            return Ok(());
        }
        let p = std::mem::take(&mut self.pending);
        if let Err(e) = Self::write_batch(&mut self.conn, &p) {
            // keep the batch (e.g. SQLITE_BUSY from another oomtop); it is retried on the next flush. The queue
            // cannot have grown meanwhile: `flush` holds `&mut self`.
            self.pending = p;
            return Err(e);
        }
        if self.last_prune.is_none_or(|t| t.elapsed() >= PRUNE_EVERY) {
            self.last_prune = Some(Instant::now());
            let now = p
                .impressions
                .iter()
                .map(|i| i.at_ms)
                .chain(p.events.iter().map(|e| e.2))
                .chain(p.lineage.values().map(|l| l.last_seen_ms))
                .max()
                .unwrap_or(0);
            if now > 0 {
                self.prune(now)?;
            }
        }
        Ok(())
    }

    /// Writes one batch in a single transaction (rolled back on error).
    fn write_batch(conn: &mut Connection, p: &Pending) -> Result<()> {
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO lineage(pid, start_time, ppid, group_id, group_kind, group_label, group_fp, session_id,
                   spawned_by_agent, first_seen_ms, last_seen_ms, last_active_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(pid, start_time) DO UPDATE SET
                   ppid = CASE WHEN lineage.ppid IS NOT NULL AND lineage.ppid > 1 THEN lineage.ppid ELSE excluded.ppid END,
                   group_id = CASE WHEN lineage.spawned_by_agent = 1 AND excluded.spawned_by_agent = 0 THEN lineage.group_id ELSE excluded.group_id END,
                   group_kind = CASE WHEN lineage.spawned_by_agent = 1 AND excluded.spawned_by_agent = 0 THEN lineage.group_kind ELSE excluded.group_kind END,
                   group_label = CASE WHEN lineage.spawned_by_agent = 1 AND excluded.spawned_by_agent = 0 THEN lineage.group_label ELSE excluded.group_label END,
                   group_fp = CASE WHEN lineage.spawned_by_agent = 1 AND excluded.spawned_by_agent = 0 THEN lineage.group_fp ELSE excluded.group_fp END,
                   session_id = COALESCE(excluded.session_id, lineage.session_id),
                   spawned_by_agent = MAX(lineage.spawned_by_agent, excluded.spawned_by_agent),
                   last_seen_ms = MAX(lineage.last_seen_ms, excluded.last_seen_ms),
                   last_active_ms = MAX(lineage.last_active_ms, excluded.last_active_ms)",
            )?;
            for e in p.lineage.values() {
                st.execute(params![
                    e.id.pid,
                    e.id.start_time as i64,
                    e.ppid,
                    e.group_id,
                    e.group_kind.as_str(),
                    e.group_label,
                    e.group_fingerprint,
                    e.session_id,
                    e.spawned_by_agent as i64,
                    e.first_seen_ms as i64,
                    e.last_seen_ms as i64,
                    e.last_active_ms as i64,
                ])?;
            }
            let mut st = tx.prepare_cached(
                "INSERT INTO entity(fingerprint, display_name, first_seen_ms, last_seen_ms) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(fingerprint) DO UPDATE SET display_name = excluded.display_name,
                   first_seen_ms = MIN(entity.first_seen_ms, excluded.first_seen_ms),
                   last_seen_ms = MAX(entity.last_seen_ms, excluded.last_seen_ms)",
            )?;
            for (fp, (name, first, last)) in &p.entities {
                st.execute(params![fp, name, *first as i64, *last as i64])?;
            }
            let mut st = tx.prepare_cached(
                "INSERT INTO event(fingerprint, kind, weight, at_ms, mode) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (fp, kind, at, mode) in &p.events {
                st.execute(params![fp, kind.as_str(), kind.weight(), *at as i64, mode])?;
            }
            let mut st = tx.prepare_cached(
                "INSERT INTO impression(at_ms, served, selected, keystrokes, typed, reformulated, dismissed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for imp in &p.impressions {
                st.execute(params![
                    imp.at_ms as i64,
                    serde_json::to_string(&imp.served)?,
                    imp.selected,
                    imp.keystrokes,
                    imp.typed as i64,
                    imp.reformulated as i64,
                    imp.dismissed as i64
                ])?;
            }
            let mut st = tx.prepare_cached(
                "INSERT INTO query(text, count, last_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT(text) DO UPDATE SET count = count + excluded.count,
                   last_ms = MAX(query.last_ms, excluded.last_ms)",
            )?;
            for (text, (count, last)) in &p.queries {
                st.execute(params![text, *count as i64, *last as i64])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    // ----- lineage journal -----------------------------------------------------------------------------

    /// Queues lineage entries (upsert semantics of [`merge_lineage`]). Session ids that are not already
    /// hashed are hashed before they are stored.
    pub fn record_lineage(&mut self, entries: &[LineageEntry]) -> Result<()> {
        for e in entries {
            let mut e = e.clone();
            if let Some(s) = &e.session_id {
                if !is_hashed(s) {
                    e.session_id = Some(oomtop_core::redact::hash_marker(s));
                }
            }
            match self.pending.lineage.get(&e.id) {
                Some(old) => {
                    let merged = merge_lineage(old, &e);
                    self.pending.lineage.insert(e.id, merged);
                }
                None => {
                    self.pending.lineage.insert(e.id, e);
                }
            }
        }
        self.after_write()
    }

    fn lineage_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<LineageEntry> {
        let kind: String = r.get(4)?;
        Ok(LineageEntry {
            id: ProcId::new(r.get::<_, u32>(0)?, r.get::<_, i64>(1)? as u64),
            ppid: r.get(2)?,
            group_id: r.get(3)?,
            group_kind: GroupKind::parse(&kind).unwrap_or_default(),
            group_label: r.get(5)?,
            group_fingerprint: r.get(6)?,
            session_id: r.get(7)?,
            spawned_by_agent: r.get::<_, i64>(8)? != 0,
            first_seen_ms: r.get::<_, i64>(9)? as u64,
            last_seen_ms: r.get::<_, i64>(10)? as u64,
            last_active_ms: r.get::<_, i64>(11)? as u64,
        })
    }

    const LINEAGE_COLS: &'static str = "pid, start_time, ppid, group_id, group_kind, group_label, group_fp, session_id, spawned_by_agent, first_seen_ms, last_seen_ms, last_active_ms";

    fn overlay(&self, out: &mut BTreeMap<ProcId, LineageEntry>, only: Option<&HashSet<ProcId>>) {
        for (id, p) in &self.pending.lineage {
            if only.is_some_and(|s| !s.contains(id)) {
                continue;
            }
            let merged = match out.get(id) {
                Some(db) => merge_lineage(db, p),
                None => p.clone(),
            };
            out.insert(*id, merged);
        }
    }

    /// Lineage for specific processes.
    pub fn lineage_for(&self, ids: &[ProcId]) -> Result<BTreeMap<ProcId, LineageEntry>> {
        let mut st = self.conn.prepare_cached(&format!(
            "SELECT {} FROM lineage WHERE pid = ?1 AND start_time = ?2",
            Self::LINEAGE_COLS
        ))?;
        let mut out = BTreeMap::new();
        for id in ids {
            if let Some(e) = st
                .query_row(params![id.pid, id.start_time as i64], Self::lineage_row)
                .optional()?
            {
                out.insert(*id, e);
            }
        }
        let want: HashSet<ProcId> = ids.iter().copied().collect();
        self.overlay(&mut out, Some(&want));
        Ok(out)
    }

    /// Whole journal.
    pub fn lineage_all(&self) -> Result<BTreeMap<ProcId, LineageEntry>> {
        let mut st = self
            .conn
            .prepare(&format!("SELECT {} FROM lineage", Self::LINEAGE_COLS))?;
        let rows = st.query_map([], Self::lineage_row)?;
        let mut out = BTreeMap::new();
        for r in rows {
            let e = r?;
            out.insert(e.id, e);
        }
        self.overlay(&mut out, None);
        Ok(out)
    }

    /// Entries seen at or after `since_ms` (what a new instance seeds its idle tracker with).
    pub fn lineage_since(&self, since_ms: u64) -> Result<BTreeMap<ProcId, LineageEntry>> {
        let mut all = self.lineage_all()?;
        all.retain(|_, e| e.last_seen_ms >= since_ms);
        Ok(all)
    }

    /// Removes entries not seen since `before_ms`.
    pub fn prune_lineage(&mut self, before_ms: u64) -> Result<usize> {
        let queued = self.pending.lineage.len();
        self.pending.lineage.retain(|_, e| e.last_seen_ms >= before_ms);
        let dropped_queued = queued - self.pending.lineage.len();
        let n = self.conn.execute(
            "DELETE FROM lineage WHERE last_seen_ms < ?1",
            params![before_ms as i64],
        )?;
        Ok(n + dropped_queued)
    }

    // ----- profile ---------------------------------------------------------------------------------------

    /// Creates or refreshes an entity's display name and last-seen time (a learning signal: skipped with
    /// learning off).
    pub fn upsert_entity(&mut self, fingerprint: &str, display_name: &str, now_ms: u64) -> Result<()> {
        if !self.learn {
            return Ok(());
        }
        let e = self
            .pending
            .entities
            .entry(fingerprint.to_string())
            .or_insert_with(|| (display_name.to_string(), now_ms, now_ms));
        e.0 = display_name.to_string();
        e.1 = e.1.min(now_ms);
        e.2 = e.2.max(now_ms);
        self.after_write()
    }

    /// Records a learning event (skipped with learning off).
    pub fn record_event(
        &mut self,
        fingerprint: &str,
        kind: EventKind,
        at_ms: u64,
        mode: Option<&str>,
    ) -> Result<()> {
        if !self.learn {
            return Ok(());
        }
        self.pending
            .events
            .push((fingerprint.to_string(), kind, at_ms, mode.map(str::to_string)));
        self.after_write()
    }

    fn ensure_entity(&mut self, fingerprint: &str, now_ms: u64) -> Result<()> {
        let (name, first) = self
            .pending
            .entities
            .get(fingerprint)
            .map(|e| (e.0.clone(), e.1.min(now_ms)))
            .unwrap_or_else(|| (fingerprint.to_string(), now_ms));
        self.conn.execute(
            "INSERT OR IGNORE INTO entity(fingerprint, display_name, first_seen_ms, last_seen_ms) VALUES (?1, ?2, ?3, ?4)",
            params![fingerprint, name, first as i64, now_ms as i64],
        )?;
        Ok(())
    }

    fn explicit_event(&mut self, fingerprint: &str, kind: EventKind, now_ms: u64) -> Result<()> {
        if self.learn {
            self.conn.execute(
                "INSERT INTO event(fingerprint, kind, weight, at_ms, mode) VALUES (?1, ?2, ?3, ?4, NULL)",
                params![fingerprint, kind.as_str(), kind.weight(), now_ms as i64],
            )?;
        }
        Ok(())
    }

    /// Pin (`p`) — written immediately.
    pub fn set_pinned(&mut self, fingerprint: &str, pinned: bool, now_ms: u64) -> Result<()> {
        self.ensure_entity(fingerprint, now_ms)?;
        self.conn.execute(
            "UPDATE entity SET pinned = ?2 WHERE fingerprint = ?1",
            params![fingerprint, pinned as i64],
        )?;
        if pinned {
            self.explicit_event(fingerprint, EventKind::Pin, now_ms)?;
        }
        Ok(())
    }

    /// Mute (`m`) until `until_ms`; `None` unmutes — written immediately.
    pub fn set_muted(&mut self, fingerprint: &str, until_ms: Option<u64>, now_ms: u64) -> Result<()> {
        self.ensure_entity(fingerprint, now_ms)?;
        self.conn.execute(
            "UPDATE entity SET muted_until_ms = ?2 WHERE fingerprint = ?1",
            params![fingerprint, until_ms.map(|v| v as i64)],
        )?;
        if until_ms.is_some() {
            self.explicit_event(fingerprint, EventKind::Mute, now_ms)?;
        }
        Ok(())
    }

    /// User rename (`n`); `None` clears it — written immediately.
    pub fn rename(&mut self, fingerprint: &str, alias: Option<&str>, now_ms: u64) -> Result<()> {
        self.ensure_entity(fingerprint, now_ms)?;
        let alias = alias.map(str::trim).filter(|a| !a.is_empty());
        self.conn.execute(
            "UPDATE entity SET alias = ?2 WHERE fingerprint = ?1",
            params![fingerprint, alias],
        )?;
        Ok(())
    }

    /// All entities (queued sightings merged), sorted by fingerprint.
    pub fn entities(&self) -> Result<Vec<EntityRecord>> {
        let mut st = self.conn.prepare(
            "SELECT fingerprint, display_name, alias, pinned, muted_until_ms, first_seen_ms, last_seen_ms FROM entity ORDER BY fingerprint",
        )?;
        let rows = st.query_map([], |r| {
            Ok(EntityRecord {
                fingerprint: r.get(0)?,
                display_name: r.get(1)?,
                alias: r.get(2)?,
                pinned: r.get::<_, i64>(3)? != 0,
                muted_until_ms: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                first_seen_ms: r.get::<_, i64>(5)? as u64,
                last_seen_ms: r.get::<_, i64>(6)? as u64,
            })
        })?;
        let mut by_fp: BTreeMap<String, EntityRecord> = BTreeMap::new();
        for r in rows {
            let e = r?;
            by_fp.insert(e.fingerprint.clone(), e);
        }
        for (fp, (name, first, last)) in &self.pending.entities {
            let e = by_fp.entry(fp.clone()).or_insert_with(|| EntityRecord {
                fingerprint: fp.clone(),
                display_name: name.clone(),
                first_seen_ms: *first,
                last_seen_ms: *last,
                ..Default::default()
            });
            e.display_name = name.clone();
            e.first_seen_ms = e.first_seen_ms.min(*first);
            e.last_seen_ms = e.last_seen_ms.max(*last);
        }
        Ok(by_fp.into_values().collect())
    }

    /// One entity.
    pub fn entity(&self, fingerprint: &str) -> Result<Option<EntityRecord>> {
        Ok(self
            .entities()?
            .into_iter()
            .find(|e| e.fingerprint == fingerprint))
    }

    /// Pinned fingerprints ("Your things", UX §5.5).
    pub fn pinned(&self) -> Result<Vec<String>> {
        let mut st = self
            .conn
            .prepare("SELECT fingerprint FROM entity WHERE pinned = 1 ORDER BY fingerprint")?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Fingerprints muted at `now_ms`.
    pub fn muted(&self, now_ms: u64) -> Result<HashSet<String>> {
        let mut st = self
            .conn
            .prepare("SELECT fingerprint FROM entity WHERE muted_until_ms > ?1")?;
        let rows = st.query_map(params![now_ms as i64], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<HashSet<_>>>()?)
    }

    /// User renames: fingerprint → alias (query vocabulary, UX §4).
    pub fn aliases(&self) -> Result<HashMap<String, String>> {
        let mut st = self
            .conn
            .prepare("SELECT fingerprint, alias FROM entity WHERE alias IS NOT NULL")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// Frecency per fingerprint: positive events decay with `half_life_s`, negative ones with 30 days.
    pub fn frecency(&self, now_ms: u64, half_life_s: f64) -> Result<HashMap<String, f64>> {
        let mut st = self
            .conn
            .prepare_cached("SELECT fingerprint, weight, at_ms FROM event")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        let mut all: Vec<(String, f64, u64)> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        all.extend(
            self.pending
                .events
                .iter()
                .map(|(fp, k, at, _)| (fp.clone(), k.weight(), *at)),
        );
        let mut pos: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
        let mut neg: HashMap<String, Vec<(f64, f64)>> = HashMap::new();
        for (fp, w, at) in all {
            let age = now_ms.saturating_sub(at) as f64 / 1000.0;
            if w >= 0.0 {
                pos.entry(fp).or_default().push((age, w));
            } else {
                neg.entry(fp).or_default().push((age, w));
            }
        }
        let mut out: HashMap<String, f64> = HashMap::new();
        for (fp, ev) in pos {
            *out.entry(fp).or_default() += frecency(&ev, half_life_s);
        }
        for (fp, ev) in neg {
            *out.entry(fp).or_default() += frecency(&ev, NEGATIVE_HALF_LIFE_S);
        }
        Ok(out)
    }

    /// Event counts per kind (for `profile show`).
    pub fn event_counts(&self) -> Result<BTreeMap<String, u64>> {
        let mut st = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM event GROUP BY kind")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
        let mut out: BTreeMap<String, u64> = rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        for (_, k, _, _) in &self.pending.events {
            *out.entry(k.as_str().to_string()).or_default() += 1;
        }
        Ok(out)
    }

    /// Records a typed query for learned completion (skipped with learning off). Secret-looking parts are
    /// redacted before storage.
    pub fn record_query(&mut self, text: &str, at_ms: u64) -> Result<()> {
        if !self.learn {
            return Ok(());
        }
        let t: String = text
            .split_whitespace()
            .map(oomtop_core::redact::redact_text)
            .collect::<Vec<_>>()
            .join(" ");
        if t.is_empty() {
            return Ok(());
        }
        let e = self.pending.queries.entry(t).or_insert((0, at_ms));
        e.0 += 1;
        e.1 = e.1.max(at_ms);
        self.after_write()
    }

    /// Past queries ranked by frecency (count decayed by recency).
    pub fn top_queries(&self, n: usize, now_ms: u64, half_life_s: f64) -> Result<Vec<(String, f64)>> {
        let mut st = self.conn.prepare("SELECT text, count, last_ms FROM query")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, i64>(2)? as u64,
            ))
        })?;
        let mut merged: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for r in rows {
            let (t, c, last) = r?;
            merged.insert(t, (c, last));
        }
        for (t, (c, last)) in &self.pending.queries {
            let e = merged.entry(t.clone()).or_insert((0, *last));
            e.0 += c;
            e.1 = e.1.max(*last);
        }
        let mut v: Vec<(String, f64)> = merged
            .into_iter()
            .map(|(t, (c, last))| {
                let age = now_ms.saturating_sub(last) as f64 / 1000.0;
                (t, frecency(&[(age, c as f64)], half_life_s))
            })
            .collect();
        v.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        v.truncate(n);
        Ok(v)
    }

    /// Logs one impression → selection pair (skipped with learning off).
    pub fn log_impression(&mut self, imp: &Impression) -> Result<()> {
        if !self.learn {
            return Ok(());
        }
        self.pending.impressions.push(imp.clone());
        self.after_write()
    }

    /// UX metrics since `since_ms` (UX §5.6).
    pub fn ux_stats(&self, since_ms: u64) -> Result<UxStats> {
        let mut st = self.conn.prepare(
            "SELECT served, selected, keystrokes, typed, reformulated, dismissed FROM impression WHERE at_ms >= ?1",
        )?;
        let rows = st.query_map(params![since_ms as i64], |r| {
            Ok(Impression {
                at_ms: 0,
                served: serde_json::from_str(&r.get::<_, String>(0)?).unwrap_or_default(),
                selected: r.get::<_, Option<String>>(1)?,
                keystrokes: r.get::<_, i64>(2)?.max(0) as u32,
                typed: r.get::<_, i64>(3)? != 0,
                reformulated: r.get::<_, i64>(4)? != 0,
                dismissed: r.get::<_, i64>(5)? != 0,
            })
        })?;
        let mut all: Vec<Impression> = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        all.extend(
            self.pending
                .impressions
                .iter()
                .filter(|i| i.at_ms >= since_ms)
                .cloned(),
        );
        Ok(stats_of(&all))
    }

    /// Machine profile JSON (UX §6), refreshed daily by the caller.
    pub fn set_machine_profile(&mut self, json: &serde_json::Value, now_ms: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO machine_profile(id, json, updated_ms) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET json = excluded.json, updated_ms = excluded.updated_ms",
            params![serde_json::to_string(json)?, now_ms as i64],
        )?;
        Ok(())
    }

    pub fn machine_profile(&self) -> Result<Option<(serde_json::Value, u64)>> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                "SELECT json, updated_ms FROM machine_profile WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((j, t)) => Ok(Some((serde_json::from_str(&j)?, t as u64))),
            None => Ok(None),
        }
    }

    /// True when the machine profile is missing or older than a day (UX §6: refreshed daily).
    pub fn machine_profile_stale(&self, now_ms: u64) -> Result<bool> {
        Ok(self
            .machine_profile()?
            .is_none_or(|(_, t)| now_ms.saturating_sub(t) >= 86_400_000))
    }

    /// `oomtop profile reset`: forgets entities, events, impressions, queries and the machine profile
    /// (ranking returns to salience-only defaults). The lineage journal is kept.
    pub fn reset_profile(&mut self) -> Result<()> {
        let lineage = std::mem::take(&mut self.pending.lineage);
        self.pending = Pending {
            lineage,
            ..Default::default()
        };
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM entity; DELETE FROM event; DELETE FROM impression; DELETE FROM query; DELETE FROM machine_profile;",
        )?;
        tx.commit()?;
        let _ = self.conn.execute_batch("PRAGMA incremental_vacuum;");
        Ok(())
    }

    /// `oomtop profile export`: everything learned, as JSON (no raw command lines, no environments).
    pub fn export_profile(&self, now_ms: u64, half_life_s: f64) -> Result<serde_json::Value> {
        let mut fr: Vec<(String, f64)> = self.frecency(now_ms, half_life_s)?.into_iter().collect();
        fr.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(serde_json::json!({
            "schema_version": SCHEMA_VERSION,
            "exported_at_ms": now_ms,
            "learning": self.learn,
            "half_life_s": half_life_s,
            "entities": self.entities()?,
            "frecency": fr,
            "events": self.event_counts()?,
            "queries": self.top_queries(100, now_ms, half_life_s)?,
            "ux_stats": self.ux_stats(now_ms.saturating_sub(RETENTION_MS))?,
            "machine_profile": self.machine_profile()?.map(|(j, _)| j),
        }))
    }

    /// Approximate bytes used by the learning log (impressions, events, queries).
    pub fn log_bytes(&self) -> Result<u64> {
        let q = |sql: &str| -> Result<u64> {
            Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))?.max(0) as u64)
        };
        Ok(q("SELECT COALESCE(SUM(LENGTH(served) + COALESCE(LENGTH(selected), 0) + 48), 0) FROM impression")?
            + q("SELECT COALESCE(SUM(LENGTH(fingerprint) + LENGTH(kind) + COALESCE(LENGTH(mode), 0) + 40), 0) FROM event")?
            + q("SELECT COALESCE(SUM(LENGTH(text) + 24), 0) FROM query")?)
    }

    /// Size of the database file(s) on disk (db + WAL), 0 in memory.
    pub fn file_bytes(&self) -> u64 {
        let Some(p) = &self.path else { return 0 };
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        let mut wal = p.as_os_str().to_owned();
        wal.push("-wal");
        size(p) + size(Path::new(&wal))
    }

    /// Approximate log bytes of one table (same formula as [`Self::log_bytes`]).
    fn table_log_bytes(&self, table: &str) -> Result<u64> {
        let sql = match table {
            "impression" => "SELECT COALESCE(SUM(LENGTH(served) + COALESCE(LENGTH(selected), 0) + 48), 0) FROM impression",
            "event" => "SELECT COALESCE(SUM(LENGTH(fingerprint) + LENGTH(kind) + COALESCE(LENGTH(mode), 0) + 40), 0) FROM event WHERE weight >= 0",
            _ => "SELECT COALESCE(SUM(LENGTH(text) + 24), 0) FROM query",
        };
        Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))?.max(0) as u64)
    }

    fn count(&self, table: &str) -> Result<u64> {
        Ok(self
            .conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                r.get::<_, i64>(0)
            })?
            .max(0) as u64)
    }

    /// Drops log rows older than 30 days and caps the log at 5 MB (UX §5.6).
    pub fn prune(&mut self, now_ms: u64) -> Result<()> {
        self.prune_with(now_ms, RETENTION_MS, LOG_CAP_BYTES).map(|_| ())
    }

    /// [`Self::prune`] with explicit limits (config `personalization.retention_days` / `log_max_mb`).
    /// Positive events older than the retention window go; negative ones (mute / show less) are kept while
    /// their mute lasts, since they decay on their own 30-day half-life. When the log is still over the cap,
    /// the oldest impressions go first, then the oldest positive events, then the least recent queries.
    pub fn prune_with(&mut self, now_ms: u64, retention_ms: u64, cap_bytes: u64) -> Result<PruneReport> {
        let cutoff = now_ms.saturating_sub(retention_ms) as i64;
        let mut rep = PruneReport::default();
        let tx = self.conn.transaction()?;
        rep.impressions = tx.execute("DELETE FROM impression WHERE at_ms < ?1", params![cutoff])? as u64;
        rep.events = tx.execute(
            "DELETE FROM event WHERE at_ms < ?1 AND (weight >= 0 OR at_ms < ?2)",
            params![cutoff, now_ms.saturating_sub(4 * retention_ms) as i64],
        )? as u64;
        rep.queries = tx.execute("DELETE FROM query WHERE last_ms < ?1", params![cutoff])? as u64;
        rep.lineage = tx.execute("DELETE FROM lineage WHERE last_seen_ms < ?1", params![cutoff])? as u64;
        rep.expired_mutes = tx.execute(
            "UPDATE entity SET muted_until_ms = NULL WHERE muted_until_ms IS NOT NULL AND muted_until_ms <= ?1",
            params![now_ms as i64],
        )? as u64;
        tx.commit()?;
        // size cap: drop the globally oldest log rows (impressions and positive events alike), then the least
        // recent queries
        let mut guard = 0;
        while self.log_bytes()? > cap_bytes && guard < 100_000 {
            guard += 1;
            let min_of = |sql: &str| -> Result<Option<i64>> {
                Ok(self.conn.query_row(sql, [], |r| r.get::<_, Option<i64>>(0))?)
            };
            let imp = min_of("SELECT MIN(at_ms) FROM impression")?;
            let ev = min_of("SELECT MIN(at_ms) FROM event WHERE weight >= 0")?;
            let (table, order, extra) = match (imp, ev) {
                (Some(a), Some(b)) if b < a => ("event", "at_ms", "WHERE weight >= 0"),
                (Some(_), _) => ("impression", "at_ms", ""),
                (None, Some(_)) => ("event", "at_ms", "WHERE weight >= 0"),
                (None, None) => ("query", "last_ms", ""),
            };
            let rows = self.count(table)?;
            if rows == 0 {
                break;
            }
            // delete about as many of the oldest rows as the overflow needs (by this table's mean row size), so
            // a far-over-cap log converges in a few rounds instead of thousands — but never past the other
            // table's oldest row, so "globally oldest first" still holds between impressions and events
            let over = self.log_bytes()?.saturating_sub(cap_bytes);
            let table_bytes = self.table_log_bytes(table)?.max(1);
            let mean = (table_bytes / rows).max(1);
            let mut batch = over.div_ceil(mean).clamp(1, rows);
            let other_min = match table {
                "impression" => ev,
                "event" => imp,
                _ => None,
            };
            if let Some(m) = other_min {
                let older: i64 = self.conn.query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} WHERE {order} <= ?1 {}",
                        if extra.is_empty() { "" } else { "AND weight >= 0" }
                    ),
                    params![m],
                    |r| r.get(0),
                )?;
                // interleaved timestamps would otherwise shrink this to one row per round; allow the old
                // fixed step (5 %, ≤ 200 rows) of near-oldest rows
                let step = (rows / 20).clamp(1, 200);
                batch = batch.min((older.max(0) as u64).max(step));
            }
            let batch = batch as i64;
            let n = self.conn.execute(
                &format!(
                    "DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} {extra} ORDER BY {order} ASC LIMIT ?1)"
                ),
                params![batch],
            )? as u64;
            if n == 0 {
                break;
            }
            match table {
                "impression" => rep.impressions += n,
                "event" => rep.events += n,
                _ => rep.queries += n,
            }
        }
        let _ = self.conn.execute_batch("PRAGMA incremental_vacuum;");
        if self.path.is_some() {
            let _ = self
                .conn
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
        }
        Ok(rep)
    }

    /// `oomtop profile show`.
    pub fn summary(&self, now_ms: u64, half_life_s: f64) -> Result<ProfileSummary> {
        profile::summarize(self, now_ms, half_life_s)
    }

    pub(crate) fn counts(&self) -> Result<(u64, u64, u64, u64)> {
        Ok((
            self.count("event")? + self.pending.events.len() as u64,
            self.count("impression")? + self.pending.impressions.len() as u64,
            self.top_queries(usize::MAX, 0, 1.0)?.len() as u64,
            self.lineage_all()?.len() as u64,
        ))
    }

    pub(crate) fn oldest_event_ms(&self) -> Result<Option<u64>> {
        let v: Option<i64> = self
            .conn
            .query_row("SELECT MIN(at_ms) FROM event", [], |r| r.get(0))?;
        let pending = self.pending.events.iter().map(|e| e.2).min();
        Ok(match (v.map(|x| x as u64), pending) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        })
    }
}

impl Drop for StateDb {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn stats_of(all: &[Impression]) -> UxStats {
    let (mut sel, mut hits, mut keys, mut typed_n, mut reform, mut dismissed) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for imp in all {
        if imp.dismissed {
            dismissed += 1;
        }
        if imp.typed {
            typed_n += 1;
            if imp.reformulated {
                reform += 1;
            }
        }
        if let Some(s) = &imp.selected {
            sel += 1;
            keys += imp.keystrokes as u64;
            if !imp.typed && imp.served.iter().take(3).any(|x| x == s) {
                hits += 1;
            }
        }
    }
    let n = all.len() as u64;
    let ratio = |a: u64, b: u64| (b > 0).then(|| a as f64 / b as f64);
    UxStats {
        impressions: n,
        selections: sel,
        hit_at_3: ratio(hits, sel),
        mean_keystrokes: (sel > 0).then(|| keys as f64 / sel as f64),
        reformulation_rate: ratio(reform, typed_n),
        dismiss_rate: ratio(dismissed, n),
    }
}

#[cfg(test)]
mod tests;

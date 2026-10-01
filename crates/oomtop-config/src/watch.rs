//! Live reload (UX §12.2, §11 test 9): all config files are watched and reloaded on save, debounced 200 ms.
//! An invalid edit keeps the last good config and reports file:line (see [`crate::layered::LayerCache`]).
//!
//! - [`watch`] is the low-level primitive: watch paths, call `on_change` once per burst of events.
//! - [`LiveConfig`] is what frontends use: it owns a [`Loaded`], re-runs the layered load on every change and
//!   exposes the current config, the latest errors and a generation counter to poll each frame.

use crate::layered::{load_layered_cached, LayerCache, LoadOptions, Loaded};
use crate::ConfigError;
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// Quiet period that ends a burst of file events.
pub const DEBOUNCE: Duration = Duration::from_millis(200);
/// A burst never delays a reload longer than this (editors that write in many steps).
pub const MAX_DELAY: Duration = Duration::from_millis(600);

/// Fastest allowed polling interval for [`WatchMode::from_poll_ms`].
pub const MIN_POLL: Duration = Duration::from_millis(100);

/// Watch backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    /// FSEvents / inotify.
    Native,
    /// Polling at the given interval (network filesystems, tests).
    Poll(Duration),
}

impl WatchMode {
    /// From `general.watch_poll_ms` (0 = native). Polls faster than [`MIN_POLL`] are raised to it: polling
    /// compares file contents, and a 1 ms loop would blow the CPU budget (SPEC §14).
    pub fn from_poll_ms(ms: u64) -> WatchMode {
        if ms == 0 {
            WatchMode::Native
        } else {
            WatchMode::Poll(Duration::from_millis(ms).max(MIN_POLL))
        }
    }
}

type SharedWatcher = Arc<Mutex<Box<dyn Watcher + Send>>>;
/// A non-owning handle to the watcher (used from its own callback).
type WeakWatcher = Weak<Mutex<Box<dyn Watcher + Send>>>;

/// Keeps the watcher alive; dropping it stops watching.
pub struct ConfigWatcher {
    watcher: SharedWatcher,
    _thread: std::thread::JoinHandle<()>,
}

impl ConfigWatcher {
    /// Adds a path (recursive for directories when `recursive`).
    pub fn add(&self, path: &std::path::Path, recursive: bool) -> Result<(), ConfigError> {
        add_path(&self.watcher, path, recursive)
    }
}

fn add_path(w: &SharedWatcher, path: &std::path::Path, recursive: bool) -> Result<(), ConfigError> {
    let mode = if recursive && path.is_dir() {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    let mut guard = w.lock().map_err(|_| ConfigError {
        path: None,
        line: None,
        message: "watch: lock poisoned".into(),
    })?;
    guard.watch(path, mode).map_err(|e| ConfigError {
        path: Some(path.to_path_buf()),
        line: None,
        message: format!("watch: {e}"),
    })
}

impl std::fmt::Debug for ConfigWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigWatcher")
    }
}

fn relevant(ev: &notify::Event) -> bool {
    if matches!(ev.kind, notify::EventKind::Access(_)) {
        return false;
    }
    // ignore our own atomic-write temp files and editor swap files
    ev.paths.is_empty()
        || ev.paths.iter().any(|p| {
            let name = p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
            !(name.starts_with('.') && name.contains(".tmp-")
                || name.ends_with('~')
                || name.ends_with(".swp")
                || name.ends_with(".swx")
                || name == "4913")
        })
}

/// Watches `paths` (directories recursively, files directly; missing ones are skipped) and calls `on_change`
/// once per burst.
pub fn watch(
    paths: Vec<PathBuf>,
    mode: WatchMode,
    on_change: impl Fn() + Send + 'static,
) -> Result<ConfigWatcher, ConfigError> {
    watch_entries(paths.into_iter().map(|p| (p, true)).collect(), mode, on_change)
}

/// [`watch`] with explicit recursion per path.
pub fn watch_entries(
    entries: Vec<(PathBuf, bool)>,
    mode: WatchMode,
    on_change: impl Fn() + Send + 'static,
) -> Result<ConfigWatcher, ConfigError> {
    let (tx, rx) = mpsc::channel::<()>();
    let handler = move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if relevant(&ev) {
                let _ = tx.send(());
            }
        }
    };
    let err = |e: notify::Error| ConfigError {
        path: None,
        line: None,
        message: format!("watch: {e}"),
    };
    let watcher: Box<dyn Watcher + Send> = match mode {
        WatchMode::Native => Box::new(notify::recommended_watcher(handler).map_err(err)?),
        WatchMode::Poll(iv) => Box::new(
            notify::PollWatcher::new(
                handler,
                notify::Config::default()
                    .with_poll_interval(iv)
                    .with_compare_contents(true),
            )
            .map_err(err)?,
        ),
    };
    let watcher: SharedWatcher = Arc::new(Mutex::new(watcher));
    for (p, recursive) in &entries {
        if p.exists() {
            add_path(&watcher, p, *recursive)?;
        }
    }
    let thread = std::thread::Builder::new()
        .name("oomtop-config-watch".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                // debounce: wait for DEBOUNCE of quiet, but never longer than MAX_DELAY in total
                let start = Instant::now();
                loop {
                    let left = MAX_DELAY.saturating_sub(start.elapsed());
                    if left.is_zero() {
                        break;
                    }
                    match rx.recv_timeout(DEBOUNCE.min(left)) {
                        Ok(()) => continue,
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                on_change();
            }
        })
        .map_err(|e| ConfigError {
            path: None,
            line: None,
            message: format!("watch thread: {e}"),
        })?;
    Ok(ConfigWatcher {
        watcher,
        _thread: thread,
    })
}

/// Directories holding the targets of symlinked files in the config dir and its sub-directories
/// (`config.d`, `themes`, `layouts`, `rules.d`), deduplicated.
fn symlink_target_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let subdirs = ["", "config.d", "themes", "layouts", "rules.d"];
    for sub in subdirs {
        let Ok(rd) = std::fs::read_dir(dir.join(sub)) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let is_link = std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink());
            if !is_link || p.is_dir() {
                continue;
            }
            let target = crate::write::resolve_symlinks(&p);
            if let Some(parent) = target.parent().filter(|d| d.is_dir()) {
                let parent = parent.to_path_buf();
                if !out.contains(&parent) {
                    out.push(parent);
                }
            }
        }
    }
    out
}

fn nearest_existing_ancestor(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .skip(1)
        .find(|a| a.is_dir())
        .map(Path::to_path_buf)
}

struct Shared {
    opts: LoadOptions,
    cache: LayerCache,
    loaded: Loaded,
}

/// A config that reloads itself when its files change.
pub struct LiveConfig {
    shared: Arc<Mutex<Shared>>,
    generation: Arc<AtomicU64>,
    _watcher: Option<ConfigWatcher>,
}

impl std::fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConfig")
            .field("generation", &self.generation())
            .finish()
    }
}

fn reload(shared: &Mutex<Shared>, generation: &AtomicU64) {
    if let Ok(mut s) = shared.lock() {
        let Shared { opts, cache, loaded } = &mut *s;
        let fresh = load_layered_cached(opts, cache);
        // runtime toggles survive reloads
        let runtime: Vec<(String, String)> = loaded
            .origins
            .iter()
            .filter(|(_, o)| **o == crate::layered::Origin::Runtime)
            .filter_map(|(k, _)| loaded.value(k).map(|v| (k.clone(), v)))
            .collect();
        let mut fresh = fresh;
        for (k, v) in runtime {
            let _ = fresh.set_runtime(&k, &v);
        }
        *loaded = fresh;
        generation.fetch_add(1, Ordering::SeqCst);
    }
}

impl LiveConfig {
    /// Loads once and starts watching the config directory (and the system layer) when `live` is true.
    pub fn start(opts: LoadOptions, mode: WatchMode, live: bool) -> Result<LiveConfig, ConfigError> {
        let mut cache = LayerCache::new();
        let loaded = load_layered_cached(&opts, &mut cache);
        let dir = opts.user_dir();
        // Watch the config dir recursively; if it does not exist yet, watch its nearest existing ancestor
        // *non-recursively* (never a recursive watch of ~/.config or $HOME) and add the dir once it appears.
        let mut entries: Vec<(PathBuf, bool)> = Vec::new();
        let dir_exists = dir.is_dir();
        if dir_exists {
            entries.push((dir.clone(), true));
        } else if let Some(anc) = nearest_existing_ancestor(&dir) {
            entries.push((anc, false));
        }
        if let Some(main) = &opts.config_path {
            if !main.starts_with(&dir) {
                // `--config` outside the config dir: watch its directory, not the file, because editors save
                // by renaming a new file over the old one, which silently ends a watch on the old inode
                let parent = main
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                entries.push((parent, false));
            }
        }
        if let Some(sys) = opts.system() {
            if sys.is_dir() {
                entries.push((sys, true));
            }
        }
        // files symlinked in from a dotfile repo: edits happen at the link target, outside the watched dir
        for target_dir in symlink_target_dirs(&dir) {
            if !entries
                .iter()
                .any(|(p, rec)| target_dir.starts_with(p) && (*rec || *p == target_dir))
            {
                entries.push((target_dir, false));
            }
        }
        let shared = Arc::new(Mutex::new(Shared { opts, cache, loaded }));
        let generation = Arc::new(AtomicU64::new(0));
        let watcher = if live {
            let (s, g) = (shared.clone(), generation.clone());
            // Weak: the callback runs on the watcher's own thread; a strong reference here would keep the
            // watcher (and that thread) alive forever after the LiveConfig is dropped.
            let slot: Arc<Mutex<Option<WeakWatcher>>> = Arc::new(Mutex::new(None));
            let slot2 = slot.clone();
            let dir_watched = Arc::new(std::sync::atomic::AtomicBool::new(dir_exists));
            let w = watch_entries(entries, mode, move || {
                if !dir_watched.load(Ordering::SeqCst) {
                    if let Some(w) = slot2.lock().ok().and_then(|g| g.as_ref().and_then(Weak::upgrade)) {
                        if dir.is_dir() {
                            if add_path(&w, &dir, true).is_ok() {
                                dir_watched.store(true, Ordering::SeqCst);
                            }
                        } else if let Some(anc) = nearest_existing_ancestor(&dir) {
                            // an intermediate directory appeared (e.g. ~/.config itself): move one level closer
                            let _ = add_path(&w, &anc, false);
                        }
                    }
                }
                reload(&s, &g)
            })?;
            if let Ok(mut g) = slot.lock() {
                *g = Some(Arc::downgrade(&w.watcher));
            }
            Some(w)
        } else {
            None
        };
        Ok(LiveConfig {
            shared,
            generation,
            _watcher: watcher,
        })
    }

    /// Increments on every reload; compare with the last value seen to know when to re-read.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// The current (last good) state.
    pub fn current(&self) -> Loaded {
        match self.shared.lock() {
            Ok(s) => s.loaded.clone(),
            Err(p) => p.into_inner().loaded.clone(),
        }
    }

    /// Errors of the latest load (shown inline with file:line).
    pub fn errors(&self) -> Vec<ConfigError> {
        self.current().errors
    }

    /// Reloads now (e.g. after the settings screen saved a file).
    pub fn reload_now(&self) {
        reload(&self.shared, &self.generation);
    }

    /// Applies a runtime toggle (kept across reloads, never persisted).
    pub fn set_runtime(&self, key: &str, value: &str) -> Result<(), ConfigError> {
        let mut s = self.shared.lock().map_err(|_| ConfigError {
            path: None,
            line: None,
            message: "config lock poisoned".into(),
        })?;
        s.loaded.set_runtime(key, value)?;
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn reload_fires_after_edit() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("config.toml");
        std::fs::write(&f, "[appearance]\ntheme = \"terminal\"\n").unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let _w = watch(
            vec![d.path().to_path_buf()],
            WatchMode::Poll(Duration::from_millis(50)),
            move || {
                h.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(150));
        std::fs::write(&f, "[appearance]\ntheme = \"none\"\n").unwrap();
        let start = Instant::now();
        while hits.load(Ordering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "reload within 1 s target (5 s test cap)"
        );
    }

    #[test]
    fn config_dir_created_later_is_picked_up() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("cfg/oomtop");
        std::fs::create_dir_all(d.path().join("cfg")).unwrap();
        let live = LiveConfig::start(
            LoadOptions {
                config_dir: Some(dir.clone()),
                system_dir: Some(PathBuf::new()),
                hostname: Some("h".into()),
                env: Some(vec![]),
                ..Default::default()
            },
            WatchMode::Poll(Duration::from_millis(50)),
            true,
        )
        .unwrap();
        assert_eq!(live.current().config.appearance.theme, "terminal");
        std::thread::sleep(Duration::from_millis(150));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "[appearance]\ntheme = \"sand\"\n").unwrap();
        let start = Instant::now();
        while live.current().config.appearance.theme != "sand" && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(live.current().config.appearance.theme, "sand");
        // edits inside the new dir are seen too
        std::thread::sleep(Duration::from_millis(150));
        std::fs::write(dir.join("config.toml"), "[appearance]\ntheme = \"mint\"\n").unwrap();
        let start = Instant::now();
        while live.current().config.appearance.theme != "mint" && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(live.current().config.appearance.theme, "mint");
    }

    #[test]
    fn dropping_live_config_stops_the_watcher() {
        // regression: the "add the dir once it appears" callback held a strong reference to the watcher, so
        // the watcher thread (and the shared config) lived forever after LiveConfig was dropped
        let d = tempfile::tempdir().unwrap();
        for dir_exists in [false, true] {
            let dir = d.path().join(format!("c{dir_exists}/oomtop"));
            std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
            if dir_exists {
                std::fs::create_dir_all(&dir).unwrap();
            }
            let live = LiveConfig::start(
                LoadOptions {
                    config_dir: Some(dir),
                    system_dir: Some(PathBuf::new()),
                    hostname: Some("h".into()),
                    env: Some(vec![]),
                    ..Default::default()
                },
                WatchMode::Poll(Duration::from_millis(30)),
                true,
            )
            .unwrap();
            let weak = Arc::downgrade(&live.shared);
            drop(live);
            let start = Instant::now();
            while weak.upgrade().is_some() && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(
                weak.upgrade().is_none(),
                "watcher thread still alive (dir_exists={dir_exists})"
            );
        }
    }

    #[test]
    fn external_config_file_survives_rename_saves() {
        // `--config /elsewhere/my.toml`: editors save by renaming over the file; the parent is watched
        let d = tempfile::tempdir().unwrap();
        let other = d.path().join("elsewhere");
        std::fs::create_dir_all(&other).unwrap();
        let main = other.join("my.toml");
        std::fs::write(&main, "[appearance]\ntheme = \"mono\"\n").unwrap();
        let live = LiveConfig::start(
            LoadOptions {
                config_path: Some(main.clone()),
                config_dir: Some(d.path().join("cfg/oomtop")),
                system_dir: Some(PathBuf::new()),
                hostname: Some("h".into()),
                env: Some(vec![]),
                ..Default::default()
            },
            WatchMode::Poll(Duration::from_millis(40)),
            true,
        )
        .unwrap();
        assert_eq!(live.current().config.appearance.theme, "mono");
        for theme in ["sand", "mint"] {
            std::thread::sleep(Duration::from_millis(120));
            let tmp = other.join(".my.toml.swap");
            std::fs::write(&tmp, format!("[appearance]\ntheme = \"{theme}\"\n")).unwrap();
            std::fs::rename(&tmp, &main).unwrap();
            let start = Instant::now();
            while live.current().config.appearance.theme != theme && start.elapsed() < Duration::from_secs(5)
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(live.current().config.appearance.theme, theme);
        }
    }

    #[cfg(unix)]
    #[test]
    fn edits_at_a_symlink_target_reload() {
        let d = tempfile::tempdir().unwrap();
        let repo = d.path().join("dotfiles");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("oomtop.toml"), "[appearance]\ntheme = \"mono\"\n").unwrap();
        let dir = d.path().join("cfg/oomtop");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(repo.join("oomtop.toml"), dir.join("config.toml")).unwrap();
        assert_eq!(symlink_target_dirs(&dir), vec![repo.clone()]);
        let live = LiveConfig::start(
            LoadOptions {
                config_dir: Some(dir),
                system_dir: Some(PathBuf::new()),
                hostname: Some("h".into()),
                env: Some(vec![]),
                ..Default::default()
            },
            WatchMode::Poll(Duration::from_millis(40)),
            true,
        )
        .unwrap();
        assert_eq!(live.current().config.appearance.theme, "mono");
        std::thread::sleep(Duration::from_millis(120));
        // the dotfile repo's editor saves by rename, at the target
        std::fs::write(repo.join(".tmp"), "[appearance]\ntheme = \"coral\"\n").unwrap();
        std::fs::rename(repo.join(".tmp"), repo.join("oomtop.toml")).unwrap();
        let start = Instant::now();
        while live.current().config.appearance.theme != "coral" && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(live.current().config.appearance.theme, "coral");
    }

    #[test]
    fn temp_and_swap_files_are_ignored() {
        let ev = |name: &str| notify::Event {
            kind: notify::EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from(format!("/c/{name}"))],
            attrs: Default::default(),
        };
        assert!(!relevant(&ev(".config.toml.tmp-123")));
        assert!(!relevant(&ev("config.toml.swp")));
        assert!(!relevant(&ev("config.toml~")));
        assert!(relevant(&ev("config.toml")));
        assert_eq!(WatchMode::from_poll_ms(0), WatchMode::Native);
        assert_eq!(WatchMode::from_poll_ms(1), WatchMode::Poll(MIN_POLL));
        assert_eq!(
            WatchMode::from_poll_ms(2000),
            WatchMode::Poll(Duration::from_secs(2))
        );
    }
}

//! A local index of folders, programs and shortcuts, kept current at almost no CPU cost.
//! Design, edge cases and tests: `documentation/FILE_INDEX_IMPLEMENTATION.md`.
//!
//! * `store`   SQLite schema and every query.
//! * `rules`   what is indexed, name folding.
//! * `scan`    `reconcile_dir`: make the database agree with one folder.
//! * `watch`   one thread, `ReadDirectoryChangesW`, "something changed under here".
//! * `worker`  the single writer thread: queue, throttle, pause rules, housekeeping.
//! * `resolve` lookup, ranking, verifying a hit still exists, recovering when it does not.

pub mod platform;
pub mod resolve;
pub mod rules;
pub mod scan;
pub mod store;
pub mod watch;
mod worker;

pub use resolve::{Lookup, LookupRequest, Want};
pub use rules::Settings;

use platform::VolumeMap;
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

/// "Should indexing wait right now?" Supplied by the core (a task is thinking or speaking).
pub type PauseFn = Arc<dyn Fn() -> bool + Send + Sync>;

pub(crate) enum Cmd {
    Watch(watch::Event),
    MarkDirty(PathBuf),
    RemoveEntry(i64),
    Opened(i64),
    SetSettings(Settings),
    Rebuild,
    Forget,
    Stop,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct VolStatus {
    pub guid: String,
    pub online: bool,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub enabled: bool,
    pub index_files: bool,
    /// disabled | starting | scanning | idle | paused | error
    pub state: String,
    pub paused_reason: Option<String>,
    pub folders: i64,
    pub programs: i64,
    pub files: i64,
    pub queue: i64,
    /// The main places (Start menu, Desktop, Downloads, Documents, Program Files...) are indexed.
    pub tier1_ready: bool,
    /// Every place has been scanned (nothing left in the queue for the first pass). Until then a miss is not final.
    pub complete: bool,
    pub volumes: Vec<VolStatus>,
    pub watching: usize,
    pub polled: usize,
    pub last_update: i64,
    /// CPU seconds used by the indexer thread since Aqua started: the cost, measured.
    pub cpu_seconds: f64,
    pub db_bytes: u64,
    pub lookup_p50_ms: u32,
    pub lookup_p95_ms: u32,
    /// Slices that ran longer than planned.
    pub overruns: u64,
    /// Change notifications received from Windows, and folders listed because of any cause, since Aqua started.
    pub events: u64,
    pub listed: u64,
    /// The folders that produced the most change notifications (the noisiest places), most first.
    pub busiest: Vec<(String, u64)>,
    pub error: Option<String>,
    pub restarts: u32,
    pub rebuilt_after_damage: bool,
}

pub(crate) struct Shared {
    pub status: Mutex<Status>,
    pub vols: RwLock<VolumeMap>,
    pub lookups_ms: Mutex<VecDeque<u32>>,
    pub user_paused: AtomicBool,
    pub settings: RwLock<Settings>,
    pub watch_states: Mutex<Option<Arc<Mutex<Vec<watch::TargetState>>>>>,
}

pub struct FileIndex {
    pub(crate) db_path: PathBuf,
    pub(crate) tx: Sender<Cmd>,
    pub(crate) shared: Arc<Shared>,
}

static GLOBAL: OnceLock<Arc<FileIndex>> = OnceLock::new();

/// Start the index in `dir` (created if needed). `pause_fn` says when to wait. Call once, at startup.
pub fn init_global(dir: PathBuf, pause_fn: PauseFn) -> anyhow::Result<()> {
    if GLOBAL.get().is_some() {
        return Ok(());
    }
    let idx = FileIndex::start(dir, pause_fn)?;
    let _ = GLOBAL.set(idx);
    Ok(())
}

pub fn global() -> Option<&'static Arc<FileIndex>> {
    GLOBAL.get()
}

impl FileIndex {
    pub fn start(dir: PathBuf, pause_fn: PauseFn) -> anyhow::Result<Arc<FileIndex>> {
        worker::spawn(dir, pause_fn)
    }

    /// Start an index that covers only `roots` (tests and tools; the normal start covers this machine's usual places).
    pub fn start_with_roots(dir: PathBuf, pause_fn: PauseFn, roots: Vec<scan::Desired>) -> anyhow::Result<Arc<FileIndex>> {
        worker::spawn_with(dir, pause_fn, Some(roots))
    }

    pub fn status(&self) -> Status {
        let mut s = self.shared.status.lock().map(|s| s.clone()).unwrap_or_default();
        if let Ok(l) = self.shared.lookups_ms.lock() {
            let mut v: Vec<u32> = l.iter().copied().collect();
            v.sort_unstable();
            if !v.is_empty() {
                s.lookup_p50_ms = v[v.len() / 2];
                s.lookup_p95_ms = v[(v.len() * 95 / 100).min(v.len() - 1)];
            }
        }
        s.db_bytes = std::fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(self.db_path.with_extension("db-wal")).map(|m| m.len()).unwrap_or(0);
        s.index_files = self.settings().index_files;
        s
    }

    pub fn settings(&self) -> Settings {
        self.shared.settings.read().map(|s| s.clone()).unwrap_or_default()
    }

    /// Replace the settings. Rejects folders that are not local drive folders.
    pub fn set_settings(&self, s: Settings) -> Result<(), String> {
        for p in s.extra_roots.iter().chain(s.exclude_paths.iter()) {
            if !platform::is_local_drive_path(p) {
                return Err(format!("{p} is not a folder on a local drive (network and device paths are not allowed)"));
            }
            if p.split(['\\', '/']).any(|c| c == "..") {
                return Err(format!("{p} contains '..'"));
            }
        }
        for p in &s.extra_roots {
            if !std::path::Path::new(p).is_dir() {
                return Err(format!("{p} is not an existing folder"));
            }
        }
        if let Ok(mut cur) = self.shared.settings.write() {
            *cur = s.clone();
        }
        self.tx.send(Cmd::SetSettings(s)).map_err(|_| "the index is not running".to_string())
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.user_paused.store(paused, Ordering::SeqCst);
    }

    pub fn rebuild(&self) {
        let _ = self.tx.send(Cmd::Rebuild);
    }

    /// Delete everything the index stored and switch it off.
    pub fn forget(&self) {
        let _ = self.tx.send(Cmd::Forget);
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub(crate) fn record_lookup(&self, ms: u32) {
        if let Ok(mut l) = self.shared.lookups_ms.lock() {
            if l.len() >= 200 {
                l.pop_front();
            }
            l.push_back(ms);
        }
    }
}

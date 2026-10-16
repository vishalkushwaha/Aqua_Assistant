//! The single writer thread. It owns the database connection, the work queue and the throttle.
//!
//! Loop: take commands (watcher events, settings, stale-hit reports), wait while paused, turn changed paths into
//! queued folders, list a few folders in a short slice, rest, repeat. When there is nothing to do it blocks, so idle
//! cost is zero. Nothing here polls on a timer except a 30 second heartbeat that checks the pause state.

use super::platform::{self, VolumeMap};
use super::rules::{self, Settings, FILE, FOLDER};
use super::scan::{self, Ctx, Outcome};
use super::store::{self, Db};
use super::watch::{self, Blocked, Event, Target, Watcher};
use super::{Cmd, FileIndex, PauseFn, Shared, Status, VolStatus};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const SLICE: Duration = Duration::from_millis(40);
const REST: Duration = Duration::from_millis(250);
const COALESCE: Duration = Duration::from_secs(2);
const HEARTBEAT: Duration = Duration::from_secs(30);
const ROW_CAP: i64 = 1_000_000;
const MAX_RESTARTS_PER_HOUR: usize = 3;

pub(super) fn spawn(dir: PathBuf, pause_fn: PauseFn) -> Result<Arc<FileIndex>> {
    spawn_with(dir, pause_fn, None)
}

/// `fixed_roots` replaces the machine's usual roots (tests point the index at a temporary folder).
pub(super) fn spawn_with(dir: PathBuf, pause_fn: PauseFn, fixed_roots: Option<Vec<scan::Desired>>) -> Result<Arc<FileIndex>> {
    std::fs::create_dir_all(&dir)?;
    let db_path = dir.join("files.db");
    let (tx, rx) = channel::<Cmd>();
    let shared = Arc::new(Shared {
        status: std::sync::Mutex::new(Status { state: "starting".into(), ..Status::default() }),
        vols: std::sync::RwLock::new(VolumeMap::default()),
        lookups_ms: std::sync::Mutex::new(Default::default()),
        user_paused: Default::default(),
        settings: std::sync::RwLock::new(Settings::default()),
        watch_states: std::sync::Mutex::new(None),
    });
    let index = Arc::new(FileIndex { db_path: db_path.clone(), tx: tx.clone(), shared: shared.clone() });
    let sh = shared.clone();
    std::thread::Builder::new().name("aqua-index".into()).spawn(move || supervise(db_path, tx, rx, sh, pause_fn, fixed_roots))?;
    Ok(index)
}

/// Runs the worker, and starts it again (a few times) if it ever panics, so one bad folder cannot end indexing silently.
fn supervise(db_path: PathBuf, tx: std::sync::mpsc::Sender<Cmd>, rx: Receiver<Cmd>, shared: Arc<Shared>, pause_fn: PauseFn, fixed_roots: Option<Vec<scan::Desired>>) {
    let mut crashes: Vec<Instant> = Vec::new();
    let rx = std::sync::Mutex::new(rx);
    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rx = rx.lock().unwrap_or_else(|e| e.into_inner());
            match Worker::new(&db_path, tx.clone(), &rx, shared.clone(), pause_fn.clone(), fixed_roots.clone()) {
                Ok(mut w) => w.run(),
                Err(e) => {
                    set_error(&shared, format!("{e:#}"));
                }
            }
        }));
        match result {
            Ok(()) => return, // stop requested, or could not start (error published)
            Err(_) => {
                crashes.retain(|t| t.elapsed() < Duration::from_secs(3600));
                crashes.push(Instant::now());
                if let Ok(mut s) = shared.status.lock() {
                    s.restarts += 1;
                }
                tracing::warn!("file index worker panicked ({} in the last hour)", crashes.len());
                if crashes.len() >= MAX_RESTARTS_PER_HOUR {
                    set_error(&shared, "the file index stopped after repeated failures; turn it off and on again in the admin panel".into());
                    return;
                }
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

fn set_error(shared: &Shared, msg: String) {
    tracing::warn!("file index: {msg}");
    if let Ok(mut s) = shared.status.lock() {
        s.state = "error".into();
        s.error = Some(msg);
    }
}

fn now_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

struct Worker<'a> {
    db: Db,
    tx: std::sync::mpsc::Sender<Cmd>,
    rx: &'a Receiver<Cmd>,
    shared: Arc<Shared>,
    pause_fn: PauseFn,
    settings: Settings,
    fixed_roots: Option<Vec<scan::Desired>>,
    vols: VolumeMap,
    // caches rebuilt when roots or volumes change
    roots: HashMap<i64, (i64, i64)>,
    root_list: Vec<(i64, String, i64)>, // id, lower-case absolute path, tier
    root_paths: HashSet<String>,
    blocked: HashSet<String>,
    /// What the watcher thread drops before it reaches us (the same folders, plus the user's exclusions).
    watch_blocked: Blocked,
    /// Aqua's own data folder (lower case), blocked everywhere.
    own_data: Option<String>,
    /// Where documents are indexed, and where they are not (lower case, no trailing slash).
    doc_dirs: Vec<String>,
    doc_skip: Vec<String>,
    // work
    dirty: HashSet<PathBuf>,
    dirty_since: Option<Instant>,
    polled: HashSet<PathBuf>,
    watcher: Option<Watcher>,
    watch_stale: bool,
    stop: bool,
    // housekeeping clocks
    last_vol_refresh: Instant,
    last_counts: Instant,
    last_verify: Instant,
    last_polled_sweep: Instant,
    last_checkpoint: Instant,
    last_wake: SystemTime,
    last_slice_end: Instant,
    catchup_done: bool,
    over_cap: bool,
    overruns: u64,
    events_total: u64,
    listed_total: u64,
    busy_places: HashMap<PathBuf, u64>,
    counts_stale: bool,
    published_complete: bool,
    cached_busy: (Instant, Option<&'static str>),
    last_error_logged: Option<String>,
}

impl<'a> Worker<'a> {
    fn new(db_path: &Path, tx: std::sync::mpsc::Sender<Cmd>, rx: &'a Receiver<Cmd>, shared: Arc<Shared>, pause_fn: PauseFn, fixed_roots: Option<Vec<scan::Desired>>) -> Result<Worker<'a>> {
        let (db, rebuilt) = store::open_or_recover(db_path)?;
        let settings = db.load_settings();
        if let Ok(mut s) = shared.settings.write() {
            *s = settings.clone();
        }
        if rebuilt {
            if let Ok(mut s) = shared.status.lock() {
                s.rebuilt_after_damage = true;
            }
        }
        let now = Instant::now();
        let mut w = Worker {
            db,
            tx,
            rx,
            shared,
            pause_fn,
            settings,
            fixed_roots,
            vols: VolumeMap::default(),
            roots: HashMap::new(),
            root_list: Vec::new(),
            root_paths: HashSet::new(),
            blocked: scan::blocked_paths(),
            watch_blocked: Arc::new(std::sync::RwLock::new(Vec::new())),
            own_data: None,
            doc_dirs: Vec::new(),
            doc_skip: Vec::new(),
            dirty: HashSet::new(),
            dirty_since: None,
            polled: HashSet::new(),
            watcher: None,
            watch_stale: true,
            stop: false,
            last_vol_refresh: now,
            last_counts: now - Duration::from_secs(60),
            last_verify: now,
            last_polled_sweep: now,
            last_checkpoint: now,
            last_wake: SystemTime::now(),
            last_slice_end: now,
            catchup_done: false,
            over_cap: false,
            overruns: 0,
            events_total: 0,
            listed_total: 0,
            busy_places: HashMap::new(),
            counts_stale: true,
            published_complete: false,
            cached_busy: (now - Duration::from_secs(60), None),
            last_error_logged: None,
        };
        // Aqua's own data folder (its databases create and delete files all the time, and it holds its tokens and
        // models) is never indexed or watched.
        if let Some(data) = db_path.parent().and_then(Path::parent) {
            let d = data.to_string_lossy().trim_end_matches('\\').to_lowercase();
            w.blocked.insert(d.clone());
            w.own_data = Some(d);
        }
        w.refresh_watch_blocked();
        w.refresh_volumes(true);
        Ok(w)
    }

    // ------------------------------------------------------------------------------------------------ main loop

    fn run(&mut self) {
        platform::set_background_priority();
        if self.settings.enabled {
            self.start_indexing();
        }
        while !self.stop {
            self.drain();
            if self.stop {
                break;
            }
            if self.resumed_from_sleep() {
                self.mark_roots_dirty(5, 2);
            }
            if !self.settings.enabled {
                self.publish_state("disabled", None);
                self.block(Duration::from_secs(2));
                continue;
            }
            if let Some(reason) = self.pause_reason() {
                self.publish_state("paused", Some(reason));
                self.block(Duration::from_secs(2));
                continue;
            }
            self.flush_dirty();
            if self.watch_stale {
                self.update_watch_targets();
            }
            if self.work_slice() {
                continue;
            }
            if self.housekeeping() {
                continue;
            }
            self.publish_state("idle", None);
            // Counting rows is real work on a big index: only when something changed since the last count.
            if self.counts_stale || !self.published_complete {
                self.update_counts(true);
            } else {
                self.publish_cpu();
            }
            let wait = match self.dirty_since {
                Some(t) => COALESCE.saturating_sub(t.elapsed()).max(Duration::from_millis(50)),
                None => HEARTBEAT,
            };
            self.block(wait);
        }
        if let Some(w) = self.watcher.take() {
            w.stop();
        }
    }

    /// Wait up to `d` for a command, handling it. Returns early when something arrives.
    fn block(&mut self, d: Duration) {
        match self.rx.recv_timeout(d) {
            Ok(c) => self.handle(c),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => self.stop = true,
        }
    }

    fn drain(&mut self) {
        while let Ok(c) = self.rx.try_recv() {
            self.handle(c);
        }
    }

    fn pause_reason(&mut self) -> Option<&'static str> {
        if self.shared.user_paused.load(Ordering::SeqCst) {
            return Some("paused by you");
        }
        if (self.pause_fn)() {
            return Some("a task is running");
        }
        // The next two are a little costly: ask at most every 10 seconds.
        if self.cached_busy.0.elapsed() > Duration::from_secs(10) {
            let r = if platform::user_busy_fullscreen() {
                Some("a full-screen app or game is running")
            } else if platform::battery_saver() {
                Some("battery saver is on")
            } else {
                None
            };
            self.cached_busy = (Instant::now(), r);
        }
        self.cached_busy.1
    }

    /// The thread woke much later than planned: the machine slept. Watchers may have missed changes.
    fn resumed_from_sleep(&mut self) -> bool {
        let gap = SystemTime::now().duration_since(self.last_wake).unwrap_or_default();
        self.last_wake = SystemTime::now();
        gap > Duration::from_secs(120)
    }

    // ------------------------------------------------------------------------------------------------ commands

    fn handle(&mut self, c: Cmd) {
        match c {
            Cmd::Watch(Event::Changed(p)) => {
                self.events_total += 1;
                let place = p.parent().map(Path::to_path_buf).unwrap_or_else(|| p.clone());
                if self.busy_places.len() < 2_000 || self.busy_places.contains_key(&place) {
                    *self.busy_places.entry(place).or_insert(0) += 1;
                }
                self.note_dirty(p.parent().map(Path::to_path_buf).unwrap_or(p))
            }
            Cmd::Watch(Event::Overflow(p)) => self.mark_path_dirty(&p, 1),
            Cmd::Watch(Event::Lost(p)) | Cmd::Watch(Event::Stormed(p)) => {
                self.polled.insert(p.clone());
                self.mark_path_dirty(&p, 2);
                self.watch_stale = true;
            }
            Cmd::MarkDirty(p) => self.note_dirty(p),
            Cmd::RemoveEntry(id) => {
                let parent = self.db.entry(id).and_then(|e| e.parent_id);
                if self.db.begin().is_ok() {
                    let _ = self.db.delete_subtree(id);
                    let _ = self.db.commit();
                }
                if let Some(p) = parent {
                    let _ = self.db.todo_push(p, 0);
                }
            }
            Cmd::Opened(id) => {
                let _ = self.db.mark_opened(id, now_s());
            }
            Cmd::SetSettings(s) => self.apply_settings(s),
            Cmd::Rebuild => {
                let _ = self.db.clear_all();
                self.dirty.clear();
                self.catchup_done = true;
                if self.settings.enabled {
                    self.sync_roots();
                }
            }
            Cmd::Forget => self.forget(),
            Cmd::Stop => self.stop = true,
        }
    }

    fn note_dirty(&mut self, dir: PathBuf) {
        if self.dirty.len() < 50_000 {
            self.dirty.insert(dir);
        }
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    fn mark_path_dirty(&mut self, p: &Path, prio: i64) {
        if let Some(id) = self.nearest_dir(p) {
            let _ = self.db.todo_push(id, prio);
        }
    }

    fn refresh_watch_blocked(&self) {
        // Prefix matching: only whole folders that must never be seen (not the scan's skip-this-child set).
        let mut list: Vec<String> = self.own_data.iter().cloned().collect();
        list.extend(self.settings.exclude_paths.iter().map(|p| p.trim().trim_end_matches(['\\', '/']).to_lowercase()));
        if let Ok(mut w) = self.watch_blocked.write() {
            *w = list;
        }
    }

    fn apply_settings(&mut self, new: Settings) {
        let old = std::mem::replace(&mut self.settings, new.clone());
        self.refresh_watch_blocked();
        let _ = self.db.save_settings(&new);
        if let Ok(mut s) = self.shared.settings.write() {
            *s = new.clone();
        }
        if !new.enabled {
            if let Some(w) = &self.watcher {
                w.set_targets(Vec::new());
            }
            return;
        }
        if !old.enabled {
            self.start_indexing();
            return;
        }
        // Which files are kept changed. Narrowing: drop every file row and re-add what is still wanted. Widening, or
        // narrowing to something: list every folder again, because a folder whose own time has not changed would
        // otherwise be skipped and its files never found.
        if (old.index_files, old.index_documents) != (new.index_files, new.index_documents) {
            if (old.index_files && !new.index_files) || (old.index_documents && !new.index_documents) {
                if self.db.begin().is_ok() {
                    let _ = self.db.delete_kind(FILE);
                    let _ = self.db.commit();
                }
            }
            if new.index_files || new.index_documents {
                self.relist_all_folders(3);
                let _ = self.db.set_meta("docs_scanned", "1");
            }
        }
        if old.extra_roots != new.extra_roots || old.other_drives != new.other_drives {
            self.sync_roots();
        }
        if old.exclude_paths != new.exclude_paths {
            self.apply_exclusions();
        }
    }

    /// An exclude path was added: drop whatever is already indexed under it.
    fn apply_exclusions(&mut self) {
        let paths: Vec<String> = self.settings.exclude_paths.clone();
        for p in paths {
            if let Some(id) = self.exact_entry(Path::new(&p)) {
                if self.db.begin().is_ok() {
                    let _ = self.db.delete_subtree(id);
                    let _ = self.db.commit();
                }
            }
        }
        self.watch_stale = true;
    }

    fn forget(&mut self) {
        let _ = self.db.clear_all();
        let _ = self.db.conn.execute_batch("VACUUM; PRAGMA wal_checkpoint(TRUNCATE);");
        if let Some(dir) = self.db.conn.path().map(PathBuf::from).and_then(|p| p.parent().map(Path::to_path_buf)) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                if e.file_name().to_string_lossy().contains(".db.bad-") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        self.settings.enabled = false;
        let _ = self.db.save_settings(&self.settings);
        if let Ok(mut s) = self.shared.settings.write() {
            *s = self.settings.clone();
        }
        self.dirty.clear();
        self.root_list.clear();
        self.roots.clear();
        self.root_paths.clear();
        if let Some(w) = &self.watcher {
            w.set_targets(Vec::new());
        }
        self.update_counts(true);
        self.publish_state("disabled", None);
    }

    // ------------------------------------------------------------------------------------------------ roots and volumes

    fn start_indexing(&mut self) {
        self.refresh_volumes(true);
        self.sync_roots();
        if self.watcher.is_none() {
            let tx = self.tx.clone();
            let sink: watch::Sink = Arc::new(move |e| tx.send(Cmd::Watch(e)).is_ok());
            let w = Watcher::start(sink, self.watch_blocked.clone());
            if let Ok(mut g) = self.shared.watch_states.lock() {
                *g = Some(w.states.clone());
            }
            self.watcher = Some(w);
        }
        self.watch_stale = true;
        // An index saved before documents were indexed: its folders have not changed, so nothing would re-list them.
        // Do it once.
        if self.settings.index_documents && self.db.get_meta("docs_scanned").as_deref() != Some("1") {
            self.relist_all_folders(3);
            let _ = self.db.set_meta("docs_scanned", "1");
        }
    }

    fn refresh_volumes(&mut self, force: bool) {
        if !force && self.last_vol_refresh.elapsed() < Duration::from_secs(60) {
            return;
        }
        self.last_vol_refresh = Instant::now();
        let snap = platform::snapshot_volumes();
        let known = self.db.volumes();
        let mut returned: Vec<i64> = Vec::new();
        for v in &snap.list {
            if let Ok(id) = self.db.volume_id(&v.guid) {
                if known.iter().any(|k| k.0 == id && !k.2) {
                    returned.push(id);
                }
                let _ = self.db.set_volume_online(id, true);
            }
        }
        for (id, guid, online, _) in &known {
            if *online && !snap.list.iter().any(|v| &v.guid == guid) {
                let _ = self.db.set_volume_online(*id, false);
            }
        }
        let changed = snap.list.len() != self.vols.list.len() || snap.list.iter().zip(self.vols.list.iter()).any(|(a, b)| a != b);
        self.vols = snap;
        if let Ok(mut s) = self.shared.vols.write() {
            *s = self.vols.clone();
        }
        if let Ok(mut st) = self.shared.status.lock() {
            let mut list: Vec<VolStatus> = self.db.volumes().into_iter().map(|(_, guid, online, _)| VolStatus { guid, online }).collect();
            list.sort_by(|a, b| a.guid.cmp(&b.guid));
            st.volumes = list;
        }
        if changed {
            self.rebuild_caches();
            self.watch_stale = true;
        }
        // A drive that came back is caught up like a start.
        for vol in returned {
            let roots: Vec<i64> = self.db.roots().into_iter().filter(|r| r.volume_id == vol).map(|r| r.id).collect();
            for r in roots {
                let _ = self.db.todo_push(r, 5);
            }
        }
    }

    fn rebuild_caches(&mut self) {
        // Documents: the user's profile and (when other drives are on) every drive; never Program Files or Windows.
        let lower = |p: PathBuf| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
        self.doc_dirs = std::env::var_os("USERPROFILE").map(|p| vec![lower(PathBuf::from(p))]).unwrap_or_default();
        if self.settings.other_drives {
            self.doc_dirs.extend(self.vols.list.iter().filter(|v| v.fixed).filter_map(|v| v.mount.clone()).map(lower));
        }
        self.doc_skip = ["ProgramFiles", "ProgramFiles(x86)", "ProgramData", "SystemRoot"].iter().filter_map(|v| std::env::var_os(v)).map(|p| lower(PathBuf::from(p))).collect();
        let rows = self.db.roots();
        self.roots = rows.iter().map(|r| (r.id, (r.tier, r.max_depth))).collect();
        self.root_list.clear();
        self.root_paths.clear();
        for r in &rows {
            let Some(guid) = self.db.volume_guid(r.volume_id) else { continue };
            let Some(mut p) = self.vols.mount_of(&guid) else { continue };
            if !r.rel_path.is_empty() {
                p.push(&r.rel_path);
            }
            let lower = p.to_string_lossy().trim_end_matches('\\').to_lowercase();
            self.root_paths.insert(lower.clone());
            self.root_list.push((r.id, lower, r.tier));
        }
    }

    /// Make the stored roots match what this machine and the settings call for.
    fn sync_roots(&mut self) {
        let desired = match &self.fixed_roots {
            Some(f) => f.clone(),
            None => scan::desired_roots(&self.settings, &self.vols),
        };
        let existing = self.db.roots();
        // The root set changes how folders are skipped, so the cache must be current before anything is listed.
        let mut wanted_keys: HashSet<(i64, String)> = HashSet::new();
        for d in &desired {
            let Some((vol, rel)) = self.vols.locate(&d.path) else { continue };
            let guid = vol.guid.clone();
            let Ok(vid) = self.db.volume_id(&guid) else { continue };
            let rel_s = rel.to_string_lossy().to_string();
            wanted_keys.insert((vid, rel_s.to_lowercase()));
            let already = existing.iter().any(|r| r.volume_id == vid && r.rel_path.eq_ignore_ascii_case(&rel_s));
            if !already {
                self.rebuild_caches();
                // A new root inside an existing tree takes that subtree over: drop the outer root's rows for it.
                if let Some(old) = self.exact_entry(&d.path) {
                    if self.db.begin().is_ok() {
                        let _ = self.db.delete_subtree(old);
                        let _ = self.db.commit();
                    }
                }
            }
            let _ = self.db.add_root(vid, &rel_s, d.tier, d.max_depth, d.user_added);
        }
        // Roots that are no longer wanted: remove them, and let the root above pick the folder up again.
        for r in existing {
            if !wanted_keys.contains(&(r.volume_id, r.rel_path.to_lowercase())) {
                let p = self.db.path_of(r.id, &self.vols);
                let _ = self.db.remove_root(r.id);
                if let Some(p) = p {
                    self.rebuild_caches();
                    self.mark_path_dirty(p.parent().unwrap_or(&p), 2);
                }
            }
        }
        self.rebuild_caches();
        self.watch_stale = true;
    }

    /// Put every indexed folder of the main roots on the queue, in one transaction.
    fn relist_all_folders(&mut self, prio: i64) {
        let ids = self.db.folder_ids(3);
        if self.db.begin().is_ok() {
            for id in ids {
                let _ = self.db.todo_push(id, prio);
            }
            let _ = self.db.commit();
        }
        self.counts_stale = true;
    }

    fn mark_roots_dirty(&mut self, prio: i64, max_tier: i64) {
        let ids: Vec<i64> = self.roots.iter().filter(|(_, (t, _))| *t <= max_tier).map(|(id, _)| *id).collect();
        for id in ids {
            let _ = self.db.todo_push(id, prio);
        }
        // The folders below are re-listed through the catch-up pass.
        self.catchup_done = false;
        let _ = self.db.set_meta("catchup_cursor", "0");
    }

    // ------------------------------------------------------------------------------------------------ paths and events

    /// The deepest indexed folder that is `p` or contains it (the nearest ancestor we know about).
    fn nearest_dir(&mut self, p: &Path) -> Option<i64> {
        let lower = p.to_string_lossy().to_lowercase();
        let (root_id, root_path) = self
            .root_list
            .iter()
            .filter(|(_, rp, _)| lower == *rp || lower.starts_with(&format!("{rp}\\")))
            .max_by_key(|(_, rp, _)| rp.len())
            .map(|(id, rp, _)| (*id, rp.clone()))?;
        let rest = lower[root_path.len()..].trim_start_matches('\\').to_string();
        let mut cur = root_id;
        for comp in rest.split('\\').filter(|c| !c.is_empty()) {
            match self.db.child_by_key(cur, comp) {
                Some(id) => cur = id,
                None => break,
            }
        }
        // A file matched on the way: the folder that holds it is what changed.
        match self.db.entry(cur) {
            Some(e) if e.kind != FOLDER => e.parent_id,
            Some(_) => Some(cur),
            None => None,
        }
    }

    /// The entry for exactly this path, if it is indexed.
    fn exact_entry(&mut self, p: &Path) -> Option<i64> {
        let id = self.nearest_dir(p)?;
        let target = p.to_string_lossy().trim_end_matches('\\').to_lowercase();
        let found = self.db.path_of(id, &self.vols)?.to_string_lossy().trim_end_matches('\\').to_lowercase();
        (found == target).then_some(id)
    }

    fn flush_dirty(&mut self) {
        let Some(since) = self.dirty_since else { return };
        if since.elapsed() < COALESCE {
            return;
        }
        let dirs: Vec<PathBuf> = self.dirty.drain().collect();
        self.dirty_since = None;
        let mut queued: HashSet<i64> = HashSet::new();
        for d in dirs {
            if let Some(id) = self.nearest_dir(&d) {
                if queued.insert(id) {
                    let _ = self.db.todo_push(id, 0);
                }
            }
        }
    }

    // ------------------------------------------------------------------------------------------------ the work

    /// List a few folders (about 40 ms of work), then rest. Returns true if it did any work.
    fn work_slice(&mut self) -> bool {
        let t0 = Instant::now();
        let mut did = false;
        let mut root_listed = false;
        while t0.elapsed() < SLICE {
            let Some(id) = self.db.todo_pop() else { break };
            let ctx = Ctx { settings: &self.settings, vols: &self.vols, root_paths: &self.root_paths, blocked_paths: &self.blocked, roots: &self.roots, now: now_s(), doc_dirs: &self.doc_dirs, doc_skip: &self.doc_skip, over_cap: self.over_cap };
            match scan::reconcile_dir(&mut self.db, &ctx, id) {
                Ok(Outcome::Listed { added, removed, changed }) => {
                    self.listed_total += 1;
                    if added + removed + changed > 0 {
                        self.counts_stale = true;
                    }
                    if self.roots.contains_key(&id) {
                        root_listed = true;
                    }
                }
                Ok(_) => {}
                Err(e) => self.log_once(format!("{e:#}")),
            }
            did = true;
        }
        if root_listed {
            self.watch_stale = true;
        }
        if !did {
            return false;
        }
        if t0.elapsed() > Duration::from_millis(250) {
            self.overruns += 1;
        }
        self.update_counts(false);
        self.publish_state("scanning", None);
        let rest = if platform::on_battery() { REST * 3 } else { REST };
        self.last_slice_end = Instant::now();
        self.block(rest);
        true
    }

    fn log_once(&mut self, msg: String) {
        if self.last_error_logged.as_deref() != Some(msg.as_str()) {
            tracing::warn!("file index: {msg}");
            self.last_error_logged = Some(msg.clone());
        }
        // A full disk or a locked database must not look like a working index.
        if msg.to_lowercase().contains("disk is full") || msg.to_lowercase().contains("database or disk is full") {
            set_error(&self.shared, "the disk is full: indexing is paused".into());
        }
    }

    /// Small periodic jobs, one per call. Returns true if it did something that should be followed by a rest.
    fn housekeeping(&mut self) -> bool {
        self.refresh_volumes(false);
        // Startup catch-up: tier 1 and 2 folders whose own time changed while Aqua was closed (or asleep).
        if !self.catchup_done && self.db.todo_len_up_to(3) == 0 {
            let cursor: i64 = self.db.get_meta("catchup_cursor").and_then(|v| v.parse().ok()).unwrap_or(0);
            let batch = self.db.folders_after(cursor, 500);
            let mut last = cursor;
            for (id, stored) in &batch {
                last = *id;
                let Some(path) = self.db.path_of(*id, &self.vols) else { continue };
                let mt = std::fs::metadata(platform::long_path(&path)).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64);
                if mt != Some(*stored) {
                    let _ = self.db.todo_push(*id, 5);
                }
            }
            if batch.len() < 500 {
                self.catchup_done = true;
                let _ = self.db.set_meta("catchup_cursor", "0");
            } else {
                let _ = self.db.set_meta("catchup_cursor", &last.to_string());
            }
            self.block(REST);
            return true;
        }
        let on_ac = !platform::on_battery();
        // Weekly verify, an hour at a time: folders not listed for 7 days.
        if on_ac && self.last_verify.elapsed() > Duration::from_secs(3600) {
            self.last_verify = Instant::now();
            for id in self.db.stale_folders(now_s() - 7 * 86_400, 500) {
                let _ = self.db.todo_push(id, 7);
            }
            if self.db.todo_len() > 0 {
                return true;
            }
        }
        // Folders that could not be watched (storm, lost): look at them every 10 minutes.
        if on_ac && !self.polled.is_empty() && self.last_polled_sweep.elapsed() > Duration::from_secs(600) {
            self.last_polled_sweep = Instant::now();
            let paths: Vec<PathBuf> = self.polled.iter().cloned().collect();
            for p in paths {
                self.mark_path_dirty(&p, 5);
            }
            return true;
        }
        if self.last_checkpoint.elapsed() > Duration::from_secs(300) {
            self.last_checkpoint = Instant::now();
            let _ = self.db.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        }
        false
    }

    // ------------------------------------------------------------------------------------------------ watching

    fn update_watch_targets(&mut self) {
        self.watch_stale = false;
        let Some(w) = &self.watcher else { return };
        let mut targets: Vec<Target> = Vec::new();
        let mut push = |t: Target, polled: &HashSet<PathBuf>| {
            if !polled.contains(&t.path) && !targets.contains(&t) {
                targets.push(t);
            }
        };
        let rows = self.db.roots();
        for r in rows {
            let Some(guid) = self.db.volume_guid(r.volume_id) else { continue };
            let Some(mut path) = self.vols.mount_of(&guid) else { continue };
            if !r.rel_path.is_empty() {
                path.push(&r.rel_path);
            }
            if !path.is_dir() {
                continue;
            }
            // A big tree (the profile, a drive) is watched folder by folder, so noisy places (AppData, caches) are
            // never watched at all; its top level is watched on its own to see new folders appear.
            if r.tier >= 2 {
                let kids: Vec<PathBuf> = std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false))
                    .filter_map(|e| {
                        let n = e.file_name().to_string_lossy().to_lowercase();
                        let child = path.join(e.file_name());
                        let cl = child.to_string_lossy().to_lowercase();
                        (!rules::excluded_dir(&n) && !rules::sensitive_dir(&n) && !self.root_paths.contains(&cl) && !self.blocked.contains(&cl) && !rules::under_excluded_path(&cl, &self.settings)).then_some(child)
                    })
                    .collect();
                if kids.len() <= 40 {
                    push(Target { path: path.clone(), recursive: false }, &self.polled);
                    for k in kids {
                        push(Target { path: k, recursive: true }, &self.polled);
                    }
                    continue;
                }
            }
            push(Target { path, recursive: true }, &self.polled);
        }
        w.set_targets(targets);
    }

    // ------------------------------------------------------------------------------------------------ status

    fn publish_cpu(&self) {
        if let Ok(mut s) = self.shared.status.lock() {
            s.cpu_seconds = platform::thread_cpu_ms() as f64 / 1000.0;
            s.events = self.events_total;
            s.listed = self.listed_total;
        }
    }

    fn publish_state(&self, state: &str, reason: Option<&str>) {
        if let Ok(mut s) = self.shared.status.lock() {
            if s.state == "error" && state != "disabled" {
                return;
            }
            s.state = state.to_string();
            s.paused_reason = reason.map(str::to_string);
            s.enabled = self.settings.enabled;
        }
    }

    fn update_counts(&mut self, force: bool) {
        if !force && self.last_counts.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.last_counts = Instant::now();
        self.counts_stale = false;
        let (folders, programs, files) = self.db.counts();
        let queue = self.db.todo_len();
        let tier1_pending = self.db.todo_len_up_to(1);
        let tier3_pending = self.db.todo_len_up_to(3);
        let (mut watching, mut polled) = (0, 0);
        if let Ok(g) = self.shared.watch_states.lock() {
            if let Some(states) = g.as_ref() {
                if let Ok(list) = states.lock() {
                    watching = list.iter().filter(|t| t.state == "watching").count();
                    polled = list.iter().filter(|t| t.state != "watching").count();
                }
            }
        }
        if let Ok(mut s) = self.shared.status.lock() {
            s.folders = folders;
            s.programs = programs;
            s.files = files;
            s.queue = queue;
            s.tier1_ready = !self.roots.is_empty() && tier1_pending == 0;
            s.complete = !self.roots.is_empty() && tier3_pending == 0;
            self.published_complete = s.complete;
            s.watching = watching;
            s.polled = polled;
            s.last_update = now_s();
            s.cpu_seconds = platform::thread_cpu_ms() as f64 / 1000.0;
            s.overruns = self.overruns;
            s.events = self.events_total;
            s.listed = self.listed_total;
            let mut top: Vec<(&PathBuf, &u64)> = self.busy_places.iter().collect();
            top.sort_by(|a, b| b.1.cmp(a.1));
            s.busiest = top.into_iter().take(6).map(|(p, n)| (p.to_string_lossy().to_string(), *n)).collect();
            s.enabled = self.settings.enabled;
        }
        // Past the cap, the biggest and least useful tier (other drives) stops growing.
        self.over_cap = folders + programs + files > ROW_CAP;
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_index::{LookupRequest, Want};
    use std::sync::atomic::AtomicBool;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_worker_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("tree")).unwrap();
        d
    }

    fn roots_for(tree: &Path) -> Vec<scan::Desired> {
        vec![scan::Desired { path: tree.to_path_buf(), tier: 1, max_depth: 6, user_added: false }]
    }

    fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(secs) {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out after {secs}s waiting for: {what}");
    }

    fn find(ix: &FileIndex, name: &str, want: Want) -> Vec<PathBuf> {
        ix.lookup(&LookupRequest { query: name.into(), want, limit: 5 }).found.into_iter().map(|f| f.path).collect()
    }

    fn never_pause() -> PauseFn {
        Arc::new(|| false)
    }

    #[test]
    fn it_scans_then_follows_create_rename_and_delete_without_being_told() {
        let d = tmp("follow");
        let tree = d.join("tree");
        std::fs::create_dir_all(tree.join("Work/Personal_AI")).unwrap();
        let ix = FileIndex::start_with_roots(d.join("data").join("idx"), never_pause(), roots_for(&tree)).unwrap();
        wait_until("first scan", 30, || ix.status().tier1_ready && ix.status().queue == 0);
        assert_eq!(find(&ix, "Personal AI", Want::Folder).len(), 1);

        // A new folder with a program in it is picked up by the watcher.
        std::fs::create_dir_all(tree.join("Games/Racing Game")).unwrap();
        std::fs::write(tree.join("Games/Racing Game/race.exe"), b"1234567").unwrap();
        wait_until("created folder found", 20, || !find(&ix, "Racing Game", Want::Folder).is_empty() && !find(&ix, "race", Want::Program).is_empty());

        // Rename the folder: the new name is found, the old one is not.
        std::fs::rename(tree.join("Games/Racing Game"), tree.join("Games/Speed Game")).unwrap();
        wait_until("renamed folder found", 20, || !find(&ix, "Speed Game", Want::Folder).is_empty() && find(&ix, "Racing Game", Want::Folder).is_empty());

        // Move the project to another folder: found at the new place, never at the old one.
        std::fs::create_dir_all(tree.join("Archive")).unwrap();
        std::fs::rename(tree.join("Work/Personal_AI"), tree.join("Archive/Personal_AI")).unwrap();
        wait_until("moved folder found at its new place", 20, || {
            let f = find(&ix, "Personal AI", Want::Folder);
            f.len() == 1 && f[0].starts_with(tree.join("Archive"))
        });

        // Delete: gone.
        std::fs::remove_dir_all(tree.join("Archive/Personal_AI")).unwrap();
        wait_until("deleted folder gone", 20, || find(&ix, "Personal AI", Want::Folder).is_empty());
        let st = ix.status();
        assert!(st.watching >= 1, "the watcher is running: {st:?}");
        ix.stop();
    }

    #[test]
    fn nothing_is_listed_while_paused_and_the_scan_continues_when_it_ends() {
        let d = tmp("pause");
        let tree = d.join("tree");
        for i in 0..40 {
            std::fs::create_dir_all(tree.join(format!("dir{i}/sub"))).unwrap();
        }
        let paused = Arc::new(AtomicBool::new(true));
        let p = paused.clone();
        let ix = FileIndex::start_with_roots(d.join("data").join("idx"), Arc::new(move || p.load(Ordering::SeqCst)), roots_for(&tree)).unwrap();
        std::thread::sleep(Duration::from_secs(3));
        let st = ix.status();
        assert_eq!((st.folders, st.state.as_str()), (0, "paused"), "a task is running: nothing is listed ({st:?})");
        paused.store(false, Ordering::SeqCst);
        wait_until("scan after the pause", 30, || ix.status().folders >= 80);
        ix.stop();
    }

    #[test]
    fn the_queue_survives_a_restart() {
        let d = tmp("resume");
        let tree = d.join("tree");
        for i in 0..30 {
            std::fs::create_dir_all(tree.join(format!("a{i}/b/c"))).unwrap();
        }
        // First run: paused the whole time, so the root is queued and nothing else happens. Then it is stopped.
        let ix1 = FileIndex::start_with_roots(d.join("data").join("idx"), Arc::new(|| true), roots_for(&tree)).unwrap();
        wait_until("queued", 10, || ix1.status().state == "paused");
        ix1.stop();
        std::thread::sleep(Duration::from_millis(600));
        // Second run on the same database finishes the job.
        let ix2 = FileIndex::start_with_roots(d.join("data").join("idx"), never_pause(), roots_for(&tree)).unwrap();
        wait_until("resumed scan", 30, || ix2.status().folders >= 30 * 3);
        assert_eq!(ix2.status().folders, 1 + 30 * 3, "root + 30 x (a, b, c)");
        ix2.stop();
    }

    #[test]
    fn turning_it_off_and_forgetting_leaves_no_data() {
        let d = tmp("forget");
        let tree = d.join("tree");
        std::fs::create_dir_all(tree.join("Secret Project")).unwrap();
        let ix = FileIndex::start_with_roots(d.join("data").join("idx"), never_pause(), roots_for(&tree)).unwrap();
        wait_until("scan", 20, || ix.status().folders >= 2);
        ix.forget();
        wait_until("forgotten", 10, || ix.status().folders == 0 && !ix.settings().enabled);
        wait_until("disabled", 10, || ix.status().state == "disabled");
        assert!(find(&ix, "Secret Project", Want::Folder).is_empty());
        let bytes = std::fs::read(d.join("data").join("idx").join("files.db")).unwrap_or_default();
        assert!(!bytes.windows(14).any(|w| w == b"Secret Project"), "the name is not left in the file");
        ix.stop();
    }

    #[test]
    fn an_excluded_folder_is_removed_and_a_damaged_database_is_rebuilt() {
        let d = tmp("damage");
        let tree = d.join("tree");
        std::fs::create_dir_all(tree.join("Keep/x")).unwrap();
        std::fs::create_dir_all(tree.join("Private/y")).unwrap();
        let ix = FileIndex::start_with_roots(d.join("data").join("idx"), never_pause(), roots_for(&tree)).unwrap();
        wait_until("scan", 20, || ix.status().folders >= 5);
        let mut s = ix.settings();
        s.exclude_paths = vec![tree.join("Private").to_string_lossy().to_string()];
        ix.set_settings(s).unwrap();
        wait_until("excluded", 20, || find(&ix, "Private", Want::Folder).is_empty() && !find(&ix, "Keep", Want::Folder).is_empty());
        ix.stop();
        std::thread::sleep(Duration::from_millis(800));
        // Damage the file, start again: it is moved aside and rebuilt.
        std::fs::write(d.join("data").join("idx").join("files.db"), vec![7u8; 4096]).unwrap();
        let _ = std::fs::remove_file(d.join("data").join("idx").join("files.db-wal"));
        let _ = std::fs::remove_file(d.join("data").join("idx").join("files.db-shm"));
        let ix2 = FileIndex::start_with_roots(d.join("data").join("idx"), never_pause(), roots_for(&tree)).unwrap();
        wait_until("rebuilt", 30, || ix2.status().folders >= 3 && ix2.status().rebuilt_after_damage);
        assert!(std::fs::read_dir(d.join("data").join("idx")).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains(".db.bad-")));
        ix2.stop();
    }

    #[test]
    fn documents_are_found_by_name_with_or_without_the_extension_and_the_switch_reaches_subfolders() {
        let d = tmp("docs");
        let tree = d.join("tree");
        std::fs::create_dir_all(tree.join("Papers/2024/Jobs")).unwrap();
        std::fs::write(tree.join("Papers/2024/Jobs/Vishal CV.pdf"), b"%PDF").unwrap();
        std::fs::write(tree.join("Papers/2024/Budget plan.xlsx"), b"PK").unwrap();
        std::fs::write(tree.join("Papers/2024/holiday.jpg"), b"jpg").unwrap();
        let ix = FileIndex::start_with_roots(d.join("data").join("idx"), never_pause(), roots_for(&tree)).unwrap();
        wait_until("first scan", 30, || ix.status().complete && ix.status().queue == 0);
        for q in ["Vishal CV.pdf", "vishal cv", "Vishal CV"] {
            let f = find(&ix, q, Want::File);
            assert_eq!(f.len(), 1, "{q}: {f:?}");
            assert!(f[0].ends_with("Vishal CV.pdf"));
        }
        assert!(!find(&ix, "Budget plan.xlsx", Want::File).is_empty());
        assert!(find(&ix, "holiday", Want::File).is_empty(), "photos are not documents");

        // A document saved later is picked up by the watcher.
        std::fs::write(tree.join("Papers/2024/Jobs/Cover letter.docx"), b"PK").unwrap();
        wait_until("new document found", 20, || !find(&ix, "Cover letter.docx", Want::File).is_empty());

        // Switch documents off: they go. Switch on again: they come back, even in folders that did not change.
        let mut s = ix.settings();
        s.index_documents = false;
        ix.set_settings(s.clone()).unwrap();
        wait_until("documents gone", 20, || find(&ix, "Vishal CV", Want::File).is_empty());
        s.index_documents = true;
        ix.set_settings(s).unwrap();
        wait_until("documents back in the subfolder", 30, || !find(&ix, "Vishal CV.pdf", Want::File).is_empty() && !find(&ix, "Cover letter", Want::File).is_empty());
        ix.stop();
    }

    #[test]
    fn settings_that_point_off_this_computer_are_refused() {
        let d = tmp("settings");
        let ix = FileIndex::start_with_roots(d.join("data").join("idx"), never_pause(), roots_for(&d.join("tree"))).unwrap();
        for bad in [r"\\server\share", r"\\?\C:\x", "relative", "C:\\a\\..\\b"] {
            let mut s = ix.settings();
            s.extra_roots = vec![bad.to_string()];
            assert!(ix.set_settings(s).is_err(), "{bad}");
        }
        let mut s = ix.settings();
        s.extra_roots = vec![d.join("tree").to_string_lossy().to_string()];
        assert!(ix.set_settings(s).is_ok());
        ix.stop();
    }
}

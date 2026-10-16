//! Aqua's own recycle folder. Deleting, and overwriting a file, moves the old content here first
//! so every change the tools make can be undone with `fs.restore`.
//!
//! Layout: `<dir>/<undo_id>/content` (the file or folder), `<dir>/<undo_id>/meta.json` (the record) and `<dir>/index.jsonl` (which run and which
//! group each slot belongs to, so that a run or a group is found without reading every record).
//!
//! **The record is written first.** A stash writes `meta.json` with the state `pending` before it touches the file, moves the file, and only
//! then marks the record `committed`; every write of a record goes through a temporary file and a rename, so a crash leaves the old record or
//! the new one, never half of one. The consequences, each of them tested:
//! * a record that cannot be written stops the stash with the file where it was;
//! * a kill between the move and the commit leaves a `pending` record with the content in the slot: it is listed, and restorable by its id;
//! * a slot that holds content but has no record that can be read (a truncated file, a crash in an older version) is listed as `unrecorded`
//!   and never deleted by the clean-up of crash leftovers: only a slot with nothing in it is;
//! * a backup copy (`copying`) is hidden until it is complete, because the original is still where it was and the change it protects has not begun.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

/// How long a trashed item is kept, and how much the trash may hold. Past either, the oldest go first.
pub const DEFAULT_MAX_AGE_DAYS: u64 = 30;
pub const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// How often a stash looks at the limits (a scan reads every record, so it is not done on every delete).
const ENFORCE_EVERY_MS: u64 = 10 * 60 * 1000;
/// A trash slot with nothing in it, left by a crash in the middle of a stash, is cleared after this long. (A slot that holds content is never cleared as a leftover.)
const ORPHAN_AFTER_MS: u64 = 24 * 60 * 60 * 1000;
/// A stash that began less than this long ago may still be going on in another thread: the size limit does not touch it.
const IN_FLIGHT_MS: u64 = 10 * 60 * 1000;
const INDEX_FILE: &str = "index.jsonl";

/// What a clean-up removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Purged {
    pub entries: usize,
    pub bytes: u64,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub undo_id: String,
    pub original_path: String,
    /// `deleted`, `overwritten` or `edited`.
    pub reason: String,
    pub kind: String,
    pub bytes: u64,
    pub at_unix_ms: u64,
    /// The patch (`p_...`) this copy belongs to, when a change to several files was made as one: they are undone together.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The agent run that made this change, when one did: `fs.undo_run` puts back everything one run changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_run_id: Option<String>,
    /// For a move: where the thing went (`original_path` is where it came from).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_path: Option<String>,
    /// Where the record is in its life: `pending` (written before the file is moved), `copying` (a backup being copied), `partial` (a move that stopped half way, the copy
    /// is kept) or `committed`. A record from before this field has none, and is a committed one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Set by [`Trash::list`] on a slot that holds content but whose record cannot be read; `original_path` is then what could be salvaged, often empty. Never stored.
    #[serde(default, skip_deserializing, skip_serializing_if = "is_false")]
    pub unrecorded: bool,
    /// The SHA-256 of the file as the change left it, when the change was to one file: a file that has the same content later was not changed by anybody (an editor that
    /// saved it again), whatever its modification time says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_sha256: Option<String>,
    /// Which of the run's file-changing tool calls made this record (the first is 1): `fs.undo_run` can put back from a step on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    /// What a record of a command or a checkpoint says about where its files are kept (reasons `command` and `checkpoint`, card OA7-03). None for every other record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<EffectRecord>,
}

/// What a record with no copy in it says about a command or a checkpoint of a run (card OA7-03: TM-12, ED-11, CX-11). The files themselves are git objects, in the repository's own
/// object store (a command: the commit after it, whose parent is the commit before) or in the store of Aqua's own under `data/snapshots` (a checkpoint: two trees); this holds only
/// the names to find them by.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectRecord {
    /// A command: the id its result named (`term.undo call_id=...`). A checkpoint: empty.
    #[serde(default)]
    pub id: String,
    /// The repository (a command) or the project folder (a checkpoint).
    #[serde(default)]
    pub root: String,
    /// A command: the reference (`refs/aqua/journal/<run>/<id>`) of the commit that holds the tree after it; the tree before is that commit's parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// A checkpoint: the tree before the first command of the run, and the tree at its end (set when the run ends), as git object ids in the store of the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    /// A command: what it was, short (the program and its first word, never its arguments).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// How many files it changed (a command), when it is known.
    #[serde(default)]
    pub changed: usize,
}

impl Entry {
    /// The state is anything but a finished one: a move that was interrupted, so the copy may be incomplete.
    pub fn is_interrupted(&self) -> bool {
        matches!(self.state.as_deref(), Some("pending") | Some("partial"))
    }
}

thread_local! {
    static RUN: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    /// The step the tool call running on this thread was given. The first record the call makes takes the next one for its run; the others of the call share it.
    static STEP: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// Runs `f` with every record the undo store makes on this thread tagged with the agent run `run_id` (an empty id tags nothing). The previous
/// tag comes back when `f` ends, however it ends.
pub fn with_run<R>(run_id: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>, Option<u32>);
    impl Drop for Restore {
        fn drop(&mut self) {
            RUN.with(|r| *r.borrow_mut() = self.0.take());
            STEP.with(|s| s.set(self.1.take()));
        }
    }
    let tag = (!run_id.is_empty()).then(|| run_id.to_string());
    let _restore = Restore(RUN.with(|r| r.replace(tag)), STEP.with(|s| s.replace(None)));
    f()
}

fn current_run() -> Option<String> {
    RUN.with(|r| r.borrow().clone())
}

/// Writes one record: the path and the bytes. The default is [`write_record_durably`]; a test puts another in its place to make a write fail.
pub type RecordWriter = Box<dyn Fn(&Path, &[u8]) -> std::io::Result<()> + Send + Sync>;

/// Writes `bytes` to `path` through a temporary file next to it (written, flushed to the disk, then renamed over `path`), so that a crash or a full disk leaves the old file or the
/// new one and never a truncated one. A failure removes the temporary file. This is the tool host's one durable writer (`aqua_tool_host::fsutil`).
pub fn write_record_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    aqua_tool_host::fsutil::write_atomic_durable(path, bytes)
}

/// What a slot of the trash folder is.
enum Slot {
    /// Something to show: a record, or content without one.
    Listed(Entry),
    /// Nothing worth showing: an empty slot, a stash that stopped before it moved anything, a backup that was not finished. The clean-up of leftovers may remove it once it is old.
    Junk,
    /// A folder whose name is not an undo id: not ours, left alone.
    Foreign,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// One line of `index.jsonl`.
#[derive(Debug, Serialize, Deserialize)]
struct IndexLine {
    undo_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    at: u64,
}

pub struct Trash {
    dir: PathBuf,
    /// When the limits were last looked at by a stash.
    last_enforced_ms: AtomicU64,
    /// How a record is written (see [`RecordWriter`]).
    writer: RecordWriter,
    /// The last step given to each run in this process.
    steps: Mutex<HashMap<String, u32>>,
    /// Slots whose damage has been logged already, so that a listing does not repeat it every time.
    warned: Mutex<HashSet<String>>,
    /// Says whether an agent run is still going on; its entries are the last the size limit takes (see [`Trash::set_live_run_check`]).
    live_run: OnceLock<Box<dyn Fn(&str) -> bool + Send + Sync>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Trash {
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            last_enforced_ms: AtomicU64::new(0),
            writer: Box::new(|path, bytes| write_record_durably(path, bytes)),
            steps: Mutex::new(HashMap::new()),
            warned: Mutex::new(HashSet::new()),
            live_run: OnceLock::new(),
        })
    }

    /// Use `writer` for every record instead of [`write_record_durably`] (tests make it fail).
    pub fn with_record_writer(mut self, writer: RecordWriter) -> Self {
        self.writer = writer;
        self
    }

    /// Installs the question "is this agent run still going on?". The size limit then takes the entries of such a run last: a long run keeps its earliest undo copies while it
    /// works. Set once; `false` when one was set already.
    pub fn set_live_run_check(&self, check: impl Fn(&str) -> bool + Send + Sync + 'static) -> bool {
        self.live_run.set(Box::new(check)).is_ok()
    }

    fn run_is_live(&self, run_id: &str) -> bool {
        self.live_run.get().is_some_and(|check| check(run_id))
    }

    /// Remove what is too old (`max_age_days`), then the oldest entries until the trash holds at most `max_bytes`, and clear slots a
    /// crash left with nothing in them. `protect` is never removed: the entry that was just made must still be there to undo. The entries of a run that is still going on are
    /// taken last, and only when the trash is over twice its limit.
    pub fn enforce_limits(&self, max_age_days: u64, max_bytes: u64, protect: Option<&str>) -> Purged {
        self.clear(max_age_days, max_bytes, protect, ORPHAN_AFTER_MS)
    }

    fn clear(&self, max_age_days: u64, max_bytes: u64, protect: Option<&str>, orphan_after_ms: u64) -> Purged {
        let mut purged = Purged::default();
        let slots = self.scan();
        let mut entries: Vec<Entry> = slots.iter().filter_map(|(_, s)| if let Slot::Listed(e) = s { Some(e.clone()) } else { None }).collect();
        entries.sort_by(|a, b| b.at_unix_ms.cmp(&a.at_unix_ms)); // newest first
        let now = now_ms();
        let cutoff = now.saturating_sub(max_age_days.saturating_mul(86_400_000));
        let mut gone: HashSet<String> = HashSet::new();
        for e in &entries {
            let too_old = e.at_unix_ms < cutoff && Some(e.undo_id.as_str()) != protect && !in_flight(e, now);
            if too_old && self.drop_slot(e, &mut purged) {
                gone.insert(e.undo_id.clone());
            }
        }
        let mut total: u64 = entries.iter().filter(|e| !gone.contains(&e.undo_id)).map(|e| e.bytes).sum();
        // The oldest go first. The entries of a run that is going on wait: a long run would otherwise lose its earliest copies to its own later ones.
        for e in entries.iter().rev() {
            if total <= max_bytes {
                break;
            }
            let live = e.agent_run_id.as_deref().is_some_and(|r| self.run_is_live(r));
            if gone.contains(&e.undo_id) || Some(e.undo_id.as_str()) == protect || in_flight(e, now) || live {
                continue;
            }
            if self.drop_slot(e, &mut purged) {
                gone.insert(e.undo_id.clone());
                total = total.saturating_sub(e.bytes);
            }
        }
        // But the disk is small: past twice the limit even those go, oldest first.
        let hard_limit = max_bytes.saturating_mul(2);
        for e in entries.iter().rev() {
            if total <= hard_limit {
                break;
            }
            if gone.contains(&e.undo_id) || Some(e.undo_id.as_str()) == protect || in_flight(e, now) {
                continue;
            }
            if self.drop_slot(e, &mut purged) {
                gone.insert(e.undo_id.clone());
                total = total.saturating_sub(e.bytes);
            }
        }
        // What a crash left with nothing worth keeping in it. A slot that holds content is not here: it is listed (see `read_slot`), and goes by age and size like any entry.
        for (path, slot) in &slots {
            if !matches!(slot, Slot::Junk) {
                continue;
            }
            let old = path.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d.as_millis() as u64 >= orphan_after_ms);
            if old && std::fs::remove_dir_all(path).is_ok() {
                purged.entries += 1;
            }
        }
        if purged.entries > 0 {
            self.compact_index();
        }
        purged
    }

    /// Remove every entry (the person asked to empty the undo store).
    pub fn purge_all(&self) -> Purged {
        self.enforce_limits(0, 0, None)
    }

    fn drop_slot(&self, e: &Entry, purged: &mut Purged) -> bool {
        if !valid_id(&e.undo_id) {
            return false;
        }
        let removed = std::fs::remove_dir_all(self.dir.join(&e.undo_id)).is_ok();
        if removed {
            purged.entries += 1;
            purged.bytes += e.bytes;
        }
        removed
    }

    /// Called after every stash and backup: looks at the limits, but only every ten minutes.
    fn limits_after(&self, made: &Entry) {
        let now = now_ms();
        let last = self.last_enforced_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= ENFORCE_EVERY_MS && self.last_enforced_ms.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
            self.enforce_limits(DEFAULT_MAX_AGE_DAYS, DEFAULT_MAX_BYTES, Some(&made.undo_id));
        }
    }

    /// How many entries and bytes the trash holds.
    pub fn usage(&self) -> Purged {
        let all = self.list(usize::MAX);
        Purged { entries: all.len(), bytes: all.iter().map(|e| e.bytes).sum() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn new_id(&self) -> String {
        let mut b = [0u8; 4];
        // A collision needs the same millisecond and the same 32 random bits; the loop checks anyway.
        let mut n = now_ms();
        loop {
            let _ = std::fs::metadata(&self.dir);
            let r = (n ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9)) as u32;
            b.copy_from_slice(&r.to_be_bytes());
            let id = format!("u_{:x}_{:02x}{:02x}{:02x}{:02x}", n, b[0], b[1], b[2], b[3]);
            if !self.dir.join(&id).exists() {
                return id;
            }
            n += 1;
        }
    }

    /// Logs damage to a slot once per process.
    fn warn_once(&self, key: &str, message: &str) {
        let mut seen = self.warned.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.len() > 500 {
            seen.clear();
        }
        if seen.insert(key.to_string()) {
            tracing::warn!("undo store: {message}");
        }
    }

    /// Writes the record of a slot through the record writer.
    fn write_record(&self, slot: &Path, entry: &Entry) -> Result<(), String> {
        let text = serde_json::to_string(entry).map_err(|e| e.to_string())?;
        (self.writer)(&slot.join("meta.json"), text.as_bytes()).map_err(|e| e.to_string())
    }

    /// The step the tool call on this thread has, when it is inside a run (see [`Entry::step`]).
    fn step_for_current_run(&self) -> Option<u32> {
        let run = current_run()?;
        if let Some(step) = STEP.with(|s| s.get()) {
            return Some(step);
        }
        let known = self.steps.lock().unwrap_or_else(PoisonError::into_inner).get(&run).copied();
        // After a restart the first record of a run that went on across it continues from what the records say.
        let base = match known {
            Some(b) => b,
            None => self.run_entries(&run).iter().filter_map(|e| e.step).max().unwrap_or(0),
        };
        let step = {
            let mut steps = self.steps.lock().unwrap_or_else(PoisonError::into_inner);
            if steps.len() > 2000 {
                steps.clear();
            }
            let cur = steps.entry(run).or_insert(base);
            *cur = (*cur).max(base) + 1;
            *cur
        };
        STEP.with(|s| s.set(Some(step)));
        Some(step)
    }

    fn new_entry(&self, id: String, path: &Path, reason: &str, kind: &str, bytes: u64, state: &str) -> Entry {
        let mut entry = self.new_entry_without_step(id, path, reason, kind, bytes, state);
        entry.step = self.step_for_current_run();
        entry
    }

    /// A record as `new_entry` makes it, but taking no step of the run: for the record of a checkpoint, which belongs to the whole run.
    fn new_entry_without_step(&self, id: String, path: &Path, reason: &str, kind: &str, bytes: u64, state: &str) -> Entry {
        Entry {
            undo_id: id,
            original_path: path.to_string_lossy().to_string(),
            reason: reason.to_string(),
            kind: kind.to_string(),
            bytes,
            at_unix_ms: now_ms(),
            group: None,
            agent_run_id: current_run(),
            to_path: None,
            state: Some(state.to_string()),
            unrecorded: false,
            after_sha256: None,
            step: None,
            effect: None,
        }
    }

    /// Move `path` into the trash and return the undo id. Falls back to copy-then-delete when the
    /// trash is on another drive.
    ///
    /// The order is the point: the slot is made, its record is written (`pending`), and only then is the file touched. If the record cannot be written the file is where it was
    /// and the answer is an error. After the move the record is rewritten as `committed`; if that last write fails the file is in the slot with a `pending` record, which is
    /// listed and restorable, so the stash still succeeds.
    pub fn stash(&self, path: &Path, reason: &str) -> Result<Entry, String> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let id = self.new_id();
        let slot = self.dir.join(&id);
        std::fs::create_dir_all(&slot).map_err(|e| format!("cannot create trash slot: {e}"))?;
        let content = slot.join("content");
        let bytes = if meta.is_dir() { dir_size(path) } else { meta.len() };
        let mut entry = self.new_entry(id, path, reason, if meta.is_dir() { "dir" } else { "file" }, bytes, "pending");
        if let Err(e) = self.write_record(&slot, &entry) {
            let _ = std::fs::remove_dir_all(&slot);
            return Err(format!("cannot write trash record: {e}; {} was not touched", path.display()));
        }
        if std::fs::rename(path, &content).is_err() {
            if let Err(e) = copy_recursive(path, &content) {
                // Nothing has been taken from the original yet: the half copy and the slot go, and the file stays.
                remove_any(&content);
                let _ = std::fs::remove_dir_all(&slot);
                return Err(format!("cannot move to trash: {e}; {} was not touched", path.display()));
            }
            let removed = if meta.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
            if let Err(e) = removed {
                if !meta.is_dir() {
                    // The file is still whole where it was: the copy is not needed.
                    remove_any(&content);
                    let _ = std::fs::remove_dir_all(&slot);
                    return Err(format!("cannot remove after copy: {e}; {} was not touched", path.display()));
                }
                // A folder may be part gone: the complete copy is kept and listed, marked as interrupted.
                entry.state = Some("partial".into());
                let _ = self.write_record(&slot, &entry);
                return Err(format!("cannot remove after copy: {e}; the whole folder is kept as undo entry {}", entry.undo_id));
            }
        }
        entry.state = Some("committed".into());
        if let Err(e) = self.write_record(&slot, &entry) {
            self.warn_once(&entry.undo_id, &format!("{} is in the undo store as {} but its record could not be marked complete ({e}); it is listed as interrupted", path.display(), entry.undo_id));
        }
        self.append_index(&entry);
        self.limits_after(&entry);
        Ok(entry)
    }

    /// Keep a copy of `path` (leaving it in place) so an overwrite or edit can be undone.
    ///
    /// The copy is hidden (`copying`) until it is complete, and then committed. A backup that cannot be completed or recorded is an error and is removed: the file has not been
    /// changed yet, so the caller does not go on to change it.
    pub fn backup(&self, path: &Path, reason: &str) -> Result<Entry, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let id = self.new_id();
        let slot = self.dir.join(&id);
        std::fs::create_dir_all(&slot).map_err(|e| format!("cannot create trash slot: {e}"))?;
        let mut entry = self.new_entry(id, path, reason, if meta.is_dir() { "dir" } else { "file" }, if meta.is_dir() { dir_size(path) } else { meta.len() }, "copying");
        if let Err(e) = self.write_record(&slot, &entry) {
            let _ = std::fs::remove_dir_all(&slot);
            return Err(format!("cannot write trash record: {e}; nothing was changed"));
        }
        if let Err(e) = copy_recursive(path, &slot.join("content")) {
            let _ = std::fs::remove_dir_all(&slot);
            return Err(format!("cannot back up: {e}"));
        }
        entry.state = Some("committed".into());
        if let Err(e) = self.write_record(&slot, &entry) {
            // An uncommitted backup would be hidden, and the caller is about to rely on it: refuse, with the file still as it was.
            let _ = std::fs::remove_dir_all(&slot);
            return Err(format!("cannot write trash record: {e}; nothing was changed"));
        }
        self.append_index(&entry);
        self.limits_after(&entry);
        Ok(entry)
    }

    /// Writes a record that has no copy in it: something an agent run made (a new file or folder) or moved, so the run can be undone. Only a
    /// change made inside a run (see [`with_run`]) is recorded; the answer is `None` for any other. Call it after the change is made.
    fn note(&self, reason: &str, path: &Path, to: Option<&Path>, after_sha256: Option<String>) -> Option<Entry> {
        current_run()?;
        let meta = std::fs::symlink_metadata(to.unwrap_or(path)).ok()?;
        let id = self.new_id();
        let slot = self.dir.join(&id);
        std::fs::create_dir_all(&slot).ok()?;
        let mut entry = self.new_entry(id, path, reason, if meta.is_dir() { "dir" } else { "file" }, if meta.is_dir() { 0 } else { meta.len() }, "committed");
        entry.to_path = to.map(|t| t.to_string_lossy().to_string());
        entry.after_sha256 = after_sha256;
        if let Err(e) = self.write_record(&slot, &entry) {
            let _ = std::fs::remove_dir_all(&slot);
            self.warn_once(&format!("note-{reason}"), &format!("a change by an agent run could not be recorded, so it cannot be undone with fs.undo_run: {e}"));
            return None;
        }
        self.append_index(&entry);
        Some(entry)
    }

    /// A file or folder an agent run created.
    pub fn note_created(&self, path: &Path) -> Option<Entry> {
        self.note("created", path, None, None)
    }

    /// Like [`Trash::note_created`] for a file whose content is known: `sha256` is the hash of what was written.
    pub fn note_created_with_hash(&self, path: &Path, sha256: &str) -> Option<Entry> {
        self.note("created", path, None, Some(sha256.to_string()))
    }

    /// Something an agent run moved from `from` to `to`.
    pub fn note_moved(&self, from: &Path, to: &Path) -> Option<Entry> {
        self.note("moved", from, Some(to), None)
    }

    /// Records the hash of the file as the change left it (see [`Entry::after_sha256`]). The record is rewritten whole; a failure is logged and the entry keeps working with the
    /// modification time.
    pub fn record_after(&self, entry: &Entry, sha256: &str) {
        if !valid_id(&entry.undo_id) {
            return;
        }
        let mut updated = entry.clone();
        updated.after_sha256 = Some(sha256.to_string());
        if let Err(e) = self.write_record(&self.dir.join(&entry.undo_id), &updated) {
            self.warn_once(&format!("after-{}", entry.undo_id), &format!("the content hash of {} could not be recorded ({e}); the modification time is used instead", entry.undo_id));
        }
    }

    /// Writes a record that holds no copy but says where the files of a command or a checkpoint are kept (reason `command` or `checkpoint`, card OA7-03). Only inside a run (see
    /// [`with_run`]); `None` for any other. A command is one of the run's file-changing calls and takes the next step; a checkpoint covers the whole run and takes none, so that undoing
    /// from a step on leaves it alone.
    pub fn note_effect(&self, reason: &str, root: &Path, effect: EffectRecord) -> Option<Entry> {
        current_run()?;
        let id = self.new_id();
        let slot = self.dir.join(&id);
        std::fs::create_dir_all(&slot).ok()?;
        let mut entry = self.new_entry_without_step(id, root, reason, "dir", 0, "committed");
        if reason == "command" {
            entry.step = self.step_for_current_run();
        }
        entry.effect = Some(effect);
        if let Err(e) = self.write_record(&slot, &entry) {
            let _ = std::fs::remove_dir_all(&slot);
            self.warn_once(&format!("note-{reason}"), &format!("what a {reason} changed could not be recorded, so it cannot be undone with fs.undo_run: {e}"));
            return None;
        }
        self.append_index(&entry);
        Some(entry)
    }

    /// Rewrites the effect of a record (a checkpoint gets its end tree when the run ends). A failure is logged and the record stays as it was.
    pub fn update_effect(&self, entry: &Entry, effect: EffectRecord) -> Option<Entry> {
        if !valid_id(&entry.undo_id) {
            return None;
        }
        let mut updated = entry.clone();
        updated.effect = Some(effect);
        match self.write_record(&self.dir.join(&entry.undo_id), &updated) {
            Ok(()) => Some(updated),
            Err(e) => {
                self.warn_once(&format!("effect-{}", entry.undo_id), &format!("the record {} could not be updated ({e})", entry.undo_id));
                None
            }
        }
    }

    /// The record of a command by the id its result named, in any run.
    pub fn find_command(&self, id: &str) -> Option<Entry> {
        self.list(usize::MAX).into_iter().find(|e| e.reason == "command" && e.effect.as_ref().is_some_and(|f| f.id == id))
    }

    /// What one agent run changed, in the order it was done. Records with no copy in them (`created`, `moved`) are among them.
    pub fn run_entries(&self, run_id: &str) -> Vec<Entry> {
        if run_id.is_empty() {
            return Vec::new();
        }
        let mut v: Vec<Entry> = match self.indexed_ids(|l| l.run.as_deref() == Some(run_id)) {
            Some(ids) => ids.iter().filter_map(|id| self.get(id)).filter(|e| e.agent_run_id.as_deref() == Some(run_id)).collect(),
            None => self.list(usize::MAX).into_iter().filter(|e| e.agent_run_id.as_deref() == Some(run_id)).collect(),
        };
        v.sort_by(|a, b| a.at_unix_ms.cmp(&b.at_unix_ms).then(a.undo_id.cmp(&b.undo_id)));
        v
    }

    /// Forget a record (and the copy it holds).
    pub fn drop_entry(&self, undo_id: &str) {
        if valid_id(undo_id) {
            let _ = std::fs::remove_dir_all(self.dir.join(undo_id));
        }
    }

    /// A new id for a group of copies made by one change to several files (`p_...`; an undo id is `u_...`).
    pub fn new_group_id(&self) -> String {
        self.new_id().replacen("u_", "p_", 1)
    }

    /// Marks an existing record as part of `group`, so one patch can be undone together.
    pub fn in_group(&self, mut entry: Entry, group: &str) -> Result<Entry, String> {
        if !valid_id(&entry.undo_id) {
            return Err("not an undo id".into());
        }
        entry.group = Some(group.to_string());
        self.write_record(&self.dir.join(&entry.undo_id), &entry).map_err(|e| format!("cannot write trash record: {e}"))?;
        self.append_index(&entry);
        Ok(entry)
    }

    /// Like [`Trash::backup`], and the copy is part of the group `group`.
    pub fn backup_grouped(&self, path: &Path, reason: &str, group: &str) -> Result<Entry, String> {
        let entry = self.backup(path, reason)?;
        self.in_group(entry, group)
    }

    /// The copies of one group, in the order they were made.
    pub fn group(&self, group: &str) -> Vec<Entry> {
        let mut v: Vec<Entry> = match self.indexed_ids(|l| l.group.as_deref() == Some(group)) {
            Some(ids) => ids.iter().filter_map(|id| self.get(id)).filter(|e| e.group.as_deref() == Some(group)).collect(),
            None => self.list(usize::MAX).into_iter().filter(|e| e.group.as_deref() == Some(group)).collect(),
        };
        v.sort_by(|a, b| a.at_unix_ms.cmp(&b.at_unix_ms).then(a.undo_id.cmp(&b.undo_id)));
        v
    }

    /// Every slot of the folder, read.
    fn scan(&self) -> Vec<(PathBuf, Slot)> {
        std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| {
                let path = e.path();
                let slot = self.read_slot(&path);
                (path, slot)
            })
            .collect()
    }

    /// What a slot is. The folder's name is the undo id, whatever the record says: a record copied in from elsewhere cannot make another slot answer to its id.
    fn read_slot(&self, slot: &Path) -> Slot {
        let Some(id) = slot.file_name().and_then(|n| n.to_str()).filter(|n| valid_id(n)).map(str::to_string) else { return Slot::Foreign };
        let content = slot.join("content");
        let has_content = std::fs::symlink_metadata(&content).is_ok();
        match std::fs::read_to_string(slot.join("meta.json")) {
            Ok(text) => match serde_json::from_str::<Entry>(&text) {
                Ok(mut e) => {
                    e.undo_id = id;
                    e.unrecorded = false;
                    return match e.state.as_deref() {
                        // A backup that was not finished: the file it was to protect has not been changed.
                        Some("copying") => Slot::Junk,
                        // A move that was recorded and never made (or made and lost): nothing to give back.
                        Some("pending") | Some("partial") if !has_content => Slot::Junk,
                        _ => Slot::Listed(e),
                    };
                }
                Err(e) => self.warn_once(&id, &format!("the record of {id} cannot be read ({e}); it is listed as unrecorded when it holds content")),
            },
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => self.warn_once(&id, &format!("the record of {id} cannot be opened ({e}); it is listed as unrecorded when it holds content")),
            Err(_) => {}
        }
        if !has_content {
            return Slot::Junk;
        }
        // The content is there and the record is not. The temporary file of an interrupted write (`.meta.json.aqua-tmp-<pid>-<n>`) may still say where it came from.
        let salvaged = std::fs::read_dir(slot)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".meta.json.aqua-tmp-"))
            .find_map(|e| std::fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<Entry>(&t).ok()));
        let is_dir = std::fs::symlink_metadata(&content).map(|m| m.is_dir()).unwrap_or(false);
        let at = salvaged
            .as_ref()
            .map(|s| s.at_unix_ms)
            .or_else(|| id.split('_').nth(1).and_then(|hex| u64::from_str_radix(hex, 16).ok()))
            .or_else(|| slot.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64))
            .unwrap_or(0);
        Slot::Listed(Entry {
            undo_id: id,
            original_path: salvaged.map(|s| s.original_path).unwrap_or_default(),
            reason: "unrecorded".into(),
            kind: if is_dir { "dir" } else { "file" }.into(),
            bytes: if is_dir { dir_size(&content) } else { std::fs::symlink_metadata(&content).map(|m| m.len()).unwrap_or(0) },
            at_unix_ms: at,
            group: None,
            agent_run_id: None,
            to_path: None,
            state: None,
            unrecorded: true,
            after_sha256: None,
            step: None,
            effect: None,
        })
    }

    /// What the trash holds, newest first: the entries with a record, and the slots that hold content without one (marked `unrecorded`).
    pub fn list(&self, limit: usize) -> Vec<Entry> {
        let mut v: Vec<Entry> = self.scan().into_iter().filter_map(|(_, s)| if let Slot::Listed(e) = s { Some(e) } else { None }).collect();
        v.sort_by(|a, b| b.at_unix_ms.cmp(&a.at_unix_ms));
        v.truncate(limit);
        v
    }

    pub fn get(&self, undo_id: &str) -> Option<Entry> {
        if !valid_id(undo_id) {
            return None;
        }
        match self.read_slot(&self.dir.join(undo_id)) {
            Slot::Listed(e) => Some(e),
            _ => None,
        }
    }

    /// Put an entry back at `dest` and remove its trash slot.
    pub fn restore_to(&self, entry: &Entry, dest: &Path) -> Result<(), String> {
        if !valid_id(&entry.undo_id) {
            return Err("not an undo id".into());
        }
        let content = self.dir.join(&entry.undo_id).join("content");
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if std::fs::rename(&content, dest).is_err() {
            copy_recursive(&content, dest).map_err(|e| format!("cannot restore: {e}"))?;
        }
        let _ = std::fs::remove_dir_all(self.dir.join(&entry.undo_id));
        Ok(())
    }

    // ---- the index ------------------------------------------------------------------------------------------------------------------------------

    /// Adds one line for `entry` to the index: a single write of one whole line to a file opened for appending, so two threads cannot interleave their lines. The index is a help,
    /// not a record: a failure is logged once, and the lookups scan the slots when the index is missing a slot.
    fn append_index(&self, entry: &Entry) {
        let line = IndexLine { undo_id: entry.undo_id.clone(), run: entry.agent_run_id.clone(), group: entry.group.clone(), at: entry.at_unix_ms };
        let Ok(mut text) = serde_json::to_string(&line) else { return };
        text.push('\n');
        let result = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join(INDEX_FILE)).and_then(|mut f| f.write_all(text.as_bytes()));
        if let Err(e) = result {
            self.warn_once("index", &format!("the index could not be written ({e}); runs and groups are found by reading every record"));
        }
    }

    /// The ids of the slots the index says `wants`, or `None` when the index cannot be trusted to be complete: it is missing, or a slot of the folder is not in it. The caller
    /// then reads every record. The records stay the authority: the caller checks each one it finds.
    fn indexed_ids(&self, wants: impl Fn(&IndexLine) -> bool) -> Option<Vec<String>> {
        let text = std::fs::read_to_string(self.dir.join(INDEX_FILE)).ok()?;
        let mut known: HashSet<String> = HashSet::new();
        let mut wanted: Vec<String> = Vec::new();
        for line in text.lines() {
            let Ok(l) = serde_json::from_str::<IndexLine>(line) else { continue };
            if wants(&l) && !wanted.contains(&l.undo_id) {
                wanted.push(l.undo_id.clone());
            }
            known.insert(l.undo_id);
        }
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if e.file_name().to_str().is_some_and(|n| valid_id(n) && !known.contains(n)) {
                return None;
            }
        }
        Some(wanted)
    }

    /// Rewrites the index without the lines of slots that are gone, after a clean-up removed some.
    fn compact_index(&self) {
        let path = self.dir.join(INDEX_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let kept: String = text
            .lines()
            .filter(|line| serde_json::from_str::<IndexLine>(line).is_ok_and(|l| valid_id(&l.undo_id) && self.dir.join(&l.undo_id).is_dir()))
            .map(|line| format!("{line}\n"))
            .collect();
        if let Err(e) = write_record_durably(&path, kept.as_bytes()) {
            self.warn_once("index-compact", &format!("the index could not be rewritten ({e})"));
        }
    }
}

/// A stash that began a moment ago may still be going on in another thread.
fn in_flight(e: &Entry, now: u64) -> bool {
    e.state.as_deref() == Some("pending") && now.saturating_sub(e.at_unix_ms) < IN_FLIGHT_MS
}

/// Removes a file or a folder that this module made (the half copy of a stash that failed); nothing happens when it is not there.
fn remove_any(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

pub fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "refusing to copy a symbolic link or junction"));
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src, dst).map(|_| ())
    }
}

pub fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => total += dir_size(&e.path()),
                Ok(m) => total += m.len(),
                Err(_) => {}
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_trash_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stash_and_restore_a_file() {
        let d = temp("file");
        let t = Trash::new(d.join("trash")).unwrap();
        let f = d.join("a.txt");
        std::fs::write(&f, "hello").unwrap();
        let e = t.stash(&f, "deleted").unwrap();
        assert!(!f.exists());
        assert_eq!(t.list(10)[0].undo_id, e.undo_id);
        t.restore_to(&t.get(&e.undo_id).unwrap(), &f).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "hello");
        assert!(t.list(10).is_empty());
    }

    #[test]
    fn stash_a_folder_and_backup_leaves_the_original() {
        let d = temp("dir");
        let t = Trash::new(d.join("trash")).unwrap();
        std::fs::create_dir_all(d.join("proj/sub")).unwrap();
        std::fs::write(d.join("proj/sub/x.txt"), "1").unwrap();
        let e = t.stash(&d.join("proj"), "deleted").unwrap();
        assert_eq!((e.kind.as_str(), e.bytes), ("dir", 1));
        assert!(!d.join("proj").exists());
        t.restore_to(&e, &d.join("proj")).unwrap();
        assert!(d.join("proj/sub/x.txt").exists());
        let b = t.backup(&d.join("proj/sub/x.txt"), "overwritten").unwrap();
        assert!(d.join("proj/sub/x.txt").exists());
        assert_eq!(std::fs::read_to_string(d.join("trash").join(&b.undo_id).join("content")).unwrap(), "1");
    }

    #[test]
    fn undo_ids_cannot_escape_the_trash_folder() {
        let d = temp("ids");
        let t = Trash::new(d.join("trash")).unwrap();
        assert!(t.get("../x").is_none() && t.get("a/b").is_none() && t.get("..").is_none());
    }

    /// Stash a file of `size` bytes and make the entry look as if it was made `days_ago` days ago.
    fn stash_aged(t: &Trash, d: &Path, name: &str, size: usize, days_ago: u64) -> Entry {
        let f = d.join(name);
        std::fs::write(&f, vec![b'x'; size]).unwrap();
        let mut e = t.stash(&f, "deleted").unwrap();
        e.at_unix_ms = now_ms() - days_ago * 86_400_000;
        std::fs::write(t.dir().join(&e.undo_id).join("meta.json"), serde_json::to_string(&e).unwrap()).unwrap();
        e
    }

    #[test]
    fn what_is_too_old_is_removed_and_what_is_recent_stays() {
        let d = temp("age");
        let t = Trash::new(d.join("trash")).unwrap();
        t.last_enforced_ms.store(u64::MAX, Ordering::Relaxed); // no automatic pass while the entries are being set up
        let old = stash_aged(&t, &d, "old.txt", 10, 45);
        let recent = stash_aged(&t, &d, "recent.txt", 10, 2);
        let gone = t.enforce_limits(30, u64::MAX, None);
        assert_eq!((gone.entries, gone.bytes), (1, 10));
        assert!(t.get(&old.undo_id).is_none() && !t.dir().join(&old.undo_id).exists(), "the whole slot is gone");
        assert!(t.get(&recent.undo_id).is_some());
    }

    #[test]
    fn when_the_trash_is_too_big_the_oldest_go_first_and_the_newest_entry_is_never_the_one_removed() {
        let d = temp("size");
        let t = Trash::new(d.join("trash")).unwrap();
        t.last_enforced_ms.store(u64::MAX, Ordering::Relaxed);
        let a = stash_aged(&t, &d, "a.txt", 100, 3);
        let b = stash_aged(&t, &d, "b.txt", 100, 2);
        let c = stash_aged(&t, &d, "c.txt", 100, 1);
        assert_eq!(t.usage(), Purged { entries: 3, bytes: 300 });
        let gone = t.enforce_limits(30, 150, None);
        assert_eq!((gone.entries, gone.bytes), (2, 200));
        assert!(t.get(&a.undo_id).is_none() && t.get(&b.undo_id).is_none() && t.get(&c.undo_id).is_some(), "the two oldest went");
        // One item bigger than the whole limit: the entry just made is kept, because undoing it is what it is for.
        let big = stash_aged(&t, &d, "big.bin", 500, 0);
        let gone = t.enforce_limits(30, 150, Some(&big.undo_id));
        assert!(t.get(&big.undo_id).is_some() && t.get(&c.undo_id).is_none(), "{gone:?}");
    }

    #[test]
    fn a_stash_looks_at_the_limits_only_every_ten_minutes_and_the_person_can_empty_the_trash() {
        let d = temp("throttle");
        let t = Trash::new(d.join("trash")).unwrap();
        // The first stash looks (nothing is old yet); a very old entry added after it is not noticed by the next stash...
        let first = stash_aged(&t, &d, "first.txt", 10, 0);
        assert!(t.last_enforced_ms.load(Ordering::Relaxed) > 0, "the first stash did look");
        let ancient = stash_aged(&t, &d, "ancient.txt", 10, 90);
        let _second = stash_aged(&t, &d, "second.txt", 10, 0);
        assert!(t.get(&ancient.undo_id).is_some(), "not looked at again so soon");
        // ...until ten minutes have passed.
        t.last_enforced_ms.store(now_ms() - ENFORCE_EVERY_MS - 1, Ordering::Relaxed);
        let _third = stash_aged(&t, &d, "third.txt", 10, 0);
        assert!(t.get(&ancient.undo_id).is_none() && t.get(&first.undo_id).is_some(), "it was cleared by the next look, the recent ones were not");
        // Emptying it removes everything.
        let all = t.purge_all();
        assert!(all.entries >= 3 && t.usage() == Purged { entries: 0, bytes: 0 }, "{all:?}");
    }

    #[test]
    fn a_slot_a_crash_left_with_nothing_in_it_is_cleared_once_it_is_old_and_not_before() {
        let d = temp("orphan");
        let t = Trash::new(d.join("trash")).unwrap();
        // A slot that was made and never filled: no content, no record (or only the half of a record that was being written).
        let slot = t.dir().join("u_crashed_00000000");
        std::fs::create_dir_all(&slot).unwrap();
        std::fs::write(slot.join(".meta.json.aqua-tmp-1-1"), "{\"undo_id\":").unwrap();
        assert_eq!(t.enforce_limits(30, u64::MAX, None).entries, 0, "a fresh slot may be in the middle of a stash");
        assert!(slot.exists());
        // Once it is as old as the rule says (here: any age), it goes. A slot that has a record is not an orphan and is left alone.
        let kept = stash_aged(&t, &d, "kept.txt", 5, 0);
        assert_eq!(t.clear(30, u64::MAX, None, 0).entries, 1);
        assert!(!slot.exists() && t.get(&kept.undo_id).is_some());
    }

    #[test]
    fn ids_are_unique() {
        let d = temp("uniq");
        let t = Trash::new(d.join("trash")).unwrap();
        let mut ids = std::collections::HashSet::new();
        for i in 0..20 {
            let f = d.join(format!("f{i}.txt"));
            std::fs::write(&f, "x").unwrap();
            assert!(ids.insert(t.stash(&f, "deleted").unwrap().undo_id));
        }
    }

    // ---- the record is written first (QA-14) ----------------------------------------------------------------------------------------------------

    /// A trash whose record writer fails from the `n`th write on (counting from 1) and writes durably before that.
    fn failing_from(dir: PathBuf, n: usize) -> Trash {
        let calls = std::sync::atomic::AtomicUsize::new(0);
        Trash::new(dir)
            .unwrap()
            .with_record_writer(Box::new(move |path, bytes| {
                if calls.fetch_add(1, Ordering::SeqCst) + 1 >= n {
                    return Err(std::io::Error::other("the disk is full"));
                }
                write_record_durably(path, bytes)
            }))
    }

    #[test]
    fn when_the_record_cannot_be_written_the_file_is_not_touched_and_no_slot_is_left() {
        let d = temp("record_fails");
        let t = failing_from(d.join("trash"), 1);
        let f = d.join("precious.txt");
        std::fs::write(&f, "the only copy").unwrap();
        let said = t.stash(&f, "deleted").expect_err("no record, no move");
        assert!(said.contains("cannot write trash record") && said.contains("was not touched"), "{said}");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "the only copy", "the file is where it was");
        let slots: Vec<_> = std::fs::read_dir(t.dir()).unwrap().flatten().collect();
        assert!(slots.is_empty(), "the slot that was made is removed: {slots:?}");
        // The same for a backup, and for a folder.
        assert!(t.backup(&f, "edited").unwrap_err().contains("nothing was changed"));
        std::fs::create_dir_all(d.join("folder/inner")).unwrap();
        std::fs::write(d.join("folder/inner/x.txt"), "x").unwrap();
        assert!(t.stash(&d.join("folder"), "deleted").is_err() && d.join("folder/inner/x.txt").exists());
    }

    #[test]
    fn a_kill_between_the_move_and_the_commit_leaves_a_slot_that_the_sweep_keeps_and_the_list_shows() {
        let d = temp("killed_after_move");
        // The first write (the pending record) succeeds, the second (the commit) fails: what a kill at that moment leaves on the disk.
        let t = failing_from(d.join("trash"), 2);
        let f = d.join("report.docx");
        std::fs::write(&f, "weeks of work").unwrap();
        let e = t.stash(&f, "deleted").expect("the file is in the trash, so the stash answers");
        assert!(!f.exists());
        let on_disk: Entry = serde_json::from_str(&std::fs::read_to_string(t.dir().join(&e.undo_id).join("meta.json")).unwrap()).unwrap();
        assert_eq!(on_disk.state.as_deref(), Some("pending"), "the record on the disk is the one written before the move");
        assert_eq!(on_disk.original_path, f.to_string_lossy());
        // Days later: the sweep of crash leftovers, at any age, does not touch it.
        let swept = t.clear(30, u64::MAX, None, 0);
        assert_eq!(swept.entries, 0, "{swept:?}");
        let listed = t.list(10);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].is_interrupted() && !listed[0].unrecorded && listed[0].original_path == f.to_string_lossy());
        // And it can be put back by its id.
        t.restore_to(&t.get(&e.undo_id).unwrap(), &f).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "weeks of work");
    }

    #[test]
    fn a_slot_with_content_and_no_record_is_listed_as_unrecorded_never_swept_and_can_be_restored() {
        let d = temp("unrecorded");
        let t = Trash::new(d.join("trash")).unwrap();
        let f = d.join("a.txt");
        std::fs::write(&f, "kept").unwrap();
        let e = t.stash(&f, "deleted").unwrap();
        // The record is truncated (a write that was cut off by a power loss in an older version).
        std::fs::write(t.dir().join(&e.undo_id).join("meta.json"), "{\"undo_id\":\"u_").unwrap();
        let listed = t.list(10);
        assert_eq!(listed.len(), 1, "an unreadable record does not hide the content");
        assert!(listed[0].unrecorded && listed[0].undo_id == e.undo_id && listed[0].original_path.is_empty() && listed[0].bytes == 4, "{:?}", listed[0]);
        // A record that is not there at all: the same.
        let g = d.join("b.txt");
        std::fs::write(&g, "also kept").unwrap();
        let e2 = t.stash(&g, "deleted").unwrap();
        std::fs::remove_file(t.dir().join(&e2.undo_id).join("meta.json")).unwrap();
        assert_eq!(t.list(10).len(), 2);
        // The sweep of leftovers, at any age, removes neither.
        assert_eq!(t.clear(30, u64::MAX, None, 0).entries, 0);
        assert!(t.dir().join(&e.undo_id).join("content").exists() && t.dir().join(&e2.undo_id).join("content").exists());
        // They age out like any entry, by the slot's own time, and can be restored to a place the caller names.
        let got = t.get(&e.undo_id).unwrap();
        assert!(got.unrecorded);
        t.restore_to(&got, &d.join("elsewhere").join("a.txt")).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("elsewhere").join("a.txt")).unwrap(), "kept");
    }

    #[test]
    fn the_temporary_file_of_an_interrupted_record_write_tells_where_the_content_came_from() {
        let d = temp("salvage");
        let t = Trash::new(d.join("trash")).unwrap();
        let f = d.join("a.txt");
        std::fs::write(&f, "kept").unwrap();
        let e = t.stash(&f, "deleted").unwrap();
        let slot = t.dir().join(&e.undo_id);
        std::fs::rename(slot.join("meta.json"), slot.join(".meta.json.aqua-tmp-1-1")).unwrap();
        let listed = t.list(10);
        assert!(listed[0].unrecorded && listed[0].original_path == f.to_string_lossy(), "{:?}", listed[0]);
    }

    #[test]
    fn a_backup_that_was_not_finished_is_hidden_and_goes_with_the_old_leftovers_while_a_finished_one_is_listed() {
        let d = temp("backup_states");
        let t = Trash::new(d.join("trash")).unwrap();
        let f = d.join("a.txt");
        std::fs::write(&f, "v1").unwrap();
        let done = t.backup(&f, "edited").unwrap();
        // A backup killed in the middle of its copy: its record says `copying`, and the content may be half of the file.
        let slot = t.dir().join("u_killed_backup");
        std::fs::create_dir_all(slot.join("content")).unwrap();
        let unfinished = Entry { undo_id: "u_killed_backup".into(), state: Some("copying".into()), ..done.clone() };
        std::fs::write(slot.join("meta.json"), serde_json::to_string(&unfinished).unwrap()).unwrap();
        std::fs::write(slot.join("content").join("half"), "v").unwrap();
        let ids: Vec<String> = t.list(10).into_iter().map(|e| e.undo_id).collect();
        assert_eq!(ids, vec![done.undo_id.clone()], "an incomplete slot is not listed");
        assert!(t.get("u_killed_backup").is_none());
        assert_eq!(t.enforce_limits(30, u64::MAX, None).entries, 0, "and it is not removed while it is young");
        assert_eq!(t.clear(30, u64::MAX, None, 0).entries, 1, "but it is when it is old: the file it protected was never changed");
        assert!(!slot.exists() && t.get(&done.undo_id).is_some());
    }

    #[test]
    fn a_backup_whose_commit_cannot_be_written_is_refused_and_removed_so_that_the_caller_does_not_go_on() {
        let d = temp("backup_commit");
        let t = failing_from(d.join("trash"), 2);
        let f = d.join("a.txt");
        std::fs::write(&f, "v1").unwrap();
        let said = t.backup(&f, "edited").expect_err("an unrecorded backup is no backup");
        assert!(said.contains("nothing was changed"), "{said}");
        assert!(std::fs::read_dir(t.dir()).unwrap().flatten().next().is_none());
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
    }

    #[test]
    fn a_record_is_replaced_whole_or_not_at_all() {
        let d = temp("durable");
        let p = d.join("meta.json");
        write_record_durably(&p, b"first").unwrap();
        write_record_durably(&p, b"second, longer").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"second, longer");
        assert!(std::fs::read_dir(&d).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(".aqua-tmp-")), "no temporary file is left");
        // A path that cannot be renamed onto (a folder is there) fails and leaves nothing behind.
        let blocked = d.join("blocked");
        std::fs::create_dir_all(blocked.join("inside")).unwrap();
        assert!(write_record_durably(&blocked, b"x").is_err());
        assert!(std::fs::read_dir(&d).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(".aqua-tmp-")), "and no temporary file");
    }

    #[test]
    fn a_record_that_names_another_slot_cannot_make_one_slot_answer_for_another() {
        let d = temp("foreign_id");
        let t = Trash::new(d.join("trash")).unwrap();
        let f = d.join("a.txt");
        std::fs::write(&f, "a").unwrap();
        let a = t.stash(&f, "deleted").unwrap();
        let g = d.join("b.txt");
        std::fs::write(&g, "b").unwrap();
        let b = t.stash(&g, "deleted").unwrap();
        // The record of slot b is overwritten with the record of slot a.
        std::fs::write(t.dir().join(&b.undo_id).join("meta.json"), serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(t.get(&b.undo_id).unwrap().undo_id, b.undo_id, "the folder's name is the id");
        assert_eq!(t.list(10).iter().filter(|e| e.undo_id == a.undo_id).count(), 1);
        // A folder that is not an undo id is not ours: not listed, not removed.
        std::fs::create_dir_all(t.dir().join("not an id").join("content")).unwrap();
        assert_eq!(t.list(10).len(), 2);
        assert_eq!(t.clear(30, u64::MAX, None, 0).entries, 0);
        assert!(t.dir().join("not an id").exists());
    }

    // ---- the index, the steps, the hash and the runs that are going on (ED-13) ---------------------------------------------------------------------

    #[test]
    fn index_and_scan_agree_and_a_missing_or_short_index_falls_back_to_the_scan() {
        let d = temp("index");
        let t = Trash::new(d.join("trash")).unwrap();
        let mut made = Vec::new();
        for (i, run) in ["run_a", "run_b", "run_a", ""].iter().enumerate() {
            let f = d.join(format!("f{i}.txt"));
            std::fs::write(&f, format!("{i}")).unwrap();
            made.push(with_run(run, || t.backup(&f, "edited").unwrap()));
        }
        let group = t.new_group_id();
        let h = d.join("g.txt");
        std::fs::write(&h, "g").unwrap();
        let grouped = t.backup_grouped(&h, "patched", &group).unwrap();
        let scan = |run: &str| -> Vec<String> { t.list(usize::MAX).into_iter().filter(|e| e.agent_run_id.as_deref() == Some(run)).map(|e| e.undo_id).collect::<std::collections::BTreeSet<_>>().into_iter().collect() };
        let by_index = |run: &str| -> Vec<String> { t.run_entries(run).into_iter().map(|e| e.undo_id).collect::<std::collections::BTreeSet<_>>().into_iter().collect() };
        assert!(t.indexed_ids(|l| l.run.as_deref() == Some("run_a")).is_some(), "the index is complete, so it is used");
        assert_eq!(by_index("run_a"), scan("run_a"));
        assert_eq!(by_index("run_a").len(), 2);
        assert_eq!(by_index("run_b").len(), 1);
        assert_eq!(t.group(&group).iter().map(|e| e.undo_id.clone()).collect::<Vec<_>>(), vec![grouped.undo_id.clone()]);
        // The index is deleted: the answer is the same, from the scan.
        std::fs::remove_file(t.dir().join(INDEX_FILE)).unwrap();
        assert!(t.indexed_ids(|_| true).is_none());
        assert_eq!(by_index("run_a"), scan("run_a"));
        assert_eq!(t.group(&group).len(), 1);
        // The index is short (a slot is not in it) and has a line that is not JSON: the scan again.
        std::fs::write(t.dir().join(INDEX_FILE), format!("not json\n{{\"undo_id\":\"{}\",\"run\":\"run_b\",\"at\":1}}\n", made[1].undo_id)).unwrap();
        assert!(t.indexed_ids(|_| true).is_none());
        assert_eq!(by_index("run_a").len(), 2);
        // An index that names a run for a slot whose record says another is not believed.
        let all: String = t.list(usize::MAX).iter().map(|e| format!("{{\"undo_id\":\"{}\",\"run\":\"run_b\",\"at\":1}}\n", e.undo_id)).collect();
        std::fs::write(t.dir().join(INDEX_FILE), all).unwrap();
        assert_eq!(by_index("run_b").len(), 1, "the record is the authority");
    }

    #[test]
    fn a_clean_up_that_removes_slots_also_removes_their_lines_from_the_index() {
        let d = temp("index_compact");
        let t = Trash::new(d.join("trash")).unwrap();
        t.last_enforced_ms.store(u64::MAX, Ordering::Relaxed);
        let old = stash_aged(&t, &d, "old.txt", 10, 45);
        let recent = stash_aged(&t, &d, "recent.txt", 10, 1);
        t.enforce_limits(30, u64::MAX, None);
        let index = std::fs::read_to_string(t.dir().join(INDEX_FILE)).unwrap();
        assert!(!index.contains(&old.undo_id) && index.contains(&recent.undo_id), "{index}");
    }

    #[test]
    fn steps_count_the_file_changing_calls_of_a_run_and_all_the_records_of_one_call_share_one() {
        let d = temp("steps");
        let t = Trash::new(d.join("trash")).unwrap();
        let file = |name: &str| {
            let f = d.join(name);
            std::fs::write(&f, "x").unwrap();
            f
        };
        let (a, b, c, other) = (file("a"), file("b"), file("c"), file("o"));
        let first = with_run("run_s", || t.backup(&a, "edited").unwrap());
        let (second_1, second_2) = with_run("run_s", || (t.backup(&b, "edited").unwrap(), t.note_created(&b).unwrap()));
        let third = with_run("run_s", || t.backup(&c, "edited").unwrap());
        let elsewhere = with_run("run_t", || t.backup(&other, "edited").unwrap());
        let outside = t.backup(&other, "edited").unwrap();
        assert_eq!((first.step, second_1.step, second_2.step, third.step), (Some(1), Some(2), Some(2), Some(3)));
        assert_eq!(elsewhere.step, Some(1), "each run counts for itself");
        assert_eq!(outside.step, None, "a change outside a run has no step");
        // After a restart (a new Trash over the same folder) the count goes on from the records.
        let again = Trash::new(d.join("trash")).unwrap();
        let next = with_run("run_s", || again.backup(&a, "edited").unwrap());
        assert_eq!(next.step, Some(4));
    }

    #[test]
    fn the_hash_of_what_a_change_left_is_kept_with_the_record() {
        let d = temp("after_hash");
        let t = Trash::new(d.join("trash")).unwrap();
        let f = d.join("a.txt");
        std::fs::write(&f, "v1").unwrap();
        let e = with_run("run_h", || t.backup(&f, "edited").unwrap());
        assert_eq!(e.after_sha256, None);
        t.record_after(&e, "abc123");
        let got = t.get(&e.undo_id).unwrap();
        assert_eq!(got.after_sha256.as_deref(), Some("abc123"));
        assert_eq!((got.agent_run_id.as_deref(), got.step), (Some("run_h"), Some(1)), "the rest of the record is as it was");
        let made = with_run("run_h", || t.note_created_with_hash(&f, "def456").unwrap());
        assert_eq!(t.get(&made.undo_id).unwrap().after_sha256.as_deref(), Some("def456"));
        // A record from before these fields still reads.
        let old = r#"{"undo_id":"u_1_aa","original_path":"C:/x","reason":"deleted","kind":"file","bytes":3,"at_unix_ms":5}"#;
        let parsed: Entry = serde_json::from_str(old).unwrap();
        assert_eq!((parsed.state, parsed.unrecorded, parsed.after_sha256, parsed.step), (None, false, None, None));
        // `unrecorded` is never read from a record.
        let forged: Entry = serde_json::from_str(r#"{"undo_id":"u_1_aa","original_path":"","reason":"x","kind":"file","bytes":0,"at_unix_ms":0,"unrecorded":true}"#).unwrap();
        assert!(!forged.unrecorded);
    }

    #[test]
    fn live_run_entries_survive_a_purge_until_the_trash_is_over_twice_its_limit() {
        let d = temp("live_runs");
        let t = Trash::new(d.join("trash")).unwrap();
        t.last_enforced_ms.store(u64::MAX, Ordering::Relaxed);
        assert!(t.set_live_run_check(|run| run == "run_live"));
        assert!(!t.set_live_run_check(|_| false), "set once");
        let stash_for = |run: &str, name: &str, days_ago: u64| {
            let f = d.join(name);
            std::fs::write(&f, vec![b'x'; 100]).unwrap();
            let mut e = with_run(run, || t.stash(&f, "deleted").unwrap());
            e.at_unix_ms = now_ms() - days_ago * 86_400_000;
            std::fs::write(t.dir().join(&e.undo_id).join("meta.json"), serde_json::to_string(&e).unwrap()).unwrap();
            e
        };
        let earliest = stash_for("run_live", "earliest.txt", 5); // the oldest of all, and its run is going on
        let finished = stash_for("run_done", "finished.txt", 4);
        let later = stash_for("run_live", "later.txt", 1);
        // 300 bytes held, limit 250: the finished run's copy goes, not the live run's earliest one, although it is older.
        t.enforce_limits(30, 250, None);
        assert!(t.get(&earliest.undo_id).is_some() && t.get(&finished.undo_id).is_none() && t.get(&later.undo_id).is_some());
        // Limit 100: the live run's entries alone are over it, but under twice it: they stay.
        t.enforce_limits(30, 100, None);
        assert!(t.get(&earliest.undo_id).is_some() && t.get(&later.undo_id).is_some());
        // Limit 90: 200 bytes is over twice 90: the oldest of them goes.
        t.enforce_limits(30, 90, None);
        assert!(t.get(&earliest.undo_id).is_none() && t.get(&later.undo_id).is_some());
    }

    #[test]
    fn a_stash_that_is_going_on_is_not_taken_by_the_size_limit_and_one_that_is_old_is_listed_as_interrupted() {
        let d = temp("in_flight");
        let t = failing_from(d.join("trash"), 2);
        let f = d.join("a.txt");
        std::fs::write(&f, vec![b'x'; 100]).unwrap();
        let e = t.stash(&f, "deleted").unwrap(); // pending on the disk, as if still being made
        assert_eq!(t.enforce_limits(30, 10, None).entries, 0, "young and pending: left alone by the size limit");
        // The same record, made long ago, is just a big old entry.
        let mut old: Entry = serde_json::from_str(&std::fs::read_to_string(t.dir().join(&e.undo_id).join("meta.json")).unwrap()).unwrap();
        old.at_unix_ms = now_ms() - 3_600_000;
        std::fs::write(t.dir().join(&e.undo_id).join("meta.json"), serde_json::to_string(&old).unwrap()).unwrap();
        assert!(t.list(10)[0].is_interrupted());
        assert_eq!(t.enforce_limits(30, 10, None).entries, 1);
    }

    /// Card OA7-03: the record of a command takes the next step of its run, the record of a checkpoint takes none (it covers the whole run, and an undo from a step on leaves it alone),
    /// and neither is written outside a run.
    #[test]
    fn a_command_takes_the_next_step_of_its_run_and_a_checkpoint_takes_none() {
        let d = temp("effects");
        let t = Trash::new(d.join("trash")).unwrap();
        let f = d.join("a.txt");
        std::fs::write(&f, "x").unwrap();
        let id = "a".repeat(40);
        let command = |n: u32| EffectRecord { id: format!("jr_{n}_{n}"), root: d.to_string_lossy().to_string(), reference: Some(format!("refs/aqua/journal/run_e/jr_{n}_{n}")), command: Some("cargo fmt".into()), changed: 2, ..Default::default() };
        let first = with_run("run_e", || t.backup(&f, "edited").unwrap());
        let checkpoint = with_run("run_e", || t.note_effect("checkpoint", &d, EffectRecord { root: d.to_string_lossy().to_string(), start: Some(id.clone()), ..Default::default() }).unwrap());
        let second = with_run("run_e", || t.note_effect("command", &d, command(1)).unwrap());
        let third = with_run("run_e", || t.backup(&f, "edited").unwrap());
        assert_eq!((first.step, checkpoint.step, second.step, third.step), (Some(1), None, Some(2), Some(3)));
        assert!(t.note_effect("command", &d, command(2)).is_none() && t.note_effect("checkpoint", &d, EffectRecord::default()).is_none(), "outside a run nothing is recorded");
        // They are the run's records, in the order they were made, and the record says where the files are.
        let kept = t.run_entries("run_e");
        assert_eq!(kept.iter().map(|e| e.reason.as_str()).collect::<Vec<_>>(), vec!["edited", "checkpoint", "command", "edited"]);
        assert_eq!(second.effect.as_ref().unwrap().reference.as_deref(), Some("refs/aqua/journal/run_e/jr_1_1"));
        // A command is found by the id its result named; a checkpoint's end tree is added to its record whole.
        assert_eq!(t.find_command("jr_1_1").map(|e| e.undo_id), Some(second.undo_id.clone()));
        assert!(t.find_command("jr_9_9").is_none());
        let end = "b".repeat(40);
        let updated = t.update_effect(&checkpoint, EffectRecord { root: d.to_string_lossy().to_string(), start: Some(id.clone()), end: Some(end.clone()), changed: 3, ..Default::default() }).unwrap();
        assert_eq!(t.get(&checkpoint.undo_id).unwrap().effect, updated.effect);
        assert_eq!(t.get(&checkpoint.undo_id).unwrap().effect.unwrap().end.as_deref(), Some(end.as_str()));
        // A record from before this field (no `effect`) reads as it always did, and one with it survives a restart of the store.
        let old = r#"{"undo_id":"u_1_00000000","original_path":"C:/x","reason":"edited","kind":"file","bytes":1,"at_unix_ms":5}"#;
        assert!(serde_json::from_str::<Entry>(old).unwrap().effect.is_none());
        let again = Trash::new(d.join("trash")).unwrap();
        assert_eq!(again.run_entries("run_e").len(), 4);
        // Taking a record away takes the slot and its index line away.
        again.drop_entry(&second.undo_id);
        assert!(again.find_command("jr_1_1").is_none() && again.run_entries("run_e").len() == 3);
    }
}

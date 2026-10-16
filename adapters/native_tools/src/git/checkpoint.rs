//! Checkpoints of a trusted project (card OA7-03: ED-11 and CX-11).
//!
//! `fs.undo_run` puts back what the file tools changed. A command can change files no tool saw (`cargo fmt`, a code generator, `git checkout`), and the person wants to see "what did
//! this run change" whatever changed it. With checkpoints on (`AQUA_SNAPSHOT=on`) the folder of a run's project, when it is a TRUSTED folder (the workspace, or the project folder under
//! full access: never a folder the person allowed for one request), is looked at before the first command of the run that may change files and again when the run ends. Each look is a
//! tree (`capture.rs`) kept in a store of Aqua's own: `data/snapshots/<hash of the folder>`, a bare git folder driven with `--git-dir` and `--work-tree`, so that the project's own `.git`
//! (and its configuration, which a cloned repository could use to run a program) is never read, and a folder that is not a repository has checkpoints as well.
//!
//! What it never does: look at a folder that is not trusted, follow a link out of the folder, keep a file over 2 MB, a secret-looking name, a credential folder, `.git`, `target` or
//! `node_modules`, or any file of Aqua's own; take more than 5 seconds or more than 50,000 files; make a checkpoint with less than 1 GiB free; or grow past 512 MiB (the stores of the
//! projects least used go first). The checkpoint is recorded with the run in the undo store (reason `checkpoint`), so `fs.undo_run` (and the app's "Undo this run") puts it back last,
//! after the file tools' own changes and the commands', leaving every file somebody changed since and every file those already put back.

use super::capture::{allow_rule, free_bytes, Captured, CaptureOpts, Change, FilePatch, Git, Place, Refused, Stop, CAPTURE_SECONDS, FILE_CAP_BYTES, MAX_FILES};
use crate::fs::guard::{canonical, Op, PathGuard};
use crate::fs::provider::{handled_key, EffectReport, EffectUndo};
use crate::fs::trash::{with_run, EffectRecord, Entry, Trash};
use crate::term::rules;
use aqua_tool_host::{ErrorCode, ToolError};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How much the stores of all projects together may take.
pub const QUOTA_BYTES: u64 = 512 << 20;
/// Below this much free disk a checkpoint is not made.
pub const MIN_FREE_BYTES: u64 = 1 << 30;
/// How often a store is cleaned up with git's own `gc` (it keeps objects of the last seven days).
const GC_EVERY: Duration = Duration::from_secs(7 * 86_400);
/// The files a run diff shows, and the lines of each.
pub const DIFF_FILES: usize = 100;
pub const DIFF_LINES: usize = 200;
const MARKER: &str = "aqua-project";
const RUNS_IN_MEMORY: usize = 256;

/// Why there is no checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoCheckpoint {
    /// The folder is not a trusted one.
    NotTrusted,
    GitMissing,
    DiskLow(u64),
    Refused(Refused),
    Store(String),
}

impl NoCheckpoint {
    pub fn words(&self) -> String {
        match self {
            NoCheckpoint::NotTrusted => "the project folder is not a trusted one".to_string(),
            NoCheckpoint::GitMissing => "git is not installed".to_string(),
            NoCheckpoint::DiskLow(free) => format!("only {} MB of the disk is free, and a checkpoint needs {} MB", free >> 20, MIN_FREE_BYTES >> 20),
            NoCheckpoint::Refused(why) => why.words(),
            NoCheckpoint::Store(why) => why.clone(),
        }
    }
}

/// What a run diff shows.
#[derive(Debug, Clone)]
pub struct RunDiff {
    pub root: PathBuf,
    pub start: String,
    pub end: String,
    pub changes: Vec<Change>,
    pub patches: Vec<FilePatch>,
}

enum RunState {
    /// The first look was made.
    Started { root: PathBuf, entry: Option<Entry>, start: String },
    /// There is none for this run: the reason was logged once.
    Failed,
}

/// The checkpoints.
pub struct Checkpoints {
    git: Git,
    dir: PathBuf,
    guard: Arc<PathGuard>,
    trash: Arc<Trash>,
    quota: u64,
    /// The most files a capture looks at, and how long it may take (the limits of the card; a test makes them small).
    max_files: usize,
    budget: Duration,
    runs: Mutex<HashMap<String, RunState>>,
}

/// Can this tool call change files in a folder? For commands: `journal::may_change_files`; for the git tools: the ones that write the work folder.
pub fn tool_may_change_files(tool: &str, args: &Value) -> bool {
    let strings = |v: &Value| -> Vec<String> { v.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };
    match tool {
        "term.run" | "term.start" => super::journal::may_change_files(args.get("exe").and_then(|v| v.as_str()).unwrap_or(""), &strings(args)),
        "term.shell" => rules::shell_segments(args.get("command").and_then(|v| v.as_str()).unwrap_or("")).map(|segs| segs.iter().any(|(e, a)| super::journal::may_change_files(e, a))).unwrap_or(true),
        "git.switch" | "git.merge" | "git.stash" | "git.discard" | "git.reset" | "git.pull" => true,
        _ => false,
    }
}

impl Checkpoints {
    /// `dir` is `data/snapshots`; `git_path` is the program the person named in `repos.json`, if they did.
    pub fn new(git_path: &Option<String>, dir: PathBuf, guard: Arc<PathGuard>, trash: Arc<Trash>) -> Self {
        Self { git: Git::new(super::find_git(git_path)), dir, guard, trash, quota: QUOTA_BYTES, max_files: MAX_FILES, budget: Duration::from_secs(CAPTURE_SECONDS), runs: Mutex::new(HashMap::new()) }
    }

    /// The same with another quota (a test fills a small one).
    pub fn with_quota(mut self, bytes: u64) -> Self {
        self.quota = bytes;
        self
    }

    /// The same with other limits for one capture: the most files, and the time (a test makes them small to see them hold).
    pub fn with_limits(mut self, max_files: usize, budget: Duration) -> Self {
        self.max_files = max_files;
        self.budget = budget;
        self
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The folder of a project when it is one that gets checkpoints: inside a trusted root (the workspace, or the project folder under full access). A folder that was allowed for
    /// one request is never trusted, so it never gets one: a repository there could try to influence git.
    pub fn project_root(&self, dir: &Path) -> Option<PathBuf> {
        // Read, not write: a folder that holds a file of Aqua's own (the workspace root of a person who works on Aqua itself holds `data`) cannot be written as a whole, and the files
        // that are put back are each held to the rule for a write (`capture::Git::restore`).
        let resolved = self.guard.resolve(&dir.to_string_lossy(), Op::Read).ok()?;
        (resolved.trusted && resolved.path.is_dir()).then_some(resolved.path)
    }

    /// The folder a call named (`cwd`, as the file tools read it: `@ws/proj` or a path), when the file guard allows it.
    pub fn folder_named(&self, name: &str) -> Option<PathBuf> {
        self.guard.resolve(name, Op::Read).ok().map(|r| r.path)
    }

    /// The folder a command runs in when it names none: the first of the folders Aqua may use (what `term.run` does).
    pub fn default_project(&self) -> Option<PathBuf> {
        self.guard.roots().first().map(|r| r.1.clone())
    }

    /// The name of the store of a folder: a hash of its canonical path.
    fn store_name(root: &Path) -> String {
        let key = crate::fs::guard::display(root).to_ascii_lowercase();
        Sha256::digest(key.as_bytes()).iter().take(8).map(|b| format!("{b:02x}")).collect()
    }

    fn store_path(&self, root: &Path) -> PathBuf {
        self.dir.join(Self::store_name(root))
    }

    /// The place of a project's store, made when it is not there yet (an empty bare folder, the files that are never kept, a note of whose it is).
    fn place_of(&self, root: &Path, make: bool, deadline: Instant, cancel: &AtomicBool) -> Result<Place, NoCheckpoint> {
        let git_dir = self.store_path(root);
        if make && !git_dir.join("HEAD").is_file() {
            std::fs::create_dir_all(&git_dir).map_err(|e| NoCheckpoint::Store(format!("the store {} cannot be made ({e})", crate::fs::guard::display(&git_dir))))?;
            // A bare repository with the person's own configuration kept out of it: what it needs is passed on every call (`settings`).
            // Making the store is not part of the five seconds a look may take: it is done once for a project.
            let _ = deadline;
            if !self.git.init_bare(&git_dir, Instant::now() + Duration::from_secs(30), cancel) {
                return Err(NoCheckpoint::Store("git could not make the store".into()));
            }
            let _ = std::fs::create_dir_all(git_dir.join("info"));
            let _ = std::fs::write(git_dir.join("info").join("exclude"), "# Made by Aqua: what is not part of a project's checkpoints.\n.git/\ntarget/\nnode_modules/\n.aqua_sandbox/\n");
            let _ = std::fs::write(git_dir.join(MARKER), root.to_string_lossy().as_bytes());
            // A store that was just made has nothing to clean up: its first clean-up is a week away.
            let _ = std::fs::write(git_dir.join("aqua-gc"), b"");
        }
        if !git_dir.join("HEAD").is_file() {
            return Err(NoCheckpoint::Store("the store of this project is gone".into()));
        }
        Ok(Place {
            work_tree: root.to_path_buf(),
            git_dir: git_dir.clone(),
            index: git_dir.join("index"),
            programs_off: Vec::new(),
            // What the store needs to keep files as they are: no end-of-line conversion, no links made, long names.
            settings: ["core.autocrlf=false", "core.safecrlf=false", "core.symlinks=false", "core.longpaths=true", "core.fsmonitor=false", "gc.auto=0"].iter().map(|s| s.to_string()).collect(),
        })
    }

    /// Looks at a project folder now and keeps the tree. The five seconds, the 50,000 files, the disk and the quota are held here.
    pub fn capture(&self, root: &Path, cancel: &AtomicBool) -> Result<Captured, NoCheckpoint> {
        if !self.git.available() {
            return Err(NoCheckpoint::GitMissing);
        }
        if let Some(free) = free_bytes(&self.dir).or_else(|| free_bytes(root)) {
            if free < MIN_FREE_BYTES {
                return Err(NoCheckpoint::DiskLow(free));
            }
        }
        let deadline = Instant::now() + self.budget;
        let place = self.place_of(root, true, deadline, cancel)?;
        let allow = allow_rule(&self.guard, root, true);
        let captured = self.git.capture(&place, &CaptureOpts { seed: None, allow: &allow, max_files: self.max_files, cap_bytes: FILE_CAP_BYTES, deadline, fast: true }, cancel).map_err(NoCheckpoint::Refused)?;
        // The store was used now: the quota takes the ones used least first.
        let _ = std::fs::write(place.git_dir.join(MARKER), root.to_string_lossy().as_bytes());
        self.evict_to_quota(&place.git_dir);
        self.gc_when_due(&place);
        Ok(captured)
    }

    /// How much the stores take.
    pub fn usage(&self) -> u64 {
        crate::fs::trash::dir_size(&self.dir)
    }

    /// Removes the stores used least, oldest first, until the stores fit the quota; `keep` is never removed. Only a folder with the note Aqua writes into every store is touched.
    pub fn evict_to_quota(&self, keep: &Path) {
        let mut stores: Vec<(PathBuf, std::time::SystemTime, u64)> = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return };
        for e in rd.flatten() {
            let path = e.path();
            let marker = path.join(MARKER);
            if !path.is_dir() || !marker.is_file() {
                continue;
            }
            let used = marker.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            stores.push((path.clone(), used, crate::fs::trash::dir_size(&path)));
        }
        let mut total: u64 = stores.iter().map(|s| s.2).sum();
        stores.sort_by_key(|s| s.1);
        for (path, _, size) in stores {
            if total <= self.quota {
                break;
            }
            if crate::fs::guard::same_place(&path, keep) {
                continue;
            }
            if std::fs::remove_dir_all(&path).is_ok() {
                total = total.saturating_sub(size);
                tracing::info!("checkpoints: the store of a project that was not used lately was removed to keep to the quota");
            }
        }
    }

    /// Cleans the store with git's own `gc` once a week, without waiting for it (objects of the last seven days are kept).
    fn gc_when_due(&self, place: &Place) {
        let marker = place.git_dir.join("aqua-gc");
        let due = marker.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_none_or(|age| age >= GC_EVERY);
        if !due {
            return;
        }
        let _ = std::fs::write(&marker, b"");
        let (git, place) = (self.git.clone(), place.clone());
        std::thread::spawn(move || {
            let never = AtomicBool::new(false);
            let _ = git.run(&place, &["gc", "--prune=7.days.ago", "--quiet"], None, Instant::now() + Duration::from_secs(300), &never);
        });
    }

    /// Takes away the store of a project (the person asked Aqua to forget it). The checkpoints of its runs can no longer be put back.
    pub fn forget(&self, root: &Path) -> bool {
        let path = self.store_path(root);
        path.join(MARKER).is_file() && std::fs::remove_dir_all(&path).is_ok()
    }

    // ---- what a run does -------------------------------------------------------------------------------------------------------------------------

    /// Called before a tool call of a run that may change files: the first time for a run it looks at the project folder and writes the record of the checkpoint to the undo store.
    /// Any other time, and any call that gets no checkpoint, does nothing (a run that could not get one is not tried again). Returns why there is none, once.
    pub fn before_command(&self, run_id: &str, project_dir: &Path, cancel: &AtomicBool) -> Result<(), NoCheckpoint> {
        if run_id.is_empty() {
            return Ok(());
        }
        if self.runs.lock().unwrap_or_else(PoisonError::into_inner).contains_key(run_id) {
            return Ok(());
        }
        let outcome = (|| -> Result<(PathBuf, Captured), NoCheckpoint> {
            let root = self.project_root(project_dir).ok_or(NoCheckpoint::NotTrusted)?;
            let captured = self.capture(&root, cancel)?;
            Ok((root, captured))
        })();
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        if runs.len() >= RUNS_IN_MEMORY {
            runs.clear();
        }
        match outcome {
            Ok((root, captured)) => {
                let effect = EffectRecord { root: root.to_string_lossy().to_string(), start: Some(captured.tree.clone()), ..Default::default() };
                let entry = with_run(run_id, || self.trash.note_effect("checkpoint", &root, effect));
                runs.insert(run_id.to_string(), RunState::Started { root, entry, start: captured.tree });
                Ok(())
            }
            Err(why) => {
                runs.insert(run_id.to_string(), RunState::Failed);
                tracing::info!("checkpoints: no checkpoint for run {run_id} ({})", why.words());
                Err(why)
            }
        }
    }

    /// Called when a run ends: looks at the folder again. The record keeps the end tree; when nothing changed, or the second look could not be made, the record is removed (there is
    /// nothing to put back, or no way to tell what is the run's). Returns how many files changed.
    pub fn finish_run(&self, run_id: &str, cancel: &AtomicBool) -> Option<usize> {
        let state = self.runs.lock().unwrap_or_else(PoisonError::into_inner).remove(run_id)?;
        let RunState::Started { root, entry, start } = state else { return None };
        let entry = entry?;
        let end = match self.capture(&root, cancel) {
            Ok(c) => c.tree,
            Err(why) => {
                tracing::info!("checkpoints: no look at the end of run {run_id} ({}); its checkpoint is dropped", why.words());
                self.trash.drop_entry(&entry.undo_id);
                return None;
            }
        };
        if end == start {
            self.trash.drop_entry(&entry.undo_id);
            return Some(0);
        }
        let deadline = Instant::now() + Duration::from_secs(CAPTURE_SECONDS);
        let count = self.place_of(&root, false, deadline, cancel).ok().and_then(|p| self.git.changes(&p, &start, &end, deadline, cancel).ok()).map(|c| c.len()).unwrap_or(0);
        let effect = EffectRecord { root: root.to_string_lossy().to_string(), start: Some(start), end: Some(end), changed: count, ..Default::default() };
        if self.trash.update_effect(&entry, effect).is_none() {
            self.trash.drop_entry(&entry.undo_id);
            return None;
        }
        Some(count)
    }

    /// What differs between two trees of a project.
    pub fn changes_between(&self, root: &Path, a: &str, b: &str, cancel: &AtomicBool) -> Result<Vec<Change>, ToolError> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let place = self.place_of(root, false, deadline, cancel).map_err(|why| ToolError::new(ErrorCode::NotFound, why.words()))?;
        self.git.changes(&place, a, b, deadline, cancel).map_err(|s| ToolError::new(ErrorCode::BackendError, s.words()))
    }

    /// The names of the files a tree of a project holds.
    pub fn files_of(&self, root: &Path, tree: &str, cancel: &AtomicBool) -> Result<Vec<String>, ToolError> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let place = self.place_of(root, false, deadline, cancel).map_err(|why| ToolError::new(ErrorCode::NotFound, why.words()))?;
        self.git.list_tree(&place, tree, deadline, cancel).map_err(|s| ToolError::new(ErrorCode::BackendError, s.words()))
    }

    /// What a run changed, as the checkpoints saw it: the files, and the patch of each (100 files, 200 lines each). `None` when the run has no finished checkpoint.
    pub fn diff_of_run(&self, run_id: &str, cancel: &AtomicBool) -> Option<Result<RunDiff, ToolError>> {
        let entry = self.trash.run_entries(run_id).into_iter().find(|e| e.reason == "checkpoint" && e.effect.as_ref().is_some_and(|f| f.end.is_some()))?;
        Some(self.diff_of(&entry, cancel))
    }

    fn diff_of(&self, entry: &Entry, cancel: &AtomicBool) -> Result<RunDiff, ToolError> {
        let (root, start, end) = self.trees_of(entry)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let place = self.place_of(&root, false, deadline, cancel).map_err(|why| ToolError::new(ErrorCode::NotFound, format!("the checkpoint is gone: {}", why.words())))?;
        let stop = |s: Stop| ToolError::new(ErrorCode::BackendError, s.words());
        let changes = self.git.changes(&place, &start, &end, deadline, cancel).map_err(stop)?;
        let patches = self.git.patches(&place, &start, &end, &changes, DIFF_FILES, DIFF_LINES, deadline, cancel).map_err(stop)?;
        Ok(RunDiff { root, start, end, changes, patches })
    }

    fn trees_of(&self, entry: &Entry) -> Result<(PathBuf, String, String), ToolError> {
        let effect = entry.effect.as_ref().ok_or_else(|| ToolError::new(ErrorCode::InvalidArguments, format!("{} is not a checkpoint", entry.undo_id)))?;
        match (&effect.start, &effect.end) {
            (Some(s), Some(e)) if super::capture::is_object_id(s) && super::capture::is_object_id(e) => Ok((PathBuf::from(&effect.root), s.clone(), e.clone())),
            _ => Err(ToolError::new(ErrorCode::NotFound, "the checkpoint of this run was not finished: the run did not end, or the second look was given up")),
        }
    }

    /// Puts back what a run's checkpoint recorded: the files of the project that changed between the two trees, for the ones still as the run left them, and not the ones in `handled`.
    pub fn undo_checkpoint(&self, entry: &Entry, guard: &PathGuard, dry_run: bool, handled: &mut HashSet<String>, cancel: &AtomicBool) -> Result<super::capture::Restored, ToolError> {
        let (root, start, end) = self.trees_of(entry)?;
        let deadline = Instant::now() + Duration::from_secs(120);
        let place = self.place_of(&root, false, deadline, cancel).map_err(|why| ToolError::new(ErrorCode::NotFound, format!("the checkpoint is gone: {}", why.words())))?;
        for id in [&start, &end] {
            if !self.git.has_object(&place, id, deadline, cancel) {
                return Err(ToolError::new(ErrorCode::NotFound, "the checkpoint of this run is no longer kept (the store was cleaned up to keep to its size limit)"));
            }
        }
        let canonical_root = canonical(&root).unwrap_or(root.clone());
        let already = handled.clone();
        let leave = |rel: &str| already.contains(&handled_key(&canonical_root.join(rel)));
        let done = self.git.restore(&place, guard, &place.git_dir, &start, &end, &leave, "run", dry_run, deadline, cancel).map_err(|s| ToolError::new(ErrorCode::BackendError, s.words()))?;
        for rel in &done.considered {
            handled.insert(handled_key(&canonical_root.join(rel)));
        }
        Ok(done)
    }
}

impl EffectUndo for Checkpoints {
    fn undo(&self, entry: &Entry, guard: &PathGuard, dry_run: bool, handled: &mut HashSet<String>, cancel: &AtomicBool) -> Result<EffectReport, ToolError> {
        let done = self.undo_checkpoint(entry, guard, dry_run, handled, cancel)?;
        let (put, took) = if dry_run { ("would put back", "would remove") } else { ("put back", "removed") };
        let mut restored: Vec<String> = done.restored.iter().map(|p| format!("{put} {p} (from the checkpoint of the run)")).collect();
        restored.extend(done.removed.iter().map(|p| format!("{took} {p} (made during the run)")));
        Ok(EffectReport { restored, settled: done.left.is_empty(), left: done.left })
    }
}

/// What `fs.undo_run` is given to put back the records of commands and checkpoints: the journal and the checkpoints, each used for the records it made. Either may be missing (it is
/// off, or its folder could not be made): a record of that kind is then listed as one that cannot be put back from here.
pub struct Effects {
    pub journal: Option<Arc<super::journal::Journal>>,
    pub checkpoints: Option<Arc<Checkpoints>>,
}

impl EffectUndo for Effects {
    fn undo(&self, entry: &Entry, guard: &PathGuard, dry_run: bool, handled: &mut HashSet<String>, cancel: &AtomicBool) -> Result<EffectReport, ToolError> {
        let gone = |what: &str| ToolError::new(ErrorCode::Unavailable, format!("what {what} changed cannot be put back: it is not set up"));
        match entry.reason.as_str() {
            "command" => EffectUndo::undo(self.journal.as_ref().ok_or_else(|| gone("a command"))?.as_ref(), entry, guard, dry_run, handled, cancel),
            "checkpoint" => EffectUndo::undo(self.checkpoints.as_ref().ok_or_else(|| gone("a run"))?.as_ref(), entry, guard, dry_run, handled, cancel),
            other => Err(ToolError::new(ErrorCode::InvalidArguments, format!("a record of '{other}' is not one this puts back"))),
        }
    }
}


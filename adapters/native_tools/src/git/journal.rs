//! The journal of what a command changed (card OA7-03, TM-12).
//!
//! The file tools can undo what they changed (`fs.undo_run`); a command (`cargo fmt`, `npm install`, a code generator, `git checkout`) changes files no undo reached. With the journal
//! on (`journal` in `term.json`, or `AQUA_TERM_JOURNAL=on`) a `term.run` or `term.shell` that is not known to be read-only, run in a git repository inside the folders Aqua may use,
//! is looked at before and after: a tree of the work folder is taken each time (`capture.rs`: a copy of the repository's own index, the files that changed and the new ones that
//! no ignore rule names, `write-tree`), and the difference is the list the result carries, `files changed by this command`. Both trees are kept in the repository's own object
//! store as two commits under `refs/aqua/journal/<run>/<id>` (local, never pushed, unreachable from any branch), and `term.undo` puts the files back from them.
//!
//! What it never does: write the person's index, rewrite history, move a branch, delete a file the command did not make, put back a file somebody changed after the command
//! (that one is left and listed), or put back a path the file guard would not let a tool write. What it leaves out: files over 2 MB, ignored files (a build folder), anything in a
//! folder Aqua keeps its own files in, credential folders and secret-looking names. It gives up quietly, and the command runs as it always did, when the folder is not a repository,
//! the repository's configuration cannot be read, there are more than 50,000 files to look at, the disk has less than 512 MB free, or a capture takes more than five seconds.
//!
//! The references are removed after seven days (`KEEP_DAYS`). The objects they held are left to git's own clean-up: Aqua never runs `git gc --prune` in a repository it does not own.

use super::capture::{allow_rule, free_bytes, list_changes, scratch_name, Captured, CaptureOpts, Change, Git, Place, Refused, Restored, Stop, CAPTURE_SECONDS, FILE_CAP_BYTES, MAX_FILES};
use super::discover;
use crate::fs::guard::{same_place, Op, PathGuard};
use crate::fs::provider::{handled_key, EffectReport, EffectUndo};
use crate::fs::trash::{with_run, EffectRecord, Entry, Trash};
use crate::term::rules;
use aqua_tool_host::{ErrorCode, ToolError};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long the references of the journal are kept.
pub const KEEP_DAYS: u64 = 7;
/// How many changed files a result names.
pub const LISTED: usize = 20;
/// Below this much free disk the journal is not made.
pub const MIN_FREE_BYTES: u64 = 512 << 20;
/// What is put back is allowed more time than a capture: a restore of a thousand files is a few calls of git.
const RESTORE_SECONDS: u64 = 60;
const RECORDS_IN_MEMORY: usize = 200;
const TIDY_EVERY: Duration = Duration::from_secs(3600);

/// One journalled command, as `term.undo` needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: String,
    /// The top folder of the repository.
    pub root: PathBuf,
    /// `refs/aqua/journal/<run>/<id>`: the commit after the command; its parent is the commit before.
    pub reference: String,
    /// What it was, short: the program and its first word.
    pub label: String,
    pub changed: usize,
}

impl Record {
    /// The record a trash entry of a command stands for (`None` for any other entry, or one whose names are not what Aqua writes).
    pub fn from_entry(entry: &Entry) -> Option<Record> {
        let e = entry.effect.as_ref()?;
        (entry.reason == "command" && valid_id(&e.id) && super::capture::is_aqua_ref(e.reference.as_deref()?)).then(|| Record {
            id: e.id.clone(),
            root: PathBuf::from(&e.root),
            reference: e.reference.clone().unwrap_or_default(),
            label: e.command.clone().unwrap_or_default(),
            changed: e.changed,
        })
    }
}

/// What the look before a command came to.
pub enum Begun {
    /// The command does not run in a repository inside the folders Aqua may use: nothing is looked at and nothing is said.
    NotInARepository,
    /// The look was given up (the disk is nearly full, the configuration of the repository cannot be read, there are too many files, it took too long): the answer says so, since an
    /// empty list would be read as "it changed nothing".
    GaveUp(String),
    Before(Before),
}

/// What a command was looked at for: the tree before it.
pub struct Before {
    root: PathBuf,
    place: Place,
    tree: String,
    /// How long looking before the command took, in milliseconds.
    began_millis: u64,
    /// The commit that keeps the tree before the command, made while the command runs (it needs nothing from the folder, so it costs the command no time).
    kept: std::thread::JoinHandle<Result<String, Stop>>,
}

/// What a journalled command changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journalled {
    /// Where the files are kept, for `term.undo`; `None` when the change could not be kept (the list is still true).
    pub record: Option<Record>,
    pub changes: Vec<Change>,
    /// Files over 2 MB that were left out of both trees.
    pub skipped_large: usize,
    /// How long looking took, in milliseconds (both captures and the record).
    pub millis: u64,
    /// Why it could not be kept for undo, when it could not.
    pub not_kept: Option<String>,
    /// Why the files it changed could not be listed, when they could not (the look was given up): the list is then empty, and that is not "nothing changed".
    pub gave_up: Option<String>,
}

impl Journalled {
    /// The lines for the end of the result of the command: what it changed (at most 20 names) and how to take it back. Empty when it changed nothing.
    pub fn text(&self) -> String {
        if let Some(why) = &self.gave_up {
            return format!("\n(The files this command changed could not be listed: {why}.)");
        }
        if self.changes.is_empty() {
            return String::new();
        }
        let mut text = format!("\nfiles changed by this command ({}): {}.", self.changes.len(), list_changes(&self.changes, LISTED));
        match (&self.record, &self.not_kept) {
            (Some(r), _) => text.push_str(&format!(" Take it back with term.undo call_id={}.", r.id)),
            (None, Some(why)) => text.push_str(&format!(" (It could not be kept for undo: {why}.)")),
            (None, None) => {}
        }
        if self.skipped_large > 0 {
            text.push_str(&format!(" {} file{} over 2 MB {} not recorded.", self.skipped_large, if self.skipped_large == 1 { "" } else { "s" }, if self.skipped_large == 1 { "was" } else { "were" }));
        }
        text
    }

    /// The same for the data of the result.
    pub fn data(&self) -> Value {
        json!({
            "call_id": self.record.as_ref().map(|r| r.id.clone()),
            "changed": self.changes.len(),
            "files": self.changes.iter().take(LISTED).map(|c| json!({"status": c.status.to_string(), "path": c.path})).collect::<Vec<_>>(),
            "skipped_large": self.skipped_large,
            "millis": self.millis,
            "gave_up": self.gave_up,
        })
    }
}

/// An id a result can name and `term.undo` can take: `jr_`, hexadecimal milliseconds, a number.
fn valid_id(id: &str) -> bool {
    id.len() <= 40 && id.starts_with("jr_") && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The run's name as part of a reference: letters, digits, `_` and `-`, at most 60; a call outside a run is `norun`.
fn run_part(run: &str) -> String {
    let clean: String = run.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(60).collect();
    if clean.is_empty() {
        "norun".to_string()
    } else {
        clean
    }
}

/// Programs that only look: asked about a version or a status they change no file. (A program that is not here may change files.)
const LOOKERS: &[&str] = &[
    "hostname", "whoami", "where", "echo", "ping", "tasklist", "systeminfo", "ipconfig", "netstat", "type", "dir", "findstr", "ver", "date", "time", "tree", "more", "fc", "nslookup", "tracert", "rg", "grep",
    "cat", "ls", "head", "tail", "wc", "which", "pwd", "env", "printenv", "uname",
];

/// What `git` does to the files of the work folder (the index and the history are not files of it).
const GIT_CHANGES_FILES: &[&str] = &["checkout", "switch", "restore", "reset", "merge", "rebase", "pull", "stash", "clean", "apply", "am", "cherry-pick", "revert", "mv", "rm", "submodule", "sparse-checkout", "worktree", "bisect", "read-tree", "checkout-index", "init", "clone"];

/// Can this command change files in the folder it runs in? `false` only for what is known to look and nothing more: a program that asks for its version or its help, `git status`, `dir`.
/// Anything else may, and is journalled (the cost is two looks at the folder). `args` are the arguments as given.
pub fn may_change_files(exe: &str, args: &[String]) -> bool {
    let name = rules::program_name(exe);
    if args.len() <= 2 && args.iter().any(|a| matches!(a.as_str(), "--version" | "-V" | "--help" | "-h" | "-?" | "/?" | "version" | "help")) {
        return false;
    }
    if name == "git" {
        return rules::git_subcommand_index(args).map(|i| args[i].as_str()).is_some_and(|sub| GIT_CHANGES_FILES.contains(&sub));
    }
    !LOOKERS.contains(&name.as_str())
}

/// What a command is called in a record: its program and the first word of its arguments when that is a word and not an option (`cargo fmt`, `npm install`). Never the rest: arguments
/// can hold a secret, and the record is kept on disk.
pub fn label_of(exe: &str, args: &[String]) -> String {
    let name = rules::program_name(exe);
    match args.first().filter(|a| !a.starts_with('-') && a.len() <= 24 && a.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))) {
        Some(first) => format!("{name} {first}"),
        None => name,
    }
}

/// The journal.
pub struct Journal {
    git: Git,
    trash: Arc<Trash>,
    known: Mutex<VecDeque<Record>>,
    counter: AtomicU64,
    tidied: Mutex<HashMap<String, Instant>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Journal {
    /// `git_path` is the program the person named in `repos.json`, if they did (see `GitConfig::git_path`); `trash` is the undo store the record of a command in a run is written to.
    pub fn new(git_path: &Option<String>, trash: Arc<Trash>) -> Self {
        Self { git: Git::new(super::find_git(git_path)), trash, known: Mutex::new(VecDeque::new()), counter: AtomicU64::new(1), tidied: Mutex::new(HashMap::new()) }
    }

    pub fn git(&self) -> &Git {
        &self.git
    }

    fn new_id(&self) -> String {
        format!("jr_{:x}_{}", now_ms(), self.counter.fetch_add(1, Ordering::SeqCst))
    }

    /// The top folder of the repository that holds `cwd`, when there is one inside the folders Aqua may use (found as `git.repos` finds them: a real `.git`, never a link, and never
    /// above the root that holds `cwd`).
    pub fn repository_of(&self, guard: &PathGuard, cwd: &Path) -> Option<PathBuf> {
        let start = guard.resolve(&cwd.to_string_lossy(), Op::Read).ok()?.path;
        let top = guard.root_path_of(&start)?;
        let mut dir = start;
        loop {
            // A repository the git tools may use, in a folder the file tools may read (what is put back is held to the rule for a write, file by file).
            if discover::is_repo(guard, &dir) && guard.resolve(&dir.to_string_lossy(), Op::Read).is_ok() {
                return Some(dir);
            }
            if same_place(&dir, &top) || !dir.pop() {
                return None;
            }
        }
    }

    /// Looks at the repository of `cwd` before a command. The command goes on as it always did whatever this says; what it says is whether the files it changes will be listed.
    pub fn begin(&self, guard: &PathGuard, cwd: &Path, cancel: &AtomicBool) -> Begun {
        let began = Instant::now();
        let Some(root) = self.repository_of(guard, cwd) else { return Begun::NotInARepository };
        if let Some(free) = free_bytes(&root).filter(|free| *free < MIN_FREE_BYTES) {
            return Begun::GaveUp(format!("only {} MB of the disk is free, and the journal needs {} MB", free >> 20, MIN_FREE_BYTES >> 20));
        }
        let deadline = Instant::now() + Duration::from_secs(CAPTURE_SECONDS);
        let place = match self.git.place_of_repository(&root, &scratch_name("journal"), deadline, cancel) {
            Ok(p) => p,
            Err(why) => return Begun::GaveUp(why.words()),
        };
        self.tidy(&place, cancel);
        let tree = match self.take(guard, &place, deadline, cancel) {
            Ok((tree, _)) => tree,
            Err(why) => return Begun::GaveUp(why.words()),
        };
        // Keeping the tree is a call of git that does not look at the folder: it is made while the command runs. (An object that is made and never named is cleaned up by git in time.)
        let kept = {
            let (git, place, tree) = (self.git.clone(), place.clone(), tree.clone());
            std::thread::spawn(move || git.commit_tree(&place, &tree, None, "aqua journal: the folder before a command", Instant::now() + Duration::from_secs(60), &AtomicBool::new(false)))
        };
        Begun::Before(Before { root, place, tree, began_millis: began.elapsed().as_millis() as u64, kept })
    }

    fn take(&self, guard: &PathGuard, place: &Place, deadline: Instant, cancel: &AtomicBool) -> Result<(String, Captured), Refused> {
        let allow = allow_rule(guard, &place.work_tree, false);
        let seed = place.git_dir.join("index");
        let captured = self.git.capture(place, &CaptureOpts { seed: Some(&seed), allow: &allow, max_files: MAX_FILES, cap_bytes: FILE_CAP_BYTES, deadline, fast: false }, cancel);
        // The copy of the index was only the means.
        let _ = std::fs::remove_file(&place.index);
        captured.map(|c| (c.tree.clone(), c))
    }

    /// Looks again after the command and keeps what changed. `run` is the agent run (empty outside one: the reference is then under `norun`, and no record is written to the undo
    /// store). A look that cannot be made says so (`gave_up`): an empty list would be read as "it changed nothing".
    pub fn finish(&self, before: Before, guard: &PathGuard, run: &str, label: &str, cancel: &AtomicBool) -> Journalled {
        let finished = Instant::now();
        let deadline = finished + Duration::from_secs(CAPTURE_SECONDS);
        let mut done = Journalled { record: None, changes: Vec::new(), skipped_large: 0, millis: 0, not_kept: None, gave_up: None };
        let gave_up = |mut done: Journalled, why: String| {
            tracing::debug!("journal: the files a command changed could not be listed ({why})");
            done.millis = before.began_millis + finished.elapsed().as_millis() as u64;
            done.gave_up = Some(why);
            done
        };
        // The configuration is read again: a command may have changed it, and what it names must be switched off for this look as well.
        let place = match self.git.refreshed(&before.place, deadline, cancel) {
            Ok(p) => p,
            Err(why) => return gave_up(done, why.words()),
        };
        let (tree, after) = match self.take(guard, &place, deadline, cancel) {
            Ok(t) => t,
            Err(why) => return gave_up(done, why.words()),
        };
        done.skipped_large = after.skipped_large;
        if tree != before.tree {
            match self.git.changes(&place, &before.tree, &tree, deadline, cancel) {
                Ok(changes) => done.changes = changes,
                Err(why) => return gave_up(done, why.words()),
            }
        }
        if !done.changes.is_empty() {
            let id = self.new_id();
            let reference = format!("refs/aqua/journal/{}/{id}", run_part(run));
            // Keeping the two trees is a few small objects; it gets the time a capture gets again. The commit of the tree before was made while the command ran.
            let keep_until = Instant::now() + Duration::from_secs(CAPTURE_SECONDS);
            let kept = (|| -> Result<(), Stop> {
                let was = before.kept.join().map_err(|_| Stop::Failed("the commit of the folder before the command was not made".into()))??;
                let now = self.git.commit_tree(&place, &tree, Some(&was), &format!("aqua journal {id}: {label}"), keep_until, cancel)?;
                self.git.set_ref(&place, &reference, &now, keep_until, cancel)
            })();
            match kept {
                Ok(()) => {
                    let record = Record { id: id.clone(), root: before.root.clone(), reference: reference.clone(), label: label.to_string(), changed: done.changes.len() };
                    self.remember(record.clone());
                    if !run.is_empty() {
                        let effect = EffectRecord { id, root: before.root.to_string_lossy().to_string(), reference: Some(reference), command: Some(label.to_string()), changed: done.changes.len(), ..Default::default() };
                        if with_run(run, || self.trash.note_effect("command", &before.root, effect)).is_none() {
                            tracing::debug!("journal: the command could not be written to the undo store of the run; term.undo still works with its id");
                        }
                    }
                    done.record = Some(record);
                }
                Err(why) => done.not_kept = Some(why.words()),
            }
        }
        done.millis = before.began_millis + finished.elapsed().as_millis() as u64;
        done
    }

    /// A command that was put back whole is of no further use: its record in the undo store is forgotten (the references of the journal stay until they are old).
    pub fn settle(&self, id: &str) {
        if let Some(entry) = self.trash.find_command(id) {
            self.trash.drop_entry(&entry.undo_id);
        }
    }

    fn remember(&self, record: Record) {
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        if known.len() >= RECORDS_IN_MEMORY {
            known.pop_front();
        }
        known.push_back(record);
    }

    /// Takes away what is old: the references of the journal after seven days, and the temporary indexes a crash left in the git folder. At most once an hour for a repository.
    fn tidy(&self, place: &Place, cancel: &AtomicBool) {
        let key = place.git_dir.to_string_lossy().to_ascii_lowercase();
        {
            let mut seen = self.tidied.lock().unwrap_or_else(PoisonError::into_inner);
            if seen.get(&key).is_some_and(|at| at.elapsed() < TIDY_EVERY) {
                return;
            }
            if seen.len() > 200 {
                seen.clear();
            }
            seen.insert(key, Instant::now());
        }
        let deadline = Instant::now() + Duration::from_secs(CAPTURE_SECONDS);
        let cutoff = now_ms() / 1000 - KEEP_DAYS * 86_400;
        for (name, when) in self.git.refs_under(place, "refs/aqua/journal/", deadline, cancel) {
            if when < cutoff {
                self.git.delete_ref(place, &name, deadline, cancel);
            }
        }
        if let Ok(rd) = std::fs::read_dir(&place.git_dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d > Duration::from_secs(3600));
                if old && name.starts_with("aqua-") && (name.contains("journal") || name.contains("restore")) && e.path().is_file() {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    /// The record of the command a result named: in memory, in the undo store, or (with `cwd`) in that repository's own references.
    pub fn find(&self, guard: &PathGuard, id: &str, cwd: Option<&Path>, cancel: &AtomicBool) -> Option<Record> {
        if !valid_id(id) {
            return None;
        }
        if let Some(r) = self.known.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|r| r.id == id) {
            return Some(r.clone());
        }
        if let Some(r) = self.trash.find_command(id).as_ref().and_then(Record::from_entry) {
            return Some(r);
        }
        let root = self.repository_of(guard, cwd?)?;
        let deadline = Instant::now() + Duration::from_secs(CAPTURE_SECONDS);
        let place = self.git.place_of_repository(&root, &scratch_name("journal"), deadline, cancel).ok()?;
        let suffix = format!("/{id}");
        let (reference, _) = self.git.refs_under(&place, "refs/aqua/journal/", deadline, cancel).into_iter().find(|(name, _)| name.ends_with(&suffix))?;
        Some(Record { id: id.to_string(), root, reference, label: String::new(), changed: 0 })
    }

    /// The files a journalled command changed, read from the two trees.
    pub fn changes_of(&self, record: &Record, cancel: &AtomicBool) -> Result<Vec<Change>, ToolError> {
        let deadline = Instant::now() + Duration::from_secs(CAPTURE_SECONDS * 2);
        let (place, before, after) = self.trees_of(record, deadline, cancel)?;
        self.git.changes(&place, &before, &after, deadline, cancel).map_err(stop_error)
    }

    /// The place of the repository and the two trees of a record. An error says that the journal is gone (the references are kept seven days) when it is.
    fn trees_of(&self, record: &Record, deadline: Instant, cancel: &AtomicBool) -> Result<(Place, String, String), ToolError> {
        let place = self.git.place_of_repository(&record.root, &scratch_name("journal"), deadline, cancel).map_err(stop_error)?;
        let gone = || ToolError::new(ErrorCode::NotFound, format!("what command {} changed is no longer kept: the journal is kept {KEEP_DAYS} days, and git may have cleaned up since", record.id));
        let after_commit = self.git.ref_target(&place, &record.reference, deadline, cancel).ok_or_else(gone)?;
        let after = self.git.tree_of(&place, &after_commit, deadline, cancel).ok_or_else(gone)?;
        let parent = self.git.parent_of(&place, &after_commit, deadline, cancel).ok_or_else(gone)?;
        let before = self.git.tree_of(&place, &parent, deadline, cancel).ok_or_else(gone)?;
        Ok((place, before, after))
    }

    /// Puts back what a journalled command changed, for the files that are still as the command left them (see `capture.rs`). `handled` is the files an undo of a whole run has
    /// dealt with already (lower-cased, with `/`): those are left to it, and what this deals with is added.
    pub fn undo(&self, record: &Record, guard: &PathGuard, dry_run: bool, handled: &mut HashSet<String>, cancel: &AtomicBool) -> Result<Restored, ToolError> {
        let deadline = Instant::now() + Duration::from_secs(RESTORE_SECONDS);
        let (place, before, after) = self.trees_of(record, deadline, cancel)?;
        let root = place.work_tree.clone();
        let already = handled.clone();
        let leave = |rel: &str| already.contains(&handled_key(&root.join(rel)));
        let done = self.git.restore(&place, guard, &place.git_dir, &before, &after, &leave, "command", dry_run, deadline, cancel).map_err(stop_error)?;
        for rel in &done.considered {
            handled.insert(handled_key(&root.join(rel)));
        }
        Ok(done)
    }
}

fn stop_error(s: Stop) -> ToolError {
    match s {
        Stop::Cancelled => ToolError::new(ErrorCode::Cancelled, "the run was cancelled and git was stopped"),
        Stop::TimedOut => ToolError::new(ErrorCode::Timeout, "git took too long and was stopped"),
        Stop::Failed(why) => ToolError::new(ErrorCode::BackendError, why),
    }
}

/// The words for a restore: what was put back, what was taken away, what was left and why.
pub fn restore_lines(done: &Restored, label: &str, dry_run: bool) -> Vec<String> {
    let by = if label.is_empty() { String::new() } else { format!(" (by {label})") };
    let (put, took) = if dry_run { ("would put back", "would remove") } else { ("put back", "removed") };
    let mut lines: Vec<String> = done.restored.iter().map(|p| format!("{put} {p}{by}")).collect();
    lines.extend(done.removed.iter().map(|p| format!("{took} {p} (it was new){by}")));
    lines
}

impl EffectUndo for Journal {
    fn undo(&self, entry: &Entry, guard: &PathGuard, dry_run: bool, handled: &mut HashSet<String>, cancel: &AtomicBool) -> Result<EffectReport, ToolError> {
        let record = Record::from_entry(entry).ok_or_else(|| ToolError::new(ErrorCode::InvalidArguments, format!("{} is not a record of a command", entry.undo_id)))?;
        let done = Journal::undo(self, &record, guard, dry_run, handled, cancel)?;
        Ok(EffectReport { restored: restore_lines(&done, &record.label, dry_run), settled: done.left.is_empty(), left: done.left })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_command_that_only_looks_is_not_looked_at_and_any_other_may_have_changed_files() {
        for looks in [("git", &["status", "--short"][..]), ("git", &["log", "-n", "3"]), ("git", &["diff"]), ("git", &["commit", "-m", "x"]), ("git", &["add", "."]), ("git", &["push"]), ("cargo", &["--version"]), ("node", &["-V"]), ("where", &["git"]), ("hostname", &[]), ("dir", &["/b"]), ("python", &["--help"])] {
            assert!(!may_change_files(looks.0, &args(looks.1)), "{looks:?}");
        }
        for may in [("git", &["checkout", "main"][..]), ("git", &["restore", "a.txt"]), ("git", &["reset", "--hard"]), ("git", &["stash", "pop"]), ("git", &["clean", "-fd"]), ("git", &["pull"]), ("cargo", &["fmt"]), ("npm", &["install"]), ("node", &["gen.js"]), ("cargo", &["test"]), ("rustfmt", &["a.rs"]), ("C:\\Tools\\Prettier.exe", &["--write", "."])] {
            assert!(may_change_files(may.0, &args(may.1)), "{may:?}");
        }
    }

    #[test]
    fn a_record_names_the_program_and_a_word_and_never_the_arguments() {
        assert_eq!(label_of("cargo.exe", &args(&["fmt", "--all"])), "cargo fmt");
        assert_eq!(label_of("npm", &args(&["install", "--registry", "https://user:pw@example.com"])), "npm install");
        assert_eq!(label_of("tool", &args(&["--token=abcdef", "x"])), "tool");
        assert_eq!(label_of("tool", &args(&["a-very-long-first-argument-indeed"])), "tool", "a long word could be anything");
        assert_eq!(label_of("tool", &args(&["https://user:pw@host/x"])), "tool", "not a word");
        assert_eq!(label_of("C:\\bin\\gen.exe", &[]), "gen");
    }

    #[test]
    fn ids_and_run_names_are_made_of_safe_characters() {
        assert!(valid_id("jr_1a2b3c_4") && !valid_id("u_1a2b3c_4") && !valid_id("jr_1;x") && !valid_id(&format!("jr_{}", "1".repeat(40))));
        assert_eq!(run_part("run_20261009_ab-1"), "run_20261009_ab-1");
        assert_eq!(run_part(""), "norun");
        assert_eq!(run_part("../../x y"), "xy");
        assert_eq!(run_part(&"r".repeat(100)).len(), 60);
    }
}

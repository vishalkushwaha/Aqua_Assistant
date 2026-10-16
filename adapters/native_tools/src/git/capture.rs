//! Trees of a work folder, made and compared with git's own plumbing (card OA7-03: TM-12, ED-11, CX-11).
//!
//! A tree is what `git write-tree` makes from an index: the names and the contents of the files of a folder at one moment, kept as git objects. Two things use this module. The
//! journal of what a command changed (`journal.rs`) takes a tree before and after a command in the repository's own object store, working in a COPY of the repository's index. The
//! checkpoints of a trusted project (`checkpoint.rs`) take trees of a project in a store of Aqua's own (`data/snapshots/<hash of the folder>`), so any folder can have them.
//!
//! What keeps it safe, each part with a test:
//! * git never runs a program the repository names. Every call starts with the options that switch off hooks, fsmonitor, external diff and the like (`rules::git_safe_options`), and
//!   the programs the repository's own configuration names (a filter, a merge driver, a credential helper...) are switched off by name from a listing of that configuration
//!   (`rules::switch_off_program_keys`), as `git.*` and `term.run git` do. A repository whose configuration cannot be read is not run in.
//! * The person's own index is never written. The journal copies it and works on the copy; the checkpoints have an index of their own.
//! * Nothing is added to a tree that is not a plain file inside the folder: a link or a junction (and every file below one) is left out, so a folder that holds a junction to somewhere
//!   else is not followed out of itself; a file over 2 MB is left out and counted; Aqua's own files, credential folders, secret-looking names and `.git` are left out (`allow`).
//! * A capture is cut short, and says why, when it takes more than five seconds, finds more than 50,000 files, is stopped, or finds the disk nearly full.
//! * Putting a tree back changes only files that are still as the command (or the run) left them: a file somebody changed since is left and listed (the rule of `fs.undo_run`), a path
//!   goes through the file guard as any write does, and a file that is not in the tree of the command is never touched. Nothing is deleted that the command did not make.

use crate::contain::build_env;
use crate::fs::guard::{canonical, within, Op, PathGuard};
use crate::term::rules::{git_config_scan_args, git_safe_options, switch_off_program_keys, NO_HOOKS_PATH};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How long one capture may take: past this it is given up and the answer says "no checkpoint".
pub const CAPTURE_SECONDS: u64 = 5;
/// The most files one capture looks at.
pub const MAX_FILES: usize = 50_000;
/// A file bigger than this is left out of a tree (and counted).
pub const FILE_CAP_BYTES: u64 = 2 * 1024 * 1024;
/// What one call of git may print before the rest is dropped.
const OUTPUT_CAP: usize = 64 << 20;
/// How many git programs a bisecting retry may start in one capture.
const RETRY_BUDGET: usize = 300;
/// A store of Aqua's own takes more new files than this as a pack.
const FAST_ABOVE: usize = 48;

/// Why a call of git did not give an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    Cancelled,
    TimedOut,
    Failed(String),
}

impl Stop {
    /// In words for the person and the model.
    pub fn words(&self) -> String {
        match self {
            Stop::Cancelled => "it was stopped".to_string(),
            Stop::TimedOut => format!("it took more than {CAPTURE_SECONDS} seconds"),
            Stop::Failed(why) => why.clone(),
        }
    }
}

/// What a call of git printed.
#[derive(Debug)]
pub struct Out {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// The first line git said, for an error.
    pub fn first_line(&self) -> String {
        let said = if self.stderr.trim().is_empty() { String::from_utf8_lossy(&self.stdout).to_string() } else { self.stderr.clone() };
        said.lines().find(|l| !l.trim().is_empty()).unwrap_or("git failed").chars().take(200).collect()
    }
}

/// Where git works: the folder of the files, git's own folder, and the index the tree is made in.
#[derive(Debug, Clone)]
pub struct Place {
    pub work_tree: PathBuf,
    pub git_dir: PathBuf,
    /// The index file. It is a copy of the repository's, or a file of Aqua's own; the person's own index is never given here.
    pub index: PathBuf,
    /// `-c name=value` pairs put in front of the safety options: the programs the repository's own configuration names, switched off.
    pub programs_off: Vec<String>,
    /// More settings (`core.autocrlf=false` for the checkpoint store), as `name=value`.
    pub settings: Vec<String>,
}

impl Place {
    /// The same place with another index file.
    pub fn with_index(&self, index: PathBuf) -> Place {
        Place { index, ..self.clone() }
    }
}

/// A tree that was made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub tree: String,
    /// How many files were put in the tree (or taken out of it) by this capture.
    pub updated: usize,
    /// Files over [`FILE_CAP_BYTES`] that were left out.
    pub skipped_large: usize,
    /// Files that could not be read, were links, or were left out by the rules of `allow`.
    pub skipped_other: usize,
}

/// What the caller says about a capture.
pub struct CaptureOpts<'a> {
    /// A file to copy over the index first (the repository's own index): the capture then finds what differs from it. `None`: the index is used as it is.
    pub seed: Option<&'a Path>,
    /// May this path (relative, with `/`) be in a tree? Aqua's own files, credential folders and secret-looking names are not.
    pub allow: &'a dyn Fn(&str) -> bool,
    pub max_files: usize,
    pub cap_bytes: u64,
    pub deadline: Instant,
    /// A store of Aqua's own, which keeps the files as they are (no conversion of line ends, no program of a filter): many new files are written as one pack (see `add_as_pack`).
    /// Never for a repository of the person's, whose own settings decide what is stored.
    pub fast: bool,
}

/// One difference between two trees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// `A` (added), `M` (modified), `D` (deleted) or `T` (the kind of thing changed).
    pub status: char,
    pub path: String,
    pub old_sha: String,
    pub new_sha: String,
}

/// The patch of one file of two trees, cut to a size a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub path: String,
    pub status: char,
    pub patch: String,
    pub truncated: bool,
    pub binary: bool,
}

/// What putting a tree back did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Restored {
    /// Files put back to what they were before (or would be, in a dry run).
    pub restored: Vec<String>,
    /// Files the command made and that were taken away again (or would be).
    pub removed: Vec<String>,
    /// Files left as they are, and why: changed since, not allowed, could not be written.
    pub left: Vec<(String, String)>,
    /// Files that were already as they were before: nothing to do.
    pub already: Vec<String>,
    /// Every path of the change that the caller did not leave out, whatever became of it: the caller remembers them as dealt with.
    pub considered: Vec<String>,
}

impl Restored {
    pub fn nothing(&self) -> bool {
        self.restored.is_empty() && self.removed.is_empty() && self.left.is_empty() && self.already.is_empty()
    }
}

/// The git program, and the safe way to run it.
#[derive(Clone, Debug)]
pub struct Git {
    exe: PathBuf,
}

/// The environment of every call: what the tools give any program, and what stops git from asking, paging or opening an editor.
fn environment() -> Vec<(String, String)> {
    let mut env = build_env(std::env::vars(), &HashMap::new()).unwrap_or_default();
    for (k, v) in [("GIT_TERMINAL_PROMPT", "0"), ("GIT_PAGER", "cat"), ("GIT_EDITOR", "true"), ("GIT_ASKPASS", ""), ("LC_ALL", "C"), ("LANGUAGE", "C"), ("GCM_INTERACTIVE", "never"), ("GIT_OPTIONAL_LOCKS", "0")] {
        env.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
        env.push((k.into(), v.into()));
    }
    env
}

/// A git sha as git prints it: 40 or 64 hexadecimal characters. Anything that is going to be given to git as an argument and was read from a file is held to this first.
pub fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A name of a reference Aqua made: `refs/aqua/` and then letters, digits and `_ - . /`, no `..`, no `//`, no trailing `/` or `.lock`.
pub fn is_aqua_ref(s: &str) -> bool {
    s.starts_with("refs/aqua/") && s.len() <= 200 && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/')) && !s.contains("..") && !s.contains("//") && !s.ends_with('/') && !s.ends_with(".lock") && !s.ends_with('.')
}

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

/// Free bytes on the drive that holds `path`, or `None` where the system does not say.
#[cfg(windows)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    // The folder may not exist yet: ask about the nearest one that does.
    let mut at = path.to_path_buf();
    while !at.exists() {
        if !at.pop() {
            return None;
        }
    }
    let wide: Vec<u16> = at.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    // SAFETY: the path is NUL-terminated and outlives the call; the three out pointers are valid for the call.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, &mut total_free) };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// `paths` in batches of at most `count` paths and `chars` characters, for a command line.
fn path_batches(paths: &[String], count: usize, chars: usize) -> Vec<&[String]> {
    let mut out = Vec::new();
    let (mut from, mut used) = (0usize, 0usize);
    for (i, p) in paths.iter().enumerate() {
        if i > from && (i - from >= count || used + p.len() + 1 > chars) {
            out.push(&paths[from..i]);
            from = i;
            used = 0;
        }
        used += p.len() + 1;
    }
    if from < paths.len() {
        out.push(&paths[from..]);
    }
    out
}

/// Splits the output of a `-z` listing into its entries.
fn nul_entries(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|b| *b == 0).filter(|e| !e.is_empty())
}

impl Git {
    pub fn new(exe: PathBuf) -> Self {
        Self { exe }
    }

    pub fn exe(&self) -> &Path {
        &self.exe
    }

    /// Does the program start at all?
    pub fn available(&self) -> bool {
        let mut cmd = Command::new(&self.exe);
        cmd.arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        no_window(&mut cmd);
        cmd.status().map(|s| s.success()).unwrap_or(false)
    }

    /// Runs git with every protection on: `args` are what to do, `place` says where. `stdin` is written on a thread of its own. The call ends at `deadline` or when `cancel` turns true,
    /// and git is stopped then.
    pub fn run(&self, place: &Place, args: &[&str], stdin: Option<Vec<u8>>, deadline: Instant, cancel: &AtomicBool) -> Result<Out, Stop> {
        // What the repository's own configuration names comes first and the safe options after it: git takes the last of two `-c` for a key (see `rules::git_run_args`).
        let mut argv: Vec<String> = place.programs_off.clone();
        argv.extend(git_safe_options(NO_HOOKS_PATH));
        for c in ["core.quotepath=false", "core.longpaths=true", "credential.helper=", "filter.lfs.smudge=", "filter.lfs.process=", "filter.lfs.clean=", "filter.lfs.required=false", "core.untrackedCache=false"] {
            argv.extend(["-c".into(), c.into()]);
        }
        for c in &place.settings {
            argv.extend(["-c".into(), c.clone()]);
        }
        argv.push("--literal-pathspecs".into());
        argv.push(format!("--git-dir={}", place.git_dir.to_string_lossy()));
        argv.push(format!("--work-tree={}", place.work_tree.to_string_lossy()));
        argv.extend(args.iter().map(|a| a.to_string()));
        let mut cmd = Command::new(&self.exe);
        cmd.args(&argv).current_dir(&place.work_tree).env_clear().envs(environment()).env("GIT_INDEX_FILE", &place.index);
        self.spawn_and_wait(cmd, stdin, deadline, cancel)
    }

    /// A call that is not in a place: `git --version`-like questions and the listing of a repository's configuration. `args` come with whatever options they need.
    fn run_bare(&self, args: &[String], cwd: &Path, deadline: Instant, cancel: &AtomicBool) -> Result<Out, Stop> {
        let mut cmd = Command::new(&self.exe);
        cmd.args(args).current_dir(cwd).env_clear().envs(environment());
        self.spawn_and_wait(cmd, None, deadline, cancel)
    }

    fn spawn_and_wait(&self, mut cmd: Command, stdin: Option<Vec<u8>>, deadline: Instant, cancel: &AtomicBool) -> Result<Out, Stop> {
        if cancel.load(Ordering::SeqCst) {
            return Err(Stop::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Stop::TimedOut);
        }
        cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        no_window(&mut cmd);
        let mut child = cmd.spawn().map_err(|e| Stop::Failed(format!("git could not be started ({e})")))?;
        // `git.exe` of the usual install is a launcher that starts the real git as its child: stopping the launcher alone leaves the real one running, holding the pipes and the lock of the index.
        // The call is put in a Job Object, and a stop ends the whole tree (what `term.run` does for every program).
        let job = crate::contain::Contained::attach(&child, crate::contain::Limits { memory_mb: 2048, max_processes: 16 }).ok();
        let stop = |child: &mut std::process::Child| {
            match &job {
                Some(job) => job.kill_tree(),
                None => {
                    let _ = child.kill();
                }
            }
            let _ = child.wait();
        };
        if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
            std::thread::spawn(move || {
                let _ = pipe.write_all(&bytes);
            });
        }
        let read_all = |mut pipe: Box<dyn Read + Send>| {
            std::thread::spawn(move || {
                let mut kept: Vec<u8> = Vec::new();
                let mut buf = [0u8; 65536];
                while let Ok(n) = pipe.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    if kept.len() < OUTPUT_CAP {
                        kept.extend_from_slice(&buf[..n.min(OUTPUT_CAP - kept.len())]);
                    }
                }
                kept
            })
        };
        let out_thread = child.stdout.take().map(|p| read_all(Box::new(p)));
        let err_thread = child.stderr.take().map(|p| read_all(Box::new(p)));
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(Stop::Failed(format!("git could not be waited for ({e})"))),
            }
            if cancel.load(Ordering::SeqCst) {
                stop(&mut child);
                break Err(Stop::Cancelled);
            }
            if Instant::now() >= deadline {
                stop(&mut child);
                break Err(Stop::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let stdout = out_thread.and_then(|t| t.join().ok()).unwrap_or_default();
        let stderr = err_thread.and_then(|t| t.join().ok()).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
        let status = status?;
        Ok(Out { code: status.code(), stdout, stderr })
    }

    /// Makes `dir` (which exists) a bare repository: the store of the checkpoints of one project.
    pub fn init_bare(&self, dir: &Path, deadline: Instant, cancel: &AtomicBool) -> bool {
        let mut argv = git_safe_options(NO_HOOKS_PATH);
        argv.extend(["init".to_string(), "--bare".to_string(), "--quiet".to_string(), dir.to_string_lossy().to_string()]);
        self.run_bare(&argv, dir, deadline, cancel).map(|o| o.ok()).unwrap_or(false)
    }

    /// The options that switch off, by name, every program the repository's own configuration names. A repository whose configuration cannot be read, or that names more than can be
    /// switched off, is not run in: the answer says why.
    pub fn programs_switched_off(&self, root: &Path, deadline: Instant, cancel: &AtomicBool) -> Result<Vec<String>, Stop> {
        let scan = git_config_scan_args(&["-C".to_string(), root.to_string_lossy().to_string(), "config".to_string()]);
        let r = self.run_bare(&scan, root, deadline, cancel)?;
        if !r.ok() {
            return Err(Stop::Failed(format!("the configuration of the repository could not be read ({}): git is not run in it", r.first_line())));
        }
        switch_off_program_keys(&String::from_utf8_lossy(&r.stdout)).map_err(Stop::Failed)
    }

    /// The place of a repository whose top folder is `root`: its own git folder (a linked worktree has one of its own), and a temporary index named `index_name` inside it.
    pub fn place_of_repository(&self, root: &Path, index_name: &str, deadline: Instant, cancel: &AtomicBool) -> Result<Place, Stop> {
        let programs_off = self.programs_switched_off(root, deadline, cancel)?;
        let mut argv: Vec<String> = programs_off.clone();
        argv.extend(git_safe_options(NO_HOOKS_PATH));
        argv.extend(["-C".to_string(), root.to_string_lossy().to_string(), "rev-parse".to_string(), "--absolute-git-dir".to_string()]);
        let r = self.run_bare(&argv, root, deadline, cancel)?;
        if !r.ok() {
            return Err(Stop::Failed(format!("the git folder of the repository could not be found ({})", r.first_line())));
        }
        let dir = String::from_utf8_lossy(&r.stdout).trim().to_string();
        let git_dir = canonical(Path::new(&dir)).map_err(|e| Stop::Failed(format!("the git folder of the repository cannot be used ({e})")))?;
        let index = git_dir.join(index_name);
        Ok(Place { work_tree: root.to_path_buf(), git_dir, index, programs_off, settings: Vec::new() })
    }

    /// The same place with the repository's own configuration read again: what a command did to `.git/config` (or to a file it includes) between two looks must not be taken from the
    /// first reading, so the list of programs to switch off is made again before every capture.
    pub fn refreshed(&self, place: &Place, deadline: Instant, cancel: &AtomicBool) -> Result<Place, Stop> {
        let programs_off = self.programs_switched_off(&place.work_tree, deadline, cancel)?;
        Ok(Place { programs_off, ..place.clone() })
    }

    fn must(&self, place: &Place, args: &[&str], stdin: Option<Vec<u8>>, deadline: Instant, cancel: &AtomicBool) -> Result<Out, Stop> {
        let r = self.run(place, args, stdin, deadline, cancel)?;
        if r.ok() {
            Ok(r)
        } else {
            Err(Stop::Failed(format!("git {} failed: {}", args.first().copied().unwrap_or(""), r.first_line())))
        }
    }

    /// Does the repository hold this object?
    pub fn has_object(&self, place: &Place, id: &str, deadline: Instant, cancel: &AtomicBool) -> bool {
        is_object_id(id) && self.run(place, &["cat-file", "-e", id], None, deadline, cancel).map(|r| r.ok()).unwrap_or(false)
    }

    // ---- making a tree ---------------------------------------------------------------------------------------------------------------------------

    /// Makes the tree of the work folder as it is now: the index (the copy of the repository's, or the store's own) with every changed and every new file put in it, then `write-tree`.
    pub fn capture(&self, place: &Place, opts: &CaptureOpts<'_>, cancel: &AtomicBool) -> Result<Captured, Refused> {
        let deadline = opts.deadline;
        if let Some(seed) = opts.seed {
            // A repository with no commit yet may have no index: the capture then starts from an empty one.
            let _ = std::fs::remove_file(&place.index);
            if seed.is_file() {
                std::fs::copy(seed, &place.index).map_err(|e| Refused::Failed(format!("the index could not be copied ({e})")))?;
            }
        }
        // What differs from the index among the files it knows (changed, or gone), and the files it does not know that no ignore rule names. A submodule is not looked into.
        let changed = self.run(place, &["diff-files", "--name-only", "-z", "--ignore-submodules=all"], None, deadline, cancel).map_err(Refused::from)?;
        if !changed.ok() {
            return Err(Refused::Failed(format!("git diff-files failed: {}", changed.first_line())));
        }
        let others = self.run(place, &["ls-files", "--others", "--exclude-standard", "-z"], None, deadline, cancel).map_err(Refused::from)?;
        if !others.ok() {
            return Err(Refused::Failed(format!("git ls-files failed: {}", others.first_line())));
        }
        let mut candidates: Vec<&[u8]> = nul_entries(&changed.stdout).collect();
        candidates.extend(nul_entries(&others.stdout));
        if candidates.len() > opts.max_files {
            return Err(Refused::TooManyFiles(candidates.len()));
        }
        let root_real = canonical(&place.work_tree).map_err(|e| Refused::Failed(format!("the folder cannot be resolved ({e})")))?;
        let mut folders: HashMap<String, bool> = HashMap::new();
        let (mut present, mut gone, mut skipped_large, mut skipped_other): (Vec<String>, Vec<String>, usize, usize) = (Vec::new(), Vec::new(), 0, 0);
        for raw in candidates {
            if Instant::now() >= deadline {
                return Err(Refused::TooSlow);
            }
            let Ok(path) = std::str::from_utf8(raw) else {
                skipped_other += 1;
                continue;
            };
            // A path git lists is relative and has no `..`; one that does is not trusted, whatever lists it.
            if path.is_empty() || path.ends_with('/') || path.split('/').any(|c| c.is_empty() || c == ".." || c == "." || c.eq_ignore_ascii_case(".git")) || path.contains([':', '\\', '\n', '\r']) || path.starts_with('"') {
                skipped_other += 1;
                continue;
            }
            if !(opts.allow)(path) {
                skipped_other += 1;
                continue;
            }
            let full = place.work_tree.join(path);
            match std::fs::symlink_metadata(&full) {
                // Gone: a tracked file that was deleted. The index entry is taken out.
                Err(_) => gone.push(path.to_string()),
                Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
                    if meta.len() > opts.cap_bytes {
                        skipped_large += 1;
                        continue;
                    }
                    // Every folder above the file must really be inside the folder: a junction to somewhere else is not followed out of it.
                    let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
                    let inside = *folders.entry(parent.to_string()).or_insert_with(|| {
                        let dir = if parent.is_empty() { place.work_tree.clone() } else { place.work_tree.join(parent) };
                        canonical(&dir).map(|c| within(&c, &root_real)).unwrap_or(false)
                    });
                    if inside {
                        present.push(path.to_string());
                    } else {
                        skipped_other += 1;
                    }
                }
                // A link, a folder, a device: not a file of the tree.
                Ok(_) => skipped_other += 1,
            }
        }
        let mut budget = RETRY_BUDGET;
        // A store of Aqua's own takes many new files as one pack: git writes a loose object for each file otherwise, which costs about five milliseconds a file on this computer (measured:
        // 1,857 files took 9 to 18 seconds as loose objects and 1.4 seconds as a pack), and a first look at a project would never fit in five seconds.
        let (mut done, mut failed) = (0usize, 0usize);
        if opts.fast && present.len() > FAST_ABOVE {
            let (d, f) = self.add_as_pack(place, &present, deadline, cancel).map_err(Refused::from)?;
            done += d;
            failed += f;
            let (d, f) = self.update_index(place, &gone, deadline, cancel, &mut budget).map_err(Refused::from)?;
            done += d;
            failed += f;
        } else {
            let all: Vec<String> = present.into_iter().chain(gone).collect();
            let (d, f) = self.update_index(place, &all, deadline, cancel, &mut budget).map_err(Refused::from)?;
            done += d;
            failed += f;
        }
        skipped_other += failed;
        // `write-tree` writes a loose object for every folder that is new: a first look at a project has hundreds (about 2.5 seconds for 1,851 files in 200 folders here), and one that was
        // put in as a pack gets its trees in a pack as well.
        let tree = if opts.fast && done > FAST_ABOVE { self.tree_as_pack(place, deadline, cancel).map_err(Refused::from)? } else { self.write_tree(place, deadline, cancel).map_err(Refused::from)? };
        Ok(Captured { tree, updated: done, skipped_large, skipped_other })
    }

    /// Puts these files in the index of a store of Aqua's own as blobs of one pack (`git fast-import`), without any conversion of the contents: what is in the store is what is in the file.
    /// The files are read here (at most 48 MB to a pack), so a file that cannot be read is counted and left out; the index then says what it holds (`update-index --index-info`), and
    /// a refresh gives each entry the file's own modification data back, so that the next look finds only what changed. Returns how many files were put in and how many could not be.
    fn add_as_pack(&self, place: &Place, paths: &[String], deadline: Instant, cancel: &AtomicBool) -> Result<(usize, usize), Stop> {
        const PACK_BYTES: usize = 48 << 20;
        let (mut added, mut failed) = (0usize, 0usize);
        let mut at = 0usize;
        while at < paths.len() {
            let (mut stream, mut named): (Vec<u8>, Vec<&String>) = (Vec::new(), Vec::new());
            while at < paths.len() && stream.len() < PACK_BYTES {
                if Instant::now() >= deadline {
                    return Err(Stop::TimedOut);
                }
                let path = &paths[at];
                at += 1;
                // The stream says how long the content is (`data N`): what is put in the pack is what was read here, whatever the file does next.
                let Ok(bytes) = std::fs::read(place.work_tree.join(path)) else {
                    failed += 1;
                    continue;
                };
                named.push(path);
                stream.extend_from_slice(format!("blob\nmark :{}\ndata {}\n", named.len(), bytes.len()).as_bytes());
                stream.extend_from_slice(&bytes);
                stream.push(b'\n');
            }
            if named.is_empty() {
                continue;
            }
            let marks = place.git_dir.join(format!("aqua-marks-{}-{}", std::process::id(), at));
            let marks_arg = format!("--export-marks={}", marks.to_string_lossy());
            let imported = self.run(place, &["fast-import", "--quiet", &marks_arg], Some(stream), deadline, cancel);
            let text = std::fs::read_to_string(&marks).unwrap_or_default();
            let _ = std::fs::remove_file(&marks);
            let imported = imported?;
            if !imported.ok() {
                return Err(Stop::Failed(format!("git fast-import failed: {}", imported.first_line())));
            }
            // The marks file says which blob each mark became: `:3 <id>`.
            let mut ids: HashMap<usize, &str> = HashMap::new();
            for line in text.lines() {
                if let Some((mark, id)) = line.strip_prefix(':').and_then(|l| l.split_once(' ')) {
                    if let (Ok(n), true) = (mark.parse::<usize>(), is_object_id(id.trim())) {
                        ids.insert(n, id.trim());
                    }
                }
            }
            let mut info: Vec<u8> = Vec::new();
            for (i, path) in named.iter().enumerate() {
                match ids.get(&(i + 1)) {
                    Some(id) => {
                        info.extend_from_slice(format!("100644 {id}\t{path}").as_bytes());
                        info.push(0);
                        added += 1;
                    }
                    None => failed += 1,
                }
            }
            self.must(place, &["update-index", "--add", "-z", "--index-info"], Some(info), deadline, cancel)?;
        }
        // The entries have no modification data yet: the refresh reads each file once and gives it back (an entry whose file changed since stays as it is, and the next look finds it).
        if added > 0 {
            let _ = self.run(place, &["update-index", "-q", "--refresh"], None, deadline, cancel)?;
        }
        Ok((added, failed))
    }

    /// `update-index --add --remove` for these paths. When the whole batch fails (one file that cannot be read stops git), the batch is halved until the files that fail are found and left
    /// out; the number of git programs this may start is bounded. Returns how many paths were put in and how many could not be.
    fn update_index(&self, place: &Place, paths: &[String], deadline: Instant, cancel: &AtomicBool, budget: &mut usize) -> Result<(usize, usize), Stop> {
        if paths.is_empty() {
            return Ok((0, 0));
        }
        if *budget == 0 {
            return Err(Stop::Failed("too many files could not be read".into()));
        }
        *budget -= 1;
        let mut input: Vec<u8> = Vec::new();
        for p in paths {
            input.extend_from_slice(p.as_bytes());
            input.push(0);
        }
        let r = self.run(place, &["update-index", "--add", "--remove", "-z", "--stdin"], Some(input), deadline, cancel)?;
        if r.ok() {
            return Ok((paths.len(), 0));
        }
        if paths.len() == 1 {
            return Ok((0, 1));
        }
        let (a, b) = paths.split_at(paths.len() / 2);
        let (da, fa) = self.update_index(place, a, deadline, cancel, budget)?;
        let (db, fb) = self.update_index(place, b, deadline, cancel, budget)?;
        Ok((da + db, fa + fb))
    }

    /// The tree of the index as `write-tree` would make it, but written into a pack: the entries of the index (`ls-files -s`) become one commit of `git fast-import` (all files named, none
    /// kept from before), which puts the commit and every tree in one pack, and the tree is the commit's. The same files give the same tree id as `write-tree` gives. `refs/aqua/capture`
    /// of the store names the commit, so that git's clean-up does not count what it holds as unused.
    fn tree_as_pack(&self, place: &Place, deadline: Instant, cancel: &AtomicBool) -> Result<String, Stop> {
        let listed = self.must(place, &["ls-files", "-s", "-z"], None, deadline, cancel)?;
        let mut stream: Vec<u8> = Vec::new();
        let message = "aqua capture";
        stream.extend_from_slice(format!("commit refs/aqua/capture\ncommitter Aqua <aqua@localhost> 0 +0000\ndata {}\n{message}\ndeleteall\n", message.len()).as_bytes());
        for entry in nul_entries(&listed.stdout) {
            // `<mode> <id> <stage>\t<path>`
            let text = std::str::from_utf8(entry).map_err(|_| Stop::Failed("a name in the index is not text".into()))?;
            let (head, path) = text.split_once('\t').ok_or_else(|| Stop::Failed("the index listing is not as expected".into()))?;
            let fields: Vec<&str> = head.split(' ').collect();
            let (mode, id, stage) = (fields.first().copied().unwrap_or(""), fields.get(1).copied().unwrap_or(""), fields.get(2).copied().unwrap_or(""));
            if stage != "0" || !is_object_id(id) || path.contains('\n') || path.starts_with('"') {
                return Err(Stop::Failed("the index holds an entry that cannot be written as a commit".into()));
            }
            stream.extend_from_slice(format!("M {mode} {id} {path}\n").as_bytes());
        }
        stream.extend_from_slice(b"\n");
        let made = self.run(place, &["fast-import", "--quiet"], Some(stream), deadline, cancel)?;
        if !made.ok() {
            return Err(Stop::Failed(format!("git fast-import failed: {}", made.first_line())));
        }
        let tree = self.must(place, &["rev-parse", "--verify", "-q", "refs/aqua/capture^{tree}"], None, deadline, cancel)?;
        let id = String::from_utf8_lossy(&tree.stdout).trim().to_string();
        if is_object_id(&id) {
            Ok(id)
        } else {
            Err(Stop::Failed("git gave no tree for the capture".into()))
        }
    }

    fn write_tree(&self, place: &Place, deadline: Instant, cancel: &AtomicBool) -> Result<String, Stop> {
        let r = self.must(place, &["write-tree"], None, deadline, cancel)?;
        let id = String::from_utf8_lossy(&r.stdout).trim().to_string();
        if is_object_id(&id) {
            Ok(id)
        } else {
            Err(Stop::Failed("git write-tree gave no tree".into()))
        }
    }

    // ---- comparing trees -------------------------------------------------------------------------------------------------------------------------

    /// What differs between two trees, by path.
    pub fn changes(&self, place: &Place, a: &str, b: &str, deadline: Instant, cancel: &AtomicBool) -> Result<Vec<Change>, Stop> {
        if !is_object_id(a) || !is_object_id(b) {
            return Err(Stop::Failed("not a tree".into()));
        }
        let r = self.must(place, &["diff-tree", "-r", "--raw", "-z", "--no-renames", "--no-abbrev", "--ignore-submodules=all", a, b], None, deadline, cancel)?;
        let mut out = Vec::new();
        let mut it = nul_entries(&r.stdout);
        while let (Some(head), Some(path)) = (it.next(), it.next()) {
            let head = String::from_utf8_lossy(head);
            let fields: Vec<&str> = head.trim_start_matches(':').split(' ').collect();
            let (Some(old_sha), Some(new_sha), Some(status)) = (fields.get(2), fields.get(3), fields.get(4)) else { continue };
            let Ok(path) = std::str::from_utf8(path) else { continue };
            out.push(Change { status: status.chars().next().unwrap_or('M'), path: path.to_string(), old_sha: old_sha.to_string(), new_sha: new_sha.to_string() });
        }
        out.sort_by(|x, y| x.path.cmp(&y.path));
        Ok(out)
    }

    /// The names of every file a tree holds.
    pub fn list_tree(&self, place: &Place, tree: &str, deadline: Instant, cancel: &AtomicBool) -> Result<Vec<String>, Stop> {
        if !is_object_id(tree) {
            return Err(Stop::Failed("not a tree".into()));
        }
        let r = self.must(place, &["ls-tree", "-r", "--name-only", "-z", tree], None, deadline, cancel)?;
        let mut names: Vec<String> = nul_entries(&r.stdout).filter_map(|e| std::str::from_utf8(e).ok().map(String::from)).collect();
        names.sort();
        Ok(names)
    }

    /// The patch of every changed file between two trees (at most `max_files` files, `max_lines` lines of each), for a person to read. A binary file is named, not shown.
    #[allow(clippy::too_many_arguments)]
    pub fn patches(&self, place: &Place, a: &str, b: &str, changes: &[Change], max_files: usize, max_lines: usize, deadline: Instant, cancel: &AtomicBool) -> Result<Vec<FilePatch>, Stop> {
        if !is_object_id(a) || !is_object_id(b) {
            return Err(Stop::Failed("not a tree".into()));
        }
        let mut out = Vec::new();
        let wanted: Vec<&Change> = changes.iter().take(max_files).collect();
        let names: Vec<String> = wanted.iter().map(|c| c.path.clone()).collect();
        for batch in path_batches(&names, 25, 12_000) {
            let chunk: Vec<&Change> = wanted.iter().filter(|c| batch.contains(&c.path)).copied().collect();
            let mut args: Vec<&str> = vec!["diff-tree", "-r", "-p", "-U3", "--no-color", "--no-ext-diff", "--no-textconv", "--no-renames", "--ignore-submodules=all", a, b, "--"];
            args.extend(batch.iter().map(|p| p.as_str()));
            let r = self.must(place, &args, None, deadline, cancel)?;
            let text = String::from_utf8_lossy(&r.stdout).to_string();
            let mut files: Vec<(String, String)> = Vec::new();
            for part in text.split("\ndiff --git ") {
                let part = part.strip_prefix("diff --git ").unwrap_or(part);
                if part.trim().is_empty() {
                    continue;
                }
                // `a/path b/path` on the first line; the path is what the changes say (the header is quoted when it has odd characters, so the changes are the authority).
                let head = part.lines().next().unwrap_or("");
                let name = chunk.iter().find(|c| head.ends_with(&format!(" b/{}", c.path)) || head.contains(&format!(" b/\"{}", c.path))).map(|c| c.path.clone()).unwrap_or_else(|| head.rsplit(" b/").next().unwrap_or("").trim_matches('"').to_string());
                files.push((name, part.to_string()));
            }
            for (name, body) in files {
                let status = chunk.iter().find(|c| c.path == name).map(|c| c.status).unwrap_or('M');
                let binary = body.contains("\nBinary files ") || body.contains("\nGIT binary patch");
                let mut lines: Vec<&str> = body.lines().collect();
                let truncated = lines.len() > max_lines;
                lines.truncate(max_lines);
                // The patch starts at the first hunk: the `index` and `mode` lines say nothing a person needs.
                let shown: Vec<&str> = if binary { vec!["Binary file differs"] } else { lines.into_iter().skip_while(|l| !l.starts_with("--- ") && !l.starts_with("@@")).collect() };
                out.push(FilePatch { path: name, status, patch: shown.join("\n"), truncated, binary });
            }
        }
        Ok(out)
    }

    /// What each of `paths` is in the work folder now, as the blob git would make of it (`git add`'s own end-of-line rules and all, the programs the repository names switched off):
    /// `None` when nothing is there, `Some("")` when what is there is not a plain file (a link, a folder), else the id of the blob. A file git cannot read is `Some("")` too.
    fn blobs_now(&self, place: &Place, paths: &[String], deadline: Instant, cancel: &AtomicBool) -> Result<HashMap<String, Option<String>>, Stop> {
        let mut out: HashMap<String, Option<String>> = HashMap::new();
        let mut hash_these: Vec<&String> = Vec::new();
        for p in paths {
            match std::fs::symlink_metadata(place.work_tree.join(p)) {
                Err(_) => {
                    out.insert(p.clone(), None);
                }
                Ok(m) if m.is_file() && !m.file_type().is_symlink() && !p.contains(['\n', '\r']) => hash_these.push(p),
                Ok(_) => {
                    out.insert(p.clone(), Some(String::new()));
                }
            }
        }
        for batch in hash_these.chunks(2000) {
            let input = batch.iter().map(|p| p.as_str()).collect::<Vec<_>>().join("\n") + "\n";
            let hashed = self.run(place, &["hash-object", "--stdin-paths"], Some(input.into_bytes()), deadline, cancel)?;
            let ids: Vec<String> = String::from_utf8_lossy(&hashed.stdout).lines().map(str::to_string).collect();
            if hashed.ok() && ids.len() == batch.len() && ids.iter().all(|i| is_object_id(i)) {
                for (p, id) in batch.iter().zip(ids) {
                    out.insert((*p).clone(), Some(id));
                }
                continue;
            }
            // One file that cannot be read stops the whole call: they are asked one at a time, and the ones that fail are marked as not plain files.
            for p in batch {
                let one = self.run(place, &["hash-object", "--", p.as_str()], None, deadline, cancel)?;
                let id = String::from_utf8_lossy(&one.stdout).trim().to_string();
                out.insert((*p).clone(), Some(if one.ok() && is_object_id(&id) { id } else { String::new() }));
            }
        }
        Ok(out)
    }

    // ---- putting a tree back ---------------------------------------------------------------------------------------------------------------------

    /// Puts back what the change from `before` to `after` made, for the files that are still as `after` left them (see the module text). `skip` says which paths something else has
    /// dealt with already (`fs.undo_run` put them back), `scratch` is a folder for temporary index files, `what` names what made the change in the words of the answer (`command`,
    /// `run`). With `dry_run` nothing is written and the answer says what would be done.
    #[allow(clippy::too_many_arguments)]
    pub fn restore(&self, place: &Place, guard: &PathGuard, scratch: &Path, before: &str, after: &str, skip: &dyn Fn(&str) -> bool, what: &str, dry_run: bool, deadline: Instant, cancel: &AtomicBool) -> Result<Restored, Stop> {
        let changes = self.changes(place, before, after, deadline, cancel)?;
        let mut done = Restored::default();
        let mut todo: Vec<&Change> = Vec::new();
        let mut held_paths: Vec<PathBuf> = Vec::new();
        for c in &changes {
            if skip(&c.path) {
                continue;
            }
            done.considered.push(c.path.clone());
            let unsafe_name = c.path.is_empty() || c.path.starts_with('/') || c.path.contains(['\\', ':', '\0']) || c.path.split('/').any(|s| s.is_empty() || s == ".." || s == "." || s.eq_ignore_ascii_case(".git"));
            if unsafe_name {
                done.left.push((c.path.clone(), "not a path inside the folder".into()));
                continue;
            }
            // The guard decides as it does for any write: inside a folder Aqua may use, not Aqua's own files, not a name that decides what other programs run.
            let full = place.work_tree.join(&c.path);
            match guard.resolve(&full.to_string_lossy(), Op::Write) {
                Ok(r) if within(&r.path, &place.work_tree) || canonical(&place.work_tree).map(|root| within(&r.path, &root)).unwrap_or(false) => {
                    held_paths.push(r.path);
                    todo.push(c);
                }
                Ok(_) => done.left.push((c.path.clone(), "outside the folder".into())),
                Err(e) => done.left.push((c.path.clone(), format!("not allowed: {}", e.message))),
            }
        }
        if todo.is_empty() {
            return Ok(done);
        }
        // One writer at a time for each file, as for the file tools: a file tool that is changing one of these files finishes first, and the compare and the write below are not split by it.
        let held: Vec<&Path> = held_paths.iter().map(|p| p.as_path()).collect();
        let _held = crate::fs::locks::lock(&held).map_err(|e| Stop::Failed(e.message))?;
        let paths: Vec<String> = todo.iter().map(|c| c.path.clone()).collect();
        let stamp = format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        let before_index = scratch.join(format!("aqua-restore-before-{stamp}"));
        // What every file is now, as git would hash it: compared with what the command left (`new_sha`) and what it found (`old_sha`).
        let now = self.blobs_now(place, &paths, deadline, cancel)?;
        let mut put_back: Vec<&Change> = Vec::new();
        let mut take_away: Vec<&Change> = Vec::new();
        for c in &todo {
            let is = now.get(&c.path).cloned().flatten();
            let as_left = is.as_deref() == Some(c.new_sha.as_str());
            let as_found = is.as_deref() == Some(c.old_sha.as_str());
            match c.status {
                // The command made it: it goes only when it is still what the command made.
                'A' => match is {
                    None => done.already.push(c.path.clone()),
                    Some(_) if as_left => take_away.push(c),
                    Some(_) => done.left.push((c.path.clone(), format!("changed after the {what}, so it was left"))),
                },
                // The command took it away: it comes back when nothing is there.
                'D' => match is {
                    None => put_back.push(c),
                    Some(_) if as_found => done.already.push(c.path.clone()),
                    Some(_) => done.left.push((c.path.clone(), "a different file is there now, so it was left".into())),
                },
                // The command changed it: it is put back when it is still as the command left it.
                'M' => match is {
                    None => done.left.push((c.path.clone(), "gone since, so it was left".into())),
                    Some(_) if as_found => done.already.push(c.path.clone()),
                    Some(_) if as_left => put_back.push(c),
                    Some(_) => done.left.push((c.path.clone(), format!("changed after the {what}, so it was left"))),
                },
                other => done.left.push((c.path.clone(), format!("the kind of thing it is changed ({other}); put it back by hand"))),
            }
        }
        if dry_run {
            done.restored = put_back.iter().map(|c| c.path.clone()).collect();
            done.removed = take_away.iter().map(|c| c.path.clone()).collect();
            return Ok(done);
        }
        // The files the command made are taken away; one that is locked is tried again a few times.
        for c in &take_away {
            let full = place.work_tree.join(&c.path);
            let mut last = None;
            for attempt in 0..5 {
                match std::fs::remove_file(&full) {
                    Ok(()) => {
                        last = None;
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        last = None;
                        break;
                    }
                    Err(e) => {
                        last = Some(e);
                        if attempt < 4 {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                }
            }
            match last {
                None => done.removed.push(c.path.clone()),
                Some(e) => done.left.push((c.path.clone(), format!("could not be removed ({e}): another program may be using it"))),
            }
        }
        // The files that are put back come out of an index made from the tree before the command, so that they are written with the same conversions as `git checkout` would.
        if !put_back.is_empty() {
            let at = place.with_index(before_index.clone());
            let _ = std::fs::remove_file(&before_index);
            let result = (|| -> Result<(), Stop> {
                self.must(&at, &["read-tree", before], None, deadline, cancel)?;
                let names: Vec<String> = put_back.iter().map(|c| c.path.clone()).collect();
                let wanted: HashMap<&str, &str> = put_back.iter().map(|c| (c.path.as_str(), c.old_sha.as_str())).collect();
                let mut remaining = names.clone();
                for attempt in 0..5 {
                    let mut input: Vec<u8> = Vec::new();
                    for p in &remaining {
                        input.extend_from_slice(p.as_bytes());
                        input.push(0);
                    }
                    let _ = self.run(&at, &["checkout-index", "-f", "-z", "--stdin"], Some(input), deadline, cancel)?;
                    // What is not as it should be after the call is tried again (a file a program had open).
                    let written = self.blobs_now(place, &remaining, deadline, cancel)?;
                    remaining.retain(|p| written.get(p).cloned().flatten().as_deref() != wanted.get(p.as_str()).copied());
                    if remaining.is_empty() {
                        break;
                    }
                    if attempt < 4 {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                for p in names {
                    if remaining.contains(&p) {
                        done.left.push((p, "could not be written: another program may be using it".into()));
                    } else {
                        done.restored.push(p);
                    }
                }
                Ok(())
            })();
            let _ = std::fs::remove_file(&before_index);
            result?;
        }
        Ok(done)
    }

    // ---- keeping a tree --------------------------------------------------------------------------------------------------------------------------

    /// A commit that holds `tree` (and follows `parent`), so that a reference can keep the tree. The identity is Aqua's own; nothing is signed.
    pub fn commit_tree(&self, place: &Place, tree: &str, parent: Option<&str>, message: &str, deadline: Instant, cancel: &AtomicBool) -> Result<String, Stop> {
        let mut args: Vec<String> = vec!["-c".into(), "user.name=Aqua".into(), "-c".into(), "user.email=aqua@localhost".into(), "commit-tree".into(), tree.to_string()];
        if let Some(p) = parent {
            args.extend(["-p".into(), p.to_string()]);
        }
        args.extend(["-m".into(), message.chars().filter(|c| !c.is_control() || *c == '\n').take(300).collect()]);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let r = self.must(place, &refs, None, deadline, cancel)?;
        let id = String::from_utf8_lossy(&r.stdout).trim().to_string();
        if is_object_id(&id) {
            Ok(id)
        } else {
            Err(Stop::Failed("git commit-tree gave no commit".into()))
        }
    }

    /// Points `name` (a reference of Aqua's own, `refs/aqua/...`) at `id`.
    pub fn set_ref(&self, place: &Place, name: &str, id: &str, deadline: Instant, cancel: &AtomicBool) -> Result<(), Stop> {
        if !is_aqua_ref(name) || !is_object_id(id) {
            return Err(Stop::Failed("not a reference of Aqua's".into()));
        }
        self.must(place, &["update-ref", name, id], None, deadline, cancel).map(|_| ())
    }

    /// The commit a reference of Aqua's own points at, or `None`.
    pub fn ref_target(&self, place: &Place, name: &str, deadline: Instant, cancel: &AtomicBool) -> Option<String> {
        if !is_aqua_ref(name) {
            return None;
        }
        let r = self.run(place, &["rev-parse", "--verify", "-q", &format!("{name}^{{commit}}")], None, deadline, cancel).ok()?;
        let id = String::from_utf8_lossy(&r.stdout).trim().to_string();
        (r.ok() && is_object_id(&id)).then_some(id)
    }

    /// The first parent of a commit.
    pub fn parent_of(&self, place: &Place, commit: &str, deadline: Instant, cancel: &AtomicBool) -> Option<String> {
        if !is_object_id(commit) {
            return None;
        }
        let r = self.run(place, &["rev-parse", "--verify", "-q", &format!("{commit}^1")], None, deadline, cancel).ok()?;
        let id = String::from_utf8_lossy(&r.stdout).trim().to_string();
        (r.ok() && is_object_id(&id)).then_some(id)
    }

    /// The tree of a commit.
    pub fn tree_of(&self, place: &Place, commit: &str, deadline: Instant, cancel: &AtomicBool) -> Option<String> {
        if !is_object_id(commit) {
            return None;
        }
        let r = self.run(place, &["rev-parse", "--verify", "-q", &format!("{commit}^{{tree}}")], None, deadline, cancel).ok()?;
        let id = String::from_utf8_lossy(&r.stdout).trim().to_string();
        (r.ok() && is_object_id(&id)).then_some(id)
    }

    /// The references of Aqua's own under `prefix` with when each was made (seconds since 1970), oldest first.
    pub fn refs_under(&self, place: &Place, prefix: &str, deadline: Instant, cancel: &AtomicBool) -> Vec<(String, u64)> {
        if !is_aqua_ref(prefix.trim_end_matches('/')) {
            return Vec::new();
        }
        let Ok(r) = self.run(place, &["for-each-ref", "--format=%(refname) %(committerdate:unix)", prefix], None, deadline, cancel) else { return Vec::new() };
        let mut v: Vec<(String, u64)> = String::from_utf8_lossy(&r.stdout)
            .lines()
            .filter_map(|l| {
                let (name, when) = l.rsplit_once(' ')?;
                is_aqua_ref(name).then(|| (name.to_string(), when.trim().parse().unwrap_or(0)))
            })
            .collect();
        v.sort_by_key(|(_, t)| *t);
        v
    }

    /// Removes a reference of Aqua's own (not what it pointed at: git takes unreachable objects away when its own clean-up runs, not before).
    pub fn delete_ref(&self, place: &Place, name: &str, deadline: Instant, cancel: &AtomicBool) {
        if is_aqua_ref(name) {
            let _ = self.run(place, &["update-ref", "-d", name], None, deadline, cancel);
        }
    }
}

/// Why a capture was given up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    TooManyFiles(usize),
    TooSlow,
    Cancelled,
    Failed(String),
}

impl From<Stop> for Refused {
    fn from(s: Stop) -> Self {
        match s {
            Stop::Cancelled => Refused::Cancelled,
            Stop::TimedOut => Refused::TooSlow,
            Stop::Failed(why) => Refused::Failed(why),
        }
    }
}

impl Refused {
    pub fn words(&self) -> String {
        match self {
            Refused::TooManyFiles(n) => format!("it has {n} files to look at, more than {MAX_FILES}"),
            Refused::TooSlow => format!("it took more than {CAPTURE_SECONDS} seconds"),
            Refused::Cancelled => "it was stopped".to_string(),
            Refused::Failed(why) => why.clone(),
        }
    }
}

/// The rule of which paths of a folder may be in a tree, as a function: not `.git`, not a credential folder, not a name the guard calls a secret, not Aqua's own files, not a build or
/// package folder (`target`, `node_modules`) when the caller says so. `root` is the folder.
pub fn allow_rule(guard: &PathGuard, root: &Path, skip_build_folders: bool) -> impl Fn(&str) -> bool {
    // Aqua's own files that sit inside the folder, as paths relative to it.
    let protected: Vec<Vec<String>> = guard
        .protected()
        .iter()
        .filter(|p| within(p, root) && !within(root, p))
        .map(|p| p.strip_prefix(root).map(|r| r.components().map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase()).collect()).unwrap_or_default())
        .filter(|v: &Vec<String>| !v.is_empty())
        .collect();
    let sensitive: Vec<String> = guard.config().sensitive_names.clone();
    move |rel: &str| {
        let lower: Vec<String> = rel.split('/').map(|c| c.to_ascii_lowercase()).collect();
        if protected.iter().any(|p| lower.len() >= p.len() && lower[..p.len()] == p[..]) {
            return false;
        }
        if lower.iter().any(|c| matches!(c.as_str(), ".ssh" | ".aws" | ".gnupg" | ".kube" | ".azure" | ".docker" | ".aqua_sandbox")) || lower.windows(2).any(|w| w[0] == ".config" && w[1] == "gcloud") {
            return false;
        }
        if skip_build_folders && lower[..lower.len().saturating_sub(1)].iter().any(|c| matches!(c.as_str(), "target" | "node_modules")) {
            return false;
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        !sensitive.iter().any(|p| crate::fs::guard::wildcard(p, name))
    }
}

/// `names` in the order they come, as one line for a result: `M src/a.rs, A out.txt, D old.txt`, at most `max` of them and then `and N more`.
pub fn list_changes(changes: &[Change], max: usize) -> String {
    let shown: Vec<String> = changes.iter().take(max).map(|c| format!("{} {}", c.status, c.path)).collect();
    let more = if changes.len() > max { format!(", and {} more", changes.len() - max) } else { String::new() };
    format!("{}{more}", shown.join(", "))
}

/// The name of a temporary index file of Aqua's in a git folder: it says whose it is, so that a crash's leftovers can be told from the repository's own files.
pub fn scratch_name(tag: &str) -> String {
    format!("aqua-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FsConfig;

    #[test]
    fn only_a_reference_aqua_made_and_a_whole_object_id_are_ever_given_to_git() {
        let id = "a".repeat(40);
        assert!(is_object_id(&id) && is_object_id(&"0123456789abcdef".repeat(4)));
        for bad in ["", "abc", &"g".repeat(40), &format!("{id}\n"), &format!("--{}", &id[2..]), &"a".repeat(41)] {
            assert!(!is_object_id(bad), "{bad:?}");
        }
        assert!(is_aqua_ref("refs/aqua/journal/run-1/jr_1a_2"));
        for bad in ["refs/heads/main", "refs/aqua/../heads/main", "refs/aqua//x", "refs/aqua/x/", "refs/aqua/x.lock", "refs/aqua/x y", "refs/aqua/x;rm", "-refs/aqua/x", "refs/aqua/x."] {
            assert!(!is_aqua_ref(bad), "{bad:?}");
        }
    }

    #[test]
    fn paths_go_to_a_command_line_in_batches_that_keep_it_short() {
        let paths: Vec<String> = (0..250).map(|i| format!("src/folder/file{i}.rs")).collect();
        let batches = path_batches(&paths, 100, 12_000);
        assert_eq!(batches.iter().map(|b| b.len()).sum::<usize>(), 250, "every path is in a batch");
        assert!(batches.iter().all(|b| b.len() <= 100 && b.iter().map(|p| p.len() + 1).sum::<usize>() <= 12_000));
        let long: Vec<String> = (0..10).map(|i| format!("{}{i}", "x".repeat(5_000))).collect();
        let by_size = path_batches(&long, 100, 12_000);
        assert!(by_size.len() >= 5 && by_size.iter().all(|b| b.len() <= 2), "five thousand characters of path fill a batch quickly: {}", by_size.len());
        assert!(path_batches(&[], 100, 12_000).is_empty());
    }

    #[test]
    fn the_changes_of_a_command_are_listed_in_one_line_with_a_count_of_the_rest() {
        let c = |s: char, p: &str| Change { status: s, path: p.to_string(), old_sha: String::new(), new_sha: String::new() };
        let few = vec![c('M', "a.rs"), c('A', "b.rs"), c('D', "c.rs")];
        assert_eq!(list_changes(&few, 20), "M a.rs, A b.rs, D c.rs");
        let many: Vec<Change> = (0..25).map(|i| c('A', &format!("f{i}"))).collect();
        let line = list_changes(&many, 20);
        assert!(line.starts_with("A f0, A f1") && line.ends_with("A f19, and 5 more"), "{line}");
    }

    #[test]
    fn what_may_be_in_a_tree_leaves_out_aquas_own_files_credentials_secrets_and_git() {
        let dir = std::env::temp_dir().join(format!("aqua_allow_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws/data/vault")).unwrap();
        let root = canonical(&dir.join("ws")).unwrap();
        let guard = PathGuard::new(FsConfig::with_workspace(root.to_string_lossy().to_string()), vec![root.join("data")]).unwrap();
        let allow = allow_rule(&guard, &root, true);
        for fine in ["src/main.rs", "README.md", "docs/target.md", "a/b/c/d.txt", "data2/x.txt"] {
            assert!(allow(fine), "{fine}");
        }
        for left_out in ["data/vault/vault.db", "data/x.txt", ".ssh/id_rsa", "home/.aws/credentials", "x/.config/gcloud/a.json", ".env", "cfg/.env.local", "server.pem", "target/debug/app.exe", "web/node_modules/x/index.js", ".aqua_sandbox/run_1/out.txt"] {
            assert!(!allow(left_out), "{left_out}");
        }
        // Without the build folders in the rule (the journal): `target` is only left out by the repository's own ignore rules.
        let journal_rule = allow_rule(&guard, &root, false);
        assert!(journal_rule("target/debug/app.exe") && !journal_rule(".env") && !journal_rule("data/x.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

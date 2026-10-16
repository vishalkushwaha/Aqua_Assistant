//! Worktrees for a coding run (card OA7-05, TM-13).
//!
//! A coding run that works on a branch of its own, in a second working folder of the same repository, does not touch what the person has open in the first one, and two runs do not meet in
//! one folder. `git.worktree_add` makes such a folder under `<the repository's folder>/.aqua/worktrees/<name>` (a folder Aqua owns, with a `.gitignore` of its own so that the repository
//! does not see it), `git.worktree_list` says what is there and `git.worktree_remove` takes one away.
//!
//! The rules, all of them kept by the code and not by the model:
//! * Only the main working folder of a repository makes worktrees, and only when that folder is inside a folder Aqua may write. The folder is a plain folder: a link on the way refuses the call.
//! * Aqua records what it made (`data/tools/git_worktrees.json`). **Only a recorded worktree is ever removed**, by its name and never by a path, and only after git itself says the folder
//!   is a worktree of this repository. The main working folder and a worktree Aqua did not make are never removed by these tools.
//! * A worktree with changes is not removed unless the call says `force`; then the changes to tracked files are kept first as a backup reference (as `git.discard` does) and what is not
//!   tracked is lost, and the question says so.
//! * A link or a junction inside the folder is taken away on its own first (it is only the link that goes, never what it points at), and the folder is only then removed by git.
//! * No branch is ever force-deleted, no history is rewritten, nothing is reset or cleaned, and git's hooks stay off.
//! * The worktree is offered to the git tools as a repository that is not trusted: every write there asks.
//! * New behaviour: the three tools exist only with `AQUA_WORKTREES=on` (see [`Inner::worktrees_enabled`]).

use super::{args, arg_bool, arg_str, bad, discover, git_failure, Head, Inner, Repo, RepoEntry};
use crate::fs::guard::{canonical, display, same_place, wildcard, within, Access, Op};
use aqua_tool_host::{fsutil, ErrorCode, ProviderOutput, ToolError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The folder inside a folder Aqua may write, where the worktrees are.
pub const FOLDER: [&str; 2] = [".aqua", "worktrees"];
/// The file of what Aqua made, in `data/tools`.
pub const REGISTRY_FILE: &str = "git_worktrees.json";
/// The most worktrees Aqua keeps at once for all the repositories.
pub const MAX_WORKTREES: usize = 20;
/// The most entries of a folder looked at for links before it is removed; a folder with more is not removed by this tool.
pub const MAX_ENTRIES_LOOKED_AT: usize = 1_000_000;
/// The room that is left on the disk after a worktree is made, on top of the size of its files.
const DISK_MARGIN: u64 = 256 * 1024 * 1024;
/// How many times a removal is tried again when a program holds a file of the folder, 100 ms apart.
const REMOVE_TRIES: usize = 50;

/// One worktree Aqua made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub name: String,
    /// The alias the git tools use for it (`repo` of a call).
    pub alias: String,
    /// The main working folder of the repository it belongs to.
    pub repo: String,
    pub path: String,
    pub branch: String,
    /// The branch was made with the worktree (and may be deleted with it, when it is merged).
    pub created_branch: bool,
    /// The commit it began at, short.
    pub base: String,
    /// Seconds since 1970.
    pub created: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    #[serde(default)]
    worktrees: Vec<Record>,
}

/// What Aqua made, kept in a file of its own. The file is read again when it changes, and replaced whole (never half written) when a worktree is added or goes.
pub struct Registry {
    file: PathBuf,
    stamp: Mutex<Option<(SystemTime, u64)>>,
    cached: Mutex<Vec<Record>>,
    writing: Mutex<()>,
}

impl Registry {
    pub fn new(file: PathBuf) -> Self {
        Self { file, stamp: Mutex::new(None), cached: Mutex::new(Vec::new()), writing: Mutex::new(()) }
    }

    /// What is recorded. A file that is damaged is an error that says so (it is never replaced: the person decides).
    pub fn records(&self) -> Result<Vec<Record>, String> {
        let meta = match std::fs::metadata(&self.file) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                *self.stamp.lock().unwrap_or_else(PoisonError::into_inner) = None;
                self.cached.lock().unwrap_or_else(PoisonError::into_inner).clear();
                return Ok(Vec::new());
            }
            Err(e) => return Err(format!("{} could not be read: {e}", REGISTRY_FILE)),
        };
        let stamp = meta.modified().ok().map(|t| (t, meta.len()));
        if stamp.is_some() && *self.stamp.lock().unwrap_or_else(PoisonError::into_inner) == stamp {
            return Ok(self.cached.lock().unwrap_or_else(PoisonError::into_inner).clone());
        }
        let store = match fsutil::read_json_strict::<Store>(&self.file) {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => return Err(format!("{REGISTRY_FILE} {e}: no worktree is made or removed until it is put right or moved away")),
        };
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = store.worktrees.clone();
        *self.stamp.lock().unwrap_or_else(PoisonError::into_inner) = stamp;
        Ok(store.worktrees)
    }

    /// Changes the records and writes them. `f` sees what is in the file now.
    pub fn update<R>(&self, f: impl FnOnce(&mut Vec<Record>) -> Result<R, ToolError>) -> Result<R, ToolError> {
        let _one_at_a_time = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let mut records = self.records().map_err(|e| ToolError::new(ErrorCode::BackendError, e))?;
        let out = f(&mut records)?;
        let bytes = serde_json::to_vec_pretty(&Store { worktrees: records.clone() }).map_err(|e| ToolError::new(ErrorCode::BackendError, e.to_string()))?;
        fsutil::write_atomic_durable(&self.file, &bytes).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("{REGISTRY_FILE} could not be written: {e}")))?;
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = records;
        *self.stamp.lock().unwrap_or_else(PoisonError::into_inner) = std::fs::metadata(&self.file).ok().and_then(|m| m.modified().ok().map(|t| (t, m.len())));
        Ok(out)
    }
}

/// A name for a worktree: 1 to 40 letters, digits, `_` and `-`, beginning with a letter or a digit, and not the name of a device Windows reserves (it becomes the name of a folder).
pub fn valid_name(s: &str) -> Result<&str, ToolError> {
    let fine = !s.is_empty() && s.len() <= 40 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') && s.chars().next().is_some_and(|c| c.is_ascii_alphanumeric());
    if !fine {
        return Err(bad(format!("'{s}' is not a worktree name: 1 to 40 letters, digits, '_' or '-', beginning with a letter or a digit")));
    }
    let low = s.to_ascii_lowercase();
    let reserved = matches!(low.as_str(), "con" | "prn" | "aux" | "nul") || ((low.starts_with("com") || low.starts_with("lpt")) && low.len() == 4 && low.as_bytes()[3].is_ascii_digit());
    if reserved {
        return Err(bad(format!("'{s}' is the name of a device on Windows and cannot be the name of a folder")));
    }
    Ok(s)
}

/// A name made from a branch name (`feature/login-form` is `feature-login-form`), or none when nothing usable is left.
pub fn name_from_branch(branch: &str) -> Option<String> {
    let name: String = branch.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect();
    let name: String = name.trim_matches('-').chars().take(40).collect();
    let fine = valid_name(&name).is_ok();
    fine.then_some(name)
}

/// One worktree as `git worktree list --porcelain` prints it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listed {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub locked: bool,
    pub bare: bool,
}

/// The blocks of `git worktree list --porcelain`: `worktree <path>`, `HEAD <sha>`, `branch refs/heads/<name>` or `detached`, and `locked`, `prunable` or `bare` when they apply.
pub fn parse_porcelain(text: &str) -> Vec<Listed> {
    let mut out: Vec<Listed> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(p) = line.strip_prefix("worktree ") {
            out.push(Listed { path: p.to_string(), ..Default::default() });
        } else if let Some(cur) = out.last_mut() {
            if let Some(h) = line.strip_prefix("HEAD ") {
                cur.head = h.to_string();
            } else if let Some(b) = line.strip_prefix("branch ") {
                cur.branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
            } else if line == "detached" {
                cur.detached = true;
            } else if line == "locked" || line.starts_with("locked ") {
                cur.locked = true;
            } else if line == "bare" {
                cur.bare = true;
            }
        }
    }
    out
}

/// The number of files and their size in bytes from `git ls-tree -r -l -z`: `<mode> <type> <sha> <size>\t<path>` and a NUL. A folder of a submodule has no size (`-`) and is not counted.
pub fn sum_tree(text: &str) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    for entry in text.split('\0') {
        let Some((meta, _path)) = entry.split_once('\t') else { continue };
        let mut it = meta.split_whitespace();
        let (_mode, kind, _sha, size) = (it.next(), it.next(), it.next(), it.next());
        if kind == Some("blob") {
            files += 1;
            bytes += size.and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        }
    }
    (files, bytes)
}

/// The links and junctions inside `dir`, found without going into any of them. An error says the folder is too big to look through (more than `max` entries) or cannot be read: it is then
/// not removed, because a folder that was not looked through may hold a link to somewhere else.
pub fn links_in(dir: &Path, max: usize) -> Result<Vec<PathBuf>, String> {
    let mut links = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    let mut seen = 0usize;
    while let Some(d) = stack.pop() {
        let entries = std::fs::read_dir(&d).map_err(|e| format!("{} could not be read ({e})", display(&d)))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("{} could not be read ({e})", display(&d)))?;
            seen += 1;
            if seen > max {
                return Err(format!("it holds more than {max} files and folders"));
            }
            let meta = std::fs::symlink_metadata(entry.path()).map_err(|e| format!("{} could not be read ({e})", display(&entry.path())))?;
            if fsutil::is_link(&meta) {
                links.push(entry.path());
            } else if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(links)
}

/// Takes away the link (a symbolic link or a junction) at `link`, and only the link: what it points at is not touched. Refuses anything that is not a link.
pub fn unlink(link: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(link)?;
    if !fsutil::is_link(&meta) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a link"));
    }
    // A file link goes with remove_file, a link to a folder (a junction) with remove_dir; both take only the link.
    std::fs::remove_file(link).or_else(|_| std::fs::remove_dir(link))
}

/// The folder that holds the worktrees of the repository whose main working folder is `root`.
pub fn owned_folder(root: &Path) -> PathBuf {
    root.join(FOLDER[0]).join(FOLDER[1])
}

/// Makes `<repository>/.aqua/worktrees` when it is not there, as plain folders, and gives it a `.gitignore` that keeps what is inside out of the repository. A link anywhere on the way, or a
/// file in the place of a folder, refuses.
fn ensure_folder(root: &Path) -> Result<PathBuf, ToolError> {
    let aqua = root.join(FOLDER[0]);
    let owned = aqua.join(FOLDER[1]);
    for d in [&aqua, &owned] {
        match std::fs::symlink_metadata(d) {
            Ok(m) if fsutil::is_link(&m) => return Err(ToolError::new(ErrorCode::PermissionDenied, format!("{} is a link: no worktree is made through a link", display(d)))),
            Ok(m) if !m.is_dir() => return Err(ToolError::new(ErrorCode::AlreadyExists, format!("{} is a file, not a folder: no worktree is made there", display(d)))),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(d).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("{} could not be made: {e}", display(d))))?,
            Err(e) => return Err(ToolError::new(ErrorCode::BackendError, format!("{} could not be read: {e}", display(d)))),
        }
    }
    let ignore = owned.join(".gitignore");
    if std::fs::symlink_metadata(&ignore).is_err() {
        std::fs::write(&ignore, "# Made by Aqua: the working folders of the worktrees it made for coding runs. They are not part of the repository.\n*\n").map_err(|e| ToolError::new(ErrorCode::BackendError, format!("{} could not be written: {e}", display(&ignore))))?;
    }
    Ok(owned)
}

/// The free space a worktree of `bytes` of files needs: a fifth more than the files (the disk rounds each file up to a cluster) and the room that is always left.
pub fn room_needed(bytes: u64) -> u64 {
    bytes.saturating_mul(12) / 10 + DISK_MARGIN
}

fn megabytes(b: u64) -> String {
    format!("{:.1} MB", b as f64 / 1_048_576.0)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Everything a call to make a worktree has worked out, before anything is made.
struct AddPlan {
    name: String,
    branch: String,
    new_branch: bool,
    /// The commit the worktree begins at (a whole object id).
    start: String,
    root: PathBuf,
    path: PathBuf,
    files: usize,
    bytes: u64,
    cut: bool,
}

/// What a call to remove one has worked out.
struct RemovePlan {
    rec: Record,
    /// The worktree as a repository; `None` when its folder is gone.
    wt: Option<Arc<Repo>>,
    /// The paths git lists as changed in it.
    changed: Vec<String>,
}

impl Inner {
    /// Are the worktree tools on (the setting `AQUA_WORKTREES`, asked each time)?
    pub(super) fn worktrees_enabled(&self) -> bool {
        self.worktrees_on.as_ref().is_some_and(|on| on())
    }

    fn need_worktrees(&self) -> Result<(), ToolError> {
        if self.worktrees_enabled() {
            Ok(())
        } else {
            Err(ToolError::new(ErrorCode::Unavailable, "the worktree tools are off (AQUA_WORKTREES=on turns them on)"))
        }
    }

    fn records(&self) -> Result<Vec<Record>, ToolError> {
        self.registry.records().map_err(|e| ToolError::new(ErrorCode::BackendError, e))
    }

    /// The folder that is writable and holds `repo_root`: the innermost one.
    fn writable_root_of(&self, repo_root: &Path) -> Option<PathBuf> {
        self.guard.roots().into_iter().filter(|(_, p, access)| *access == Access::Rw && within(repo_root, p)).map(|(_, p, _)| p).max_by_key(|p| p.components().count())
    }

    /// A worktree as a repository the git tools can be pointed at: not trusted, the main repository's protected branches and author, no hooks.
    pub(super) fn worktree_repo(&self, rec: &Record, parent: &Repo) -> Option<Arc<Repo>> {
        let root = canonical(Path::new(&rec.path)).ok()?;
        if !root.join(".git").exists() {
            return None;
        }
        let entry = RepoEntry { alias: rec.alias.clone(), path: display(&root), trusted: false, protected_branches: parent.entry.protected_branches.clone(), author_name: parent.entry.author_name.clone(), author_email: parent.entry.author_email.clone(), hooks: super::HooksMode::Off, attribution: parent.entry.attribution.clone() };
        Some(Arc::new(Repo { entry, root, found: true, worktree_of: Some(parent.root.clone()) }))
    }

    /// Adds the worktrees Aqua made to the repositories of a snapshot, when the tools are on. A record whose folder is gone is not offered (`git.worktree_list` still says it).
    pub(super) fn add_worktree_repos(&self, all: &mut Vec<Arc<Repo>>) {
        if !self.worktrees_enabled() {
            return;
        }
        let Ok(records) = self.registry.records() else { return };
        for rec in records {
            let Some(parent) = all.iter().find(|r| r.worktree_of.is_none() && same_place(&r.root, Path::new(&rec.repo))).cloned() else { continue };
            let Some(mut repo) = self.worktree_repo(&rec, &parent) else { continue };
            let alias = discover::unique_alias(&discover::sanitize_alias(&rec.alias), &|a: &str| all.iter().any(|r| r.entry.alias.eq_ignore_ascii_case(a)));
            if let Some(r) = Arc::get_mut(&mut repo) {
                r.entry.alias = alias;
            }
            all.push(repo);
        }
    }

    /// `git worktree list --porcelain` of the repository.
    fn listed(&self, repo: &Repo, cancel: &AtomicBool) -> Result<Vec<Listed>, ToolError> {
        let r = self.ok(repo, &["worktree", "list", "--porcelain"], false, 64, 30, cancel)?;
        Ok(parse_porcelain(&r.stdout))
    }

    fn branch_exists(&self, repo: &Repo, branch: &str, cancel: &AtomicBool) -> Result<bool, ToolError> {
        let r = self.git(repo, &["rev-parse", "-q", "--verify", &format!("refs/heads/{branch}")], false, 4, 15, cancel)?;
        match r.code {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(git_failure(&r.stderr, &r.stdout, repo)),
        }
    }

    /// The commit a revision names, as a whole object id.
    fn commit_of(&self, repo: &Repo, rev: &str, cancel: &AtomicBool) -> Result<String, ToolError> {
        let r = self.git(repo, &["rev-parse", "-q", "--verify", &format!("{rev}^{{commit}}")], false, 4, 15, cancel)?;
        let sha = r.stdout.trim().to_string();
        if r.code == Some(0) && super::capture::is_object_id(&sha) {
            Ok(sha)
        } else {
            Err(ToolError::new(ErrorCode::NotFound, format!("'{rev}' is not a commit of {} (repo {})", repo.entry.alias, repo.entry.alias)))
        }
    }

    /// Checks everything a new worktree needs and says what it would be, without making anything. `a`: `name`, `branch`, `start_point`.
    fn plan_add(&self, repo: &Repo, a: &Value, cancel: &AtomicBool) -> Result<AddPlan, ToolError> {
        self.need_worktrees()?;
        if let Some(parent) = &repo.worktree_of {
            return Err(bad(format!("{} is itself a worktree (of {}): make worktrees from the main repository", repo.entry.alias, display(parent))));
        }
        if !repo.root.join(".git").is_dir() {
            return Err(bad(format!("{} is not the main working folder of a repository (its .git is not a folder): worktrees are made from the main one", repo.entry.alias)));
        }
        // The worktrees are made inside the repository's own folder (`.aqua/worktrees`), which has to be one Aqua may write to.
        if self.writable_root_of(&repo.root).is_none() {
            let roots: Vec<String> = self.guard.roots().into_iter().filter(|(_, _, access)| *access == Access::Rw).map(|(alias, path, _)| format!("@{alias} = {}", display(&path))).collect();
            return Err(ToolError::new(
                ErrorCode::PermissionDenied,
                format!("{} ({}) is not inside a folder Aqua may write to ({}): no worktree is made. A worktree is made under .aqua/worktrees inside the repository's own folder.", repo.entry.alias, display(&repo.root), roots.join("; ")),
            ));
        }
        let root = repo.root.clone();
        let records = self.records()?;
        if records.len() >= MAX_WORKTREES {
            return Err(ToolError::new(ErrorCode::ResourceLimit, format!("Aqua keeps at most {MAX_WORKTREES} worktrees: remove one with git.worktree_remove first (git.worktree_list shows them)")));
        }
        if matches!(self.head(repo), Head::Unborn(_)) {
            return Err(bad(format!("{} has no commit yet: a worktree needs a commit to begin at", repo.entry.alias)));
        }
        // The branch.
        let given = arg_str(a, "branch");
        let given_name = (!given.is_empty()).then(|| given.to_string());
        let name = match arg_str(a, "name") {
            "" => {
                let mut candidates: Vec<String> = given_name.iter().filter_map(|b| name_from_branch(b)).collect();
                candidates.extend((1..100).map(|n| format!("wt{n}")));
                candidates
                    .into_iter()
                    .find(|c| !records.iter().any(|r| r.name.eq_ignore_ascii_case(c)) && std::fs::symlink_metadata(owned_folder(&root).join(c)).is_err())
                    .ok_or_else(|| ToolError::new(ErrorCode::ResourceLimit, "no free worktree name: give name="))?
            }
            n => valid_name(n)?.to_string(),
        };
        if let Some(r) = records.iter().find(|r| r.name.eq_ignore_ascii_case(&name)) {
            return Err(ToolError::new(ErrorCode::AlreadyExists, format!("a worktree named '{name}' exists (of {}): give another name, or remove it with git.worktree_remove", display(Path::new(&r.repo)))));
        }
        let branch = given_name.clone().unwrap_or_else(|| format!("aqua/{name}"));
        args::branch_name(&branch)?;
        if repo.entry.protected_branches.iter().any(|p| wildcard(p, &branch)) {
            return Err(ToolError::new(ErrorCode::ProtectedRef, format!("'{branch}' is a protected branch of {}: a worktree is not made on one. Leave branch out (a new branch aqua/<name> is made) or give another name.", repo.entry.alias)));
        }
        let exists = self.branch_exists(repo, &branch, cancel)?;
        let start_point = arg_str(a, "start_point");
        if exists && given_name.is_none() {
            return Err(ToolError::new(ErrorCode::AlreadyExists, format!("the branch {branch} exists: give another name, or branch={branch} to work on it")));
        }
        if exists && !start_point.is_empty() {
            return Err(bad(format!("start_point is for a branch that is made; {branch} exists and the worktree begins at its tip")));
        }
        if exists {
            if let Some(there) = self.listed(repo, cancel)?.into_iter().find(|w| w.branch.as_deref() == Some(branch.as_str())) {
                return Err(ToolError::new(ErrorCode::Conflict, format!("{branch} is checked out in {} already: a branch is in one working folder at a time", there.path)));
            }
        }
        let start = if exists { self.commit_of(repo, &format!("refs/heads/{branch}"), cancel)? } else { self.commit_of(repo, if start_point.is_empty() { "HEAD" } else { start_point }, cancel)? };
        // The folder.
        let owned = owned_folder(&root);
        for d in [root.join(FOLDER[0]), owned.clone()] {
            if let Ok(m) = std::fs::symlink_metadata(&d) {
                if fsutil::is_link(&m) {
                    return Err(ToolError::new(ErrorCode::PermissionDenied, format!("{} is a link: no worktree is made through a link", display(&d))));
                }
            }
        }
        let path = owned.join(&name);
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(ToolError::new(ErrorCode::AlreadyExists, format!("{} exists already: give another name", display(&path))));
        }
        // The guard's own answer for where the folder would be: inside the folders Aqua may write, not Aqua's own, and not somewhere a link leads away.
        let resolved = self.guard.resolve(&path.to_string_lossy(), Op::Write)?;
        if !same_place(&resolved.path, &path) {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("{} leads to {}: no worktree is made through a link", display(&path), display(&resolved.path))));
        }
        // What it costs on the disk, and whether the disk has the room.
        let tree = self.git(repo, &["ls-tree", "-r", "-l", "-z", &start], false, 4096, 30, cancel)?;
        if tree.code != Some(0) {
            return Err(git_failure(&tree.stderr, &tree.stdout, repo));
        }
        let (files, bytes) = sum_tree(&tree.stdout);
        if let Some(free) = super::capture::free_bytes(&root) {
            if free < room_needed(bytes) {
                return Err(ToolError::new(
                    ErrorCode::ResourceLimit,
                    format!("a worktree of {} needs about {} for {files} files{}, and the disk that holds {} has {} free (it keeps {} spare): no worktree is made", repo.entry.alias, megabytes(bytes), if tree.truncated { " or more" } else { "" }, display(&root), megabytes(free), megabytes(DISK_MARGIN)),
                ));
            }
        }
        Ok(AddPlan { name, branch, new_branch: !exists, start, root, path, files, bytes, cut: tree.truncated })
    }

    /// The line of the question for `git.worktree_add`: the checks are made now, so that a call that cannot be made is refused before the person is asked.
    pub(super) fn worktree_add_summary(&self, repo: &Repo, a: &Value) -> Result<String, ToolError> {
        let cancel = AtomicBool::new(false);
        let p = self.plan_add(repo, a, &cancel)?;
        Ok(format!(
            "Make the worktree {} of {} at {} on {} branch {} (about {} files, {} copied to the disk{})",
            p.name,
            repo.entry.alias,
            display(&p.path),
            if p.new_branch { "a new" } else { "the existing" },
            p.branch,
            p.files,
            megabytes(p.bytes),
            if p.cut { " or more" } else { "" }
        ))
    }

    pub(super) fn worktree_add(&self, repo: &Repo, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let plan = self.plan_add(repo, a, cancel)?;
        ensure_folder(&plan.root)?;
        let path_text = plan.path.to_string_lossy().to_string();
        let mut v: Vec<&str> = vec!["worktree", "add"];
        if plan.new_branch {
            v.extend(["-b", plan.branch.as_str(), path_text.as_str(), plan.start.as_str()]);
        } else {
            v.extend([path_text.as_str(), plan.branch.as_str()]);
        }
        let r = self.git(repo, &v, false, 32, 300, cancel)?;
        if r.code != Some(0) {
            return Err(git_failure(&r.stderr, &r.stdout, repo));
        }
        if !plan.path.join(".git").is_file() {
            return Err(ToolError::new(ErrorCode::BackendError, format!("git said the worktree was made, but {} is not one: nothing was recorded", display(&plan.path))));
        }
        let taken_aliases: Vec<String> = self.snapshot(false).iter().map(|r| r.entry.alias.to_ascii_lowercase()).collect();
        let alias = discover::unique_alias(&discover::sanitize_alias(&format!("{}-{}", repo.entry.alias, plan.name)), &|c: &str| taken_aliases.contains(&c.to_ascii_lowercase()));
        let rec = Record {
            name: plan.name.clone(),
            alias: alias.clone(),
            repo: display(&repo.root),
            path: display(&canonical(&plan.path).unwrap_or(plan.path.clone())),
            branch: plan.branch.clone(),
            created_branch: plan.new_branch,
            base: plan.start.chars().take(8).collect(),
            created: now(),
        };
        let recorded = rec.clone();
        self.registry.update(|records| {
            if records.iter().any(|r| r.name.eq_ignore_ascii_case(&recorded.name)) {
                return Err(ToolError::new(ErrorCode::AlreadyExists, format!("a worktree named '{}' was recorded while this one was made; the folder {} is a worktree git knows, and nothing here removes it", recorded.name, recorded.path)));
            }
            records.push(recorded);
            Ok(())
        })?;
        Ok(ProviderOutput {
            data: json!({"repo": repo.entry.alias, "name": rec.name, "alias": alias, "path": rec.path, "branch": rec.branch, "created_branch": rec.created_branch, "base": rec.base, "files": plan.files}),
            text: format!(
                "Made the worktree {name} of {repo_alias} on {kind} branch {branch}, beginning at {base}.\nFolder: {path}\nUse repo={alias} with the git tools. Use the folder above for the paths of fs.* and for the cwd of term.run: the first folder of {repo_alias} is not touched by what is done there.\nIt is not trusted: a commit or a switch there asks, as in any repository you did not name in repos.json.\nWhen the work is done, merge the branch with git.merge from {repo_alias}, then git.worktree_remove name={name}.",
                name = rec.name,
                repo_alias = repo.entry.alias,
                kind = if rec.created_branch { "a new" } else { "the existing" },
                branch = rec.branch,
                base = rec.base,
                path = rec.path,
                alias = alias,
            ),
            ..Default::default()
        })
    }

    pub(super) fn worktree_list(&self, repo: &Repo, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        self.need_worktrees()?;
        let records: Vec<Record> = self.records()?.into_iter().filter(|r| same_place(Path::new(&r.repo), &repo.root)).collect();
        let listed = self.listed(repo, cancel)?;
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        for rec in &records {
            let wt = self.worktree_repo(rec, repo);
            let (state, changed, head) = match &wt {
                Some(w) => {
                    let (head, changed) = self.head_and_changes(w, cancel);
                    (if changed == Some(0) { "clean".to_string() } else { changed.map_or("?".to_string(), |n| format!("{n} changed")) }, changed, Some(head.label()))
                }
                None => ("folder missing".to_string(), None, None),
            };
            lines.push(format!("{}  repo={}  {}  {}  {}", rec.name, rec.alias, head.clone().unwrap_or_else(|| rec.branch.clone()), state, rec.path));
            rows.push(json!({"name": rec.name, "alias": rec.alias, "path": rec.path, "branch": rec.branch, "head": head, "state": state, "changed": changed, "created_branch": rec.created_branch, "base": rec.base, "created": rec.created}));
        }
        // What git knows besides the main folder and the ones Aqua made: said, and never touched by these tools.
        let others: Vec<&Listed> = listed.iter().skip(1).filter(|w| !records.iter().any(|r| same_place(Path::new(&r.path), Path::new(&w.path)))).collect();
        let other_rows: Vec<Value> = others.iter().map(|w| json!({"path": w.path, "branch": w.branch, "detached": w.detached, "locked": w.locked})).collect();
        let main = listed.first().map(|w| format!("main folder: {} on {}", w.path, w.branch.clone().unwrap_or_else(|| "no branch".into())));
        let mut text = if lines.is_empty() { format!("Aqua has made no worktree of {}.", repo.entry.alias) } else { format!("name  repo  where  files  folder\n{}", lines.join("\n")) };
        if let Some(m) = main {
            text.push_str(&format!("\n{m}"));
        }
        if !others.is_empty() {
            text.push_str("\nNot made by Aqua (these tools never change them):");
            for w in &others {
                text.push_str(&format!("\n  {} on {}{}", w.path, w.branch.clone().unwrap_or_else(|| "no branch".into()), if w.locked { " (locked)" } else { "" }));
            }
        }
        Ok(ProviderOutput { data: json!({"repo": repo.entry.alias, "worktrees": rows, "others": other_rows}), text, ..Default::default() })
    }

    /// Works out what removing the worktree `name` of `repo` is, and refuses what is not one Aqua made, is the main folder, is locked or is not the folder the record says.
    fn plan_remove(&self, repo: &Repo, a: &Value, cancel: &AtomicBool) -> Result<RemovePlan, ToolError> {
        self.need_worktrees()?;
        let name = valid_name(arg_str(a, "name"))?;
        let records = self.records()?;
        let Some(rec) = records.into_iter().find(|r| r.name.eq_ignore_ascii_case(name) && same_place(Path::new(&r.repo), &repo.root)) else {
            return Err(ToolError::new(ErrorCode::NotFound, format!("Aqua made no worktree named '{name}' of {}. git.worktree_list shows the ones it made; a worktree it did not make is never removed by these tools.", repo.entry.alias)));
        };
        let path = PathBuf::from(&rec.path);
        // The main working folder is never a worktree to remove, whatever a file says.
        if same_place(&path, &repo.root) || within(&repo.root, &path) || within(&path, &repo.root.join(".git")) {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("{} is the main working folder of {} (or holds it): it is never removed by these tools", display(&path), repo.entry.alias)));
        }
        // It is where Aqua puts them: `.../.aqua/worktrees/<name>`, a plain folder.
        let comps: Vec<String> = path.components().filter_map(|c| if let std::path::Component::Normal(s) = c { Some(s.to_string_lossy().to_ascii_lowercase()) } else { None }).collect();
        let n = comps.len();
        if n < 3 || comps[n - 1] != rec.name.to_ascii_lowercase() || comps[n - 2] != FOLDER[1] || comps[n - 3] != FOLDER[0] {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("{} is not under a folder .aqua/worktrees: it is not removed by these tools", display(&path))));
        }
        if std::fs::symlink_metadata(&path).is_err() {
            return Ok(RemovePlan { rec, wt: None, changed: Vec::new() });
        }
        let resolved = self.guard.resolve(&path.to_string_lossy(), Op::Write)?;
        if !same_place(&resolved.path, &path) {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("{} leads to {}: it is not removed through a link", display(&path), display(&resolved.path))));
        }
        let listed = self.listed(repo, cancel)?;
        let Some(entry) = listed.iter().skip(1).find(|w| same_place(Path::new(&w.path), &path)) else {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("git does not list {} as a worktree of {}: it is not removed", display(&path), repo.entry.alias)));
        };
        if entry.locked {
            return Err(ToolError::new(ErrorCode::Conflict, format!("{} is locked (git worktree lock): it is not removed by these tools", display(&path))));
        }
        let Some(wt) = self.worktree_repo(&rec, repo) else {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("{} has no .git: it is not removed by these tools", display(&path))));
        };
        let st = self.git(&wt, &["status", "--porcelain=v1", "--untracked-files=normal"], false, 256, 60, cancel)?;
        if st.code != Some(0) {
            return Err(git_failure(&st.stderr, &st.stdout, &wt));
        }
        let changed: Vec<String> = st.stdout.lines().filter(|l| !l.trim().is_empty()).map(|l| l.chars().skip(3).collect::<String>()).collect();
        Ok(RemovePlan { rec, wt: Some(wt), changed })
    }

    pub(super) fn worktree_remove_summary(&self, repo: &Repo, a: &Value) -> Result<String, ToolError> {
        let cancel = AtomicBool::new(false);
        let p = self.plan_remove(repo, a, &cancel)?;
        let force = arg_bool(a, "force");
        let state = if p.wt.is_none() {
            "its folder is already gone; only the record is removed".to_string()
        } else if p.changed.is_empty() {
            "it has no changes".to_string()
        } else if force {
            format!("{} file(s) have changes: changes to tracked files are kept in a backup reference, files git does not track are LOST", p.changed.len())
        } else {
            format!("{} file(s) have changes, so it will be refused unless force=true", p.changed.len())
        };
        Ok(format!("Remove the worktree {} of {} ({}): {state}{}", p.rec.name, repo.entry.alias, p.rec.path, if arg_bool(a, "delete_branch") { "; its branch is deleted if it is merged" } else { "" }))
    }

    pub(super) fn worktree_remove(&self, repo: &Repo, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let plan = self.plan_remove(repo, a, cancel)?;
        let force = arg_bool(a, "force");
        let rec = plan.rec.clone();
        let Some(wt) = plan.wt.clone() else {
            self.forget(&rec)?;
            return Ok(ProviderOutput {
                data: json!({"repo": repo.entry.alias, "name": rec.name, "removed": false, "forgotten": true}),
                text: format!("The folder of the worktree {} was already gone. Its record is removed. (git keeps a note of it until `git worktree prune` is run; that prunes every worktree whose folder is missing, so these tools do not run it.)", rec.name),
                ..Default::default()
            });
        };
        if !plan.changed.is_empty() && !force {
            let shown: Vec<&str> = plan.changed.iter().take(10).map(String::as_str).collect();
            return Err(ToolError::new(
                ErrorCode::DirtyWorktree,
                format!("the worktree {} has changes in {} file(s) ({}{}): nothing was removed. Commit them (git.commit with repo={}), or give force=true to throw them away.", rec.name, plan.changed.len(), shown.join(", "), if plan.changed.len() > 10 { ", ..." } else { "" }, rec.alias),
            )
            .with_details(json!({"files": plan.changed.iter().take(50).collect::<Vec<_>>()})));
        }
        // Changes to tracked files are kept first (a reference under refs/aqua/backup, as git.discard does).
        let backup = if !plan.changed.is_empty() { self.backup(&wt, cancel)? } else { None };
        // A file-system monitor of git that was started in the folder holds it open; stopping it is harmless when there is none.
        let _ = self.git(&wt, &["fsmonitor--daemon", "stop"], false, 4, 15, cancel);
        // A link inside the folder goes first, and only the link.
        let links = links_in(&wt.root, MAX_ENTRIES_LOOKED_AT).map_err(|why| ToolError::new(ErrorCode::ResourceLimit, format!("the worktree {} was not removed: {why}, so it cannot be told that no link in it leads to another place. Remove it yourself.", rec.name)))?;
        for link in &links {
            unlink(link).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the link {} in the worktree could not be taken away ({e}): nothing else was removed", display(link))))?;
        }
        // We have looked at the changes ourselves (and kept what is tracked), and taken the links out of the folder, which git would count as changes: git is told to go on.
        let mut tries = 0usize;
        loop {
            let r = self.git(repo, &["worktree", "remove", "--force", &wt.root.to_string_lossy()], false, 16, 120, cancel)?;
            if r.code == Some(0) {
                break;
            }
            let low = format!("{} {}", r.stderr, r.stdout).to_ascii_lowercase();
            let busy = low.contains("permission denied") || low.contains("being used by another process") || low.contains("directory not empty") || low.contains("device or resource busy") || low.contains("invalid argument");
            tries += 1;
            if !busy || tries >= REMOVE_TRIES || cancel.load(Ordering::Relaxed) {
                let mut e = git_failure(&r.stderr, &r.stdout, repo);
                e.message = format!("the worktree {} was not removed completely: {}. A program may hold a file in {}; close it and call this again.", rec.name, e.message, rec.path);
                return Err(e);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let gone = std::fs::symlink_metadata(&wt.root).is_err();
        if !gone {
            return Err(ToolError::new(ErrorCode::BackendError, format!("git said the worktree {} was removed, but {} is still there: it is left as it is", rec.name, rec.path)));
        }
        self.forget(&rec)?;
        let mut notes = Vec::new();
        if !links.is_empty() {
            notes.push(format!("{} link(s) inside it were taken away first; what they pointed at is untouched", links.len()));
        }
        if let Some(sha) = &backup {
            notes.push(format!("the changes to tracked files are kept as a backup (commit {}; see git.discard for how to get them back); files git does not track are gone", &sha[..sha.len().min(12)]));
        } else if !plan.changed.is_empty() {
            notes.push("files git does not track are gone".to_string());
        }
        // The branch goes only when it was made with the worktree, the person asked, and git itself says it is merged (-d, never -D).
        let mut branch_note = format!("The branch {} stays.", rec.branch);
        if arg_bool(a, "delete_branch") {
            if rec.created_branch {
                let d = self.git(repo, &["branch", "-d", &rec.branch], false, 8, 30, cancel)?;
                branch_note = if d.code == Some(0) { format!("The branch {} was merged and is deleted.", rec.branch) } else { format!("The branch {} stays: it has commits that are not merged into the branch you are on (git.merge first, or git.branch_delete if you do not want them).", rec.branch) };
            } else {
                branch_note = format!("The branch {} was there before the worktree and stays.", rec.branch);
            }
        }
        Ok(ProviderOutput {
            data: json!({"repo": repo.entry.alias, "name": rec.name, "removed": true, "backup": backup, "links_taken_away": links.len(), "branch": rec.branch}),
            text: format!("Removed the worktree {} ({}).{}\n{branch_note}", rec.name, rec.path, if notes.is_empty() { String::new() } else { format!(" {}.", notes.join("; ")) }),
            ..Default::default()
        })
    }

    fn forget(&self, rec: &Record) -> Result<(), ToolError> {
        self.registry.update(|records| {
            records.retain(|r| !r.name.eq_ignore_ascii_case(&rec.name));
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worktree_name_is_a_safe_folder_name() {
        for ok in ["fix-login", "a", "wt1", "A_b-2", &"x".repeat(40)] {
            assert!(valid_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "_x", "a b", "a/b", "a\\b", "..", "a.b", "con", "NUL", "com1", "lpt9", "Aux", &"x".repeat(41), "a:b", "é"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        assert!(valid_name("com10").is_ok(), "only com1 to com9 are devices");
    }

    #[test]
    fn a_name_is_made_from_a_branch_when_there_is_something_to_make_it_from() {
        assert_eq!(name_from_branch("feature/login-form").as_deref(), Some("feature-login-form"));
        assert_eq!(name_from_branch("/x/").as_deref(), Some("x"));
        assert_eq!(name_from_branch("///"), None);
        assert_eq!(name_from_branch("con"), None);
        assert_eq!(name_from_branch(&"y".repeat(60)).map(|n| n.len()), Some(40));
    }

    #[test]
    fn the_listing_of_git_is_read_with_the_main_folder_a_detached_one_and_a_locked_one() {
        let text = "worktree C:/work/main\nHEAD 1111111111111111111111111111111111111111\nbranch refs/heads/main\n\nworktree C:/work/main/.aqua/worktrees/a\nHEAD 2222222222222222222222222222222222222222\nbranch refs/heads/aqua/a\nlocked reason here\n\nworktree C:/other\nHEAD 3333333333333333333333333333333333333333\ndetached\nprunable gitdir file points to non-existent location\n";
        let l = parse_porcelain(text);
        assert_eq!(l.len(), 3);
        assert_eq!((l[0].path.as_str(), l[0].branch.as_deref(), l[0].locked), ("C:/work/main", Some("main"), false));
        assert_eq!((l[1].branch.as_deref(), l[1].locked), (Some("aqua/a"), true));
        assert!(l[2].detached && l[2].branch.is_none());
        assert!(parse_porcelain("").is_empty());
    }

    #[test]
    fn a_worktree_needs_its_files_and_a_fifth_more_and_the_room_that_is_always_left() {
        assert_eq!(room_needed(0), DISK_MARGIN);
        assert_eq!(room_needed(1000), 1200 + DISK_MARGIN);
        assert!(room_needed(u64::MAX) > room_needed(1 << 40), "a size that does not fit does not wrap round to a small one");
        assert!(room_needed(10 << 30) > 12 << 30);
    }

    #[test]
    fn the_size_of_a_tree_is_the_blobs_and_not_the_submodules() {
        let text = "100644 blob aaaa      12\tsrc/a.rs\u{0}100644 blob bbbb    4096\tdocs/b.md\u{0}160000 commit cccc       -\tvendor/sub\u{0}100755 blob dddd       0\trun.sh\u{0}";
        assert_eq!(sum_tree(text), (3, 4108));
        assert_eq!(sum_tree(""), (0, 0));
    }

    #[test]
    fn the_registry_is_a_file_that_is_replaced_whole_and_a_damaged_one_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("aqua_wt_registry_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let reg = Registry::new(dir.join(REGISTRY_FILE));
        assert!(reg.records().unwrap().is_empty());
        let rec = Record { name: "a".into(), alias: "r-a".into(), repo: "C:/r".into(), path: "C:/r/.aqua/worktrees/a".into(), branch: "aqua/a".into(), created_branch: true, base: "1234abcd".into(), created: 1 };
        reg.update(|r| {
            r.push(rec.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(reg.records().unwrap(), vec![rec.clone()]);
        // A second registry on the same file (another process, a restart) reads the same.
        assert_eq!(Registry::new(dir.join(REGISTRY_FILE)).records().unwrap(), vec![rec]);
        // A damaged file is an error and stays as it is.
        std::fs::write(dir.join(REGISTRY_FILE), "{ not json").unwrap();
        let damaged = Registry::new(dir.join(REGISTRY_FILE));
        assert!(damaged.records().unwrap_err().contains("is damaged"));
        assert!(damaged.update(|r| {
            r.clear();
            Ok(())
        })
        .is_err());
        assert_eq!(std::fs::read_to_string(dir.join(REGISTRY_FILE)).unwrap(), "{ not json");
        // A key that is not one of ours is a damaged file too.
        std::fs::write(dir.join(REGISTRY_FILE), r#"{"worktrees": [], "extra": 1}"#).unwrap();
        assert!(Registry::new(dir.join(REGISTRY_FILE)).records().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn links_are_found_without_going_into_them_and_only_a_link_is_taken_away() {
        let dir = std::env::temp_dir().join(format!("aqua_wt_links_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let outside = dir.join("outside");
        let inside = dir.join("inside");
        std::fs::create_dir_all(outside.join("deep")).unwrap();
        std::fs::create_dir_all(inside.join("sub")).unwrap();
        std::fs::write(outside.join("keep.txt"), "mine").unwrap();
        std::fs::write(outside.join("deep/also.txt"), "mine too").unwrap();
        std::fs::write(inside.join("sub/f.txt"), "x").unwrap();
        // A junction needs no right on Windows.
        // (mklink wants backslashes: no `/` in the name.)
        let link = inside.join("sub").join("link");
        let made = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(&link).arg(&outside).output().map(|o| o.status.success()).unwrap_or(false);
        assert!(made, "a junction needs no right on Windows, so it can be made here");
        if made {
            let found = links_in(&inside, 100).unwrap();
            assert_eq!(found, vec![link.clone()], "the junction is found, and what is behind it is not looked into");
            assert!(unlink(&inside.join("sub/f.txt")).is_err(), "a plain file is never taken away by unlink");
            unlink(&link).unwrap();
            assert!(std::fs::symlink_metadata(&link).is_err(), "the link is gone");
            assert_eq!(std::fs::read_to_string(outside.join("keep.txt")).unwrap(), "mine", "what it pointed at is not touched");
            assert_eq!(std::fs::read_to_string(outside.join("deep/also.txt")).unwrap(), "mine too");
        }
        assert!(links_in(&inside, 100).unwrap().is_empty());
        assert!(links_in(&inside, 1).is_err(), "a folder with more entries than the limit is not looked through");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

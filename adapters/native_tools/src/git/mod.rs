//! Git tools (`git.*`) built on the system `git.exe`.
//!
//! Repositories come from two places and must sit inside the filesystem roots. Those named in `data/tools/repos.json` are the person's: they can be trusted, carry their own protected
//! branches, and win over anything found. The others are found by looking in the folders Aqua may use (`discover.rs`, card OA7-02): a repository there works without a line of
//! configuration, is never trusted (every write asks), and has whatever its own configuration would make git run switched off, by name, on every call.
//! Every command runs with hooks, fsmonitor, external diff/textconv helpers, editors, pagers,
//! signing programs and LFS filters switched off from the command line, because a repository's
//! own config (a cloned one especially) could otherwise run code. There is no force push, no
//! history rewriting and no `--amend`; reset and discard keep a backup ref first. Before a commit
//! the lines it adds are read for secrets (`gate.rs`); the repository's own hooks run only when
//! the person turned them on, for a repository they trust (`hooks` in `repos.json`).
//!
//! Two sets of tools are off until the person switches them on (card OA7-05): the worktree tools (`worktree.rs`, `AQUA_WORKTREES=on`), which give a coding run a working folder of its own, and
//! the review tools (`review.rs`, `AQUA_GIT_REVIEW=on`), which describe merge conflicts, resolve a file by an edit the person approves, and draft the text of a pull request from the local
//! log; with it on the commit gate also refuses what still holds conflict markers. Nothing here talks to a forge.

pub mod args;
pub mod capture;
pub mod checkpoint;
pub mod conflicts;
pub mod discover;
pub mod gate;
pub mod journal;
pub mod prdraft;
mod review;
pub mod worktree;

use crate::contain::{build_env, Limits};
use crate::fs::guard::{canonical, display, wildcard, within, Access, Op, PathGuard};
use crate::term::run_command;
use aqua_tool_host::{
    Annotations, ChangePreview, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Whether the repository's own git hooks run when `git.commit` commits. Off unless the person says `run` for a repository they trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HooksMode {
    /// Hooks are switched off and the commit passes `--no-verify` (what it always did).
    #[default]
    Off,
    /// The repository's own hooks run for the commit (pre-commit, commit-msg and the like), within the same limits as the commit: no prompt for input, 60 seconds. Needs `trusted`.
    Run,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    pub alias: String,
    pub path: String,
    /// A repository you made or reviewed: routine writes (add, commit, switch) run without a prompt
    /// on branches that are not protected. Others ask every time.
    #[serde(default)]
    pub trusted: bool,
    #[serde(default = "default_protected")]
    pub protected_branches: Vec<String>,
    #[serde(default)]
    pub author_name: Option<String>,
    #[serde(default)]
    pub author_email: Option<String>,
    /// `"off"` (the default) or `"run"`: whether the repository's own hooks run for `git.commit`. `"run"` is for a repository with `trusted: true` only (a hook is code from the repository).
    #[serde(default)]
    pub hooks: HooksMode,
    /// A line added to the end of every commit message `git.commit` writes in this repository, such as `Co-Authored-By: Aqua <aqua@example.com>`. None by default.
    #[serde(default)]
    pub attribution: Option<String>,
}

impl RepoEntry {
    /// An entry the search of `discover.rs` made: the folder's name, not trusted, the usual protected branches, no hooks.
    fn found(alias: String, root: &Path) -> Self {
        Self { alias, path: root.to_string_lossy().to_string(), trusted: false, protected_branches: default_protected(), author_name: None, author_email: None, hooks: HooksMode::Off, attribution: None }
    }
}

fn default_protected() -> Vec<String> {
    ["main", "master", "trunk", "develop", "release/*", "hotfix/*", "prod*"].iter().map(|s| s.to_string()).collect()
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitConfig {
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
    #[serde(default)]
    pub git_path: Option<String>,
    /// Look for repositories in the folders Aqua may use, and offer the ones found (not trusted) next to those in `repos`. On unless this says `false`.
    #[serde(default = "yes")]
    pub discover: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self { repos: Vec::new(), git_path: None, discover: true }
    }
}

impl GitConfig {
    pub fn validate(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for r in &self.repos {
            if r.alias.is_empty() || r.alias.len() > 40 || !r.alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(format!("repo alias '{}' must be 1-40 letters, digits, '_' or '-'", r.alias));
            }
            if !seen.insert(r.alias.to_ascii_lowercase()) {
                return Err(format!("repo alias '{}' is listed twice", r.alias));
            }
            for e in [&r.author_name, &r.author_email].into_iter().flatten() {
                if e.is_empty() || e.len() > 100 || e.contains(['\n', '\r', '\0', '<', '>']) {
                    return Err(format!("author of '{}' is not valid", r.alias));
                }
            }
            // A hook is code from the repository: it runs only where the person said they trust the repository.
            if r.hooks == HooksMode::Run && !r.trusted {
                return Err(format!("repo '{}': hooks \"run\" needs \"trusted\": true (a hook is code from the repository)", r.alias));
            }
            if let Some(t) = &r.attribution {
                if !gate::trailer_is_valid(t) {
                    return Err(format!("attribution of '{}' must be one line such as \"Co-Authored-By: Name <name@example.com>\"", r.alias));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct Repo {
    entry: RepoEntry,
    root: PathBuf,
    /// Found by looking in the folders (not named in `repos.json`).
    found: bool,
    /// The main working folder of the repository this is a worktree of, when Aqua made it (card OA7-05; see `worktree.rs`). A worktree is `found` too: it is not trusted and its configuration is read.
    worktree_of: Option<PathBuf>,
}

/// The repositories found by looking, and when and where the looking was done.
#[derive(Default)]
struct Found {
    repos: Vec<Arc<Repo>>,
    /// The read-write roots the search covered (the folders can change while Aqua runs: a folder allowed in a request, the project folder under full access).
    roots: Vec<PathBuf>,
    searched_at: Option<Instant>,
}

/// A search that is older than this is made again when a call comes (a repository made with `git init` or `git clone` since).
const SEARCH_AGAIN_AFTER: Duration = Duration::from_secs(30);
/// A repository that was asked for and is not known sends the call to look again, but not more often than this.
const SEARCH_AT_MOST_EVERY: Duration = Duration::from_secs(1);

/// Asked each time the tools are listed or called: is a set of tools on?
type Switch = Arc<dyn Fn() -> bool + Send + Sync>;

struct Inner {
    /// The repositories of `repos.json`, in its order.
    explicit: Vec<Arc<Repo>>,
    found: Mutex<Found>,
    discover: bool,
    guard: Arc<PathGuard>,
    git: PathBuf,
    no_hooks: PathBuf,
    /// The worktree tools are on while this says so (`AQUA_WORKTREES`); without it they are off (card OA7-05).
    worktrees_on: Option<Switch>,
    /// What Aqua made with them, in `data/tools/git_worktrees.json`.
    registry: worktree::Registry,
    /// The review tools (conflicts, a resolution, the text of a pull request) and the refusal of conflict markers in a commit are on while this says so (`AQUA_GIT_REVIEW`).
    review_on: Option<Switch>,
    /// Where a file is kept before `git.resolve_conflict` changes it, so that it can be put back. Without it that tool is not on offer.
    trash: Option<Arc<crate::fs::Trash>>,
}

/// The git tools. State is behind an `Arc` so blocking tasks can own it.
pub struct GitProvider(Arc<Inner>);

impl GitProvider {
    /// Offers the worktree tools whenever `on` says so (card OA7-05, `AQUA_WORKTREES`). Call it before the provider is shared.
    pub fn with_worktrees(mut self, on: Switch) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.0) {
            inner.worktrees_on = Some(on);
        }
        self
    }

    /// Offers the review tools whenever `on` says so (card OA7-05, `AQUA_GIT_REVIEW`). Call it before the provider is shared.
    pub fn with_review(mut self, on: Switch) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.0) {
            inner.review_on = Some(on);
        }
        self
    }

    /// The undo store `git.resolve_conflict` keeps a file in before it changes it. Call it before the provider is shared.
    pub fn with_trash(mut self, trash: Arc<crate::fs::Trash>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.0) {
            inner.trash = Some(trash);
        }
        self
    }

    /// `dir` is a folder Aqua owns (`data/tools`); it holds an empty hooks folder.
    pub fn new(cfg: GitConfig, guard: Arc<PathGuard>, dir: &Path) -> Result<Self, String> {
        cfg.validate()?;
        let no_hooks = dir.join("git_no_hooks");
        std::fs::create_dir_all(&no_hooks).map_err(|e| format!("cannot create {}: {e}", no_hooks.display()))?;
        let mut repos = Vec::new();
        for entry in cfg.repos {
            // A repository must be inside the roots, so every protection of the file tools applies to it.
            let resolved = guard.resolve(&entry.path, Op::Read).map_err(|e| format!("repo '{}': {}", entry.alias, e.message))?;
            let root = canonical(&resolved.path).map_err(|e| format!("repo '{}': {e}", entry.alias))?;
            if !root.join(".git").exists() {
                return Err(format!("repo '{}': {} is not a git repository (no .git)", entry.alias, display(&root)));
            }
            repos.push(Arc::new(Repo { entry, root, found: false, worktree_of: None }));
        }
        Ok(Self(Arc::new(Inner {
            explicit: repos,
            found: Mutex::new(Found::default()),
            discover: cfg.discover,
            guard,
            git: find_git(&cfg.git_path),
            no_hooks,
            worktrees_on: None,
            registry: worktree::Registry::new(dir.join(worktree::REGISTRY_FILE)),
            review_on: None,
            trash: None,
        })))
    }
}

/// The git program: the one `repos.json` names (`git_path`), else the first one found in the usual places, else `git` from the path. The journal and the checkpoints use it too.
pub(crate) fn find_git(configured: &Option<String>) -> PathBuf {
    if let Some(p) = configured {
        return PathBuf::from(crate::fs::guard::expand_env(p));
    }
    for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Ok(b) = std::env::var(base) {
            for rel in ["Git/cmd/git.exe", "Programs/Git/cmd/git.exe"] {
                let p = PathBuf::from(&b).join(rel);
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    PathBuf::from("git")
}

fn bad(msg: impl Into<String>) -> ToolError {
    ToolError::new(ErrorCode::InvalidArguments, msg)
}

fn arg_str<'a>(a: &'a Value, k: &str) -> &'a str {
    a.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn arg_bool(a: &Value, k: &str) -> bool {
    a.get(k).and_then(|v| v.as_bool()).unwrap_or(false)
}

fn arg_u64(a: &Value, k: &str, d: u64) -> u64 {
    a.get(k).and_then(|v| v.as_u64()).unwrap_or(d)
}

fn arg_paths(a: &Value) -> Vec<String> {
    a.get("paths").and_then(|v| v.as_array()).map(|x| x.iter().filter_map(|p| p.as_str().map(String::from)).collect()).unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Read,
    Write,
    Network,
    Push,
    Destructive,
}

fn specs() -> Vec<(&'static str, &'static str, Kind, Value, Vec<&'static str>)> {
    let repo = json!({"type": "string", "maxLength": 40});
    let paths = json!({"type": "array", "items": {"type": "string", "minLength": 1, "maxLength": 1024}, "maxItems": 100});
    let rev = json!({"type": "string", "minLength": 1, "maxLength": 200});
    let limit = |d: u64, m: u64| json!({"type": "integer", "minimum": 1, "maximum": m, "default": d});
    vec![
        ("git.repos", "List the git repositories Aqua can use, with the branch and how many files are changed. Say path to see only the ones in or around a folder.", Kind::Read, json!({"path": {"type": "string", "maxLength": 1024}, "limit": {"type": "integer", "minimum": 1, "maximum": 8, "default": 8}}), vec![]),
        ("git.status", "Show changed, staged and untracked files and the current branch.", Kind::Read, json!({"repo": repo, "paths": paths, "untracked": {"type": "boolean", "default": true}}), vec![]),
        ("git.diff", "Show changes: unstaged, staged (staged=true), or between two revisions.", Kind::Read, json!({"repo": repo, "staged": {"type": "boolean", "default": false}, "from": rev, "to": rev, "paths": paths, "mode": {"type": "string", "enum": ["patch", "stat", "names"], "default": "patch"}, "context": {"type": "integer", "minimum": 0, "maximum": 20, "default": 3}}), vec![]),
        ("git.log", "Show commit history, newest first.", Kind::Read, json!({"repo": repo, "ref": rev, "path": {"type": "string", "maxLength": 1024}, "limit": limit(20, 200), "author": {"type": "string", "maxLength": 100}, "grep": {"type": "string", "maxLength": 200}, "since": {"type": "string", "maxLength": 40}}), vec![]),
        ("git.show", "Show one commit (message and changes), or a file at a revision.", Kind::Read, json!({"repo": repo, "rev": rev, "path": {"type": "string", "maxLength": 1024}, "mode": {"type": "string", "enum": ["patch", "stat"], "default": "stat"}}), vec!["rev"]),
        ("git.blame", "Show who last changed each line of a file.", Kind::Read, json!({"repo": repo, "path": {"type": "string", "minLength": 1, "maxLength": 1024}, "rev": rev, "start_line": {"type": "integer", "minimum": 1}, "end_line": {"type": "integer", "minimum": 1}}), vec!["path"]),
        ("git.branch", "List branches (local and remote) with their latest commit.", Kind::Read, json!({"repo": repo, "pattern": {"type": "string", "maxLength": 100}, "limit": limit(50, 300)}), vec![]),
        ("git.tag", "List tags.", Kind::Read, json!({"repo": repo, "pattern": {"type": "string", "maxLength": 100}, "limit": limit(50, 300)}), vec![]),
        ("git.remote", "List remotes and their URLs.", Kind::Read, json!({"repo": repo}), vec![]),
        ("git.stash_list", "List stash entries.", Kind::Read, json!({"repo": repo}), vec![]),
        ("git.grep", "Search the tracked files for text.", Kind::Read, json!({"repo": repo, "query": {"type": "string", "minLength": 1, "maxLength": 200}, "rev": rev, "paths": paths, "limit": limit(50, 300)}), vec!["query"]),
        ("git.add", "Stage files (paths) or all changes (all=true).", Kind::Write, json!({"repo": repo, "paths": paths, "all": {"type": "boolean", "default": false}}), vec![]),
        ("git.unstage", "Remove files from the staging area, keeping their changes.", Kind::Write, json!({"repo": repo, "paths": paths}), vec!["paths"]),
        ("git.commit", "Commit what is staged, or stage paths (or all=true) and commit them in one call. A change that adds a secret is refused. Signing is off. There is no amend.", Kind::Write, json!({"repo": repo, "message": {"type": "string", "minLength": 1, "maxLength": 5000}, "allow_empty": {"type": "boolean", "default": false}, "paths": paths, "all": {"type": "boolean", "default": false}}), vec!["message"]),
        ("git.switch", "Switch to a branch, or create it with create=true (from start_point or the current commit).", Kind::Write, json!({"repo": repo, "branch": {"type": "string", "minLength": 1, "maxLength": 200}, "create": {"type": "boolean", "default": false}, "start_point": rev}), vec!["branch"]),
        ("git.tag_create", "Create a tag, annotated when a message is given.", Kind::Write, json!({"repo": repo, "name": {"type": "string", "minLength": 1, "maxLength": 200}, "rev": rev, "message": {"type": "string", "maxLength": 1000}}), vec!["name"]),
        ("git.stash", "Stash work in progress: op is push, apply or pop (see git.stash_list to look).", Kind::Write, json!({"repo": repo, "op": {"type": "string", "enum": ["push", "apply", "pop"], "default": "push"}, "message": {"type": "string", "maxLength": 200}, "include_untracked": {"type": "boolean", "default": false}, "index": {"type": "integer", "minimum": 0, "maximum": 100, "default": 0}}), vec![]),
        ("git.merge", "Merge a branch into the current one (ff when possible), or abort a merge in progress.", Kind::Write, json!({"repo": repo, "source": {"type": "string", "maxLength": 200}, "action": {"type": "string", "enum": ["merge", "abort"], "default": "merge"}, "ff": {"type": "string", "enum": ["auto", "only", "never"], "default": "auto"}}), vec![]),
        ("git.fetch", "Download from a remote without changing your branches.", Kind::Network, json!({"repo": repo, "remote": {"type": "string", "maxLength": 100, "default": "origin"}, "prune": {"type": "boolean", "default": false}, "tags": {"type": "boolean", "default": false}}), vec![]),
        ("git.pull", "Fast-forward the current branch from its remote (never creates a merge commit).", Kind::Network, json!({"repo": repo, "remote": {"type": "string", "maxLength": 100, "default": "origin"}}), vec![]),
        ("git.push", "Push a branch to a remote. Never forced. Always asks.", Kind::Push, json!({"repo": repo, "remote": {"type": "string", "maxLength": 100, "default": "origin"}, "branch": {"type": "string", "maxLength": 200}, "set_upstream": {"type": "boolean", "default": false}}), vec![]),
        ("git.discard", "Throw away uncommitted changes to files. A backup ref is kept first. Always asks.", Kind::Destructive, json!({"repo": repo, "paths": paths}), vec!["paths"]),
        ("git.reset", "Move the branch to a commit. mode soft, mixed or hard (hard discards changes; a backup ref is kept). Always asks.", Kind::Destructive, json!({"repo": repo, "to": rev, "mode": {"type": "string", "enum": ["soft", "mixed", "hard"], "default": "mixed"}}), vec!["to"]),
        ("git.branch_delete", "Delete a merged local branch (force=true also deletes an unmerged one). Always asks.", Kind::Destructive, json!({"repo": repo, "branch": {"type": "string", "minLength": 1, "maxLength": 200}, "force": {"type": "boolean", "default": false}}), vec!["branch"]),
        ("git.stash_drop", "Delete a stash entry. Always asks.", Kind::Destructive, json!({"repo": repo, "index": {"type": "integer", "minimum": 0, "maximum": 100, "default": 0}}), vec![]),
        // On offer only with AQUA_WORKTREES=on (card OA7-05, TM-13).
        ("git.worktree_add", "Make a worktree: a second working folder of the repository on a branch of its own. Returns the repo alias and the folder to use. Always asks.", Kind::Write, json!({"repo": repo, "name": {"type": "string", "minLength": 1, "maxLength": 40}, "branch": {"type": "string", "minLength": 1, "maxLength": 200}, "start_point": rev}), vec![]),
        ("git.worktree_list", "List the worktrees Aqua made of the repository, with their branch and how many files are changed, and the ones git knows that Aqua did not make.", Kind::Read, json!({"repo": repo}), vec![]),
        ("git.worktree_remove", "Remove a worktree Aqua made, by name. Refused when it has changes unless force=true (untracked files are lost). Always asks.", Kind::Destructive, json!({"repo": repo, "name": {"type": "string", "minLength": 1, "maxLength": 40}, "force": {"type": "boolean", "default": false}, "delete_branch": {"type": "boolean", "default": false}}), vec!["name"]),
        // On offer only with AQUA_GIT_REVIEW=on (card OA7-05, TM-17 and TM-18).
        ("git.conflicts", "List the files with merge conflicts, how each is conflicted and the hunks of each text file (ours, theirs, base); says what is in progress.", Kind::Read, json!({"repo": repo, "path": {"type": "string", "maxLength": 1024}, "max_hunks": {"type": "integer", "minimum": 1, "maximum": 10, "default": 3}}), vec![]),
        ("git.resolve_conflict", "Resolve the conflict hunks of one file (take=ours|theirs|both, or resolutions per hunk). The person approves the diff. Stages nothing.", Kind::Write, json!({"repo": repo, "path": {"type": "string", "minLength": 1, "maxLength": 1024}, "take": {"type": "string", "enum": ["ours", "theirs", "both"]}, "resolutions": {"type": "array", "maxItems": 50, "items": {"type": "object", "properties": {"hunk": {"type": "integer", "minimum": 1}, "take": {"type": "string", "enum": ["ours", "theirs", "both", "text"]}, "text": {"type": "string", "maxLength": 20000}}, "required": ["hunk", "take"]}}}), vec!["path"]),
        ("git.pr_draft", "Draft a pull request title and description from this branch's commits and files. Local only: nothing is sent (forge.pr_create sends).", Kind::Read, json!({"repo": repo, "base": rev, "head": rev}), vec![]),
    ]
}

impl ToolProvider for GitProvider {
    fn id(&self) -> &str {
        "git"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        specs()
            .into_iter()
            .map(|(name, description, kind, props, required)| {
                let classes = match kind {
                    Kind::Read => vec![PermClass::Read],
                    Kind::Write => vec![PermClass::Write],
                    Kind::Network => vec![PermClass::Network, PermClass::Remote],
                    Kind::Push => vec![PermClass::Network, PermClass::Remote, PermClass::ExternalEffect],
                    Kind::Destructive => vec![PermClass::Write, PermClass::Destructive],
                };
                let network = matches!(kind, Kind::Network | Kind::Push);
                ToolManifest {
                    name: name.into(),
                    title: name.into(),
                    description: description.into(),
                    classes,
                    default_decision: Decision::Auto,
                    input_schema: json!({"type": "object", "properties": props, "required": required}),
                    timeout_default_ms: if network { 120_000 } else { 30_000 },
                    timeout_max_ms: if network { 300_000 } else { 60_000 },
                    annotations: Annotations { read_only: kind == Kind::Read, destructive: kind == Kind::Destructive, idempotent: kind == Kind::Read, open_world: network },
                    // Commit messages, branch names, diffs and file contents are written by other people.
                    untrusted_output: true,
                }
            })
            .collect()
    }

    fn availability(&self, tool: &str) -> aqua_tool_host::Availability {
        use aqua_tool_host::Availability::{Available, Unavailable};
        match tool {
            "git.worktree_add" | "git.worktree_list" | "git.worktree_remove" if !self.0.worktrees_enabled() => Unavailable(format!("{tool} is off (AQUA_WORKTREES=on turns the worktree tools on)")),
            "git.conflicts" | "git.resolve_conflict" | "git.pr_draft" if !self.0.review_enabled() => Unavailable(format!("{tool} is off (AQUA_GIT_REVIEW=on turns the conflict and pull request tools on)")),
            // A resolution that cannot be put back is not on offer.
            "git.resolve_conflict" if self.0.trash.is_none() => Unavailable("git.resolve_conflict needs the undo store of the file tools, which is not there".to_string()),
            _ => Available,
        }
    }

    /// A resolution is shown to the person as a diff each time and an answer is never kept for it; a worktree takes disk and a branch each time (card OA7-05).
    fn always_ask_reason(&self, tool: &str, _args: &Value) -> Option<String> {
        match tool {
            "git.resolve_conflict" => Some("a resolution changes a file the person has not read: each one is shown as a diff and approved on its own".to_string()),
            "git.worktree_add" => Some("a worktree takes room on the disk and a branch of its own: each one is approved on its own".to_string()),
            _ => None,
        }
    }

    /// What a resolution of a conflict would change, as a diff for the question (card OA7-05): the file as it is now against the file with the hunks replaced, worked out with the code the call itself runs.
    fn approval_preview(&self, tool: &str, args: &Value) -> Option<ChangePreview> {
        if tool != "git.resolve_conflict" || !self.0.review_enabled() {
            return None;
        }
        let repo = self.0.repo(args).ok()?;
        self.0.resolve_preview(&repo, args)
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        let s = &self.0;
        let (_, _, kind, _, _) = specs().into_iter().find(|x| x.0 == tool).ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("unknown tool '{tool}'")))?;
        if tool == "git.repos" {
            // The list of repositories names none: it reads the folders Aqua may use and asks git for the branch of each.
            s.repos_path_filter(args)?;
            return Ok(Classification { classes: vec![PermClass::Read], target: "git repositories".to_string(), summary: "List the git repositories".to_string(), target_allowed: true });
        }
        let repo = s.repo(args)?;
        let repo: &Repo = &repo;
        // Validate everything now so a bad value is refused before any prompt.
        s.validate_args(repo, tool, args)?;
        let alias = repo.entry.alias.clone();
        let mut classes = match kind {
            Kind::Read => vec![PermClass::Read],
            Kind::Write => vec![PermClass::Write],
            Kind::Network => vec![PermClass::Network, PermClass::Remote],
            Kind::Push => vec![PermClass::Network, PermClass::Remote, PermClass::ExternalEffect],
            Kind::Destructive => vec![PermClass::Write, PermClass::Destructive],
        };
        // Paths: the file guard's flags (secret-looking names, scripts and build files) apply to staged files too.
        let mut trusted = repo.entry.trusted;
        // `git.commit` with `paths` stages them first, so it answers for the same flags as `git.add` of those paths (card OA5-01, TL-06).
        if matches!(tool, "git.add" | "git.unstage" | "git.discard" | "git.commit") {
            for p in arg_paths(args) {
                let r = s.guard.resolve(&repo.root.join(&p).to_string_lossy(), Op::Write)?;
                if r.sensitive && !classes.contains(&PermClass::Credential) {
                    classes.push(PermClass::Credential);
                }
                if r.executable_kind && !classes.contains(&PermClass::Execute) {
                    classes.push(PermClass::Execute);
                }
            }
        }
        // A resolution writes the file it names: a file that looks like it holds secrets asks as such, and one that can run code asks as running code (card OA7-05).
        if tool == "git.resolve_conflict" {
            let r = s.guard.resolve(&repo.root.join(arg_str(args, "path")).to_string_lossy(), Op::Write)?;
            if r.sensitive && !classes.contains(&PermClass::Credential) {
                classes.push(PermClass::Credential);
            }
            if r.executable_kind && !classes.contains(&PermClass::Execute) {
                classes.push(PermClass::Execute);
            }
        }
        if matches!(tool, "git.add" | "git.commit") && arg_bool(args, "all") {
            // `add -A` picks up whatever is there, secret files included.
            trusted = false;
        }
        // A commit that lets the repository's hooks run executes code that came with the repository: it asks where running code asks (card OA7-02, TM-07).
        if tool == "git.commit" && s.hooks_run_for(repo) && !classes.contains(&PermClass::Execute) {
            classes.push(PermClass::Execute);
        }
        let mut summary = format!("{tool} in {alias}");
        if kind == Kind::Write || kind == Kind::Push || kind == Kind::Destructive || kind == Kind::Network {
            let head = s.head(repo);
            // What the question says about where the commit lands: the branch, or that HEAD is on no branch.
            let branch = head.label();
            // On no branch a commit or a tag lands where no branch name protects it, so it asks like a protected one.
            let protected = match &head {
                Head::Detached(_) => true,
                Head::Branch(name) | Head::Unborn(name) => repo.entry.protected_branches.iter().any(|p| wildcard(p, name)),
            };
            summary = match tool {
                "git.commit" => {
                    // Both effects are in the question: what is staged first, then the commit (card OA5-01).
                    let stages = if arg_bool(args, "all") {
                        "Stage every change, then commit".to_string()
                    } else if !arg_paths(args).is_empty() {
                        let paths = arg_paths(args);
                        format!("Stage {} ({}), then commit", if paths.len() == 1 { "1 file".to_string() } else { format!("{} files", paths.len()) }, truncate(&paths.iter().take(5).cloned().collect::<Vec<_>>().join(", "), 160))
                    } else {
                        "Commit staged changes".to_string()
                    };
                    format!("{stages} on {branch} in {alias}: {}", truncate(arg_str(args, "message"), 120))
                }
                "git.push" => {
                    let b = if arg_str(args, "branch").is_empty() { branch.clone() } else { arg_str(args, "branch").to_string() };
                    let b_protected = repo.entry.protected_branches.iter().any(|p| wildcard(p, &b));
                    format!("Push {b} to {} in {alias}{}", if arg_str(args, "remote").is_empty() { "origin" } else { arg_str(args, "remote") }, if b_protected { " (a protected branch)" } else { "" })
                }
                "git.reset" => format!("Reset {branch} in {alias} to {} ({})", arg_str(args, "to"), if arg_str(args, "mode").is_empty() { "mixed" } else { arg_str(args, "mode") }),
                "git.discard" => format!("Discard uncommitted changes to {} file(s) in {alias}; a backup ref is kept", arg_paths(args).len()),
                "git.branch_delete" => format!("Delete branch {} in {alias}{}", arg_str(args, "branch"), if arg_bool(args, "force") { " even if unmerged" } else { "" }),
                "git.switch" => format!("Switch {alias} to {}{}", arg_str(args, "branch"), if arg_bool(args, "create") { " (new branch)" } else { "" }),
                "git.merge" => format!("Merge {} into {branch} in {alias}", arg_str(args, "source")),
                "git.worktree_add" => s.worktree_add_summary(repo, args)?,
                "git.worktree_remove" => s.worktree_remove_summary(repo, args)?,
                "git.resolve_conflict" => s.resolve_summary(repo, args)?,
                "git.pull" => format!("Fast-forward {branch} in {alias} from {}", if arg_str(args, "remote").is_empty() { "origin" } else { arg_str(args, "remote") }),
                other => format!("{other} in {alias} (on {branch})"),
            };
            // Routine writes on a trusted repository run without a prompt, but never on a protected branch.
            // Merging and re-applying stashes move history or overwrite work, so they ask. Anything that
            // reaches a remote or destroys work asks whatever the repository's trust.
            match kind {
                Kind::Write => {
                    let op = arg_str(args, "op");
                    // A worktree takes disk and a branch, and a resolution changes a file the person has not seen: both ask, in every repository.
                    if (protected && matches!(tool, "git.commit" | "git.tag_create")) || matches!(tool, "git.merge" | "git.worktree_add" | "git.resolve_conflict") || (tool == "git.stash" && matches!(op, "apply" | "pop")) {
                        trusted = false;
                    }
                }
                Kind::Network | Kind::Push | Kind::Destructive => trusted = false,
                Kind::Read => {}
            }
        }
        Ok(Classification { classes, target: format!("{alias} ({})", display(&repo.root)), summary, target_allowed: matches!(kind, Kind::Read) || trusted })
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let inner = self.0.clone();
        let cancel = ctx.cancel.clone();
        let tool = tool.to_string();
        let timeout_ms = ctx.timeout_ms;
        let cx = CallCx { run_id: ctx.agent_run_id.clone(), shown: ctx.shown.clone() };
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                // The programs of this call get no more than what is left of the host's time for it (card OA4-03).
                let _clock = crate::term::CallClock::start(timeout_ms);
                inner.dispatch(&tool, &args, &cancel, &cx)
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("git task failed: {e}")))?
        })
    }
}

/// What a call knows besides its arguments: the run it is a step of, and what the person was shown when they were asked about it (see `ChangePreview::guard`).
struct CallCx {
    run_id: String,
    shown: Option<String>,
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
}

/// What `HEAD` is: on a branch, on no branch (a commit was checked out), or on a branch that has no commit yet (a new repository).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Head {
    Branch(String),
    /// The short hash of the commit, or `unknown` when git could not say (a `HEAD` file that is damaged).
    Detached(String),
    Unborn(String),
}

impl Head {
    /// How a question or a listing says where the repository is: `main`, `main (no commits yet)`, `detached HEAD at 1a2b3c4`.
    fn label(&self) -> String {
        match self {
            Head::Branch(name) => name.clone(),
            Head::Unborn(name) => format!("{name} (no commits yet)"),
            Head::Detached(sha) => format!("detached HEAD at {sha}"),
        }
    }

    /// The branch name when there is one (also for a branch with no commit yet).
    fn branch(&self) -> Option<&str> {
        match self {
            Head::Branch(name) | Head::Unborn(name) => Some(name),
            Head::Detached(_) => None,
        }
    }
}

impl Inner {
    /// The repositories a call may name: those of `repos.json`, then the ones found in the folders Aqua may use (see `discover.rs`). The folders are looked through again when they
    /// changed (a folder was allowed, the project folder was set), when the last look is old, and, with `look_again`, when a repository that was asked for is not known (once in a few seconds at most).
    fn snapshot(&self, look_again: bool) -> Vec<Arc<Repo>> {
        let mut all = self.explicit.clone();
        if self.discover {
            let roots: Vec<PathBuf> = self.guard.roots().into_iter().filter(|(_, _, access)| *access == Access::Rw).map(|(_, path, _)| path).collect();
            let mut found = self.found.lock().unwrap_or_else(PoisonError::into_inner);
            let due = match found.searched_at {
                None => true,
                Some(at) => found.roots != roots || at.elapsed() > SEARCH_AGAIN_AFTER || (look_again && at.elapsed() > SEARCH_AT_MOST_EVERY),
            };
            if due {
                let repos = self.search(&found.repos, &roots);
                *found = Found { repos, roots, searched_at: Some(Instant::now()) };
            }
            all.extend(found.repos.iter().cloned());
        }
        // The worktrees Aqua made (only while the worktree tools are on), after everything else so that no alias of a repository the person named is taken from it.
        self.add_worktree_repos(&mut all);
        all
    }

    /// Look through `roots` for repositories that `repos.json` does not name. A repository seen before keeps its alias (a name the model used a moment ago must not change under it); a new one gets
    /// the name of its folder, and `-2`, `-3` when that name is taken by a repository of `repos.json` or by another found.
    fn search(&self, before: &[Arc<Repo>], roots: &[PathBuf]) -> Vec<Arc<Repo>> {
        let tops: Vec<PathBuf> = discover::find_repos(&self.guard, roots).into_iter().filter(|top| !self.explicit.iter().any(|e| crate::fs::guard::same_place(&e.root, top))).collect();
        let mut kept: Vec<Arc<Repo>> = Vec::new();
        let mut taken: Vec<String> = self.explicit.iter().map(|r| r.entry.alias.to_ascii_lowercase()).collect();
        for top in &tops {
            if let Some(old) = before.iter().find(|r| crate::fs::guard::same_place(&r.root, top)) {
                if !taken.contains(&old.entry.alias.to_ascii_lowercase()) {
                    taken.push(old.entry.alias.to_ascii_lowercase());
                    kept.push(old.clone());
                }
            }
        }
        for top in &tops {
            if kept.iter().any(|r| crate::fs::guard::same_place(&r.root, top)) {
                continue;
            }
            let name = top.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let alias = discover::unique_alias(&discover::sanitize_alias(&name), &|a: &str| taken.contains(&a.to_ascii_lowercase()));
            taken.push(alias.to_ascii_lowercase());
            kept.push(Arc::new(Repo { entry: RepoEntry::found(alias, top), root: top.clone(), found: true, worktree_of: None }));
        }
        kept.sort_by_key(|r| r.root.to_string_lossy().to_ascii_lowercase());
        kept
    }

    fn known(snapshot: &[Arc<Repo>], alias: &str) -> Option<Arc<Repo>> {
        snapshot.iter().find(|r| r.entry.alias.eq_ignore_ascii_case(alias)).cloned()
    }

    /// The file or folder that the arguments of a call point at by an absolute path (or an `@root` one), when one does: the way to tell which repository is meant when none was named.
    fn path_named_by(&self, args: &Value) -> Option<PathBuf> {
        let mut named: Vec<String> = arg_paths(args);
        named.extend(args.get("path").and_then(|v| v.as_str()).map(String::from));
        named.into_iter().filter(|p| p.starts_with('@') || Path::new(p).is_absolute()).find_map(|p| self.guard.resolve(&p, Op::Read).ok().map(|r| r.path))
    }

    /// The repository that holds `path`, the innermost when one is inside another.
    fn holding(snapshot: &[Arc<Repo>], path: &Path) -> Option<Arc<Repo>> {
        snapshot.iter().filter(|r| within(path, &r.root)).max_by_key(|r| r.root.components().count()).cloned()
    }

    fn repo(&self, args: &Value) -> Result<Arc<Repo>, ToolError> {
        let alias = arg_str(args, "repo");
        let mut snapshot = self.snapshot(false);
        if !alias.is_empty() {
            if let Some(r) = Self::known(&snapshot, alias) {
                return Ok(r);
            }
            // A repository made a moment ago (`git init`, a clone) is found by looking again.
            snapshot = self.snapshot(true);
            return Self::known(&snapshot, alias).ok_or_else(|| {
                ToolError::new(ErrorCode::NotFound, format!("no repo '{alias}'. Known: {}. (git.repos lists them)", if snapshot.is_empty() { "none".to_string() } else { snapshot.iter().map(|r| r.entry.alias.as_str()).collect::<Vec<_>>().join(", ") }))
            });
        }
        // None was named. The person's own list comes first: one entry is the repository, several are a question.
        let others = |snapshot: &[Arc<Repo>]| snapshot.iter().map(|r| r.entry.alias.clone()).collect::<Vec<_>>().join(", ");
        let named_path = self.path_named_by(args);
        // A path inside a worktree Aqua made means that worktree (a worktree is only ever named, by its alias or by a path in it, and is never the repository a call falls back to).
        let worktrees: Vec<Arc<Repo>> = snapshot.iter().filter(|r| r.worktree_of.is_some()).cloned().collect();
        if let Some(r) = named_path.as_deref().and_then(|p| Self::holding(&worktrees, p)) {
            return Ok(r);
        }
        let snapshot: Vec<Arc<Repo>> = snapshot.into_iter().filter(|r| r.worktree_of.is_none()).collect();
        let explicit: Vec<Arc<Repo>> = snapshot.iter().filter(|r| !r.found).cloned().collect();
        if let [only] = explicit.as_slice() {
            return Ok(only.clone());
        }
        let say_which_from_found = explicit.is_empty();
        let candidates = if say_which_from_found { snapshot.clone() } else { explicit };
        if let Some(r) = named_path.as_deref().and_then(|p| Self::holding(&candidates, p)) {
            return Ok(r);
        }
        match candidates.as_slice() {
            [only] => Ok(only.clone()),
            [] => {
                // Nothing known: look once more (a repository may have been made since the last look) before saying there is none.
                let again: Vec<Arc<Repo>> = self.snapshot(true).into_iter().filter(|r| r.worktree_of.is_none()).collect();
                match again.as_slice() {
                    [only] => Ok(only.clone()),
                    [] => Err(ToolError::new(ErrorCode::NotFound, "no repositories are configured and none was found in the folders Aqua may use: make a folder a repository with git init, or add one to data/tools/repos.json")),
                    many => Err(bad(format!("say which repo: {}", others(many)))),
                }
            }
            many => {
                // Several repositories were found (none named in `repos.json`): the only one under the first folder, the workspace, is the one meant. Several named in `repos.json` are asked about, as they always were.
                if say_which_from_found {
                    let first_root = self.guard.roots().into_iter().find(|(_, _, access)| *access == Access::Rw).map(|(_, path, _)| path);
                    let under_first: Vec<&Arc<Repo>> = first_root.iter().flat_map(|root| many.iter().filter(move |r| within(&r.root, root))).collect();
                    if let [only] = under_first.as_slice() {
                        return Ok((*only).clone());
                    }
                }
                Err(bad(format!("say which repo: {}", others(many))))
            }
        }
    }

    /// Does `repo` let the commit run the repository's own hooks? Only when the person turned that on for a repository they trust (and the file said both; `GitConfig::validate` holds it to that).
    fn hooks_run_for(&self, repo: &Repo) -> bool {
        !repo.found && repo.entry.trusted && repo.entry.hooks == HooksMode::Run
    }

    /// Options on every git call. They switch off everything a repository's own config could use to run code. `hooks` leaves the repository's own hooks in place: only the commit of a
    /// repository with `hooks: "run"` asks for it.
    fn base_args(&self, repo: &Repo, hooks: bool) -> Vec<String> {
        let no_hooks = self.no_hooks.to_string_lossy().to_string();
        let mut v: Vec<String> = vec!["--no-pager".into(), "--literal-pathspecs".into(), "--no-optional-locks".into()];
        for c in [
            if hooks { String::new() } else { format!("core.hooksPath={no_hooks}") },
            // File names are shown as they are written, not as `\303\251` (git's quoting of anything outside ASCII), so the name the model reads is one it can give back; and a path over 260
            // characters is a path on Windows (card OA7-02, TM-06).
            "core.quotepath=false".into(),
            "core.longpaths=true".into(),
            "core.fsmonitor=false".into(),
            "core.editor=true".into(),
            "sequence.editor=true".into(),
            "core.pager=cat".into(),
            "core.askPass=".into(),
            "diff.external=".into(),
            "diff.noprefix=false".into(),
            "commit.gpgsign=false".into(),
            "tag.gpgsign=false".into(),
            "gpg.program=gpg".into(),
            "protocol.ext.allow=never".into(),
            "http.followRedirects=initial".into(),
            "filter.lfs.smudge=".into(),
            "filter.lfs.process=".into(),
            "filter.lfs.required=false".into(),
            "gc.auto=0".into(),
            "maintenance.auto=false".into(),
            "color.ui=false".into(),
        ] {
            if !c.is_empty() {
                v.extend(["-c".into(), c]);
            }
        }
        if let Some(n) = &repo.entry.author_name {
            v.extend(["-c".into(), format!("user.name={n}")]);
        }
        if let Some(e) = &repo.entry.author_email {
            v.extend(["-c".into(), format!("user.email={e}")]);
        }
        v.extend(["-C".into(), repo.root.to_string_lossy().to_string()]);
        v
    }

    fn env(&self) -> Vec<(String, String)> {
        let mut env = build_env(std::env::vars(), &HashMap::new()).unwrap_or_default();
        for (k, v) in [("GIT_TERMINAL_PROMPT", "0"), ("GIT_PAGER", "cat"), ("GIT_EDITOR", "true"), ("GIT_ASKPASS", ""), ("LC_ALL", "C"), ("LANGUAGE", "C"), ("GCM_INTERACTIVE", "never")] {
            env.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
            env.push((k.into(), v.into()));
        }
        env
    }

    /// The options that switch off, by name, every program the repository's own configuration names (a filter, a merge driver, a credential helper, an ssh command, an alias that runs a
    /// shell...). `git.*` already switches off the keys it knows by name (`base_args`); for a repository that was found and not named by the person this goes further and is the
    /// same list `term.run git` uses (`switch_off_program_keys`): it lists the repository's configuration with `git config`, which runs nothing, and keeps what is in the repository's own
    /// files. A repository whose configuration cannot be read (git's `safe.directory` check says so, the file is damaged), or names more than can be switched off, is not run in: the
    /// message is git's own first line.
    fn programs_switched_off(&self, repo: &Repo, cancel: &AtomicBool) -> Result<Vec<String>, ToolError> {
        let scan = crate::term::rules::git_config_scan_args(&["-C".to_string(), repo.root.to_string_lossy().to_string(), "config".to_string()]);
        let r = run_command(self.git.clone(), scan, repo.root.clone(), self.env(), None, Limits { memory_mb: 512, max_processes: 8 }, Duration::from_secs(15), 256 * 1024, cancel)?;
        if r.cancelled {
            return Err(ToolError::new(ErrorCode::Cancelled, "the run was cancelled and git was stopped"));
        }
        if r.timed_out || r.exit_code != Some(0) {
            let said = r.stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("git config did not finish").chars().take(160).collect::<String>();
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("the configuration of the repository {} could not be read ({}): git is not run in it", repo.entry.alias, args::scrub_urls(&said))));
        }
        crate::term::rules::switch_off_program_keys(&r.stdout).map_err(|why| ToolError::new(ErrorCode::PermissionDenied, format!("{why} (repo {})", repo.entry.alias)))
    }

    /// One run of git with every protection on. `hooks` lets the repository's own hooks run (only the commit of a repository with `hooks: "run"` asks for it). The text is as git printed
    /// it; `git` below is the same with the credentials of a URL masked, which is the one to use for anything that is shown.
    fn git_raw(&self, repo: &Repo, extra: &[&str], network: bool, cap_kb: usize, timeout_s: u64, hooks: bool, cancel: &AtomicBool) -> Result<Ran, ToolError> {
        // What a repository that was found (not named by the person) names to run is switched off first: git takes the last of two `-c` for a key, so the safe options after them win.
        let mut argv = if repo.found { self.programs_switched_off(repo, cancel)? } else { Vec::new() };
        argv.extend(self.base_args(repo, hooks));
        if network {
            // Only the user's credential manager may supply credentials; a repository's own helper is ignored.
            argv.extend(["-c".into(), "credential.helper=".into(), "-c".into(), "credential.helper=manager".into()]);
        } else {
            argv.extend(["-c".into(), "credential.helper=".into()]);
        }
        argv.extend(extra.iter().map(|s| s.to_string()));
        let r = run_command(self.git.clone(), argv, repo.root.clone(), self.env(), None, Limits { memory_mb: 1024, max_processes: 16 }, Duration::from_secs(timeout_s), cap_kb * 1024, cancel)?;
        if r.cancelled {
            return Err(ToolError::new(ErrorCode::Cancelled, "the run was cancelled and git was stopped"));
        }
        if r.timed_out {
            return Err(ToolError::new(ErrorCode::Timeout, format!("git ran longer than {} and was stopped", r.limit_words())));
        }
        Ok(Ran { code: r.exit_code, stdout: r.stdout, stderr: r.stderr, truncated: r.truncated })
    }

    fn git(&self, repo: &Repo, extra: &[&str], network: bool, cap_kb: usize, timeout_s: u64, cancel: &AtomicBool) -> Result<Ran, ToolError> {
        let r = self.git_raw(repo, extra, network, cap_kb, timeout_s, false, cancel)?;
        Ok(Ran { code: r.code, stdout: args::scrub_urls(&r.stdout), stderr: args::scrub_urls(&r.stderr), truncated: r.truncated })
    }

    /// Run git and turn a failure into the right error.
    fn ok(&self, repo: &Repo, extra: &[&str], network: bool, cap_kb: usize, timeout_s: u64, cancel: &AtomicBool) -> Result<Ran, ToolError> {
        let r = self.git(repo, extra, network, cap_kb, timeout_s, cancel)?;
        if r.code == Some(0) {
            Ok(r)
        } else {
            Err(git_failure(&r.stderr, &r.stdout, repo))
        }
    }

    /// Where `HEAD` is (card OA7-02, TM-06). `symbolic-ref` says the branch, also for a branch with no commit yet; it fails with status 1 when HEAD is on no branch, and then the commit is
    /// named. What git cannot say (a damaged `HEAD`, a timeout) is read as on no branch, so that a write asks.
    fn head(&self, repo: &Repo) -> Head {
        let cancel = AtomicBool::new(false);
        let detached = |this: &Self| {
            let sha = match this.git(repo, &["rev-parse", "--short", "HEAD"], false, 4, 10, &cancel) {
                Ok(r) if r.code == Some(0) && !r.stdout.trim().is_empty() => r.stdout.trim().to_string(),
                _ => "unknown".to_string(),
            };
            Head::Detached(sha)
        };
        match self.git(repo, &["symbolic-ref", "-q", "--short", "HEAD"], false, 4, 10, &cancel) {
            Ok(r) if r.code == Some(0) && !r.stdout.trim().is_empty() => {
                let name = r.stdout.trim().to_string();
                match self.git(repo, &["rev-parse", "-q", "--verify", "HEAD"], false, 4, 10, &cancel) {
                    Ok(c) if c.code == Some(0) => Head::Branch(name),
                    Ok(c) if c.code == Some(1) => Head::Unborn(name),
                    _ => Head::Detached("unknown".to_string()),
                }
            }
            Ok(r) if r.code == Some(1) => detached(self),
            _ => Head::Detached("unknown".to_string()),
        }
    }

    /// Where `HEAD` is and how many files are changed, from one `git status` (`--porcelain=v2 --branch` says both). What cannot be read this way is asked the slower way (`head`) and the count is left out.
    fn head_and_changes(&self, repo: &Repo, cancel: &AtomicBool) -> (Head, Option<usize>) {
        match self.git(repo, &["status", "--porcelain=v2", "--branch", "--untracked-files=normal"], false, 64, 30, cancel) {
            Ok(r) if r.code == Some(0) => match parse_status_v2(&r.stdout) {
                Some((head, changed)) => (head, Some(changed)),
                None => (self.head(repo), Some(r.stdout.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()).count())),
            },
            _ => (self.head(repo), None),
        }
    }

    /// The gate before a commit (card OA7-02, TM-07): the lines the staged change adds, and the message, are read with the rules of the commit hook. A secret stops the commit and the
    /// run; a change too big to be read (over a megabyte of added lines) or a read that did not finish stops it too, because a gate that gives way when it cannot look is not one.
    fn gate(&self, repo: &Repo, message: &str, cancel: &AtomicBool) -> Result<(), ToolError> {
        // The text as git printed it: `git` masks the login in a URL, which is exactly what one of the rules looks for.
        let r = self.git_raw(
            repo,
            &["diff", "--cached", "-U0", "--no-color", "--no-ext-diff", "--no-textconv", "--find-renames", "--src-prefix=a/", "--dst-prefix=b/"],
            false,
            gate::SCAN_CAP_BYTES / 1024,
            60,
            false,
            cancel,
        )?;
        if r.code != Some(0) {
            return Err(git_failure(&args::scrub_urls(&r.stderr), "", repo));
        }
        if r.truncated {
            return Err(ToolError::new(
                ErrorCode::ResourceLimit,
                format!("the staged change is larger than {} MB, so it cannot be read for secrets before it is committed, and it is not committed. Commit it in smaller parts (name fewer files), or leave it to the person to commit it themselves.", gate::SCAN_CAP_BYTES >> 20),
            ));
        }
        let mut hits = gate::scan_staged_diff(&r.stdout);
        hits.extend(gate::scan_message(message));
        if hits.is_empty() {
            // With the review tools on, lines that add a whole conflict (card OA7-05) stop the commit too: what a merge left in a file and nobody resolved.
            if self.review_enabled() {
                let markers = gate::scan_conflict_markers(&r.stdout);
                if !markers.is_empty() {
                    let details = json!({"findings": markers.iter().take(50).map(|h| json!({"file": h.file, "line": h.line, "kind": h.kind})).collect::<Vec<_>>()});
                    return Err(ToolError::new(ErrorCode::MergeConflict, gate::markers_blocked_message(&markers)).with_details(details));
                }
            }
            return Ok(());
        }
        let details = json!({"findings": hits.iter().take(50).map(|h| json!({"file": h.file, "line": h.line, "kind": h.kind})).collect::<Vec<_>>()});
        Err(ToolError::new(ErrorCode::SecretBlocked, gate::blocked_message(&hits)).with_details(details))
    }

    /// The repositories `git.repos` is asked about: all, or the ones in or around the folder named by `path`.
    fn repos_path_filter(&self, args: &Value) -> Result<Option<PathBuf>, ToolError> {
        match args.get("path").and_then(|v| v.as_str()).filter(|p| !p.is_empty()) {
            None => Ok(None),
            // The same rules as any path of a file tool: inside the folders Aqua may use (a relative one is inside the first).
            Some(p) => Ok(Some(self.guard.resolve(p, Op::Read)?.path)),
        }
    }

    /// `git.repos`: the repositories there are, which the person named and which were found, with the branch and the number of changed files of the first few.
    fn list_repos(&self, a: &Value, cancel: &AtomicBool) -> Result<ProviderOutput, ToolError> {
        let around = self.repos_path_filter(a)?;
        let all = self.snapshot(true);
        let chosen: Vec<Arc<Repo>> = all.iter().filter(|r| around.as_ref().is_none_or(|p| within(p, &r.root) || within(&r.root, p))).cloned().collect();
        // Asking git costs a moment for each repository: the first eight are asked (fewer with `limit`: the brief of a coding run asks about one).
        let asked = (arg_u64(a, "limit", 8) as usize).clamp(1, 8);
        let mut lines = Vec::new();
        let mut rows = Vec::new();
        for (i, r) in chosen.iter().enumerate() {
            let (head, changed) = if i < asked {
                let (head, changed) = self.head_and_changes(r, cancel);
                (Some(head), changed)
            } else {
                (None, None)
            };
            let where_ = head.as_ref().map(|h| h.label()).unwrap_or_else(|| "(not asked)".to_string());
            let state = match changed {
                Some(0) => "clean".to_string(),
                Some(n) => format!("{n} changed"),
                None => "?".to_string(),
            };
            let from = if r.worktree_of.is_some() { "worktree Aqua made" } else if r.found { "found" } else { "in repos.json" };
            lines.push(format!("{}  {}  {}  {}  {}  {}", r.entry.alias, display(&r.root), where_, state, if r.entry.trusted { "trusted" } else { "not trusted" }, from));
            rows.push(json!({
                "alias": r.entry.alias,
                "path": display(&r.root),
                "source": if r.worktree_of.is_some() { "worktree" } else if r.found { "found" } else { "configured" },
                "trusted": r.entry.trusted,
                "branch": head.as_ref().and_then(|h| h.branch()),
                "head": head.as_ref().map(|h| match h { Head::Branch(_) => "branch", Head::Unborn(_) => "unborn", Head::Detached(_) => "detached" }),
                "changed": changed,
            }));
        }
        let text = if lines.is_empty() {
            let roots: Vec<String> = self.guard.roots().into_iter().filter(|(_, _, access)| *access == Access::Rw).map(|(alias, path, _)| format!("@{alias} = {}", display(&path))).collect();
            format!(
                "No git repository was found{} in the folders Aqua may use ({}). Make a folder a repository with git init, or list one in data/tools/repos.json.{}",
                if around.is_some() { " there" } else { "" },
                roots.join("; "),
                if self.discover { "" } else { " (Looking for repositories is off: \"discover\" is false in repos.json.)" }
            )
        } else {
            format!("alias  path  where  files  trust  from\n{}", lines.join("\n"))
        };
        Ok(ProviderOutput { data: json!({"repos": rows, "discover": self.discover}), text, ..Default::default() })
    }

    /// A repo-relative path with forward slashes, checked by the file guard and kept inside the repo.
    fn rel_path(&self, repo: &Repo, p: &str, op: Op) -> Result<String, ToolError> {
        if p.is_empty() || p.contains('\0') || p.starts_with(':') {
            return Err(bad(format!("'{p}' is not a valid path")));
        }
        let full = if Path::new(p).is_absolute() { PathBuf::from(p) } else { repo.root.join(p) };
        let r = self.guard.resolve(&full.to_string_lossy(), op)?;
        let rel = r.path.strip_prefix(&repo.root).map_err(|_| ToolError::new(ErrorCode::PermissionDenied, format!("'{p}' is outside the repository")))?;
        let s = rel.to_string_lossy().replace('\\', "/");
        Ok(if s.is_empty() { ".".into() } else { s })
    }

    fn validate_args(&self, repo: &Repo, tool: &str, a: &Value) -> Result<(), ToolError> {
        for k in ["from", "to", "rev", "ref", "start_point", "base", "head"] {
            if let Some(v) = a.get(k).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    args::revision(v)?;
                }
            }
        }
        for k in ["branch", "source"] {
            if let Some(v) = a.get(k).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    if tool == "git.merge" && k == "source" {
                        args::revision(v)?;
                    } else {
                        args::branch_name(v)?;
                    }
                }
            }
        }
        if let Some(n) = a.get("name").and_then(|v| v.as_str()) {
            args::branch_name(n)?;
        }
        if let Some(r) = a.get("remote").and_then(|v| v.as_str()) {
            if !r.is_empty() {
                args::remote_name(r)?;
            }
        }
        for (k, max) in [("message", 5000usize), ("query", 200), ("author", 100), ("grep", 200), ("pattern", 100), ("since", 40)] {
            if let Some(v) = a.get(k).and_then(|v| v.as_str()) {
                args::text(v, max, k)?;
            }
        }
        let write = !matches!(tool, "git.status" | "git.diff" | "git.log" | "git.show" | "git.blame" | "git.grep");
        for p in arg_paths(a) {
            self.rel_path(repo, &p, if write { Op::Write } else { Op::Read })?;
        }
        if let Some(p) = a.get("path").and_then(|v| v.as_str()) {
            if !p.is_empty() {
                self.rel_path(repo, p, Op::Read)?;
            }
        }
        if let Some(since) = a.get("since").and_then(|v| v.as_str()) {
            if since.starts_with('-') {
                return Err(bad("since may not start with '-'"));
            }
        }
        match tool {
            "git.add" if arg_paths(a).is_empty() && !arg_bool(a, "all") => Err(bad("give paths, or all=true")),
            "git.commit" if arg_bool(a, "all") && !arg_paths(a).is_empty() => Err(bad("give paths (stage only those files) or all=true (stage every change), not both")),
            "git.push" if arg_str(a, "branch").is_empty() => Ok(()),
            "git.merge" if arg_str(a, "action") != "abort" && arg_str(a, "source").is_empty() => Err(bad("git.merge needs a source branch")),
            "git.worktree_add" if !arg_str(a, "name").is_empty() => worktree::valid_name(arg_str(a, "name")).map(|_| ()),
            "git.worktree_remove" => worktree::valid_name(arg_str(a, "name")).map(|_| ()),
            "git.resolve_conflict" => review::parse_choices(a).map(|_| ()),
            _ => Ok(()),
        }
    }

    fn dispatch(&self, tool: &str, a: &Value, cancel: &AtomicBool, cx: &CallCx) -> Result<ProviderOutput, ToolError> {
        if tool == "git.repos" {
            return self.list_repos(a, cancel);
        }
        let repo = self.repo(a)?;
        let repo: &Repo = &repo;
        self.validate_args(repo, tool, a)?;
        let paths: Vec<String> = arg_paths(a).iter().map(|p| self.rel_path(repo, p, Op::Read)).collect::<Result<_, _>>()?;
        let path_args = |mut v: Vec<String>| {
            if !paths.is_empty() {
                v.push("--".into());
                v.extend(paths.iter().cloned());
            }
            v
        };
        let run = |v: Vec<String>, network: bool, cap: usize, t: u64| -> Result<Ran, ToolError> {
            let refs: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
            self.ok(repo, &refs, network, cap, t, cancel)
        };
        let show = |r: Ran, empty: &str| -> Result<ProviderOutput, ToolError> {
            let text = if r.stdout.trim().is_empty() { empty.to_string() } else { r.stdout.trim_end().to_string() };
            Ok(ProviderOutput { data: json!({"repo": repo.entry.alias, "output": text, "truncated": r.truncated}), text: if r.truncated { format!("{text}\n… output cut") } else { text }, ..Default::default() })
        };
        match tool {
            "git.status" => {
                let mut v = vec!["status".to_string(), "-sb".into(), if arg_bool(a, "untracked") || a.get("untracked").is_none() { "--untracked-files=normal".into() } else { "--untracked-files=no".into() }];
                v = path_args(v);
                show(run(v, false, 128, 30)?, "Nothing to report.")
            }
            "git.diff" => {
                let mut v: Vec<String> = vec!["diff".into(), "--no-color".into(), "--no-ext-diff".into(), "--no-textconv".into()];
                match arg_str(a, "mode") {
                    "stat" => v.push("--stat".into()),
                    "names" => v.push("--name-status".into()),
                    _ => v.push(format!("-U{}", arg_u64(a, "context", 3).min(20))),
                }
                if arg_bool(a, "staged") {
                    v.push("--cached".into());
                }
                for k in ["from", "to"] {
                    if !arg_str(a, k).is_empty() {
                        v.push(arg_str(a, k).to_string());
                    }
                }
                show(run(path_args(v), false, 256, 60)?, "No changes.")
            }
            "git.log" => {
                let mut v: Vec<String> = vec!["log".into(), "--no-color".into(), format!("-n{}", arg_u64(a, "limit", 20).clamp(1, 200)), "--date=short".into(), "--pretty=format:%h %ad %an  %s".into()];
                if !arg_str(a, "author").is_empty() {
                    v.push(format!("--author={}", arg_str(a, "author")));
                }
                if !arg_str(a, "grep").is_empty() {
                    v.extend(["-F".into(), format!("--grep={}", arg_str(a, "grep"))]);
                }
                if !arg_str(a, "since").is_empty() {
                    v.push(format!("--since={}", arg_str(a, "since")));
                }
                if !arg_str(a, "ref").is_empty() {
                    v.push(arg_str(a, "ref").to_string());
                }
                if !arg_str(a, "path").is_empty() {
                    v.extend(["--".into(), self.rel_path(repo, arg_str(a, "path"), Op::Read)?]);
                }
                show(run(v, false, 128, 30)?, "No commits.")
            }
            "git.show" => {
                let mut v: Vec<String> = vec!["show".into(), "--no-color".into(), "--no-ext-diff".into(), "--no-textconv".into()];
                if arg_str(a, "mode") != "patch" {
                    v.push("--stat".into());
                }
                let rev = arg_str(a, "rev");
                if !arg_str(a, "path").is_empty() {
                    v.push(format!("{rev}:{}", self.rel_path(repo, arg_str(a, "path"), Op::Read)?));
                } else {
                    v.push(rev.to_string());
                }
                show(run(v, false, 256, 30)?, "Nothing to show.")
            }
            "git.blame" => {
                let mut v: Vec<String> = vec!["blame".into(), "-s".into(), "--date=short".into()];
                if a.get("start_line").is_some() {
                    let s = arg_u64(a, "start_line", 1);
                    let e = arg_u64(a, "end_line", s + 100).max(s).min(s + 500);
                    v.extend(["-L".into(), format!("{s},{e}")]);
                }
                if !arg_str(a, "rev").is_empty() {
                    v.push(arg_str(a, "rev").to_string());
                }
                v.extend(["--".into(), self.rel_path(repo, arg_str(a, "path"), Op::Read)?]);
                show(run(v, false, 128, 30)?, "No output.")
            }
            "git.branch" => {
                let mut v: Vec<String> = vec!["branch".into(), "--list".into(), "-a".into(), "-vv".into(), "--no-color".into()];
                if !arg_str(a, "pattern").is_empty() {
                    v.push(arg_str(a, "pattern").to_string());
                }
                limit_lines(show(run(v, false, 128, 30)?, "No branches.")?, arg_u64(a, "limit", 50) as usize)
            }
            "git.tag" => {
                let mut v: Vec<String> = vec!["tag".into(), "--list".into(), "--sort=-creatordate".into()];
                if !arg_str(a, "pattern").is_empty() {
                    v.push(arg_str(a, "pattern").to_string());
                }
                limit_lines(show(run(v, false, 64, 30)?, "No tags.")?, arg_u64(a, "limit", 50) as usize)
            }
            "git.stash_list" => show(run(vec!["stash".into(), "list".into()], false, 16, 30)?, "No stash entries."),
            "git.remote" => show(run(vec!["remote".into(), "-v".into()], false, 16, 30)?, "No remotes."),
            "git.grep" => {
                let mut v: Vec<String> = vec!["grep".into(), "-n".into(), "-I".into(), "--no-color".into(), "-F".into(), "-e".into(), arg_str(a, "query").to_string()];
                if !arg_str(a, "rev").is_empty() {
                    v.push(arg_str(a, "rev").to_string());
                }
                let r = self.git(repo, &path_args(v).iter().map(|s| s.as_str()).collect::<Vec<_>>(), false, 128, 30, cancel)?;
                match r.code {
                    Some(0) | Some(1) => limit_lines(show(r, "No matches.")?, arg_u64(a, "limit", 50) as usize),
                    _ => Err(git_failure(&r.stderr, &r.stdout, repo)),
                }
            }
            "git.add" => {
                let v = if arg_bool(a, "all") { vec!["add".to_string(), "-A".into()] } else { path_args(vec!["add".to_string()]) };
                run(v, false, 16, 60)?;
                let st = run(vec!["status".into(), "-sb".into()], false, 32, 30)?;
                show(st, "Staged.")
            }
            "git.unstage" => {
                run(path_args(vec!["restore".to_string(), "--staged".into()]), false, 16, 30)?;
                show(run(vec!["status".into(), "-sb".into()], false, 32, 30)?, "Unstaged.")
            }
            "git.commit" => {
                // `all` or `paths` stage first, in the same call (card OA5-01, TL-06): a commit that failed for "nothing is staged" was one more round, and a tool the list could cut (`git.add`) away from it.
                // The questions the host asked for the call covered both (see `classify`).
                let stage_all = arg_bool(a, "all");
                if stage_all {
                    run(vec!["add".to_string(), "-A".into()], false, 16, 60)?;
                } else if !paths.is_empty() {
                    run(path_args(vec!["add".to_string()]), false, 16, 60)?;
                }
                let staged = self.git(repo, &["diff", "--cached", "--quiet"], false, 4, 30, cancel)?;
                if staged.code == Some(0) && !arg_bool(a, "allow_empty") {
                    return Err(bad(if stage_all || !paths.is_empty() {
                        "nothing to commit: the files named have no changes that are not already committed"
                    } else {
                        "nothing is staged; give paths (or all=true) to stage and commit in one call, or use git.add first"
                    }));
                }
                // With the review tools on, nothing is committed while a file is still unmerged (card OA7-05). The files stay staged when it says no.
                if self.review_enabled() {
                    self.refuse_unmerged(repo, cancel)?;
                }
                // The lines this commit adds, and its message, are read for secrets before anything is committed (card OA7-02, TM-07). The files stay staged when it says no.
                // The trailer the person asked for is part of the message, but it is theirs and is not read.
                self.gate(repo, arg_str(a, "message"), cancel)?;
                let message = match &repo.entry.attribution {
                    Some(trailer) if !arg_str(a, "message").lines().any(|l| l.trim() == trailer.trim()) => format!("{}\n\n{}", arg_str(a, "message").trim_end(), trailer.trim()),
                    _ => arg_str(a, "message").to_string(),
                };
                let hooks = self.hooks_run_for(repo);
                let mut v = vec!["commit".to_string(), "-m".into(), message];
                if !hooks {
                    // What it always did: no hook of the repository runs, and `--no-verify` says so to git as well.
                    v.insert(1, "--no-verify".into());
                }
                if arg_bool(a, "allow_empty") {
                    v.push("--allow-empty".into());
                }
                let refs: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
                // With the repository's hooks on, the commit is the one call that lets them run: the same 60 seconds, the same Job Object, no prompt for input (GIT_TERMINAL_PROMPT=0).
                let made = self.git_raw(repo, &refs, false, 32, 60, hooks, cancel)?;
                if made.code != Some(0) {
                    let failure = git_failure(&args::scrub_urls(&made.stderr), &args::scrub_urls(&made.stdout), repo);
                    // A failure git has no word for, with the hooks on, is most likely a hook that said no: its first line is the reason.
                    return Err(if hooks && failure.code == ErrorCode::BackendError { ToolError::new(ErrorCode::BackendError, format!("the commit was not made: a hook of the repository stopped it ({})", failure.message)) } else { failure });
                }
                let head = run(vec!["log".into(), "-1".into(), "--pretty=format:%h %s".into()], false, 4, 30)?;
                Ok(ProviderOutput { data: json!({"repo": repo.entry.alias, "commit": head.stdout.trim()}), text: format!("Committed {}", head.stdout.trim()), ..Default::default() })
            }
            "git.switch" => {
                let branch = arg_str(a, "branch");
                let mut v: Vec<String> = vec!["switch".into()];
                if arg_bool(a, "create") {
                    v.push("-c".into());
                }
                v.push(branch.to_string());
                if arg_bool(a, "create") && !arg_str(a, "start_point").is_empty() {
                    v.push(arg_str(a, "start_point").to_string());
                }
                run(v, false, 16, 60)?;
                Ok(ProviderOutput { data: json!({"repo": repo.entry.alias, "branch": branch}), text: format!("On branch {branch}."), ..Default::default() })
            }
            "git.tag_create" => {
                let mut v: Vec<String> = vec!["tag".into()];
                if !arg_str(a, "message").is_empty() {
                    v.extend(["-a".into(), "-m".into(), arg_str(a, "message").to_string()]);
                }
                v.push(arg_str(a, "name").to_string());
                if !arg_str(a, "rev").is_empty() {
                    v.push(arg_str(a, "rev").to_string());
                }
                run(v, false, 8, 30)?;
                Ok(ProviderOutput { data: json!({"tag": arg_str(a, "name")}), text: format!("Created tag {}.", arg_str(a, "name")), ..Default::default() })
            }
            "git.stash" => {
                let idx = format!("stash@{{{}}}", arg_u64(a, "index", 0));
                let v: Vec<String> = match arg_str(a, "op") {
                    "apply" => vec!["stash".into(), "apply".into(), idx],
                    "pop" => vec!["stash".into(), "pop".into(), idx],
                    _ => {
                        let mut v = vec!["stash".to_string(), "push".into()];
                        if arg_bool(a, "include_untracked") {
                            v.push("--include-untracked".into());
                        }
                        if !arg_str(a, "message").is_empty() {
                            v.extend(["-m".into(), arg_str(a, "message").to_string()]);
                        }
                        v
                    }
                };
                show(run(v, false, 32, 60)?, "Done.")
            }
            "git.merge" => {
                if arg_str(a, "action") == "abort" {
                    run(vec!["merge".into(), "--abort".into()], false, 8, 30)?;
                    return Ok(ProviderOutput { text: "Merge aborted.".into(), data: json!({"aborted": true}), ..Default::default() });
                }
                let mut v: Vec<String> = vec!["merge".into(), "--no-edit".into()];
                v.push(match arg_str(a, "ff") {
                    "only" => "--ff-only".into(),
                    "never" => "--no-ff".into(),
                    _ => "--ff".into(),
                });
                v.push(arg_str(a, "source").to_string());
                let refs: Vec<&str> = v.iter().map(|s| s.as_str()).collect();
                let r = self.git(repo, &refs, false, 64, 60, cancel)?;
                if r.code == Some(0) {
                    return show(r, "Merged.");
                }
                let conflicts = self.git(repo, &["diff", "--name-only", "--diff-filter=U"], false, 8, 30, cancel).map(|c| c.stdout.trim().to_string()).unwrap_or_default();
                if !conflicts.is_empty() {
                    let more = if self.review_enabled() { " git.conflicts shows the hunks of each file, and git.resolve_conflict resolves one by an edit you approve." } else { "" };
                    return Err(ToolError::new(ErrorCode::MergeConflict, format!("the merge stopped with conflicts in: {}. Fix them and commit, or use git.merge action=abort.{more}", conflicts.replace('\n', ", "))).with_details(json!({"files": conflicts.lines().collect::<Vec<_>>()})));
                }
                Err(git_failure(&r.stderr, &r.stdout, repo))
            }
            "git.fetch" => {
                let mut v: Vec<String> = vec!["fetch".into()];
                if arg_bool(a, "prune") {
                    v.push("--prune".into());
                }
                if arg_bool(a, "tags") {
                    v.push("--tags".into());
                }
                v.push(remote_or_origin(a));
                let r = run(v, true, 32, 240)?;
                let text = format!("{}{}", r.stdout.trim(), r.stderr.trim());
                Ok(ProviderOutput { data: json!({"repo": repo.entry.alias}), text: if text.is_empty() { "Already up to date.".into() } else { text }, ..Default::default() })
            }
            "git.pull" => {
                let r = run(vec!["pull".into(), "--ff-only".into(), remote_or_origin(a)], true, 32, 240)?;
                show(r, "Already up to date.")
            }
            "git.push" => {
                let branch = if arg_str(a, "branch").is_empty() {
                    match self.head(repo) {
                        Head::Branch(name) | Head::Unborn(name) => name,
                        // `refs/heads/HEAD:refs/heads/HEAD` is what the old reading of "HEAD" would have pushed; a push of no branch names the branch.
                        Head::Detached(sha) => return Err(bad(format!("HEAD is on no branch (detached at {sha}): say which branch to push with branch=..., or switch to one first"))),
                    }
                } else {
                    arg_str(a, "branch").to_string()
                };
                args::branch_name(&branch)?;
                let mut v: Vec<String> = vec!["push".into()];
                if arg_bool(a, "set_upstream") {
                    v.push("-u".into());
                }
                v.extend([remote_or_origin(a), format!("refs/heads/{branch}:refs/heads/{branch}")]);
                let r = run(v, true, 32, 240)?;
                let text = format!("{}{}", r.stdout.trim(), r.stderr.trim());
                Ok(ProviderOutput { data: json!({"repo": repo.entry.alias, "branch": branch}), text: if text.is_empty() { format!("Pushed {branch}.") } else { text }, ..Default::default() })
            }
            "git.discard" => {
                let backup = self.backup(repo, cancel)?;
                run(path_args(vec!["restore".to_string()]), false, 16, 60)?;
                Ok(ProviderOutput { data: json!({"backup": backup}), text: format!("Discarded changes to {} file(s).{}", paths.len(), backup_note(&backup)), ..Default::default() })
            }
            "git.reset" => {
                let mode = match arg_str(a, "mode") {
                    "soft" => "--soft",
                    "hard" => "--hard",
                    _ => "--mixed",
                };
                let backup = if mode == "--hard" { self.backup(repo, cancel)? } else { None };
                let before = run(vec!["rev-parse".into(), "--short".into(), "HEAD".into()], false, 4, 30).map(|r| r.stdout.trim().to_string()).unwrap_or_default();
                run(vec!["reset".into(), mode.into(), arg_str(a, "to").to_string()], false, 16, 60)?;
                Ok(ProviderOutput { data: json!({"backup": backup, "previous_head": before}), text: format!("Reset to {} ({}). Previous head was {before}.{}", arg_str(a, "to"), &mode[2..], backup_note(&backup)), ..Default::default() })
            }
            "git.branch_delete" => {
                let branch = arg_str(a, "branch");
                if repo.entry.protected_branches.iter().any(|p| wildcard(p, branch)) {
                    return Err(ToolError::new(ErrorCode::ProtectedRef, format!("'{branch}' is a protected branch and is never deleted by these tools")));
                }
                if self.head(repo).branch() == Some(branch) {
                    return Err(bad("cannot delete the branch you are on"));
                }
                let tip = run(vec!["rev-parse".into(), "--short".into(), format!("refs/heads/{branch}")], false, 4, 30)?.stdout.trim().to_string();
                run(vec!["branch".into(), if arg_bool(a, "force") { "-D" } else { "-d" }.into(), branch.to_string()], false, 8, 30)?;
                Ok(ProviderOutput { data: json!({"branch": branch, "tip": tip}), text: format!("Deleted branch {branch} (it was at {tip}; restore with git.switch create=true start_point={tip})."), ..Default::default() })
            }
            "git.stash_drop" => {
                let r = run(vec!["stash".into(), "drop".into(), format!("stash@{{{}}}", arg_u64(a, "index", 0))], false, 8, 30)?;
                show(r, "Dropped.")
            }
            // The worktree tools and the review tools check their own switch (card OA7-05).
            "git.worktree_add" => self.worktree_add(repo, a, cancel),
            "git.worktree_list" => self.worktree_list(repo, cancel),
            "git.worktree_remove" => self.worktree_remove(repo, a, cancel),
            "git.conflicts" => self.conflicts(repo, a, cancel),
            "git.resolve_conflict" => self.resolve_conflict(repo, a, cx, cancel),
            "git.pr_draft" => self.pr_draft(repo, a, cancel),
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }

    /// Save uncommitted tracked changes as a commit under `refs/aqua/backup/` so a discard or a hard reset can be undone.
    fn backup(&self, repo: &Repo, cancel: &AtomicBool) -> Result<Option<String>, ToolError> {
        let r = self.git(repo, &["stash", "create"], false, 4, 60, cancel)?;
        let sha = r.stdout.trim().to_string();
        if r.code != Some(0) || sha.is_empty() {
            return Ok(None); // nothing to save: the working tree is clean
        }
        let name = format!("refs/aqua/backup/{}", &sha[..sha.len().min(12)]);
        self.ok(repo, &["update-ref", &name, &sha], false, 4, 30, cancel)?;
        Ok(Some(sha))
    }
}

/// The branch and the number of changed files from `git status --porcelain=v2 --branch`: `# branch.oid <commit>` (or `(initial)` before the first commit) and `# branch.head <name>` (or
/// `(detached)`) come first, then one line for each changed, renamed, unmerged, untracked or ignored file. None when the lines that say where HEAD is are not there.
fn parse_status_v2(text: &str) -> Option<(Head, usize)> {
    let (mut oid, mut name): (Option<&str>, Option<&str>) = (None, None);
    let mut changed = 0usize;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("# branch.oid ") {
            oid = Some(v.trim());
        } else if let Some(v) = line.strip_prefix("# branch.head ") {
            name = Some(v.trim());
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            changed += 1;
        }
    }
    let (oid, name) = (oid?, name?);
    let head = match (name, oid) {
        ("(detached)", sha) => Head::Detached(sha.chars().take(7).collect()),
        (branch, "(initial)") => Head::Unborn(branch.to_string()),
        (branch, _) => Head::Branch(branch.to_string()),
    };
    Some((head, changed))
}

fn backup_note(b: &Option<String>) -> String {
    match b {
        Some(sha) => format!(" A backup of the changes is kept: recover it with git.stash op=apply after `git stash store`, or `git checkout {} -- .` (commit {sha}).", &sha[..sha.len().min(12)]),
        None => String::new(),
    }
}

fn remote_or_origin(a: &Value) -> String {
    let r = arg_str(a, "remote");
    if r.is_empty() { "origin".into() } else { r.into() }
}

fn limit_lines(mut out: ProviderOutput, n: usize) -> Result<ProviderOutput, ToolError> {
    let lines: Vec<&str> = out.text.lines().collect();
    if lines.len() > n {
        out.text = format!("{}\n… {} more", lines[..n].join("\n"), lines.len() - n);
    }
    Ok(out)
}

/// Turn git's stderr into a specific error.
fn git_failure(stderr: &str, stdout: &str, repo: &Repo) -> ToolError {
    let all = format!("{stderr}\n{stdout}");
    let low = all.to_ascii_lowercase();
    let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or_else(|| stdout.lines().next().unwrap_or("git failed")).trim().to_string();
    let has = |m: &str| low.contains(m);
    let code = if has("not a git repository") {
        ErrorCode::NotFound
    } else if has("would be overwritten") || has("please commit your changes or stash") || has("local changes") && has("overwritten") {
        ErrorCode::DirtyWorktree
    } else if has("conflict") || has("automatic merge failed") {
        ErrorCode::MergeConflict
    } else if has("non-fast-forward") || has("fetch first") || has("not possible to fast-forward") || has("diverging branches") {
        ErrorCode::NonFastForward
    } else if has("authentication failed") || has("could not read username") || has("terminal prompts disabled") || has("permission denied (publickey)") {
        ErrorCode::AuthRequired
    } else if has("could not resolve host") || has("unable to access") || has("connection timed out") || has("connection refused") {
        ErrorCode::Unavailable
    } else if has("did not match any") || has("unknown revision") || has("bad revision") || has("not a valid object") || has("no such") || has("does not exist") {
        ErrorCode::NotFound
    } else if has("already exists") {
        ErrorCode::AlreadyExists
    } else if has("nothing to commit") || has("no changes added") {
        ErrorCode::InvalidArguments
    } else {
        ErrorCode::BackendError
    };
    ToolError::new(code, format!("{first} (repo {})", repo.entry.alias))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(repos: Vec<RepoEntry>) -> GitConfig {
        GitConfig { repos, git_path: None, discover: true }
    }

    fn entry(alias: &str) -> RepoEntry {
        RepoEntry { alias: alias.into(), path: "x".into(), trusted: false, protected_branches: default_protected(), author_name: None, author_email: None, hooks: HooksMode::Off, attribution: None }
    }

    #[test]
    fn config_validation_rejects_bad_aliases_duplicates_and_authors() {
        assert!(cfg(vec![entry("aqua"), entry("lib_2")]).validate().is_ok());
        for bad in ["", "a b", "a/b", &"x".repeat(41)] {
            assert!(cfg(vec![entry(bad)]).validate().is_err(), "{bad}");
        }
        assert!(cfg(vec![entry("a"), entry("A")]).validate().is_err());
        let mut e = entry("a");
        e.author_email = Some("x\ny".into());
        assert!(cfg(vec![e]).validate().is_err());
        assert!(serde_json::from_str::<GitConfig>(r#"{"repos": [], "hooks": "enabled"}"#).is_err());
    }

    #[test]
    fn the_options_of_a_call_carry_the_name_and_path_flags_and_the_hooks_stay_off_unless_asked_for() {
        let dir = std::env::temp_dir().join(format!("aqua_gitflags_{}", std::process::id()));
        let repo = dir.join("r");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let guard = Arc::new(PathGuard::new(crate::fs::FsConfig::with_workspace(dir.to_string_lossy().to_string()), vec![]).unwrap());
        let p = GitProvider::new(GitConfig { repos: vec![RepoEntry { path: repo.to_string_lossy().to_string(), ..entry("r") }], git_path: None, discover: false }, guard, &dir.join("aqua")).unwrap();
        let r = p.0.explicit[0].clone();
        let off = p.0.base_args(&r, false);
        let has = |v: &[String], c: &str| v.windows(2).any(|w| w[0] == "-c" && w[1] == c);
        assert!(has(&off, "core.quotepath=false") && has(&off, "core.longpaths=true"), "{off:?}");
        assert!(off.iter().any(|a| a.starts_with("core.hooksPath=") && a.len() > "core.hooksPath=".len()), "the hooks point at the empty folder: {off:?}");
        let on = p.0.base_args(&r, true);
        assert!(!on.iter().any(|a| a.starts_with("core.hooksPath")) && has(&on, "core.fsmonitor=false") && on.iter().all(|a| !a.is_empty()), "{on:?}");
        // The discovery switch is the file's: with it off the provider does not look and the named ones are all there is.
        assert_eq!(p.0.snapshot(true).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hooks_run_needs_trust_and_an_attribution_must_be_a_trailer() {
        let mut e = entry("a");
        e.hooks = HooksMode::Run;
        assert!(cfg(vec![e.clone()]).validate().unwrap_err().contains("trusted"), "a hook is code from the repository");
        e.trusted = true;
        assert!(cfg(vec![e.clone()]).validate().is_ok());
        for bad in ["no colon", "Key:", "Key: a
b", ""] {
            e.attribution = Some(bad.into());
            assert!(cfg(vec![e.clone()]).validate().is_err(), "{bad:?}");
        }
        e.attribution = Some("Co-Authored-By: Aqua <aqua@example.com>".into());
        assert!(cfg(vec![e]).validate().is_ok());
        // The words of the file: "run" and "off"; anything else is refused with the file.
        let parsed: GitConfig = serde_json::from_str(r#"{"repos": [{"alias": "a", "path": "x", "trusted": true, "hooks": "run"}]}"#).unwrap();
        assert_eq!(parsed.repos[0].hooks, HooksMode::Run);
        assert_eq!(serde_json::from_str::<GitConfig>(r#"{"repos": [{"alias": "a", "path": "x"}]}"#).unwrap().repos[0].hooks, HooksMode::Off);
        assert!(serde_json::from_str::<GitConfig>(r#"{"repos": [{"alias": "a", "path": "x", "hooks": "enabled"}]}"#).is_err());
    }

    #[test]
    fn looking_for_repositories_is_on_unless_the_file_says_it_is_not() {
        assert!(GitConfig::default().discover);
        assert!(serde_json::from_str::<GitConfig>(r#"{"repos": []}"#).unwrap().discover, "an old file without the key");
        assert!(!serde_json::from_str::<GitConfig>(r#"{"repos": [], "discover": false}"#).unwrap().discover);
        assert!(serde_json::to_string(&GitConfig::default()).unwrap().contains("\"discover\":true"), "the default file shows the switch");
    }

    #[test]
    fn one_status_says_where_head_is_and_how_many_files_changed() {
        let on_branch = "# branch.oid 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b
# branch.head feature/x
# branch.upstream origin/feature/x
# branch.ab +1 -0
1 .M N... 100644 100644 100644 a b a.txt
? new.txt
";
        assert_eq!(parse_status_v2(on_branch), Some((Head::Branch("feature/x".into()), 2)));
        assert_eq!(parse_status_v2("# branch.oid (initial)
# branch.head main
? x.txt
"), Some((Head::Unborn("main".into()), 1)));
        assert_eq!(parse_status_v2("# branch.oid 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b
# branch.head (detached)
"), Some((Head::Detached("1a2b3c4".into()), 0)));
        assert_eq!(parse_status_v2("1 .M N... 100644 100644 100644 a b a.txt
"), None, "no word on where HEAD is: asked the slower way");
        assert_eq!(parse_status_v2(""), None);
    }

    #[test]
    fn where_head_is_is_said_in_words_and_only_a_branch_has_a_name() {
        assert_eq!(Head::Branch("main".into()).label(), "main");
        assert_eq!(Head::Unborn("main".into()).label(), "main (no commits yet)");
        assert_eq!(Head::Detached("1a2b3c4".into()).label(), "detached HEAD at 1a2b3c4");
        assert_eq!((Head::Branch("a/b".into()).branch(), Head::Unborn("main".into()).branch(), Head::Detached("x".into()).branch()), (Some("a/b"), Some("main"), None));
    }

    #[test]
    fn git_errors_map_to_specific_codes() {
        let repo = Repo { entry: entry("r"), root: PathBuf::from("."), found: false, worktree_of: None };
        let cases = [
            ("fatal: not a git repository (or any of the parent directories): .git", ErrorCode::NotFound),
            ("error: Your local changes to the following files would be overwritten by checkout:", ErrorCode::DirtyWorktree),
            ("CONFLICT (content): Merge conflict in a.txt\nAutomatic merge failed", ErrorCode::MergeConflict),
            ("! [rejected] main -> main (non-fast-forward)", ErrorCode::NonFastForward),
            ("fatal: Not possible to fast-forward, aborting.", ErrorCode::NonFastForward),
            ("fatal: could not read Username for 'https://github.com': terminal prompts disabled", ErrorCode::AuthRequired),
            ("fatal: unable to access 'https://x/': Could not resolve host: x", ErrorCode::Unavailable),
            ("error: pathspec 'nope' did not match any file(s) known to git", ErrorCode::NotFound),
            ("fatal: a branch named 'x' already exists", ErrorCode::AlreadyExists),
            ("something else broke", ErrorCode::BackendError),
        ];
        for (stderr, want) in cases {
            assert_eq!(git_failure(stderr, "", &repo).code, want, "{stderr}");
        }
    }

    #[test]
    fn every_tool_has_a_valid_manifest_and_a_sensible_class() {
        let dir = std::env::temp_dir().join(format!("aqua_gitmanifests_{}", std::process::id()));
        let repo = dir.join("r");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let guard = Arc::new(PathGuard::new(crate::fs::FsConfig::with_workspace(dir.to_string_lossy().to_string()), vec![]).unwrap());
        let p = GitProvider::new(GitConfig { repos: vec![RepoEntry { path: repo.to_string_lossy().to_string(), ..entry("r") }], git_path: None, discover: true }, guard, &dir.join("aqua")).unwrap();
        let m = p.manifests();
        // 25 that are always on offer, and six that wait for a switch (card OA7-05): the three worktree tools, and the two conflict tools and the pull request draft.
        assert_eq!(m.len(), 31);
        for t in &m {
            t.check().unwrap_or_else(|e| panic!("{e}"));
        }
        let by = |n: &str| m.iter().find(|t| t.name == n).unwrap();
        assert!(by("git.push").classes.contains(&PermClass::ExternalEffect));
        assert!(by("git.reset").classes.contains(&PermClass::Destructive) && by("git.discard").classes.contains(&PermClass::Destructive));
        assert!(!m.iter().any(|t| t.name.contains("force") || t.name.contains("rebase") || t.name.contains("amend")));
        assert_eq!(by("git.status").classes, vec![PermClass::Read]);
        // The new tools (card OA7-05): what only looks is a read; a worktree and a resolution write; removing a worktree is destructive and asks every time.
        for read in ["git.worktree_list", "git.conflicts", "git.pr_draft"] {
            assert_eq!(by(read).classes, vec![PermClass::Read], "{read}");
            assert!(by(read).annotations.read_only && by(read).untrusted_output, "{read}");
        }
        assert_eq!(by("git.worktree_add").classes, vec![PermClass::Write]);
        assert_eq!(by("git.resolve_conflict").classes, vec![PermClass::Write]);
        assert!(by("git.worktree_remove").classes.contains(&PermClass::Destructive) && by("git.worktree_remove").annotations.destructive);
        // None of them is on offer by default: a provider that was not given the switches lists none of them as available.
        for tool in ["git.worktree_add", "git.worktree_list", "git.worktree_remove", "git.conflicts", "git.resolve_conflict", "git.pr_draft"] {
            assert!(matches!(p.availability(tool), aqua_tool_host::Availability::Unavailable(_)), "{tool}");
        }
        assert_eq!(p.availability("git.status"), aqua_tool_host::Availability::Available);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

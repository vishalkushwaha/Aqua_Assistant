//! Path rules for the filesystem tools.
//!
//! Every path the model supplies is turned into one canonical absolute path and checked before
//! anything touches the disk: it must sit inside a configured root, must not be a UNC path,
//! alternate data stream or device name, and must not be one of Aqua's own protected files or a
//! well-known credential store. Symlinks and junctions are resolved first, so a link inside a
//! root that points outside it is refused. The check and the operation are two steps, so a
//! same-user process that swaps a directory for a link in between can still win that race;
//! the guard narrows the window by operating on the canonical path it returns.

use aqua_tool_host::{ErrorCode, ToolError};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::{PoisonError, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Ro,
    Rw,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Root {
    /// Short name used as `@alias/relative/path`.
    pub alias: String,
    pub path: String,
    pub access: Access,
    /// A trusted root: writes inside it run without a prompt (they are backed up and undoable).
    #[serde(default)]
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsConfig {
    pub roots: Vec<Root>,
    /// File names (with `*` and `?`) whose contents are likely secrets: reading or writing them asks.
    #[serde(default = "default_sensitive")]
    pub sensitive_names: Vec<String>,
    #[serde(default = "default_limits")]
    pub limits: Limits,
    /// Where the tool host keeps the whole text of a long tool result (card OA5-02): the folder behind the built-in read-only root `@out`. Not set: a folder called
    /// `tool-output` beside Aqua's `data` folder (one for each data folder). It must not be inside Aqua's own files; an empty text turns the keeping off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spill_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub read_max_bytes: u64,
    pub write_max_bytes: u64,
    pub edit_max_file_bytes: u64,
    pub list_max_entries: usize,
    pub find_max_files: usize,
    pub find_max_scan_bytes: u64,
    pub hash_max_bytes: u64,
    /// How many characters of text a page of `fs.read` (its numbered lines) and the lines of a search or a listing may have when the call does not say (card OA5-03, CU-03): what the
    /// model is shown of the newest result of a tool, less the room for the lines around the text. The core sets it from the model's window when it starts, and a `fs.json` that has it
    /// keeps its own number. A file written before the field was there has the default.
    #[serde(default = "default_view_chars")]
    pub view_chars: usize,
}

/// What a page of text is, in characters, in the window of this machine (6,144 tokens: 900 for the newest result, three characters a token, less 300 for the lines around the text).
pub const DEFAULT_VIEW_CHARS: usize = 2400;

fn default_view_chars() -> usize {
    DEFAULT_VIEW_CHARS
}

fn default_limits() -> Limits {
    Limits {
        read_max_bytes: 1 << 20,
        write_max_bytes: 1 << 20,
        edit_max_file_bytes: 4 << 20,
        list_max_entries: 500,
        find_max_files: 20_000,
        find_max_scan_bytes: 200 << 20,
        hash_max_bytes: 64 << 20,
        view_chars: DEFAULT_VIEW_CHARS,
    }
}

/// Names whose contents are likely secrets. The last line is card OA6-06 (SE-04): key stores and login files of other programs (`.ppk`, `.gpg`, `.jks`, `.keystore`, `.pypirc`,
/// `.dockercfg`, `.git-credentials`, `.htpasswd`, the GitHub CLI's `hosts.yml`), and files that are called a token or a password: by the end of the name (`.aqua_api_token`, `access_token`,
/// `db_password`), not by any part of it, because `tokenizer.rs`, `token.rs` and `PasswordField.tsx` are source files that a search and a read must keep seeing. A folder's own `fs.json` keeps
/// the list it was written with: a name added here reaches a new setup, and an old one by adding it to `sensitive_names` there.
fn default_sensitive() -> Vec<String> {
    [
        "*.env", ".env", ".env.*", "*.tfvars", "*secret*", "*credential*", "*.kdbx", "*.pem", "*.key", "*.pfx", "*.p12", "*.ovpn", ".pgpass", "id_rsa*", "id_ed25519*", "id_ecdsa*", "terraform.tfstate*", ".npmrc", ".netrc",
        "*token", "*password", "*passwords", "*.ppk", "*.gpg", ".pypirc", ".dockercfg", ".git-credentials", ".htpasswd", "*.jks", "*.keystore", "hosts.yml",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl FsConfig {
    /// One trusted read-write root at `workspace`.
    pub fn with_workspace(workspace: impl Into<String>) -> Self {
        Self {
            roots: vec![Root { alias: "ws".into(), path: workspace.into(), access: Access::Rw, trusted: true }],
            sensitive_names: default_sensitive(),
            limits: default_limits(),
            spill_dir: None,
        }
    }
}

/// The name of the built-in read-only root that shows the folder of kept tool output (`@out/<file>`).
pub const OUT_ALIAS: &str = "out";

/// The largest file of that folder a search opens (the host keeps at most 8 MiB in one file): the configured read limit applies to every other file.
const OUT_FILE_MAX_BYTES: u64 = 16 << 20;

/// Directory names whose contents are credential stores. They are never readable or writable.
pub(crate) const PROTECTED_DIR_NAMES: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube", ".azure", ".config/gcloud", ".docker"];

/// Names (a file or a folder, or a run of folders) that decide what other programs run, trust or ask. They can be read, but file
/// tools never write them: an injected instruction that can write an editor setting which approves commands, an MCP server entry,
/// an agent hook, a shell profile or a start-up item has turned itself into a standing one. The audit of other agents found this
/// is how several of them were taken over (an auto-approve switch written into `.vscode/settings.json`, hooks and MCP entries in
/// a project's own tool folders).
const WRITE_PROTECTED_NAMES: &[&str] = &[
    ".vscode", ".idea", ".cargo", ".devcontainer", ".husky", ".claude", ".codex", ".cursor", ".windsurf", ".gemini", ".continue", ".roo",
    ".cline", ".mcp.json", ".npmrc", ".gitconfig", "profile.ps1", "microsoft.powershell_profile.ps1", "microsoft.vscode_profile.ps1",
    "start menu/programs/startup",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub root_alias: String,
    pub trusted: bool,
    /// The file name looks like it holds a secret.
    pub sensitive: bool,
    /// Writing here can run code later (an executable, a script, a build or CI file).
    pub executable_kind: bool,
}

pub struct PathGuard {
    roots: Vec<(Root, PathBuf)>,
    /// The project folder, added while "full access" is on (see `set_project_root`). Not part of the saved configuration.
    project: RwLock<Option<(Root, PathBuf)>>,
    /// Folders the person allowed while Aqua runs (see `grant_folder`): never trusted, gone when Aqua restarts, not part of the saved configuration.
    grants: RwLock<Vec<(Root, PathBuf)>>,
    /// The folder of kept tool output (see `set_out_root`): read-only, never trusted, not part of the saved configuration. A path inside it is always judged as `@out`, whichever root also holds it.
    out: RwLock<Option<(Root, PathBuf)>>,
    /// Places `grant_folder` never hands out (Windows, the programs, the person's whole profile and its AppData).
    kept: RwLock<Vec<Kept>>,
    config: FsConfig,
    protected: Vec<PathBuf>,
    /// `protected`, split into components and folded for comparing (see `fold`): what a walk checks each entry against, made once.
    protected_norm: Vec<Vec<String>>,
}

/// The most folders the person can have allowed at one time (see `PathGuard::grant_folder`).
pub const MAX_GRANTS: usize = 16;

/// A place that is never allowed as a folder of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// What it is, for the sentence that says no.
    pub what: String,
    pub path: PathBuf,
    /// The place itself and everything inside it is refused.
    pub inside: bool,
    /// A folder that holds the place (so the place would be inside it) is refused too.
    pub holding: bool,
}

/// Windows, the program folders, the person's whole user folder and its AppData: the places a folder named in a sentence must not be, whoever names it.
fn default_kept() -> Vec<Kept> {
    let mut v = Vec::new();
    for (what, var) in [
        ("Windows", "SystemRoot"),
        ("Windows", "windir"),
        ("the program folders", "ProgramFiles"),
        ("the program folders", "ProgramFiles(x86)"),
        ("the program folders", "ProgramW6432"),
        ("the program data folder", "ProgramData"),
    ] {
        if let Some(path) = std::env::var_os(var).filter(|x| !x.is_empty()).and_then(|x| canonical_lenient(Path::new(&x))) {
            v.push(Kept { what: what.into(), path, inside: true, holding: true });
        }
    }
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|x| !x.is_empty()).and_then(|x| canonical_lenient(Path::new(&x))) {
        v.push(Kept { what: "AppData (the settings and saved logins of your programs)".into(), path: profile.join("AppData"), inside: true, holding: false });
        v.push(Kept { what: "your whole user folder".into(), path: profile, inside: false, holding: true });
    }
    v
}

/// What `PathGuard::plan_grant` found out about a folder it was asked to allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantPlan {
    /// The folder, in canonical form (where it will be, when it is not there yet).
    pub path: PathBuf,
    /// It is there. When it is not, it is made (one level, inside a folder that exists).
    pub exists: bool,
    /// The alias of a root that already holds it with at least this access: nothing needs to be allowed.
    pub already: Option<String>,
}

impl PathGuard {
    /// `protected` are Aqua's own files and folders (tokens, policy, audit, trash): no tool may
    /// name them, whichever root they sit in.
    pub fn new(config: FsConfig, protected: Vec<PathBuf>) -> Result<Self, String> {
        let mut roots = Vec::new();
        for r in &config.roots {
            if r.alias.is_empty() || !r.alias.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(format!("root alias '{}' must be letters, digits, '_' or '-'", r.alias));
            }
            let expanded = expand_env(&r.path);
            if expanded.starts_with("\\\\") || expanded.starts_with("//") {
                return Err(format!("root '{}' is a network path, which is not allowed", r.alias));
            }
            let p = PathBuf::from(&expanded);
            std::fs::create_dir_all(&p).map_err(|e| format!("root '{}' ({expanded}): {e}", r.alias))?;
            let canon = canonical(&p).map_err(|e| format!("root '{}' ({expanded}): {e}", r.alias))?;
            roots.push((r.clone(), canon));
        }
        if roots.is_empty() {
            return Err("at least one root is required".into());
        }
        let protected: Vec<PathBuf> = protected.into_iter().filter_map(|p| canonical_lenient(&p)).collect();
        let protected_norm = protected.iter().map(|p| norm_components(p)).collect();
        Ok(Self { roots, project: RwLock::new(None), grants: RwLock::new(Vec::new()), out: RwLock::new(None), kept: RwLock::new(default_kept()), config, protected, protected_norm })
    }

    /// The configured roots, then the project root when there is one.
    fn all_roots(&self) -> Vec<(Root, PathBuf)> {
        let mut all = self.roots.clone();
        if let Some(p) = self.project.read().unwrap_or_else(PoisonError::into_inner).clone() {
            all.push(p);
        }
        if let Some(o) = self.out.read().unwrap_or_else(PoisonError::into_inner).clone() {
            all.push(o);
        }
        all.extend(self.grants.read().unwrap_or_else(PoisonError::into_inner).iter().cloned());
        all
    }

    /// The root that judges `target`: the folder of kept tool output when the path is inside it (even when another root holds it too: a kept page must not be read as a file of a
    /// trusted workspace, nor written through a read-write one), else the first root that holds it.
    fn root_of(&self, roots: &[(Root, PathBuf)], target: &Path) -> Option<(Root, PathBuf)> {
        if let Some((r, p)) = self.out.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
            if within(target, p) {
                return Some((r.clone(), p.clone()));
            }
        }
        roots.iter().find(|(_, rp)| within(target, rp)).cloned()
    }

    /// Show `dir` to the file tools as the built-in read-only root `@out`: the folder where the tool host keeps the full text of long tool results (card OA5-02). It is made when it is
    /// not there. The root is never trusted (what was kept is output of tools, some of it from the web) and is not in `fs.json`. Refused: a name `out` that a configured root already
    /// has, a whole drive, and a folder that is inside Aqua's own files or holds them. Returns the folder in canonical form.
    pub fn set_out_root(&self, dir: &Path) -> Result<PathBuf, String> {
        if self.roots.iter().any(|(r, _)| r.alias == OUT_ALIAS) {
            return Err(format!("fs.json has a root called '{OUT_ALIAS}', a name kept for the folder of kept tool output: rename it, or the full text of long output is not kept"));
        }
        // The place is judged before anything is made: a folder inside Aqua's own files is not created just to be refused.
        let planned = canonical_lenient(dir).ok_or_else(|| format!("the folder of kept tool output {} cannot be resolved", dir.display()))?;
        if planned.parent().is_none() {
            return Err(format!("{} is a whole drive, not a folder for kept tool output", planned.display()));
        }
        if self.protected.iter().any(|p| within(&planned, p) || within(p, &planned)) {
            return Err(format!("{} is inside Aqua's own files or holds them, so it cannot be the folder of kept tool output", display(&planned)));
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("the folder of kept tool output {}: {e}", dir.display()))?;
        let canon = canonical(dir).map_err(|e| format!("the folder of kept tool output {}: {e}", dir.display()))?;
        *self.out.write().unwrap_or_else(PoisonError::into_inner) = Some((Root { alias: OUT_ALIAS.into(), path: canon.to_string_lossy().into(), access: Access::Ro, trusted: false }, canon.clone()));
        Ok(canon)
    }

    /// The folder of kept tool output, when it is shown as `@out`.
    pub fn out_root(&self) -> Option<PathBuf> {
        self.out.read().unwrap_or_else(PoisonError::into_inner).as_ref().map(|(_, p)| p.clone())
    }

    /// Is `path` inside the folder of kept tool output?
    pub fn in_out_root(&self, path: &Path) -> bool {
        self.out.read().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|(_, p)| within(path, p))
    }

    /// The largest file a search (`fs.grep`, `fs.find` with text) opens at `path`: the configured read limit, and for a file of the folder of kept tool output the most one such file holds.
    pub fn search_limit_for(&self, path: &Path) -> u64 {
        let base = self.config.limits.read_max_bytes;
        if self.in_out_root(path) {
            base.max(OUT_FILE_MAX_BYTES)
        } else {
            base
        }
    }

    /// Add `path` as a read-write, trusted root named `project` (or withdraw it with `None`). Everything else the guard
    /// refuses stays refused inside it: Aqua's own files, credential folders, `.git` contents, links that leave the folder.
    /// The folder must exist, and a whole drive is refused: "the project" is never the whole machine.
    pub fn set_project_root(&self, path: Option<&Path>) -> Result<(), String> {
        let entry = match path {
            None => None,
            Some(p) => {
                if !p.is_dir() {
                    return Err(format!("the project folder {} does not exist", p.display()));
                }
                let canon = canonical(p).map_err(|e| format!("the project folder {}: {e}", p.display()))?;
                if canon.parent().is_none() {
                    return Err(format!("{} is a whole drive, not a project folder", canon.display()));
                }
                if self.roots.iter().any(|(r, _)| r.alias == "project") {
                    return Err("a root named 'project' is already configured".into());
                }
                Some((Root { alias: "project".into(), path: canon.to_string_lossy().into(), access: Access::Rw, trusted: true }, canon))
            }
        };
        *self.project.write().unwrap_or_else(PoisonError::into_inner) = entry;
        Ok(())
    }

    /// The project folder while it is added.
    pub fn project_root(&self) -> Option<PathBuf> {
        self.project.read().unwrap_or_else(PoisonError::into_inner).as_ref().map(|(_, p)| p.clone())
    }

    /// Replace the places `plan_grant` keeps apart (the default is Windows, the programs and the person's profile: tests that work inside the temporary folder, which is inside AppData, need to).
    pub fn set_kept(&self, places: Vec<Kept>) {
        *self.kept.write().unwrap_or_else(PoisonError::into_inner) = places;
    }

    /// Check that `input` names a folder that may be allowed for the file tools, and say what allowing it would do. It must be a full path starting with a drive; a whole drive, a network path,
    /// Windows, the program folders, the person's whole user folder, its AppData, Aqua's own folders and credential folders are refused, and so is a folder inside a root that is read-only
    /// when read and write is wanted. A folder that is not there yet is accepted when the folder that would hold it is (only that one level is ever made).
    pub fn plan_grant(&self, input: &str, access: Access) -> Result<GrantPlan, ToolError> {
        let deny = |msg: String| ToolError::new(ErrorCode::PermissionDenied, msg);
        let invalid = |msg: String| ToolError::new(ErrorCode::InvalidArguments, msg);
        let raw = input.trim();
        if raw.is_empty() || raw.contains('\0') {
            return Err(invalid("the folder is empty or contains a NUL character".into()));
        }
        if raw.starts_with("\\\\") || raw.starts_with("//") {
            return Err(deny("network (UNC) paths are not allowed".into()));
        }
        let b = raw.as_bytes();
        if !(b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) {
            return Err(invalid("name the folder by its full path, starting with the drive (for example E:\\work)".into()));
        }
        let trimmed = raw.trim_end_matches(['/', '\\']);
        if trimmed.len() <= 2 {
            return Err(deny(format!("{raw} is a whole drive, not a folder: name a folder on it")));
        }
        for seg in trimmed[2..].split(['/', '\\']) {
            check_segment(seg).map_err(&deny)?;
        }
        if trimmed[2..].contains(':') {
            return Err(deny("alternate data streams and device paths are not allowed".into()));
        }
        let target = canonical_lenient(Path::new(trimmed)).ok_or_else(|| deny("the folder cannot be resolved".into()))?;
        let shown = display(&target);
        if target.parent().is_none() {
            return Err(deny(format!("{shown} is a whole drive, not a folder: name a folder on it")));
        }
        let exists = match std::fs::metadata(&target) {
            Ok(m) if m.is_dir() => true,
            Ok(_) => return Err(invalid(format!("{shown} is a file, not a folder"))),
            Err(_) => false,
        };
        if !exists && !target.parent().is_some_and(|p| p.is_dir()) {
            return Err(invalid(format!("{shown} does not exist, and neither does the folder that would hold it")));
        }
        for k in self.kept.read().unwrap_or_else(PoisonError::into_inner).iter() {
            if (k.inside && within(&target, &k.path)) || (k.holding && within(&k.path, &target)) {
                return Err(deny(format!("{shown} is, holds or is inside {}, which no tool is given. Name a folder of your own", k.what)));
            }
        }
        if self.protected.iter().any(|p| within(&target, p)) {
            return Err(deny("this folder belongs to Aqua itself (tokens, policy, audit log or trash) and cannot be used".into()));
        }
        let lower: Vec<String> = target.components().filter_map(|c| if let Component::Normal(s) = c { Some(s.to_string_lossy().to_ascii_lowercase()) } else { None }).collect();
        if let Some(name) = PROTECTED_DIR_NAMES.iter().find(|n| {
            let parts: Vec<&str> = n.split('/').collect();
            lower.windows(parts.len()).any(|w| w.iter().zip(&parts).all(|(a, b)| a == b))
        }) {
            return Err(deny(format!("'{name}' holds credentials and is never accessible to tools")));
        }
        let roots = self.all_roots();
        if let Some((r, _)) = self.root_of(&roots, &target) {
            if access == Access::Rw && r.access == Access::Ro {
                return Err(deny(format!("{shown} is inside @{}, which is read-only. That is set in data/tools/fs.json", r.alias)));
            }
            return Ok(GrantPlan { path: target, exists, already: Some(r.alias.clone()) });
        }
        if self.grants.read().unwrap_or_else(PoisonError::into_inner).len() >= MAX_GRANTS {
            return Err(deny(format!("{MAX_GRANTS} folders are already allowed this time; Aqua must restart before more can be")));
        }
        Ok(GrantPlan { path: target, exists, already: None })
    }

    /// Allow `input` for the file tools until Aqua restarts: a root that is never trusted, so every change in it still asks. The caller must have been told yes by the person (the tool host
    /// asks before `fs.allow_folder` runs, in every mode); this function only checks the folder (`plan_grant`) and adds it. Returns the alias, the folder, and whether it was added now (it is not
    /// when a root already holds it).
    pub fn grant_folder(&self, input: &str, access: Access) -> Result<(String, PathBuf, bool), ToolError> {
        let plan = self.plan_grant(input, access)?;
        if let Some(alias) = plan.already {
            return Ok((alias, plan.path, false));
        }
        if !plan.exists {
            std::fs::create_dir(&plan.path).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the folder {} could not be made: {e}", display(&plan.path))))?;
        }
        let canon = canonical(&plan.path).map_err(|e| ToolError::new(ErrorCode::BackendError, format!("the folder {}: {e}", display(&plan.path))))?;
        let alias = self.grant_alias(&canon);
        let root = Root { alias: alias.clone(), path: canon.to_string_lossy().into(), access, trusted: false };
        self.grants.write().unwrap_or_else(PoisonError::into_inner).push((root, canon.clone()));
        Ok((alias, canon, true))
    }

    /// A name for an allowed folder: its own name in letters, digits, `_` and `-`, made different from every root there is.
    fn grant_alias(&self, canon: &Path) -> String {
        let base: String = canon
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c.to_ascii_lowercase() } else { '_' })
            .take(24)
            .collect();
        let base = if base.is_empty() { "folder".to_string() } else { base };
        let taken: Vec<String> = self.all_roots().iter().map(|(r, _)| r.alias.clone()).collect();
        let (mut alias, mut n) = (base.clone(), 2);
        while taken.contains(&alias) {
            alias = format!("{base}_{n}");
            n += 1;
        }
        alias
    }

    /// Take back a folder that `grant_folder` allowed, named by its alias (`@work` or `work`) or by its path (any spelling `grant_folder` takes: a closing slash, another case).
    /// Only a grant can go. A configured root, the project folder and a place inside either belong to the settings, and are refused with the reason, so that this can never take away
    /// what `data/tools/fs.json` gives. `Ok(Some((alias, folder)))` is the grant that was removed; `Ok(None)` is that nothing was allowed by that name or at that place (a second call, or
    /// Aqua restarted since, which forgets every grant). A folder inside a grant is not the grant: the whole grant is taken back by its own name or place only.
    pub fn take_back_folder(&self, input: &str) -> Result<Option<(String, PathBuf)>, ToolError> {
        let raw = input.trim();
        if raw.is_empty() || raw.contains('\0') {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "name the folder by its @name or by its full path"));
        }
        let path_like = raw.contains(['/', '\\', ':']);
        let named: Option<&str> = match raw.strip_prefix('@') {
            Some(name) => Some(name),
            None if !path_like => Some(raw),
            None => None,
        };
        let target = if named.is_none() { canonical_lenient(Path::new(raw)) } else { None };
        {
            let mut grants = self.grants.write().unwrap_or_else(PoisonError::into_inner);
            let found = grants.iter().position(|(r, p)| match named {
                Some(name) => r.alias.eq_ignore_ascii_case(name),
                None => target.as_ref().is_some_and(|t| same_place(t, p)),
            });
            if let Some(i) = found {
                let (root, path) = grants.remove(i);
                return Ok(Some((root.alias, path)));
            }
        }
        let mut fixed: Vec<(String, PathBuf)> = self.roots.iter().map(|(r, p)| (r.alias.clone(), p.clone())).collect();
        if let Some((r, p)) = self.project.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
            fixed.push((r.alias.clone(), p.clone()));
        }
        if let Some((r, p)) = self.out.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
            fixed.push((r.alias.clone(), p.clone()));
        }
        let hit = fixed.iter().find(|(alias, p)| match named {
            Some(name) => alias.eq_ignore_ascii_case(name),
            None => target.as_ref().is_some_and(|t| within(t, p)),
        });
        if let Some((alias, _)) = hit {
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("@{alias} is a folder that the settings (or the project) give, not one that was allowed for a time, so it cannot be taken back here")));
        }
        Ok(None)
    }

    /// [`take_back_folder`](Self::take_back_folder) as a yes or no: `true` when a grant was removed.
    pub fn revoke_folder(&self, alias_or_path: &str) -> Result<bool, ToolError> {
        self.take_back_folder(alias_or_path).map(|taken| taken.is_some())
    }

    /// The folders that are allowed for a time right now (the grants), with the alias each has: what `revoke_folder` can take back.
    pub fn granted_folders(&self) -> Vec<(String, PathBuf, Access)> {
        self.grants.read().unwrap_or_else(PoisonError::into_inner).iter().map(|(r, p)| (r.alias.clone(), p.clone(), r.access)).collect()
    }

    /// Aqua's own files and folders (canonical): nothing a program started by a tool may be given access to (card R6-35: the container's policy names them as denied).
    pub fn protected(&self) -> &[PathBuf] {
        &self.protected
    }

    /// Is this file name one the guard calls a secret (`sensitive_names`)? A search does not open such a file (it says how many it left out).
    pub fn is_sensitive_name(&self, file_name: &str) -> bool {
        self.config.sensitive_names.iter().any(|p| wildcard(p, file_name))
    }

    /// The root that holds `path` (canonical), or `None` when no root does.
    pub fn root_path_of(&self, path: &Path) -> Option<PathBuf> {
        self.root_of(&self.all_roots(), path).map(|(_, p)| p)
    }

    /// The gate of the folder `dir` for a walk (cards SE-04, CU-02): what a walk may not enter or read below it. `None` when `dir` is outside every root (nothing is walked there).
    /// `dir` must be canonical, as `resolve` gives it. The rules are the ones `resolve` applies to a single path (the protected files and folders, the credential folders, `.git/config`),
    /// made for walking: the gate of a folder is made once, and each entry in it is a comparison of one name.
    pub fn walk_gate(&self, dir: &Path) -> Option<DirGate<'_>> {
        let root = self.root_path_of(dir)?;
        let n = root.components().count();
        let rel: Vec<String> = dir.components().skip(n).filter_map(|c| if let Component::Normal(s) = c { Some(s.to_string_lossy().to_string()) } else { None }).collect();
        let norm = norm_components(dir);
        // A walk that starts inside Aqua's own files or inside a credential folder: everything in it is barred.
        let inside = if self.protected_norm.iter().any(|p| p.len() <= norm.len() && norm[..p.len()] == p[..]) {
            Some(Barred::Protected)
        } else if (1..=rel.len()).any(|i| credential_sequence_ends_at(&rel[..i])) {
            Some(Barred::Credential)
        } else {
            None
        };
        let mut gate = DirGate { protected: &self.protected_norm, norm, rel, kids: Vec::new(), inside };
        gate.kids = gate.kids_of();
        Some(gate)
    }

    /// Is `path` (canonical) one that no walk may enter or read: inside Aqua's own files, in a credential folder, or `.git/config`? The answer a walk gets, for one path.
    pub fn barred(&self, path: &Path) -> Option<Barred> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else { return Some(Barred::Protected) };
        match self.walk_gate(parent) {
            Some(gate) => gate.check(&name.to_string_lossy()),
            None => Some(Barred::Protected),
        }
    }

    pub fn config(&self) -> &FsConfig {
        &self.config
    }

    pub fn limits(&self) -> &Limits {
        &self.config.limits
    }

    pub fn roots(&self) -> Vec<(String, PathBuf, Access)> {
        self.all_roots().iter().map(|(r, p)| (r.alias.clone(), p.clone(), r.access)).collect()
    }

    /// Resolve and check `input` for `op`. The returned path is what the operation must use.
    pub fn resolve(&self, input: &str, op: Op) -> Result<Resolved, ToolError> {
        let deny = |msg: String| ToolError::new(ErrorCode::PermissionDenied, msg);
        // The path is used exactly as given: trimming would hide trailing spaces, which Windows drops.
        let raw = input;
        if raw.trim().is_empty() || raw.contains('\0') {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "path is empty or contains a NUL character"));
        }
        let roots = self.all_roots();

        // `@alias/rest` names a root; a plain relative path is inside the first root.
        let (base, rest): (PathBuf, String) = if let Some(after) = raw.strip_prefix('@') {
            let (alias, rest) = after.split_once(['/', '\\']).unwrap_or((after, ""));
            let Some((_, root_path)) = roots.iter().find(|(r, _)| r.alias == alias) else {
                let known: Vec<&str> = roots.iter().map(|(r, _)| r.alias.as_str()).collect();
                return Err(ToolError::new(ErrorCode::NotFound, format!("no root named '@{alias}'; roots are: {}", known.join(", "))));
            };
            (root_path.clone(), rest.to_string())
        } else if is_absolute_like(raw) {
            (PathBuf::new(), raw.to_string())
        } else {
            (roots[0].1.clone(), raw.to_string())
        };

        if rest.starts_with("\\\\") || rest.starts_with("//") {
            return Err(deny("network (UNC) paths are not allowed".into()));
        }
        for seg in rest.split(['/', '\\']) {
            check_segment(seg).map_err(&deny)?;
        }
        // An alternate data stream (`file.txt:hidden`) or device path: a colon anywhere but the drive letter.
        let after_drive = if rest.len() >= 2 && rest.as_bytes()[1] == b':' && rest.as_bytes()[0].is_ascii_alphabetic() { &rest[2..] } else { &rest[..] };
        if after_drive.contains(':') {
            return Err(deny("alternate data streams and device paths are not allowed".into()));
        }

        let joined = if base.as_os_str().is_empty() { PathBuf::from(&rest) } else { base.join(rest.trim_start_matches(['/', '\\'])) };
        let target = canonical_lenient(&joined).ok_or_else(|| deny("the path cannot be resolved".into()))?;

        // Must be inside a root, after links are resolved.
        let Some((root, root_path)) = self.root_of(&roots, &target) else {
            return Err(deny(format!("'{}' is outside the folders Aqua may use. Use a path inside one of: {}", display(&target), roots.iter().map(|(r, rp)| format!("@{} = {}", r.alias, display(rp))).collect::<Vec<_>>().join("; "))));
        };
        if op == Op::Write && root.access == Access::Ro {
            return Err(deny(format!("@{} is read-only", root.alias)));
        }
        if self.protected.iter().any(|p| within(&target, p) || within(p, &target) && op == Op::Write) {
            return Err(deny("this path belongs to Aqua itself (tokens, policy, audit log or trash) and cannot be used".into()));
        }
        let rel = target.strip_prefix(&root_path).map(|p| p.to_path_buf()).unwrap_or_else(|_| {
            // Case-insensitive match on Windows: strip by component count.
            let n = root_path.components().count();
            target.components().skip(n).collect()
        });
        let comps: Vec<String> = rel.components().filter_map(|c| if let Component::Normal(s) = c { Some(s.to_string_lossy().to_string()) } else { None }).collect();
        let lower: Vec<String> = comps.iter().map(|c| c.to_ascii_lowercase()).collect();
        if let Some(name) = PROTECTED_DIR_NAMES.iter().find(|n| {
            let parts: Vec<&str> = n.split('/').collect();
            lower.windows(parts.len()).any(|w| w.iter().zip(&parts).all(|(a, b)| a == b))
        }) {
            return Err(deny(format!("'{name}' holds credentials and is never accessible to tools")));
        }
        if let Some(i) = lower.iter().position(|c| c == ".git") {
            // Reading git objects is harmless; the config can hold credentials, and writes corrupt the repo.
            let inside = &lower[i + 1..];
            if op == Op::Write || inside.first().map(|s| s.as_str()) == Some("config") {
                return Err(deny("the contents of .git are managed by git, not by file tools".into()));
            }
        }
        if op == Op::Write {
            if let Some(name) = WRITE_PROTECTED_NAMES.iter().find(|n| {
                let parts: Vec<&str> = n.split('/').collect();
                lower.windows(parts.len()).any(|w| w.iter().zip(&parts).all(|(a, b)| a == b))
            }) {
                return Err(deny(format!("'{name}' decides what other programs run or trust, so file tools do not write it (reading it is fine)")));
            }
        }

        let file_name = comps.last().cloned().unwrap_or_default();
        // What was kept in `@out` was masked when it was kept, and its names are made by Aqua from the tool's name: a tool called `secrets.list` must not make its own kept output a "secret file".
        let sensitive = root.alias != OUT_ALIAS && self.config.sensitive_names.iter().any(|p| wildcard(p, &file_name));
        let executable_kind = is_executable_kind(&file_name, &lower);
        Ok(Resolved { path: target, root_alias: root.alias.clone(), trusted: root.trusted, sensitive, executable_kind })
    }
}

/// Why a walk leaves an entry alone. Told apart only to be counted: both are "protected" to the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Barred {
    /// Aqua's own files and folders (`PathGuard::protected`).
    Protected,
    /// A credential store: a folder of `PROTECTED_DIR_NAMES` (`.ssh`, `.aws`, `.docker`, `.config/gcloud`...), or `.git/config`.
    Credential,
}

/// The gate of one folder during a walk (see `PathGuard::walk_gate`).
#[derive(Debug, Clone)]
pub struct DirGate<'g> {
    protected: &'g [Vec<String>],
    /// The folder's own components, folded.
    norm: Vec<String>,
    /// The folder's components below the root that holds it, as they are on disk.
    rel: Vec<String>,
    /// The folded names of the protected entries directly inside the folder.
    kids: Vec<String>,
    /// The folder is itself barred (a walk that was started inside Aqua's own files or a credential folder): everything in it is.
    inside: Option<Barred>,
}

impl<'g> DirGate<'g> {
    /// The folder's components below its root, as they are on disk.
    pub fn rel(&self) -> &[String] {
        &self.rel
    }

    fn kids_of(&self) -> Vec<String> {
        self.protected.iter().filter(|p| p.len() == self.norm.len() + 1 && p[..self.norm.len()] == self.norm[..]).filter_map(|p| p.last().cloned()).collect()
    }

    /// May the walk look at the entry called `name` in this folder? `None`: it may. `Some(why)`: it may neither read nor enter it, and counts it.
    pub fn check(&self, name: &str) -> Option<Barred> {
        if self.inside.is_some() {
            return self.inside;
        }
        if !self.kids.is_empty() && self.kids.contains(&fold(name)) {
            return Some(Barred::Protected);
        }
        for pattern in PROTECTED_DIR_NAMES {
            // The parts of the pattern, last first: the last is the entry's own name, the ones after it the folders above the entry.
            let mut parts = pattern.rsplit('/');
            if !parts.next().is_some_and(|last| name.eq_ignore_ascii_case(last)) {
                continue;
            }
            let above: Vec<&str> = parts.collect();
            if above.len() <= self.rel.len() && above.iter().zip(self.rel.iter().rev()).all(|(a, b)| a.eq_ignore_ascii_case(b)) {
                return Some(Barred::Credential);
            }
        }
        if name.eq_ignore_ascii_case("config") && self.rel.last().is_some_and(|l| l.eq_ignore_ascii_case(".git")) {
            return Some(Barred::Credential);
        }
        None
    }

    /// The gate of the folder `name` inside this one (the walk enters it after `check` said it may).
    pub fn enter(&self, name: &str) -> DirGate<'g> {
        let mut norm = self.norm.clone();
        norm.push(fold(name));
        let mut rel = self.rel.clone();
        rel.push(name.to_string());
        let mut gate = DirGate { protected: self.protected, norm, rel, kids: Vec::new(), inside: self.check(name) };
        gate.kids = gate.kids_of();
        gate
    }
}

/// Do the last components of `rel` make up a credential folder of `PROTECTED_DIR_NAMES` (`.config/gcloud` is two of them)?
fn credential_sequence_ends_at(rel: &[String]) -> bool {
    PROTECTED_DIR_NAMES.iter().any(|pattern| {
        let parts: Vec<&str> = pattern.split('/').collect();
        rel.len() >= parts.len() && rel[rel.len() - parts.len()..].iter().zip(&parts).all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

fn check_segment(seg: &str) -> Result<(), String> {
    if seg.is_empty() || seg == "." {
        return Ok(());
    }
    if seg == ".." {
        return Err("'..' is not allowed in a path; use the full path".into());
    }
    if cfg!(windows) {
        if seg.ends_with('.') || seg.ends_with(' ') {
            return Err(format!("'{seg}' ends with a dot or space, which Windows silently changes"));
        }
        let stem = seg.split('.').next().unwrap_or("").to_ascii_uppercase();
        const DEVICES: &[&str] = &["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"];
        let is_com_lpt = (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit();
        if DEVICES.contains(&stem.as_str()) || is_com_lpt {
            return Err(format!("'{seg}' is a reserved device name"));
        }
        if seg.contains(['<', '>', '"', '|', '?', '*']) {
            return Err(format!("'{seg}' contains characters Windows does not allow in names"));
        }
    }
    Ok(())
}

fn is_absolute_like(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/') || p.starts_with('\\') || (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':')
}

/// Expand `%NAME%` and a leading `~`, so the config can name folders portably.
pub fn expand_env(p: &str) -> String {
    let mut out = String::new();
    let mut rest = p;
    if let Some(r) = p.strip_prefix('~') {
        if let Ok(h) = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")) {
            out.push_str(&h);
            rest = r;
        }
    }
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let name: String = chars.by_ref().take_while(|x| *x != '%').collect();
            match std::env::var(&name) {
                Ok(v) => out.push_str(&v),
                Err(_) => out.push_str(&format!("%{name}%")),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Canonical form of an existing path, without the `\\?\` prefix Windows adds.
pub fn canonical(p: &Path) -> std::io::Result<PathBuf> {
    Ok(strip_verbatim(std::fs::canonicalize(p)?))
}

/// Canonical form of a path that may not exist yet: the longest existing ancestor is resolved
/// (following links) and the rest is appended, provided it has no `..`.
pub fn canonical_lenient(p: &Path) -> Option<PathBuf> {
    let mut existing = p.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() || std::fs::symlink_metadata(&existing).is_ok() {
            let mut base = canonical(&existing).ok()?;
            for t in tail.iter().rev() {
                if t == ".." {
                    return None;
                }
                if t != "." {
                    base.push(t);
                }
            }
            return Some(base);
        }
        let name = existing.file_name()?.to_os_string();
        tail.push(name);
        if !existing.pop() {
            return None;
        }
    }
}

/// Where the system says an open file is (links followed, long names, no `\\?\` prefix). `None` where this cannot be asked.
#[cfg(windows)]
pub fn final_path_of(f: &std::fs::File) -> Option<std::io::Result<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS};
    let handle = f.as_raw_handle();
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: the handle belongs to `f`, which outlives the call, and `buf` is as long as the length passed.
        let n = unsafe { GetFinalPathNameByHandleW(handle as _, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED | VOLUME_NAME_DOS) } as usize;
        if n == 0 {
            return Some(Err(std::io::Error::last_os_error()));
        }
        if n < buf.len() {
            return Some(Ok(strip_verbatim(PathBuf::from(std::ffi::OsString::from_wide(&buf[..n])))));
        }
        buf.resize(n + 1, 0);
    }
}

#[cfg(not(windows))]
pub fn final_path_of(f: &std::fs::File) -> Option<std::io::Result<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        return Some(std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd())));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = f;
        None
    }
}

/// The same place: every component equal, ignoring case on Windows.
pub fn same_place(a: &Path, b: &Path) -> bool {
    within(a, b) && within(b, a)
}

/// The path was checked a moment ago; a link put in its place since (a folder swapped for a junction, a file for a link) would be followed by the open that
/// comes after. So the check is made again on what was opened: the open file must be at the place that was checked. Nothing is read or written when it is not.
pub fn check_opened(f: &std::fs::File, r: &Resolved) -> Result<(), ToolError> {
    check_opened_at(f, &r.path)
}

/// `check_opened` for a path a walk met (a canonical folder's entry), which has no `Resolved`: the open file must be at `path`.
pub fn check_opened_at(f: &std::fs::File, path: &Path) -> Result<(), ToolError> {
    match final_path_of(f) {
        None => Ok(()),
        Some(Ok(actual)) if same_place(&actual, path) => Ok(()),
        Some(Ok(actual)) => Err(ToolError::new(
            ErrorCode::PermissionDenied,
            format!("{} turned out to be {} when it was opened: a link was put in its place after the path was checked, so nothing was read or written", display(path), display(&actual)),
        )),
        Some(Err(e)) => Err(ToolError::new(ErrorCode::PermissionDenied, format!("which file was opened could not be confirmed ({e}), so nothing was read or written"))),
    }
}

fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(rest.to_string());
    }
    p
}

/// True when `child` is `root` or inside it, comparing whole components (and ignoring case on Windows).
pub fn within(child: &Path, root: &Path) -> bool {
    let (c, r) = (norm_components(child), norm_components(root));
    c.len() >= r.len() && c[..r.len()] == r[..]
}

/// The components of `p` that name something (the drive and the folder names), folded for comparing: what `within` compares.
fn norm_components(p: &Path) -> Vec<String> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(fold(&s.to_string_lossy())),
            Component::Prefix(pre) => Some(fold(&pre.as_os_str().to_string_lossy())),
            _ => None,
        })
        .collect()
}

fn fold(s: &str) -> String {
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

pub fn display(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Case-insensitive `*` / `?` matcher.
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

const EXEC_EXTENSIONS: &[&str] = &[
    "exe", "dll", "sys", "scr", "com", "cpl", "msi", "bat", "cmd", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "hta", "lnk", "url", "reg", "inf", "jar",
    "sh", "py", "pyw", "rb", "pl", "php", "docm", "xlsm", "pptm", "chm", "rdp", "pth", "node", "xll",
];
const BUILD_NAMES: &[&str] = &[
    "makefile", "package.json", "setup.py", "pyproject.toml", "conftest.py", "sitecustomize.py", ".envrc", "dockerfile", "docker-compose.yml", "build.rs", "cargo.toml",
    ".pre-commit-config.yaml", ".gitlab-ci.yml", "azure-pipelines.yml", "jenkinsfile", "desktop.ini", "autorun.inf",
];

fn is_executable_kind(file_name: &str, lower_components: &[String]) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    EXEC_EXTENSIONS.contains(&ext)
        || BUILD_NAMES.contains(&lower.as_str())
        || lower_components.windows(2).any(|w| (w[0] == ".github" && w[1] == "workflows") || w[0] == ".husky")
        || matches!(lower.as_str(), "agents.md" | "claude.md" | "gemini.md" | ".cursorrules" | ".windsurfrules" | "copilot-instructions.md")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rig {
        dir: PathBuf,
        guard: PathGuard,
    }

    fn rig(name: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("aqua_fsguard_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws").join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("ro")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        std::fs::create_dir_all(dir.join("aqua").join("tools")).unwrap();
        let cfg = FsConfig {
            roots: vec![
                Root { alias: "ws".into(), path: dir.join("ws").to_string_lossy().into(), access: Access::Rw, trusted: true },
                Root { alias: "ro".into(), path: dir.join("ro").to_string_lossy().into(), access: Access::Ro, trusted: false },
            ],
            sensitive_names: default_sensitive(),
            limits: default_limits(),
            spill_dir: None,
        };
        let guard = PathGuard::new(cfg, vec![dir.join("aqua")]).unwrap();
        Rig { dir, guard }
    }

    impl Rig {
        fn ws(&self, rel: &str) -> String {
            self.dir.join("ws").join(rel).to_string_lossy().into()
        }
    }

    fn code(r: Result<Resolved, ToolError>) -> ErrorCode {
        r.unwrap_err().code
    }

    #[test]
    fn files_that_decide_what_other_tools_run_or_trust_can_be_read_but_never_written() {
        let r = rig("write_protected");
        for name in [
            ".vscode/settings.json", "proj/.vscode/tasks.json", ".idea/workspace.xml", ".cargo/config.toml", ".devcontainer/devcontainer.json", ".husky/pre-commit",
            ".claude/settings.json", ".codex/config.toml", ".cursor/mcp.json", ".mcp.json", "proj/.mcp.json", ".npmrc", "proj/.gitconfig",
            "Documents/WindowsPowerShell/Profile.ps1", "Microsoft.PowerShell_profile.ps1", "AppData/Roaming/Microsoft/Windows/Start Menu/Programs/Startup/x.bat",
        ] {
            assert_eq!(code(r.guard.resolve(&format!("@ws/{name}"), Op::Write)), ErrorCode::PermissionDenied, "{name} must not be written");
            r.guard.resolve(&format!("@ws/{name}"), Op::Read).unwrap_or_else(|e| panic!("{name} may be read: {e}"));
        }
        for name in ["notes.md", "src/main.rs", "proj/vscode/x.json", "cargo/config.toml", "my.mcp.json.txt", "npmrc"] {
            r.guard.resolve(&format!("@ws/{name}"), Op::Write).unwrap_or_else(|e| panic!("{name} is an ordinary file: {e}"));
        }
    }

    #[test]
    fn paths_inside_a_root_resolve_by_alias_relative_or_absolute() {
        let r = rig("inside");
        for input in ["@ws/sub/a.txt".to_string(), "sub/a.txt".to_string(), r.ws("sub/a.txt"), "@ws".to_string(), "@ws/".to_string()] {
            let ok = r.guard.resolve(&input, Op::Write).unwrap_or_else(|e| panic!("{input}: {e}"));
            assert_eq!(ok.root_alias, "ws");
            assert!(ok.trusted);
        }
    }

    #[test]
    fn dot_dot_unc_streams_devices_and_outside_paths_are_refused() {
        let r = rig("refuse");
        assert_eq!(code(r.guard.resolve("../outside/x.txt", Op::Read)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve("sub/../../outside/x", Op::Read)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve(&r.dir.join("outside").join("x.txt").to_string_lossy(), Op::Read)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve(r"\\server\share\x.txt", Op::Read)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve("//server/share/x.txt", Op::Read)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve("a.txt:hidden", Op::Read)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve("", Op::Read)), ErrorCode::InvalidArguments);
        assert_eq!(code(r.guard.resolve("a\0b", Op::Read)), ErrorCode::InvalidArguments);
        assert_eq!(code(r.guard.resolve("@nope/x", Op::Read)), ErrorCode::NotFound);
        if cfg!(windows) {
            for bad in ["con", "NUL.txt", "sub/COM1", "name.", "name ", "a?b"] {
                assert_eq!(code(r.guard.resolve(bad, Op::Read)), ErrorCode::PermissionDenied, "{bad}");
            }
        }
    }

    /// A folder junction (`mklink /J`): it needs no privilege, unlike a symbolic link.
    #[cfg(windows)]
    fn junction(link: &Path, target: &Path) {
        let out = std::process::Command::new("cmd").args(["/C", "mklink", "/J"]).arg(link).arg(target).output().expect("cmd runs");
        assert!(out.status.success(), "mklink /J failed: {}", String::from_utf8_lossy(&out.stdout));
    }

    /// The 8.3 name of an existing path, or `None` when the volume makes none (then the test says so and checks nothing).
    #[cfg(windows)]
    fn short_name(p: &Path) -> Option<PathBuf> {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: `wide` ends in NUL and `buf` is as long as the length passed.
        let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if n == 0 || n >= buf.len() {
            return None;
        }
        let short = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
        (short != p).then_some(short)
    }

    #[cfg(windows)]
    #[test]
    fn the_8_3_name_of_a_protected_folder_or_of_a_file_is_that_folder_or_file() {
        let r = rig("short_names");
        let ssh = r.dir.join("ws").join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("id_rsa_long_file_name.txt"), "secret").unwrap();
        let Some(short_dir) = short_name(&ssh) else {
            eprintln!("skipped: this volume makes no 8.3 names");
            return;
        };
        // The short name of the protected folder (`SSH~1`) is the protected folder, whatever the spelling around it.
        let short_file = short_dir.join("id_rsa_long_file_name.txt");
        for input in [short_dir.to_string_lossy().to_string(), short_file.to_string_lossy().to_string(), short_file.to_string_lossy().to_uppercase()] {
            let said = r.guard.resolve(&input, Op::Read).unwrap_err();
            assert_eq!(said.code, ErrorCode::PermissionDenied, "{input}");
            assert!(said.message.contains(".ssh"), "{input}: {}", said.message);
        }
        // And the short name of a harmless file is that file, found where it is, not somewhere else.
        let long = r.dir.join("ws").join("sub").join("a_rather_long_file_name.txt");
        std::fs::write(&long, "x").unwrap();
        let short = short_name(&long).expect("this volume makes 8.3 names");
        let ok = r.guard.resolve(&short.to_string_lossy(), Op::Read).unwrap();
        assert!(same_place(&ok.path, &long), "{} is not {}", ok.path.display(), long.display());
        // The short name of a folder that holds a file which does not exist yet is judged by where the folder is.
        let short_sub = short_name(&r.dir.join("ws").join("sub")).map(|s| s.join("new.txt"));
        if let Some(p) = short_sub {
            assert!(r.guard.resolve(&p.to_string_lossy(), Op::Write).is_ok());
        }
    }

    #[test]
    fn every_other_way_to_spell_a_place_outside_the_roots_or_on_the_network_is_refused() {
        let r = rig("spellings");
        let outside = r.dir.join("outside").join("x.txt").to_string_lossy().to_string();
        let mut spellings: Vec<String> = vec![
            outside.clone(),
            outside.replace('\\', "/"),
            outside.to_uppercase(),
            format!(r"\\?\{outside}"),
            format!(r"\\.\{outside}"),
            format!("//?/{}", outside.replace('\\', "/")),
            format!("//./{}", outside.replace('\\', "/")),
            r"\\localhost\C$\Windows\win.ini".into(),
            r"\\127.0.0.1\C$\Windows\win.ini".into(),
            r"\\?\UNC\server\share\x.txt".into(),
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\x".into(),
            r"\??\C:\Windows\win.ini".into(),
            r"\\server\share".into(),
            "//server/share".into(),
            r"\\".into(),
            "//".into(),
        ];
        // A path that leaves by `..` however it is written, and one that is spelled with dots and spaces Windows throws away.
        spellings.extend(["..", "../outside/x.txt", r"..\outside\x.txt", "sub/./../../outside/x.txt", "sub\\..\\..\\outside\\x.txt"].map(String::from));
        for s in &spellings {
            let said = r.guard.resolve(s, Op::Read);
            assert!(said.is_err(), "{s} must be refused, it resolved to {:?}", said.map(|x| x.path));
        }
        if cfg!(windows) {
            for s in ["sub/name.txt.", "sub/name.txt ", "sub/a.txt::$DATA", "sub/a.txt:stream", "sub/CON", "sub/nul.txt", "sub/COM1.log", "sub/aux", "sub/LPT9", "sub/CONIN$", "sub/a*b", "sub/a|b", "sub/a<b"] {
                assert!(r.guard.resolve(s, Op::Read).is_err(), "{s} must be refused");
            }
        }
        // The same places named in the way the guard does allow are still found.
        assert!(r.guard.resolve("sub/x.txt", Op::Write).is_ok() && r.guard.resolve("@ws/sub/x.txt", Op::Write).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_to_the_outside_is_followed_to_where_it_goes_and_refused_there() {
        let r = rig("junction");
        std::fs::write(r.dir.join("outside").join("secret.txt"), "outside").unwrap();
        junction(&r.dir.join("ws").join("door"), &r.dir.join("outside"));
        for input in ["door/secret.txt", "@ws/door/secret.txt", "door", "door/new.txt", "DOOR\\secret.txt"] {
            assert_eq!(code(r.guard.resolve(input, Op::Read)), ErrorCode::PermissionDenied, "{input}");
            assert_eq!(code(r.guard.resolve(input, Op::Write)), ErrorCode::PermissionDenied, "{input}");
        }
        // A junction that stays inside is only another name for the same folder.
        junction(&r.dir.join("ws").join("alias"), &r.dir.join("ws").join("sub"));
        std::fs::write(r.dir.join("ws").join("sub").join("in.txt"), "in").unwrap();
        let ok = r.guard.resolve("alias/in.txt", Op::Read).unwrap();
        assert!(same_place(&ok.path, &r.dir.join("ws").join("sub").join("in.txt")));
    }

    #[cfg(windows)]
    #[test]
    fn a_link_put_in_place_after_the_check_is_found_when_the_file_is_opened() {
        let r = rig("swapped");
        std::fs::create_dir_all(r.dir.join("ws").join("docs")).unwrap();
        std::fs::write(r.dir.join("ws").join("docs").join("note.txt"), "inside").unwrap();
        std::fs::write(r.dir.join("outside").join("note.txt"), "OUTSIDE").unwrap();
        // The check passes: the folder is a plain folder inside the root.
        let checked = r.guard.resolve("docs/note.txt", Op::Read).unwrap();
        // The open that follows is of the same path, so it is the same file.
        let f = std::fs::File::open(&checked.path).unwrap();
        check_opened(&f, &checked).expect("nothing was swapped yet");
        drop(f);
        // Between the check and the open, the folder is swapped for a junction to the outside.
        std::fs::rename(r.dir.join("ws").join("docs"), r.dir.join("ws").join("docs_old")).unwrap();
        junction(&r.dir.join("ws").join("docs"), &r.dir.join("outside"));
        let f = std::fs::File::open(&checked.path).expect("the path still opens: it now leads outside");
        let mut text = String::new();
        std::io::Read::read_to_string(&mut &f, &mut text).unwrap();
        assert_eq!(text, "OUTSIDE", "this is what an open without the second check would have returned");
        let said = check_opened(&f, &checked).unwrap_err();
        assert_eq!(said.code, ErrorCode::PermissionDenied);
        assert!(said.message.contains("a link was put in its place") && said.message.contains("nothing was read or written"), "{}", said.message);
    }

    #[test]
    fn a_project_root_can_be_added_and_withdrawn_while_running() {
        let r = rig("project");
        let proj = r.dir.join("proj");
        std::fs::create_dir_all(proj.join("src")).unwrap();
        let file = proj.join("src").join("a.rs").to_string_lossy().to_string();
        assert_eq!(code(r.guard.resolve(&file, Op::Write)), ErrorCode::PermissionDenied, "outside every root until it is added");
        r.guard.set_project_root(Some(&proj)).unwrap();
        let ok = r.guard.resolve(&file, Op::Write).unwrap();
        assert_eq!(ok.root_alias, "project");
        assert!(ok.trusted, "writes in the project run without a prompt");
        assert!(r.guard.resolve("@project/src/a.rs", Op::Write).is_ok());
        assert!(r.guard.roots().iter().any(|(a, _, _)| a == "project"));
        // The first configured root is still where a plain relative path goes.
        assert_eq!(r.guard.resolve("sub/x.txt", Op::Write).unwrap().root_alias, "ws");
        r.guard.set_project_root(None).unwrap();
        assert_eq!(code(r.guard.resolve(&file, Op::Write)), ErrorCode::PermissionDenied, "withdrawn: outside again");
        assert_eq!(code(r.guard.resolve("@project/src/a.rs", Op::Read)), ErrorCode::NotFound);
        assert!(r.guard.project_root().is_none());
    }

    #[test]
    fn the_project_root_keeps_every_protection() {
        let r = rig("projprot");
        // The project is the folder that also holds Aqua's own files and a credential folder.
        std::fs::create_dir_all(r.dir.join(".ssh")).unwrap();
        std::fs::create_dir_all(r.dir.join(".git")).unwrap();
        r.guard.set_project_root(Some(&r.dir)).unwrap();
        assert!(r.guard.resolve("@project/ws/sub/a.txt", Op::Write).is_ok());
        assert_eq!(code(r.guard.resolve("@project/aqua/tools/policy.json", Op::Read)), ErrorCode::PermissionDenied, "Aqua's own files");
        assert_eq!(code(r.guard.resolve("@project/aqua", Op::Write)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve("@project/.ssh/id_rsa", Op::Read)), ErrorCode::PermissionDenied, "credential folders");
        assert_eq!(code(r.guard.resolve("@project/.git/config", Op::Read)), ErrorCode::PermissionDenied, "git config");
        assert_eq!(code(r.guard.resolve("@project/../outside.txt", Op::Read)), ErrorCode::PermissionDenied, "no way out through ..");
    }

    #[test]
    fn a_missing_folder_or_a_whole_drive_is_not_a_project() {
        let r = rig("projbad");
        assert!(r.guard.set_project_root(Some(&r.dir.join("does-not-exist"))).unwrap_err().contains("does not exist"));
        let drive_root = r.dir.ancestors().last().unwrap().to_path_buf();
        assert!(r.guard.set_project_root(Some(&drive_root)).unwrap_err().contains("whole drive"));
        assert!(r.guard.project_root().is_none(), "a refused folder leaves nothing added");
    }

    #[test]
    fn read_only_roots_refuse_writes() {
        let r = rig("ro");
        assert!(r.guard.resolve("@ro/a.txt", Op::Read).is_ok());
        assert_eq!(code(r.guard.resolve("@ro/a.txt", Op::Write)), ErrorCode::PermissionDenied);
    }

    #[test]
    fn aqua_protected_paths_and_credential_folders_are_never_reachable() {
        let r = rig("protected");
        // A root that contains Aqua's own folder still cannot reach it.
        let cfg = FsConfig::with_workspace(r.dir.to_string_lossy().to_string());
        let g = PathGuard::new(cfg, vec![r.dir.join("aqua")]).unwrap();
        for op in [Op::Read, Op::Write] {
            assert_eq!(code(g.resolve(&r.dir.join("aqua").join("tools").join("host.audit.jsonl").to_string_lossy(), op)), ErrorCode::PermissionDenied);
        }
        for p in ["ws/.ssh/id_rsa", "ws/.aws/credentials", "ws/x/.gnupg/pubring.kbx", "ws/.kube/config"] {
            std::fs::create_dir_all(r.dir.join(p).parent().unwrap()).unwrap();
            assert_eq!(code(g.resolve(&r.dir.join(p).to_string_lossy(), Op::Read)), ErrorCode::PermissionDenied, "{p}");
        }
        // .git: objects readable, config and every write refused.
        std::fs::create_dir_all(r.dir.join("ws/.git/objects")).unwrap();
        assert!(r.guard.resolve("@ws/.git/HEAD", Op::Read).is_ok());
        assert_eq!(code(r.guard.resolve("@ws/.git/config", Op::Read)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve("@ws/.git/HEAD", Op::Write)), ErrorCode::PermissionDenied);
    }

    #[test]
    fn a_link_that_leaves_the_root_is_refused() {
        let r = rig("link");
        let link = r.dir.join("ws").join("escape");
        let made = if cfg!(windows) {
            // A junction needs no privilege; a directory symlink often does.
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J", &link.to_string_lossy(), &r.dir.join("outside").to_string_lossy()])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        } else {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(r.dir.join("outside"), &link).is_ok()
            }
            #[cfg(not(unix))]
            {
                false
            }
        };
        if !made {
            eprintln!("skipped: could not create a link on this machine");
            return;
        }
        std::fs::write(r.dir.join("outside").join("secret.txt"), "x").unwrap();
        assert_eq!(code(r.guard.resolve("@ws/escape/secret.txt", Op::Read)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve("@ws/escape", Op::Read)), ErrorCode::PermissionDenied);
    }

    #[test]
    fn sensitive_names_and_executable_kinds_are_flagged() {
        let r = rig("flags");
        for (name, sens, exec) in [("app.env", true, false), ("id_rsa", true, false), ("notes.txt", false, false), ("run.ps1", false, true), ("Makefile", false, true), ("x.PY", false, true), ("CLAUDE.md", false, true)] {
            let res = r.guard.resolve(&format!("@ws/{name}"), Op::Write).unwrap();
            assert_eq!((res.sensitive, res.executable_kind), (sens, exec), "{name}");
        }
    }

    #[test]
    fn new_files_in_new_folders_resolve_and_case_is_ignored_on_windows() {
        let r = rig("new");
        assert!(r.guard.resolve("@ws/not/yet/created.txt", Op::Write).is_ok());
        if cfg!(windows) {
            let upper = r.dir.join("WS").join("sub").to_string_lossy().to_string();
            assert!(r.guard.resolve(&upper, Op::Read).is_ok());
        }
    }

    #[test]
    fn wildcards_match_like_a_shell() {
        assert!(wildcard("*.env", "prod.env") && wildcard("*.ENV", "prod.env") && wildcard(".env.*", ".env.local"));
        assert!(wildcard("id_rsa*", "id_rsa.pub") && wildcard("a?c", "abc") && wildcard("*secret*", "my-secret-key"));
        assert!(!wildcard("*.env", "env.txt") && !wildcard("a?c", "ac") && !wildcard("*.rs", "main.rsx"));
    }

    #[test]
    fn a_bad_config_is_rejected() {
        let bad_alias = FsConfig { roots: vec![Root { alias: "a b".into(), path: ".".into(), access: Access::Rw, trusted: false }], sensitive_names: vec![], limits: default_limits(), spill_dir: None };
        assert!(PathGuard::new(bad_alias, vec![]).is_err());
        let unc = FsConfig { roots: vec![Root { alias: "n".into(), path: r"\\server\share".into(), access: Access::Rw, trusted: false }], sensitive_names: vec![], limits: default_limits(), spill_dir: None };
        assert!(PathGuard::new(unc, vec![]).is_err());
        let none = FsConfig { roots: vec![], sensitive_names: vec![], limits: default_limits(), spill_dir: None };
        assert!(PathGuard::new(none, vec![]).is_err());
    }

    fn win(p: &Path) -> String {
        p.to_string_lossy().to_string()
    }

    #[test]
    fn an_allowed_folder_is_a_root_that_is_never_trusted_and_everything_else_outside_stays_refused() {
        let r = rig("grant_basic");
        r.guard.set_kept(vec![]);
        let target = r.dir.join("outside");
        let plan = r.guard.plan_grant(&win(&target), Access::Rw).unwrap();
        assert!(plan.exists && plan.already.is_none(), "{plan:?}");
        let (alias, path, added) = r.guard.grant_folder(&win(&target), Access::Rw).unwrap();
        assert!(added && alias == "outside" && within(&path, &target), "{alias} {path:?}");
        let file = win(&target.join("hello.py"));
        let resolved = r.guard.resolve(&file, Op::Write).unwrap();
        assert!(!resolved.trusted && resolved.root_alias == "outside", "a granted folder is never trusted, so every change in it asks: {resolved:?}");
        assert!(r.guard.resolve("@outside/hello.py", Op::Write).is_ok());
        assert_eq!(code(r.guard.resolve(&win(&r.dir.join("elsewhere").join("x.txt")), Op::Write)), ErrorCode::PermissionDenied, "a folder that was not allowed stays outside");
        assert!(r.guard.roots().iter().any(|(a, _, acc)| a == "outside" && *acc == Access::Rw));
        // The same folder again, in the way a person writes it (a closing backslash), adds nothing.
        let (alias2, _, added2) = r.guard.grant_folder(&format!("{}\\", win(&target)), Access::Rw).unwrap();
        assert!(!added2 && alias2 == "outside");
        // A read-only grant lets nothing be written.
        let ro_dir = r.dir.join("only_read");
        std::fs::create_dir_all(&ro_dir).unwrap();
        r.guard.grant_folder(&win(&ro_dir), Access::Ro).unwrap();
        assert_eq!(code(r.guard.resolve(&win(&ro_dir.join("a.txt")), Op::Write)), ErrorCode::PermissionDenied);
        assert!(r.guard.resolve(&win(&ro_dir.join("a.txt")), Op::Read).is_ok());
    }

    #[test]
    fn a_folder_that_is_not_there_yet_is_made_one_level_deep_and_two_folders_of_one_name_get_two_aliases() {
        let r = rig("grant_new");
        r.guard.set_kept(vec![]);
        let fresh = r.dir.join("fresh_project");
        let plan = r.guard.plan_grant(&win(&fresh), Access::Rw).unwrap();
        assert!(!plan.exists);
        assert!(!fresh.exists(), "planning makes nothing");
        r.guard.grant_folder(&win(&fresh), Access::Rw).unwrap();
        assert!(fresh.is_dir());
        let deep = r.dir.join("no_such").join("deeper");
        assert_eq!(r.guard.plan_grant(&win(&deep), Access::Rw).unwrap_err().code, ErrorCode::InvalidArguments, "only the folder itself is ever made");
        assert!(!r.dir.join("no_such").exists());
        let (a, b) = (r.dir.join("one").join("data"), r.dir.join("two").join("data"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_eq!(r.guard.grant_folder(&win(&a), Access::Rw).unwrap().0, "data");
        assert_eq!(r.guard.grant_folder(&win(&b), Access::Rw).unwrap().0, "data_2");
    }

    #[test]
    fn a_folder_that_is_not_a_plain_folder_of_the_persons_own_is_never_allowed() {
        let r = rig("grant_refused");
        r.guard.set_kept(vec![]);
        let root_of_drive = r.dir.ancestors().last().map(|p| win(p)).unwrap_or_default();
        std::fs::write(r.dir.join("a_file.txt"), "x").unwrap();
        std::fs::create_dir_all(r.dir.join(".ssh")).unwrap();
        for (input, want, why) in [
            ("work", ErrorCode::InvalidArguments, "not a full path"),
            ("\\work", ErrorCode::InvalidArguments, "no drive"),
            ("\\\\server\\share\\x", ErrorCode::PermissionDenied, "a network path"),
            ("", ErrorCode::InvalidArguments, "empty"),
            ("E:", ErrorCode::InvalidArguments, "a drive with no slash"),
            (root_of_drive.as_str(), ErrorCode::PermissionDenied, "a whole drive"),
        ] {
            assert_eq!(r.guard.plan_grant(input, Access::Rw).unwrap_err().code, want, "{why}: {input}");
        }
        assert_eq!(r.guard.plan_grant(&win(&r.dir.join("a_file.txt")), Access::Rw).unwrap_err().code, ErrorCode::InvalidArguments, "a file is not a folder");
        assert_eq!(r.guard.plan_grant(&win(&r.dir.join("aqua")), Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied, "Aqua's own folder");
        assert_eq!(r.guard.plan_grant(&win(&r.dir.join("aqua").join("tools")), Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied);
        assert_eq!(r.guard.plan_grant(&win(&r.dir.join(".ssh")), Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied, "a credential folder");
        assert_eq!(r.guard.plan_grant(&format!("{}\\CON", win(&r.dir)), Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied, "a device name");
        // Inside a read-only root: reading is already allowed, writing is not.
        let inside_ro = win(&r.dir.join("ro").join("sub"));
        std::fs::create_dir_all(r.dir.join("ro").join("sub")).unwrap();
        assert_eq!(r.guard.plan_grant(&inside_ro, Access::Ro).unwrap().already.as_deref(), Some("ro"));
        assert_eq!(r.guard.plan_grant(&inside_ro, Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied);
        // A folder a root holds already needs nothing.
        assert_eq!(r.guard.plan_grant(&win(&r.dir.join("ws").join("sub")), Access::Rw).unwrap().already.as_deref(), Some("ws"));
    }

    #[test]
    fn windows_the_programs_and_the_persons_whole_profile_are_kept_apart_by_default() {
        let r = rig("grant_kept");
        // (A guard made the ordinary way, with the default places.)
        let mut tried = 0;
        for var in ["SystemRoot", "ProgramFiles", "ProgramData"] {
            if let Some(p) = std::env::var_os(var).filter(|v| !v.is_empty()) {
                let p = p.to_string_lossy().to_string();
                assert_eq!(r.guard.plan_grant(&p, Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied, "{var} {p}");
                assert_eq!(r.guard.plan_grant(&format!("{p}\\System32"), Access::Rw).map(|_| ()).unwrap_err().code, ErrorCode::PermissionDenied, "inside {var}");
                tried += 1;
            }
        }
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            let profile = profile.to_string_lossy().to_string();
            let refused = |p: &str| r.guard.plan_grant(p, Access::Rw).map(|_| ()).unwrap_err().code;
            assert_eq!(refused(&profile), ErrorCode::PermissionDenied, "the whole user folder");
            assert_eq!(refused(&format!("{profile}\\AppData")), ErrorCode::PermissionDenied, "AppData");
            assert_eq!(refused(&format!("{profile}\\AppData\\Roaming")), ErrorCode::PermissionDenied, "inside AppData");
            if let Some(parent) = Path::new(&profile).parent() {
                assert_eq!(refused(&win(parent)), ErrorCode::PermissionDenied, "the folder that holds the user folder");
            }
            tried += 1;
        }
        // A place can be replaced: the temporary folder of a test run is inside AppData.
        r.guard.set_kept(vec![Kept { what: "a place for the test".into(), path: r.dir.join("outside"), inside: true, holding: true }]);
        assert_eq!(r.guard.plan_grant(&win(&r.dir.join("outside")), Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied);
        assert_eq!(r.guard.plan_grant(&win(&r.dir), Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied, "a folder that holds the place");
        let _ = tried;
    }

    #[test]
    fn only_sixteen_folders_can_be_allowed_at_a_time() {
        let r = rig("grant_many");
        r.guard.set_kept(vec![]);
        for i in 0..MAX_GRANTS {
            let d = r.dir.join(format!("g{i}"));
            std::fs::create_dir_all(&d).unwrap();
            r.guard.grant_folder(&win(&d), Access::Rw).unwrap();
        }
        let more = r.dir.join("one_more");
        std::fs::create_dir_all(&more).unwrap();
        assert_eq!(r.guard.grant_folder(&win(&more), Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied);
        assert_eq!(r.guard.roots().len(), 2 + MAX_GRANTS, "the two configured roots and the sixteen");
    }

    #[test]
    fn a_granted_folder_can_be_taken_back_by_name_or_by_place_and_the_access_goes_with_it() {
        let r = rig("revoke_basic");
        r.guard.set_kept(vec![]);
        let target = r.dir.join("outside");
        let file = win(&target.join("hello.py"));
        r.guard.grant_folder(&win(&target), Access::Rw).unwrap();
        assert!(r.guard.resolve(&file, Op::Write).is_ok());
        assert_eq!(r.guard.granted_folders().len(), 1);
        // By its name: the access is gone and the folder is outside again.
        assert!(r.guard.revoke_folder("@outside").unwrap());
        assert_eq!(code(r.guard.resolve(&file, Op::Write)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve(&file, Op::Read)), ErrorCode::PermissionDenied);
        assert!(r.guard.granted_folders().is_empty() && !r.guard.roots().iter().any(|(a, _, _)| a == "outside"));
        // A second call finds nothing to take back, and that is not an error.
        assert!(!r.guard.revoke_folder("@outside").unwrap());
        // Allowed again, then taken back by its place in another spelling (forward slashes and a closing slash).
        let (alias, _, added) = r.guard.grant_folder(&win(&target), Access::Rw).unwrap();
        assert!(added && alias == "outside");
        let spelled = format!("{}/", win(&target).replace('\\', "/"));
        assert!(r.guard.revoke_folder(&spelled).unwrap());
        assert_eq!(code(r.guard.resolve(&file, Op::Write)), ErrorCode::PermissionDenied);
        // A folder inside a grant is not the grant: the whole grant stays.
        r.guard.grant_folder(&win(&target), Access::Rw).unwrap();
        std::fs::create_dir_all(target.join("sub")).unwrap();
        assert!(!r.guard.revoke_folder(&win(&target.join("sub"))).unwrap());
        assert!(r.guard.resolve(&file, Op::Write).is_ok(), "the whole grant is still there");
        // The name with no @ is the name as well, and only the folder named goes.
        let other = r.dir.join("only_read");
        std::fs::create_dir_all(&other).unwrap();
        r.guard.grant_folder(&win(&other), Access::Ro).unwrap();
        assert!(r.guard.revoke_folder("outside").unwrap());
        assert!(r.guard.resolve(&win(&other.join("a.txt")), Op::Read).is_ok(), "the other grant is untouched");
        assert_eq!(code(r.guard.resolve(&file, Op::Read)), ErrorCode::PermissionDenied);
    }

    #[test]
    fn a_configured_root_and_the_project_folder_cannot_be_taken_back_and_nothing_else_is_an_error() {
        let r = rig("revoke_fixed");
        r.guard.set_kept(vec![]);
        let proj = r.dir.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        r.guard.set_project_root(Some(&proj)).unwrap();
        let names: Vec<String> = vec![
            "@ws".into(),
            "ws".into(),
            "@ro".into(),
            "@project".into(),
            win(&r.dir.join("ws")),
            win(&r.dir.join("ws").join("sub")),
            win(&r.dir.join("ro")),
            win(&proj),
        ];
        for what in &names {
            let said = r.guard.revoke_folder(what).unwrap_err();
            assert_eq!(said.code, ErrorCode::PermissionDenied, "{what}");
            assert!(said.message.contains("settings"), "{what}: {}", said.message);
        }
        assert!(r.guard.resolve(&r.ws("sub/a.txt"), Op::Write).is_ok(), "the configured root is still there");
        assert!(r.guard.resolve(&win(&proj.join("a.txt")), Op::Write).is_ok(), "and so is the project folder");
        assert_eq!(r.guard.resolve("@ro/a.txt", Op::Read).unwrap().root_alias, "ro");
        // A name or a place that was never allowed: nothing removed, no error.
        assert!(!r.guard.revoke_folder("@nothing").unwrap());
        assert!(!r.guard.revoke_folder(&win(&r.dir.join("elsewhere"))).unwrap());
        // Nothing to name is a plain refusal.
        assert_eq!(r.guard.revoke_folder("  ").unwrap_err().code, ErrorCode::InvalidArguments);
        assert_eq!(r.guard.revoke_folder("a\0b").unwrap_err().code, ErrorCode::InvalidArguments);
    }

    #[test]
    fn taking_a_folder_back_frees_its_place_among_the_sixteen() {
        let r = rig("revoke_many");
        r.guard.set_kept(vec![]);
        for i in 0..MAX_GRANTS {
            let d = r.dir.join(format!("g{i}"));
            std::fs::create_dir_all(&d).unwrap();
            r.guard.grant_folder(&win(&d), Access::Rw).unwrap();
        }
        let more = r.dir.join("one_more");
        std::fs::create_dir_all(&more).unwrap();
        assert_eq!(r.guard.grant_folder(&win(&more), Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied);
        assert!(r.guard.revoke_folder("@g3").unwrap());
        assert_eq!(r.guard.grant_folder(&win(&more), Access::Rw).unwrap().0, "one_more");
        assert_eq!(r.guard.granted_folders().len(), MAX_GRANTS);
        assert_eq!(r.guard.roots().len(), 2 + MAX_GRANTS);
    }

    // ---- card OA5-02: the folder of kept tool output, `@out` ----

    fn kept_name(tool: &str) -> String {
        format!("20261009_153012_3_a1b2c3_{tool}.txt")
    }

    #[test]
    fn the_folder_of_kept_output_is_a_read_only_untrusted_root_that_is_never_written() {
        let r = rig("out_root");
        r.guard.set_kept(vec![]);
        let out = r.dir.join("tool-output").join("1a2b3c4d");
        let shown = r.guard.set_out_root(&out).unwrap();
        assert!(out.is_dir(), "the folder is made");
        assert_eq!(r.guard.out_root(), Some(shown.clone()));
        assert!(r.guard.roots().iter().any(|(a, p, acc)| a == "out" && p == &shown && *acc == Access::Ro), "it is listed as @out, read-only: {:?}", r.guard.roots());
        std::fs::write(shown.join(kept_name("term_run")), "alpha\nbeta\n").unwrap();
        let read = r.guard.resolve(&format!("@out/{}", kept_name("term_run")), Op::Read).unwrap();
        assert_eq!((read.root_alias.as_str(), read.trusted), ("out", false));
        assert_eq!(r.guard.resolve(&format!("@OUT/{}", kept_name("term_run")), Op::Read).is_err(), true, "an alias is exact, as for every root");
        // Nothing writes there: not by the alias, not by the real path, not a new file, not a sub-folder.
        for target in [format!("@out/{}", kept_name("term_run")), "@out/new.txt".to_string(), "@out/sub/new.txt".to_string(), win(&shown.join("new.txt"))] {
            let e = r.guard.resolve(&target, Op::Write).unwrap_err();
            assert_eq!((e.code, e.message.as_str()), (ErrorCode::PermissionDenied, "@out is read-only"), "{target}");
        }
        // The alias works for the reads of a link-free path only, like any root.
        assert_eq!(code(r.guard.resolve("@out/../x.txt", Op::Read)), ErrorCode::PermissionDenied);
        // A folder cannot be allowed for writing inside it, and the root cannot be taken back as a grant.
        assert_eq!(r.guard.plan_grant(&win(&shown.join("sub")), Access::Rw).unwrap_err().code, ErrorCode::PermissionDenied);
        assert_eq!(r.guard.revoke_folder("@out").unwrap_err().code, ErrorCode::PermissionDenied);
    }

    #[test]
    fn a_path_in_the_folder_of_kept_output_is_judged_as_that_folder_even_when_a_workspace_holds_it() {
        // The folder sits inside the read-write, trusted workspace (the person works on Aqua itself): a kept page must not be read as a trusted file, nor written through the workspace.
        let r = rig("out_inside_ws");
        r.guard.set_kept(vec![]);
        let shown = r.guard.set_out_root(&r.dir.join("ws").join("tool-output")).unwrap();
        std::fs::write(shown.join(kept_name("web_get")), "a page\n").unwrap();
        let by_ws = format!("@ws/tool-output/{}", kept_name("web_get"));
        let read = r.guard.resolve(&by_ws, Op::Read).unwrap();
        assert_eq!((read.root_alias.as_str(), read.trusted), ("out", false), "judged as @out whichever way it is named");
        assert_eq!(code(r.guard.resolve(&by_ws, Op::Write)), ErrorCode::PermissionDenied);
        assert_eq!(code(r.guard.resolve(&win(&shown.join("x.txt")), Op::Write)), ErrorCode::PermissionDenied);
        // The rest of the workspace is as it was.
        let note = r.guard.resolve("@ws/notes.txt", Op::Write).unwrap();
        assert_eq!((note.root_alias.as_str(), note.trusted), ("ws", true));
        // A grant that asks to write inside the folder is told it is read-only; a read-only one is "already allowed".
        assert!(r.guard.plan_grant(&win(&shown), Access::Rw).unwrap_err().message.contains("@out"));
        assert_eq!(r.guard.plan_grant(&win(&shown), Access::Ro).unwrap().already.as_deref(), Some("out"));
    }

    #[test]
    fn the_folder_of_kept_output_is_refused_inside_aqua_s_own_files_or_around_them_or_under_a_name_a_root_has() {
        let r = rig("out_refused");
        // Inside the protected folder, and around it.
        for bad in [r.dir.join("aqua").join("tool-output"), r.dir.join("aqua").join("tools"), r.dir.clone()] {
            let why = r.guard.set_out_root(&bad).unwrap_err();
            assert!(why.contains("Aqua's own files"), "{}: {why}", bad.display());
        }
        assert!(r.guard.out_root().is_none(), "nothing was set");
        // A root of fs.json called `out` keeps the name; the folder is then not shown.
        let cfg = FsConfig { roots: vec![Root { alias: "out".into(), path: r.dir.join("ws").to_string_lossy().into(), access: Access::Rw, trusted: false }], ..FsConfig::with_workspace("unused") };
        let named = PathGuard::new(cfg, vec![]).unwrap();
        assert!(named.set_out_root(&r.dir.join("kept")).unwrap_err().contains("a root called 'out'"));
        assert!(named.out_root().is_none());
        // A second call replaces the folder, it does not add one.
        r.guard.set_out_root(&r.dir.join("kept_a")).unwrap();
        let second = r.guard.set_out_root(&r.dir.join("kept_b")).unwrap();
        assert_eq!(r.guard.out_root(), Some(second));
        assert_eq!(r.guard.roots().iter().filter(|(a, _, _)| a == "out").count(), 1);
    }

    #[test]
    fn what_was_kept_is_not_a_secret_file_whatever_the_tool_was_called_and_may_be_searched_when_it_is_big() {
        let r = rig("out_sensitive");
        let shown = r.guard.set_out_root(&r.dir.join("kept")).unwrap();
        let name = kept_name("vault_secrets_list");
        std::fs::write(shown.join(&name), "x\n").unwrap();
        // The same name in an ordinary root is a "secret file" (the pattern `*secret*`), in @out it is not: it was masked when it was kept.
        assert!(r.guard.resolve(&format!("@ws/{name}"), Op::Read).unwrap().sensitive);
        assert!(!r.guard.resolve(&format!("@out/{name}"), Op::Read).unwrap().sensitive);
        // A search opens a file of @out up to 16 MiB, any other file up to the read limit.
        assert_eq!(r.guard.search_limit_for(&shown.join(&name)), 16 << 20);
        assert_eq!(r.guard.search_limit_for(&r.dir.join("ws").join("a.txt")), r.guard.limits().read_max_bytes);
        assert!(r.guard.in_out_root(&shown.join(&name)) && !r.guard.in_out_root(&r.dir.join("ws")));
    }

    // ---- card OA6-06 (SE-04, CU-02): the secret names, and the gate a walk asks about each entry ----

    #[test]
    fn the_logins_of_other_programs_are_secret_names_and_source_files_called_token_are_not() {
        let r = rig("sensitive_names_oa606");
        for name in [
            "key.ppk", "Prod.PPK", "backup.gpg", ".pypirc", ".dockercfg", ".git-credentials", ".htpasswd", "store.jks", "app.keystore", "hosts.yml", ".aqua_api_token", "access_token", "db_password", "passwords",
        ] {
            assert!(r.guard.resolve(&format!("@ws/{name}"), Op::Read).unwrap().sensitive, "{name} is a secret name");
            assert!(r.guard.is_sensitive_name(name), "{name}");
        }
        // Source files that only mention the word keep being searched and read.
        for name in ["tokenizer.rs", "token.rs", "PasswordField.tsx", "password_reset.py", "hosts.yaml", "notes.gpg.md", "keystore_notes.txt", "pypirc.md"] {
            assert!(!r.guard.resolve(&format!("@ws/{name}"), Op::Read).unwrap().sensitive, "{name} is not");
            assert!(!r.guard.is_sensitive_name(name), "{name}");
        }
    }

    fn gate_guard(name: &str) -> (PathBuf, PathGuard) {
        let dir = std::env::temp_dir().join(format!("aqua_fsgate_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws").join("data")).unwrap();
        std::fs::create_dir_all(dir.join("outside")).unwrap();
        let cfg = FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string());
        let guard = PathGuard::new(cfg, vec![dir.join("ws").join("data"), dir.join("ws").join(".aqua_ui_token")]).unwrap();
        (canonical(&dir).unwrap(), guard)
    }

    #[test]
    fn a_walk_gate_bars_the_protected_the_credential_folders_and_git_config_by_name() {
        let (dir, guard) = gate_guard("names");
        let ws = dir.join("ws");
        let gate = guard.walk_gate(&ws).unwrap();
        assert_eq!(gate.check("data"), Some(Barred::Protected));
        assert_eq!(gate.check(".aqua_ui_token"), Some(Barred::Protected), "a protected file is barred like a folder");
        assert_eq!(gate.check(".ssh"), Some(Barred::Credential));
        assert_eq!(gate.check(".DOCKER"), Some(Barred::Credential), "names of credential folders are compared without regard to case");
        assert_eq!((gate.check("src"), gate.check("data2"), gate.check(".aqua_ui_token2")), (None, None, None));
        if cfg!(windows) {
            assert_eq!(gate.check("DATA"), Some(Barred::Protected), "Windows names ignore case");
        }
        // `.config/gcloud` is two components: only the one under `.config` is a credential folder.
        let config = gate.enter(".config");
        assert_eq!((config.check("gcloud"), config.check("other")), (Some(Barred::Credential), None));
        assert_eq!(gate.enter("proj").check("gcloud"), None);
        // `.git` may be looked at (its objects are harmless), `.git/config` may not.
        let git = gate.enter(".git");
        assert_eq!((git.check("config"), git.check("HEAD")), (Some(Barred::Credential), None));
        assert_eq!(gate.enter("proj").check("config"), None);
        // The folders of the walk are the ones below the root.
        assert_eq!(config.rel(), [".config".to_string()]);
    }

    #[test]
    fn a_walk_gate_started_inside_a_barred_place_bars_everything_in_it_and_a_path_outside_every_root_is_barred() {
        let (dir, guard) = gate_guard("inside");
        let ws = dir.join("ws");
        assert_eq!(guard.walk_gate(&ws.join("data")).unwrap().check("anything"), Some(Barred::Protected));
        assert_eq!(guard.walk_gate(&ws.join("data").join("deep").join("er")).unwrap().check("x"), Some(Barred::Protected));
        assert_eq!(guard.walk_gate(&ws.join(".aws")).unwrap().check("credentials"), Some(Barred::Credential));
        assert_eq!(guard.walk_gate(&ws.join("a").join(".config").join("gcloud")).unwrap().check("x"), Some(Barred::Credential));
        assert!(guard.walk_gate(&dir.join("outside")).is_none(), "no walk outside the roots");
        // The same answers for a single path.
        assert_eq!(guard.barred(&ws.join("data").join("x.json")), Some(Barred::Protected));
        assert_eq!(guard.barred(&ws.join("data")), Some(Barred::Protected));
        assert_eq!(guard.barred(&ws.join(".ssh").join("id_rsa")), Some(Barred::Credential));
        assert_eq!(guard.barred(&ws.join("proj").join(".git").join("config")), Some(Barred::Credential));
        assert_eq!(guard.barred(&ws.join("proj").join("src").join("a.rs")), None);
        assert_eq!(guard.barred(&dir.join("outside").join("x")), Some(Barred::Protected));
    }
}

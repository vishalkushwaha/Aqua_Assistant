//! Full access for the project: one switch, saved on disk, that stops Aqua asking about ordinary work in this project.
//!
//! The person turns it on from the "Browser and desktop agents" page. Switching it on:
//! - tells the tool host's policy that reads, writes, programs and the network may run without a prompt (the rules it keeps are
//!   in `aqua_tool_host::PolicyEngine::set_full_access`);
//! - adds the project folder to the file tools as a read-write trusted root named `project`;
//! - tells the browser policy and the Windows guard (through `aqua_mcp_adapters::full_access`) that unknown sites, typing into a
//!   browser window and a send shortcut need no question.
//!
//! Switching it off puts every layer back and forgets every "approve for this session" answer given meanwhile.
//!
//! The file is written first, so what is enforced is what is on disk after a restart. It is restored at start-up only for the
//! folder it was given for: an install moved to another folder starts with it off. Only the UI token may change it (the route
//! checks), so the agent and any program using the API token cannot give itself access. `data/tools` is one of the folders no
//! file tool can touch, so it cannot be edited through the tools either.

use aqua_native_tools::fs::PathGuard;
use aqua_tool_host::{Mode, ToolHost};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const FILE: &str = "full_access.json";

/// What is saved: whether it is on, since when, and for which folder.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Saved {
    pub enabled: bool,
    #[serde(default)]
    pub since_unix_ms: u64,
    #[serde(default)]
    pub project_root: String,
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("tools").join(FILE)
}

/// The saved state. A missing file is "off"; a file that cannot be read is also "off" (and says so in the log): a broken file
/// never turns a permission on.
pub fn load(data_dir: &Path) -> Saved {
    let p = path(data_dir);
    match std::fs::read_to_string(&p) {
        Ok(text) => serde_json::from_str::<Saved>(crate::tool_host::strip_bom(&text)).unwrap_or_else(|e| {
            warn!("full access: {} is not valid ({e}); it stays off", p.display());
            Saved::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Saved::default(),
        Err(e) => {
            warn!("full access: {} is unreadable ({e}); it stays off", p.display());
            Saved::default()
        }
    }
}

fn save(data_dir: &Path, saved: &Saved) -> Result<(), String> {
    let p = path(data_dir);
    let text = serde_json::to_string_pretty(saved).map_err(|e| e.to_string())?;
    // Written beside the file, flushed, and renamed over it, so a crash or a power cut leaves the old state, never half of a new one.
    aqua_tool_host::fsutil::write_atomic_durable(&p, text.as_bytes()).map_err(|e| e.to_string())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// One layer a permission passes through, as the page shows it.
#[derive(Debug, Clone, Serialize)]
pub struct Layer {
    pub id: &'static str,
    pub label: &'static str,
    /// `on`, `off` or `unavailable`.
    pub state: &'static str,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub enabled: bool,
    pub since_unix_ms: Option<u64>,
    pub project_root: Option<String>,
    pub layers: Vec<Layer>,
    /// Things that could not be set up (a missing folder, file tools that are off). Empty when everything took effect.
    pub problems: Vec<String>,
    pub runs_without_asking: Vec<&'static str>,
    pub still_asks: Vec<&'static str>,
    pub never_allowed: Vec<&'static str>,
}

const RUNS_WITHOUT_ASKING: &[&str] = &[
    "Reading, searching, writing and editing files in the project (changes are kept for undo)",
    "Running programs and scripts the terminal tool allows",
    "Opening websites that are on no list",
    "Clicking and typing in desktop apps, and typing into a browser window",
    "The agent's calls to the browser and desktop tools",
];

const STILL_ASKS: &[&str] = &[
    "Secrets: passwords, .env files, keys",
    "Deleting something in the project (asked once, then remembered for the session)",
    "Connecting to another machine",
    "Browser uploads, purchases, sending messages and sensitive form fields",
    "Any change after the agent has read untrusted text from a web page or file",
    "Typing into a shell or an administration console",
];

const NEVER_ALLOWED: &[&str] = &[
    "Aqua's own files: tokens, policy, audit log, models, databases",
    "Credential folders (.ssh, .aws and the like) and git configuration",
    "File tools outside the project folder",
    "Privileged actions and actions with an external effect (these always ask)",
    "Tools on the hard-deny list, desktop shells, the registry and the clipboard",
    "Aqua's own window and local services, where the approval prompts live",
    "Programs outside Aqua that use it as an MCP server get none of this",
    "Voice can still only look at the screen and open apps by name",
];

/// What the page shows. `guard` is `None` when the file tools are off (their configuration could not be read).
pub fn status(host: &ToolHost, guard: Option<&PathGuard>, saved: &Saved, problems: Vec<String>) -> Status {
    let enabled = host.policy().full_access();
    let project = guard.and_then(|g| g.project_root());
    let files = match (enabled, guard, &project) {
        (false, _, _) => Layer { id: "files", label: "Project files", state: "off", note: "Only the folders in fs.json are available to file tools.".into() },
        (true, None, _) => Layer { id: "files", label: "Project files", state: "unavailable", note: "The file tools are off (fs.json could not be read), so there is no project folder to add.".into() },
        (true, Some(_), None) => Layer { id: "files", label: "Project files", state: "unavailable", note: "The project folder could not be added.".into() },
        (true, Some(_), Some(p)) => Layer { id: "files", label: "Project files", state: "on", note: format!("Read and write inside {}", p.display()) },
    };
    let (state, tools_note) = if enabled {
        ("on", "Reads, writes, programs and the network run without asking; the rules below still hold.")
    } else {
        ("off", "Asks as the policy mode says (see Tools and approvals).")
    };
    let tools = Layer { id: "tools", label: "Tool calls", state, note: tools_note.into() };
    let browser = Layer {
        id: "browser",
        label: "Browser",
        state,
        note: if enabled { "Sites on no list open without asking; uploads, purchases, sending and sensitive fields still ask.".into() } else { "Asks before opening a site that is on no list.".into() },
    };
    let desktop = Layer {
        id: "desktop",
        label: "Desktop apps",
        state,
        note: if enabled { "Typing into a browser window and Ctrl+Enter no longer ask; shells, administration consoles and Aqua's own window stay protected.".into() } else { "Asks before typing into a browser window or pressing Ctrl+Enter.".into() },
    };
    Status {
        enabled,
        since_unix_ms: (enabled && saved.since_unix_ms > 0).then_some(saved.since_unix_ms),
        project_root: project.map(|p| p.display().to_string()).or_else(|| (enabled && !saved.project_root.is_empty()).then(|| saved.project_root.clone())),
        layers: vec![tools, files, browser, desktop],
        problems,
        runs_without_asking: RUNS_WITHOUT_ASKING.to_vec(),
        still_asks: STILL_ASKS.to_vec(),
        never_allowed: NEVER_ALLOWED.to_vec(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetError {
    /// The tool policy file could not be read, so the host is locked: nothing may be unlocked until it is fixed.
    Locked,
    /// The state could not be saved, so nothing was changed.
    Save(String),
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetError::Locked => write!(f, "the tool policy file could not be read, so the tools are locked; fix data/tools/policy.json first"),
            SetError::Save(e) => write!(f, "could not save the setting, so nothing was changed: {e}"),
        }
    }
}

/// Switch full access on or off for the project at `root`. `set_mcp` flips the browser and desktop layers (injected so that
/// tests do not touch the process-wide switch).
pub fn set_with(host: &ToolHost, guard: Option<&PathGuard>, data_dir: &Path, root: &Path, enable: bool, set_mcp: &dyn Fn(bool)) -> Result<Status, SetError> {
    if enable && host.policy().mode() == Mode::Locked {
        return Err(SetError::Locked);
    }
    let saved = Saved { enabled: enable, since_unix_ms: if enable { now_ms() } else { 0 }, project_root: root.display().to_string() };
    // The disk first: what is enforced is what is saved.
    save(data_dir, &saved).map_err(SetError::Save)?;
    let problems = apply(host, guard, root, enable, set_mcp);
    info!("full access {} for {}", if enable { "enabled" } else { "disabled" }, root.display());
    Ok(status(host, guard, &saved, problems))
}

/// `set_with`, flipping the browser and desktop layers' process-wide switch.
pub fn set(host: &ToolHost, guard: Option<&PathGuard>, data_dir: &Path, root: &Path, enable: bool) -> Result<Status, SetError> {
    set_with(host, guard, data_dir, root, enable, &aqua_mcp_adapters::full_access::set)
}

/// Put every layer in the state `enable` asks for. Returns what could not be set up.
fn apply(host: &ToolHost, guard: Option<&PathGuard>, root: &Path, enable: bool, set_mcp: &dyn Fn(bool)) -> Vec<String> {
    let mut problems = Vec::new();
    if enable {
        match guard {
            Some(g) => {
                if let Err(e) = g.set_project_root(Some(root)) {
                    problems.push(format!("Project files: {e}"));
                }
            }
            None => problems.push("Project files: the file tools are off (fs.json could not be read)".into()),
        }
        host.policy().set_full_access(true);
        set_mcp(true);
    } else {
        // Withdraw the permission first, then what was granted under it.
        host.policy().set_full_access(false);
        set_mcp(false);
        if let Some(g) = guard {
            let _ = g.set_project_root(None);
        }
        host.clear_grants();
    }
    problems
}

/// At start-up: switch it on again if it was on, and only for the folder it was given for.
pub fn restore(host: &ToolHost, guard: Option<&PathGuard>, data_dir: &Path, root: &Path) {
    restore_with(host, guard, data_dir, root, &aqua_mcp_adapters::full_access::set)
}

pub fn restore_with(host: &ToolHost, guard: Option<&PathGuard>, data_dir: &Path, root: &Path, set_mcp: &dyn Fn(bool)) {
    let saved = load(data_dir);
    if !saved.enabled {
        return;
    }
    if host.policy().mode() == Mode::Locked {
        warn!("full access was on, but the tool policy is locked; it stays off");
        return;
    }
    if Path::new(&saved.project_root) != root {
        warn!("full access was given for {} but the project is {}; it stays off", saved.project_root, root.display());
        return;
    }
    for p in apply(host, guard, root, true, set_mcp) {
        warn!("full access: {p}");
    }
    info!("full access restored for {}", root.display());
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_native_tools::fs::{FsConfig, Op};
    use aqua_tool_host::{AuditLedger, PolicyEngine};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct Rig {
        dir: PathBuf,
        project: PathBuf,
        host: ToolHost,
        guard: PathGuard,
        mcp: AtomicBool,
    }

    fn rig(name: &str, mode: Mode) -> Rig {
        let dir = std::env::temp_dir().join(format!("aqua_fullaccess_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let data = dir.join("data");
        std::fs::create_dir_all(data.join("tools")).unwrap();
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        let project = dir.join("project");
        std::fs::create_dir_all(project.join("src")).unwrap();
        let policy = Arc::new(PolicyEngine::new(mode));
        let audit = Arc::new(AuditLedger::open(data.join("tools").join("host.audit.jsonl")));
        let guard = PathGuard::new(FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string()), vec![data.clone()]).unwrap();
        Rig { dir, project, host: ToolHost::new(policy, audit), guard, mcp: AtomicBool::new(false) }
    }

    impl Rig {
        fn data(&self) -> PathBuf {
            self.dir.join("data")
        }
        fn set(&self, enable: bool) -> Result<Status, SetError> {
            set_with(&self.host, Some(&self.guard), &self.data(), &self.project, enable, &|on| self.mcp.store(on, Ordering::SeqCst))
        }
        fn project_file(&self) -> String {
            self.project.join("src").join("lib.rs").to_string_lossy().to_string()
        }
    }

    #[test]
    fn it_is_off_by_default_and_a_missing_or_broken_file_never_turns_it_on() {
        let r = rig("default", Mode::Standard);
        assert!(!r.host.policy().full_access());
        assert_eq!(load(&r.data()), Saved::default());
        std::fs::write(path(&r.data()), "{ not json").unwrap();
        assert!(!load(&r.data()).enabled);
        std::fs::write(path(&r.data()), r#"{"enabled": true, "typo": 1}"#).unwrap();
        assert!(!load(&r.data()).enabled, "an unknown field is a broken file, not a yes");
        std::fs::write(path(&r.data()), "\u{feff}{\"enabled\": true}").unwrap();
        assert!(load(&r.data()).enabled, "a byte-order mark from Notepad is accepted");
    }

    #[test]
    fn enabling_reaches_every_layer_saves_and_shows_the_project() {
        let r = rig("enable", Mode::Standard);
        assert!(r.guard.resolve(&r.project_file(), Op::Write).is_err(), "outside every root before");
        let s = r.set(true).unwrap();
        assert!(s.enabled && s.problems.is_empty(), "{:?}", s.problems);
        assert!(r.host.policy().full_access());
        assert!(r.mcp.load(Ordering::SeqCst), "the browser and desktop layers were told");
        assert!(r.guard.resolve(&r.project_file(), Op::Write).unwrap().trusted);
        assert!(s.layers.iter().all(|l| l.state == "on"), "{:?}", s.layers.iter().map(|l| (l.id, l.state)).collect::<Vec<_>>());
        assert!(s.project_root.as_deref().is_some_and(|p| p.ends_with("project")));
        assert!(s.since_unix_ms.is_some());
        let saved = load(&r.data());
        assert!(saved.enabled && saved.project_root == r.project.display().to_string());
    }

    #[test]
    fn disabling_puts_every_layer_back_and_forgets_what_was_granted() {
        let r = rig("disable", Mode::Standard);
        r.set(true).unwrap();
        let s = r.set(false).unwrap();
        assert!(!s.enabled && s.since_unix_ms.is_none());
        assert!(!r.host.policy().full_access());
        assert!(!r.mcp.load(Ordering::SeqCst));
        assert!(r.guard.resolve(&r.project_file(), Op::Write).is_err(), "the project folder is withdrawn");
        assert!(s.layers.iter().all(|l| l.state == "off"));
        assert!(!load(&r.data()).enabled);
    }

    #[test]
    fn a_restart_restores_it_only_for_the_same_folder() {
        let r = rig("restore", Mode::Standard);
        r.set(true).unwrap();
        // A fresh host (the "restart"): off until restored.
        let again = rig_again(&r);
        assert!(!again.host.policy().full_access());
        restore_with(&again.host, Some(&again.guard), &again.data(), &again.project, &|on| again.mcp.store(on, Ordering::SeqCst));
        assert!(again.host.policy().full_access() && again.mcp.load(Ordering::SeqCst));
        assert!(again.guard.resolve(&again.project_file(), Op::Write).is_ok());
        // The install moved to another folder: the saved yes was for the old one.
        let moved = rig_again(&r);
        restore_with(&moved.host, Some(&moved.guard), &moved.data(), &r.dir.join("elsewhere"), &|on| moved.mcp.store(on, Ordering::SeqCst));
        assert!(!moved.host.policy().full_access(), "not restored for a different project folder");
    }

    /// A second host and guard on the same data folder, as a restart would build.
    fn rig_again(r: &Rig) -> Rig {
        let policy = Arc::new(PolicyEngine::new(Mode::Standard));
        let audit = Arc::new(AuditLedger::open(r.data().join("tools").join("host2.audit.jsonl")));
        let guard = PathGuard::new(FsConfig::with_workspace(r.dir.join("ws").to_string_lossy().to_string()), vec![r.data()]).unwrap();
        Rig { dir: r.dir.clone(), project: r.project.clone(), host: ToolHost::new(policy, audit), guard, mcp: AtomicBool::new(false) }
    }

    #[test]
    fn a_locked_host_cannot_be_unlocked_by_it() {
        let r = rig("locked", Mode::Locked);
        assert_eq!(r.set(true).unwrap_err(), SetError::Locked);
        assert!(!r.host.policy().full_access());
        assert!(!load(&r.data()).enabled, "nothing was saved");
        // Switching it off is always allowed.
        assert!(r.set(false).is_ok());
    }

    #[test]
    fn a_project_folder_that_is_missing_is_reported_while_the_rest_still_takes_effect() {
        let mut r = rig("missing", Mode::Standard);
        r.project = r.dir.join("gone");
        let s = r.set(true).unwrap();
        assert!(s.enabled);
        assert!(s.problems.iter().any(|p| p.contains("does not exist")), "{:?}", s.problems);
        let files = s.layers.iter().find(|l| l.id == "files").unwrap();
        assert_eq!(files.state, "unavailable");
        assert_eq!(s.layers.iter().find(|l| l.id == "tools").unwrap().state, "on");
    }

    #[test]
    fn without_file_tools_it_says_so() {
        let r = rig("nofs", Mode::Standard);
        let s = set_with(&r.host, None, &r.data(), &r.project, true, &|_| {}).unwrap();
        assert!(s.enabled);
        assert!(s.problems.iter().any(|p| p.contains("file tools are off")));
        assert_eq!(s.layers.iter().find(|l| l.id == "files").unwrap().state, "unavailable");
    }

    #[test]
    fn a_save_that_fails_changes_nothing() {
        let r = rig("savefail", Mode::Standard);
        // The tools folder is a file, so the state cannot be written.
        std::fs::remove_dir_all(r.data().join("tools")).unwrap();
        std::fs::write(r.data().join("tools"), "in the way").unwrap();
        assert!(matches!(r.set(true), Err(SetError::Save(_))));
        assert!(!r.host.policy().full_access(), "what is enforced must be what is saved");
        assert!(!r.mcp.load(Ordering::SeqCst));
    }
}

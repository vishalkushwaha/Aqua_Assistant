//! Browser MCP detect → setup → start → connect → verify.
//!
//! Contract: ready only when
//! `installed && configured && server_running && browser_connected && browser_action_available`.
//!
//! Does not touch passwords, cookies, or session tokens. Extension Connect is a user action.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::stdio_mcp::StdioMcpClient;

use super::client::{DEFAULT_BROWSER_MCP_ARGS, DEFAULT_BROWSER_MCP_COMMAND};

pub const EXTENSION_SETUP_URL: &str = "https://docs.browsermcp.io/setup-extension";
pub const BROWSER_MCP_PACKAGE: &str = "@browsermcp/mcp@0.1.3";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserMcpFailStage {
    None,
    Install,
    Configure,
    ServerStart,
    BrowserConnect,
    ActionProbe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserMcpHealth {
    pub installed: bool,
    pub configured: bool,
    pub server_running: bool,
    pub browser_connected: bool,
    pub browser_action_available: bool,
    /// True only when all five contract flags are true.
    pub ready: bool,
    pub stage: BrowserMcpFailStage,
    pub detail: String,
    pub user_action_required: Option<String>,
    pub auto_repaired: bool,
    pub tool_count: usize,
    pub tools: Vec<String>,
    pub package: String,
    pub extension_url: String,
}

/// The last refusal of an open listener and when it was made. The health route starts the server when it is down, and the app asks for health every few seconds: without
/// this a refused Browser MCP would be started and stopped again each time, which costs a Node process and, at its start, runs the package's own `taskkill`.
static LAST_REFUSAL: Mutex<Option<(Instant, crate::listening::OpenListener)>> = Mutex::new(None);
const REFUSAL_REMEMBERED: Duration = Duration::from_secs(120);

/// The refusal made less than `REFUSAL_REMEMBERED` ago, unless the person has accepted open listeners since.
fn remembered_refusal_in(store: &Mutex<Option<(Instant, crate::listening::OpenListener)>>, now: Instant, accepted: bool) -> Option<crate::listening::OpenListener> {
    if accepted {
        return None;
    }
    let held = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    held.as_ref().filter(|(at, _)| now.saturating_duration_since(*at) < REFUSAL_REMEMBERED).map(|(_, open)| open.clone())
}

fn health_of_refusal(open: &crate::listening::OpenListener) -> BrowserMcpHealth {
    let mut h = BrowserMcpHealth::not_ready(BrowserMcpFailStage::ServerStart, open.to_string());
    h.installed = true;
    h.configured = true;
    h.user_action_required = Some(format!(
        "Browser MCP (the Chrome extension connection) does not let Aqua choose where it listens. Use the managed browser instead, or set {}=1 if the firewall blocks port {} from other devices.",
        crate::listening::ALLOW_OPEN_LISTENER_ENV,
        open.port
    ));
    h
}

fn open_listeners_accepted() -> bool {
    std::env::var(crate::listening::ALLOW_OPEN_LISTENER_ENV).is_ok_and(|v| v.trim() == "1")
}

/// A listener that is open to the network is a state the person can act on, not a crash: it comes back as the health of the connection, and is remembered for a while.
fn refused_listener(e: &anyhow::Error) -> Option<BrowserMcpHealth> {
    let open = e.downcast_ref::<crate::listening::OpenListener>()?;
    *LAST_REFUSAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), open.clone()));
    Some(health_of_refusal(open))
}

/// The health of a refusal that was made a moment ago: the server is not started again to be refused again.
fn recent_refusal() -> Option<BrowserMcpHealth> {
    remembered_refusal_in(&LAST_REFUSAL, Instant::now(), open_listeners_accepted()).map(|open| health_of_refusal(&open))
}

impl BrowserMcpHealth {
    pub fn not_ready(stage: BrowserMcpFailStage, detail: impl Into<String>) -> Self {
        Self {
            installed: false,
            configured: false,
            server_running: false,
            browser_connected: false,
            browser_action_available: false,
            ready: false,
            stage,
            detail: detail.into(),
            user_action_required: None,
            auto_repaired: false,
            tool_count: 0,
            tools: Vec::new(),
            package: BROWSER_MCP_PACKAGE.into(),
            extension_url: EXTENSION_SETUP_URL.into(),
        }
    }

    fn recompute_ready(&mut self) {
        self.ready = self.installed
            && self.configured
            && self.server_running
            && self.browser_connected
            && self.browser_action_available;
        if self.ready {
            self.stage = BrowserMcpFailStage::None;
        }
    }
}

#[derive(Debug, Clone)]
pub struct EnsureBrowserMcpOptions {
    /// Run install/setup scripts when Node/package/config missing.
    pub auto_setup: bool,
    /// Start the stdio MCP process when not running.
    pub auto_start: bool,
    /// Seconds to poll for Chrome extension Connect after opening docs.
    pub poll_connect_secs: u64,
    /// Open extension setup docs in the default browser when disconnected.
    pub open_extension_docs: bool,
    /// Optional progress callback: (phase, message).
    pub on_progress: Option<fn(&str, &str)>,
}

impl Default for EnsureBrowserMcpOptions {
    fn default() -> Self {
        Self {
            auto_setup: true,
            auto_start: true,
            poll_connect_secs: 12,
            open_extension_docs: true,
            on_progress: None,
        }
    }
}

fn progress(opts: &EnsureBrowserMcpOptions, phase: &str, message: &str) {
    info!("browser_mcp health [{phase}]: {message}");
    if let Some(cb) = opts.on_progress {
        cb(phase, message);
    }
}

/// The folder Aqua keeps its `data\` in. The one rule every part of Aqua (the core, `aqua-cli`, the browser setup,
/// the desktop shell) follows, in this order:
///
/// 1. `AQUA_WORKSPACE_ROOT`, when set and not empty;
/// 2. a development checkout: the repository root, found by walking up from the current folder and then from the
///    program's own folder (so `target\release\aqua-core.exe` started from anywhere still uses the repository's
///    `data\`, and a release build in the repository agrees with the dev server and the scripts);
/// 3. otherwise an installed copy: `%LOCALAPPDATA%\Aqua`.
///
/// Whether it is a debug or a release build does not matter.
///
/// `#[track_caller]` and the observer exist for the data-folder tripwire of the tests (task QA-04, `aqua_core::runtime::data_tripwire`): a
/// test binary may install one function that is told every lookup and where it was made. Nothing installs one in a running Aqua, so
/// there the lookup costs one extra read of an empty cell.
#[track_caller]
pub fn aqua_workspace_root() -> PathBuf {
    let root = match std::env::var_os(WORKSPACE_ROOT_ENV).filter(|v| !v.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => natural_workspace_root(),
    };
    if let Some(observer) = ROOT_OBSERVER.get() {
        observer(&root, std::panic::Location::caller());
    }
    root
}

/// The environment variable that moves the folder Aqua keeps its data in.
pub const WORKSPACE_ROOT_ENV: &str = "AQUA_WORKSPACE_ROOT";

/// What `aqua_workspace_root` gives when `AQUA_WORKSPACE_ROOT` says nothing: the repository root of a development checkout, else `%LOCALAPPDATA%\Aqua`.
/// This is the owner's real data folder, whoever asks, so a test uses it to tell the real folder from one of its own.
pub fn natural_workspace_root() -> PathBuf {
    let cwd = std::env::current_dir().ok();
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_workspace_root(cwd.as_deref(), exe_dir.as_deref(), std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(PathBuf::from))
}

/// A function told every lookup of the workspace folder: the folder it found and the place that asked.
pub type WorkspaceRootObserver = fn(root: &Path, asked_at: &'static std::panic::Location<'static>);

static ROOT_OBSERVER: OnceLock<WorkspaceRootObserver> = OnceLock::new();

/// Test support: installs the function that hears every lookup (once per process; a second call changes nothing).
pub fn observe_workspace_root(observer: WorkspaceRootObserver) {
    let _ = ROOT_OBSERVER.set(observer);
}

/// The pure part of `aqua_workspace_root`, so it can be tested without touching the real environment.
pub fn resolve_workspace_root(cwd: Option<&Path>, exe_dir: Option<&Path>, local_app_data: Option<PathBuf>) -> PathBuf {
    for start in [cwd, exe_dir].into_iter().flatten() {
        if let Some(root) = find_repo_root(start) {
            return root;
        }
    }
    local_app_data.unwrap_or_else(|| PathBuf::from("C:\\Users\\Public\\AppData\\Local")).join("Aqua")
}

/// Walk up (at most six levels) from `start` to a repository root.
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = start.to_path_buf();
    for _ in 0..6 {
        if cur.join("scripts").join("setup-browser-mcp.ps1").is_file()
            || cur.join("scripts").join("setup_browser_mcp.ps1").is_file()
            || cur.join("Cargo.toml").is_file() && cur.join("adapters").is_dir()
        {
            return Some(cur);
        }
        if !cur.pop() {
            break;
        }
    }
    None
}

pub fn browser_mcp_state_path(root: &Path) -> PathBuf {
    root.join("data").join("browser_mcp").join("state.json")
}

pub fn node_available() -> bool {
    which_cmd("node").is_some()
}

pub fn npx_available() -> bool {
    which_cmd("npx").is_some()
}

fn which_cmd(name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        Command::new("where")
            .arg(name)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .map(|l| PathBuf::from(l.trim()))
            })
    }
    #[cfg(not(windows))]
    {
        Command::new("which")
            .arg(name)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .map(|l| PathBuf::from(l.trim()))
            })
    }
}

fn state_configured(root: &Path) -> bool {
    let p = browser_mcp_state_path(root);
    if !p.is_file() {
        return false;
    }
    std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| v.get("setup_ok").and_then(|x| x.as_bool()) == Some(true))
        .unwrap_or(false)
}

/// Run project setup script (idempotent). Returns parsed JSON when `-Json` is supported.
pub fn run_browser_mcp_setup_script(root: &Path) -> Result<Value> {
    let ps1 = root.join("scripts").join("setup-browser-mcp.ps1");
    let legacy = root.join("scripts").join("setup_browser_mcp.ps1");
    let _sh = root.join("scripts").join("setup-browser-mcp.sh");

    #[cfg(windows)]
    {
        let script = if ps1.is_file() {
            ps1
        } else if legacy.is_file() {
            legacy
        } else {
            bail!("Missing scripts/setup-browser-mcp.ps1 under {}", root.display());
        };
        let output = Command::new("powershell")
            .args([
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                script.to_str().ok_or_else(|| anyhow!("non-utf8 script path"))?,
                "-Json",
            ])
            .current_dir(root)
            .output()
            .context("run setup-browser-mcp.ps1")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(json_line) = extract_json_object(&stdout) {
            return Ok(json_line);
        }
        if !output.status.success() {
            bail!(
                "setup-browser-mcp failed (exit {:?}): {stderr}{stdout}",
                output.status.code()
            );
        }
        Ok(json!({
            "ok": true,
            "installed": true,
            "configured": true,
            "detail": stdout.trim(),
        }))
    }

    #[cfg(not(windows))]
    {
        let script = if sh.is_file() {
            sh
        } else {
            bail!("Missing scripts/setup-browser-mcp.sh under {}", root.display());
        };
        let output = Command::new("bash")
            .arg(&script)
            .arg("--json")
            .current_dir(root)
            .output()
            .context("run setup-browser-mcp.sh")?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Some(json_line) = extract_json_object(&stdout) {
            return Ok(json_line);
        }
        if !output.status.success() {
            bail!(
                "setup-browser-mcp failed (exit {:?}): {stderr}{stdout}",
                output.status.code()
            );
        }
        Ok(json!({
            "ok": true,
            "installed": true,
            "configured": true,
            "detail": stdout.trim(),
        }))
    }
}

fn extract_json_object(stdout: &str) -> Option<Value> {
    // Prefer last line that looks like a JSON object (scripts may print human text first).
    for line in stdout.lines().rev() {
        let t = line.trim();
        if t.starts_with('{') && t.ends_with('}') {
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                return Some(v);
            }
        }
    }
    let start = stdout.find('{')?;
    let end = stdout.rfind('}')?;
    if end > start {
        serde_json::from_str(&stdout[start..=end]).ok()
    } else {
        None
    }
}

/// Open a web address in the person's default browser. It goes through `shell_open` (the address is data, only http and
/// https are accepted); the old version built `cmd /C start "" <url>`, where `&`, `^` and `%` in a crafted address are
/// command syntax.
pub fn open_url_in_default_browser(url: &str) -> Result<()> {
    crate::shell_open::open_web_url(url)
}

pub fn observation_indicates_extension_offline(text: &str) -> bool {
    let l = text.to_ascii_lowercase();
    l.contains("no connection to browser extension")
        || (l.contains("browser extension")
            && (l.contains("connect") || l.contains("not connected")))
        || (l.contains("extension")
            && (l.contains("not connected")
                || l.contains("please connect")
                || l.contains("click connect")))
        || l.contains("extension is not connected")
        || l.contains("browser is not connected")
}

fn tool_names(client: &StdioMcpClient) -> Vec<String> {
    client
        .cached_tools()
        .iter()
        .map(|t| t.name.clone())
        .collect()
}

fn has_browser_control_tools(client: &StdioMcpClient) -> bool {
    let names: Vec<_> = client
        .cached_tools()
        .iter()
        .map(|t| t.name.to_ascii_lowercase())
        .collect();
    names.iter().any(|n| {
        matches!(
            n.as_str(),
            "browser_navigate"
                | "browser_snapshot"
                | "browser_click"
                | "browser_type"
                | "browser_fill"
                | "browser_press"
        )
    })
}

/// Probe a real browser tool. Returns (connected_and_action_ok, detail).
pub fn probe_browser_action(client: &mut StdioMcpClient) -> (bool, String) {
    if !client.is_initialized() {
        return (false, "MCP server not initialized".into());
    }
    if client.cached_tools().is_empty() {
        if let Err(e) = client.list_tools() {
            return (false, format!("tools/list failed: {e}"));
        }
    }
    if !has_browser_control_tools(client) {
        return (
            false,
            "Browser MCP started but no browser_* control tools were listed".into(),
        );
    }

    let names: Vec<_> = client
        .cached_tools()
        .iter()
        .map(|t| t.name.clone())
        .collect();

    let attempt = if names.iter().any(|n| n == "browser_snapshot") {
        client.call_tool("browser_snapshot", json!({}))
    } else if names.iter().any(|n| n == "browser_navigate") {
        client.call_tool("browser_navigate", json!({ "url": "about:blank" }))
    } else {
        return (false, "No probe tool (browser_snapshot / browser_navigate)".into());
    };

    match attempt {
        Ok(value) => {
            let text = value.to_string();
            if observation_indicates_extension_offline(&text) {
                (
                    false,
                    "Browser MCP server is up but Chrome extension is not connected".into(),
                )
            } else {
                (true, "Browser action probe succeeded".into())
            }
        }
        Err(e) => {
            let msg = e.to_string();
            if observation_indicates_extension_offline(&msg) {
                (
                    false,
                    "Browser MCP server is up but Chrome extension is not connected".into(),
                )
            } else {
                (false, format!("Browser action probe failed: {msg}"))
            }
        }
    }
}

/// Passive-only health snapshot (may start the server when `auto_start` if already installed).
pub fn check_browser_mcp_health(
    client: &mut StdioMcpClient,
    opts: &EnsureBrowserMcpOptions,
) -> BrowserMcpHealth {
    let root = aqua_workspace_root();
    let mut health = BrowserMcpHealth::not_ready(
        BrowserMcpFailStage::Install,
        "Checking Browser MCP prerequisites…",
    );
    health.installed = node_available() && npx_available();
    if !health.installed {
        health.stage = BrowserMcpFailStage::Install;
        health.detail =
            "Node.js/npx not found. Install Node.js from https://nodejs.org then re-run.".into();
        health.user_action_required =
            Some("Install Node.js LTS and ensure `node` + `npx` are on PATH.".into());
        return health;
    }

    health.configured = state_configured(&root)
        || std::env::var("AQUA_BROWSER_MCP_COMMAND").is_ok()
        || DEFAULT_BROWSER_MCP_COMMAND == "npx";
    // Defaults count as configured once Node is present; state file records explicit setup.
    if !state_configured(&root) {
        health.configured = true; // env defaults are valid config
        health.detail = "Using default npx @browsermcp/mcp@0.1.3 configuration.".into();
    } else {
        health.detail = "Browser MCP setup state found.".into();
    }

    if opts.auto_start && !client.is_initialized() {
        if let Some(refused) = recent_refusal() {
            return refused;
        }
        match client.start() {
            Ok(_) => {}
            Err(e) => {
                if let Some(refused) = refused_listener(&e) {
                    return refused;
                }
                health.stage = BrowserMcpFailStage::ServerStart;
                health.server_running = false;
                health.detail = format!("Failed to start Browser MCP: {e}");
                health.user_action_required = Some(
                    "Run scripts/setup-browser-mcp then POST /api/v1/browser-mcp/start.".into(),
                );
                return health;
            }
        }
    }

    health.server_running = client.is_running() && client.is_initialized();
    if !health.server_running {
        health.stage = BrowserMcpFailStage::ServerStart;
        health.detail = "Browser MCP process is not running.".into();
        return health;
    }

    if client.cached_tools().is_empty() {
        let _ = client.list_tools();
    }
    health.tools = tool_names(client);
    health.tool_count = health.tools.len();

    let (ok, detail) = probe_browser_action(client);
    health.browser_connected = ok;
    health.browser_action_available = ok;
    health.detail = detail;
    if !ok {
        health.stage = if health.detail.to_ascii_lowercase().contains("extension") {
            BrowserMcpFailStage::BrowserConnect
        } else {
            BrowserMcpFailStage::ActionProbe
        };
        health.user_action_required = Some(format!(
            "Install the Browser MCP Chrome extension and click Connect ({EXTENSION_SETUP_URL}). \
Do not share passwords with Aqua — sign in inside Chrome yourself if a site requires it."
        ));
    }
    health.recompute_ready();
    health
}

/// Full ensure path: setup if needed → start → probe → optional connect poll.
pub fn ensure_browser_mcp_ready(
    client: &mut StdioMcpClient,
    opts: EnsureBrowserMcpOptions,
) -> Result<BrowserMcpHealth> {
    let root = aqua_workspace_root();
    let mut auto_repaired = false;

    progress(&opts, "detect", "Checking Node.js / npx…");
    if !(node_available() && npx_available()) {
        let mut h = BrowserMcpHealth::not_ready(
            BrowserMcpFailStage::Install,
            "Node.js/npx missing — cannot install Browser MCP automatically without Node.",
        );
        h.user_action_required =
            Some("Install Node.js LTS from https://nodejs.org, then retry the task.".into());
        return Ok(h);
    }

    let needs_setup = !state_configured(&root);
    if needs_setup && opts.auto_setup {
        progress(&opts, "setup", "Running scripts/setup-browser-mcp (idempotent)…");
        match run_browser_mcp_setup_script(&root) {
            Ok(report) => {
                auto_repaired = true;
                let ok = report.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
                if !ok {
                    let detail = report
                        .get("error")
                        .or_else(|| report.get("detail"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("setup reported failure")
                        .to_string();
                    let mut h =
                        BrowserMcpHealth::not_ready(BrowserMcpFailStage::Configure, detail);
                    h.installed = true;
                    h.user_action_required = report
                        .get("user_action_required")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    return Ok(h);
                }
            }
            Err(e) => {
                warn!("Browser MCP setup script failed: {e:#}");
                // Continue — start may still work if npx can fetch on the fly.
                auto_repaired = true;
            }
        }
    }

    if opts.auto_start {
        progress(
            &opts,
            "start",
            &format!(
                "Starting Browser MCP ({} {:?})…",
                DEFAULT_BROWSER_MCP_COMMAND, DEFAULT_BROWSER_MCP_ARGS
            ),
        );
        if !client.is_initialized() {
            if let Some(refused) = recent_refusal() {
                return Ok(refused);
            }
            if let Err(e) = client.start() {
                if let Some(refused) = refused_listener(&e) {
                    return Ok(refused);
                }
                return Err(e.context("start Browser MCP stdio server"));
            }
            auto_repaired = true;
        }
    }

    let mut health = check_browser_mcp_health(
        client,
        &EnsureBrowserMcpOptions {
            auto_setup: false,
            auto_start: false,
            ..opts.clone()
        },
    );
    health.auto_repaired = auto_repaired;

    if health.ready {
        progress(&opts, "verify", "Browser MCP ready (action probe OK)");
        return Ok(health);
    }

    if health.stage == BrowserMcpFailStage::BrowserConnect
        || health.stage == BrowserMcpFailStage::ActionProbe
    {
        if opts.open_extension_docs {
            progress(
                &opts,
                "connect",
                &format!("Opening extension setup docs: {EXTENSION_SETUP_URL}"),
            );
            let _ = open_url_in_default_browser(EXTENSION_SETUP_URL);
        }

        let deadline = Instant::now() + Duration::from_secs(opts.poll_connect_secs);
        while Instant::now() < deadline {
            progress(
                &opts,
                "connect",
                "Waiting for Chrome extension Connect… (open extension → Connect)",
            );
            std::thread::sleep(Duration::from_secs(2));
            let (ok, detail) = probe_browser_action(client);
            if ok {
                health.browser_connected = true;
                health.browser_action_available = true;
                health.detail = detail;
                health.user_action_required = None;
                health.auto_repaired = true;
                health.recompute_ready();
                progress(&opts, "verify", "Browser connection established");
                return Ok(health);
            }
            health.detail = detail;
        }

        health.user_action_required = Some(format!(
            "Browser MCP server is running, but the Chrome extension is not connected. \
Install/open the extension and click Connect: {EXTENSION_SETUP_URL}. \
Then re-run the task (or keep the agent running and Connect — Aqua will retry browser tools)."
        ));
        health.stage = BrowserMcpFailStage::BrowserConnect;
    }

    Ok(health)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_repository_is_recognised_from_a_subfolder_or_from_next_to_the_program_and_otherwise_it_is_the_installed_folder() {
        let base = std::env::temp_dir().join(format!("aqua_root_rule_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join("adapters")).unwrap();
        std::fs::create_dir_all(repo.join("target").join("release")).unwrap();
        std::fs::create_dir_all(repo.join("core").join("src")).unwrap();
        std::fs::write(repo.join("Cargo.toml"), "[workspace]").unwrap();
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let installed = base.join("LocalAppData");
        let local = Some(installed.clone());

        // started from a subfolder, or from the repository root: the repository root
        assert_eq!(resolve_workspace_root(Some(&repo.join("core").join("src")), None, local.clone()), repo);
        // a release exe in the repository, started from some other folder: still the repository (the case that
        // broke when this depended on debug_assertions)
        assert_eq!(resolve_workspace_root(Some(&elsewhere), Some(&repo.join("target").join("release")), local.clone()), repo);
        // an installed program started from anywhere with no repository around: the installed folder
        assert_eq!(resolve_workspace_root(Some(&elsewhere), Some(&elsewhere), local.clone()), installed.join("Aqua"));
        assert_eq!(resolve_workspace_root(None, None, local), installed.join("Aqua"));
        assert!(find_repo_root(&elsewhere).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    use super::*;

    #[test]
    fn a_refusal_is_remembered_for_two_minutes_so_the_server_is_not_started_to_be_refused_again_unless_the_person_accepted_it() {
        let open = crate::listening::OpenListener { label: "Browser MCP".into(), port: 9009, addresses: vec!["0.0.0.0:9009".into()] };
        let store = Mutex::new(None);
        let at = Instant::now();
        assert!(remembered_refusal_in(&store, at, false).is_none(), "nothing was refused yet");
        *store.lock().unwrap() = Some((at, open.clone()));
        assert_eq!(remembered_refusal_in(&store, at + Duration::from_secs(5), false), Some(open.clone()));
        assert_eq!(remembered_refusal_in(&store, at + Duration::from_secs(119), false), Some(open.clone()));
        assert!(remembered_refusal_in(&store, at + Duration::from_secs(121), false).is_none(), "after two minutes it is tried again");
        assert!(remembered_refusal_in(&store, at + Duration::from_secs(5), true).is_none(), "the person accepted open listeners since");
        // What the app is told: not ready, at the start of the server, with the way out in plain words.
        let h = health_of_refusal(&open);
        assert!(!h.ready && h.stage == BrowserMcpFailStage::ServerStart && h.installed && h.configured);
        let said = h.user_action_required.unwrap();
        assert!(said.contains("AQUA_ALLOW_OPEN_MCP_LISTENER=1") && said.contains("9009") && h.detail.contains("every network interface"), "{said} / {}", h.detail);
    }

    #[test]
    fn offline_detection_matches_known_errors() {
        assert!(observation_indicates_extension_offline(
            "Error: No connection to browser extension"
        ));
        assert!(observation_indicates_extension_offline(
            "Please connect the Browser MCP extension"
        ));
        assert!(!observation_indicates_extension_offline(
            "Navigated to https://mail.google.com"
        ));
    }

    #[test]
    fn ready_requires_all_five_flags() {
        let mut h = BrowserMcpHealth::not_ready(BrowserMcpFailStage::None, "x");
        h.installed = true;
        h.configured = true;
        h.server_running = true;
        h.browser_connected = true;
        h.browser_action_available = false;
        h.recompute_ready();
        assert!(!h.ready);
        h.browser_action_available = true;
        h.recompute_ready();
        assert!(h.ready);
    }
}

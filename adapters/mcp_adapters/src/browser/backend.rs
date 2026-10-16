//! `BrowserBackend`: the only way Aqua touches a browser. Owns the managed (Playwright MCP) and
//! extension (Browser MCP) clients, and runs every call through [`BrowserPolicyConfig`].

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use super::audit::{now_ms, redact_args, AuditEntry, AuditLog};
use super::confirm::{ConfirmKind, ConfirmationBroker, Decision};
use super::extract::{self, ExtractKind};
use super::managed::{browser_installed, managed_browser_config, managed_browser_installed, BrowserDirs};
use super::observation::{
    cap_text, classify_error, compact_snapshot, label_for_target, parse_sections, result_text,
    Artifact, BrowserErrorCode, BrowserObservation, Confirmation, PlaywrightSections,
};
use super::policy::{
    is_idempotent, is_irreversible_label, is_sensitive_field_label, is_submit_shortcut,
    is_snapshot_ref, resolve_upload_paths, sanitize_args, validate_args, BrowserChannel, BrowserPolicyConfig, ProfileMode,
    Resolver, SystemResolver, UrlDecision, AQUA_TOOLS, EXTENSION_TOOLS, MANAGED_TOOLS,
};
use crate::browser_mcp::{ensure_browser_mcp_ready, BrowserMcpHealth, EnsureBrowserMcpOptions};
use crate::stdio_mcp::{McpTool, StdioMcpClient};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// Playwright MCP + installed Edge/Chrome in an Aqua-only profile (default).
    Managed,
    /// Browser MCP + Chrome extension in the user's own profile (opt-in).
    Extension,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Managed => "managed",
            BackendKind::Extension => "extension",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "managed" => Some(Self::Managed),
            "extension" => Some(Self::Extension),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BrowserReadiness {
    pub backend: BackendKind,
    pub ready: bool,
    pub detail: String,
    pub user_action_required: Option<String>,
    /// Full Browser MCP contract when the extension backend is active.
    pub extension_health: Option<BrowserMcpHealth>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BrowserStatus {
    pub backend: BackendKind,
    pub installed: bool,
    pub running: bool,
    pub initialized: bool,
    pub tool_count: usize,
    pub tabs: usize,
    pub url: Option<String>,
    pub browser: String,
    pub profile: ProfileMode,
    pub headless: bool,
    pub pending_confirmations: usize,
    pub hint: String,
    /// Whether the tools the active browser server lists are the ones last recorded (see `tool_lock`).
    #[serde(default)]
    pub tool_lock: crate::tool_lock::LockReport,
}

#[derive(Debug, Default, Clone)]
struct PageState {
    url: Option<String>,
    /// Latest full (uncompacted) snapshot, used to label targets for confirmations.
    snapshot: String,
    tabs: usize,
}

pub struct BrowserBackend {
    workspace_root: PathBuf,
    dirs: BrowserDirs,
    kind: RwLock<BackendKind>,
    managed: Arc<Mutex<StdioMcpClient>>,
    extension: Arc<Mutex<StdioMcpClient>>,
    policy: RwLock<BrowserPolicyConfig>,
    /// Turns a host name into addresses for the site policy (before a page opens and after it has loaded). The computer's own resolver; a test sets a fixed one.
    resolver: RwLock<Arc<dyn Resolver>>,
    state: Mutex<PageState>,
    /// Sites allowed "once" during this Aqua run.
    session_sites: Mutex<HashSet<String>>,
    broker: ConfirmationBroker,
    audit: AuditLog,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl BrowserBackend {
    /// `extension` is the existing Browser MCP client (kept for "Use my Chrome" mode and the
    /// legacy `/api/v1/browser-mcp/*` endpoints).
    pub fn new(workspace_root: &Path, extension: StdioMcpClient) -> Self {
        Self::with_dirs(workspace_root, BrowserDirs::new(workspace_root), extension)
    }

    /// Like [`Self::new`] with browser data in `dirs` instead of `data/browser`.
    pub fn with_dirs(workspace_root: &Path, dirs: BrowserDirs, extension: StdioMcpClient) -> Self {
        if let Err(e) = dirs.create_all() {
            warn!("could not create {}: {e}", dirs.root.display());
        }
        let policy = BrowserPolicyConfig::load_or_create(&dirs.policy_file).unwrap_or_else(|e| {
            warn!("{e:#} — using the default (ask-first) browser policy until the file is fixed");
            BrowserPolicyConfig::default()
        });
        let kind = std::env::var("AQUA_BROWSER_BACKEND")
            .ok()
            .and_then(|v| BackendKind::parse(&v))
            .unwrap_or(BackendKind::Managed);
        let managed = StdioMcpClient::new(managed_browser_config(workspace_root, &dirs, &policy));
        Self {
            workspace_root: workspace_root.to_path_buf(),
            audit: AuditLog::new(dirs.audit_file.clone()),
            dirs,
            kind: RwLock::new(kind),
            managed: Arc::new(Mutex::new(managed)),
            extension: Arc::new(Mutex::new(extension)),
            policy: RwLock::new(policy),
            resolver: RwLock::new(Arc::new(SystemResolver)),
            state: Mutex::new(PageState::default()),
            session_sites: Mutex::new(HashSet::new()),
            broker: ConfirmationBroker::default(),
        }
    }

    pub fn kind(&self) -> BackendKind {
        *self.kind.read().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_kind(&self, kind: BackendKind) {
        *self.kind.write().unwrap_or_else(PoisonError::into_inner) = kind;
        *lock(&self.state) = PageState::default();
        info!("Browser backend set to {}", kind.as_str());
    }

    pub fn extension_client(&self) -> Arc<Mutex<StdioMcpClient>> {
        Arc::clone(&self.extension)
    }

    pub fn broker(&self) -> &ConfirmationBroker {
        &self.broker
    }

    pub fn dirs(&self) -> &BrowserDirs {
        &self.dirs
    }

    pub fn policy(&self) -> BrowserPolicyConfig {
        self.policy.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Replace the resolver the site policy uses for host names (for tests: a fixed answer instead of the network).
    pub fn set_resolver(&self, resolver: Arc<dyn Resolver>) {
        *self.resolver.write().unwrap_or_else(PoisonError::into_inner) = resolver;
    }

    fn resolver(&self) -> Arc<dyn Resolver> {
        Arc::clone(&self.resolver.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The site policy's decision for `url`, names resolved: before a page is opened, after it has loaded (the second look-up is what catches a name that pointed
    /// somewhere else by then), and when going back.
    fn decide_url(&self, policy: &BrowserPolicyConfig, url: &str) -> UrlDecision {
        policy.check_url_resolving(url, crate::full_access::is_on(), self.resolver().as_ref())
    }

    /// The sites the person allowed for good.
    pub fn allowed_sites(&self) -> Vec<String> {
        self.policy().allow
    }

    /// Takes a site off the allow list for good: the next visit asks again. `Ok(false)` when it was not on the list.
    pub fn revoke_site(&self, site: &str) -> Result<bool> {
        let mut updated = self.policy();
        if !updated.remove_allowed_site(site) {
            return Ok(false);
        }
        self.set_policy(updated)?;
        lock(&self.session_sites).remove(&site.trim().to_ascii_lowercase());
        Ok(true)
    }

    /// Validate, persist and apply a new policy. Launch settings (browser, profile, headless,
    /// idle timeout) restart the managed browser on its next use.
    pub fn set_policy(&self, new: BrowserPolicyConfig) -> Result<()> {
        new.validate()?;
        new.save(&self.dirs.policy_file)?;
        let relaunch = {
            let old = self.policy();
            old.browser != new.browser
                || old.profile != new.profile
                || old.headless != new.headless
                || old.idle_shutdown_minutes != new.idle_shutdown_minutes
        };
        if relaunch {
            let cfg = managed_browser_config(&self.workspace_root, &self.dirs, &new);
            lock(&self.managed).set_config(cfg)?;
            *lock(&self.state) = PageState::default();
        }
        *self.policy.write().unwrap_or_else(PoisonError::into_inner) = new;
        Ok(())
    }

    /// The browser a task named ("in Chrome go to Facebook"). The managed browser drives one browser at a time (Edge or Chrome, each with its own Aqua-only
    /// profile), so asking for the other one switches it: the choice is saved in the browser policy and the browser starts again with the new one on its next use.
    /// `Ok(true)` when it was switched, `Ok(false)` when it already was that browser or when the person's own Chrome is what is driven (`extension`), `Err` when
    /// that browser is not installed (nothing is switched and nothing else is used in its place).
    pub fn use_channel(&self, channel: BrowserChannel) -> Result<bool> {
        self.use_channel_with(channel, browser_installed)
    }

    fn use_channel_with(&self, channel: BrowserChannel, installed: impl Fn(BrowserChannel) -> bool) -> Result<bool> {
        if self.kind() != BackendKind::Managed {
            return Ok(false);
        }
        let mut policy = self.policy();
        if policy.browser == channel {
            return Ok(false);
        }
        anyhow::ensure!(installed(channel), "{} is not installed on this computer", channel.label());
        let _ = std::fs::create_dir_all(self.dirs.profile_for(channel));
        policy.browser = channel;
        self.set_policy(policy)?;
        info!("The browser is now {} because the task named it", channel.label());
        Ok(true)
    }

    pub fn managed_installed(&self) -> bool {
        managed_browser_installed(&self.workspace_root)
    }

    fn client(&self, kind: BackendKind) -> &Arc<Mutex<StdioMcpClient>> {
        match kind {
            BackendKind::Managed => &self.managed,
            BackendKind::Extension => &self.extension,
        }
    }

    pub fn status(&self) -> BrowserStatus {
        let kind = self.kind();
        let policy = self.policy();
        let state = lock(&self.state).clone();
        // Never block status on a long tool call.
        let (running, initialized, tool_count, hint, tool_lock) = match self.client(kind).try_lock() {
            Ok(c) => {
                let s = c.status();
                (s.process_running, s.initialized, s.tool_count, s.hint, s.tool_lock)
            }
            Err(_) => (true, true, 0, "Busy with a browser action.".into(), Default::default()),
        };
        BrowserStatus {
            backend: kind,
            installed: kind == BackendKind::Extension || self.managed_installed(),
            running,
            initialized,
            tool_count,
            tabs: state.tabs,
            url: state.url,
            browser: policy.browser.as_arg().into(),
            profile: policy.profile,
            headless: policy.headless,
            pending_confirmations: self.broker.pending().len(),
            hint,
            tool_lock,
        }
    }

    /// Start the active backend's MCP server (the managed browser window opens on first use).
    pub fn start(&self) -> Result<BrowserStatus> {
        let kind = self.kind();
        if kind == BackendKind::Managed && !self.managed_installed() {
            anyhow::bail!("Managed browser is not installed. Run scripts/setup-managed-browser.ps1");
        }
        {
            let mut c = lock(self.client(kind));
            if !c.is_initialized() {
                c.start()?;
            }
        }
        Ok(self.status())
    }

    pub fn stop(&self) -> BrowserStatus {
        let _ = lock(self.client(self.kind())).shutdown();
        *lock(&self.state) = PageState::default();
        self.status()
    }

    /// Delete the Aqua browser profile (all logins/cookies). Managed backend only.
    pub fn reset_profile(&self) -> Result<()> {
        let _ = lock(&self.managed).shutdown();
        if self.dirs.profile.exists() {
            std::fs::remove_dir_all(&self.dirs.profile)?;
        }
        std::fs::create_dir_all(&self.dirs.profile)?;
        *lock(&self.state) = PageState::default();
        lock(&self.session_sites).clear();
        Ok(())
    }

    /// Kill browsers left running on the Aqua profile by a previous crash (managed only).
    pub fn sweep_stale_browsers(&self) {
        #[cfg(windows)]
        {
            let profile = self.dirs.profile.display().to_string();
            if profile.contains(['\'', '"', '`', '*', '?', '[', ']']) {
                return;
            }
            let ps = format!(
                "Get-CimInstance Win32_Process -Filter \"Name='msedge.exe' OR Name='chrome.exe'\" | \
                 Where-Object {{ $_.CommandLine -like '*{profile}*' }} | \
                 ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}"
            );
            let _ = std::process::Command::new("powershell")
                .args(["-NoProfile", "-NonInteractive", "-Command", &ps])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }

    /// Prepare the active backend for an agent run.
    pub fn ensure_ready(&self) -> BrowserReadiness {
        let kind = self.kind();
        match kind {
            BackendKind::Managed => {
                if !self.managed_installed() {
                    return BrowserReadiness {
                        backend: kind,
                        ready: false,
                        detail: "Managed browser is not installed.".into(),
                        user_action_required: Some("Run scripts/setup-managed-browser.ps1".into()),
                        extension_health: None,
                    };
                }
                match self.start() {
                    Ok(s) => BrowserReadiness {
                        backend: kind,
                        ready: true,
                        detail: format!(
                            "Managed {} browser ready ({} profile, {} tools)",
                            s.browser,
                            match s.profile {
                                ProfileMode::Persistent => "Aqua",
                                ProfileMode::Ephemeral => "temporary",
                            },
                            // What the agent can use, not everything the server offers.
                            self.tools().map(|t| t.len()).unwrap_or(s.tool_count)
                        ),
                        user_action_required: None,
                        extension_health: None,
                    },
                    Err(e) => BrowserReadiness {
                        backend: kind,
                        ready: false,
                        detail: format!("Managed browser failed to start: {e:#}"),
                        user_action_required: Some(
                            "Check Node.js is installed, then run scripts/setup-managed-browser.ps1".into(),
                        ),
                        extension_health: None,
                    },
                }
            }
            BackendKind::Extension => {
                let health = {
                    let mut c = lock(&self.extension);
                    ensure_browser_mcp_ready(
                        &mut c,
                        EnsureBrowserMcpOptions {
                            auto_setup: true,
                            auto_start: true,
                            poll_connect_secs: 10,
                            open_extension_docs: true,
                            on_progress: None,
                        },
                    )
                };
                match health {
                    Ok(h) => BrowserReadiness {
                        backend: kind,
                        ready: h.ready,
                        detail: h.detail.clone(),
                        user_action_required: h.user_action_required.clone(),
                        extension_health: Some(h),
                    },
                    Err(e) => BrowserReadiness {
                        backend: kind,
                        ready: false,
                        detail: format!("Browser MCP failed: {e:#}"),
                        user_action_required: Some("Run scripts/setup-browser-mcp.ps1".into()),
                        extension_health: None,
                    },
                }
            }
        }
    }

    /// Tools the agent may use on the active backend (tagged `source: browser`).
    pub fn tools(&self) -> Result<Vec<McpTool>> {
        let kind = self.kind();
        let allowed: &[&str] = match kind {
            BackendKind::Managed => MANAGED_TOOLS,
            BackendKind::Extension => EXTENSION_TOOLS,
        };
        let mut tools: Vec<McpTool> = {
            let mut c = lock(self.client(kind));
            if !c.is_initialized() {
                return Ok(Vec::new());
            }
            let list = if c.cached_tools().is_empty() {
                c.list_tools()?
            } else {
                c.cached_tools().to_vec()
            };
            list.into_iter().filter(|t| allowed.contains(&t.name.as_str())).collect()
        };
        if kind == BackendKind::Managed {
            tools.extend(aqua_tool_definitions());
        }
        for t in &mut tools {
            t.source = Some("browser".into());
        }
        Ok(tools)
    }

    /// Run one browser tool under policy. Never panics; failures come back as observations.
    pub fn call(&self, tool: &str, args: Value) -> BrowserObservation {
        let started = Instant::now();
        let kind = self.kind();
        let url_before = lock(&self.state).url.clone();
        let mut ctx = CallCtx::default();
        let mut obs = self.call_inner(kind, tool, args.clone(), &mut ctx);
        obs.duration_ms = started.elapsed().as_millis() as u64;
        let decision = if obs.error_code() == Some(BrowserErrorCode::ConfirmationDenied) {
            "denied"
        } else if matches!(
            obs.error_code(),
            Some(
                BrowserErrorCode::PolicyBlockedOrigin
                    | BrowserErrorCode::PolicyToolBlocked
                    | BrowserErrorCode::PolicyJsDisabled
                    | BrowserErrorCode::PolicyUploadPath
                    | BrowserErrorCode::TabLimit
            )
        ) {
            "blocked"
        } else if ctx.confirmed {
            "confirmed"
        } else {
            "allowed"
        };
        self.audit.append(&AuditEntry {
            ts_unix_ms: now_ms(),
            backend: kind.as_str(),
            tool,
            args: redact_args(tool, &args, ctx.sensitive),
            url_before: url_before.as_deref(),
            url_after: obs.url.as_deref(),
            decision,
            ok: obs.ok,
            error_code: obs.error_code().map(|c| c.as_str()),
            duration_ms: obs.duration_ms,
        });
        obs
    }

    fn call_inner(&self, kind: BackendKind, tool: &str, args: Value, ctx: &mut CallCtx) -> BrowserObservation {
        let backend = kind.as_str();
        let fail = |code, msg: String| BrowserObservation::failure(tool, backend, code, msg);

        let allowed = match kind {
            BackendKind::Managed => MANAGED_TOOLS.contains(&tool) || AQUA_TOOLS.contains(&tool),
            BackendKind::Extension => EXTENSION_TOOLS.contains(&tool),
        };
        if !allowed {
            return fail(
                BrowserErrorCode::PolicyToolBlocked,
                format!("{tool} is not available to Aqua's agent on the {backend} browser"),
            );
        }
        let policy = self.policy();
        let mut args = sanitize_args(tool, args);
        if let Err(msg) = validate_args(tool, &args) {
            return fail(BrowserErrorCode::InvalidArguments, msg);
        }

        match tool {
            "aqua_browser_session" => return self.session_observation(tool, backend),
            "aqua_browser_extract" => return self.extract(kind, &args, &policy),
            _ => {}
        }

        let page_url = lock(&self.state).url.clone();

        // Where is this call trying to go?
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let target_url = match tool {
            "browser_navigate" => args.get("url").and_then(|v| v.as_str()).map(String::from),
            "browser_tabs" if action == "new" => args.get("url").and_then(|v| v.as_str()).map(String::from),
            _ => None,
        };
        if let Some(u) = &target_url {
            if let Err(o) = self.authorize_url(tool, backend, u, &policy, ctx) {
                return o;
            }
        }
        if tool == "browser_tabs" && action == "new" && lock(&self.state).tabs >= policy.max_tabs as usize {
            return fail(
                BrowserErrorCode::TabLimit,
                format!("{} tabs are open (limit {}); close one first", policy.max_tabs, policy.max_tabs),
            );
        }
        if tool == "browser_evaluate" && !policy.javascript_allowed(page_url.as_deref()) {
            return fail(
                BrowserErrorCode::PolicyJsDisabled,
                "JavaScript is disabled for this site by the browser policy; use aqua_browser_extract instead".into(),
            );
        }
        if tool == "browser_file_upload" {
            let paths: Vec<String> = args["paths"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            match resolve_upload_paths(&self.dirs.uploads, &paths) {
                Ok(real) => {
                    args["paths"] = json!(real.iter().map(|p| p.display().to_string()).collect::<Vec<_>>());
                }
                Err(msg) => return fail(BrowserErrorCode::PolicyUploadPath, msg),
            }
        }
        if let Some(reason) = self.confirmation_reason(tool, &args, &policy, &mut ctx.sensitive) {
            let site = page_url.as_deref().and_then(host_of);
            // The question shows what the host's questions show (card R6-08, T6-08.8): what the call can do, what could go wrong, and the rule that made it ask.
            let (classes, why) = action_question_why(tool, &args, &policy, page_url.as_deref());
            let (req, decision) = self.broker.request_explained(
                ConfirmKind::Action,
                tool,
                reason.clone(),
                page_url.clone(),
                site,
                false,
                classes,
                why,
                Duration::from_secs(policy.confirm_timeout_secs),
            );
            if decision == Decision::Deny {
                let mut o = fail(
                    BrowserErrorCode::ConfirmationDenied,
                    format!("The user did not approve: {reason}. Do not retry this action; tell the user."),
                );
                o.confirmation = Some(Confirmation {
                    request_id: req.request_id,
                    reason,
                });
                return o;
            }
            ctx.confirmed = true;
        }

        // Nobody is waiting for this call any more (the host timed out or the run was cancelled): do not act now.
        if super::confirm::is_abandoned() {
            return fail(BrowserErrorCode::ConfirmationDenied, "The request was cancelled or timed out before it ran. Nothing was done; do not retry it unless asked.".to_string());
        }
        let mut obs = self.dispatch(kind, tool, &args, &policy);
        if !obs.ok
            && is_idempotent(tool)
            && matches!(
                obs.error_code(),
                Some(BrowserErrorCode::BrowserCrashed | BrowserErrorCode::BrowserNotRunning)
            )
        {
            warn!("{tool}: browser unavailable ({:?}), restarting once", obs.error_code());
            obs = self.dispatch(kind, tool, &args, &policy);
        }
        if obs.error_code() == Some(BrowserErrorCode::StaleRef) {
            let fresh = self.dispatch(kind, "browser_snapshot", &json!({}), &policy);
            if fresh.ok {
                obs.text = format!("Fresh snapshot (use its refs):\n{}", fresh.text);
                obs.truncated = fresh.truncated;
            }
        }
        // A target that is not a ref at all (the model made up a name such as `post_text_area`): say so and give it the page's refs, as for a stale one.
        if obs.error_code() == Some(BrowserErrorCode::ElementNotFound) {
            if let Some(name) = args.get("target").and_then(|v| v.as_str()).filter(|t| !is_snapshot_ref(t)) {
                let fresh = self.dispatch(kind, "browser_snapshot", &json!({}), &policy);
                if fresh.ok {
                    obs.text = format!("\"{name}\" is not a ref: a target is a ref from the page snapshot (like e55), never a name.\nFresh snapshot (use its refs):\n{}", fresh.text);
                    obs.truncated = fresh.truncated;
                }
            }
        }

        // Clicks, redirects and scripts can land somewhere the policy never approved.
        if let Some(landed) = obs.url.clone() {
            if landed != page_url.clone().unwrap_or_default() || target_url.is_some() {
                if let Err(reason) = self.authorize_landing(tool, backend, &landed, &policy, ctx) {
                    self.go_back(kind, &policy);
                    let mut o = fail(
                        BrowserErrorCode::PolicyBlockedOrigin,
                        format!("The page went to {landed}, which is not allowed ({reason}). Aqua went back."),
                    );
                    o.url = lock(&self.state).url.clone();
                    return o;
                }
            }
        }
        if tool == "browser_close" {
            *lock(&self.state) = PageState::default();
        }
        obs
    }

    /// Check an address the model wants opened in the person's own browser, with the same rules and the same
    /// question as `browser_navigate`: denied sites stay denied and an unknown site asks first.
    pub fn authorize_open_url(&self, url: &str) -> std::result::Result<(), String> {
        let policy = self.policy();
        let mut ctx = CallCtx::default();
        self.authorize_url("local_open_url", "local", url, &policy, &mut ctx)
            .map_err(|o| o.error.map(|e| e.message).unwrap_or_else(|| "this address is not allowed".to_string()))
    }

    fn authorize_url(
        &self,
        tool: &str,
        backend: &str,
        url: &str,
        policy: &BrowserPolicyConfig,
        ctx: &mut CallCtx,
    ) -> std::result::Result<(), BrowserObservation> {
        match self.decide_url(policy, url) {
            UrlDecision::Allow => Ok(()),
            UrlDecision::Deny(reason) => Err(BrowserObservation::failure(
                tool,
                backend,
                BrowserErrorCode::PolicyBlockedOrigin,
                format!("Blocked by browser policy: {reason}"),
            )),
            UrlDecision::Ask { site, origin } => {
                if lock(&self.session_sites).contains(&site) {
                    return Ok(());
                }
                let (classes, why) = site_question_why(&origin);
                let (req, decision) = self.broker.request_explained(
                    ConfirmKind::Site,
                    tool,
                    format!("Open {origin}? It isn't on your allowed sites list."),
                    Some(url.to_string()),
                    Some(site.clone()),
                    false,
                    classes,
                    why,
                    Duration::from_secs(policy.confirm_timeout_secs),
                );
                match decision {
                    Decision::Deny => {
                        let mut o = BrowserObservation::failure(
                            tool,
                            backend,
                            BrowserErrorCode::ConfirmationDenied,
                            format!("The user did not allow opening {origin}. Pick an allowed site or tell the user."),
                        );
                        o.confirmation = Some(Confirmation {
                            request_id: req.request_id,
                            reason: req.reason,
                        });
                        Err(o)
                    }
                    Decision::Allow => {
                        lock(&self.session_sites).insert(site);
                        ctx.confirmed = true;
                        Ok(())
                    }
                    Decision::AllowAlways => {
                        lock(&self.session_sites).insert(site.clone());
                        let mut updated = self.policy();
                        if updated.add_allowed_site(&site) {
                            if let Err(e) = updated
                                .save(&self.dirs.policy_file)
                                .map(|_| *self.policy.write().unwrap_or_else(PoisonError::into_inner) = updated)
                            {
                                warn!("could not save allowed site {site}: {e:#}");
                            }
                        }
                        ctx.confirmed = true;
                        Ok(())
                    }
                }
            }
        }
    }

    /// Post-action check for where the page actually is. Unknown sites in ask mode are
    /// confirmed after the fact; denied ones make the caller go back.
    fn authorize_landing(
        &self,
        tool: &str,
        backend: &str,
        url: &str,
        policy: &BrowserPolicyConfig,
        ctx: &mut CallCtx,
    ) -> std::result::Result<(), String> {
        if url.starts_with("chrome-error://") || url.starts_with("edge-error://") {
            return Ok(());
        }
        match self.decide_url(policy, url) {
            UrlDecision::Allow => Ok(()),
            UrlDecision::Deny(reason) => Err(reason),
            UrlDecision::Ask { .. } => self
                .authorize_url(tool, backend, url, policy, ctx)
                .map_err(|o| o.error.map(|e| e.message).unwrap_or_default()),
        }
    }

    fn go_back(&self, kind: BackendKind, policy: &BrowserPolicyConfig) {
        let tool = match kind {
            BackendKind::Managed => "browser_navigate_back",
            BackendKind::Extension => "browser_go_back",
        };
        let back = self.dispatch(kind, tool, &json!({}), policy);
        let still_bad = back
            .url
            .as_deref()
            .is_some_and(|u| matches!(self.decide_url(policy, u), UrlDecision::Deny(_)));
        if !back.ok || still_bad {
            let _ = self.dispatch(kind, "browser_navigate", &json!({"url": "about:blank"}), policy);
        }
    }

    /// Why this call needs the user's approval, if it does.
    fn confirmation_reason(
        &self,
        tool: &str,
        args: &Value,
        policy: &BrowserPolicyConfig,
        sensitive: &mut bool,
    ) -> Option<String> {
        let state = lock(&self.state).clone();
        let on_site = state
            .url
            .as_deref()
            .and_then(host_of)
            .map(|h| format!(" on {h}"))
            .unwrap_or_default();
        let label = |target: Option<&str>| -> String {
            target
                .and_then(|t| label_for_target(&state.snapshot, t))
                .or_else(|| args.get("element").and_then(|v| v.as_str()).map(String::from))
                .unwrap_or_default()
        };
        let target = args.get("target").and_then(|v| v.as_str());
        match tool {
            // Uploads always need approval, whatever the policy says.
            "browser_file_upload" => {
                let names: Vec<String> = args["paths"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .filter_map(|p| Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(format!("Upload {}{on_site}", names.join(", ")))
            }
            _ if !policy.confirm_irreversible => {
                *sensitive = is_sensitive_tool_call(tool, args, &state.snapshot);
                None
            }
            "browser_click" => {
                let l = label(target);
                let role = l.split_whitespace().next().unwrap_or("");
                let actionable = !matches!(role, "checkbox" | "radio" | "textbox" | "combobox" | "option" | "tab");
                (actionable && is_irreversible_label(&l)).then(|| format!("Click {l}{on_site}"))
            }
            "browser_press_key" => {
                let key = args["key"].as_str().unwrap_or("");
                is_submit_shortcut(key).then(|| format!("Press {key}{on_site} (usually sends or submits)"))
            }
            "browser_type" => {
                let l = label(target);
                if is_sensitive_field_label(&l) {
                    *sensitive = true;
                    Some(format!("Type into {l}{on_site}"))
                } else {
                    None
                }
            }
            "browser_fill_form" => {
                let fields = args["fields"].as_array().cloned().unwrap_or_default();
                let flagged: Vec<String> = fields
                    .iter()
                    .filter_map(|f| {
                        let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let l = f
                            .get("target")
                            .and_then(|v| v.as_str())
                            .and_then(|t| label_for_target(&state.snapshot, t))
                            .unwrap_or_default();
                        (is_sensitive_field_label(name) || is_sensitive_field_label(&l)).then(|| name.to_string())
                    })
                    .collect();
                if flagged.is_empty() {
                    None
                } else {
                    *sensitive = true;
                    Some(format!("Fill {}{on_site}", flagged.join(", ")))
                }
            }
            _ => None,
        }
    }

    /// Call the MCP server and normalize the answer; updates page state.
    fn dispatch(&self, kind: BackendKind, tool: &str, args: &Value, policy: &BrowserPolicyConfig) -> BrowserObservation {
        let backend = kind.as_str();
        let outcome = {
            let mut c = lock(self.client(kind));
            if !c.is_initialized() {
                if kind == BackendKind::Managed && !self.managed_installed() {
                    return BrowserObservation::failure(
                        tool,
                        backend,
                        BrowserErrorCode::BrowserNotRunning,
                        "Managed browser is not installed. Run scripts/setup-managed-browser.ps1",
                    );
                }
                if let Err(e) = c.start() {
                    return BrowserObservation::failure(
                        tool,
                        backend,
                        BrowserErrorCode::BrowserNotRunning,
                        format!("Could not start the {backend} browser: {e:#}"),
                    );
                }
            }
            let r = c.call_tool(tool, args.clone());
            if let Err(e) = &r {
                if classify_error(&format!("{e:#}")) == BrowserErrorCode::BrowserCrashed {
                    let _ = c.shutdown();
                }
            }
            r
        };
        let obs = match outcome {
            Ok(result) => self.observation_from_text(tool, backend, &result_text(&result), true, policy),
            Err(e) => self.observation_from_text(tool, backend, &format!("{e:#}"), false, policy),
        };
        self.remember(&obs);
        obs
    }

    fn observation_from_text(
        &self,
        tool: &str,
        backend: &str,
        text: &str,
        ok: bool,
        policy: &BrowserPolicyConfig,
    ) -> BrowserObservation {
        let s = parse_sections(text);
        let budget = policy.snapshot_budget_chars;
        let snapshot_full = s
            .snapshot_inline
            .clone()
            .or_else(|| s.snapshot_file.as_deref().and_then(|p| self.read_artifact(p)));
        let mut obs = BrowserObservation {
            ok,
            tool: tool.into(),
            backend: backend.into(),
            url: s.url.clone().or_else(|| scan_page_url(&s)),
            title: s.title.clone(),
            text: String::new(),
            data: None,
            truncated: false,
            artifacts: Vec::new(),
            confirmation: None,
            duration_ms: 0,
            error: None,
        };
        if !ok || s.error.is_some() {
            let msg = s.error.clone().unwrap_or_else(|| text.trim().to_string());
            let code = classify_error(&format!("{msg}\n{}", s.modal.clone().unwrap_or_default()));
            obs.ok = false;
            obs.error = Some(super::observation::BrowserError {
                code,
                retryable: code.retryable(),
                message: first_line(&msg),
            });
            if let Some(m) = &s.modal {
                obs.text = format!("Dialog open: {m}");
            }
            return obs;
        }
        let mut parts: Vec<String> = Vec::new();
        if let Some(r) = &s.result {
            if tool == "browser_evaluate" {
                obs.data = serde_json::from_str(r).ok();
            }
            if s.screenshot_file.is_none() {
                let (r, cut) = cap_text(r, budget / 2);
                obs.truncated |= cut;
                parts.push(r);
            }
        }
        if let Some(path) = &s.screenshot_file {
            obs.artifacts.push(Artifact {
                kind: "screenshot".into(),
                path: self.display_path(path),
            });
        }
        if let Some(m) = &s.modal {
            parts.push(format!("Dialog open: {m}"));
        }
        if let Some(snap) = &snapshot_full {
            let used: usize = parts.iter().map(|p| p.chars().count()).sum();
            let (compact, cut) = compact_snapshot(snap, budget.saturating_sub(used).max(1000));
            obs.truncated |= cut;
            parts.push(compact);
        }
        if parts.is_empty() && s.url.is_none() && s.result.is_none() && !text.trim().is_empty() && !text.contains("### ") {
            // Extension backend / unknown format: pass the text through, capped. Keep refs so
            // confirmations can still label targets (e.g. button "Send").
            if ok && text.contains("[ref=") {
                lock(&self.state).snapshot = text.to_string();
            }
            let (t, cut) = cap_text(text.trim(), budget);
            obs.truncated |= cut;
            parts.push(t);
        }
        obs.text = parts.join("\n").trim().to_string();
        if let (Some(snap), true) = (snapshot_full, ok) {
            lock(&self.state).snapshot = snap;
        }
        if let Some(n) = s.open_tabs {
            lock(&self.state).tabs = n;
        }
        obs
    }

    fn remember(&self, obs: &BrowserObservation) {
        let mut st = lock(&self.state);
        if let Some(u) = &obs.url {
            st.url = Some(u.clone());
            if st.tabs == 0 {
                st.tabs = 1;
            }
        }
    }

    /// Read a snapshot file only if it lives in Aqua's artifact directory.
    fn read_artifact(&self, path: &str) -> Option<String> {
        let p = Path::new(path);
        let p = if p.is_absolute() { p.to_path_buf() } else { self.dirs.uploads.join(p) };
        let real = p.canonicalize().ok()?;
        let root = self.dirs.artifacts.canonicalize().ok()?;
        if !real.starts_with(&root) {
            warn!("ignoring snapshot outside the artifact folder: {}", real.display());
            return None;
        }
        std::fs::read_to_string(real).ok()
    }

    fn display_path(&self, path: &str) -> String {
        Path::new(path)
            .strip_prefix(&self.workspace_root)
            .map(|p| p.display().to_string().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string())
    }

    fn session_observation(&self, tool: &str, backend: &str) -> BrowserObservation {
        let s = self.status();
        let data = serde_json::to_value(&s).unwrap_or(Value::Null);
        BrowserObservation {
            ok: true,
            tool: tool.into(),
            backend: backend.into(),
            url: s.url.clone(),
            title: None,
            text: format!(
                "Browser: {} ({}), running={}, tabs={}, profile={:?}",
                s.backend.as_str(),
                s.browser,
                s.running,
                s.tabs,
                s.profile
            ),
            data: Some(data),
            truncated: false,
            artifacts: Vec::new(),
            confirmation: None,
            duration_ms: 0,
            error: None,
        }
    }

    fn extract(&self, kind: BackendKind, args: &Value, policy: &BrowserPolicyConfig) -> BrowserObservation {
        let tool = "aqua_browser_extract";
        let backend = kind.as_str();
        let Some(ek) = args.get("kind").and_then(|v| v.as_str()).and_then(ExtractKind::parse) else {
            return BrowserObservation::failure(
                tool,
                backend,
                BrowserErrorCode::InvalidArguments,
                "kind must be headlines, links, tables, article, metadata or search_results",
            );
        };
        let max = args.get("max_items").and_then(|v| v.as_u64()).unwrap_or(10) as u32;
        let mut eval_args = json!({ "function": extract::script(ek, max) });
        if let Some(t) = args.get("target").and_then(|v| v.as_str()) {
            eval_args["target"] = json!(t);
        }
        // Built-in audited script: deliberately not subject to the model-facing JS gate.
        let mut obs = self.dispatch(kind, "browser_evaluate", &eval_args, policy);
        obs.tool = tool.into();
        if obs.ok {
            match obs.data.clone() {
                Some(data) => {
                    let (text, cut) = cap_text(&extract::render(ek, &data), policy.snapshot_budget_chars);
                    obs.text = text;
                    obs.truncated = cut;
                }
                None => {
                    obs.ok = false;
                    obs.error = Some(super::observation::BrowserError {
                        code: BrowserErrorCode::BackendError,
                        retryable: false,
                        message: "extractor returned no data".into(),
                    });
                }
            }
        }
        obs
    }
}

#[derive(Default)]
struct CallCtx {
    confirmed: bool,
    sensitive: bool,
}

fn is_sensitive_tool_call(tool: &str, args: &Value, snapshot: &str) -> bool {
    match tool {
        "browser_type" => args
            .get("target")
            .and_then(|v| v.as_str())
            .and_then(|t| label_for_target(snapshot, t))
            .is_some_and(|l| is_sensitive_field_label(&l)),
        "browser_fill_form" => args["fields"].as_array().is_some_and(|f| {
            f.iter().any(|x| x.get("name").and_then(|v| v.as_str()).is_some_and(is_sensitive_field_label))
        }),
        _ => false,
    }
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(String::from))
}

/// What the question about an action of the page shows besides the action (card R6-08, T6-08.8), as the host's questions show it: the classes of the call (from the model's name
/// for the tool), what could go wrong, the rule that made it ask, and the page it is about. The backend does not know what the run has read or holds, so it says nothing of
/// that; the question the host puts for the same call says it.
pub(super) fn action_question_why(tool: &str, args: &Value, policy: &BrowserPolicyConfig, page: Option<&str>) -> (Vec<String>, super::confirm::ConfirmWhy) {
    use aqua_tool_host::PermClass;
    let classes = super::provider::BrowserProvider::classes_of_wire_call(tool, args, policy, crate::full_access::is_on()).unwrap_or_else(|| vec![PermClass::Network, PermClass::Write]);
    let target = page.map(super::audit::reduce_url).unwrap_or_else(|| "the page that is open".to_string());
    let rule = "Aqua asks before an action that cannot be taken back or that handles something sensitive: a send, a payment, a delete, a password or card field, an upload.";
    (super::confirm::class_words(&classes), super::confirm::explained(&classes, &target, rule, false, false))
}

/// The same for the question about opening a site that is not on the list.
pub(super) fn site_question_why(origin: &str) -> (Vec<String>, super::confirm::ConfirmWhy) {
    let classes = [aqua_tool_host::PermClass::Network];
    (super::confirm::class_words(&classes), super::confirm::explained(&classes, origin, "This site is not on your list of allowed sites.", false, false))
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with("Call log"))
        .unwrap_or("")
        .trim_start_matches("Error: ")
        .to_string()
}

/// Older Browser MCP output has `- Page URL:` lines without a `### Page` header.
fn scan_page_url(s: &PlaywrightSections) -> Option<String> {
    s.raw
        .lines()
        .find_map(|l| l.trim().strip_prefix("- Page URL:").map(|u| u.trim().to_string()))
}

fn aqua_tool_definitions() -> Vec<McpTool> {
    vec![
        McpTool {
            name: "aqua_browser_extract".into(),
            description: "Get structured data from the current page. kind: headlines | links | tables | article | metadata | search_results. Prefer this over reading snapshots for lists and news.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["headlines", "links", "tables", "article", "metadata", "search_results"]},
                    "max_items": {"type": "integer", "minimum": 1, "maximum": 50},
                    "target": {"type": "string", "description": "Optional element ref to limit extraction to"}
                },
                "required": ["kind"]
            }),
            source: Some("browser".into()),
        },
        McpTool {
            name: "aqua_browser_session".into(),
            description: "Report the browser's state: backend, current URL, open tabs.".into(),
            input_schema: json!({"type": "object", "properties": {}}),
            source: Some("browser".into()),
        },
    ]
}

#[cfg(test)]
mod question_tests {
    use super::*;

    /// Card R6-08, T6-08.8: the browser's own questions carry what the host's carry: what the call can do, what could go wrong, which rule asked, and what is involved.
    #[test]
    fn the_browsers_own_questions_say_what_could_go_wrong_which_rule_asked_and_what_is_involved() {
        let policy = BrowserPolicyConfig::default();
        // A click on Send: the page is named by its origin and path, never by a query or a fragment that can carry a token.
        let (classes, why) = action_question_why("browser_click", &json!({"target": "e5"}), &policy, Some("https://mail.example.org/inbox?token=secret123#frag"));
        assert!(classes.contains(&"WRITE".to_string()) && classes.contains(&"NETWORK".to_string()), "{classes:?}");
        assert!(why.risk.starts_with("This changes a file or a setting"), "{}", why.risk);
        assert!(why.rule.contains("cannot be taken back"), "{}", why.rule);
        assert_eq!(why.data, vec!["Target: https://mail.example.org/inbox".to_string()]);
        // An upload is a change on another system, and with no page named the question says so.
        let (classes, why) = action_question_why("browser_file_upload", &json!({"paths": ["a.txt"]}), &policy, None);
        assert!(classes.contains(&"EXTERNAL_EFFECT".to_string()), "{classes:?}");
        assert!(why.risk.contains("another system"), "{}", why.risk);
        assert_eq!(why.data, vec!["Target: the page that is open".to_string()]);
        // A name that is not a browser tool is taken for the common case, not left blank.
        let (classes, why) = action_question_why("browser_nothing", &json!({}), &policy, None);
        assert_eq!(classes, vec!["NETWORK".to_string(), "WRITE".to_string()]);
        assert!(!why.risk.is_empty() && !why.rule.is_empty());
        // A site that is not on the list.
        let (classes, why) = site_question_why("https://collector.example.net");
        assert_eq!(classes, vec!["NETWORK".to_string()]);
        assert_eq!((why.risk.as_str(), why.rule.as_str()), ("This sends a request over the internet.", "This site is not on your list of allowed sites."));
        assert_eq!(why.data, vec!["Target: https://collector.example.net".to_string()]);
    }
}

#[cfg(test)]
mod channel_tests {
    use super::*;

    fn backend(name: &str) -> (BrowserBackend, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aqua_browser_channel_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dirs = BrowserDirs::under(dir.clone());
        dirs.create_all().unwrap();
        BrowserPolicyConfig::default().save(&dirs.policy_file).unwrap();
        let b = BrowserBackend::with_dirs(&dir, dirs, StdioMcpClient::new(crate::default_browser_config()));
        b.set_kind(BackendKind::Managed);
        (b, dir)
    }

    #[test]
    fn a_task_that_names_the_other_browser_switches_the_managed_browser_and_the_choice_is_kept() {
        let (b, dir) = backend("switch");
        assert_eq!(b.policy().browser, BrowserChannel::Msedge);
        // Already that browser: nothing changes.
        assert!(!b.use_channel_with(BrowserChannel::Msedge, |_| true).unwrap());
        // Chrome is named: the managed browser is Chrome from now on, in a profile folder of its own, and the policy file says so.
        assert!(b.use_channel_with(BrowserChannel::Chrome, |_| true).unwrap());
        assert_eq!(b.policy().browser, BrowserChannel::Chrome);
        assert!(b.dirs().profile_for(BrowserChannel::Chrome).is_dir());
        let saved = BrowserPolicyConfig::load_or_create(&b.dirs().policy_file).unwrap();
        assert_eq!(saved.browser, BrowserChannel::Chrome);
        // And back.
        assert!(b.use_channel_with(BrowserChannel::Msedge, |_| true).unwrap());
        assert_eq!(b.policy().browser, BrowserChannel::Msedge);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_browser_that_is_not_installed_is_an_error_and_nothing_else_is_used_in_its_place() {
        let (b, dir) = backend("missing");
        let e = b.use_channel_with(BrowserChannel::Chrome, |c| c == BrowserChannel::Msedge).unwrap_err().to_string();
        assert!(e.contains("Google Chrome is not installed"), "{e}");
        assert_eq!(b.policy().browser, BrowserChannel::Msedge, "the policy was not touched");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn when_the_persons_own_chrome_is_driven_there_is_nothing_to_switch() {
        let (b, dir) = backend("extension");
        b.set_kind(BackendKind::Extension);
        assert!(!b.use_channel_with(BrowserChannel::Chrome, |_| false).unwrap());
        assert_eq!(b.policy().browser, BrowserChannel::Msedge);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod resolving_tests {
    //! Audit item SE-09 at the place where a page is opened: the backend looks the name up before it asks the browser to go there, and again after the page has loaded.
    use super::*;
    use std::collections::VecDeque;
    use std::net::IpAddr;

    /// A name that answers with the next address of its list each time it is asked, and with the last one for ever after.
    struct Answers(Mutex<VecDeque<&'static str>>);

    impl Resolver for Answers {
        fn lookup(&self, _host: &str, _port: u16) -> std::io::Result<Vec<IpAddr>> {
            let mut q = self.0.lock().unwrap();
            let ip = if q.len() > 1 { q.pop_front().unwrap() } else { q[0] };
            Ok(vec![ip.parse().unwrap()])
        }
    }

    fn answers(list: &[&'static str]) -> Arc<dyn Resolver> {
        Arc::new(Answers(Mutex::new(list.iter().copied().collect())))
    }

    fn backend(name: &str, edit: impl FnOnce(&mut BrowserPolicyConfig)) -> (BrowserBackend, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aqua_browser_resolve_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let dirs = BrowserDirs::under(dir.clone());
        dirs.create_all().unwrap();
        let mut policy = BrowserPolicyConfig::default();
        edit(&mut policy);
        policy.save(&dirs.policy_file).unwrap();
        let b = BrowserBackend::with_dirs(&dir, dirs, StdioMcpClient::new(crate::default_browser_config()));
        b.set_kind(BackendKind::Managed);
        (b, dir)
    }

    #[test]
    fn a_public_looking_name_that_points_at_this_computer_is_blocked_before_the_browser_is_asked() {
        let (b, dir) = backend("blocked", |_| {});
        b.set_resolver(answers(&["127.0.0.1"]));
        // Through the tool: no browser is installed or started in this test, so a refusal that comes back is the policy's own, before any dispatch.
        let o = b.call("browser_navigate", json!({"url": "http://app.localtest.me:2375/"}));
        assert_eq!(o.error_code(), Some(BrowserErrorCode::PolicyBlockedOrigin), "{o:?}");
        let said = o.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        assert!(said.contains("Blocked by browser policy") && said.contains("resolves to 127.0.0.1"), "{said}");
        // A new tab and the person's own browser (local_open_url) take the same road.
        let tab = b.call("browser_tabs", json!({"action": "new", "url": "http://127.0.0.1.nip.io:11434/"}));
        assert_eq!(tab.error_code(), Some(BrowserErrorCode::PolicyBlockedOrigin), "{tab:?}");
        let opened = b.authorize_open_url("http://app.localtest.me:2375/").unwrap_err();
        assert!(opened.contains("resolves to"), "{opened}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_name_the_person_says_is_on_their_own_network_is_let_through() {
        let (b, dir) = backend("own_network", |p| {
            p.deny.retain(|r| r != "192.168.0.0/16");
            p.allow.push("nas.lan".into());
            p.allow_private.push("nas.lan".into());
        });
        b.set_resolver(answers(&["192.168.1.50"]));
        assert!(b.authorize_open_url("http://nas.lan/").is_ok());
        // Another name that leads to the same address is not named.
        assert!(b.authorize_open_url("http://other.lan/").is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The page has loaded, and the name now answers with an address inside: the look-up after the action refuses it, and the caller goes back.
    #[test]
    fn a_name_that_answered_public_before_the_page_loaded_and_private_after_is_refused_by_the_check_after_the_action() {
        let (b, dir) = backend("rebinding", |p| p.allow.push("rebind.example.org".into()));
        b.set_resolver(answers(&["93.184.216.34", "127.0.0.1"]));
        let policy = b.policy();
        let mut ctx = CallCtx::default();
        let url = "http://rebind.example.org:2375/";
        assert!(b.authorize_url("browser_navigate", "managed", url, &policy, &mut ctx).is_ok(), "the first answer is public");
        let reason = b.authorize_landing("browser_navigate", "managed", url, &policy, &mut ctx).unwrap_err();
        assert!(reason.contains("resolves to 127.0.0.1"), "{reason}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

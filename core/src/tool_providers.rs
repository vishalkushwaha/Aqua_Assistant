//! The table of tool providers (card OA3-03; task EX-01 of the OpenCode audit).
//!
//! `build_tool_host` used to register about twenty families of tools with one hand-written block each: load the family's config, build the provider, register it, log a line, and
//! (for the file tools) hand the folder guard to the families that work inside the allowed folders. The agent's routing (`agent_tools::PACKS`) kept a second list of the same families,
//! and nothing could say why a family was *not* there. Now a family is one row of [`PROVIDERS`], in the order they were registered before:
//!
//! * `build` makes the provider from [`ProviderCtx`] (the data folder, the host, the browser and desktop clients, and the folder guard the file tools made), or says the family is off and why;
//! * `off_when` is a switch that leaves the family out before its config is even read (`AQUA_CLIPBOARD_TOOLS`, `AQUA_RESEARCH`, `AQUA_SCREEN_TOOLS`);
//! * `needs_files` keeps the families that work inside the allowed folders off while `fs.json` is broken, as they always were;
//! * `pack` is the routing pack of the family (the words of a goal that bring its tools to the agent): [`packs`] is built from the table, in the order the old list had.
//!
//! [`register_all`] registers the table and returns what became of each row (`Registered`, `Off` with the reason, `Failed` with the reason); that is what `GET /api/v1/tools/providers`
//! shows. The log lines are the ones that were written before (`tool host: registered N <family> tools`). Adding a family is one row.

use crate::tool_host::{load_cloud_config, load_db_config, load_docker_config, load_forge_config, load_fs_config, load_git_config, load_http_config, load_iac_config, load_k8s_config, load_mon_config, load_ssh_config, load_term_config, load_term_rules, protected_paths, PROJECT_GUARD, TRASH};
use aqua_mcp_adapters::browser::{BrowserBackend, BrowserProvider};
use aqua_mcp_adapters::{StdioMcpClient, WindowsUiProvider};
use aqua_native_tools::check::CheckProvider;
use aqua_native_tools::code::provider::CodeProvider;
use aqua_native_tools::lsp::{LspConfig, LspProvider};
use aqua_native_tools::cloud::CloudProvider;
use aqua_native_tools::db::DbProvider;
use aqua_native_tools::docker::DockerProvider;
use aqua_native_tools::forge::ForgeProvider;
use aqua_native_tools::fs::{FsProvider, PathGuard, Trash};
use aqua_native_tools::git::checkpoint::{Checkpoints, Effects};
use aqua_native_tools::git::journal::Journal;
use aqua_native_tools::git::GitProvider;
use aqua_native_tools::http::HttpProvider;
use aqua_native_tools::iac::IacProvider;
use aqua_native_tools::k8s::K8sProvider;
use aqua_native_tools::mon::MonProvider;
use aqua_native_tools::secrets::CredentialManager;
use aqua_native_tools::ssh::SshProvider;
use aqua_native_tools::sys::SysProvider;
use aqua_native_tools::term::TermProvider;
use aqua_tool_host::{ToolHost, ToolProvider};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use tracing::{info, warn};

/// The routing pack of a family: the prefix of its tool names and the words of a goal that bring its tools to the agent (see `agent_tools::route`).
#[derive(Clone, Copy, Debug)]
pub struct PackSpec {
    /// Where it sits in the list; a tie in the score goes to the lower number.
    pub order: u8,
    pub name: &'static str,
    pub prefixes: &'static [&'static str],
    /// Whole words (or, when they start with `.`, file extensions) that pull the pack in.
    pub words: &'static [&'static str],
}

/// How many characters of lines of text a page of a file, or the lines of a search, may have: what the model is shown of a result in the window it runs with (card OA5-03, CU-03).
/// The regions of the prompt and the window of the model server are the business of `modules`, which this module may not name (core/tests/layering_test.rs), so the number comes from
/// a function that the program gives it: `core/src/main.rs` installs it at start-up with [`set_view_chars_source`], and a [`ProviderCtx`] can carry its own (`with_view_chars`).
pub type ViewCharsSource = fn() -> usize;

/// The function every build of the file tools asks when its `ProviderCtx` carries none; set once by the program (see [`ViewCharsSource`]).
static VIEW_CHARS_SOURCE: OnceLock<ViewCharsSource> = OnceLock::new();

/// Gives the file tools the function that says how many characters of lines fit what the model is shown (see [`ViewCharsSource`]). Called once, by `main`, before the tool host is
/// built; `false` when one was set already (the first stays). **Until it is set the file tools keep `DEFAULT_VIEW_CHARS`, which is the number for the window of 6,144 tokens.**
pub fn set_view_chars_source(source: ViewCharsSource) -> bool {
    VIEW_CHARS_SOURCE.set(source).is_ok()
}

/// The journal of what commands changed and the checkpoints of a project (card OA7-03), made by the `fs` row with the undo store they write their records to; the first host a process
/// builds sets them, as it sets `TRASH`. The agent loop asks for the checkpoints (`agent_tools`), the routes for both.
static JOURNAL: OnceLock<Arc<Journal>> = OnceLock::new();
static CHECKPOINTS: OnceLock<Arc<Checkpoints>> = OnceLock::new();

/// The checkpoints of a project, `None` until the file tools are built (and for good when they are off).
pub fn checkpoints() -> Option<Arc<Checkpoints>> {
    CHECKPOINTS.get().cloned()
}

/// The journal of what commands changed; `None` until the file tools are built.
pub fn journal() -> Option<Arc<Journal>> {
    JOURNAL.get().cloned()
}

/// What a build is given.
pub struct ProviderCtx {
    pub data_dir: PathBuf,
    pub host: Arc<ToolHost>,
    pub browser: Arc<BrowserBackend>,
    pub windows: Arc<Mutex<StdioMcpClient>>,
    /// The file guard, set by the `fs` row for the families that work inside the allowed folders.
    guard: Mutex<Option<Arc<PathGuard>>>,
    /// The terminal tools, set by the `term` row for the `check` row (card OA4-05) and for the command tools (card OA11-02): `check.run` and a declared command run through the same provider, so each is asked about, timed, ruled on and kept as a job as a `term.run` is.
    term: Mutex<Option<TermProvider>>,
    /// The journal of what commands changed, set by the `fs` row (it needs the undo store) for the `term` row (card OA7-03).
    journal: Mutex<Option<Arc<Journal>>>,
    /// Where the size of a page of a file comes from for this build; none: the one the program set (`set_view_chars_source`), and without that the default of the file tools.
    view_chars: Option<ViewCharsSource>,
}

impl ProviderCtx {
    pub fn new(data_dir: &Path, host: Arc<ToolHost>, browser: Arc<BrowserBackend>, windows: Arc<Mutex<StdioMcpClient>>) -> Self {
        Self { data_dir: data_dir.to_path_buf(), host, browser, windows, guard: Mutex::new(None), term: Mutex::new(None), journal: Mutex::new(None), view_chars: None }
    }

    fn set_journal(&self, journal: Arc<Journal>) {
        *self.journal.lock().unwrap_or_else(PoisonError::into_inner) = Some(journal);
    }

    fn journal(&self) -> Option<Arc<Journal>> {
        self.journal.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// A build that sizes the pages of a file by `source` and not by the program's (a test uses it, so that no process-wide state changes).
    pub fn with_view_chars(mut self, source: ViewCharsSource) -> Self {
        self.view_chars = Some(source);
        self
    }

    /// The function that says how many characters of lines fit what the model is shown, if there is one.
    fn view_chars_source(&self) -> Option<ViewCharsSource> {
        self.view_chars.or_else(|| VIEW_CHARS_SOURCE.get().copied())
    }

    /// The guard of the allowed folders; `None` until the `fs` row has made it, and for good when the file tools are off.
    pub fn guard(&self) -> Option<Arc<PathGuard>> {
        self.guard.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set_guard(&self, guard: Arc<PathGuard>) {
        *self.guard.lock().unwrap_or_else(PoisonError::into_inner) = Some(guard);
    }

    /// The terminal tools; `None` until the `term` row has made them, and for good when they are off.
    pub fn term(&self) -> Option<TermProvider> {
        self.term.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    fn set_term(&self, term: TermProvider) {
        *self.term.lock().unwrap_or_else(PoisonError::into_inner) = Some(term);
    }

    fn tools_dir(&self) -> PathBuf {
        self.data_dir.join("tools")
    }
}

/// What building a family came to.
pub enum Built {
    /// A provider for the host to register (the host's answer is logged: `tool host: registered N <family> tools`).
    Provider(Arc<dyn ToolProvider>),
    /// The family is not offered, and why. The build said so in the log when it was worth saying.
    Off(String),
    /// The build registered its tools itself (it needed the host to) and logged it; this many.
    Done(usize),
    /// The build tried to register and failed; it logged why.
    Failed(String),
}

/// One family of tools.
#[derive(Clone, Copy)]
pub struct ProviderSpec {
    pub id: &'static str,
    /// The word in the log lines (`tool host: registered N <family> tools`).
    pub family: &'static str,
    /// The environment variable that turns the family on or off, when there is one.
    pub off_switch: Option<&'static str>,
    /// `Some(why)` when the switch leaves the family out; asked before the family's config is read.
    pub off_when: Option<fn() -> Option<String>>,
    /// Works inside the allowed folders, so it is off while the file tools are.
    pub needs_files: bool,
    pub pack: Option<PackSpec>,
    /// A line for the status route about a row that is not what its name suggests.
    pub note: Option<&'static str>,
    pub build: fn(&ProviderCtx) -> Result<Built, String>,
}

impl ProviderSpec {
    pub const fn new(id: &'static str, family: &'static str, build: fn(&ProviderCtx) -> Result<Built, String>) -> Self {
        Self { id, family, off_switch: None, off_when: None, needs_files: false, pack: None, note: None, build }
    }

    pub const fn files(mut self) -> Self {
        self.needs_files = true;
        self
    }

    pub const fn pack(mut self, pack: PackSpec) -> Self {
        self.pack = Some(pack);
        self
    }

    pub const fn switch(mut self, name: &'static str, off_when: fn() -> Option<String>) -> Self {
        self.off_switch = Some(name);
        self.off_when = Some(off_when);
        self
    }

    pub const fn note(mut self, note: &'static str) -> Self {
        self.note = Some(note);
        self
    }
}

/// What became of one row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderState {
    Registered { tools: usize },
    Off { why: String },
    Failed { why: String },
}

#[derive(Clone, Debug)]
pub struct ProviderReport {
    pub id: &'static str,
    pub family: &'static str,
    pub off_switch: Option<&'static str>,
    pub pack: Option<&'static str>,
    pub note: Option<&'static str>,
    pub state: ProviderState,
}

impl ProviderReport {
    pub fn to_json(&self) -> serde_json::Value {
        let (state, tools, why) = match &self.state {
            ProviderState::Registered { tools } => ("registered", Some(*tools), None),
            ProviderState::Off { why } => ("off", None, Some(why.as_str())),
            ProviderState::Failed { why } => ("failed", None, Some(why.as_str())),
        };
        serde_json::json!({ "id": self.id, "family": self.family, "state": state, "tools": tools, "why": why, "switch": self.off_switch, "pack": self.pack, "note": self.note })
    }
}

// ---- the routing packs ----------------------------------------------------------------------------------------------------------

const PACK_FILES: PackSpec = PackSpec { order: 0, name: "files", prefixes: &["fs."], words: &["file", "files", "folder", "folders", "directory", "directories", "rename", "copy", "trash", "recycle", ".txt", ".md", ".csv", ".json", ".log", ".zip", ".py", ".rs"] };
const PACK_DOCS: PackSpec = PackSpec { order: 1, name: "docs", prefixes: &["doc."], words: &["pdf", "docx", "pptx", "xlsx", "xls", ".pdf", ".docx", ".pptx", ".xlsx", ".xls", ".ods", "powerpoint", "spreadsheet", "excel", "slides", "presentation", "word document", "worksheet", "workbook"] };
const PACK_FLOWS: PackSpec = PackSpec { order: 2, name: "flows", prefixes: &["flows."], words: &["flow", "flows", "workflow", "workflows", "automation", "automations"] };
const PACK_TERMINAL: PackSpec = PackSpec { order: 3, name: "terminal", prefixes: &["term."], words: &["terminal", "command", "shell", "powershell", "cmd", "script", "compile", "build", "npm", "cargo", "pip", "pytest", "run the tests", "install"] };
const PACK_GIT: PackSpec = PackSpec { order: 4, name: "git", prefixes: &["git."], words: &["git", "commit", "branch", "repo", "repository", "diff", "merge", "rebase", "stash", "checkout"] };
const PACK_FORGE: PackSpec = PackSpec { order: 5, name: "forge", prefixes: &["forge."], words: &["github", "gitlab", "issue", "issues", "pull request", "pull requests", "merge request", "pipeline", "pipelines", "ci"] };
const PACK_WEB: PackSpec = PackSpec { order: 6, name: "web", prefixes: &["web.", "http."], words: &["http", "https", "api", "endpoint", "download", "fetch", "search the web", "webhook", "curl", "url"] };
const PACK_SSH: PackSpec = PackSpec { order: 7, name: "ssh", prefixes: &["ssh."], words: &["ssh", "remote server", "remote host", "scp", "known_hosts"] };
const PACK_DOCKER: PackSpec = PackSpec { order: 8, name: "docker", prefixes: &["docker."], words: &["docker", "container", "containers", "image", "compose", "dockerfile"] };
const PACK_K8S: PackSpec = PackSpec { order: 9, name: "k8s", prefixes: &["k8s."], words: &["kubernetes", "k8s", "kubectl", "pod", "pods", "deployment", "namespace", "cluster", "helm"] };
const PACK_IAC: PackSpec = PackSpec { order: 10, name: "iac", prefixes: &["iac."], words: &["terraform", "tofu", "infrastructure", "tfplan", "iac"] };
const PACK_CLOUD: PackSpec = PackSpec { order: 11, name: "cloud", prefixes: &["cloud."], words: &["aws", "azure", "gcp", "gcloud", "s3", "ec2", "lambda", "cloud"] };
const PACK_DATABASE: PackSpec = PackSpec { order: 12, name: "database", prefixes: &["db."], words: &["database", "databases", "sqlite", "postgres", "postgresql", "mysql", "mariadb", "redis", "sql", "query", "table", "tables", "schema", "rows", "cache"] };
const PACK_MONITOR: PackSpec = PackSpec { order: 13, name: "monitor", prefixes: &["mon."], words: &["log", "logs", "event log", "health", "uptime", "monitor", "monitoring", "probe", "errors in", "disk space"] };
const PACK_SYSTEM: PackSpec = PackSpec { order: 14, name: "system", prefixes: &["sys.", "proc."], words: &["process", "processes", "cpu", "memory", "ram", "disk", "drive", "drives", "environment variable", "kill", "system info", "task manager"] };
const PACK_VAULT: PackSpec = PackSpec { order: 15, name: "vault", prefixes: &["vault."], words: &["vault", "my notes", "in my notes", "my documents", "my knowledge", "knowledge base"] };
const PACK_REMINDERS: PackSpec = PackSpec { order: 16, name: "reminders", prefixes: &["reminder."], words: &["remind", "reminder", "reminders", "timer", "timers", "alarm"] };
const PACK_MEMORY: PackSpec = PackSpec { order: 17, name: "memory", prefixes: &["memory."], words: &["remember", "forget", "what do you know about me", "my preferences"] };
const PACK_SCREEN: PackSpec = PackSpec { order: 18, name: "screen", prefixes: &["screen."], words: &["screen", "screenshot", "onscreen", "on-screen", "ocr", "visible text"] };
// The clipboard tools exist only when `AQUA_CLIPBOARD_TOOLS=on` registered them (`tool_host.rs`); with the switch off this pack finds nothing to show.
const PACK_CLIPBOARD: PackSpec = PackSpec { order: 19, name: "clipboard", prefixes: &["clipboard."], words: &["clipboard", "paste", "pasted", "copied"] };
// `research.run` exists only when `AQUA_RESEARCH=on` registered it (`research/tool.rs`), and this pack takes part in the routing only then (`route_by_words_with`).
const PACK_RESEARCH: PackSpec = PackSpec { order: 20, name: "research", prefixes: &["research."], words: &["research", "investigate", "deep research", "fact-check", "factcheck", "cross-check"] };
// `project.profile` and `check.run` are on offer only when `AQUA_CHECK_TOOLS=on` (card OA4-05): they are registered, and unavailable until then. The pack has no words of its own: a goal never routes to it by
// what it says, so with the switch off the routing is what it was; a coding run is shown the two tools by `coding_session::PINNED_WHEN_OFFERED`, and only when the host lists them.
const PACK_CHECK: PackSpec = PackSpec { order: 21, name: "check", prefixes: &["check.", "project."], words: &[] };
// `code.outline`, `code.symbols`, `code.deps` and `code.refs` are on offer only when `AQUA_CODE_TOOLS=on`, and `code.map` only when `AQUA_REPO_MAP=on` (cards OA12-02 and OA12-03): registered, and
// unavailable until then. As the check pack, this one has no words: a goal never routes to it by what it says, so with the switches off the routing is what it was; a coding run is shown the tools
// by `coding_session::PINNED_WHEN_OFFERED`, and only when the host lists them.
const PACK_CODE: PackSpec = PackSpec { order: 22, name: "code", prefixes: &["code."], words: &[] };
const PACK_COMPANY: PackSpec = PackSpec { order: 23, name: "company", prefixes: &["company."], words: &["company", "team", "teams", "project owner", "owns", "responsible", "ticket", "tickets", "decision", "decisions", "colleague", "coworker", "department"] };

// ---- the rows ---------------------------------------------------------------------------------------------------------------------

fn clipboard_off() -> Option<String> {
    (!crate::flags::is_on(&crate::flags::CLIPBOARD_TOOLS)).then(|| "AQUA_CLIPBOARD_TOOLS is not on (the agent has never been able to reach the clipboard, and a person who wants it says so)".to_string())
}

fn screen_off() -> Option<String> {
    (!crate::flags::is_on(&crate::flags::SCREEN_TOOLS)).then(|| "AQUA_SCREEN_TOOLS is off".to_string())
}

fn research_off() -> Option<String> {
    (!crate::research::enabled()).then(|| "AQUA_RESEARCH is not on".to_string())
}

fn lsp_off() -> Option<String> {
    (!crate::flags::is_on(&crate::flags::LSP)).then(|| "AQUA_LSP is not on (a language server is a program Aqua would start, so the person turns the tools on and names the program)".to_string())
}

fn command_tools_off() -> Option<String> {
    (!crate::command_tools::enabled()).then(|| "AQUA_COMMAND_TOOLS is not on (the agent has never been able to run a command you declared, and a person who wants it says so)".to_string())
}

/// Every family, in the order they were registered before this table existed.
pub static PROVIDERS: &[ProviderSpec] = &[
    ProviderSpec::new("browser", "browser", build_browser),
    ProviderSpec::new("windows", "desktop", build_windows),
    ProviderSpec::new("fs", "filesystem", build_fs).pack(PACK_FILES),
    ProviderSpec::new("doc", "document", build_doc).files().pack(PACK_DOCS),
    ProviderSpec::new("docker", "docker", build_docker).files().pack(PACK_DOCKER),
    ProviderSpec::new("iac", "iac", build_iac).files().pack(PACK_IAC),
    ProviderSpec::new("forge", "forge", build_forge).files().pack(PACK_FORGE),
    ProviderSpec::new("mon", "mon", build_mon).files().pack(PACK_MONITOR),
    ProviderSpec::new("cloud", "cloud", build_cloud).files().pack(PACK_CLOUD),
    ProviderSpec::new("k8s", "k8s", build_k8s).files().pack(PACK_K8S),
    ProviderSpec::new("http", "http and web", build_http).files().pack(PACK_WEB),
    ProviderSpec::new("db", "database", build_db).files().pack(PACK_DATABASE),
    ProviderSpec::new("git", "git", build_git).files().pack(PACK_GIT),
    ProviderSpec::new("ssh", "ssh", build_ssh).files().pack(PACK_SSH),
    ProviderSpec::new("term", "terminal", build_term).files().pack(PACK_TERMINAL),
    ProviderSpec::new("check", "check", build_check).files().pack(PACK_CHECK).note("project.profile and check.run are registered and listed, but unavailable (not shown to the agent, not callable) until AQUA_CHECK_TOOLS=on"),
    ProviderSpec::new("code", "code", build_code).files().pack(PACK_CODE).note("code.outline, code.symbols, code.deps and code.refs are registered and listed, but unavailable (not shown to the agent, not callable) until AQUA_CODE_TOOLS=on, and code.map until AQUA_REPO_MAP=on"),
    // After the code tools it joins by name (`code.definition` ...): off unless `AQUA_LSP=on` and `data/tools/lsp.json` names a server, so with the defaults nothing is registered (card OA12-04).
    ProviderSpec::new("lsp", "language server", build_lsp).files().switch("AQUA_LSP", lsp_off).note("code.definition, code.references, code.hover and code.document_symbols are registered only with AQUA_LSP=on and a server in data/tools/lsp.json; a server is started by the first call of a run that needs it, and that call asks"),
    ProviderSpec::new("sys", "system and process", build_sys).pack(PACK_SYSTEM),
    ProviderSpec::new("memory", "memory", build_memory).pack(PACK_MEMORY),
    ProviderSpec::new("company", "Company Brain", build_company).pack(PACK_COMPANY).note("company.* tools use the tenant-scoped FalkorDB graph configured in data/tools/company_brain.json; passwords stay in Credential Manager"),
    ProviderSpec::new("skills", "skill", build_skills).note("skill.load is always registered and reports itself unavailable until a skill is written (data/skills/<name>/SKILL.md) or approved, or AQUA_SHIPPED_SKILLS=on offers the ones that ship with Aqua; skill.file also needs AQUA_SKILLS_V2=on"),
    ProviderSpec::new("mcp", "MCP server", build_mcp).note("the tools of the servers in tools/mcp.json are added as each server is trusted and starts; none is started before"),
    ProviderSpec::new("flows", "flow", build_flows).pack(PACK_FLOWS),
    ProviderSpec::new("reminders", "reminder", build_reminders).pack(PACK_REMINDERS),
    ProviderSpec::new("vault", "vault", build_vault).pack(PACK_VAULT).note("the vault's tools exist only when the vault is open (it reads nothing until a folder is chosen)"),
    ProviderSpec::new("screen", "screen", build_screen).switch("AQUA_SCREEN_TOOLS", screen_off).pack(PACK_SCREEN),
    ProviderSpec::new("clipboard", "clipboard", build_clipboard).switch("AQUA_CLIPBOARD_TOOLS", clipboard_off).pack(PACK_CLIPBOARD),
    ProviderSpec::new("tools_search", "tool search", build_tools_search).note("tools.search is always registered; the agent is shown it only when AQUA_TOOL_SEARCH is on"),
    ProviderSpec::new("research", "research", build_research).switch("AQUA_RESEARCH", research_off).pack(PACK_RESEARCH),
    // After the terminal it runs through, which it needs (card OA11-02).
    ProviderSpec::new("cmd", "command tool", build_cmd)
        .files()
        .switch("AQUA_COMMAND_TOOLS", command_tools_off)
        .note("the cmd.* tools of data/tools/custom.json are registered here; the proj.* tools of a trusted repository's .aqua/tools.json are learned when a run in that project meets them, and exist only for it"),
];

/// The routing packs the table defines, in the order of the list that `agent_tools` had before (a tie in a score goes to the earlier one).
pub fn packs() -> &'static [PackSpec] {
    static PACKS: OnceLock<Vec<PackSpec>> = OnceLock::new();
    PACKS.get_or_init(|| {
        let mut all: Vec<PackSpec> = PROVIDERS.iter().filter_map(|p| p.pack).collect();
        all.sort_by_key(|p| p.order);
        all
    })
}

// ---- the builds ---------------------------------------------------------------------------------------------------------------------

/// Why a config file stopped a family: the file's name and where the mistake is.
fn config_why(name: &str, path: &Path) -> String {
    match crate::config_files::last_error(path) {
        Some(e) => format!("{name} cannot be used: {}", e.detail()),
        None => format!("{name} cannot be used"),
    }
}

fn build_browser(ctx: &ProviderCtx) -> Result<Built, String> {
    Ok(Built::Provider(Arc::new(BrowserProvider::new(ctx.browser.clone()))))
}

fn build_windows(ctx: &ProviderCtx) -> Result<Built, String> {
    Ok(Built::Provider(Arc::new(WindowsUiProvider::new(ctx.windows.clone()))))
}

fn build_fs(ctx: &ProviderCtx) -> Result<Built, String> {
    let data_dir = ctx.data_dir.as_path();
    let Some(mut cfg) = load_fs_config(data_dir) else { return Ok(Built::Off(config_why("fs.json", &crate::tool_host::fs_config_path(data_dir)))) };
    // A page of a file, and the lines of a search, are sized to what the model is shown of a result in the window it runs with (card OA5-03, CU-03); a number the person wrote in
    // `fs.json` is kept. The size comes from the function the program installed (see `ViewCharsSource`): this module may not name `modules`.
    if cfg.limits.view_chars == aqua_native_tools::fs::guard::DEFAULT_VIEW_CHARS {
        if let Some(source) = ctx.view_chars_source() {
            cfg.limits.view_chars = source();
        }
    }
    let guard = match PathGuard::new(cfg, protected_paths(data_dir)) {
        Ok(g) => Arc::new(g),
        Err(e) => {
            warn!("filesystem tools are off: {e}");
            return Ok(Built::Off(format!("fs.json: {e}")));
        }
    };
    let _ = PROJECT_GUARD.set(guard.clone());
    #[cfg(all(feature = "test-support", not(test)))]
    crate::tool_host::note_guard_of_this_thread(&guard);
    // The whole text of a result the host cuts is kept in a folder the file tools read as `@out` (card OA5-02): both are set here, where the guard is made, or neither.
    match crate::tool_host::install_spill(&ctx.host, &guard, data_dir) {
        Ok(dir) => info!("tool host: the full text of long tool output is kept in {} (read it as @out)", dir.display()),
        Err(why) => info!("tool host: the full text of long tool output is not kept: {why}"),
    }
    let trash = match Trash::new(data_dir.join("tools").join("trash")) {
        Ok(t) => Arc::new(t),
        Err(e) => {
            warn!("filesystem tools are off: cannot create the undo store: {e}");
            return Ok(Built::Off(format!("cannot create the undo store: {e}")));
        }
    };
    // The size limit takes the entries of an agent run that is still going last, so that a long run keeps its earliest undo copies.
    trash.set_live_run_check(crate::agent_tools::is_active);
    // What is past its age or over its size goes now; after that every stash looks again every ten minutes.
    let cleared = trash.enforce_limits(aqua_native_tools::fs::trash::DEFAULT_MAX_AGE_DAYS, aqua_native_tools::fs::trash::DEFAULT_MAX_BYTES, None);
    if cleared.entries > 0 {
        info!("undo store: removed {} old item(s), {} bytes", cleared.entries, cleared.bytes);
    }
    let _ = TRASH.set(trash.clone());
    // The families that work inside the allowed folders are built from here on, whether or not the host takes the file tools.
    ctx.set_guard(guard.clone());
    // What commands changed (the journal, off unless `AQUA_TERM_JOURNAL` or `journal` in term.json says so) and the checkpoints of a project (off unless `AQUA_SNAPSHOT` says so) write their
    // records to the undo store, and `fs.undo_run` puts them back (card OA7-03). They are made whether or not they are on: an undo of a run that was recorded earlier must still work.
    let git_path = load_git_config(data_dir).and_then(|c| c.git_path);
    let journal = Arc::new(Journal::new(&git_path, trash.clone()));
    let checkpoints = Arc::new(Checkpoints::new(&git_path, data_dir.join("snapshots"), guard.clone(), trash.clone()));
    let _ = JOURNAL.set(journal.clone());
    let _ = CHECKPOINTS.set(checkpoints.clone());
    ctx.set_journal(journal.clone());
    // `fs.outline` is a tool that is offered only when `AQUA_FS_OUTLINE` says so (card OA5-03, CX-17); the switch is asked each time the tools are listed.
    Ok(Built::Provider(Arc::new(
        FsProvider::new(guard, trash)
            .with_outline(Arc::new(|| crate::flags::is_on(&crate::flags::FS_OUTLINE)))
            // `fs.edit_lines` (card OA6-02, ED-07) is offered only while `AQUA_EDIT_LINES` is on; asked each time the tools are listed and at each call.
            .with_edit_lines(Arc::new(|| crate::flags::is_on(&crate::flags::EDIT_LINES)))
            // What the file tools say about a change that leaves a file's syntax worse (card OA6-03): off unless `AQUA_SYNTAX_GATE` says `warn` or `on`; asked at each call.
            .with_syntax_gate(Arc::new(|| aqua_native_tools::fs::ast::Gate::parse(crate::flags::choice(&crate::flags::SYNTAX_GATE))))
            .with_effects(Arc::new(Effects { journal: Some(journal), checkpoints: Some(checkpoints) })),
    )))
}

fn guard_of(ctx: &ProviderCtx) -> Result<Arc<PathGuard>, String> {
    ctx.guard().ok_or_else(|| "the file tools are off".to_string())
}

/// Reading PDF, Word, PowerPoint, Excel and CSV files, through the same folder guard.
fn build_doc(ctx: &ProviderCtx) -> Result<Built, String> {
    Ok(Built::Provider(Arc::new(aqua_native_tools::doc::DocProvider::new(guard_of(ctx)?))))
}

/// Bind mounts and compose files must live inside the same roots. Docker is optional.
fn build_docker(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_docker_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("docker.json", &crate::tool_host::docker_config_path(&ctx.data_dir)))) };
    let p = DockerProvider::new(cfg, guard_of(ctx)?, &ctx.tools_dir()).map_err(|e| e.to_string())?;
    if !p.installed() {
        info!("tool host: Docker is not installed, so docker.* tools are hidden");
    }
    Ok(Built::Provider(Arc::new(p)))
}

/// Terraform folders come only from iac.json and must be inside the allowed roots; credentials are read from Credential Manager by name. There is no destroy tool.
fn build_iac(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_iac_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("iac.json", &crate::tool_host::iac_config_path(&ctx.data_dir)))) };
    let p = IacProvider::new(cfg, guard_of(ctx)?, Arc::new(CredentialManager), &ctx.tools_dir()).map_err(|e| e.to_string())?;
    if !p.installed() {
        info!("tool host: terraform is not installed, so iac.* tools are hidden");
    }
    Ok(Built::Provider(Arc::new(p)))
}

/// GitHub and GitLab: accounts and repositories come only from forge.json; tokens from Credential Manager.
fn build_forge(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_forge_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("forge.json", &crate::tool_host::forge_config_path(&ctx.data_dir)))) };
    Ok(Built::Provider(Arc::new(ForgeProvider::new(cfg, Arc::new(CredentialManager)).map_err(|e| e.to_string())?)))
}

/// Monitoring: log files inside the allowed roots, the Application/System event logs, and the checks listed in mon.json.
fn build_mon(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_mon_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("mon.json", &crate::tool_host::mon_config_path(&ctx.data_dir)))) };
    Ok(Built::Provider(Arc::new(MonProvider::new(cfg, guard_of(ctx)?, &ctx.tools_dir()).map_err(|e| e.to_string())?)))
}

/// Read-only cloud inspection; nothing is enabled until cloud.json says so.
fn build_cloud(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_cloud_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("cloud.json", &crate::tool_host::cloud_config_path(&ctx.data_dir)))) };
    Ok(Built::Provider(Arc::new(CloudProvider::new(cfg, Arc::new(CredentialManager), &ctx.tools_dir()).map_err(|e| e.to_string())?)))
}

/// Manifests must live inside the same roots; clusters and namespaces come only from k8s.json.
fn build_k8s(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_k8s_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("k8s.json", &crate::tool_host::k8s_config_path(&ctx.data_dir)))) };
    let p = K8sProvider::new(cfg, guard_of(ctx)?, &ctx.tools_dir()).map_err(|e| e.to_string())?;
    if !p.installed() {
        info!("tool host: kubectl is not installed, so k8s.* tools are hidden");
    }
    Ok(Built::Provider(Arc::new(p)))
}

/// Uploads and downloads use local paths inside the same roots; secrets come from Credential Manager.
fn build_http(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_http_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("http.json", &crate::tool_host::http_config_path(&ctx.data_dir)))) };
    Ok(Built::Provider(Arc::new(HttpProvider::new(cfg, guard_of(ctx)?, Arc::new(CredentialManager)).map_err(|e| e.to_string())?)))
}

/// Database files must live inside the same roots too.
fn build_db(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_db_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("db.json", &crate::tool_host::db_config_path(&ctx.data_dir)))) };
    Ok(Built::Provider(Arc::new(DbProvider::new(cfg, guard_of(ctx)?, Arc::new(CredentialManager)).map_err(|e| e.to_string())?)))
}

/// Repositories must live inside the same roots, so the git tools need the guard too.
fn build_git(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_git_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("repos.json", &crate::tool_host::git_config_path(&ctx.data_dir)))) };
    let mut provider = GitProvider::new(cfg, guard_of(ctx)?, &ctx.tools_dir())
        .map_err(|e| e.to_string())?
        // The worktree tools (card OA7-05, TM-13) and the conflict and pull request tools (TM-17, TM-18) are on offer only while their setting says so; each is asked every time the tools are listed or called.
        .with_worktrees(Arc::new(|| crate::flags::is_on(&crate::flags::WORKTREES)))
        .with_review(Arc::new(|| crate::flags::is_on(&crate::flags::GIT_REVIEW)));
    // A resolution is kept in the undo store of the file tools first (the `fs` row built it), so that Undo this run can put the file back.
    if let Some(trash) = TRASH.get() {
        provider = provider.with_trash(trash.clone());
    }
    Ok(Built::Provider(Arc::new(provider)))
}

/// Uploads and downloads use local paths inside the same roots, so the SSH tools need the guard too.
fn build_ssh(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(cfg) = load_ssh_config(&ctx.data_dir) else { return Ok(Built::Off(config_why("ssh.json", &crate::tool_host::ssh_config_path(&ctx.data_dir)))) };
    Ok(Built::Provider(Arc::new(SshProvider::new(cfg, guard_of(ctx)?, &ctx.tools_dir()).map_err(|e| e.to_string())?)))
}

/// Commands run with a working folder inside the same roots, so the terminal tools need the guard. Both files are read, as they always were.
fn build_term(ctx: &ProviderCtx) -> Result<Built, String> {
    let (cfg, rules) = (load_term_config(&ctx.data_dir), load_term_rules(&ctx.data_dir));
    match (cfg, rules) {
        (Some(cfg), Some(rules)) => {
            let term = TermProvider::with_rules(guard_of(ctx)?, cfg, rules);
            // The journal of what commands changed: given to the tools whether or not it is on (`term.undo` is offered, and commands are looked at, only while it is).
            if let Some(journal) = ctx.journal() {
                term.set_journal(journal);
            }
            ctx.set_term(term.clone());
            Ok(Built::Provider(Arc::new(term)))
        }
        (None, _) => Ok(Built::Off(config_why("term.json", &crate::tool_host::term_config_path(&ctx.data_dir)))),
        (_, None) => Ok(Built::Off(config_why("term_rules.json", &crate::tool_host::term_rules_path(&ctx.data_dir)))),
    }
}

/// `project.profile` and `check.run` (card OA4-05): the profile of a project and the check of its tests, build, lint and types, run through the terminal tools. Unavailable unless `AQUA_CHECK_TOOLS=on`;
/// off when the terminal tools are. The profiles are kept in `data/cache/project_profiles.json`.
fn build_check(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(term) = ctx.term() else { return Ok(Built::Off("the terminal tools are off, and a check is run through them".into())) };
    let cache = ctx.data_dir.join("cache").join("project_profiles.json");
    // On offer only while `AQUA_CHECK_TOOLS` is on (asked each time the tools are listed, as `fs.outline` is).
    Ok(Built::Provider(Arc::new(CheckProvider::new(guard_of(ctx)?, term, Some(cache)).with_switch(Arc::new(|| crate::flags::is_on(&crate::flags::CHECK_TOOLS))))))
}

/// `code.outline` and `code.symbols` (card OA12-02): the symbols of a source file and a search for a name over a folder, through the file tools' guard and walker; and `code.deps`, `code.refs`
/// and `code.map` (card OA12-03): what a file imports and who imports it, where a name is used, and a map of the project of about 250 tokens. Unavailable unless `AQUA_CODE_TOOLS=on` (the map:
/// `AQUA_REPO_MAP=on`); off when the file tools are. The symbols and imports are kept in memory only, until a file changes.
fn build_code(ctx: &ProviderCtx) -> Result<Built, String> {
    // On offer only while the switches are on (asked each time the tools are listed, as `fs.outline` and the check tools are).
    Ok(Built::Provider(Arc::new(
        CodeProvider::new(guard_of(ctx)?)
            .with_switch(Arc::new(|| crate::flags::is_on(&crate::flags::CODE_TOOLS)))
            .with_map_switch(Arc::new(|| crate::flags::is_on(&crate::flags::REPO_MAP))),
    )))
}

/// `code.definition`, `code.references`, `code.hover` and `code.document_symbols` (card OA12-04): what a language server knows, from a program the person named in `data/tools/lsp.json` with its hash. Registered
/// only with `AQUA_LSP=on` (the switch leaves the family out before the file is read) and only when the file names a server; a file that does not parse makes the family fail with the line and column. Nothing is
/// started here: a server is started by the first call of a run that needs it, which is classified as running a program. Off when the file tools are.
fn build_lsp(ctx: &ProviderCtx) -> Result<Built, String> {
    let config = match LspConfig::load(&ctx.tools_dir()) {
        Ok(Some(c)) if !c.is_empty() => c,
        Ok(_) => return Ok(Built::Off("data/tools/lsp.json is missing or names no language server (no tool is registered, and nothing is started)".into())),
        Err(e) => return Err(format!("data/tools/lsp.json: {}", e.detail())),
    };
    // On offer only while the switch is on (asked each time the tools are listed, as the code tools are).
    Ok(Built::Provider(Arc::new(LspProvider::new(guard_of(ctx)?, config).with_switch(Arc::new(|| crate::flags::is_on(&crate::flags::LSP))))))
}

/// The command tools (card OA11-02): `cmd.*` from `data/tools/custom.json`, and the lookup of a trusted repository's `proj.*`. They run through the terminal provider, so they need it.
fn build_cmd(ctx: &ProviderCtx) -> Result<Built, String> {
    let Some(term) = ctx.term() else { return Ok(Built::Off("the terminal tools are off, and the command tools run through them".into())) };
    let n = crate::command_tools::attach(&ctx.host, &ctx.data_dir, term, crate::agent_tools::current_run);
    Ok(Built::Done(n))
}

fn build_sys(_ctx: &ProviderCtx) -> Result<Built, String> {
    Ok(Built::Provider(Arc::new(SysProvider::new())))
}

/// The memory tools (`memory.*`): what the person asked Aqua to remember, in `data/user_memory.db`. A database that cannot be opened leaves them out.
fn build_memory(ctx: &ProviderCtx) -> Result<Built, String> {
    match crate::memory::user_facts::FactStore::open(ctx.data_dir.join("user_memory.db")) {
        Ok(store) => {
            let store = Arc::new(store);
            // The one the program uses for "remember that ..." and for prompts is the one of the running host's data folder.
            crate::memory::user_facts::install(store.clone());
            Ok(Built::Provider(Arc::new(crate::memory::user_facts::provider::UserFactsProvider::new(store))))
        }
        Err(e) => {
            warn!("the memory of the user could not be opened ({e}): no memory tools");
            Ok(Built::Off(format!("the memory of the user could not be opened ({e})")))
        }
    }
}

fn build_company(ctx: &ProviderCtx) -> Result<Built, String> {
    let path = ctx.data_dir.join("tools").join("company_brain.json");
    let Some(config) = crate::config_files::load(&ctx.data_dir, &crate::config_files::COMPANY_BRAIN).into_option() else {
        return Ok(Built::Off(config_why("company_brain.json", &path)));
    };
    if !config.enabled {
        return Ok(Built::Off("data/tools/company_brain.json has enabled set to false".into()));
    }
    Ok(Built::Provider(Arc::new(crate::company_brain::CompanyProvider::live(config, Arc::new(CredentialManager)))))
}

/// `skill.load`, `skill.file` and the skills they read (`data/skills/<name>/SKILL.md`). The tools are registered **whether or not a skill is written yet**: the skills are swapped when one
/// is approved (`skills::reload`), and a tool that was left out at start would have waited for a restart (card OA11-01, task EX-07). With no skill they report themselves unavailable. A
/// skill that cannot be read is left out and named in the log.
fn build_skills(ctx: &ProviderCtx) -> Result<Built, String> {
    let skills = crate::skills::reload(&ctx.data_dir);
    if skills.is_empty() {
        info!("skills: none is written yet (data/skills/<name>/SKILL.md); skill.load is there for the first one that is approved");
    } else {
        info!("skills: {} read ({})", skills.len(), skills.names().join(", "));
    }
    match ctx.host.register(Arc::new(crate::skills::SkillProvider::live())) {
        Ok(n) => {
            info!("tool host: registered {n} skill tool(s)");
            Ok(Built::Done(n))
        }
        Err(e) => {
            warn!("tool host: could not register skill.load: {e}");
            Ok(Built::Failed(e))
        }
    }
}

/// Read `data/tools/mcp.json` and start, in the background, the servers the person trusted. Nothing else is started.
fn build_mcp(ctx: &ProviderCtx) -> Result<Built, String> {
    crate::tool_host::register_user_mcp(&ctx.host, &ctx.data_dir);
    Ok(Built::Done(0))
}

fn build_flows(_ctx: &ProviderCtx) -> Result<Built, String> {
    Ok(Built::Provider(Arc::new(crate::flows::tools::FlowsProvider(Arc::new(crate::flows::tools::Live)))))
}

fn build_reminders(_ctx: &ProviderCtx) -> Result<Built, String> {
    Ok(Built::Provider(Arc::new(crate::reminders::ReminderProvider(Arc::new(crate::reminders::Live)))))
}

/// The vault's tools exist only when the vault is open (it reads nothing until the person chooses a folder).
fn build_vault(_ctx: &ProviderCtx) -> Result<Built, String> {
    match crate::vault::global() {
        Some(vault) => Ok(Built::Provider(Arc::new(crate::vault::tools::VaultProvider(vault.clone())))),
        None => Ok(Built::Off("the vault is not open (no folder has been chosen)".into())),
    }
}

/// Reading the text on the screen (card R2-07). What it returns is the person's and untrusted; `AQUA_SCREEN_TOOLS=off` leaves the two tools out.
fn build_screen(_ctx: &ProviderCtx) -> Result<Built, String> {
    #[cfg(windows)]
    {
        Ok(Built::Provider(Arc::new(aqua_vision_adapter::screen::ScreenProvider::windows())))
    }
    #[cfg(not(windows))]
    {
        Ok(Built::Off("the screen tools exist on Windows only".into()))
    }
}

/// The clipboard (card R4-03): `clipboard.read` always asks and is private, `clipboard.write` asks. Off unless `AQUA_CLIPBOARD_TOOLS=on`; Windows-MCP's own Clipboard tool stays banned.
fn build_clipboard(_ctx: &ProviderCtx) -> Result<Built, String> {
    Ok(Built::Provider(Arc::new(aqua_native_tools::clipboard::ClipboardProvider::system())))
}

fn build_tools_search(ctx: &ProviderCtx) -> Result<Built, String> {
    match ctx.host.register(Arc::new(crate::tool_search::ToolSearchProvider)) {
        Ok(n) => {
            info!("tool host: tools.search is registered{}", if crate::tool_search::enabled() { " and shown to the agent" } else { " (not shown to the agent: AQUA_TOOL_SEARCH is off)" });
            Ok(Built::Done(n))
        }
        Err(e) => {
            warn!("tool host: could not register tools.search: {e}");
            Ok(Built::Failed(e))
        }
    }
}

/// Deep research (card R2-10): the tool `research.run` exists only when `AQUA_RESEARCH=on`. Its searches and pages are `web.search` and `web.read` calls to this same host.
fn build_research(ctx: &ProviderCtx) -> Result<Built, String> {
    if crate::research::tool::register(&ctx.host, PROJECT_GUARD.get().cloned(), true) {
        info!("tool host: registered the research tool (AQUA_RESEARCH is on)");
        Ok(Built::Done(1))
    } else {
        Ok(Built::Failed("the host did not take the tool (see the log)".into()))
    }
}

// ---- registering ---------------------------------------------------------------------------------------------------------------------

static LAST: Mutex<Vec<ProviderReport>> = Mutex::new(Vec::new());

/// Registers every row of [`PROVIDERS`] with the host and keeps what became of each for [`status_json`] and the doctor.
pub fn register_all(ctx: &ProviderCtx) -> Vec<ProviderReport> {
    let reports = register_with(PROVIDERS, ctx);
    *LAST.lock().unwrap_or_else(PoisonError::into_inner) = reports.clone();
    reports
}

/// [`register_all`] for the rows given (a test's own table). Nothing is remembered.
pub fn register_with(specs: &[ProviderSpec], ctx: &ProviderCtx) -> Vec<ProviderReport> {
    specs
        .iter()
        .map(|spec| {
            let state = register_one(spec, ctx);
            ProviderReport { id: spec.id, family: spec.family, off_switch: spec.off_switch, pack: spec.pack.map(|p| p.name), note: spec.note, state }
        })
        .collect()
}

fn register_one(spec: &ProviderSpec, ctx: &ProviderCtx) -> ProviderState {
    if spec.needs_files && ctx.guard().is_none() {
        return ProviderState::Off { why: "the file tools are off (see fs.json), and these tools work inside their folders".into() };
    }
    if let Some(why) = spec.off_when.and_then(|f| f()) {
        return ProviderState::Off { why };
    }
    match (spec.build)(ctx) {
        Ok(Built::Provider(p)) => match ctx.host.register(p) {
            Ok(n) => {
                info!("tool host: registered {n} {} tools", spec.family);
                ProviderState::Registered { tools: n }
            }
            Err(e) => {
                warn!("tool host: could not register {} tools: {e}", spec.family);
                ProviderState::Failed { why: e }
            }
        },
        Ok(Built::Off(why)) => ProviderState::Off { why },
        Ok(Built::Done(n)) => ProviderState::Registered { tools: n },
        Ok(Built::Failed(why)) => ProviderState::Failed { why },
        Err(e) => {
            warn!("tool host: {} tools are off: {e}", spec.family);
            ProviderState::Failed { why: e }
        }
    }
}

/// What became of each family the last time [`register_all`] ran in this process (empty before the host is built).
pub fn last_report() -> Vec<ProviderReport> {
    LAST.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The body of `GET /api/v1/tools/providers`.
pub fn status_json() -> serde_json::Value {
    let reports = last_report();
    let off = reports.iter().filter(|r| matches!(r.state, ProviderState::Off { .. })).count();
    let failed = reports.iter().filter(|r| matches!(r.state, ProviderState::Failed { .. })).count();
    serde_json::json!({ "off": off, "failed": failed, "providers": reports.iter().map(ProviderReport::to_json).collect::<Vec<_>>() })
}

/// For the doctor, which does not build the host: the families a switch controls, with whether the switch leaves them out in this environment.
pub fn switch_lines() -> Vec<(&'static str, &'static str, Option<String>)> {
    PROVIDERS.iter().filter_map(|p| p.off_switch.map(|var| (p.family, var, p.off_when.and_then(|f| f())))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_mcp_adapters::{new_browser_client, new_windows_client};
    use aqua_tool_host::{AuditLedger, PolicyEngine};

    fn ctx(name: &str) -> ProviderCtx {
        let root = std::env::temp_dir().join(format!("aqua_providers_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data").join("tools")).unwrap();
        let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(aqua_tool_host::Mode::Standard)), Arc::new(AuditLedger::open(root.join("data").join("tools").join("host.audit.jsonl")))));
        let browser = Arc::new(BrowserBackend::new(&root.join("browser"), new_browser_client()));
        ProviderCtx::new(&root.join("data"), host, browser, Arc::new(Mutex::new(new_windows_client())))
    }

    #[test]
    fn the_rows_are_in_the_order_they_were_registered_before_and_each_id_is_there_once() {
        let ids: Vec<&str> = PROVIDERS.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            ["browser", "windows", "fs", "doc", "docker", "iac", "forge", "mon", "cloud", "k8s", "http", "db", "git", "ssh", "term", "check", "code", "lsp", "sys", "memory", "skills", "mcp", "flows", "reminders", "vault", "screen", "clipboard", "tools_search", "research", "cmd"]
        );
        // The families that work inside the allowed folders are the ones the old code nested in `register_fs`.
        let files: Vec<&str> = PROVIDERS.iter().filter(|p| p.needs_files).map(|p| p.id).collect();
        assert_eq!(files, ["doc", "docker", "iac", "forge", "mon", "cloud", "k8s", "http", "db", "git", "ssh", "term", "check", "code", "lsp", "cmd"]);
    }

    /// The list `agent_tools` kept before the table existed: the table must reproduce it, name for name, in order. (Copied from that file as it was.)
    #[test]
    fn the_routing_packs_the_table_makes_are_the_list_the_router_always_had() {
        let names: Vec<&str> = packs().iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            ["files", "docs", "flows", "terminal", "git", "forge", "web", "ssh", "docker", "k8s", "iac", "cloud", "database", "monitor", "system", "vault", "reminders", "memory", "screen", "clipboard", "research", "check", "code"]
        );
        let prefixes: Vec<&str> = packs().iter().flat_map(|p| p.prefixes.iter().copied()).collect();
        assert_eq!(
            prefixes,
            ["fs.", "doc.", "flows.", "term.", "git.", "forge.", "web.", "http.", "ssh.", "docker.", "k8s.", "iac.", "cloud.", "db.", "mon.", "sys.", "proc.", "vault.", "reminder.", "memory.", "screen.", "clipboard.", "research.", "check.", "project.", "code."]
        );
        let words: usize = packs().iter().map(|p| p.words.len()).sum();
        assert_eq!(words, 189, "the words of the packs are the ones the router had");
        assert_eq!(packs().iter().map(|p| p.order).collect::<Vec<_>>(), (0..23).collect::<Vec<u8>>());
    }

    #[test]
    fn a_family_the_table_leaves_out_is_off_with_the_reason_and_one_that_needs_the_files_follows_them() {
        let c = ctx("off");
        // The file tools are off (a broken fs.json): the families that work in their folders are off with that reason, nothing is registered for them, and nothing of theirs is read.
        std::fs::write(c.data_dir.join("tools").join("fs.json"), "{ nope").unwrap();
        let reports = register_with(&PROVIDERS.iter().filter(|p| matches!(p.id, "fs" | "git" | "term")).copied().collect::<Vec<_>>(), &c);
        assert!(matches!(&reports[0].state, ProviderState::Off { why } if why.contains("fs.json cannot be used: line 1, column 3")), "{:?}", reports[0].state);
        for r in &reports[1..] {
            assert!(matches!(&r.state, ProviderState::Off { why } if why.contains("the file tools are off")), "{:?}", r.state);
        }
        assert!(!c.data_dir.join("tools").join("repos.json").exists(), "the git file is not read while the file tools are off, as before");
        assert!(c.host.manifests().is_empty());
        let _ = std::fs::remove_dir_all(c.data_dir.parent().unwrap());
    }

    #[test]
    fn the_clipboard_and_the_research_are_off_with_their_switch_named_and_come_on_with_it() {
        let c = ctx("switches");
        let pick: Vec<ProviderSpec> = PROVIDERS.iter().filter(|p| matches!(p.id, "clipboard" | "research")).copied().collect();
        let off = crate::flags::with_overrides(&[("AQUA_CLIPBOARD_TOOLS", None), ("AQUA_RESEARCH", None)], || register_with(&pick, &c));
        assert!(matches!(&off[0].state, ProviderState::Off { why } if why.contains("AQUA_CLIPBOARD_TOOLS")), "{:?}", off[0].state);
        assert!(matches!(&off[1].state, ProviderState::Off { why } if why.contains("AQUA_RESEARCH")), "{:?}", off[1].state);
        assert_eq!((off[0].off_switch, off[1].off_switch), (Some("AQUA_CLIPBOARD_TOOLS"), Some("AQUA_RESEARCH")));
        assert!(!c.host.manifests().iter().any(|m| m.name.starts_with("clipboard.") || m.name.starts_with("research.")));
        let on = crate::flags::with_overrides(&[("AQUA_CLIPBOARD_TOOLS", Some("on")), ("AQUA_RESEARCH", Some("on"))], || register_with(&pick, &c));
        assert!(matches!(on[0].state, ProviderState::Registered { tools } if tools >= 1), "{:?}", on[0].state);
        assert!(matches!(on[1].state, ProviderState::Registered { tools: 1 }), "{:?}", on[1].state);
        let names: Vec<String> = c.host.manifests().into_iter().map(|m| m.name).collect();
        assert!(names.iter().any(|n| n == "clipboard.read") && names.iter().any(|n| n == "research.run"), "{names:?}");
        let _ = std::fs::remove_dir_all(c.data_dir.parent().unwrap());
    }

    /// Card OA12-04: the four language-server tools are not registered unless the switch is on and `data/tools/lsp.json` names a server: with the defaults the host has none of them, and a file that does
    /// not parse says where. Nothing is started by registering (a server is started by a call). The folders are made inside this repository's `target` folder.
    #[test]
    fn the_language_server_tools_are_registered_only_with_the_switch_and_a_server_in_the_file() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("target").join("aqua_providers_lsp").join(format!("{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data").join("tools")).unwrap();
        let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(aqua_tool_host::Mode::Standard)), Arc::new(AuditLedger::open(root.join("data").join("tools").join("host.audit.jsonl")))));
        let browser = Arc::new(BrowserBackend::new(&root.join("browser"), new_browser_client()));
        let c = ProviderCtx::new(&root.join("data"), host, browser, Arc::new(Mutex::new(new_windows_client())));
        let pick = |ids: &[&str]| -> Vec<ProviderSpec> { PROVIDERS.iter().filter(|p| ids.contains(&p.id)).copied().collect() };
        let lsp_tools = |c: &ProviderCtx| c.host.manifests().into_iter().map(|m| m.name).filter(|n| matches!(n.as_str(), "code.definition" | "code.references" | "code.hover" | "code.document_symbols")).count();
        let exe = if cfg!(windows) { "C:/tools/rust-analyzer/rust-analyzer.exe" } else { "/opt/tools/rust-analyzer" };
        let file = c.data_dir.join("tools").join("lsp.json");
        let with_a_server = format!(r#"{{"servers": {{"rust": {{"command": "{exe}", "sha256": "{}"}}}}}}"#, "ab".repeat(32));
        std::fs::write(&file, &with_a_server).unwrap();
        // The switch is off: the file is not even read, and nothing is registered.
        let off = crate::flags::with_overrides(&[("AQUA_LSP", None)], || register_with(&pick(&["fs", "lsp"]), &c));
        assert!(matches!(off[0].state, ProviderState::Registered { .. }), "{:?}", off[0].state);
        assert!(matches!(&off[1].state, ProviderState::Off { why } if why.contains("AQUA_LSP")), "{:?}", off[1].state);
        assert_eq!(off[1].off_switch, Some("AQUA_LSP"));
        assert_eq!(lsp_tools(&c), 0);
        // On with no server named: off, with the reason.
        std::fs::write(&file, r#"{"servers": {}}"#).unwrap();
        let empty = crate::flags::with_overrides(&[("AQUA_LSP", Some("on"))], || register_with(&pick(&["lsp"]), &c));
        assert!(matches!(&empty[0].state, ProviderState::Off { why } if why.contains("names no language server")), "{:?}", empty[0].state);
        // A file that does not parse: the family fails, and says where.
        std::fs::write(&file, "{\n  \"servers\": {\n    \"rust\" {}\n  }\n}").unwrap();
        let broken = crate::flags::with_overrides(&[("AQUA_LSP", Some("on"))], || register_with(&pick(&["lsp"]), &c));
        assert!(matches!(&broken[0].state, ProviderState::Failed { why } if why.contains("data/tools/lsp.json") && why.contains("line 3")), "{:?}", broken[0].state);
        assert_eq!(lsp_tools(&c), 0);
        // On, with a server: the four tools are registered, and offered while the switch stays on.
        std::fs::write(&file, &with_a_server).unwrap();
        let on = crate::flags::with_overrides(&[("AQUA_LSP", Some("on"))], || {
            let r = register_with(&pick(&["lsp"]), &c);
            assert_eq!(lsp_tools(&c), 4, "offered with the switch on");
            r
        });
        assert!(matches!(on[0].state, ProviderState::Registered { tools: 4 }), "{:?}", on[0].state);
        assert_eq!(lsp_tools(&c), 0, "and not listed when the switch is off again");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_new_family_is_one_row() {
        fn build_dummy(_ctx: &ProviderCtx) -> Result<Built, String> {
            Ok(Built::Provider(Arc::new(crate::tool_search::ToolSearchProvider)))
        }
        fn broken(_ctx: &ProviderCtx) -> Result<Built, String> {
            Err("its program was not found".into())
        }
        let c = ctx("dummy");
        let rows = [ProviderSpec::new("dummy", "dummy", build_dummy).pack(PackSpec { order: 99, name: "dummy", prefixes: &["tools."], words: &["dummy"] }), ProviderSpec::new("broken", "broken", broken)];
        let reports = register_with(&rows, &c);
        assert_eq!(reports[0].state, ProviderState::Registered { tools: 1 });
        assert_eq!(reports[0].pack, Some("dummy"));
        assert_eq!(reports[1].state, ProviderState::Failed { why: "its program was not found".into() });
        assert!(c.host.manifests().iter().any(|m| m.name == "tools.search"));
        // The same tool twice is a failure of the second row, said with the host's own words.
        let again = register_with(&rows[..1], &c);
        assert!(matches!(&again[0].state, ProviderState::Failed { why } if why.contains("already registered")), "{:?}", again[0].state);
        let json = reports[1].to_json();
        assert_eq!((json["state"].as_str(), json["why"].as_str(), json["tools"].is_null()), (Some("failed"), Some("its program was not found"), true));
        let _ = std::fs::remove_dir_all(c.data_dir.parent().unwrap());
    }

    #[test]
    fn the_status_lists_every_row_with_a_state() {
        let c = ctx("status");
        let reports = register_with(&PROVIDERS.iter().filter(|p| matches!(p.id, "sys" | "clipboard")).copied().collect::<Vec<_>>(), &c);
        let json: Vec<serde_json::Value> = reports.iter().map(ProviderReport::to_json).collect();
        assert_eq!(json[0]["id"], "sys");
        assert_eq!(json[0]["state"], "registered");
        assert!(json[0]["tools"].as_u64().unwrap() >= 6);
        assert_eq!(json[0]["pack"], "system");
        let _ = std::fs::remove_dir_all(c.data_dir.parent().unwrap());
    }

    /// The size of a page of a file comes from the function the build was given (the program gives the one that reads the window of the model: `core/src/main.rs`), because this module
    /// may not name `modules` (the layering test). A number the person wrote in `fs.json` is kept, and with no function the file tools keep their default.
    #[test]
    fn a_page_of_a_file_is_sized_by_the_function_the_build_was_given_and_a_number_in_fs_json_is_kept() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static ASKED: AtomicUsize = AtomicUsize::new(0);
        fn window_of_a_small_model() -> usize {
            ASKED.fetch_add(1, Ordering::SeqCst);
            777
        }
        let fs: Vec<ProviderSpec> = PROVIDERS.iter().filter(|p| p.id == "fs").copied().collect();
        // Builds the file tools over a folder of this test with the `view_chars` that `fs.json` is written with, and says what the guard ended up with.
        let built = |name: &str, in_fs_json: Option<usize>, source: Option<ViewCharsSource>| -> usize {
            // The helper makes a browser client, which only looks for a file of the checkout (it reads nothing of the real data folder); everything below is given a folder of its own.
            let mut c = crate::runtime::data_tripwire::allow_real_data(|| ctx(name));
            let root = c.data_dir.parent().expect("the folder of the test").to_path_buf();
            if let Some(source) = source {
                c = c.with_view_chars(source);
            }
            let ws = c.data_dir.join("workspace");
            std::fs::create_dir_all(&ws).unwrap();
            let mut cfg = aqua_native_tools::fs::guard::FsConfig::with_workspace(ws.to_string_lossy().to_string());
            if let Some(n) = in_fs_json {
                cfg.limits.view_chars = n;
            }
            std::fs::write(crate::tool_host::fs_config_path(&c.data_dir), serde_json::to_string(&cfg).unwrap()).unwrap();
            let reports = crate::runtime::data_tripwire::with_workspace_root(&root, || register_with(&fs, &c));
            assert!(matches!(reports[0].state, ProviderState::Registered { .. }), "{:?}", reports[0].state);
            let n = c.guard().expect("the file tools made their guard").limits().view_chars;
            let _ = std::fs::remove_dir_all(&root);
            n
        };
        // No function: the default of the file tools (the number for the window of 6,144 tokens), and nobody was asked.
        assert_eq!(built("view_none", None, None), aqua_native_tools::fs::guard::DEFAULT_VIEW_CHARS);
        assert_eq!(ASKED.load(Ordering::SeqCst), 0);
        // A function: the build asks it, once, and the guard has its answer.
        assert_eq!(built("view_given", None, Some(window_of_a_small_model)), 777);
        assert_eq!(ASKED.load(Ordering::SeqCst), 1, "the function is called when the file tools are built");
        // A number the person wrote in fs.json is kept: the function is not asked.
        assert_eq!(built("view_kept", Some(1234), Some(window_of_a_small_model)), 1234);
        assert_eq!(ASKED.load(Ordering::SeqCst), 1, "a number of fs.json is not replaced, so the function is not asked");
    }
}

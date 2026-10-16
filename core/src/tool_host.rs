//! Wires the tool host into aqua-core: builds it, registers providers, and bridges approvals to
//! the confirmation prompt the UI already shows.

use aqua_mcp_adapters::browser::{BrowserBackend, ConfirmKind, Decision as UiDecision};
use aqua_mcp_adapters::StdioMcpClient;
use aqua_native_tools::cloud::CloudConfig;
use aqua_native_tools::db::DbConfig;
use aqua_native_tools::docker::DockerConfig;
use aqua_native_tools::forge::ForgeConfig;
use aqua_native_tools::fs::{FsConfig, PathGuard, Trash};
use aqua_native_tools::git::GitConfig;
use aqua_native_tools::http::HttpConfig;
use aqua_native_tools::iac::IacConfig;
use aqua_native_tools::k8s::K8sConfig;
use aqua_native_tools::mon::MonConfig;
use aqua_native_tools::secrets::CredentialManager;
use aqua_native_tools::ssh::SshConfig;
use aqua_native_tools::term::{ExecRules, TermConfig};
use aqua_tool_host::{
    ApprovalOutcome, ApprovalRequest, ApprovalScope, Approver, AuditLedger, Mode, PolicyEngine, ProviderFuture, RuleSet,
    ToolHost,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};

/// Shows a host approval request as the same confirmation prompt browser actions use, so the
/// UI, the API (`/api/v1/browser/confirmations`) and any later voice confirmation all work.
pub struct BrokerApprover {
    backend: Arc<BrowserBackend>,
    /// What the person typed with a "no" (card OA10-03, AG-11), by the id of the host's question, until the host takes it ([`Approver::denial_reason`]).
    reasons: Mutex<Vec<(String, String)>>,
}

impl BrokerApprover {
    pub fn new(backend: Arc<BrowserBackend>) -> Self {
        Self { backend, reasons: Mutex::new(Vec::new()) }
    }
}

impl Approver for BrokerApprover {
    fn denial_reason(&self, approval_id: &str) -> Option<String> {
        let mut reasons = self.reasons.lock().unwrap_or_else(|e| e.into_inner());
        let at = reasons.iter().position(|(id, _)| id == approval_id)?;
        Some(reasons.remove(at).1)
    }

    fn request(&self, req: ApprovalRequest) -> ProviderFuture<'_, ApprovalOutcome> {
        let backend = self.backend.clone();
        Box::pin(async move {
            let wait = Duration::from_secs(req.expires_in_secs);
            let approval_id = req.id.clone();
            // An unattended run (workflows, practice) refuses every approval on the spot. The flag is
            // per thread and the broker waits on a different one, so it is read here and passed on.
            let unattended = aqua_mcp_adapters::browser::is_unattended();
            // The broker blocks its thread until someone answers, so it gets its own.
            let answer = tokio::task::spawn_blocking(move || {
                let _unattended = unattended.then(aqua_mcp_adapters::browser::unattended);
                let classes: Vec<String> = req.classes.iter().map(|c| format!("{c:?}").to_uppercase().replace("EXTERNALEFFECT", "EXTERNAL_EFFECT")).collect();
                let why = aqua_mcp_adapters::browser::ConfirmWhy { risk: req.why.risk.clone(), rule: req.why.rule.clone(), data: req.why.data.clone(), preview: req.preview.clone(), run_grant: req.run_grant.clone() };
                // The question is listed with the run it is about (`/browser/confirmations?run=`).
                let _run = aqua_mcp_adapters::browser::asking_for_run(&req.agent_run_id);
                let (asked, decision) = backend.broker().request_explained(ConfirmKind::Tool, &req.tool, req.summary.clone(), None, None, req.always_ask, classes, why, wait);
                // The words the person typed with a "no", if they did: taken here, where the answer is, and handed to the host when it asks for them.
                let said = (decision == UiDecision::Deny).then(|| backend.broker().take_reason(&asked.request_id)).flatten();
                // Whether the yes carried the run option (card OA6-05): only the route of the app puts the mark, and only on a question that offered it.
                let run = decision == UiDecision::Allow && backend.broker().take_run_scope(&asked.request_id);
                (decision, said, run)
            })
            .await;
            match answer {
                Ok((UiDecision::Allow, _, true)) => ApprovalOutcome::Approved(ApprovalScope::Run),
                Ok((UiDecision::Allow, _, false)) => ApprovalOutcome::Approved(ApprovalScope::Once),
                Ok((UiDecision::AllowAlways, _, _)) => ApprovalOutcome::Approved(ApprovalScope::Session),
                Ok((UiDecision::Deny, said, _)) => {
                    if let Some(said) = said {
                        let mut reasons = self.reasons.lock().unwrap_or_else(|e| e.into_inner());
                        reasons.push((approval_id, said));
                        // A reason the host never asked for (the call was withdrawn) does not stay.
                        while reasons.len() > 16 {
                            reasons.remove(0);
                        }
                    }
                    ApprovalOutcome::Denied
                }
                Err(e) => {
                    warn!("approval task failed: {e}");
                    ApprovalOutcome::Denied
                }
            }
        })
    }
}

/// Where the audit signing key is kept: the Windows credential store. Tests use memory, so that they do not leave a key in the person's own store. The scenario tests of `core/tests/`
/// (the `test-support` feature, card OA2-04) build real hosts too, and are tests in this respect: they never write to the credential store either.
fn audit_key_secrets() -> Arc<dyn aqua_native_tools::secrets::Secrets> {
    #[cfg(any(test, feature = "test-support"))]
    {
        static TEST_SECRETS: std::sync::OnceLock<Arc<aqua_native_tools::secrets::MemorySecrets>> = std::sync::OnceLock::new();
        TEST_SECRETS.get_or_init(|| Arc::new(aqua_native_tools::secrets::MemorySecrets::default())).clone()
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        Arc::new(CredentialManager)
    }
}

/// The mode the host starts in: `AQUA_TOOL_MODE` = `locked` | `standard` | `developer`.
/// An unknown value locks the host rather than guessing.
pub fn mode_from_env() -> Mode {
    match std::env::var("AQUA_TOOL_MODE").ok().as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("standard") => Mode::Standard,
        Some("developer") => Mode::Developer,
        Some("locked") => Mode::Locked,
        Some(other) => {
            warn!("AQUA_TOOL_MODE='{other}' is not recognised; starting in locked mode");
            Mode::Locked
        }
    }
}

/// Editors such as Notepad and Windows PowerShell save JSON with a UTF-8 byte-order mark, which a strict JSON reader rejects. Config files are read without it.
pub fn strip_bom(text: &str) -> &str {
    aqua_tool_host::config_text::strip_bom(text)
}

// Every config file below is read by `crate::config_files::load` (card OA3-03): comments and trailing commas are accepted, a missing file is written with its default (atomically),
// and a file that cannot be used keeps the result it always had (the family it configures is off, the host is locked). The functions keep their names and results.

pub fn policy_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("policy.json")
}

/// Load `data/tools/policy.json`. A missing file is created with the defaults; a file that does
/// not parse locks the host (and is left untouched so it can be fixed). `AQUA_TOOL_MODE`, when
/// set, overrides the file's mode, except that it can never unlock a host whose file is broken.
pub fn load_policy(data_dir: &Path) -> PolicyEngine {
    let engine = match crate::config_files::load(data_dir, &crate::config_files::POLICY) {
        // Not readable or not valid: locked, whatever the environment says.
        crate::config_files::Loaded::Broken(_) => return PolicyEngine::locked(),
        crate::config_files::Loaded::Defaulted(_) => PolicyEngine::new(Mode::Standard),
        crate::config_files::Loaded::Ok(file) => {
            let engine = PolicyEngine::new(file.mode);
            engine.apply(&file);
            engine
        }
    };
    if std::env::var("AQUA_TOOL_MODE").is_ok() {
        engine.set_mode(mode_from_env());
    }
    engine
}

pub fn fs_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("fs.json")
}

/// Load `data/tools/fs.json`, writing a default (one trusted workspace folder,
/// `%USERPROFILE%/AquaWorkspace`) when it is missing. `None` means the file is broken and the
/// filesystem tools stay off: a config that cannot be read never falls back to wider access.
pub fn load_fs_config(data_dir: &Path) -> Option<FsConfig> {
    crate::config_files::load(data_dir, &crate::config_files::FS).into_option()
}

/// Aqua's own files. No file tool may read or write them, wherever the roots point.
pub fn protected_paths(data_dir: &Path) -> Vec<std::path::PathBuf> {
    // The whole data folder is Aqua's own: tokens, databases, policy, audit, recipes and models. A
    // junction or a new file name must not be a way in, so it is protected as a unit.
    let mut dbs: Vec<std::path::PathBuf> = vec![data_dir.to_path_buf()];
    // Every database file in the data folder (settings, recipes, learning, reflection…), present or future.
    dbs.extend(std::fs::read_dir(data_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| ["db", "sqlite", "sqlite3", "db-wal", "db-shm", "db-journal"].iter().any(|d| x.eq_ignore_ascii_case(d))).unwrap_or(false)));
    dbs.extend(vec![
        data_dir.join(".aqua_api_token"),
        data_dir.join(".aqua_ui_token"),
        data_dir.join("tools"),
        data_dir.join("browser"),
        data_dir.join("browser_mcp"),
        data_dir.join("models"),
        data_dir.join("aqua_core.db"),
        data_dir.join("solutions.db"),
    ]);
    dbs
}

/// The folder where the full text of long tool results is kept for this data folder (card OA5-02): `spill_dir` of `fs.json`, else a folder called `tool-output` beside the data folder, in a
/// folder of its own for each data folder (the first eight hex digits of a hash of the data folder's place), so that a second core with its own data folder never shares the first one's.
/// `None`: the person set `spill_dir` to an empty text, which means keep nothing. A relative `spill_dir` is relative to the folder that holds the data folder.
pub fn spill_dir(data_dir: &Path, configured: Option<&str>) -> Option<PathBuf> {
    let beside = data_dir.parent().map(Path::to_path_buf).unwrap_or_else(|| data_dir.to_path_buf());
    match configured.map(str::trim) {
        Some("") => None,
        Some(path) => {
            let expanded = PathBuf::from(aqua_native_tools::fs::guard::expand_env(path));
            Some(if expanded.is_absolute() { expanded } else { beside.join(expanded) })
        }
        None => {
            use sha2::{Digest, Sha256};
            let place = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf()).to_string_lossy().replace('\\', "/").to_lowercase();
            let id: String = Sha256::digest(place.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect();
            Some(beside.join("tool-output").join(id))
        }
    }
}

/// Keep the whole redacted text of every result the host cuts for the model, in [`spill_dir`], and show that folder to the file tools as the read-only root `@out` (card OA5-02). Both
/// halves or neither: the host is only given the store when the guard accepted the folder, so a pointer line never names a file the file tools cannot open. On success: the folder.
/// On `Err`: why nothing is kept (the switch is off, `spill_dir` is empty, the folder is not usable); a cut result is then cut for good, as it was.
pub fn install_spill(host: &ToolHost, guard: &PathGuard, data_dir: &Path) -> Result<PathBuf, String> {
    if !crate::flags::is_on(&crate::flags::TOOL_OUTPUT_SPILL) {
        return Err("AQUA_TOOL_OUTPUT_SPILL is off".into());
    }
    let dir = spill_dir(data_dir, guard.config().spill_dir.as_deref()).ok_or_else(|| "spill_dir in fs.json is empty".to_string())?;
    let shown = guard.set_out_root(&dir)?;
    let store = aqua_tool_host::SpillStore::open(&shown).map_err(|e| format!("{}: {e}", shown.display()))?;
    store.set_free_space_probe(Arc::new(aqua_native_tools::sys::free_bytes_at));
    let swept = store.sweep();
    if swept.removed > 0 {
        info!("kept tool output: removed {} old file(s), {} bytes", swept.removed, swept.bytes);
    }
    let store = Arc::new(store);
    // After that every hour; a thread that panics is started again (and the failure is recorded).
    let hourly = store.clone();
    // A job of the registry (`jobs`): it promises a sign of life every hour, and the shutdown cuts its wait short.
    let every = Duration::from_secs(60 * 60);
    if let Err(e) = crate::jobs::spawn_supervised(crate::jobs::JobKind::Maintenance, "aqua-spill-sweep", Some(every), crate::supervise::Backoff::STANDARD, move |job| {
        while job.sleep(every) {
            hourly.sweep();
        }
    }) {
        warn!("kept tool output: the hourly clean-up could not start ({e}); old files are removed when Aqua starts");
    }
    host.set_spill(Some(store));
    Ok(shown)
}

pub fn docker_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("docker.json")
}

/// Load `data/tools/docker.json`. A missing file is created with the defaults; a broken one turns
/// `docker.*` off. Docker itself is optional: without a `docker` program the tools stay hidden.
pub fn load_docker_config(data_dir: &Path) -> Option<DockerConfig> {
    crate::config_files::load(data_dir, &crate::config_files::DOCKER).into_option()
}

pub fn iac_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("iac.json")
}

/// Load `data/tools/iac.json`. A missing file is created with no folders, so nothing can be planned until you list one. A broken file turns `iac.*` off.
pub fn load_iac_config(data_dir: &Path) -> Option<IacConfig> {
    crate::config_files::load(data_dir, &crate::config_files::IAC).into_option()
}

pub fn cloud_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("cloud.json")
}

/// Load `data/tools/cloud.json`. A missing file is created with no provider enabled. A broken file turns `cloud.*` off.
pub fn load_cloud_config(data_dir: &Path) -> Option<CloudConfig> {
    crate::config_files::load(data_dir, &crate::config_files::CLOUD).into_option()
}

pub fn mon_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("mon.json")
}

/// Load `data/tools/mon.json`. A missing file is created with no checks. A broken file turns `mon.*` off.
pub fn load_mon_config(data_dir: &Path) -> Option<MonConfig> {
    crate::config_files::load(data_dir, &crate::config_files::MON).into_option()
}

pub fn forge_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("forge.json")
}

/// Load `data/tools/forge.json`. A missing file is created with no accounts. A broken file turns `forge.*` off.
pub fn load_forge_config(data_dir: &Path) -> Option<ForgeConfig> {
    crate::config_files::load(data_dir, &crate::config_files::FORGE).into_option()
}

pub fn k8s_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("k8s.json")
}

/// Load `data/tools/k8s.json`. A missing file is created with no clusters (so nothing can be
/// reached until you list one); a broken one turns `k8s.*` off.
pub fn load_k8s_config(data_dir: &Path) -> Option<K8sConfig> {
    crate::config_files::load(data_dir, &crate::config_files::K8S).into_option()
}

pub fn http_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("http.json")
}

/// Load `data/tools/http.json` (allow list, auth profiles, search engine). A missing file is created
/// with the defaults; a broken one, or one that could put a secret in the file, turns `http.*` and
/// `web.*` off. The file names secrets; it never contains them.
pub fn load_http_config(data_dir: &Path) -> Option<HttpConfig> {
    crate::config_files::load(data_dir, &crate::config_files::HTTP).into_option()
}

pub fn db_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("db.json")
}

/// Load `data/tools/db.json` (the databases the model may use, by profile name). A missing file is
/// created with no profiles; a broken one turns `db.*` off.
pub fn load_db_config(data_dir: &Path) -> Option<DbConfig> {
    crate::config_files::load(data_dir, &crate::config_files::DB).into_option()
}

pub fn term_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("term.json")
}

pub fn term_rules_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("term_rules.json")
}

/// Load `data/tools/term_rules.json`, the person's own rules for commands (see `ExecRules`). A missing file means no rules; a wrong one
/// turns `term.*` off until it is fixed, so a typo never leaves a command less guarded than meant.
pub fn load_term_rules(data_dir: &Path) -> Option<ExecRules> {
    crate::config_files::load(data_dir, &crate::config_files::TERM_RULES).into_option()
}

/// Load `data/tools/term.json` (which folders programs may run from, and the limits). A missing
/// file is created with the defaults; a broken one turns `term.*` off.
pub fn load_term_config(data_dir: &Path) -> Option<TermConfig> {
    crate::config_files::load(data_dir, &crate::config_files::TERM).into_option()
}

pub fn git_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("repos.json")
}

/// Load `data/tools/repos.json` (the repositories the model may use, by alias). A missing file is
/// created with no repositories; a broken one turns `git.*` off. Each repository must sit inside
/// a filesystem root, which `GitProvider::new` checks. A file with no repository is enough: the
/// provider also finds the repositories inside the roots (`"discover"`, card OA7-02), looking again
/// whenever the roots change, so nothing has to be told when `fs.allow_folder` adds a folder.
pub fn load_git_config(data_dir: &Path) -> Option<GitConfig> {
    crate::config_files::load(data_dir, &crate::config_files::GIT).into_option()
}

pub fn ssh_config_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("ssh.json")
}

/// Where the person's pattern rules are (card OA8-04, task SE-20). The file is the person's to make: nothing writes it.
pub fn permission_rules_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("tools").join("permissions.json")
}

/// The rules in `data/tools/permissions.json`: none when the file is not there, `RuleSet::broken` when it cannot be used. **The rules can only ask or refuse, never allow**: they come
/// after the policy and its hard-deny list, the strictest matching rule wins, and a file that says `allow` is a wrong file (see `aqua_tool_host::rules`). A file that cannot be used does
/// not drop its rules: every call that is not a read asks until it is fixed, with the mistake in the question. The file is read once, at start.
pub fn load_permission_rules(data_dir: &Path) -> RuleSet {
    match crate::config_files::load(data_dir, &crate::config_files::PERMISSIONS) {
        crate::config_files::Loaded::Ok(rules) | crate::config_files::Loaded::Defaulted(rules) => {
            if !rules.is_empty() {
                info!("permission rules: {} rule(s) from {}; they can only ask or refuse", rules.len(), permission_rules_path(data_dir).display());
            }
            rules
        }
        crate::config_files::Loaded::Broken(e) => {
            warn!("permission rules {} cannot be used ({}); every call that is not a read asks until it is fixed", permission_rules_path(data_dir).display(), e.detail());
            RuleSet::broken(e.detail())
        }
    }
}

/// Reads the rules file and gives its rules to `policy`. How many rules are in force (0 for none, and for a file that could not be used, which asks about every change instead).
pub fn install_permission_rules(policy: &PolicyEngine, data_dir: &Path) -> usize {
    let rules = load_permission_rules(data_dir);
    let n = rules.len();
    policy.set_rules(rules);
    n
}

/// Every config file the core reads by hand (`config_files::files`), where it is in `data_dir`: the files that say what a tool may reach. The watch of the protected files lists them
/// (`protected_file::protected_files`, card OA8-04, task SE-10), so a file added to the table of config files is watched without anyone remembering to list it there.
pub fn config_file_paths(data_dir: &Path) -> Vec<PathBuf> {
    crate::config_files::files(data_dir)
}

/// Load `data/tools/ssh.json` (the machines the model may reach). A missing file is created with
/// no hosts; a broken or invalid one turns `ssh.*` off.
pub fn load_ssh_config(data_dir: &Path) -> Option<SshConfig> {
    crate::config_files::load(data_dir, &crate::config_files::SSH).into_option()
}

/// The file guard the filesystem and terminal tools share, kept so "full access" can add the project folder to it (and take it
/// away) while Aqua runs. Empty when the file tools are off.
pub static PROJECT_GUARD: std::sync::OnceLock<Arc<PathGuard>> = std::sync::OnceLock::new();

#[cfg(all(feature = "test-support", not(test)))]
thread_local! {
    /// The file guard of the host this thread built last (see [`project_guard`]).
    static THIS_THREADS_GUARD: std::cell::RefCell<Option<Arc<PathGuard>>> = const { std::cell::RefCell::new(None) };
}

/// The file guard the agent loop reads the project's folders from: the one the file tools were set up with ([`PROJECT_GUARD`], set once, by the first host a process builds). The scenario
/// tests of `core/tests/` (the `test-support` feature, card OA2-04) build a host for each scenario, in a folder of its own, so for them it is the guard of the host their thread built last;
/// without it the second scenario would be told that the project is the first one's folder, which is gone. Nothing else is changed: a build without the feature reads the static.
pub fn project_guard() -> Option<Arc<PathGuard>> {
    #[cfg(all(feature = "test-support", not(test)))]
    if let Some(own) = THIS_THREADS_GUARD.with(|g| g.borrow().clone()) {
        return Some(own);
    }
    PROJECT_GUARD.get().cloned()
}

/// A host was built on this thread with `guard` (the scenario tests; see [`project_guard`]).
#[cfg(all(feature = "test-support", not(test)))]
pub(crate) fn note_guard_of_this_thread(guard: &Arc<PathGuard>) {
    THIS_THREADS_GUARD.with(|g| *g.borrow_mut() = Some(guard.clone()));
}

/// The undo store of the file tools (what `fs.delete` and overwrites move aside), kept so it can be emptied from the API. Empty when
/// the file tools are off.
pub static TRASH: std::sync::OnceLock<Arc<Trash>> = std::sync::OnceLock::new();

pub fn build_tool_host(data_dir: &Path, browser: Arc<BrowserBackend>, windows: Arc<Mutex<StdioMcpClient>>) -> Arc<ToolHost> {
    let policy = Arc::new(load_policy(data_dir));
    // The person's own pattern rules (`data/tools/permissions.json`, card OA8-04): they can only ask or refuse, after everything the policy decides. A file that cannot be used makes every
    // change ask until it is fixed. No file, no rules.
    install_permission_rules(&policy, data_dir);
    let audit = Arc::new(AuditLedger::open(data_dir.join("tools").join("host.audit.jsonl")));
    // Opening an app or a page by voice goes round the host pipeline; those actions write their own lines into this same ledger, with the run that is on the thread (if any).
    aqua_tool_host::install_direct_ledger(audit.clone());
    aqua_tool_host::set_direct_run_source(crate::agent_tools::current_run);
    // The ledger's tree heads are signed with a key held by the credential store (see `audit_key`). Without one the ledger is still chained.
    match crate::audit_key::load_or_create(audit_key_secrets().as_ref(), data_dir) {
        Ok(signer) => {
            info!("tool host audit ledger: tree heads are signed with key {}", aqua_tool_host::HeadSigner::key_id(&signer));
            audit.set_head_signer(Arc::new(signer));
        }
        Err(e) => warn!("tool host audit ledger: no signing key ({e}); the ledger is chained but its heads are not signed"),
    }
    // The chain and the signed heads together (card R6-12): the chain alone cannot tell a ledger that was rewritten consistently, the heads can, for every line that was signed.
    let check = audit.check(&crate::audit_key::known_public_keys(data_dir));
    if !check.problems.is_empty() {
        warn!("tool host audit ledger FAILED verification: {}", check.problems.join("; "));
    } else if check.lines == 0 {
        info!("tool host audit ledger is new");
    } else {
        info!(
            "tool host audit ledger verified ({} records; {} signed heads cover {} of them; {} not yet signed)",
            check.lines, check.heads, check.covered_lines, check.unsealed_lines
        );
    }
    // What each MCP server says its tools are is compared with the record in `data/tools/mcp_lock.json` every time it starts.
    aqua_mcp_adapters::tool_lock::set_lock_path(data_dir.join("tools").join("mcp_lock.json"));
    let host = Arc::new(ToolHost::new(policy, audit));
    // Tokens that already exist are masked from the start (the gateway registers ones it creates later).
    for f in [".aqua_api_token", ".aqua_ui_token"] {
        if let Ok(t) = std::fs::read_to_string(data_dir.join(f)) {
            host.register_secret(t.trim());
        }
    }
    host.set_approver(Arc::new(BrokerApprover::new(browser.clone())));
    // (The live output of a call that prints as it runs, card OA7-04, is wired in `main.rs` with `agent_runs::live::source()`, not here: this module may not name `agent_runs`.)
    // The person's own checks around a call: they can only tighten, and a wrong file makes everything but a read ask until it is fixed. Beside them a watch on the tokens and
    // the permission files (card R6-10): it labels them against programs of Low integrity, and if one changes while an approved program runs, every call that is not a read
    // asks until Aqua is restarted (decision D-17).
    let person_hooks = crate::hooks::for_host(data_dir);
    let watch = Arc::new(crate::protected_file::Watch::start(data_dir));
    // Beside them, the hooks of the project a run works in, when the person has trusted that project's pack as code (card OA11-01; `hooks::PackAware`): they can only tighten as well.
    // The person's hooks are kept where a reload can put the file's hooks in their place, when that cannot loosen anything (card OA11-03; `config_reload`).
    let pack_aware = Arc::new(crate::hooks::PackAware::new(person_hooks));
    crate::hooks::install_reloadable(pack_aware.clone());
    let with_pack: Arc<dyn aqua_tool_host::Hooks> = pack_aware;
    let chain: Arc<dyn aqua_tool_host::Hooks> = Arc::new(aqua_tool_host::hooks::Watched::new(Some(with_pack), watch));
    host.set_hooks(chain.clone());
    // The core tells the same hooks of the events that are not calls (the start and the end of a run, the words a person typed).
    crate::hooks::install_chain(chain);
    // What the hooks are told about where the run is, which folders a hook may be told to run in, and that a secret given to a hook is masked in everything the host shows and writes.
    host.set_project_resolver(Arc::new(|run| crate::project_context::project_dir_of_run(run).map(|p| p.to_string_lossy().to_string())));
    crate::hooks::set_cwd_guard(Arc::new(|real| {
        PROJECT_GUARD.get().is_some_and(|g| g.roots().iter().any(|(_, root, _)| aqua_native_tools::fs::guard::within(real, &std::fs::canonicalize(root).unwrap_or_else(|_| root.clone()))))
    }));
    let masking = host.clone();
    crate::hooks::set_secret_sink(Arc::new(move |secret| masking.register_secret(secret)));
    // MCP servers the person added start inside the MXC container only if they chose that (`mcp_tier` in term.json); by default this does nothing.
    crate::mcp_sandbox::install(data_dir);
    // Every family of tools is a row of `crate::tool_providers::PROVIDERS`, in the order they were registered in before the table; what became of each (registered, off and why, failed and why) is
    // kept for `GET /api/v1/tools/providers` and the doctor.
    let ctx = crate::tool_providers::ProviderCtx::new(data_dir, host.clone(), browser, windows);
    crate::tool_providers::register_all(&ctx);
    // With `AQUA_CONFIG_WATCH=on` a job looks at hooks.json, mcp.json and the skills and reads one again when it changed (card OA11-03); without it this does nothing.
    crate::config_reload::start_watch(data_dir);
    host
}

/// The key that makes the record of trusted MCP servers (`mcp_trusted.json`) something that whoever can only write the file cannot forge: 32 random bytes in the credential store,
/// made the first time they are needed (card R6-16; the code is `audit_key::trust_key`, which the project packs use too).
///
/// The answer says whether the key was made now: only at that start is a record in the first format converted (see `ServerManager::set_trust_key`).
pub(crate) fn mcp_trust_key() -> Option<(Vec<u8>, bool)> {
    crate::audit_key::trust_key(audit_key_secrets().as_ref(), true)
}

/// A key a test has put in place of the one of the credential store (`None`: the real one).
static PACK_KEY_FOR_TEST: std::sync::Mutex<Option<Vec<u8>>> = std::sync::Mutex::new(None);

/// The key that signs the trust records of the project packs (`project_pack`): the one of the MCP servers. `create` is true only for the act of trusting something; a look at what is
/// trusted never makes a key. `None` when there is none: nothing is trusted.
pub fn pack_trust_key(create: bool) -> Option<Vec<u8>> {
    if let Some(key) = PACK_KEY_FOR_TEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone() {
        return Some(key);
    }
    crate::audit_key::trust_key(audit_key_secrets().as_ref(), create).map(|(key, _)| key)
}

/// For a test that is not inside this crate (it is built without `cfg(test)`, so it would otherwise read and write the credential store of the machine it runs on).
#[doc(hidden)]
pub fn set_pack_trust_key_for_test(key: Option<Vec<u8>>) {
    *PACK_KEY_FOR_TEST.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = key;
}

/// The servers of `data/tools/mcp.json` (see `aqua_mcp_adapters::user_servers`), or why the file is wrong. `None` before the tool host is built. (They are kept by `mcp_exposure`, which
/// says what a run is shown of them, and to which this module hands them: card OA11-03.)
pub fn mcp_servers() -> Option<&'static Result<Arc<aqua_mcp_adapters::user_servers::ServerManager>, String>> {
    crate::mcp_exposure::servers()
}

/// Read `data/tools/mcp.json` and start, in the background, the servers the person trusted. Nothing else is started.
pub(crate) fn register_user_mcp(host: &Arc<ToolHost>, data_dir: &Path) {
    let resolve: aqua_mcp_adapters::user_servers::SecretResolver = Arc::new(|name: &str| {
        use aqua_native_tools::secrets::Secrets;
        CredentialManager.get(name).ok().flatten()
    });
    let opened = aqua_mcp_adapters::user_servers::ServerManager::open(&data_dir.join("tools"), host.clone(), resolve);
    // Which runs a server is for is looked up by the core (it knows the projects and the agents), and a remote server (an `url`) is spoken to through the tool host's HTTP engine, only with
    // `AQUA_MCP_REMOTE=on` (card OA11-03). Set whether or not the file lists a server: the servers of a repository's pack come later.
    crate::mcp_exposure::set_keys(crate::mcp_exposure::Keys { pack: pack_trust_key, mcp: mcp_trust_key });
    if let Ok(m) = &opened {
        m.set_run_info(crate::mcp_exposure::run_info_fn());
        m.set_remote_connector(crate::mcp_remote::connector(data_dir.to_path_buf(), load_http_config));
    }
    match &opened {
        Ok(m) if m.is_empty() => {}
        Ok(m) => {
            info!("MCP servers added by the user: {} (started only when trusted)", m.len());
            // The record of trust is keyed (card R6-16): without the key (no credential store) it stays in the first format and says so.
            match mcp_trust_key() {
                Some((key, key_is_new)) => m.set_trust_key(key, key_is_new),
                None => warn!("MCP servers: no key for the record of trust could be read or made, so it stays in its first format (a plain fingerprint, which a file writer can forge)"),
            }
            // The servers are started one after the other on a thread of the job registry (`jobs`): it is listed, and the shutdown stops it before the next server is started.
            let starting = m.clone();
            if let Err(e) = crate::jobs::spawn(crate::jobs::JobKind::Maintenance, "mcp-start", None, move |job| {
                starting.start_trusted_now(&|| job.cancelled());
                Ok(())
            }) {
                warn!("could not start the thread that starts MCP servers: {e}");
            }
        }
        Err(e) => warn!("mcp.json is wrong ({e}): none of its servers is used until it is fixed"),
    }
    // The file is read once more by the loader that every config file goes through, so that `GET /api/v1/config/status` and the doctor know it (it writes nothing).
    let _ = crate::config_files::load(data_dir, &crate::config_files::MCP);
    let _ = crate::mcp_exposure::set_servers(opened);
}

#[cfg(test)]
mod permission_rules_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_mcp_adapters::{new_browser_client, new_windows_client};
    use aqua_tool_host::{Caller, ErrorCode, RequestContext, ToolRequest};
    use serde_json::json;

    fn backend(name: &str) -> Arc<BrowserBackend> {
        let root = std::env::temp_dir().join(format!("aqua_core_th_{name}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        Arc::new(BrowserBackend::new(&root, new_browser_client()))
    }

    fn approval(tool: &str, always: bool) -> ApprovalRequest {
        ApprovalRequest {
            id: "apr_1".into(),
            agent_run_id: "run_1".into(),
            request_id: "req_1".into(),
            session_id: "ses".into(),
            tool: tool.into(),
            summary: "Press Ctrl+Enter in chrome.exe".into(),
            target: "Shortcut".into(),
            classes: vec![aqua_tool_host::PermClass::Write, aqua_tool_host::PermClass::ExternalEffect],
            digest: "d".into(),
            always_ask: always,
            expires_in_secs: 5,
            why: aqua_tool_host::ApprovalWhy { risk: "This changes something.".into(), rule: "an external action asks".into(), data: vec!["Target: Shortcut".into()] },
            preview: None,
            run_grant: None,
        }
    }

    async fn answer_next(b: &Arc<BrowserBackend>, decision: UiDecision) {
        for _ in 0..200 {
            if let Some(p) = b.broker().pending().first() {
                assert!(b.broker().resolve(&p.request_id, decision));
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no confirmation request appeared");
    }

    #[tokio::test]
    async fn approvals_appear_as_ui_confirmations_and_map_back() {
        let b = backend("approve");
        let approver = BrokerApprover::new(b.clone());
        for (ui, want) in [
            (UiDecision::Allow, ApprovalOutcome::Approved(ApprovalScope::Once)),
            (UiDecision::AllowAlways, ApprovalOutcome::Approved(ApprovalScope::Session)),
            (UiDecision::Deny, ApprovalOutcome::Denied),
        ] {
            let waiting = approver.request(approval("win.shortcut", false));
            let (got, _) = tokio::join!(waiting, answer_next(&b, ui));
            assert_eq!(got, want);
        }
        // The prompt carries the exact action and says when it will ask every time.
        let waiting = approver.request(approval("win.shortcut", true));
        let (_, seen) = tokio::join!(waiting, async {
            for _ in 0..200 {
                if let Some(p) = b.broker().pending().first().cloned() {
                    b.broker().resolve(&p.request_id, UiDecision::Deny);
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("no request");
        });
        assert_eq!(seen.tool, "win.shortcut");
        assert!(seen.reason.contains("Press Ctrl+Enter in chrome.exe"));
        assert!(seen.always_ask, "the prompt knows this one is asked every time, so it can hide 'for this session'");
        assert_eq!(seen.classes, vec!["WRITE".to_string(), "EXTERNAL_EFFECT".to_string()]);
        // And the explanation: what could go wrong, the rule that made it ask, and what is involved.
        assert_eq!((seen.risk.as_str(), seen.rule.as_str(), seen.data.as_slice()), ("This changes something.", "an external action asks", &["Target: Shortcut".to_string()][..]));
    }

    /// Card OA6-04: the preview of a change goes with the question to the person, as it is listed for the app (`GET /api/v1/browser/confirmations`) and written into the run's log, and a
    /// question with none lists none.
    #[tokio::test]
    async fn the_preview_of_a_change_goes_with_the_question_to_the_app() {
        let b = backend("preview");
        let approver = BrokerApprover::new(b.clone());
        let mut question = approval("fs.edit", false);
        question.preview = Some("--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n-old\n+new".into());
        let waiting = approver.request(question);
        let (_, seen) = tokio::join!(waiting, async {
            for _ in 0..200 {
                if let Some(p) = b.broker().pending().first().cloned() {
                    b.broker().resolve(&p.request_id, UiDecision::Deny);
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("no request");
        });
        assert_eq!(seen.preview.as_deref(), Some("--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n-old\n+new"));
        // As the list of questions sends it: a field of the question, plain text.
        let listed = serde_json::to_value(&seen).unwrap();
        assert_eq!(listed["preview"], "--- a.txt\n+++ a.txt\n@@ -1 +1 @@\n-old\n+new");
        // A question with none has no such field at all, so that an app written before it sees the same JSON as it did.
        let waiting = approver.request(approval("win.shortcut", false));
        let (_, seen) = tokio::join!(waiting, async {
            for _ in 0..200 {
                if let Some(p) = b.broker().pending().first().cloned() {
                    b.broker().resolve(&p.request_id, UiDecision::Deny);
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("no request");
        });
        assert_eq!(seen.preview, None);
        assert!(serde_json::to_value(&seen).unwrap().get("preview").is_none());
    }

    /// What a workflow run sees: its thread is unattended, the approval is requested from an async
    /// call on that thread, and nobody is there to answer. It must be refused at once, with no prompt
    /// shown (it once waited out the full timeout, because the broker's thread did not know).
    #[test]
    fn an_unattended_thread_is_refused_at_once_and_no_prompt_is_shown() {
        let b = backend("unattended");
        let approver = BrokerApprover::new(b.clone());
        let started = std::time::Instant::now();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let got = {
            let _guard = aqua_mcp_adapters::browser::unattended();
            rt.block_on(approver.request(approval("term.run", false)))
        };
        assert_eq!(got, ApprovalOutcome::Denied);
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
        assert!(b.broker().pending().is_empty(), "nothing was shown to the user");
        // A person's own thread is unaffected: it still waits for an answer.
        let waiting = rt.block_on(async {
            let (got, _) = tokio::join!(approver.request(approval("term.run", false)), answer_next(&b, UiDecision::Allow));
            got
        });
        assert_eq!(waiting, ApprovalOutcome::Approved(ApprovalScope::Once));
    }

    /// Card OA4-05: `project.profile` and `check.run` are on offer only when `AQUA_CHECK_TOOLS=on`; they are one row of the table of providers after the terminal tools, which they run through, and are
    /// registered either way (the reference and the status list them), unavailable and so not shown to the agent and not callable until the switch is on.
    #[test]
    fn the_check_tools_are_on_offer_only_when_their_switch_is_on() {
        let built = |name: &str, switch: &str| {
            let dir = std::env::temp_dir().join(format!("aqua_core_th_check_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("tools")).unwrap();
            let cfg = FsConfig::with_workspace(dir.join("workspace").to_string_lossy().to_string());
            std::fs::write(fs_config_path(&dir), serde_json::to_string(&cfg).unwrap()).unwrap();
            let windows = Arc::new(Mutex::new(new_windows_client()));
            let host = build_tool_host(&dir, backend(name), windows);
            let report = crate::tool_providers::last_report().into_iter().find(|r| r.id == "check").map(|r| r.state);
            // The switch is asked each time the tools are listed, so one host answers both ways.
            let listed = |on: &str| crate::flags::with_overrides(&[("AQUA_CHECK_TOOLS", Some(on))], || host.manifests().into_iter().map(|m| m.name).collect::<Vec<String>>());
            let catalog: Vec<String> = host.catalog().into_iter().filter(|(_, why)| why.is_some()).map(|(m, _)| m.name).collect();
            let called = crate::flags::with_overrides(&[("AQUA_CHECK_TOOLS", Some(switch))], || host.call_blocking(aqua_tool_host::ToolRequest { tool: "project.profile".into(), arguments: serde_json::json!({}), context: aqua_tool_host::RequestContext::new(aqua_tool_host::Caller::Agent) }));
            (listed("off"), listed("on"), catalog, report, called)
        };
        let (off, on, unavailable_off, state, called_off) = built("off", "off");
        assert!(!off.iter().any(|n| n == "check.run" || n == "project.profile"), "{off:?}");
        assert!(on.iter().any(|n| n == "check.run") && on.iter().any(|n| n == "project.profile"), "{on:?}");
        assert!(matches!(state, Some(crate::tool_providers::ProviderState::Registered { tools: 2 })), "{state:?}");
        assert!(unavailable_off.iter().any(|n| n == "check.run") && unavailable_off.iter().any(|n| n == "project.profile"), "the catalog lists them with the reason they are unavailable: {unavailable_off:?}");
        assert!(!called_off.ok && called_off.text.contains("UNAVAILABLE"), "a call while the switch is off is refused as unavailable: {}", called_off.text);
        for names in [&off, &on] {
            assert_eq!(names.iter().filter(|n| n.starts_with("term.")).count(), 6, "the terminal tools are what they were");
        }
        // And the call goes through once the switch is on (the profile of the folder the host was built over, which holds no project: the answer is the host's own not-found, not unavailable).
        let (_, _, _, _, called_on) = built("on", "on");
        assert!(!called_on.text.contains("UNAVAILABLE"), "{}", called_on.text);
    }

    /// Card OA12-02: `code.outline` and `code.symbols` are on offer only when `AQUA_CODE_TOOLS=on`; one row of the table of providers after the check tools, registered either way (the reference
    /// and the status list them), unavailable and so not shown to the agent and not callable until the switch is on, and then a read through the guard of the host's own folders.
    #[test]
    fn the_code_tools_are_on_offer_only_when_their_switch_is_on() {
        let built = |name: &str, switch: &str| {
            let dir = std::env::temp_dir().join(format!("aqua_core_th_code_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("tools")).unwrap();
            // The workspace is a folder of its own: what is inside Aqua's data folder belongs to Aqua and the file tools do not read it.
            let workspace = std::env::temp_dir().join(format!("aqua_core_th_code_ws_{name}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&workspace);
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::write(workspace.join("a.rs"), "pub struct A;\n\nimpl A {\n    pub fn go(&self) {}\n}\n").unwrap();
            let cfg = FsConfig::with_workspace(workspace.to_string_lossy().to_string());
            std::fs::write(fs_config_path(&dir), serde_json::to_string(&cfg).unwrap()).unwrap();
            let windows = Arc::new(Mutex::new(new_windows_client()));
            let host = build_tool_host(&dir, backend(name), windows);
            let report = crate::tool_providers::last_report().into_iter().find(|r| r.id == "code").map(|r| r.state);
            // The switch is asked each time the tools are listed, so one host answers both ways.
            let listed = |on: &str| crate::flags::with_overrides(&[("AQUA_CODE_TOOLS", Some(on))], || host.manifests().into_iter().map(|m| m.name).collect::<Vec<String>>());
            let catalog: Vec<String> = host.catalog().into_iter().filter(|(_, why)| why.is_some()).map(|(m, _)| m.name).collect();
            let called = crate::flags::with_overrides(&[("AQUA_CODE_TOOLS", Some(switch))], || host.call_blocking(aqua_tool_host::ToolRequest { tool: "code.outline".into(), arguments: serde_json::json!({"path": "@ws/a.rs"}), context: aqua_tool_host::RequestContext::new(aqua_tool_host::Caller::Agent) }));
            // Card OA12-03: the map has a switch of its own, `AQUA_REPO_MAP`, and the other tools of the provider have `AQUA_CODE_TOOLS`.
            let only_the_map = crate::flags::with_overrides(&[("AQUA_CODE_TOOLS", Some("off")), ("AQUA_REPO_MAP", Some("on"))], || host.manifests().into_iter().map(|m| m.name).filter(|n| n.starts_with("code.")).collect::<Vec<String>>());
            let all_but_the_map = crate::flags::with_overrides(&[("AQUA_CODE_TOOLS", Some("on")), ("AQUA_REPO_MAP", Some("off"))], || host.manifests().into_iter().map(|m| m.name).filter(|n| n.starts_with("code.")).collect::<Vec<String>>());
            assert_eq!(only_the_map, ["code.map"], "the map is on offer with its own switch alone");
            assert_eq!(all_but_the_map, ["code.deps", "code.outline", "code.refs", "code.symbols"], "the four are on offer with theirs alone");
            (listed("off"), listed("on"), catalog, report, called)
        };
        let (off, on, unavailable_off, state, called_off) = built("off", "off");
        assert!(!off.iter().any(|n| n.starts_with("code.")), "{off:?}");
        assert!(["code.outline", "code.symbols", "code.deps", "code.refs"].iter().all(|t| on.iter().any(|n| n == t)) && !on.iter().any(|n| n == "code.map"), "{on:?}");
        assert!(matches!(state, Some(crate::tool_providers::ProviderState::Registered { tools: 5 })), "the five tools are registered whatever the switches say: {state:?}");
        assert!(unavailable_off.iter().any(|n| n == "code.outline") && unavailable_off.iter().any(|n| n == "code.symbols"), "the catalog lists them with the reason they are unavailable: {unavailable_off:?}");
        assert!(!called_off.ok && called_off.text.contains("UNAVAILABLE"), "a call while the switch is off is refused as unavailable: {}", called_off.text);
        // Nothing else of the host changed with them.
        assert_eq!(off.iter().filter(|n| n.starts_with("fs.")).count(), on.iter().filter(|n| n.starts_with("fs.")).count());
        // On, the call goes through the host (the same guard, the same fence) and answers with the outline of the file.
        let (_, _, _, _, called_on) = built("on", "on");
        assert!(called_on.ok, "{}", called_on.text);
        assert!(called_on.text.contains("L1 pub struct A;") && called_on.text.contains("L3-5 impl A") && called_on.text.contains("L4   pub fn go(&self) {}"), "{}", called_on.text);
    }

    #[tokio::test]
    async fn the_built_host_lists_only_desktop_ui_tools_and_refuses_shells() {
        let dir = std::env::temp_dir().join(format!("aqua_core_th_host_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let windows = Arc::new(Mutex::new(new_windows_client()));
        // Keep the workspace root inside the temp folder so the test leaves nothing in the home folder.
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        let cfg = FsConfig::with_workspace(dir.join("workspace").to_string_lossy().to_string());
        std::fs::write(fs_config_path(&dir), serde_json::to_string(&cfg).unwrap()).unwrap();
        let host = build_tool_host(&dir, backend("host"), windows.clone());
        let names: Vec<String> = host.manifests().into_iter().map(|m| m.name).collect();
        assert_eq!(names.iter().filter(|n| n.starts_with("win.")).count(), 14);
        assert_eq!(names.iter().filter(|n| n.starts_with("fs.")).count(), 16, "the fourteen file tools, fs.allow_folder and fs.revoke_folder");
        assert_eq!(names.iter().filter(|n| n.starts_with("memory.")).count(), 5, "memory.search, list, remember, forget and consolidate");
        assert_eq!(names.iter().filter(|n| n.starts_with("flows.")).count(), 3, "flows.list, flows.run and flows.status");
        assert_eq!(names.iter().filter(|n| n.starts_with("browser.")).count(), 20);
        assert_eq!(names.iter().filter(|n| n.starts_with("term.")).count(), 6, "term.run, shell, start, output, stop and sessions");
        assert_eq!(names.iter().filter(|n| n.starts_with("ssh.")).count(), 6);
        assert_eq!(names.iter().filter(|n| n.starts_with("git.")).count(), 25, "the twenty-four git tools and git.repos");
        assert_eq!(names.iter().filter(|n| n.starts_with("db.")).count(), 5);
        assert_eq!(names.iter().filter(|n| n.starts_with("http.") || n.starts_with("web.")).count(), 5, "web.fetch, web.read, web.search (Wikipedia needs no key), http.request and web.scrape");
        assert_eq!(names.iter().filter(|n| n.starts_with("sys.") || n.starts_with("proc.")).count(), 6);
        for banned in ["powershell", "registry", "win.filesystem", "process", "clipboard"] {
            assert!(!names.iter().any(|n| n.contains(banned)), "{banned}");
        }
        // Launching a shell is refused before the server is ever started.
        let mut ctx = RequestContext::new(Caller::Api);
        ctx.session_id = "t".into();
        let res = host.call(ToolRequest { tool: "win.app".into(), arguments: json!({"mode": "launch", "name": "PowerShell"}), context: ctx.clone() }).await;
        assert_eq!(res.error.as_ref().unwrap().code, ErrorCode::PermissionDenied);
        let res = host.call(ToolRequest { tool: "win.app".into(), arguments: json!({"mode": "launch", "executable": "cmd.exe"}), context: ctx.clone() }).await;
        assert_eq!(res.error.as_ref().unwrap().code, ErrorCode::InvalidArguments);
        let res = host.call(ToolRequest { tool: "powershell".into(), arguments: json!({}), context: ctx }).await;
        assert_eq!(res.error.as_ref().unwrap().code, ErrorCode::NotFound);
        assert!(!windows.lock().unwrap().is_running(), "no refusal may start the server");
        // Each refusal is in the audit ledger, and the ledger verifies.
        assert_eq!(host.audit().verify().unwrap(), 3);
    }

    /// Card OA7-02 (TM-05), the acceptance: on a fresh data folder nobody wrote a `repos.json` in, `git.status` in the repository of the workspace answers, and `git.commit` in it asks
    /// (here nobody is there to answer, so it is refused at once, with the reason).
    #[tokio::test]
    async fn on_a_fresh_data_folder_git_status_in_the_workspace_repository_answers_and_git_commit_asks() {
        let base = std::env::temp_dir().join(format!("aqua_core_git_fresh_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // The data folder is Aqua's own and no file tool may name it: the workspace is beside it, as it is on a real machine.
        let dir = base.join("data");
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        let workspace = base.join("workspace");
        let proj = workspace.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").args(["-c", "user.name=Setup", "-c", "user.email=setup@example.com", "-C"]).arg(&proj).args(args).output().expect("git runs");
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(proj.join("a.txt"), "alpha
").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "first commit"]);
        std::fs::write(proj.join("b.txt"), "beta
").unwrap();
        let cfg = FsConfig::with_workspace(workspace.to_string_lossy().to_string());
        std::fs::write(fs_config_path(&dir), serde_json::to_string(&cfg).unwrap()).unwrap();
        let host = build_tool_host(&dir, backend("git_fresh"), Arc::new(Mutex::new(new_windows_client())));
        let mut ctx = RequestContext::new(Caller::Agent);
        ctx.session_id = "t".into();
        let st = host.call(ToolRequest { tool: "git.status".into(), arguments: json!({}), context: ctx.clone() }).await;
        assert!(st.ok && st.text.contains("## main") && st.text.contains("b.txt"), "git.status works with no repos.json: {}", st.text);
        let listed = host.call(ToolRequest { tool: "git.repos".into(), arguments: json!({}), context: ctx.clone() }).await;
        assert!(listed.ok && listed.data["repos"][0]["alias"] == "proj" && listed.data["repos"][0]["trusted"] == false, "{}", listed.text);
        // The brief of a coding run names it.
        let line = crate::modules::project_brief::git_line(&listed.data).expect("a line");
        assert_eq!(line, "Git: proj on main, 1 changed file");
        // A commit in it asks; nobody is there to answer, so it is refused at once and nothing is committed.
        ctx.no_one_to_ask = true;
        let c = host.call(ToolRequest { tool: "git.commit".into(), arguments: json!({"message": "add b", "paths": ["b.txt"]}), context: ctx }).await;
        assert!(!c.ok, "git.commit asks, and nobody answered: {}", c.text);
        assert!(matches!(c.error.as_ref().unwrap().code, ErrorCode::ApprovalDenied | ErrorCode::ApprovalRequired), "{:?}", c.error);
        let log = std::process::Command::new("git").args(["-C"]).arg(&proj).args(["rev-list", "--count", "HEAD"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "1");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn k8s_iac_and_cloud_start_with_nothing_reachable_and_broken_configs_turn_them_off() {
        let dir = std::env::temp_dir().join(format!("aqua_core_devopscfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_k8s_config(&dir).unwrap().contexts.is_empty(), "no cluster until you list one");
        assert!(load_iac_config(&dir).unwrap().dirs.is_empty(), "no terraform folder until you list one");
        assert!(load_cloud_config(&dir).unwrap().enabled.is_empty(), "no cloud provider until you enable one");
        for f in ["k8s.json", "iac.json", "cloud.json"] {
            assert!(dir.join("tools").join(f).exists(), "{f} written with the defaults");
        }
        std::fs::write(k8s_config_path(&dir), r#"{"contexts":[{"name":"x","namespaces":["*"]}]}"#).unwrap();
        assert!(load_k8s_config(&dir).is_none(), "a wildcard namespace is rejected");
        std::fs::write(iac_config_path(&dir), r#"{"env_secrets":{"TF_CLI_ARGS":"a/b"}}"#).unwrap();
        assert!(load_iac_config(&dir).is_none(), "TF_CLI_ARGS cannot be set from a secret");
        std::fs::write(cloud_config_path(&dir), r#"{"enabled":["aws"],"env":{"aws":{"AWS_ENDPOINT_URL":"http://x"}}}"#).unwrap();
        assert!(load_cloud_config(&dir).is_none(), "an endpoint override is rejected");
        assert!(load_mon_config(&dir).unwrap().probes.is_empty(), "no probes until you list them");
        assert!(load_forge_config(&dir).unwrap().accounts.is_empty(), "no forge account until you list one");
        std::fs::write(forge_config_path(&dir), r#"{"accounts":[{"name":"gh","kind":"github","repos":["*"]}]}"#).unwrap();
        assert!(load_forge_config(&dir).is_none(), "a repository pattern of * is rejected");
        std::fs::write(forge_config_path(&dir), r#"{"accounts":[{"name":"gh","kind":"github","repos":["me/*"],"token":"ghp_x"}]}"#).unwrap();
        assert!(load_forge_config(&dir).is_none(), "a token in the file is rejected");
        std::fs::write(mon_config_path(&dir), r#"{"probes":[{"name":"a","kind":"tcp","host":"h/x","port":1}]}"#).unwrap();
        assert!(load_mon_config(&dir).is_none(), "a probe host with a path is rejected");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_files_saved_with_a_byte_order_mark_are_read() {
        let dir = std::env::temp_dir().join(format!("aqua_core_bom_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        // What Notepad and Windows PowerShell write: a UTF-8 BOM before the JSON.
        std::fs::write(db_config_path(&dir), "\u{feff}{\"profiles\": []}").unwrap();
        assert!(load_db_config(&dir).is_some(), "db.json with a BOM");
        std::fs::write(mon_config_path(&dir), "\u{feff}{\"probes\": []}").unwrap();
        assert!(load_mon_config(&dir).is_some(), "mon.json with a BOM");
        std::fs::write(policy_path(&dir), "\u{feff}{\"mode\": \"developer\"}").unwrap();
        assert_eq!(load_policy(&dir).mode(), aqua_tool_host::Mode::Developer, "policy.json with a BOM keeps its mode instead of locking the host");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_docker_config_turns_the_docker_tools_off_and_docker_stays_optional() {
        let dir = std::env::temp_dir().join(format!("aqua_core_dockercfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = load_docker_config(&dir).expect("default config");
        assert!(!cfg.manage_all && cfg.allow_images.is_empty() && docker_config_path(&dir).exists());
        std::fs::write(docker_config_path(&dir), r#"{"privileged": true}"#).unwrap();
        assert!(load_docker_config(&dir).is_none(), "unknown fields are rejected");
        std::fs::write(docker_config_path(&dir), r#"{"allow_images": ["*"]}"#).unwrap();
        assert!(load_docker_config(&dir).is_none(), "an allow-everything image pattern is rejected");
        assert!(std::fs::read_to_string(docker_config_path(&dir)).unwrap().contains("\"*\""), "left untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_http_config_or_one_holding_a_secret_turns_the_web_tools_off() {
        let dir = std::env::temp_dir().join(format!("aqua_core_httpcfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = load_http_config(&dir).expect("default config");
        assert!(!cfg.allow.is_empty() && cfg.auth.is_empty() && http_config_path(&dir).exists());
        // A token typed into the file is rejected, not used.
        std::fs::write(http_config_path(&dir), r#"{"auth": {"g": {"kind": "bearer", "secret": "github/token", "hosts": ["api.github.com"], "token": "ghp_realtoken"}}}"#).unwrap();
        assert!(load_http_config(&dir).is_none());
        // A credential that could be sent anywhere is rejected.
        std::fs::write(http_config_path(&dir), r#"{"auth": {"g": {"kind": "bearer", "secret": "github/token", "hosts": ["*"]}}}"#).unwrap();
        assert!(load_http_config(&dir).is_none());
        assert!(std::fs::read_to_string(http_config_path(&dir)).unwrap().contains("\"*\""), "left untouched");
        // A proper profile names the secret and the hosts it may go to.
        std::fs::write(http_config_path(&dir), r#"{"auth": {"g": {"kind": "bearer", "secret": "github/token", "hosts": ["api.github.com"]}}}"#).unwrap();
        assert!(load_http_config(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_or_unsupported_database_config_turns_the_database_tools_off() {
        let dir = std::env::temp_dir().join(format!("aqua_core_dbcfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = load_db_config(&dir).expect("default config");
        assert!(cfg.profiles.is_empty() && db_config_path(&dir).exists());
        std::fs::write(db_config_path(&dir), r#"{"profiles": [{"alias": "a", "kind": "sqlite", "path": "x.db", "password": "p"}]}"#).unwrap();
        assert!(load_db_config(&dir).is_none(), "credentials in the config file are rejected: secrets are referenced, never stored here");
        // Every .db file next to the settings is protected from the file tools.
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("reflection.db"), "").unwrap();
        std::fs::write(data.join("notes.txt"), "").unwrap();
        let protected = protected_paths(&data);
        assert!(protected.iter().any(|p| p.ends_with("reflection.db")) && !protected.iter().any(|p| p.ends_with("notes.txt")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_git_config_turns_the_git_tools_off_and_a_repo_outside_the_roots_is_refused() {
        let dir = std::env::temp_dir().join(format!("aqua_core_gitcfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = load_git_config(&dir).expect("default config");
        assert!(cfg.repos.is_empty() && git_config_path(&dir).exists());
        std::fs::write(git_config_path(&dir), r#"{"repos": [{"alias": "a", "path": "x", "hooks": "enabled"}]}"#).unwrap();
        assert!(load_git_config(&dir).is_none(), "unknown fields (such as enabling hooks) are rejected");
        assert!(std::fs::read_to_string(git_config_path(&dir)).unwrap().contains("hooks"), "left untouched");
        // A repository outside the filesystem roots: the git tools stay off, the rest of the host works.
        let ws = dir.join("workspace");
        let outside = dir.join("elsewhere");
        std::fs::create_dir_all(outside.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(fs_config_path(&dir), serde_json::to_string(&FsConfig::with_workspace(ws.to_string_lossy().to_string())).unwrap()).unwrap();
        std::fs::write(git_config_path(&dir), serde_json::json!({"repos": [{"alias": "a", "path": outside.to_string_lossy()}]}).to_string()).unwrap();
        let host = build_tool_host(&dir, backend("gitout"), Arc::new(Mutex::new(new_windows_client())));
        let names: Vec<String> = host.manifests().into_iter().map(|m| m.name).collect();
        assert!(!names.iter().any(|n| n.starts_with("git.")) && names.iter().any(|n| n.starts_with("fs.")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_or_unsafe_ssh_config_turns_the_ssh_tools_off_and_is_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("aqua_core_sshcfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = load_ssh_config(&dir).expect("default config");
        assert!(cfg.hosts.is_empty() && ssh_config_path(&dir).exists());
        // Unknown fields (a password, agent forwarding) are rejected.
        std::fs::write(ssh_config_path(&dir), r#"{"hosts": [{"alias": "a", "host": "h.example", "user": "u", "password": "x"}]}"#).unwrap();
        assert!(load_ssh_config(&dir).is_none());
        // A host that ssh could read as an option is rejected.
        std::fs::write(ssh_config_path(&dir), r#"{"hosts": [{"alias": "a", "host": "-oProxyCommand=calc", "user": "u"}]}"#).unwrap();
        assert!(load_ssh_config(&dir).is_none());
        assert!(std::fs::read_to_string(ssh_config_path(&dir)).unwrap().contains("ProxyCommand"), "left untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_term_config_turns_the_terminal_tools_off_and_is_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("aqua_core_termcfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = load_term_config(&dir).expect("default config");
        assert_eq!(cfg, TermConfig::default());
        std::fs::write(term_config_path(&dir), r#"{"path_dirs": ["C:/"], "allow_shells": true}"#).unwrap();
        assert!(load_term_config(&dir).is_none(), "unknown fields are rejected");
        assert!(std::fs::read_to_string(term_config_path(&dir)).unwrap().contains("allow_shells"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_term_rules_file_turns_the_terminal_tools_off_and_a_missing_one_means_no_rules() {
        let dir = std::env::temp_dir().join(format!("aqua_core_termrules_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_term_rules(&dir).expect("a missing file is no rules").is_empty());
        assert!(!term_rules_path(&dir).exists(), "nothing is written for the person");
        std::fs::create_dir_all(term_rules_path(&dir).parent().unwrap()).unwrap();
        std::fs::write(term_rules_path(&dir), r#"{"rules": [{"pattern": "git push", "decision": "forbid", "examples": {"match": ["git push origin"]}}]}"#).unwrap();
        assert_eq!(load_term_rules(&dir).expect("a good file").len(), 1);
        // A rule that would loosen, or whose example does not match, turns the terminal tools off and the file is left as it was.
        for bad in [
            r#"{"rules": [{"pattern": "git push", "decision": "allow", "examples": {"match": ["git push"]}}]}"#,
            r#"{"rules": [{"pattern": "git push", "decision": "forbid", "examples": {"match": ["git pull"]}}]}"#,
            "{",
        ] {
            std::fs::write(term_rules_path(&dir), bad).unwrap();
            assert!(load_term_rules(&dir).is_none(), "{bad}");
            assert_eq!(std::fs::read_to_string(term_rules_path(&dir)).unwrap(), bad, "left untouched");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_fs_config_turns_the_filesystem_tools_off_and_is_not_overwritten() {
        let dir = std::env::temp_dir().join(format!("aqua_core_fscfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Missing: a default is written, with one trusted root.
        let cfg = load_fs_config(&dir).expect("default config");
        assert_eq!(cfg.roots.len(), 1);
        assert!(cfg.roots[0].trusted && fs_config_path(&dir).exists());
        // Broken (unknown field, so a typo cannot widen access): tools off, file untouched.
        std::fs::write(fs_config_path(&dir), r#"{"roots": [], "allow_everything": true}"#).unwrap();
        assert!(load_fs_config(&dir).is_none());
        assert!(std::fs::read_to_string(fs_config_path(&dir)).unwrap().contains("allow_everything"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn file_tools_cannot_reach_aquas_own_tokens_policy_or_audit_log() {
        let dir = std::env::temp_dir().join(format!("aqua_core_fsprot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join(".aqua_ui_token"), "secret-ui-token").unwrap();
        std::fs::write(dir.join("tools").join("policy.json"), "{}").unwrap();
        // The workspace root is the data folder itself, the worst case.
        let cfg = FsConfig::with_workspace(dir.to_string_lossy().to_string());
        std::fs::write(fs_config_path(&dir), serde_json::to_string(&cfg).unwrap()).unwrap();
        let host = build_tool_host(&dir, backend("fsprot"), Arc::new(Mutex::new(new_windows_client())));
        for path in [".aqua_ui_token", "tools/policy.json", "tools/host.audit.jsonl", "tools/trash"] {
            let mut ctx = RequestContext::new(Caller::Ui);
            ctx.session_id = "t".into();
            let res = host.call(ToolRequest { tool: "fs.read".into(), arguments: json!({"path": path}), context: ctx }).await;
            assert_eq!(res.error.as_ref().map(|e| e.code), Some(ErrorCode::PermissionDenied), "{path}: {}", res.text);
            assert!(!res.text.contains("secret-ui-token"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_policy_file_is_created_loaded_and_locks_when_broken() {
        let dir = std::env::temp_dir().join(format!("aqua_core_policy_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("AQUA_TOOL_MODE");
        // No file: defaults are written and standard applies.
        assert_eq!(load_policy(&dir).mode(), Mode::Standard);
        assert!(policy_path(&dir).exists());
        // A valid file is honoured.
        std::fs::write(policy_path(&dir), r#"{"mode": "developer", "hard_deny": ["win.*"]}"#).unwrap();
        let e = load_policy(&dir);
        assert_eq!(e.mode(), Mode::Developer);
        assert_eq!(e.snapshot().hard_deny, vec!["win.*".to_string()]);
        // A broken file locks the host and is not overwritten.
        std::fs::write(policy_path(&dir), "{ nope").unwrap();
        assert_eq!(load_policy(&dir).mode(), Mode::Locked);
        assert_eq!(std::fs::read_to_string(policy_path(&dir)).unwrap(), "{ nope");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unknown_mode_locks_the_host() {
        std::env::set_var("AQUA_TOOL_MODE", "yolo");
        assert_eq!(mode_from_env(), Mode::Locked);
        std::env::set_var("AQUA_TOOL_MODE", "Developer");
        assert_eq!(mode_from_env(), Mode::Developer);
        std::env::remove_var("AQUA_TOOL_MODE");
        assert_eq!(mode_from_env(), Mode::Standard);
    }

    // ---- card OA5-02: where the full text of long tool output is kept ----

    fn spill_rig(name: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("aqua_core_spill_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let data = base.join("data");
        std::fs::create_dir_all(data.join("tools")).unwrap();
        (base, data)
    }

    #[test]
    fn second_core_gets_its_own_spill_folder() {
        let (base, data) = spill_rig("second");
        let second = base.join("sandbox").join("data");
        let beside_data_too = base.join("data_8790");
        std::fs::create_dir_all(&second).unwrap();
        std::fs::create_dir_all(&beside_data_too).unwrap();
        let first_dir = spill_dir(&data, None).unwrap();
        // Beside the data folder, in a folder called tool-output, and in a folder of its own for this data folder.
        assert_eq!(first_dir.parent().unwrap(), base.join("tool-output"));
        let id = first_dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
        assert_eq!(spill_dir(&data, None).unwrap(), first_dir, "the same data folder, the same place, every time");
        assert_eq!(spill_dir(&base.join("data").join("..").join("data"), None).unwrap().file_name(), first_dir.file_name(), "however the data folder is spelt");
        // A second core with its own data folder, elsewhere or in the same folder as the first, never shares it.
        let elsewhere = spill_dir(&second, None).unwrap();
        let next_door = spill_dir(&beside_data_too, None).unwrap();
        assert_eq!(elsewhere.parent().unwrap(), base.join("sandbox").join("tool-output"));
        assert_eq!(next_door.parent().unwrap(), first_dir.parent().unwrap());
        assert_ne!(next_door, first_dir, "two data folders in one folder get two spill folders");
        // The person's own choice: an empty text keeps nothing, a full path is used, a relative one is beside the data folder, and %VARIABLES% are expanded.
        assert_eq!(spill_dir(&data, Some("  ")), None);
        assert_eq!(spill_dir(&data, Some("D:\\kept\\out")).unwrap(), PathBuf::from("D:\\kept\\out"));
        assert_eq!(spill_dir(&data, Some("kept")).unwrap(), base.join("kept"));
        if let Ok(profile) = std::env::var("USERPROFILE") {
            assert_eq!(spill_dir(&data, Some("%USERPROFILE%\\kept")).unwrap(), PathBuf::from(format!("{profile}\\kept")));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    fn guard_for(cfg: FsConfig, data: &Path) -> PathGuard {
        PathGuard::new(cfg, protected_paths(data)).unwrap()
    }

    fn quiet_host(base: &Path) -> ToolHost {
        ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(base.join("audit.jsonl"))))
    }

    #[test]
    fn the_spill_is_installed_with_the_read_only_root_or_not_at_all() {
        let (base, data) = spill_rig("install");
        let host = quiet_host(&base);
        let cfg = || FsConfig::with_workspace(base.join("ws").to_string_lossy().to_string());
        // The way it is meant to be: a folder beside data/, shown as @out, with a store behind it.
        let guard = guard_for(cfg(), &data);
        let dir = install_spill(&host, &guard, &data).unwrap();
        assert!(dir.starts_with(aqua_native_tools::fs::guard::canonical(&base.join("tool-output")).unwrap()), "{}", dir.display());
        assert_eq!(guard.out_root(), Some(dir.clone()));
        let store = host.spill().expect("the host has a store");
        assert_eq!(store.dir(), dir.as_path());
        assert!(guard.resolve("@out/x.txt", aqua_native_tools::fs::Op::Write).is_err() && guard.resolve("@out/x.txt", aqua_native_tools::fs::Op::Read).is_ok());
        // The switch off: nothing is kept and the root is not there either, so no line can name a file that cannot be opened.
        let (base2, data2) = spill_rig("install_off");
        let host2 = quiet_host(&base2);
        let guard2 = guard_for(FsConfig::with_workspace(base2.join("ws").to_string_lossy().to_string()), &data2);
        let why = crate::flags::with_overrides(&[("AQUA_TOOL_OUTPUT_SPILL", Some("off"))], || install_spill(&host2, &guard2, &data2)).unwrap_err();
        assert!(why.contains("AQUA_TOOL_OUTPUT_SPILL"), "{why}");
        assert!(host2.spill().is_none() && guard2.out_root().is_none() && !base2.join("tool-output").exists());
        // An empty spill_dir in fs.json is the same.
        let mut empty = FsConfig::with_workspace(base2.join("ws").to_string_lossy().to_string());
        empty.spill_dir = Some(String::new());
        let guard3 = guard_for(empty, &data2);
        assert!(install_spill(&host2, &guard3, &data2).is_err() && host2.spill().is_none() && guard3.out_root().is_none());
        // A folder inside Aqua's own files is refused by the guard, and then the host is not given a store either.
        let mut inside = FsConfig::with_workspace(base2.join("ws").to_string_lossy().to_string());
        inside.spill_dir = Some(data2.join("kept").to_string_lossy().to_string());
        let guard4 = guard_for(inside, &data2);
        let why = install_spill(&host2, &guard4, &data2).unwrap_err();
        assert!(why.contains("Aqua's own files"), "{why}");
        assert!(host2.spill().is_none() && guard4.out_root().is_none(), "no store, no root");
        assert!(!data2.join("kept").exists(), "and the folder was not made inside Aqua's data folder");
        // A configured folder is used where the person put it.
        let mut chosen = FsConfig::with_workspace(base2.join("ws").to_string_lossy().to_string());
        chosen.spill_dir = Some(base2.join("my-kept-output").to_string_lossy().to_string());
        let guard5 = guard_for(chosen, &data2);
        let dir5 = install_spill(&host2, &guard5, &data2).unwrap();
        assert!(dir5.ends_with("my-kept-output") && host2.spill().is_some_and(|s| s.dir() == dir5.as_path()));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&base2);
    }
}

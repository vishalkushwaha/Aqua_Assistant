//! One loader for the person's config files (card OA3-03; task EX-02 of the OpenCode audit).
//!
//! Fourteen functions in `tool_host.rs` read `data/tools/*.json` with the same twenty lines each, and `hooks.rs` and the MCP adapters had two more. Each one treated a
//! missing file, an unreadable file, a file that is not JSON, a file of the wrong shape and a file a rule refuses in its own words, and said so only in a `warn!`
//! line. This module is the one copy:
//!
//! * [`load`] reads a file as a [`ConfigSpec`] describes it: comments, trailing commas and a byte-order mark are accepted ([`aqua_tool_host::config_text`]); a file larger
//!   than 1 MiB is refused; a missing file is either written with the default (atomically, with the durable writer) or read as empty; a file that cannot be used is a
//!   [`Loaded::Broken`] with a [`ConfigError`] that says where. **What a broken file does is unchanged**: the family it configures stays off, a broken `policy.json` locks the
//!   host, a broken `hooks.json` makes every call that is not a read ask. The loaders in `tool_host.rs` keep their signatures and their results; only the reading moved here.
//! * Every load leaves a record in a process-wide table, and [`status`] reads each file again (without writing anything) to say what is on disk now next to what the core
//!   loaded: that is `GET /api/v1/config/status` and the doctor. It reports paths, states and positions, **never what a file holds**.
//! * A top-level key that is not known is named with the nearest known key (`unknown key `rots`: did you mean `roots`?`). Every config type refuses unknown keys, so such a file
//!   is also broken; the hint is what makes the refusal easy to fix.
//!
//! A config file is read by the core only at start. The status says when a file has changed since it was read (`needs_restart`).

use aqua_tool_host::config_text::{self, ConfigError, ConfigErrorKind, MAX_CONFIG_BYTES};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use tracing::warn;

/// What to do when the file is not there.
pub enum Missing<T> {
    /// Use the default and write it to the file (atomically), so the person has something to edit.
    WriteDefault(fn() -> T, fn(&T) -> Option<String>),
    /// Use the default and write nothing: the file is the person's to make.
    Empty(fn() -> T),
}

impl<T: Serialize> Missing<T> {
    pub const fn write_default(make: fn() -> T) -> Self {
        Missing::WriteDefault(make, pretty::<T>)
    }
}

fn pretty<T: Serialize>(value: &T) -> Option<String> {
    serde_json::to_string_pretty(value).ok()
}

/// What a file that cannot be used turns off. The behaviour is the caller's (`None` from a loader, a locked host, hooks that ask); this says it in words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnBroken {
    /// The tools named (`docker.*`) are not offered until the file is fixed.
    FamilyOff(&'static str),
    /// The whole host is locked until the file is fixed (`policy.json`).
    HostLocked,
    /// Every call that is not a read asks until the file is fixed (`hooks.json`).
    HooksAsk,
    /// None of the MCP servers of the file is used (`mcp.json`).
    NoServers,
    /// Every call that is not a read asks until the file is fixed: its rules cannot be read, and a file that can only tighten must not be dropped (`permissions.json`, card OA8-04).
    RulesAsk,
    /// None of the model servers of the file is asked until it is fixed (`providers.json`, card OA9-04).
    NoProviders,
}

impl OnBroken {
    /// The consequence, for the status and the doctor.
    pub fn effect(self) -> String {
        match self {
            OnBroken::FamilyOff(tools) => format!("{tools} tools are off until it is fixed"),
            OnBroken::HostLocked => "the host is locked until it is fixed".into(),
            OnBroken::HooksAsk => "every call that is not a read asks until it is fixed".into(),
            OnBroken::NoServers => "none of its servers is used until it is fixed".into(),
            OnBroken::RulesAsk => "every call that is not a read asks until it is fixed (its rules are not dropped)".into(),
            OnBroken::NoProviders => "none of its model servers is asked until it is fixed".into(),
        }
    }
}

/// How one file is read.
pub struct ConfigSpec<T: 'static> {
    /// The file's name in the status (`fs.json`).
    pub name: &'static str,
    /// Where it is, from the data folder (`tools/fs.json`).
    pub rel: &'static str,
    /// How the log calls it (`filesystem config`).
    pub label: &'static str,
    /// The top-level keys that exist, for the "did you mean".
    pub known_keys: &'static [&'static str],
    pub on_missing: Missing<T>,
    pub on_broken: OnBroken,
    /// Reads the prepared text (no comments, no byte-order mark, valid JSON). A position in the error is kept.
    pub parse: fn(&str) -> Result<T, ConfigError>,
    /// A rule the typed value must satisfy (a wildcard that is not allowed, a secret in the file).
    pub validate: Option<fn(&T) -> Result<(), String>>,
}

/// The result of [`load`].
#[derive(Debug)]
pub enum Loaded<T> {
    /// The file was read and is valid.
    Ok(T),
    /// The file was not there: this is the default (written to the file or not, as the spec says).
    Defaulted(T),
    /// The file cannot be used. What that turns off is the spec's `on_broken`; the file is left as it was.
    Broken(ConfigError),
}

impl<T> Loaded<T> {
    /// The value, or `None` when the file is broken: what the loaders in `tool_host.rs` return.
    pub fn into_option(self) -> Option<T> {
        match self {
            Loaded::Ok(v) | Loaded::Defaulted(v) => Some(v),
            Loaded::Broken(_) => None,
        }
    }

    pub fn error(&self) -> Option<&ConfigError> {
        match self {
            Loaded::Broken(e) => Some(e),
            _ => None,
        }
    }
}

/// The state of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigState {
    /// Read, and valid.
    Ok,
    /// Not there; the default was written (as a load) .
    Defaulted,
    /// Not there (as a status), or not there and read as empty.
    Absent,
    /// Cannot be used.
    Broken,
}

impl ConfigState {
    pub fn word(self) -> &'static str {
        match self {
            ConfigState::Ok => "ok",
            ConfigState::Defaulted => "defaulted",
            ConfigState::Absent => "absent",
            ConfigState::Broken => "broken",
        }
    }
}

/// What reading a file found, without changing anything.
pub enum Found<T> {
    Value(T),
    Missing,
    Broken(ConfigError),
}

pub struct Inspection<T> {
    pub found: Found<T>,
    /// Notes that are not errors: an unknown key with the nearest known one.
    pub warnings: Vec<String>,
    /// A number that changes when the file's bytes do (0 when it is not there).
    pub fingerprint: u64,
}

fn unreadable(e: &std::io::Error) -> ConfigError {
    ConfigError::new(ConfigErrorKind::Unreadable, format!("could not be read: {e}"))
}

/// The file's bytes: `Ok(None)` when it is not there, an error when it is there and cannot be read or is larger than 1 MiB.
fn read_limited(path: &Path) -> Result<Option<Vec<u8>>, ConfigError> {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(&e)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|e| unreadable(&e))?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::new(ConfigErrorKind::Unreadable, format!("is larger than {} KiB, which is too much for a config file", MAX_CONFIG_BYTES / 1024)));
    }
    Ok(Some(bytes))
}

fn fingerprint_of(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// The typed parse most files use: the JSON text into `T`, with the position of an error.
pub fn parse_json<T: DeserializeOwned>(text: &str) -> Result<T, ConfigError> {
    serde_json::from_str(text).map_err(|e| ConfigError::from_serde(&e))
}

/// An error text made by a parser that returns a `String` (`not valid: ...` for the shape of the JSON, anything else for a rule): its kind and position.
pub fn from_text_error(text: &str) -> ConfigError {
    let kind = if text.starts_with("not valid:") || text.starts_with("policy file is not valid") { ConfigErrorKind::Shape } else { ConfigErrorKind::Invalid };
    ConfigError::from_message(kind, text)
}

/// Reads the file `spec` describes, and changes nothing: no default is written and nothing is recorded.
pub fn inspect<T>(data_dir: &Path, spec: &ConfigSpec<T>) -> Inspection<T> {
    let path = data_dir.join(spec.rel);
    let broken = |e: ConfigError, warnings: Vec<String>, fingerprint: u64| Inspection { found: Found::Broken(e.in_file(&path)), warnings, fingerprint };
    let bytes = match read_limited(&path) {
        Ok(Some(b)) => b,
        Ok(None) => return Inspection { found: Found::Missing, warnings: Vec::new(), fingerprint: 0 },
        Err(e) => return broken(e, Vec::new(), u64::MAX),
    };
    let fingerprint = fingerprint_of(&bytes);
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return broken(ConfigError::new(ConfigErrorKind::Unreadable, "is not valid UTF-8 text"), Vec::new(), fingerprint),
    };
    let prepared = match config_text::prepare(&text) {
        Ok(p) => p,
        Err(e) => return broken(e, Vec::new(), fingerprint),
    };
    // The JSON itself first, so that a missing comma is a syntax error with its position whatever the typed parse does with it.
    if let Err(e) = serde_json::from_str::<serde::de::IgnoredAny>(&prepared) {
        return broken(ConfigError::from_serde(&e), Vec::new(), fingerprint);
    }
    let warnings = config_text::unknown_keys(&prepared, spec.known_keys);
    let value = match (spec.parse)(&prepared) {
        Ok(v) => v,
        Err(e) => return broken(e, warnings, fingerprint),
    };
    if let Some(validate) = spec.validate {
        if let Err(why) = validate(&value) {
            return broken(ConfigError::from_message(ConfigErrorKind::Invalid, &why), warnings, fingerprint);
        }
    }
    Inspection { found: Found::Value(value), warnings, fingerprint }
}

/// What the core loaded, by path.
#[derive(Clone)]
struct Record {
    state: ConfigState,
    error: Option<ConfigError>,
    warnings: Vec<String>,
    fingerprint: u64,
    at_ms: i64,
}

/// The files this process has loaded. The core has seventeen; a throw-away data folder in a test adds one entry per file, so the table keeps the newest 1024 and drops the oldest.
static LOADED: Mutex<BTreeMap<PathBuf, Record>> = Mutex::new(BTreeMap::new());

const MAX_RECORDS: usize = 1024;

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn record(path: &Path, rec: Record) {
    insert_bounded(&mut LOADED.lock().unwrap_or_else(PoisonError::into_inner), path, rec, MAX_RECORDS);
}

/// Adds the record, first dropping the oldest one when the table is full and the file is a new one.
fn insert_bounded(table: &mut BTreeMap<PathBuf, Record>, path: &Path, rec: Record, max: usize) {
    if table.len() >= max && !table.contains_key(path) {
        if let Some(oldest) = table.iter().min_by_key(|(_, r)| r.at_ms).map(|(p, _)| p.clone()) {
            table.remove(&oldest);
        }
    }
    table.insert(path.to_path_buf(), rec);
}

fn recorded(path: &Path) -> Option<Record> {
    LOADED.lock().unwrap_or_else(PoisonError::into_inner).get(path).cloned()
}

/// What the log says about a file that cannot be used. The words are the ones the loaders always used.
fn log_broken<T>(spec: &ConfigSpec<T>, path: &Path, e: &ConfigError) {
    let detail = e.detail();
    match spec.on_broken {
        OnBroken::FamilyOff(tools) => match e.kind {
            ConfigErrorKind::Unreadable => warn!("{} {} unreadable ({detail}); {tools} tools are off", spec.label, path.display()),
            ConfigErrorKind::Invalid => warn!("{} {} invalid ({detail}); {tools} tools are off until it is fixed", spec.label, path.display()),
            _ => warn!("{} {} rejected ({detail}); {tools} tools are off until it is fixed", spec.label, path.display()),
        },
        OnBroken::HostLocked => match e.kind {
            ConfigErrorKind::Unreadable => warn!("{} {} unreadable ({detail}); host is locked", spec.label, path.display()),
            _ => warn!("{} {} rejected: {detail}. The host is LOCKED until it is fixed.", spec.label, path.display()),
        },
        // The caller says what its consequence is (`hooks::for_host`, `register_user_mcp`, `tool_host::load_permission_rules`).
        OnBroken::HooksAsk | OnBroken::NoServers | OnBroken::RulesAsk | OnBroken::NoProviders => {}
    }
}

/// Reads the file `spec` describes, as the core does at start: a missing file becomes the default (written atomically, or not, as the spec says), a file that cannot be used is
/// [`Loaded::Broken`] and is left exactly as it was. The load is recorded for [`status`].
pub fn load<T>(data_dir: &Path, spec: &ConfigSpec<T>) -> Loaded<T> {
    let path = data_dir.join(spec.rel);
    let Inspection { found, mut warnings, fingerprint } = inspect(data_dir, spec);
    let (state, error, loaded) = match found {
        Found::Value(v) => (ConfigState::Ok, None, Loaded::Ok(v)),
        Found::Missing => {
            let (value, state) = match &spec.on_missing {
                Missing::WriteDefault(make, text) => {
                    let value = make();
                    if let Some(json) = text(&value) {
                        // The folder is made, the file is written whole or not at all, and a failure does not stop the load: the default is used either way.
                        if let Err(e) = config_text::write_atomic(&path, json.as_bytes()) {
                            warn!("{} {}: the default could not be written ({e})", spec.label, path.display());
                            warnings.push(format!("the default could not be written: {e}"));
                        }
                    }
                    (value, ConfigState::Defaulted)
                }
                Missing::Empty(make) => (make(), ConfigState::Absent),
            };
            // The default is held to the same rule as a file (it always was).
            match spec.validate.map(|validate| validate(&value)) {
                Some(Err(why)) => {
                    let e = ConfigError::from_message(ConfigErrorKind::Invalid, &why).in_file(&path);
                    (ConfigState::Broken, Some(e.clone()), Loaded::Broken(e))
                }
                _ => (state, None, Loaded::Defaulted(value)),
            }
        }
        Found::Broken(e) => (ConfigState::Broken, Some(e.clone()), Loaded::Broken(e)),
    };
    if let Some(e) = &error {
        log_broken(spec, &path, e);
    }
    record(&path, Record { state, error, warnings, fingerprint, at_ms: now_ms() });
    loaded
}

// ---- the files --------------------------------------------------------------------------------------------------------------------

use aqua_native_tools::cloud::CloudConfig;
use aqua_native_tools::db::DbConfig;
use aqua_native_tools::docker::DockerConfig;
use aqua_native_tools::forge::ForgeConfig;
use aqua_native_tools::fs::FsConfig;
use aqua_native_tools::git::GitConfig;
use aqua_native_tools::http::HttpConfig;
use aqua_native_tools::iac::IacConfig;
use aqua_native_tools::k8s::K8sConfig;
use aqua_native_tools::mon::MonConfig;
use aqua_native_tools::ssh::SshConfig;
use aqua_native_tools::term::{ExecRules, TermConfig};
use aqua_tool_host::{PolicyFile, RuleFile, RuleSet};
use crate::company_brain::CompanyConfig;

fn parse_policy(text: &str) -> Result<PolicyFile, ConfigError> {
    let file: PolicyFile = parse_json(text)?;
    file.check().map_err(|why| ConfigError::from_message(ConfigErrorKind::Invalid, &why))?;
    Ok(file)
}

fn parse_rules(text: &str) -> Result<ExecRules, ConfigError> {
    ExecRules::from_text(text).map_err(|why| from_text_error(&why))
}

fn parse_permissions(text: &str) -> Result<RuleSet, ConfigError> {
    let file: RuleFile = parse_json(text)?;
    file.into_rule_set().map_err(|why| ConfigError::from_message(ConfigErrorKind::Invalid, &why))
}

fn parse_hooks(text: &str) -> Result<crate::hooks::CommandHooks, ConfigError> {
    crate::hooks::CommandHooks::from_text(text).map_err(|why| from_text_error(&why))
}

type Servers = BTreeMap<String, aqua_mcp_adapters::user_servers::ServerEntry>;

fn parse_servers(text: &str) -> Result<Servers, ConfigError> {
    aqua_mcp_adapters::user_servers::parse_file(text).map_err(|why| from_text_error(&why))
}

fn no_servers() -> Servers {
    BTreeMap::new()
}

fn default_fs() -> FsConfig {
    FsConfig::with_workspace("%USERPROFILE%/AquaWorkspace")
}

macro_rules! spec {
    ($ident:ident: $ty:ty, $name:literal, $rel:literal, $label:literal, $keys:expr, $missing:expr, $broken:expr, $parse:expr, $validate:expr) => {
        pub const $ident: ConfigSpec<$ty> = ConfigSpec { name: $name, rel: $rel, label: $label, known_keys: $keys, on_missing: $missing, on_broken: $broken, parse: $parse, validate: $validate };
    };
}

spec!(POLICY: PolicyFile, "policy.json", "tools/policy.json", "tool policy", &["mode", "limits", "hard_deny", "tool_overrides"], Missing::write_default(PolicyFile::default), OnBroken::HostLocked, parse_policy, None);
spec!(FS: FsConfig, "fs.json", "tools/fs.json", "filesystem config", &["roots", "sensitive_names", "limits", "spill_dir"], Missing::write_default(default_fs), OnBroken::FamilyOff("fs.*"), parse_json::<FsConfig>, None);
spec!(DOCKER: DockerConfig, "docker.json", "tools/docker.json", "docker config", &["docker_path", "manage_all", "allow_images", "max_memory_mb", "max_cpus", "min_host_port"], Missing::write_default(DockerConfig::default), OnBroken::FamilyOff("docker.*"), parse_json::<DockerConfig>, Some(DockerConfig::validate));
spec!(IAC: IacConfig, "iac.json", "tools/iac.json", "iac config", &["terraform_path", "dirs", "env_secrets", "max_destroy"], Missing::write_default(IacConfig::default), OnBroken::FamilyOff("iac.*"), parse_json::<IacConfig>, Some(IacConfig::validate));
spec!(CLOUD: CloudConfig, "cloud.json", "tools/cloud.json", "cloud config", &["aws_path", "azure_path", "gcloud_path", "enabled", "env", "env_secrets"], Missing::write_default(CloudConfig::default), OnBroken::FamilyOff("cloud.*"), parse_json::<CloudConfig>, Some(CloudConfig::validate));
spec!(MON: MonConfig, "mon.json", "tools/mon.json", "mon config", &["probes", "disk_free_warn_percent", "memory_free_warn_percent"], Missing::write_default(MonConfig::default), OnBroken::FamilyOff("mon.*"), parse_json::<MonConfig>, Some(MonConfig::validate));
spec!(FORGE: ForgeConfig, "forge.json", "tools/forge.json", "forge config", &["accounts"], Missing::write_default(ForgeConfig::default), OnBroken::FamilyOff("forge.*"), parse_json::<ForgeConfig>, Some(ForgeConfig::validate));
spec!(K8S: K8sConfig, "k8s.json", "tools/k8s.json", "k8s config", &["kubectl_path", "kubeconfig", "contexts", "max_replicas"], Missing::write_default(K8sConfig::default), OnBroken::FamilyOff("k8s.*"), parse_json::<K8sConfig>, Some(K8sConfig::validate));
spec!(HTTP: HttpConfig, "http.json", "tools/http.json", "http config", &["allow", "deny", "allow_private", "blocked_ports", "unknown_hosts", "auth", "search", "search_fallback", "scrape"], Missing::write_default(HttpConfig::default), OnBroken::FamilyOff("http.* and web.*"), parse_json::<HttpConfig>, Some(HttpConfig::validate));
spec!(DB: DbConfig, "db.json", "tools/db.json", "database config", &["profiles"], Missing::write_default(DbConfig::default), OnBroken::FamilyOff("db.*"), parse_json::<DbConfig>, None);
spec!(COMPANY_BRAIN: CompanyConfig, "company_brain.json", "tools/company_brain.json", "Company Brain config", &["enabled", "host", "port", "graph", "tenant_id", "user", "secret", "tls"], Missing::write_default(CompanyConfig::default), OnBroken::FamilyOff("company.*"), parse_json::<CompanyConfig>, Some(CompanyConfig::validate));
spec!(TERM_RULES: ExecRules, "term_rules.json", "tools/term_rules.json", "terminal rules", &["rules"], Missing::Empty(ExecRules::none), OnBroken::FamilyOff("term.*"), parse_rules, None);
spec!(
    TERM: TermConfig,
    "term.json",
    "tools/term.json",
    "terminal config",
    &["path_dirs", "max_sessions", "memory_mb", "max_processes", "extra_system_programs", "discover", "default_tier", "mcp_tier", "mxc"],
    Missing::write_default(TermConfig::default),
    OnBroken::FamilyOff("term.*"),
    parse_json::<TermConfig>,
    None
);
spec!(GIT: GitConfig, "repos.json", "tools/repos.json", "git config", &["repos", "git_path", "discover"], Missing::write_default(GitConfig::default), OnBroken::FamilyOff("git.*"), parse_json::<GitConfig>, None);
spec!(SSH: SshConfig, "ssh.json", "tools/ssh.json", "ssh config", &["hosts", "ssh_path", "scp_path", "keyscan_path", "keygen_path", "connect_timeout_s"], Missing::write_default(SshConfig::default), OnBroken::FamilyOff("ssh.*"), parse_json::<SshConfig>, Some(SshConfig::validate));
spec!(HOOKS: crate::hooks::CommandHooks, "hooks.json", "hooks.json", "hooks", &["hooks"], Missing::Empty(crate::hooks::CommandHooks::none), OnBroken::HooksAsk, parse_hooks, None);
spec!(MCP: Servers, "mcp.json", "tools/mcp.json", "mcp config", &["mcpServers"], Missing::Empty(no_servers), OnBroken::NoServers, parse_servers, None);
spec!(PERMISSIONS: RuleSet, "permissions.json", "tools/permissions.json", "permission rules", &["rules"], Missing::Empty(RuleSet::none), OnBroken::RulesAsk, parse_permissions, None);

// ---- the status ---------------------------------------------------------------------------------------------------------------------

/// One file, as `GET /api/v1/config/status` and the doctor tell it.
#[derive(Clone, Debug)]
pub struct ConfigReport {
    pub name: &'static str,
    /// From the data folder (`tools/fs.json`).
    pub file: &'static str,
    pub path: PathBuf,
    /// What is on disk now.
    pub state: ConfigState,
    pub error: Option<ConfigError>,
    pub warnings: Vec<String>,
    /// What using this file as it is would turn off; only for a broken file.
    pub effect: Option<String>,
    /// What this process loaded (`None`: it has not loaded the file, as when the file system tools are off and the rest of the family was not read).
    pub loaded: Option<ConfigState>,
    /// What the core loaded was a broken file: the family stays off (or the host locked) until the file is fixed and Aqua restarts.
    pub loaded_error: Option<ConfigError>,
    /// The file is not what the core read.
    pub needs_restart: bool,
}

impl ConfigReport {
    /// The report as JSON: paths, states and positions, no content.
    pub fn to_json(&self) -> serde_json::Value {
        let error = |e: &ConfigError| serde_json::json!({ "kind": e.kind.word(), "line": e.line, "column": e.column, "message": e.message });
        serde_json::json!({
            "name": self.name,
            "file": self.file,
            "path": self.path.display().to_string(),
            "state": self.state.word(),
            "error": self.error.as_ref().map(error),
            "warnings": self.warnings,
            "effect": self.effect,
            "loaded": self.loaded.map(ConfigState::word),
            "loaded_error": self.loaded_error.as_ref().map(error),
            "needs_restart": self.needs_restart,
        })
    }
}

fn report_of<T>(data_dir: &Path, spec: &ConfigSpec<T>) -> ConfigReport {
    let path = data_dir.join(spec.rel);
    let Inspection { found, warnings, fingerprint } = inspect(data_dir, spec);
    let (state, error) = match found {
        Found::Value(_) => (ConfigState::Ok, None),
        Found::Missing => (ConfigState::Absent, None),
        Found::Broken(e) => (ConfigState::Broken, Some(e)),
    };
    let rec = recorded(&path);
    ConfigReport {
        name: spec.name,
        file: spec.rel,
        effect: error.as_ref().map(|_| spec.on_broken.effect()),
        needs_restart: rec.as_ref().is_some_and(|r| r.fingerprint != fingerprint),
        loaded: rec.as_ref().map(|r| r.state),
        loaded_error: rec.as_ref().and_then(|r| r.error.clone()),
        path,
        state,
        error,
        warnings,
    }
}

/// Every config file the core reads by hand, read again now (nothing is written), next to what this process loaded.
pub fn status(data_dir: &Path) -> Vec<ConfigReport> {
    vec![
        report_of(data_dir, &POLICY),
        report_of(data_dir, &FS),
        report_of(data_dir, &TERM),
        report_of(data_dir, &TERM_RULES),
        report_of(data_dir, &GIT),
        report_of(data_dir, &SSH),
        report_of(data_dir, &DB),
        report_of(data_dir, &COMPANY_BRAIN),
        report_of(data_dir, &HTTP),
        report_of(data_dir, &DOCKER),
        report_of(data_dir, &K8S),
        report_of(data_dir, &IAC),
        report_of(data_dir, &CLOUD),
        report_of(data_dir, &FORGE),
        report_of(data_dir, &MON),
        report_of(data_dir, &HOOKS),
        report_of(data_dir, &MCP),
        report_of(data_dir, &PERMISSIONS),
    ]
}

/// Where every one of these files is, in the data folder `data_dir`: the places [`status`] reads. They are what the person edits by hand and what grants a tool its reach, so they are
/// the places the watch of the protected files looks at (`protected_file::protected_files`, card OA8-04, task SE-10).
pub fn files(data_dir: &Path) -> Vec<PathBuf> {
    [POLICY.rel, FS.rel, TERM.rel, TERM_RULES.rel, GIT.rel, SSH.rel, DB.rel, COMPANY_BRAIN.rel, HTTP.rel, DOCKER.rel, K8S.rel, IAC.rel, CLOUD.rel, FORGE.rel, MON.rel, HOOKS.rel, MCP.rel, PERMISSIONS.rel]
        .into_iter()
        .map(|rel| data_dir.join(rel))
        .collect()
}

/// The body of `GET /api/v1/config/status`.
pub fn status_json(data_dir: &Path) -> serde_json::Value {
    let reports = status(data_dir);
    let broken = reports.iter().filter(|r| r.state == ConfigState::Broken).count();
    let restart = reports.iter().filter(|r| r.needs_restart).count();
    serde_json::json!({
        "checked_at_ms": now_ms(),
        "broken": broken,
        "needs_restart": restart,
        "files": reports.iter().map(ConfigReport::to_json).collect::<Vec<_>>(),
    })
}

/// Why the last load of `path` by this process found it unusable (the doctor prints it).
pub fn last_error(path: &Path) -> Option<ConfigError> {
    recorded(path).and_then(|r| r.error)
}

/// When the file at `path` was last loaded by this process, in ms since 1970.
pub fn loaded_at_ms(path: &Path) -> Option<i64> {
    recorded(path).map(|r| r.at_ms)
}

/// The warnings the last load of `path` left (an unknown key with the nearest known one).
pub fn load_warnings(path: &Path) -> Vec<String> {
    recorded(path).map(|r| r.warnings).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Demo {
        #[serde(default)]
        count: u32,
        #[serde(default)]
        names: Vec<String>,
    }

    impl Default for Demo {
        fn default() -> Self {
            Demo { count: 3, names: vec!["a".into()] }
        }
    }

    fn check_count(d: &Demo) -> Result<(), String> {
        if d.count > 10 { Err("count must be 10 or less".into()) } else { Ok(()) }
    }

    const DEMO: ConfigSpec<Demo> = ConfigSpec {
        name: "demo.json",
        rel: "tools/demo.json",
        label: "demo config",
        known_keys: &["count", "names"],
        on_missing: Missing::write_default(Demo::default),
        on_broken: OnBroken::FamilyOff("demo.*"),
        parse: parse_json::<Demo>,
        validate: Some(check_count),
    };

    const DEMO_EMPTY: ConfigSpec<Demo> = ConfigSpec { on_missing: Missing::Empty(Demo::default), ..DEMO };

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_config_files_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn put(dir: &Path, text: &str) -> PathBuf {
        let p = dir.join("tools").join("demo.json");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn a_missing_file_is_written_with_the_default_and_reported() {
        let d = dir("missing");
        let got = load(&d, &DEMO);
        assert!(matches!(&got, Loaded::Defaulted(v) if *v == Demo::default()), "{got:?}");
        let path = d.join("tools").join("demo.json");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), serde_json::to_string_pretty(&Demo::default()).unwrap(), "exactly the default, pretty printed");
        assert_eq!(recorded(&path).unwrap().state, ConfigState::Defaulted);
        // The next load reads that file.
        assert!(matches!(load(&d, &DEMO), Loaded::Ok(v) if v == Demo::default()));
        // `Empty` writes nothing.
        let e = dir("missing_empty");
        assert!(matches!(load(&e, &DEMO_EMPTY), Loaded::Defaulted(_)));
        assert!(!e.join("tools").join("demo.json").exists());
        assert_eq!(recorded(&e.join("tools").join("demo.json")).unwrap().state, ConfigState::Absent);
        let _ = std::fs::remove_dir_all(&d);
        let _ = std::fs::remove_dir_all(&e);
    }

    #[test]
    fn a_broken_file_is_left_alone_and_says_where() {
        let d = dir("broken");
        let path = put(&d, "{\n  \"count\": 1\n  \"names\": []\n}");
        let got = load(&d, &DEMO);
        let e = got.error().expect("broken");
        assert_eq!((e.kind, e.line, e.column), (ConfigErrorKind::Syntax, Some(3), Some(3)), "{e}");
        assert!(e.file.ends_with("demo.json"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\n  \"count\": 1\n  \"names\": []\n}", "left as it was");
        assert!(got.into_option().is_none());
        // A key of the wrong shape and a rule the value breaks are told apart.
        put(&d, r#"{"count": "many"}"#);
        let e = load(&d, &DEMO).error().cloned().unwrap();
        assert_eq!(e.kind, ConfigErrorKind::Shape);
        assert!(e.message.contains("invalid type: string, expected u32") && !e.message.contains("many"), "{}", e.message);
        put(&d, r#"{"count": 11}"#);
        let e = load(&d, &DEMO).error().cloned().unwrap();
        assert_eq!((e.kind, e.message.as_str()), (ConfigErrorKind::Invalid, "count must be 10 or less"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn comments_and_trailing_commas_are_read_and_a_misspelt_key_gets_a_hint() {
        let d = dir("jsonc");
        put(&d, "\u{feff}{\n  // how many\n  \"count\": 7, /* and the names */\n  \"names\": [\"x\",],\n}\n");
        assert!(matches!(load(&d, &DEMO), Loaded::Ok(v) if v == Demo { count: 7, names: vec!["x".into()] }));
        let path = d.join("tools").join("demo.json");
        assert!(load_warnings(&path).is_empty());
        put(&d, r#"{"cuont": 7}"#);
        let got = load(&d, &DEMO);
        assert!(got.error().is_some(), "an unknown key is refused");
        assert_eq!(load_warnings(&path), vec!["unknown key `cuont`: did you mean `count`?".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_unreadable_file_is_broken_and_not_replaced() {
        let d = dir("unreadable");
        // A folder where the file should be: it is there, and it cannot be read.
        let path = d.join("tools").join("demo.json");
        std::fs::create_dir_all(&path).unwrap();
        let got = load(&d, &DEMO);
        assert_eq!(got.error().map(|e| e.kind), Some(ConfigErrorKind::Unreadable));
        assert!(path.is_dir(), "the default was not written over it");
        // Text that is not UTF-8, and a file that is too large.
        let _ = std::fs::remove_dir_all(&path);
        std::fs::write(&path, [0x7b, 0xff, 0xfe, 0x7d]).unwrap();
        assert_eq!(load(&d, &DEMO).error().map(|e| e.kind), Some(ConfigErrorKind::Unreadable));
        std::fs::write(&path, format!("{{\"names\": [\"{}\"]}}", "x".repeat(MAX_CONFIG_BYTES))).unwrap();
        let e = load(&d, &DEMO).error().cloned().unwrap();
        assert_eq!(e.kind, ConfigErrorKind::Unreadable);
        assert!(e.message.contains("larger than 1024 KiB"), "{}", e.message);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_empty_file_or_one_with_only_comments_is_broken_not_the_default() {
        let d = dir("empty");
        for text in ["", "   \n", "// nothing here\n", "/* nothing */"] {
            put(&d, text);
            assert!(load(&d, &DEMO).error().is_some(), "{text:?} must not be read as the defaults");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_status_compares_the_file_with_what_was_loaded() {
        let d = dir("status");
        put(&d, r#"{"count": 2}"#);
        let _ = load(&d, &DEMO);
        let report = report_of(&d, &DEMO);
        assert_eq!((report.state, report.loaded, report.needs_restart), (ConfigState::Ok, Some(ConfigState::Ok), false));
        // The person breaks it after the core read it: the core still has the good one, and the status says the file now is not.
        put(&d, r#"{"count": }"#);
        let report = report_of(&d, &DEMO);
        assert_eq!((report.state, report.loaded, report.needs_restart), (ConfigState::Broken, Some(ConfigState::Ok), true));
        assert_eq!(report.effect.as_deref(), Some("demo.* tools are off until it is fixed"));
        let json = report.to_json();
        assert_eq!(json["error"]["kind"], "syntax");
        assert_eq!(json["error"]["line"], 1);
        assert_eq!(json["state"], "broken");
        // What the file holds is not in the answer.
        assert!(!json.to_string().contains("count\": }"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_table_of_loads_drops_the_oldest_when_it_is_full_and_never_a_file_it_is_asked_about_again() {
        let rec = |at_ms: i64| Record { state: ConfigState::Ok, error: None, warnings: Vec::new(), fingerprint: 0, at_ms };
        let mut table = BTreeMap::new();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            insert_bounded(&mut table, Path::new(name), rec(i as i64 + 1), 3);
        }
        // A file already in the table is replaced, not counted twice.
        insert_bounded(&mut table, Path::new("a"), rec(10), 3);
        assert_eq!(table.len(), 3);
        // A new file pushes out the oldest (b, loaded at 2), and the newest stays.
        insert_bounded(&mut table, Path::new("d"), rec(11), 3);
        assert_eq!(table.keys().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>(), vec!["a", "c", "d"]);
        assert_eq!(table[Path::new("a")].at_ms, 10);
    }

    #[test]
    fn every_file_the_core_reads_is_in_the_status_once() {
        let d = dir("every");
        let names: Vec<&str> = status(&d).iter().map(|r| r.name).collect();
        assert_eq!(names.len(), 17);
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "{names:?}");
        assert!(status(&d).iter().all(|r| r.state == ConfigState::Absent && r.loaded.is_none() && !r.needs_restart), "an empty data folder: nothing is there and nothing was loaded");
        let _ = std::fs::remove_dir_all(&d);
    }
}

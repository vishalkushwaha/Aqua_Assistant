//! The person's hooks (`data/hooks.json`): programs Aqua runs around a tool call and at a few moments of a run. They can only **tighten** (see `aqua_tool_host::hooks`):
//! a program may make a call ask or refuse it, and after a call mask text or add a short note. It cannot let through what the policy asks about or refuses.
//!
//! ```json
//! {"hooks": [
//!   {"event": "PreToolUse", "tools": ["term.*", "fs.delete"], "command": ["C:/tools/check.exe", "--strict"]},
//!   {"event": "PostToolUse", "tools": ["web.read"], "command": ["C:/tools/scrub.exe"]},
//!   {"event": "FileChanged", "tools": ["fs.*"], "command": ["cargo", "fmt", "--check"], "cwd": "project", "env": {"USERPROFILE": "C:/Users/me"}},
//!   {"event": "Stop", "command": ["C:/tools/done.exe"]}
//! ]}
//! ```
//!
//! A hook is a program and its arguments (no shell), run with a scrubbed environment and a timeout (5 s at most). It is given the call as JSON on its standard input: `{"event", "tool",
//! "args" (masked), "classes", "target", "decision", "caller", "run_id", "project", "cwd"}`, and for `PostToolUse` also `"result_text"` (masked, cut). `target` and `decision` are what the
//! call was about and what the host decided for it, after the call as before it. What it says:
//!
//! * exit code **2** refuses the call, the first line of what it wrote to stderr is the reason;
//! * exit code 0 with a JSON object as a line of stdout: `{"decision": "ask" | "deny", "reason": "..."}` (anything else, including `"allow"`, changes nothing); after a call
//!   `{"redact": ["text to mask"], "note": "one line for the model"}`;
//! * any other exit code, a timeout or a program that does not start makes the call **ask** (a check that cannot run never lets a call through unseen).
//!
//! **The events** (card OA11-01, task EX-04): `PreToolUse` and `PostToolUse` (a call that worked) as above. `PostToolUseFailure` is told of a call that failed or was refused (the tool, `error_code`, `decision`
//! and one masked line of the message). `FileChanged` is told of a file tool that changed a file. `RunStart` is told a run began, and `Stop` that it ended (also when it ended in an error).
//! `UserPromptSubmit` is given the start of the words a run was asked to do (500 characters, masked) **before** the run starts, and its answer counts: exit 2 or `deny` stops the run, `ask` makes
//! the person say yes first. Of these only `UserPromptSubmit` can change anything; the others run on their own threads, nobody waits for them, and nothing they say is used. No event has an
//! answer that allows.
//!
//! **`env` and `cwd`** (both optional, per hook): `env` is a map of names to a text or `{"secret": "name"}` (a secret of the credential store, resolved when the hook runs, never written down);
//! the program is given exactly the scrubbed list plus these. A name that is `AQUA_API_TOKEN` or `AQUA_UI_TOKEN`, or has TOKEN, SECRET or PASSWORD in it, is refused at load, and so is a
//! value with a line break. `cwd` is `"project"` (the folder of the project the run works in), `"data"` (the data folder) or a full path inside the folders of `fs.json`; a relative program name
//! is looked for in it. A hook of a repository's pack (`.aqua/hooks.json`, used only after the person trusted it as code) runs in the project, may not ask for a secret, and
//! may not name another folder.
//!
//! **A budget:** the hooks of one run may run for 60 seconds in all; after that every call that is not a read asks, so that a hook that is slow on every call cannot hold a run up.
//!
//! A file that is wrong makes every call that is not a read ask until it is fixed.

use aqua_tool_host::hooks::is_read_only;
use aqua_tool_host::{HookAnswer, HookCall, HookEvent, Hooks, PostAnswer};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const MAX_HOOKS: usize = 32;
const MAX_TIMEOUT_MS: u64 = 5_000;
const OUTPUT_CAP: usize = 64 * 1024;
const RESULT_TEXT_CAP: usize = 8_000;
/// The most variables one hook may be given, and the longest value of one.
const MAX_ENV: usize = 16;
const MAX_ENV_VALUE: usize = 1_000;
/// How long the hooks of one run may run in all before every call that is not a read asks.
pub const RUN_BUDGET: Duration = Duration::from_secs(60);
/// The most hook programs of the events nobody waits for that run at once; an event past that is dropped (and logged), so that a run that changes a thousand files cannot start a thousand programs.
const MAX_BACKGROUND: usize = 8;

/// The events a hook may be written for.
pub const EVENTS: [&str; 7] = ["PreToolUse", "PostToolUse", "PostToolUseFailure", "UserPromptSubmit", "RunStart", "FileChanged", "Stop"];

fn default_timeout() -> u64 {
    MAX_TIMEOUT_MS
}

/// What a variable of a hook is given: a text, or a secret of the credential store (read when the hook runs).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
pub enum EnvValue {
    Literal(String),
    Secret { secret: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookFile {
    hooks: Vec<HookEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEntry {
    /// One of [`EVENTS`].
    pub event: String,
    /// Tool names it applies to: exact (`term.run`), a prefix with a star (`fs.*`, `term.r*`) or `*`. Empty means every tool. Read for the events of a call (`PreToolUse`,
    /// `PostToolUse`, `PostToolUseFailure`, `FileChanged`); the other events have no tool.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The program and its arguments, run without a shell.
    pub command: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// Variables the program is given besides the scrubbed list (see the module's text).
    #[serde(default)]
    pub env: BTreeMap<String, EnvValue>,
    /// Where it runs: `"project"`, `"data"` or a full path inside the folders of `fs.json`. Not set: where the core runs.
    #[serde(default)]
    pub cwd: Option<String>,
}

fn tool_matches(patterns: &[String], tool: &str) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| aqua_tool_host::tool_pattern_matches(p, tool))
}

/// Whose file the hooks are from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// `data/hooks.json`: the person's own.
    Person,
    /// A repository's `.aqua/hooks.json`, trusted as code by the person.
    Pack,
}

/// A name a hook may not set: the tokens of Aqua's own API, and anything that says it is a token, a secret or a password.
fn reserved_env_name(name: &str) -> bool {
    let n = name.to_ascii_uppercase();
    n == "AQUA_API_TOKEN" || n == "AQUA_UI_TOKEN" || n.contains("TOKEN") || n.contains("SECRET") || n.contains("PASSWORD")
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && name.len() <= 64 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What a hook needs from the rest of the program: where a secret is, which folders it may run in, where the data is, and who is told of a secret that was read (so that it is masked
/// wherever it might be printed). The live one reads the credential store and the folders of the file tools; a test gives its own.
#[derive(Clone)]
pub struct HookEnv {
    pub secret: Arc<dyn Fn(&str) -> Option<String> + Send + Sync>,
    pub cwd_allowed: Arc<dyn Fn(&Path) -> bool + Send + Sync>,
    pub data_dir: Arc<dyn Fn() -> PathBuf + Send + Sync>,
    pub register_secret: Arc<dyn Fn(&str) + Send + Sync>,
}

impl std::fmt::Debug for HookEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HookEnv")
    }
}

type CwdGuard = Arc<dyn Fn(&Path) -> bool + Send + Sync>;
type SecretSink = Arc<dyn Fn(&str) + Send + Sync>;

static CWD_GUARD: RwLock<Option<CwdGuard>> = RwLock::new(None);
static SECRET_SINK: RwLock<Option<SecretSink>> = RwLock::new(None);

/// The folders a hook may be told to run in (`cwd` as a full path): set by the tool host at start, from the folders of `fs.json`. Without it no full path is allowed.
pub fn set_cwd_guard(guard: CwdGuard) {
    *CWD_GUARD.write().unwrap_or_else(PoisonError::into_inner) = Some(guard);
}

/// Who is told of a secret a hook was given, so that the host masks it in what it shows and records.
pub fn set_secret_sink(sink: SecretSink) {
    *SECRET_SINK.write().unwrap_or_else(PoisonError::into_inner) = Some(sink);
}

impl HookEnv {
    /// The program's own: the credential store, the folders of the file tools, the data folder.
    pub fn live() -> Self {
        Self {
            secret: Arc::new(|name| {
                use aqua_native_tools::secrets::Secrets;
                aqua_native_tools::secrets::CredentialManager.get(name).ok().flatten()
            }),
            cwd_allowed: Arc::new(|path| CWD_GUARD.read().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|g| g(path))),
            data_dir: Arc::new(crate::runtime::data_dir),
            register_secret: Arc::new(|value| {
                if let Some(sink) = SECRET_SINK.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
                    sink(value);
                }
            }),
        }
    }
}

/// Hooks that run programs. Empty when the file does not exist.
#[derive(Debug, Clone)]
pub struct CommandHooks {
    hooks: Vec<HookEntry>,
    source: Source,
    env: HookEnv,
    /// How long each run's hooks have run, and the most they may.
    spent: Arc<Mutex<HashMap<String, Duration>>>,
    budget: Duration,
}

impl Default for CommandHooks {
    fn default() -> Self {
        Self { hooks: Vec::new(), source: Source::Person, env: HookEnv::live(), spent: Arc::new(Mutex::new(HashMap::new())), budget: RUN_BUDGET }
    }
}

impl CommandHooks {
    pub fn none() -> Self {
        Self::default()
    }

    /// Read the file's text. `Err` says what is wrong.
    pub fn from_text(text: &str) -> Result<Self, String> {
        Self::parse(text, Source::Person)
    }

    /// Read the `hooks.json` of a repository's pack (the person trusted it as code). The same format with fewer freedoms: it runs in the project's folder (that is where a relative program
    /// is found), may not name another folder, and may not ask for a secret.
    pub fn from_pack_text(text: &str) -> Result<Self, String> {
        Self::parse(text, Source::Pack)
    }

    fn parse(text: &str, source: Source) -> Result<Self, String> {
        let file: HookFile = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| format!("not valid: {e}"))?;
        if file.hooks.len() > MAX_HOOKS {
            return Err(format!("{} hooks, at most {MAX_HOOKS}", file.hooks.len()));
        }
        for (i, h) in file.hooks.iter().enumerate() {
            let at = format!("hook {}", i + 1);
            if !EVENTS.contains(&h.event.as_str()) {
                return Err(format!("{at}: the event is {}, not {:?}", EVENTS.join(", "), h.event));
            }
            let exe = h.command.first().map(|c| c.trim()).unwrap_or("");
            if exe.is_empty() || h.command.iter().any(|a| a.contains('\0')) {
                return Err(format!("{at}: the command is a program and its arguments (no shell)"));
            }
            if h.timeout_ms == 0 || h.timeout_ms > MAX_TIMEOUT_MS {
                return Err(format!("{at}: timeout_ms is 1 to {MAX_TIMEOUT_MS}"));
            }
            if h.tools.iter().any(|t| t.trim().is_empty() || t.len() > 80) {
                return Err(format!("{at}: a tool pattern is empty or too long"));
            }
            if matches!(h.event.as_str(), "UserPromptSubmit" | "RunStart") && !h.tools.is_empty() {
                return Err(format!("{at}: {} is not about a tool, so it has no tools list", h.event));
            }
            if h.env.len() > MAX_ENV {
                return Err(format!("{at}: at most {MAX_ENV} variables in env"));
            }
            for (name, value) in &h.env {
                if !valid_env_name(name) {
                    return Err(format!("{at}: {name:?} is not a name for a variable (letters, digits and _, not starting with a digit)"));
                }
                if reserved_env_name(name) {
                    return Err(format!("{at}: env {name}: a hook is never given a variable that is called a token, a secret or a password, nor the tokens of Aqua's own API"));
                }
                match value {
                    EnvValue::Literal(v) if v.contains(['\n', '\r', '\0']) || v.chars().count() > MAX_ENV_VALUE => {
                        return Err(format!("{at}: env {name}: a value is one line (no line break) of at most {MAX_ENV_VALUE} characters"));
                    }
                    EnvValue::Secret { secret } if !aqua_native_tools::secrets::valid_name(secret) => {
                        return Err(format!("{at}: env {name}: {secret:?} is not the name of a secret"));
                    }
                    EnvValue::Secret { .. } if source == Source::Pack => {
                        return Err(format!("{at}: env {name}: the hooks of a repository may not ask for a secret"));
                    }
                    _ => {}
                }
            }
            match h.cwd.as_deref() {
                None | Some("project") => {}
                Some("data") if source == Source::Person => {}
                Some(path) if source == Source::Person && Path::new(path).is_absolute() && !path.contains('\0') && !Path::new(path).components().any(|c| matches!(c, std::path::Component::ParentDir)) => {}
                Some(other) => {
                    return Err(format!(
                        "{at}: cwd is {}, not {other:?}",
                        if source == Source::Pack { "\"project\" for the hooks of a repository" } else { "\"project\", \"data\" or a full path with no .." }
                    ))
                }
            }
        }
        Ok(Self { hooks: file.hooks, source, ..Self::default() })
    }

    /// The same hooks with another environment (for a test, or a host that is not the program's own).
    pub fn with_env(mut self, env: HookEnv) -> Self {
        self.env = env;
        self
    }

    /// The same hooks with another time budget for a run.
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// What each hook is, in a form two files can be compared by (a hook is the same when its event, tools, command, time, variables and folder are): for the reload of the file (card OA11-03).
    pub fn signatures(&self) -> Vec<String> {
        self.hooks.iter().map(|h| format!("{h:?}")).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// The events the hooks are written for, each once, in the order of [`EVENTS`] (the doctor lists them).
    pub fn events(&self) -> Vec<&'static str> {
        EVENTS.iter().copied().filter(|e| self.hooks.iter().any(|h| h.event == *e)).collect()
    }

    /// How many hooks set `env` and how many set `cwd` (the doctor counts them).
    pub fn counts_env_and_cwd(&self) -> (usize, usize) {
        (self.hooks.iter().filter(|h| !h.env.is_empty()).count(), self.hooks.iter().filter(|h| h.cwd.is_some()).count())
    }

    fn of<'a>(&'a self, event: &'a str, tool: &'a str) -> impl Iterator<Item = &'a HookEntry> + 'a {
        self.hooks.iter().filter(move |h| h.event == event && tool_matches(&h.tools, tool))
    }

    fn spent_by(&self, run_id: &str) -> Duration {
        if run_id.is_empty() {
            return Duration::ZERO;
        }
        self.spent.lock().unwrap_or_else(PoisonError::into_inner).get(run_id).copied().unwrap_or_default()
    }

    fn charge(&self, run_id: &str, used: Duration) {
        if run_id.is_empty() {
            return;
        }
        let mut spent = self.spent.lock().unwrap_or_else(PoisonError::into_inner);
        // The table is as long as the runs that are going on, not as the runs there have been.
        if spent.len() >= 64 && !spent.contains_key(run_id) {
            spent.clear();
        }
        *spent.entry(run_id.to_string()).or_default() += used;
    }

    /// Has the run used its time? (`run_id` empty: a call that is no part of a run is never held to a budget.)
    fn over_budget(&self, run_id: &str) -> bool {
        !run_id.is_empty() && self.spent_by(run_id) >= self.budget
    }

    /// Run the programs of an event that nobody waits for, on a thread of their own: nothing they say changes anything. At most [`MAX_BACKGROUND`] run at once; one past that is dropped.
    fn run_in_background(&self, hooks: Vec<HookEntry>, payload: Value, project: String) {
        if hooks.is_empty() {
            return;
        }
        if BACKGROUND.fetch_add(1, Ordering::SeqCst) >= MAX_BACKGROUND {
            BACKGROUND.fetch_sub(1, Ordering::SeqCst);
            warn!("hooks: {} programs of events nobody waits for are running; this event was not given to its hooks", MAX_BACKGROUND);
            return;
        }
        let (env, source) = (self.env.clone(), self.source);
        let spawned = std::thread::Builder::new().name("aqua-hook-event".into()).spawn(move || {
            for h in hooks {
                let _ = run_program(&env, source, &h, &payload, &project);
            }
            BACKGROUND.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            BACKGROUND.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// Run the `Stop` hooks of a run that ended (in the background: nothing waits for them and nothing they say changes anything).
    pub fn stop(&self, run_id: &str, reason: &str) {
        let hooks: Vec<HookEntry> = self.of("Stop", "").cloned().collect();
        let event = HookEvent { event: "Stop", run_id: run_id.to_string(), stopped_reason: reason.to_string(), ..Default::default() };
        self.run_in_background(hooks, event_payload(&event), String::new());
    }

    /// The answer of the hooks of one event, as the hooks of a call give it: exit code 2 refuses with the first line of stderr, a JSON line with `decision` asks or refuses, a program that
    /// cannot run asks, and a run that has used its time asks (for a call that is not a read).
    fn answer_of(&self, event: &str, tool: &str, payload: &Value, run_id: &str, project: &str, reads_only: bool) -> HookAnswer {
        let mut answer = HookAnswer::Pass;
        for h in self.of(event, tool) {
            if self.over_budget(run_id) {
                if !reads_only {
                    let over = HookAnswer::Ask(format!("the hooks of this run have used their {}, so what is not a read asks", if self.budget >= Duration::from_secs(1) { format!("{} seconds", self.budget.as_secs()) } else { format!("{} ms", self.budget.as_millis()) }));
                    if answer == HookAnswer::Pass {
                        answer = over;
                    }
                }
                break;
            }
            let started = Instant::now();
            let ran = run_program(&self.env, self.source, h, payload, project);
            self.charge(run_id, started.elapsed());
            let this = match (&ran.problem, ran.exit) {
                (Some(p), _) => HookAnswer::Ask(format!("a hook could not check this call: {p}")),
                (None, Some(2)) => {
                    let why = one_line(ran.stderr.lines().next().unwrap_or(""), 200);
                    HookAnswer::Deny(if why.is_empty() { "a hook refused it".into() } else { why })
                }
                (None, Some(0)) => {
                    let v = first_json_object(&ran.stdout);
                    let reason = v.as_ref().and_then(|v| v["reason"].as_str()).map(|r| one_line(r, 200)).filter(|r| !r.is_empty());
                    match v.as_ref().and_then(|v| v["decision"].as_str()) {
                        Some("deny") => HookAnswer::Deny(reason.unwrap_or_else(|| "a hook refused it".into())),
                        Some("ask") => HookAnswer::Ask(reason.unwrap_or_else(|| "a hook wants it checked".into())),
                        _ => HookAnswer::Pass,
                    }
                }
                (None, other) => HookAnswer::Ask(format!("a hook could not check this call: it ended with code {}", other.map_or("none".to_string(), |c| c.to_string()))),
            };
            match this {
                HookAnswer::Deny(_) => return this,
                HookAnswer::Ask(_) if answer == HookAnswer::Pass => answer = this,
                _ => {}
            }
        }
        answer
    }
}

/// How many programs of the events nobody waits for are running.
static BACKGROUND: AtomicUsize = AtomicUsize::new(0);

/// The payload of an event that is not a call, as JSON.
fn event_payload(event: &HookEvent) -> Value {
    serde_json::to_value(event).unwrap_or_else(|_| json!({ "event": event.event }))
}

struct Ran {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
    /// Why the program did not run to its end (it did not start, or it timed out).
    problem: Option<String>,
}

/// The variables a hook is given: where programs are found and where Windows keeps its own, nothing that names a credential.
fn scrubbed_env() -> Vec<(String, String)> {
    ["PATH", "PATHEXT", "SystemRoot", "SystemDrive", "TEMP", "TMP", "windir"].iter().filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect()
}

fn read_capped(mut r: impl Read) -> String {
    let mut buf = Vec::new();
    let _ = (&mut r).take(OUTPUT_CAP as u64).read_to_end(&mut buf);
    // Whatever is left is drained so the program is never stuck writing to a full pipe.
    let _ = std::io::copy(&mut r, &mut std::io::sink());
    String::from_utf8_lossy(&buf).to_string()
}

/// A hook that could not even be started, in a form the callers treat like any other program that did not run.
fn not_run(text: String) -> Ran {
    Ran { exit: None, stdout: String::new(), stderr: String::new(), problem: Some(text) }
}

/// The folder a hook runs in: `Ok(None)` is where the core runs.
fn resolve_cwd(env: &HookEnv, source: Source, h: &HookEntry, project: &str) -> Result<Option<PathBuf>, String> {
    let asked = match (h.cwd.as_deref(), source) {
        (Some(c), _) => c,
        (None, Source::Pack) => "project",
        (None, Source::Person) => return Ok(None),
    };
    let dir = match asked {
        "project" => {
            if project.is_empty() {
                return Err("it runs in the project's folder, and this run has none".into());
            }
            PathBuf::from(project)
        }
        "data" => (env.data_dir)(),
        path => {
            let real = std::fs::canonicalize(path).map_err(|_| "the folder it runs in is not there".to_string())?;
            if !(env.cwd_allowed)(&real) {
                return Err("the folder it runs in is not inside the folders Aqua works in (fs.json)".into());
            }
            real
        }
    };
    if !dir.is_dir() {
        return Err("the folder it runs in is not there".into());
    }
    Ok(Some(dir))
}

fn run_program(env: &HookEnv, source: Source, h: &HookEntry, payload: &Value, project: &str) -> Ran {
    let name = Path::new(&h.command[0]).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| h.command[0].clone());
    let cwd = match resolve_cwd(env, source, h, project) {
        Ok(c) => c,
        Err(why) => return not_run(format!("`{name}` did not start: {why}")),
    };
    // The variables it is given: the scrubbed list and the ones the hook declares. A secret is read now and given to the program and to nobody else; the host is told of it so that it is
    // masked in anything that is shown or written down.
    let mut vars = scrubbed_env();
    for (var, value) in &h.env {
        let text = match value {
            EnvValue::Literal(v) => v.clone(),
            EnvValue::Secret { secret } => match (env.secret)(secret) {
                Some(v) if !v.contains(['\n', '\r', '\0']) => {
                    (env.register_secret)(&v);
                    v
                }
                Some(_) => return not_run(format!("`{name}` did not start: the secret for {var} is not one line of text")),
                None => return not_run(format!("`{name}` did not start: the secret for {var} is not stored")),
            },
        };
        vars.retain(|(k, _)| !k.eq_ignore_ascii_case(var));
        vars.push((var.clone(), text));
    }
    let mut payload = payload.clone();
    if let (Some(dir), Some(map)) = (&cwd, payload.as_object_mut()) {
        map.insert("cwd".into(), json!(dir.to_string_lossy()));
    }
    // A program named by a relative path (`tools/check.exe`) is looked for in the folder the hook runs in: the system looks for it where the core runs unless this says where.
    let mut program = PathBuf::from(&h.command[0]);
    if let Some(dir) = &cwd {
        if program.is_relative() && h.command[0].contains(['/', '\\']) {
            program = dir.join(&program);
        }
    }
    let mut cmd = Command::new(&program);
    cmd.args(&h.command[1..]).env_clear().envs(vars).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(dir) = &cwd {
        cmd.current_dir(dir);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // no console window
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return not_run(format!("`{name}` did not start ({e})")),
    };
    // Written on a thread: a program that does not read its input must not hold this one.
    if let Some(mut stdin) = child.stdin.take() {
        let data = payload.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(data.as_bytes());
        });
    }
    let out = child.stdout.take().map(|s| std::thread::spawn(move || read_capped(s)));
    let err = child.stderr.take().map(|s| std::thread::spawn(move || read_capped(s)));
    let deadline = Instant::now() + Duration::from_millis(h.timeout_ms);
    let mut exit = None;
    let mut problem = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit = status.code();
                if exit.is_none() {
                    problem = Some(format!("`{name}` was stopped by the system"));
                }
                break;
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                problem = Some(format!("`{name}` did not answer in {} ms", h.timeout_ms));
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                problem = Some(format!("`{name}` could not be waited for ({e})"));
                break;
            }
        }
    }
    let stdout = out.and_then(|t| t.join().ok()).unwrap_or_default();
    let stderr = err.and_then(|t| t.join().ok()).unwrap_or_default();
    Ran { exit, stdout, stderr, problem }
}

/// The first JSON object in the output: a line that is one, or the end of a line that has one in it (a hook may print other things too).
fn first_json_object(text: &str) -> Option<Value> {
    text.lines().find_map(|l| l.find('{').and_then(|at| serde_json::from_str::<Value>(l[at..].trim()).ok()).filter(Value::is_object))
}

fn one_line(text: &str, max: usize) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max).collect()
}

impl Hooks for CommandHooks {
    fn pre_tool(&self, call: &HookCall) -> HookAnswer {
        let payload = match serde_json::to_value(call) {
            Ok(p) => p,
            Err(_) => return HookAnswer::Ask("a hook could not be given the call".into()),
        };
        self.answer_of("PreToolUse", &call.tool, &payload, &call.run_id, &call.project, is_read_only(call))
    }

    fn post_tool(&self, call: &HookCall, ok: bool, text: &str) -> PostAnswer {
        let mut payload = match serde_json::to_value(call) {
            Ok(p) => p,
            Err(_) => return PostAnswer::default(),
        };
        payload["ok"] = json!(ok);
        payload["result_text"] = json!(text.chars().take(RESULT_TEXT_CAP).collect::<String>());
        let mut answer = PostAnswer::default();
        for h in self.of("PostToolUse", &call.tool) {
            if self.over_budget(&call.run_id) {
                break;
            }
            let started = Instant::now();
            let ran = run_program(&self.env, self.source, h, &payload, &call.project);
            self.charge(&call.run_id, started.elapsed());
            if ran.problem.is_some() || ran.exit != Some(0) {
                continue;
            }
            if let Some(v) = first_json_object(&ran.stdout) {
                if let Some(list) = v["redact"].as_array() {
                    answer.redact.extend(list.iter().filter_map(|s| s.as_str().map(str::to_string)).take(32));
                }
                if let Some(n) = v["note"].as_str().filter(|n| !n.trim().is_empty()) {
                    answer.note = Some(one_line(n, 300));
                }
            }
        }
        answer
    }

    fn on_event(&self, event: &HookEvent) -> HookAnswer {
        let payload = event_payload(event);
        match event.event {
            // The one event whose answer counts: the words of a run, before it starts. A call that is a read does not exist here, so the budget is held to for the person's yes: ask.
            "UserPromptSubmit" => self.answer_of("UserPromptSubmit", "", &payload, &event.run_id, &event.project, false),
            "PostToolUseFailure" | "FileChanged" | "RunStart" | "Stop" => {
                let tool = if matches!(event.event, "PostToolUseFailure" | "FileChanged") { event.tool.as_str() } else { "" };
                let hooks: Vec<HookEntry> = self.of(event.event, tool).cloned().collect();
                self.run_in_background(hooks, payload, event.project.clone());
                HookAnswer::Pass
            }
            _ => HookAnswer::Pass,
        }
    }

    fn wants_event(&self, event: &str) -> bool {
        self.hooks.iter().any(|h| h.event == event)
    }
}

/// What the host is given when the file is wrong: every call that is not a read asks until it is fixed.
pub struct BrokenHooks(pub String);

impl Hooks for BrokenHooks {
    fn pre_tool(&self, call: &HookCall) -> HookAnswer {
        if call.classes.iter().all(|c| c == "READ") {
            HookAnswer::Pass
        } else {
            HookAnswer::Ask(format!("data/hooks.json is wrong ({}), so every call that is not a read asks until it is fixed", self.0))
        }
    }

    fn post_tool(&self, _call: &HookCall, _ok: bool, _text: &str) -> PostAnswer {
        PostAnswer::default()
    }
}

// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------
// The hooks of a repository's pack
// -------------------------------------------------------------------------------------------------------------------------------------------------------------------------

/// What was made of the `hooks.json` of a pack, by the hash of its bytes (a few at most: runs go on one after the other in most projects).
static PACK_HOOKS: Mutex<Vec<(String, Arc<Result<CommandHooks, String>>)>> = Mutex::new(Vec::new());

fn pack_hooks_of(bytes: &[u8]) -> Arc<Result<CommandHooks, String>> {
    use sha2::{Digest, Sha256};
    let key: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    let mut cache = PACK_HOOKS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, hooks)) = cache.iter().find(|(k, _)| *k == key) {
        return hooks.clone();
    }
    let parsed = Arc::new(String::from_utf8(bytes.to_vec()).map_err(|_| "it is not text".to_string()).and_then(|t| CommandHooks::from_pack_text(&t)));
    if cache.len() >= 8 {
        cache.remove(0);
    }
    cache.push((key, parsed.clone()));
    parsed
}

/// The person's hooks, and beside them the `hooks.json` of the project a run works in when the person has trusted that pack as code (`project_pack`). A hook of the project can only tighten
/// what the person's own hooks and the policy decided, like any hook: a refusal of either stands, a question of either is asked. A project file that cannot be used makes every call that is not a
/// read ask, as a wrong `data/hooks.json` does.
pub struct PackAware {
    /// The person's hooks. Swapped by [`reload`] (card OA11-03), and only for what cannot loosen anything.
    inner: RwLock<Option<Arc<dyn Hooks>>>,
}

impl PackAware {
    pub fn new(inner: Option<Arc<dyn Hooks>>) -> Self {
        Self { inner: RwLock::new(inner) }
    }

    /// The person's hooks as they are now.
    fn own(&self) -> Option<Arc<dyn Hooks>> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Puts other hooks of the person's in place of these (the next call is judged by them).
    fn swap(&self, hooks: Option<Arc<dyn Hooks>>) {
        *self.inner.write().unwrap_or_else(PoisonError::into_inner) = hooks;
    }

    fn pack(&self, run_id: &str) -> Option<Arc<Result<CommandHooks, String>>> {
        if run_id.is_empty() {
            return None;
        }
        crate::project_context::pack_code_file(run_id, "hooks.json").map(|bytes| pack_hooks_of(&bytes))
    }
}

/// A refusal wins over a question, and a question over nothing; two questions are the first one.
fn stricter(a: HookAnswer, b: HookAnswer) -> HookAnswer {
    match (a, b) {
        (HookAnswer::Deny(w), _) | (_, HookAnswer::Deny(w)) => HookAnswer::Deny(w),
        (HookAnswer::Ask(w), _) | (HookAnswer::Pass, HookAnswer::Ask(w)) => HookAnswer::Ask(w),
        (HookAnswer::Pass, HookAnswer::Pass) => HookAnswer::Pass,
    }
}

impl Hooks for PackAware {
    fn pre_tool(&self, call: &HookCall) -> HookAnswer {
        let own = self.own().map(|h| h.pre_tool(call)).unwrap_or(HookAnswer::Pass);
        if matches!(own, HookAnswer::Deny(_)) {
            return own;
        }
        let project = match self.pack(&call.run_id).as_deref() {
            Some(Ok(h)) => h.pre_tool(call),
            Some(Err(why)) if !is_read_only(call) => HookAnswer::Ask(format!("the project's hooks.json is wrong ({why}), so every call that is not a read asks until it is fixed")),
            _ => HookAnswer::Pass,
        };
        stricter(own, project)
    }

    fn post_tool(&self, call: &HookCall, ok: bool, text: &str) -> PostAnswer {
        let mut answer = self.own().map(|h| h.post_tool(call, ok, text)).unwrap_or_default();
        if let Some(Ok(h)) = self.pack(&call.run_id).as_deref() {
            let more = h.post_tool(call, ok, text);
            answer.redact.extend(more.redact);
            answer.note = match (answer.note.take(), more.note) {
                (Some(a), Some(b)) => Some(format!("{a}; {b}")),
                (a, b) => a.or(b),
            };
        }
        answer
    }

    fn on_event(&self, event: &HookEvent) -> HookAnswer {
        let own = self.own().map(|h| h.on_event(event)).unwrap_or(HookAnswer::Pass);
        if matches!(own, HookAnswer::Deny(_)) {
            return own;
        }
        let project = match self.pack(&event.run_id).as_deref() {
            Some(Ok(h)) => h.on_event(event),
            _ => HookAnswer::Pass,
        };
        stricter(own, project)
    }

    fn wants_event(&self, event: &str) -> bool {
        self.own().is_some_and(|h| h.wants_event(event)) || crate::project_context::enabled()
    }
}

pub fn hooks_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("hooks.json")
}

/// Load `data/hooks.json`. A missing file means no hooks; a wrong one is an `Err` that says what is wrong and where (comments and trailing commas are accepted:
/// `crate::config_files`).
pub fn load_hooks(data_dir: &Path) -> Result<CommandHooks, String> {
    match crate::config_files::load(data_dir, &crate::config_files::HOOKS) {
        crate::config_files::Loaded::Ok(hooks) | crate::config_files::Loaded::Defaulted(hooks) => Ok(hooks),
        crate::config_files::Loaded::Broken(e) => Err(e.detail()),
    }
}

/// What the host is given for `data/hooks.json`, in one place so that the test of the loader and the host runs what start-up runs (card R6-16): `None` when there is no file
/// or it holds no hooks, the hooks when it is right (and they are kept for the `Stop` event), and [`BrokenHooks`] when it is wrong.
pub fn for_host(data_dir: &Path) -> Option<Arc<dyn Hooks>> {
    let (hooks, running) = for_host_state(data_dir);
    // What the host was given is remembered, for the reload (see `install_reloadable`).
    *GIVEN.lock().unwrap_or_else(PoisonError::into_inner) = running;
    hooks
}

/// What the last call of [`for_host`] gave the host.
static GIVEN: Mutex<Running> = Mutex::new(Running::Nothing);

/// What the person's hooks that are running are, for the reload (card OA11-03): none, these (by what each says), or the ones that ask about everything because the file is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Running {
    Nothing,
    Hooks(Vec<String>),
    Broken,
}

/// [`for_host`], and what it gave the host.
fn for_host_state(data_dir: &Path) -> (Option<Arc<dyn Hooks>>, Running) {
    match load_hooks(data_dir) {
        Ok(h) if h.is_empty() => (None, Running::Nothing),
        Ok(h) => {
            info!("tool host: {} hook(s) from hooks.json (events: {})", h.len(), h.events().join(", "));
            let signatures = h.signatures();
            let h = Arc::new(h);
            install(h.clone());
            (Some(h as Arc<dyn Hooks>), Running::Hooks(signatures))
        }
        Err(e) => {
            warn!("hooks.json is wrong ({e}): every call that is not a read asks until it is fixed");
            (Some(Arc::new(BrokenHooks(e)) as Arc<dyn Hooks>), Running::Broken)
        }
    }
}

/// What reading `data/hooks.json` again did (card OA11-03, task EX-13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HooksReload {
    /// The hooks of the file are the ones that run now: how many. Nothing was running that is not in the file.
    Applied { hooks: usize },
    /// The file is wrong: every call that is not a read asks until it is fixed (that is stricter than what ran).
    Broken(String),
    /// A hook that is running is not in the file as it is (it was changed or removed): that loosens what is checked, so it waits for a restart of Aqua.
    NeedsRestart(String),
    /// There are no hooks to reload (the host was not built).
    Unavailable,
}

struct Reloadable {
    target: Arc<PackAware>,
    running: Running,
}

static RELOADABLE: Mutex<Option<Reloadable>> = Mutex::new(None);

/// Called when the host is built, after [`for_host`]: `target` holds the person's hooks that `for_host` gave the host.
pub fn install_reloadable(target: Arc<PackAware>) {
    let running = GIVEN.lock().unwrap_or_else(PoisonError::into_inner).clone();
    *RELOADABLE.lock().unwrap_or_else(PoisonError::into_inner) = Some(Reloadable { target, running });
}

/// Reads `data/hooks.json` again for the host that was built, with the rules of [`reload_into`].
pub fn reload(data_dir: &Path) -> HooksReload {
    let mut slot = RELOADABLE.lock().unwrap_or_else(PoisonError::into_inner);
    match slot.as_mut() {
        Some(r) => reload_into(&r.target, &mut r.running, data_dir),
        None => HooksReload::Unavailable,
    }
}

/// Reads `data/hooks.json` again and puts what it says in place of the hooks in `target`, **only if no check that is running is given up**: the person's hooks can only tighten what the
/// policy decided, so a file that has all the hooks that ran (and perhaps more) tightens or leaves things as they are, and a file that is wrong asks about every call that is not a read.
/// A file that no longer has a hook that ran, or has it changed (another command, tools or time), loosens something: it is not applied, and says so.
pub fn reload_into(target: &PackAware, running: &mut Running, data_dir: &Path) -> HooksReload {
    match load_hooks(data_dir) {
        Err(e) => {
            warn!("hooks.json is wrong ({e}): every call that is not a read asks until it is fixed");
            target.swap(Some(Arc::new(BrokenHooks(e.clone())) as Arc<dyn Hooks>));
            *running = Running::Broken;
            HooksReload::Broken(e)
        }
        Ok(new) => {
            let new_signatures = new.signatures();
            if let Running::Hooks(old) = running {
                let mut left = new_signatures.clone();
                for signature in old.iter() {
                    match left.iter().position(|s| s == signature) {
                        Some(i) => {
                            left.remove(i);
                        }
                        None => return HooksReload::NeedsRestart("a hook that is running is not in hooks.json as it was (it was changed or removed): that loosens what is checked, so it takes effect when Aqua is started again".into()),
                    }
                }
            }
            let n = new_signatures.len();
            if new.is_empty() {
                target.swap(None);
                *running = Running::Nothing;
            } else {
                info!("tool host: {n} hook(s) from hooks.json, read again (events: {})", new.events().join(", "));
                target.swap(Some(Arc::new(new) as Arc<dyn Hooks>));
                *running = Running::Hooks(new_signatures);
            }
            HooksReload::Applied { hooks: n }
        }
    }
}

static INSTALLED: OnceLock<Arc<CommandHooks>> = OnceLock::new();

/// Called once at start-up with the hooks the host was given, so the `Stop` event can reach them when no chain was installed.
pub fn install(hooks: Arc<CommandHooks>) {
    let _ = INSTALLED.set(hooks);
}

/// The hooks the host has, as one thing (the person's, the project's and the watch on the tokens): the core tells it of the events that are not calls.
static CHAIN: RwLock<Option<Arc<dyn Hooks>>> = RwLock::new(None);

pub fn install_chain(chain: Arc<dyn Hooks>) {
    *CHAIN.write().unwrap_or_else(PoisonError::into_inner) = Some(chain);
}

#[cfg(test)]
thread_local! {
    /// The chain a test gave this thread: the one of the process is set by every test that builds a tool host, so a test that wants to see the events its own run tells cannot rely on it.
    static TEST_CHAIN: std::cell::RefCell<Option<Arc<dyn Hooks>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with `chain` as the hooks that the events of this thread's runs are told to (for a test).
#[cfg(test)]
pub(crate) fn with_chain<R>(chain: Arc<dyn Hooks>, f: impl FnOnce() -> R) -> R {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            TEST_CHAIN.with(|c| *c.borrow_mut() = None);
        }
    }
    TEST_CHAIN.with(|c| *c.borrow_mut() = Some(chain));
    let _clear = Clear;
    f()
}

fn current_chain() -> Option<Arc<dyn Hooks>> {
    #[cfg(test)]
    if let Some(chain) = TEST_CHAIN.with(|c| c.borrow().clone()) {
        return Some(chain);
    }
    CHAIN.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// An agent run ended, however it ended (an answer, a question, a stop, an error): tell the `Stop` hooks (if any).
pub fn stop(run_id: &str, reason: &str) {
    match current_chain() {
        Some(chain) if chain.wants_event("Stop") => {
            let project = crate::project_context::project_dir_of_run(run_id).map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            chain.on_event(&HookEvent { event: "Stop", run_id: run_id.to_string(), project, stopped_reason: reason.chars().take(200).collect(), ..Default::default() });
        }
        Some(_) => {}
        None => {
            if let Some(h) = INSTALLED.get() {
                h.stop(run_id, reason);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // ---- the loader and the host together (card R6-16): the host tests use scripted hooks, and these tests never used a host ----

    /// The tools the helper program above answers to by name, in a provider that notes what ran.
    struct Tools(Arc<std::sync::Mutex<Vec<String>>>);

    impl aqua_tool_host::ToolProvider for Tools {
        fn id(&self) -> &str {
            "hooks_test_tools"
        }
        fn manifests(&self) -> Vec<aqua_tool_host::ToolManifest> {
            use aqua_tool_host::{Annotations, Decision, PermClass, ToolManifest};
            let m = |name: &str, classes: Vec<PermClass>| ToolManifest {
                name: name.into(),
                title: name.into(),
                description: format!("Does {name}."),
                annotations: Annotations { read_only: classes == vec![PermClass::Read], destructive: classes.contains(&PermClass::Destructive), idempotent: false, open_world: false },
                classes,
                default_decision: Decision::Auto,
                input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
                timeout_default_ms: 5_000,
                timeout_max_ms: 5_000,
                untrusted_output: false,
            };
            vec![
                m("fs.read", vec![PermClass::Read]),
                m("web.read", vec![PermClass::Read]),
                m("fs.write", vec![PermClass::Write]),
                m("fs.allow", vec![PermClass::Write]),
                m("fs.delete", vec![PermClass::Write, PermClass::Destructive]),
                m("term.run", vec![PermClass::Execute]),
            ]
        }
        fn classify(&self, tool: &str, _a: &Value) -> Result<aqua_tool_host::Classification, aqua_tool_host::ToolError> {
            use aqua_tool_host::PermClass;
            let classes = match tool {
                "fs.read" | "web.read" => vec![PermClass::Read],
                "fs.delete" => vec![PermClass::Write, PermClass::Destructive],
                "term.run" => vec![PermClass::Execute],
                _ => vec![PermClass::Write],
            };
            Ok(aqua_tool_host::Classification { classes, target: "a.txt".into(), summary: format!("{tool} a.txt"), target_allowed: true })
        }
        fn call<'a>(&'a self, tool: &'a str, _a: Value, _c: &'a aqua_tool_host::ExecCtx) -> aqua_tool_host::ProviderFuture<'a, Result<aqua_tool_host::ProviderOutput, aqua_tool_host::ToolError>> {
            self.0.lock().unwrap().push(tool.to_string());
            let text = if tool == "web.read" { "the page says s3cr3t-value twice".to_string() } else { format!("ran {tool}") };
            Box::pin(async move { Ok(aqua_tool_host::ProviderOutput { text, ..Default::default() }) })
        }
    }

    /// The person: says what it is told to, and keeps what it was asked.
    struct Person {
        asked: std::sync::Mutex<Vec<aqua_tool_host::ApprovalRequest>>,
        answer: std::sync::Mutex<aqua_tool_host::ApprovalOutcome>,
    }

    impl aqua_tool_host::Approver for Person {
        fn request(&self, req: aqua_tool_host::ApprovalRequest) -> aqua_tool_host::ProviderFuture<'_, aqua_tool_host::ApprovalOutcome> {
            self.asked.lock().unwrap().push(req);
            let answer = *self.answer.lock().unwrap();
            Box::pin(async move { answer })
        }
    }

    #[test]
    fn the_file_the_loader_reads_and_the_host_that_calls_the_hooks_work_together() {
        use aqua_tool_host::{ApprovalOutcome, ApprovalScope, AuditLedger, Caller, ErrorCode, Mode, PolicyEngine, RequestContext, ToolHost, ToolRequest, ToolResult};
        let dir = std::env::temp_dir().join(format!("aqua_hooks_host_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // No file: no hooks, nothing to give the host.
        assert!(for_host(&dir).is_none());
        std::fs::write(hooks_path(&dir), json!({"hooks": [{"event": "PreToolUse", "command": me()}, {"event": "PostToolUse", "tools": ["web.read"], "command": me()}]}).to_string()).unwrap();

        let ran = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let person = Arc::new(Person { asked: std::sync::Mutex::new(Vec::new()), answer: std::sync::Mutex::new(ApprovalOutcome::Approved(ApprovalScope::Once)) });
        let host = ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("audit.jsonl"))));
        host.set_approver(person.clone());
        host.register(Arc::new(Tools(ran.clone()))).unwrap();
        host.set_hooks(for_host(&dir).expect("the file has hooks"));
        let call = |tool: &str| -> ToolResult {
            let mut ctx = RequestContext::new(Caller::Agent);
            ctx.agent_run_id = "run_hooks".into();
            host.call_blocking(ToolRequest { tool: tool.into(), arguments: json!({"path": "a.txt"}), context: ctx })
        };
        let asked = || person.asked.lock().unwrap().len();
        let times = |tool: &str| ran.lock().unwrap().iter().filter(|t| *t == tool).count();

        // A hook with nothing to say, or that says "allow", changes nothing: the call runs and nobody is asked.
        assert!(call("fs.read").ok && call("fs.allow").ok);
        assert_eq!(asked(), 0);

        // A hook that asks makes a call the policy lets through ask, with its own words in the question.
        let wrote = call("fs.write");
        assert!(wrote.ok && wrote.approval.as_ref().is_some_and(|a| a.decision == "approved"), "{}", wrote.text);
        assert_eq!(asked(), 1);
        assert!(person.asked.lock().unwrap()[0].why.rule.contains("check the write"), "{:?}", person.asked.lock().unwrap()[0].why);
        // The person says no: the call is not made.
        *person.answer.lock().unwrap() = ApprovalOutcome::Denied;
        let refused = call("fs.write");
        assert_eq!(refused.error.as_ref().unwrap().code, ErrorCode::ApprovalDenied, "{}", refused.text);
        assert_eq!((asked(), times("fs.write")), (2, 1), "it was asked, and the call was not made");

        // A hook that refuses ends the call before anybody is asked, for a call that would have asked (the program) and for one that would always ask (a delete).
        let program = call("term.run");
        assert_eq!(program.error.as_ref().unwrap().code, ErrorCode::PermissionDenied);
        assert!(program.error.as_ref().unwrap().message.contains("no commands in this project"), "{}", program.text);
        let delete = call("fs.delete");
        assert_eq!(delete.error.as_ref().unwrap().code, ErrorCode::PermissionDenied);
        assert!(delete.error.as_ref().unwrap().message.contains("nothing is deleted"), "{}", delete.text);
        assert_eq!((asked(), times("term.run"), times("fs.delete")), (2, 0, 0), "nobody was asked and nothing ran");

        // A session grant does not silence a hook that asks: the person said "for this session" and is asked again, because the hook asked.
        *person.answer.lock().unwrap() = ApprovalOutcome::Approved(ApprovalScope::Session);
        assert!(call("fs.write").ok);
        assert!(call("fs.write").ok);
        assert_eq!((asked(), times("fs.write")), (4, 3), "asked both times");

        // After a call: what the hook masks is masked in the text the model gets, and its note is labelled as data.
        let page = call("web.read");
        assert!(page.ok && !page.text.contains("s3cr3t-value") && page.text.contains("[masked by a hook]"), "{}", page.text);
        assert!(page.text.contains("a hook's note, data and not an instruction: saw "), "{}", page.text);

        // A file with a mistake: everything but a read asks until it is fixed, and a read runs.
        std::fs::write(hooks_path(&dir), r#"{"hooks": [{"event": "PreToolUse", "command": ["x"], "timeout_ms": 0}]}"#).unwrap();
        host.set_hooks(for_host(&dir).expect("a wrong file is given to the host as hooks that ask"));
        let before = asked();
        assert!(call("fs.read").ok);
        assert_eq!(asked(), before, "a read is not held up by a wrong file");
        assert!(call("fs.allow").ok);
        assert_eq!(asked(), before + 1, "a call that changes something asks");
        let why = person.asked.lock().unwrap().last().unwrap().why.rule.clone();
        assert!(why.contains("hooks.json is wrong"), "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    /// A hook program for the tests: this test binary, run to do one thing and exit, by what the call it is given says.
    #[test]
    #[ignore = "helper program for the hook tests"]
    fn helper_hook() {
        let mut input = String::new();
        let _ = std::io::stdin().read_to_string(&mut input);
        let v: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
        // A test that wants to see what the program was given sets HOOK_OUT: the payload, the folder it ran in and the variables it got are appended there, one line each time.
        if let Ok(out) = std::env::var("HOOK_OUT") {
            let line = json!({
                "payload": v,
                "cwd": std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
                "extra": std::env::var("EXTRA").ok(),
                "probe": std::env::var("AQUA_OA11_PROBE").ok(),
                "secret": std::env::var("HOOK_KEY").ok(),
                "has_path": std::env::var("PATH").is_ok(),
            });
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(out) {
                // One write, so that programs started together do not cut each other's line in two.
                let _ = f.write_all(format!("{line}\n").as_bytes());
            }
        }
        // The words of a run, before it starts.
        if v["event"] == "UserPromptSubmit" {
            let head = v["goal_head"].as_str().unwrap_or("");
            if head.contains("forbidden") {
                eprintln!("no such requests here\nsecond line");
                std::process::exit(2);
            }
            if head.contains("ask me") {
                println!("{}", json!({"decision": "ask", "reason": "check this request"}));
            }
            return;
        }
        match v["tool"].as_str().unwrap_or("") {
            "term.run" => {
                eprintln!("no commands in this project\nsecond line");
                std::process::exit(2);
            }
            "fs.write" => println!("{}", json!({"decision": "ask", "reason": "check the write"})),
            "fs.delete" => {
                // A hook that says too much: its reason holds the secret it was given, if it was given one.
                let secret = std::env::var("HOOK_KEY").map(|s| format!(" ({s})")).unwrap_or_default();
                println!("{}", json!({"decision": "deny", "reason": format!("nothing is deleted{secret}")}));
            }
            "fs.allow" => println!("{}", json!({"decision": "allow"})),
            "fs.fail" => std::process::exit(3),
            "fs.slow" => std::thread::sleep(Duration::from_secs(20)),
            "web.read" => println!("{}", json!({"redact": ["s3cr3t-value"], "note": format!("saw {} chars", v["result_text"].as_str().unwrap_or("").chars().count())})),
            _ => {}
        }
    }

    fn me() -> Vec<String> {
        [std::env::current_exe().unwrap().to_string_lossy().to_string()].into_iter().chain(["--ignored", "--exact", "hooks::tests::helper_hook", "--nocapture", "--test-threads=1"].map(String::from)).collect()
    }

    fn hooks_for(event: &str, tools: &[&str], timeout_ms: u64) -> CommandHooks {
        let text = json!({"hooks": [{"event": event, "tools": tools, "command": me(), "timeout_ms": timeout_ms}]}).to_string();
        CommandHooks::from_text(&text).unwrap()
    }

    fn call(tool: &str) -> HookCall {
        HookCall { event: "PreToolUse", tool: tool.into(), args: json!({"path": "a.txt"}), classes: vec!["WRITE".into()], target: "a.txt".into(), decision: "auto", caller: "agent".into(), run_id: "run1".into(), ..Default::default() }
    }

    #[test]
    fn a_program_hook_can_refuse_with_exit_code_two_and_a_reason_and_can_ask_or_refuse_in_json() {
        let h = hooks_for("PreToolUse", &[], 5_000);
        assert_eq!(h.pre_tool(&call("term.run")), HookAnswer::Deny("no commands in this project".into()));
        assert_eq!(h.pre_tool(&call("fs.write")), HookAnswer::Ask("check the write".into()));
        assert_eq!(h.pre_tool(&call("fs.delete")), HookAnswer::Deny("nothing is deleted".into()));
        // Nothing to say, and "allow", change nothing: a hook cannot loosen.
        assert_eq!(h.pre_tool(&call("fs.read")), HookAnswer::Pass);
        assert_eq!(h.pre_tool(&call("fs.allow")), HookAnswer::Pass);
    }

    #[test]
    fn a_hook_that_fails_or_does_not_answer_in_time_makes_the_call_ask() {
        let failed = hooks_for("PreToolUse", &[], 5_000).pre_tool(&call("fs.fail"));
        assert!(matches!(&failed, HookAnswer::Ask(m) if m.contains("code 3")), "{failed:?}");
        let started = Instant::now();
        let slow = hooks_for("PreToolUse", &[], 400).pre_tool(&call("fs.slow"));
        assert!(matches!(&slow, HookAnswer::Ask(m) if m.contains("did not answer in 400 ms")), "{slow:?}");
        assert!(started.elapsed() < Duration::from_secs(8), "the program was stopped, not waited for");
        let missing = CommandHooks::from_text(r#"{"hooks": [{"event": "PreToolUse", "command": ["C:/no/such/program.exe"]}]}"#).unwrap();
        assert!(matches!(missing.pre_tool(&call("fs.write")), HookAnswer::Ask(m) if m.contains("did not start")));
    }

    #[test]
    fn only_the_hooks_for_the_tool_are_run_and_a_refusal_wins_over_an_ask() {
        let by_tool = hooks_for("PreToolUse", &["term.*"], 5_000);
        assert!(matches!(by_tool.pre_tool(&call("term.run")), HookAnswer::Deny(_)));
        assert_eq!(by_tool.pre_tool(&call("fs.write")), HookAnswer::Pass, "a hook for term.* is not run for fs.write");
        let text = json!({"hooks": [
            {"event": "PreToolUse", "tools": ["fs.write"], "command": me()},
            {"event": "PreToolUse", "tools": ["fs.w*"], "command": me()},
            {"event": "PostToolUse", "command": me()},
        ]})
        .to_string();
        let both = CommandHooks::from_text(&text).unwrap();
        assert_eq!(both.pre_tool(&call("fs.write")), HookAnswer::Ask("check the write".into()), "two asks are one ask; a post hook is not a pre hook");
        assert!(tool_matches(&["fs.*".into()], "fs.read") && tool_matches(&[], "anything") && !tool_matches(&["fs.read".into()], "fs.readx") && tool_matches(&["*".into()], "x"));
    }

    #[test]
    fn a_post_hook_masks_text_and_adds_a_note_and_nothing_else_comes_from_it() {
        let h = hooks_for("PostToolUse", &["web.read"], 5_000);
        let answer = h.post_tool(&call("web.read"), true, "the page says s3cr3t-value twice");
        assert_eq!(answer.redact, vec!["s3cr3t-value".to_string()]);
        assert_eq!(answer.note.as_deref(), Some("saw 32 chars"));
        assert_eq!(h.post_tool(&call("fs.read"), true, "x"), PostAnswer::default(), "a hook for web.read is not run for fs.read");
        // A post hook that fails is ignored: it never decides anything.
        let failing = hooks_for("PostToolUse", &[], 5_000).post_tool(&call("fs.fail"), true, "x");
        assert_eq!(failing, PostAnswer::default());
    }

    #[test]
    fn a_file_with_one_mistake_is_refused_whole_and_says_which_hook() {
        let bad = |hook: &str| CommandHooks::from_text(&format!(r#"{{"hooks": [{hook}]}}"#)).unwrap_err();
        assert!(bad(r#"{"event": "BeforeTool", "command": ["x"]}"#).contains("hook 1: the event is"));
        assert!(bad(r#"{"event": "PreToolUse", "command": []}"#).contains("a program and its arguments"));
        assert!(bad(r#"{"event": "PreToolUse", "command": ["  "]}"#).contains("a program and its arguments"));
        assert!(bad(r#"{"event": "PreToolUse", "command": ["x"], "timeout_ms": 0}"#).contains("timeout_ms"));
        assert!(bad(r#"{"event": "PreToolUse", "command": ["x"], "timeout_ms": 60000}"#).contains("timeout_ms"));
        assert!(bad(r#"{"event": "PreToolUse", "command": ["x"], "tools": [""]}"#).contains("a tool pattern"));
        assert!(bad(r#"{"event": "PreToolUse", "command": ["x"], "decision": "allow"}"#).contains("not valid"), "an unknown field is a mistake");
        for text in ["", "[]", "{}", r#"{"hooks": 1}"#, "not json"] {
            assert!(CommandHooks::from_text(text).is_err(), "{text:?}");
        }
        let many = (0..33).map(|_| r#"{"event": "Stop", "command": ["x"]}"#).collect::<Vec<_>>().join(",");
        assert!(CommandHooks::from_text(&format!(r#"{{"hooks": [{many}]}}"#)).unwrap_err().contains("at most 32"));
        assert!(CommandHooks::from_text(r#"{"hooks": []}"#).unwrap().is_empty());
    }

    #[test]
    fn a_missing_file_is_no_hooks_and_a_wrong_one_makes_everything_but_a_read_ask() {
        let dir = std::env::temp_dir().join(format!("aqua_hooks_file_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_hooks(&dir).unwrap().is_empty());
        std::fs::write(hooks_path(&dir), r#"{"hooks": [{"event": "PreToolUse", "command": ["x"], "timeout_ms": 0}]}"#).unwrap();
        let why = load_hooks(&dir).unwrap_err();
        assert!(why.contains("timeout_ms"), "{why}");
        assert_eq!(std::fs::read_to_string(hooks_path(&dir)).unwrap().contains("timeout_ms"), true, "the file is left as it was");
        let broken = BrokenHooks(why);
        let mut read = call("fs.read");
        read.classes = vec!["READ".into()];
        assert_eq!(broken.pre_tool(&read), HookAnswer::Pass);
        assert!(matches!(broken.pre_tool(&call("fs.write")), HookAnswer::Ask(m) if m.contains("hooks.json is wrong")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- hooks v2 (card OA11-01, task EX-04) ----

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_hooks_v2_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("data")).unwrap();
        std::fs::create_dir_all(d.join("proj")).unwrap();
        d
    }

    /// The environment a test gives its hooks: one secret, the folders under `dir` as the folders of the file tools, `dir/data` as the data folder, and a list of the secrets it was told of.
    fn test_env(dir: &Path, told: Arc<Mutex<Vec<String>>>) -> HookEnv {
        let allowed = dir.to_path_buf();
        let data = dir.join("data");
        HookEnv {
            secret: Arc::new(|name| (name == "mine/key").then(|| "hunter2-hook-secret".to_string())),
            cwd_allowed: Arc::new(move |p| p.starts_with(std::fs::canonicalize(&allowed).unwrap_or_else(|_| allowed.clone()))),
            data_dir: Arc::new(move || data.clone()),
            register_secret: Arc::new(move |s| told.lock().unwrap().push(s.to_string())),
        }
    }

    fn quiet_env(dir: &Path) -> HookEnv {
        test_env(dir, Arc::new(Mutex::new(Vec::new())))
    }

    /// The lines the helper program wrote to `out`, as JSON, once there are `n` of them (the programs of the events nobody waits for run on threads of their own).
    fn wait_for_lines(out: &Path, n: usize) -> Vec<Value> {
        let started = Instant::now();
        loop {
            let lines: Vec<Value> = std::fs::read_to_string(out).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
            if lines.len() >= n || started.elapsed() > Duration::from_secs(20) {
                return lines;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn file_text(hooks: Value) -> String {
        json!({ "hooks": hooks }).to_string()
    }

    #[test]
    fn the_events_that_are_not_calls_are_given_to_the_hooks_that_ask_for_them_on_their_own_threads_and_change_nothing() {
        let d = temp("events");
        let out = d.join("out.jsonl");
        let outs = out.to_string_lossy().to_string();
        let text = file_text(json!([
            {"event": "PostToolUseFailure", "tools": ["fs.*"], "command": me(), "env": {"HOOK_OUT": outs}},
            {"event": "PostToolUseFailure", "tools": ["web.*"], "command": me(), "env": {"HOOK_OUT": outs}},
            {"event": "FileChanged", "command": me(), "env": {"HOOK_OUT": outs}},
            {"event": "RunStart", "command": me(), "env": {"HOOK_OUT": outs}},
            {"event": "Stop", "command": me(), "env": {"HOOK_OUT": outs}},
        ]));
        let hooks = CommandHooks::from_text(&text).unwrap().with_env(quiet_env(&d));
        assert!(hooks.wants_event("RunStart") && hooks.wants_event("PostToolUseFailure") && !hooks.wants_event("UserPromptSubmit") && !hooks.wants_event("PreToolUse"));
        assert_eq!(hooks.events(), vec!["PostToolUseFailure", "RunStart", "FileChanged", "Stop"]);
        let at = |event: &'static str, tool: &str| HookEvent {
            event,
            run_id: "run_e".into(),
            project: d.join("proj").to_string_lossy().to_string(),
            caller: "agent".into(),
            tool: tool.into(),
            target: "src/a.rs".into(),
            decision: "denied_by_policy".into(),
            error_code: Some("PERMISSION_DENIED".into()),
            message: "outside the folders".into(),
            ..Default::default()
        };
        // `fs.delete` is a tool the helper program answers to with a refusal: for an event that is not the words of a run the answer is not read at all.
        for ev in [at("PostToolUseFailure", "fs.delete"), at("FileChanged", "fs.write"), at("RunStart", ""), at("Stop", "")] {
            assert_eq!(hooks.on_event(&ev), HookAnswer::Pass, "{}", ev.event);
        }
        let lines = wait_for_lines(&out, 4);
        let mut seen: Vec<String> = lines.iter().map(|l| l["payload"]["event"].as_str().unwrap_or("").to_string()).collect();
        seen.sort();
        assert_eq!(seen, vec!["FileChanged", "PostToolUseFailure", "RunStart", "Stop"], "the web.* failure hook was not run for an fs.* tool: {lines:#?}");
        let failure = lines.iter().find(|l| l["payload"]["event"] == "PostToolUseFailure").unwrap();
        let p = &failure["payload"];
        assert_eq!((p["tool"].as_str(), p["error_code"].as_str(), p["decision"].as_str(), p["target"].as_str(), p["message"].as_str(), p["run_id"].as_str()), (Some("fs.delete"), Some("PERMISSION_DENIED"), Some("denied_by_policy"), Some("src/a.rs"), Some("outside the folders"), Some("run_e")));
        assert!(p["project"].as_str().unwrap().ends_with("proj"), "{p}");
        // `Stop` as it was: the run and why it stopped.
        hooks.stop("run_s", "final");
        let lines = wait_for_lines(&out, 5);
        assert!(lines.iter().any(|l| l["payload"]["event"] == "Stop" && l["payload"]["stopped_reason"] == "final" && l["payload"]["run_id"] == "run_s"), "{lines:#?}");
    }

    #[test]
    fn the_words_of_a_run_are_checked_before_it_starts_and_a_refusal_or_a_question_is_the_answer() {
        let d = temp("prompt");
        let hooks = CommandHooks::from_text(&file_text(json!([{"event": "UserPromptSubmit", "command": me()}]))).unwrap().with_env(quiet_env(&d));
        let said = |goal: &str| hooks.on_event(&HookEvent { event: "UserPromptSubmit", run_id: "run_p".into(), caller: "agent".into(), route: "agent".into(), goal_head: goal.into(), ..Default::default() });
        assert_eq!(said("delete the forbidden folder"), HookAnswer::Deny("no such requests here".into()));
        assert_eq!(said("please ask me first"), HookAnswer::Ask("check this request".into()));
        assert_eq!(said("tidy the downloads"), HookAnswer::Pass);
        // A hook for the words of a run that cannot run asks (a check that cannot run never lets a run through unseen).
        let broken = CommandHooks::from_text(&file_text(json!([{"event": "UserPromptSubmit", "command": ["C:/no/such/program.exe"]}]))).unwrap().with_env(quiet_env(&d));
        assert!(matches!(broken.on_event(&HookEvent { event: "UserPromptSubmit", goal_head: "x".into(), ..Default::default() }), HookAnswer::Ask(m) if m.contains("did not start")));
        // There is no tool in it, so no list of tools.
        let e = CommandHooks::from_text(&file_text(json!([{"event": "UserPromptSubmit", "tools": ["fs.*"], "command": ["x"]}]))).unwrap_err();
        assert!(e.contains("not about a tool"), "{e}");
        assert!(CommandHooks::from_text(&file_text(json!([{"event": "RunStart", "tools": ["*"], "command": ["x"]}]))).is_err());
    }

    #[test]
    fn a_hook_runs_in_the_folder_it_asks_for_with_the_variables_it_declares_and_none_of_the_others() {
        let d = temp("env_cwd");
        // A variable of this process that has nothing to do with the hook: it must not reach the program.
        std::env::set_var("AQUA_OA11_PROBE", "leaked");
        let out = d.join("out.jsonl");
        let outs = out.to_string_lossy().to_string();
        let proj = d.join("proj");
        let hooks = CommandHooks::from_text(&file_text(json!([
            {"event": "PreToolUse", "command": me(), "cwd": "project", "env": {"HOOK_OUT": outs, "EXTRA": "hello"}},
            {"event": "PreToolUse", "tools": ["fs.data"], "command": me(), "cwd": "data", "env": {"HOOK_OUT": outs}},
            {"event": "PreToolUse", "tools": ["fs.abs"], "command": me(), "cwd": d.join("proj").to_string_lossy(), "env": {"HOOK_OUT": outs}},
        ])))
        .unwrap()
        .with_env(quiet_env(&d));
        let mut c = call("fs.thing");
        c.project = proj.to_string_lossy().to_string();
        assert_eq!(hooks.pre_tool(&c), HookAnswer::Pass);
        let lines = wait_for_lines(&out, 1);
        let first = &lines[0];
        let canon = |s: &str| std::fs::canonicalize(s).unwrap();
        assert_eq!(canon(first["cwd"].as_str().unwrap()), canon(&c.project), "ran in the project: {first}");
        assert_eq!(first["payload"]["cwd"], json!(c.project), "and told so");
        assert_eq!(first["payload"]["project"], json!(c.project));
        assert_eq!((first["extra"].as_str(), first["probe"].clone(), first["has_path"].clone()), (Some("hello"), Value::Null, json!(true)), "exactly the scrubbed list and the declared variables: {first}");
        // The data folder, and a full path that is inside the folders of the file tools.
        let mut data_call = call("fs.data");
        data_call.project = c.project.clone();
        assert_eq!(hooks.pre_tool(&data_call), HookAnswer::Pass);
        let mut abs_call = call("fs.abs");
        abs_call.project = c.project.clone();
        assert_eq!(hooks.pre_tool(&abs_call), HookAnswer::Pass);
        let lines = wait_for_lines(&out, 5);
        assert_eq!(lines.len(), 5, "the first hook fits every call (3), the other two one call each: {lines:#?}");
        assert!(lines.iter().any(|l| canon(l["cwd"].as_str().unwrap()) == canon(&d.join("data").to_string_lossy())), "the data folder");
        // No project to run in: the hook does not run and the call asks, with the reason.
        let ask = hooks.pre_tool(&call("fs.thing"));
        assert!(matches!(&ask, HookAnswer::Ask(m) if m.contains("did not start") && m.contains("this run has none")), "{ask:?}");
        // A full path outside the folders of the file tools is refused when the hook runs, and one that is not there too.
        let outside = std::env::temp_dir().join(format!("aqua_hooks_outside_{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        let outside_hooks = CommandHooks::from_text(&file_text(json!([{"event": "PreToolUse", "command": me(), "cwd": outside.to_string_lossy()}]))).unwrap().with_env(quiet_env(&d));
        assert!(matches!(outside_hooks.pre_tool(&call("fs.thing")), HookAnswer::Ask(m) if m.contains("not inside the folders")));
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_name_the_hooks_may_not_set_and_a_value_that_is_not_one_line_are_refused_when_the_file_is_read() {
        let bad = |env: Value| CommandHooks::from_text(&file_text(json!([{"event": "PreToolUse", "command": ["x"], "env": env}]))).unwrap_err();
        for name in ["AQUA_API_TOKEN", "AQUA_UI_TOKEN", "aqua_api_token", "GITHUB_TOKEN", "MY_SECRET", "DB_PASSWORD", "NpmTokenX", "client_secret_id"] {
            let e = bad(json!({ name: "x" }));
            assert!(e.contains("never given a variable that is called a token, a secret or a password"), "{name}: {e}");
        }
        assert!(bad(json!({"EXTRA": "one\ntwo"})).contains("one line"));
        assert!(bad(json!({"EXTRA": "one\r"})).contains("one line"));
        assert!(bad(json!({"EXTRA": "x".repeat(1001)})).contains("at most 1000"));
        assert!(bad(json!({"1ABC": "x"})).contains("not a name for a variable"));
        assert!(bad(json!({"A B": "x"})).contains("not a name for a variable"));
        assert!(bad(json!({"EXTRA": {"secret": "has space"}})).contains("not the name of a secret"));
        assert!(bad(json!({"EXTRA": {"nope": "x"}})).contains("not valid"));
        let many: serde_json::Map<String, Value> = (0..17).map(|i| (format!("V{i}"), json!("x"))).collect();
        assert!(bad(Value::Object(many)).contains("at most 16 variables"));
        // What is fine: a name with a digit, a secret by name, an empty map.
        assert!(CommandHooks::from_text(&file_text(json!([{"event": "PreToolUse", "command": ["x"], "env": {"RUST_LOG": "warn", "A1_B": {"secret": "mine/key"}}}]))).is_ok());
        // `cwd`: project, data, or a full path with no `..`.
        let cwd = |c: &str| CommandHooks::from_text(&file_text(json!([{"event": "PreToolUse", "command": ["x"], "cwd": c}])));
        assert!(cwd("project").is_ok() && cwd("data").is_ok() && cwd("C:/work/proj").is_ok());
        for wrong in ["src", "../x", "C:/work/../x", "~/x", ""] {
            let e = cwd(wrong).unwrap_err();
            assert!(e.contains("cwd is"), "{wrong:?}: {e}");
        }
    }

    #[test]
    fn a_secret_is_read_when_the_hook_runs_given_to_the_program_and_to_nobody_else() {
        let d = temp("secret");
        let out = d.join("out.jsonl");
        let told = Arc::new(Mutex::new(Vec::new()));
        let text = file_text(json!([{"event": "PreToolUse", "tools": ["fs.write", "fs.delete"], "command": me(), "env": {"HOOK_OUT": out.to_string_lossy(), "HOOK_KEY": {"secret": "mine/key"}}}]));
        let hooks = CommandHooks::from_text(&text).unwrap().with_env(test_env(&d, told.clone()));
        assert!(!text.contains("hunter2") && !format!("{hooks:?}").contains("hunter2"), "the file and the hooks hold the name of the secret, never its value");
        // The program was given it, and the host was told of it (so that it masks it).
        assert_eq!(hooks.pre_tool(&call("fs.write")), HookAnswer::Ask("check the write".into()));
        let lines = wait_for_lines(&out, 1);
        assert_eq!(lines[0]["secret"], "hunter2-hook-secret");
        assert_eq!(told.lock().unwrap().as_slice(), ["hunter2-hook-secret".to_string()]);
        // A secret that is not stored: the hook does not run and the call asks, and the question does not hold any value.
        let missing = CommandHooks::from_text(&file_text(json!([{"event": "PreToolUse", "command": me(), "env": {"HOOK_KEY": {"secret": "mine/other"}}}]))).unwrap().with_env(test_env(&d, told.clone()));
        let ask = missing.pre_tool(&call("fs.write"));
        assert!(matches!(&ask, HookAnswer::Ask(m) if m.contains("the secret for HOOK_KEY is not stored") && !m.contains("hunter2")), "{ask:?}");
        // The hooks of a repository may not ask for a secret at all.
        let e = CommandHooks::from_pack_text(&text).unwrap_err();
        assert!(e.contains("may not ask for a secret"), "{e}");
    }

    #[test]
    fn a_secret_a_hook_prints_is_masked_by_the_host_that_was_told_of_it() {
        use aqua_tool_host::{AuditLedger, Caller, ErrorCode, Mode, PolicyEngine, RequestContext, ToolHost, ToolRequest};
        let d = temp("secret_masked");
        let ran = Arc::new(Mutex::new(Vec::<String>::new()));
        let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(d.join("audit.jsonl")))));
        host.register(Arc::new(Tools(ran.clone()))).unwrap();
        let sink = host.clone();
        let env = HookEnv { register_secret: Arc::new(move |s| sink.register_secret(s)), ..quiet_env(&d) };
        // The hook's refusal of `fs.delete` says the secret out loud (the helper program does): the person is shown the refusal, the model is given it, and the ledger writes it down.
        let text = file_text(json!([{"event": "PreToolUse", "tools": ["fs.delete"], "command": me(), "env": {"HOOK_KEY": {"secret": "mine/key"}}}]));
        host.set_hooks(Arc::new(CommandHooks::from_text(&text).unwrap().with_env(env)));
        let mut ctx = RequestContext::new(Caller::Agent);
        ctx.agent_run_id = "run_masked".into();
        let res = host.call_blocking(ToolRequest { tool: "fs.delete".into(), arguments: json!({"path": "a.txt"}), context: ctx });
        assert_eq!(res.error.as_ref().unwrap().code, ErrorCode::PermissionDenied);
        assert!(res.text.contains("nothing is deleted") && !res.text.contains("hunter2-hook-secret"), "{}", res.text);
        let ledger = std::fs::read_to_string(d.join("audit.jsonl")).unwrap();
        assert!(ledger.contains("denied_by_hook") && !ledger.contains("hunter2-hook-secret"), "{ledger}");
        assert!(host.recent_denials().iter().all(|x| !x.reason.contains("hunter2-hook-secret")));
    }

    #[test]
    fn the_hooks_of_a_run_may_run_for_a_minute_in_all_and_after_that_what_is_not_a_read_asks() {
        let d = temp("budget");
        let out = d.join("out.jsonl");
        // A budget of one millisecond: any program takes longer, so one call spends it.
        let text = file_text(json!([{"event": "PreToolUse", "command": me(), "env": {"HOOK_OUT": out.to_string_lossy()}}]));
        let hooks = CommandHooks::from_text(&text).unwrap().with_env(quiet_env(&d)).with_budget(Duration::from_millis(1));
        let mut write = call("fs.write");
        write.run_id = "run_b".into();
        assert_eq!(hooks.pre_tool(&write), HookAnswer::Ask("check the write".into()), "the first call runs the hook");
        let second = hooks.pre_tool(&write);
        assert!(matches!(&second, HookAnswer::Ask(m) if m.contains("have used their") && m.contains("what is not a read asks")), "{second:?}");
        assert_eq!(wait_for_lines(&out, 1).len(), 1, "the hook was not run the second time");
        // A read is not held up, another run has its own time, and a call that is no part of a run has none to use up.
        let mut read = call("fs.read");
        read.run_id = "run_b".into();
        read.classes = vec!["READ".into()];
        assert_eq!(hooks.pre_tool(&read), HookAnswer::Pass);
        let mut other = call("fs.write");
        other.run_id = "run_c".into();
        assert_eq!(hooks.pre_tool(&other), HookAnswer::Ask("check the write".into()));
        let mut free = call("fs.write");
        free.run_id = String::new();
        assert_eq!(hooks.pre_tool(&free), HookAnswer::Ask("check the write".into()));
        assert_eq!(hooks.pre_tool(&free), HookAnswer::Ask("check the write".into()), "no run, no budget");
        assert_eq!(RUN_BUDGET, Duration::from_secs(60), "a minute is what a run gets");
    }

    #[test]
    fn after_a_call_the_hook_is_told_the_target_and_the_decision_in_its_payload() {
        let d = temp("payload");
        let out = d.join("out.jsonl");
        let hooks = CommandHooks::from_text(&file_text(json!([{"event": "PostToolUse", "command": me(), "env": {"HOOK_OUT": out.to_string_lossy()}}]))).unwrap().with_env(quiet_env(&d));
        let mut c = call("web.read");
        c.event = "PostToolUse";
        c.decision = "ask";
        c.project = d.join("proj").to_string_lossy().to_string();
        let _ = hooks.post_tool(&c, true, "the page");
        let lines = wait_for_lines(&out, 1);
        let p = &lines[0]["payload"];
        assert_eq!((p["target"].as_str(), p["decision"].as_str(), p["ok"].clone(), p["result_text"].as_str()), (Some("a.txt"), Some("ask"), json!(true), Some("the page")));
        assert_eq!(p["project"], json!(c.project));
    }

    #[test]
    fn a_hostile_hooks_file_in_a_repository_is_not_applied_before_the_person_trusts_it_as_code() {
        use crate::project_context::{begin_run, end_run};
        use crate::project_pack::{scan, trust, Kind};
        let d = temp("pack_hooks");
        std::fs::create_dir_all(d.join("ws/proj/.aqua")).unwrap();
        let out = d.join("pack_out.jsonl");
        // A hostile hooks file: it refuses `term.run` (and so every call of that kind), and it records where it ran.
        let hostile = file_text(json!([{"event": "PreToolUse", "command": me(), "env": {"HOOK_OUT": out.to_string_lossy()}}]));
        std::fs::write(d.join("ws/proj/.aqua/hooks.json"), &hostile).unwrap();
        let data = d.join("data");
        let key = [5u8; 32];
        let root = d.join("ws/proj");
        let roots = vec![d.join("ws")];
        let mine = PackAware::new(None);
        let asked = |run: &str| {
            let mut c = call("term.run");
            c.run_id = run.into();
            c.project = root.to_string_lossy().to_string();
            mine.pre_tool(&c)
        };
        crate::flags::with_overrides(&[("AQUA_PROJECT_PACK", Some("on"))], || {
            // Not trusted: the file is read for the page that shows it, and nothing of it is applied.
            begin_run("run_hostile_1", "fix the bug in proj", &roots, &data, &|| Some(key.to_vec()));
            assert_eq!(asked("run_hostile_1"), HookAnswer::Pass);
            assert!(!out.exists(), "the program was never started");
            // Trusted as text only: still nothing, it is code.
            let pack = scan(&root.join(".aqua"));
            trust(&data, &key, &root, &pack, &[Kind::Code], &Default::default()).unwrap();
            begin_run("run_hostile_2", "fix the bug in proj", &roots, &data, &|| Some(key.to_vec()));
            let denied = asked("run_hostile_2");
            assert_eq!(denied, HookAnswer::Deny("no commands in this project".into()));
            // It ran in the project, and a read call is let through by it as by any hook that says nothing.
            let ran = wait_for_lines(&out, 1);
            assert_eq!(std::fs::canonicalize(ran[0]["cwd"].as_str().unwrap()).unwrap(), std::fs::canonicalize(&root).unwrap(), "a repository's hook runs in the project");
            // The run that began before the file changed keeps the file it was trusted with; the next run is not trusted.
            std::fs::write(d.join("ws/proj/.aqua/hooks.json"), format!("{hostile} ")).unwrap();
            assert!(matches!(asked("run_hostile_2"), HookAnswer::Deny(_)), "the run in progress keeps what it began with");
            begin_run("run_hostile_3", "fix the bug in proj", &roots, &data, &|| Some(key.to_vec()));
            assert_eq!(asked("run_hostile_3"), HookAnswer::Pass, "a byte more and the pack is not trusted any more");
            // No key: nothing is trusted either.
            std::fs::write(d.join("ws/proj/.aqua/hooks.json"), &hostile).unwrap();
            begin_run("run_hostile_4", "fix the bug in proj", &roots, &data, &|| None);
            assert_eq!(asked("run_hostile_4"), HookAnswer::Pass);
            for run in ["run_hostile_1", "run_hostile_2", "run_hostile_3", "run_hostile_4"] {
                end_run(run);
            }
        });
        // With the setting off the pack is not even read for a run: there is no context to hold anything.
        begin_run("run_hostile_5", "fix the bug in proj", &roots, &data, &|| Some(key.to_vec()));
        assert_eq!(asked("run_hostile_5"), HookAnswer::Pass);
        assert!(crate::project_context::for_run("run_hostile_5").is_none());
        end_run("run_hostile_5");
    }

    #[test]
    fn a_trusted_hooks_file_of_a_repository_can_only_tighten_and_a_broken_one_makes_what_is_not_a_read_ask() {
        use crate::project_context::{begin_run, end_run};
        use crate::project_pack::{scan, trust, Kind};
        let d = temp("pack_hooks_tighten");
        std::fs::create_dir_all(d.join("ws/proj/.aqua")).unwrap();
        let key = [6u8; 32];
        let (data, root, roots) = (d.join("data"), d.join("ws/proj"), vec![d.join("ws")]);
        let trust_now = |text: &str| {
            std::fs::write(root.join(".aqua/hooks.json"), text).unwrap();
            trust(&data, &key, &root, &scan(&root.join(".aqua")), &[Kind::Code], &Default::default()).unwrap();
        };
        // The person's own hooks refuse `fs.delete`; the repository's say nothing about it: the refusal stands, and the repository's hook is not asked.
        let person = Arc::new(CommandHooks::from_text(&file_text(json!([{"event": "PreToolUse", "tools": ["fs.delete"], "command": me()}]))).unwrap().with_env(quiet_env(&d)));
        let both = PackAware::new(Some(person.clone() as Arc<dyn Hooks>));
        trust_now(&file_text(json!([{"event": "PreToolUse", "tools": ["fs.write"], "command": me()}])));
        crate::flags::with_overrides(&[("AQUA_PROJECT_PACK", Some("on"))], || {
            begin_run("run_tight_1", "fix the bug in proj", &roots, &data, &|| Some(key.to_vec()));
            let mut c = call("fs.delete");
            c.run_id = "run_tight_1".into();
            c.project = root.to_string_lossy().to_string();
            assert_eq!(both.pre_tool(&c), HookAnswer::Deny("nothing is deleted".into()));
            // The repository's hook asks about writes; nothing it says allows what the person's hooks or the policy refuse.
            c.tool = "fs.write".into();
            assert_eq!(both.pre_tool(&c), HookAnswer::Ask("check the write".into()));
            // A trusted file that cannot be used: every call that is not a read asks until it is fixed.
            trust_now("this is not json");
            begin_run("run_tight_2", "fix the bug in proj", &roots, &data, &|| Some(key.to_vec()));
            let mut w = call("fs.allow");
            w.run_id = "run_tight_2".into();
            w.project = root.to_string_lossy().to_string();
            assert!(matches!(both.pre_tool(&w), HookAnswer::Ask(m) if m.contains("the project's hooks.json is wrong")));
            w.classes = vec!["READ".into()];
            assert_eq!(both.pre_tool(&w), HookAnswer::Pass, "a read is not held up");
            assert!(both.wants_event("Stop"), "with packs on the project's hooks may ask for any event");
            end_run("run_tight_1");
            end_run("run_tight_2");
        });
        assert!(!both.wants_event("RunStart"), "with packs off and no hook for it, nothing is asked for");
        // A file of a repository that asks for another folder, or a secret, or is a token by name, is not read.
        for hook in [json!({"event": "PreToolUse", "command": ["x"], "cwd": "data"}), json!({"event": "PreToolUse", "command": ["x"], "cwd": "C:/work"}), json!({"event": "PreToolUse", "command": ["x"], "env": {"GH_TOKEN": "x"}})] {
            assert!(CommandHooks::from_pack_text(&file_text(json!([hook]))).is_err());
        }
        assert!(CommandHooks::from_pack_text(&file_text(json!([{"event": "PreToolUse", "command": ["x"], "cwd": "project", "env": {"RUST_LOG": "warn"}}]))).is_ok());
    }
}

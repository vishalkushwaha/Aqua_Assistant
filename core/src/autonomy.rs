//! The autonomy setting and the limits of one agent run (card R7-18, `documentation/AUTONOMY.md`).
//!
//! Two things the person can see and change, both kept in `data/tools/autonomy.json` (written the way `full_access.json` is, read at start-up):
//!
//! * **The level.** `ask_all`, `ask_risky` (today's rules, the default) or `ask_outside`. A level is applied by the tool host's policy
//!   (`aqua_tool_host::policy::AskLevel`) and can only make a decision stricter, never looser; the policy file and full access are untouched.
//! * **The caps of one run.** The seconds since the run's first call, the number of tool calls, the number of files it changed and the number of bytes it changed. The
//!   defaults are above anything a run does today (see [`DEFAULT_CAPS`]), so a run that works now is not stopped. A run that reaches a cap is not given the call: it
//!   reads `ERROR [STOPPED_BY_LIMIT] <tool>: this run reached its limit of <what> (<n>). Tell the person what was done and what is left.`, and its record names the cap
//!   (`stopped_by`). Counters belong to the run id and are forgotten when the run ends.
//!
//! Only the UI token changes the setting (`api_gateway/autonomy_api.rs`). The tool calls of one run are counted where the agent loop dispatches them
//! (`browser_agent::call_named_tool`, through [`crate::agent_runs::scope::limit_stop`]), so the browser, the desktop and the local tools count as well as the host's;
//! what a file tool changed is counted where its result comes back (`agent_tools::call_with`).
//!
//! Not counted, and said so: the calls of a run that is replayed from a recipe (it is bounded by its recipe), a flow's own steps, and money (no tool spends any).

use aqua_tool_host::policy::AskLevel;
use aqua_tool_host::ToolHost;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use tracing::{info, warn};

const FILE: &str = "autonomy.json";

/// Where the setting is kept: `<data>/tools/autonomy.json`. `data/tools` is one of the folders no file tool can touch.
pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("tools").join(FILE)
}

/// One of the four limits of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cap {
    /// Seconds since the run's first call.
    Seconds,
    ToolCalls,
    /// Files the run changed (a file counts once, however often it is changed; a folder counts as one).
    FilesChanged,
    ChangedBytes,
}

impl Cap {
    pub const ALL: [Cap; 4] = [Cap::Seconds, Cap::ToolCalls, Cap::FilesChanged, Cap::ChangedBytes];

    /// The name of the cap in the file, the route and the run record.
    pub fn id(self) -> &'static str {
        match self {
            Cap::Seconds => "max_seconds",
            Cap::ToolCalls => "max_tool_calls",
            Cap::FilesChanged => "max_files_changed",
            Cap::ChangedBytes => "max_changed_bytes",
        }
    }

    pub fn from_id(id: &str) -> Option<Cap> {
        Cap::ALL.into_iter().find(|c| c.id() == id)
    }

    /// What the cap counts, as the sentence a run reads says it.
    pub fn what(self) -> &'static str {
        match self {
            Cap::Seconds => "seconds",
            Cap::ToolCalls => "tool calls",
            Cap::FilesChanged => "files changed",
            Cap::ChangedBytes => "bytes changed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub max_seconds: u64,
    pub max_tool_calls: u64,
    pub max_files_changed: u64,
    pub max_changed_bytes: u64,
}

impl Caps {
    pub fn get(&self, cap: Cap) -> u64 {
        match cap {
            Cap::Seconds => self.max_seconds,
            Cap::ToolCalls => self.max_tool_calls,
            Cap::FilesChanged => self.max_files_changed,
            Cap::ChangedBytes => self.max_changed_bytes,
        }
    }

    pub fn set(&mut self, cap: Cap, value: u64) {
        match cap {
            Cap::Seconds => self.max_seconds = value,
            Cap::ToolCalls => self.max_tool_calls = value,
            Cap::FilesChanged => self.max_files_changed = value,
            Cap::ChangedBytes => self.max_changed_bytes = value,
        }
    }
}

impl Default for Caps {
    fn default() -> Self {
        DEFAULT_CAPS
    }
}

/// What a run may do when the person has set nothing. Each is above what a run does today, so that a run that works now is never stopped by it (read in the code):
/// * seconds: the agent loop's own clock is 180 s for a spoken request, 900 s for a typed one, 30 minutes in the background (`queue::BACKGROUND_RUN_LIMIT`), and
///   `AQUA_AGENT_MAX_SECS` is clamped to 7,200 at most (`browser_agent::agent_run_limit`): the default is that highest value;
/// * tool calls: the loop makes at most `AGENT_MAX_ROUNDS` (12) rounds, and one round chains at most 3 calls (`AQUA_PLANNER_BATCH`, clamped to 3): 36;
/// * files and bytes: nothing limited them. A run makes at most 36 calls, so 500 files and 1 GiB are far above any run today.
pub const DEFAULT_CAPS: Caps = Caps { max_seconds: 7_200, max_tool_calls: 200, max_files_changed: 500, max_changed_bytes: 1 << 30 };

/// The most a cap may be set to, and the least (1) for all of them: a cap of 0 is not "no limit" and not "stop at once", it is refused.
pub const BIGGEST_CAPS: Caps = Caps { max_seconds: 86_400, max_tool_calls: 10_000, max_files_changed: 100_000, max_changed_bytes: 100 << 30 };
pub const SMALLEST_CAP: u64 = 1;

/// The level and the caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Settings {
    pub level: AskLevel,
    pub caps: Caps,
}

/// The label of a level and what it does, in plain words (the route gives them to the app).
pub fn level_words(level: AskLevel) -> (&'static str, &'static str) {
    match level {
        AskLevel::AskAll => (
            "Ask for everything",
            "Every call that Aqua's agent, a script, a voice request or a replayed recipe makes asks first, even a read. What you do yourself in the app is not asked about again. \
             A run in the background cannot be asked, so its calls are refused and it says so.",
        ),
        AskLevel::AskRisky => (
            "Ask for risky things (the default)",
            "Today's rules: reading and working inside the project folder run on their own, and anything that changes something elsewhere, runs a program, uses a secret, reaches \
             another machine, deletes, or has an effect on someone else's system asks. Full access, when you turn it on, only relaxes ordinary work.",
        ),
        AskLevel::AskOutside => (
            "Ask for what reaches outside",
            "Today's rules, and in addition anything that has an effect on someone else's system, or reaches a web address that is on no list, asks a fresh question every time, \
             also under full access.",
        ),
    }
}

/// Why a call was not given to the run: the cap it ran into and the value of the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopReason {
    pub cap: Cap,
    pub limit: u64,
}

impl StopReason {
    /// What the run reads in place of the tool's result.
    pub fn observation(&self, tool: &str) -> String {
        format!("ERROR [STOPPED_BY_LIMIT] {tool}: this run reached its limit of {} ({}). Tell the person what was done and what is left.", self.cap.what(), self.limit)
    }
}

/// The reason an agent run is recorded with when a cap ended it (`stopped_reason`; the cap itself is in `stopped_by`). The loop ends the run at the first refused call, so the model
/// is not asked again and no repair is tried.
pub const STOPPED_REASON: &str = "limit";

/// What a run reads in place of a call a cap refused starts with this (see [`StopReason::observation`]).
pub const OBSERVATION_PREFIX: &str = "ERROR [STOPPED_BY_LIMIT]";

/// What the person reads when a cap ended the run: which limit it reached, and what was done before it. `observation` is what the run read in place of the call; `done` are the tools
/// whose calls worked, in order.
pub fn stop_answer(observation: &str, done: &[String]) -> String {
    // "ERROR [STOPPED_BY_LIMIT] fs.write: this run reached its limit of files changed (2). Tell the person what was done and what is left."
    let reached = observation
        .split_once("]")
        .and_then(|(_, rest)| rest.split_once(": "))
        .map(|(_, sentence)| sentence.split(". Tell the person").next().unwrap_or(sentence).trim().trim_end_matches('.'))
        .filter(|s| !s.is_empty())
        .unwrap_or("this run reached a limit");
    let mut names: Vec<&str> = Vec::new();
    for tool in done {
        if !names.contains(&tool.as_str()) {
            names.push(tool);
        }
    }
    let did = if done.is_empty() {
        "Nothing had been done before that.".to_string()
    } else {
        let shown: Vec<&str> = names.iter().copied().take(6).collect();
        let more = if names.len() > shown.len() { ", and others" } else { "" };
        format!("Before that, {} call{} worked ({}{more}).", done.len(), if done.len() == 1 { "" } else { "s" }, shown.join(", "))
    };
    format!("I stopped because {reached}. {did} Nothing more was started. The limit is part of the autonomy setting: raise it if you want me to go on.")
}

// ---- reading and writing the file -------------------------------------------------------------------------------------------

/// A cap's value from JSON: a whole number from [`SMALLEST_CAP`] to the most the cap may be.
pub fn cap_value(cap: Cap, v: &Value) -> Result<u64, String> {
    let most = BIGGEST_CAPS.get(cap);
    match v.as_u64() {
        Some(n) if (SMALLEST_CAP..=most).contains(&n) => Ok(n),
        _ => Err(format!("{} must be a whole number from {SMALLEST_CAP} to {most}, not {v}", cap.id())),
    }
}

/// The settings a file's text says, and what was wrong with it. A part that is wrong gives its default and does not spoil the others; text that is not a JSON object gives
/// the defaults. Nothing here can make a level looser than the default: the default is the loosest.
pub fn parse(text: &str) -> (Settings, Vec<String>) {
    let mut settings = Settings::default();
    let value: Value = match serde_json::from_str(text.trim_start_matches('\u{feff}')) {
        Ok(v) => v,
        Err(e) => return (settings, vec![format!("it is not valid JSON ({e})")]),
    };
    let Some(object) = value.as_object() else { return (settings, vec!["it is not a JSON object".to_string()]) };
    let mut problems = Vec::new();
    for (key, v) in object {
        if v.is_null() {
            continue;
        }
        if key == "level" {
            match v.as_str().and_then(AskLevel::from_id) {
                Some(level) => settings.level = level,
                None => problems.push(format!("level {v} is not ask_all, ask_risky or ask_outside")),
            }
        } else if let Some(cap) = Cap::from_id(key) {
            match cap_value(cap, v) {
                Ok(n) => settings.caps.set(cap, n),
                Err(why) => problems.push(why),
            }
        } else {
            problems.push(format!("the field {key:?} is not known and is ignored"));
        }
    }
    (settings, problems)
}

/// The setting kept in `data_dir`. A missing file is the default; a file that cannot be read, or a part of it that is wrong, is the default for that part and is said in the log.
pub fn load(data_dir: &Path) -> Settings {
    let p = path(data_dir);
    match std::fs::read_to_string(&p) {
        Ok(text) => {
            let (settings, problems) = parse(&text);
            for why in problems {
                warn!("autonomy: {}: {why}; the default is used for it", p.display());
            }
            settings
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => {
            warn!("autonomy: {} is unreadable ({e}); the defaults are used", p.display());
            Settings::default()
        }
    }
}

/// What is written: the level, and a cap only where it differs from the default (so a default that moves in a later version moves for everybody who never chose).
pub fn file_value(settings: &Settings) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("level".into(), json!(settings.level.id()));
    for cap in Cap::ALL {
        if settings.caps.get(cap) != DEFAULT_CAPS.get(cap) {
            out.insert(cap.id().into(), json!(settings.caps.get(cap)));
        }
    }
    Value::Object(out)
}

/// Written beside the file, flushed, and renamed over it (`fsutil::write_atomic_durable`), so a crash or a power cut leaves the old state and never half of a new one.
pub fn save(data_dir: &Path, settings: &Settings) -> Result<(), String> {
    let p = path(data_dir);
    let text = serde_json::to_string_pretty(&file_value(settings)).map_err(|e| e.to_string())?;
    aqua_tool_host::fsutil::write_atomic_durable(&p, text.as_bytes()).map_err(|e| e.to_string())
}

// ---- what a changing file tool changed --------------------------------------------------------------------------------------

/// The name a tool is counted by: the model's spelling (`fs_write`, `FS.WRITE`, `host.fs.edit`) as the host's name (`fs.write`). The same rule as `agent_tools::canonical`,
/// without the list of tools it looks the result up in.
pub fn counted_name(tool: &str) -> String {
    let mut n = tool.trim().to_ascii_lowercase();
    for prefix in ["host.", "host_", "aqua.", "aqua_"] {
        if let Some(rest) = n.strip_prefix(prefix) {
            n = rest.to_string();
        }
    }
    match n.split_once(['.', '_', '-']) {
        Some((domain, rest)) => format!("{domain}.{}", rest.replace(['.', '-'], "_")),
        None => n,
    }
}

/// Does this tool change files, so that the files and bytes of the run are counted for it?
pub fn counts_changes(tool: &str) -> bool {
    matches!(counted_name(tool).as_str(), "fs.write" | "fs.edit" | "fs.edit_lines" | "fs.patch" | "fs.copy" | "fs.move" | "fs.delete")
}

/// What a call that has not run yet says it will change: the files it names (`None`: it does not say, a unified diff) and the bytes it carries (0: it does not say).
struct Plan {
    paths: Option<Vec<String>>,
    declared: u64,
}

fn text_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn plan_of(tool: &str, args: &Value) -> Option<Plan> {
    let name = counted_name(tool);
    match name.as_str() {
        "fs.write" => Some(Plan { paths: Some(vec![text_of(args, "path")?.to_string()]), declared: if text_of(args, "kind") == Some("dir") { 0 } else { text_of(args, "content").map_or(0, str::len) as u64 } }),
        "fs.edit" | "fs.edit_lines" => Some(Plan { paths: Some(vec![text_of(args, "path")?.to_string()]), declared: text_of(args, "new").map_or(0, str::len) as u64 }),
        "fs.patch" => {
            if args.get("dry_run").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            match args.get("edits").and_then(Value::as_array) {
                Some(edits) => Some(Plan {
                    paths: Some(edits.iter().filter_map(|e| text_of(e, "path").map(str::to_string)).collect()),
                    declared: edits.iter().map(|e| text_of(e, "new").map_or(0, str::len) as u64).sum(),
                }),
                None => Some(Plan { paths: None, declared: text_of(args, "diff").map_or(0, str::len) as u64 }),
            }
        }
        "fs.copy" | "fs.move" => Some(Plan { paths: Some(vec![text_of(args, "dst")?.to_string()]), declared: 0 }),
        "fs.delete" => Some(Plan { paths: Some(vec![text_of(args, "path")?.to_string()]), declared: 0 }),
        _ => None,
    }
}

/// The path a single-file call names, as the model wrote it.
fn arg_path<'a>(tool: &str, args: &'a Value) -> Option<&'a str> {
    match counted_name(tool).as_str() {
        "fs.write" | "fs.edit" | "fs.edit_lines" | "fs.delete" => text_of(args, "path"),
        "fs.copy" | "fs.move" => text_of(args, "dst"),
        _ => None,
    }
}

/// What a call that ran did, as its result says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    /// The files it changed, as the tool names them.
    pub paths: Vec<String>,
    pub bytes: u64,
}

/// The bytes under a path, as the disk says (a folder is summed over at most 10,000 entries).
fn size_on_disk(path: &str) -> u64 {
    let (mut total, mut seen) = (0u64, 0u32);
    let mut stack = vec![PathBuf::from(path)];
    while let Some(p) = stack.pop() {
        seen += 1;
        if seen > 10_000 {
            break;
        }
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&p) {
                stack.extend(entries.filter_map(Result::ok).map(|e| e.path()));
            }
        } else {
            total = total.saturating_add(meta.len());
        }
    }
    total
}

/// What a file tool that ran changed, from the arguments and the data of its result, or `None` when it changed nothing (a dry run, a patch that was already applied, a folder
/// that was there) or is not one of the six. Bytes: `fs.write` and `fs.delete` say them; an edit is the text it put in, as often as it replaced; a patch is the text of its
/// edits (or of its diff); a copy or a move is what the disk holds at the new path.
pub fn changed_by(tool: &str, args: &Value, data: &Value) -> Option<Changed> {
    let size = |key: &str| data.get(key).and_then(Value::as_u64);
    match counted_name(tool).as_str() {
        "fs.write" => {
            let path = text_of(data, "path").or_else(|| text_of(args, "path"))?.to_string();
            if data.get("created").and_then(Value::as_bool) == Some(false) && size("bytes").is_none() {
                return None;
            }
            Some(Changed { paths: vec![path], bytes: size("bytes").unwrap_or(0) })
        }
        "fs.edit" | "fs.edit_lines" => {
            let path = text_of(data, "path").or_else(|| text_of(args, "path"))?.to_string();
            let times = size("replacements").unwrap_or(1).max(1);
            Some(Changed { paths: vec![path], bytes: (text_of(args, "new").map_or(0, str::len) as u64).saturating_mul(times) })
        }
        "fs.patch" => {
            if data.get("dry_run").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let paths: Vec<String> = data
                .get("files")
                .and_then(Value::as_array)?
                .iter()
                .filter(|f| f.get("changed").and_then(Value::as_bool).unwrap_or(true))
                .filter_map(|f| text_of(f, "path").map(str::to_string))
                .collect();
            if paths.is_empty() {
                return None;
            }
            let bytes = plan_of(tool, args).map_or(0, |p| p.declared);
            Some(Changed { paths, bytes })
        }
        "fs.copy" | "fs.move" => {
            let dst = text_of(data, "dst")?.to_string();
            let bytes = size_on_disk(&dst);
            Some(Changed { paths: vec![dst], bytes })
        }
        "fs.delete" => Some(Changed { paths: vec![text_of(data, "path")?.to_string()], bytes: size("bytes").unwrap_or(0) }),
        _ => None,
    }
}

/// A path as a key: one separator, one case (Windows does not tell `A.TXT` from `a.txt`).
fn norm(path: &str) -> String {
    path.trim().replace('\\', "/").to_ascii_lowercase()
}

// ---- the counters of the runs -----------------------------------------------------------------------------------------------

type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn real_clock() -> Clock {
    Arc::new(|| {
        static START: OnceLock<std::time::Instant> = OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
    })
}

/// How many runs are counted at once. Counters are forgotten when a run ends (`agent_runs::scope::RunScope`); this is only so that runs which never ended cannot make the table grow.
const MAX_RUNS_KEPT: usize = 512;
/// How many stops are kept for the route to show.
const RECENT_STOPS: usize = 20;

struct Counters {
    first_ms: u64,
    calls: u64,
    /// The files changed, by the name the tool gave.
    files: HashSet<String>,
    /// The name the model gave a file, to the name the tool gave it: a file named the same way again is not another file.
    names: HashMap<String, String>,
    bytes: u64,
    /// The first cap the run reached.
    stopped: Option<StopReason>,
}

impl Counters {
    fn new(now: u64) -> Self {
        Self { first_ms: now, calls: 0, files: HashSet::new(), names: HashMap::new(), bytes: 0, stopped: None }
    }

    fn knows(&self, path: &str) -> bool {
        let key = norm(path);
        self.files.contains(&key) || self.names.get(&key).is_some_and(|tool_name| self.files.contains(tool_name))
    }
}

/// A stop, as the route lists the recent ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopNote {
    pub run_id: String,
    pub cap: Cap,
    pub limit: u64,
    pub at_unix_ms: u64,
}

/// What the route shows of a run that is going.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunView {
    pub run_id: String,
    pub tool_calls: u64,
    pub seconds: u64,
    pub files_changed: u64,
    pub bytes_changed: u64,
    pub stopped_by: Option<Cap>,
}

pub struct Autonomy {
    settings: RwLock<Settings>,
    runs: Mutex<HashMap<String, Counters>>,
    recent: Mutex<VecDeque<StopNote>>,
    clock: Clock,
}

impl Autonomy {
    pub fn new(settings: Settings) -> Self {
        Self::with_clock(settings, real_clock())
    }

    /// With a clock of its own, in milliseconds (a test moves it by hand).
    pub fn with_clock(settings: Settings, clock: Clock) -> Self {
        Self { settings: RwLock::new(settings), runs: Mutex::new(HashMap::new()), recent: Mutex::new(VecDeque::new()), clock }
    }

    pub fn settings(&self) -> Settings {
        *self.settings.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// The caps in force from the next call on, runs that are going included.
    pub fn set_settings(&self, settings: Settings) {
        *self.settings.write().unwrap_or_else(PoisonError::into_inner) = settings;
    }

    /// May this call be given to the run? It counts the call when it may. `Err` is the cap the run has reached; the call is then not made and is not counted, and the run reads
    /// [`StopReason::observation`] in its place. A call with no run (the app's own, `run_id` empty) is never limited. The files and bytes are checked for the six file tools that
    /// change something: a call that would change a file the run has not changed yet is stopped when the run has changed as many as it may, and a call that carries more bytes
    /// than are left is stopped (a copy or a move, whose size is known only afterwards, is stopped once the run has used them all).
    pub fn check_before_call(&self, run_id: &str, tool: &str, args: &Value) -> Result<(), StopReason> {
        if run_id.is_empty() {
            return Ok(());
        }
        let caps = self.settings().caps;
        let now = (self.clock)();
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        if !runs.contains_key(run_id) && runs.len() >= MAX_RUNS_KEPT {
            if let Some(oldest) = runs.iter().min_by_key(|(_, c)| c.first_ms).map(|(id, _)| id.clone()) {
                runs.remove(&oldest);
            }
        }
        let counters = runs.entry(run_id.to_string()).or_insert_with(|| Counters::new(now));
        match verdict(counters, &caps, now, tool, args) {
            Ok(()) => {
                counters.calls += 1;
                Ok(())
            }
            Err(stop) => {
                if counters.stopped.is_none() {
                    counters.stopped = Some(stop);
                    drop(runs);
                    self.note_stop(run_id, stop);
                }
                Err(stop)
            }
        }
    }

    fn note_stop(&self, run_id: &str, stop: StopReason) {
        let at_unix_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        info!("autonomy: run {run_id} reached its limit of {} ({})", stop.cap.what(), stop.limit);
        let mut recent = self.recent.lock().unwrap_or_else(PoisonError::into_inner);
        recent.push_back(StopNote { run_id: run_id.to_string(), cap: stop.cap, limit: stop.limit, at_unix_ms });
        while recent.len() > RECENT_STOPS {
            recent.pop_front();
        }
    }

    /// A file tool of the run has run and succeeded: what it changed counts toward the run's files and bytes.
    pub fn note_changed(&self, run_id: &str, tool: &str, args: &Value, data: &Value) {
        if run_id.is_empty() {
            return;
        }
        let Some(done) = changed_by(tool, args, data) else { return };
        let now = (self.clock)();
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let counters = runs.entry(run_id.to_string()).or_insert_with(|| Counters::new(now));
        if let (Some(named), [reported]) = (arg_path(tool, args), done.paths.as_slice()) {
            counters.names.insert(norm(named), norm(reported));
        }
        for p in &done.paths {
            counters.files.insert(norm(p));
        }
        counters.bytes = counters.bytes.saturating_add(done.bytes);
    }

    /// `n` calls were made for the run by a helper it started (card OA11-04): they count toward the run's calls, so that a run cannot get round its limit of calls by handing the work to
    /// helpers. Nothing is checked here: the next call of the run is the one that is stopped, if the limit is reached.
    pub fn add_calls(&self, run_id: &str, n: u64) {
        if run_id.is_empty() || n == 0 {
            return;
        }
        let now = (self.clock)();
        let mut runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let counters = runs.entry(run_id.to_string()).or_insert_with(|| Counters::new(now));
        counters.calls = counters.calls.saturating_add(n);
    }

    /// The first cap the run reached, or `None`.
    pub fn stopped_by(&self, run_id: &str) -> Option<Cap> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner).get(run_id).and_then(|c| c.stopped).map(|s| s.cap)
    }

    /// The run has ended: its counters are forgotten.
    pub fn forget(&self, run_id: &str) {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner).remove(run_id);
    }

    /// The runs that are going, with what they have used.
    pub fn running(&self) -> Vec<RunView> {
        let now = (self.clock)();
        let runs = self.runs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out: Vec<RunView> = runs
            .iter()
            .map(|(id, c)| RunView {
                run_id: id.clone(),
                tool_calls: c.calls,
                seconds: now.saturating_sub(c.first_ms) / 1000,
                files_changed: c.files.len() as u64,
                bytes_changed: c.bytes,
                stopped_by: c.stopped.map(|s| s.cap),
            })
            .collect();
        out.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        out
    }

    /// The last stops, oldest first.
    pub fn recent_stops(&self) -> Vec<StopNote> {
        self.recent.lock().unwrap_or_else(PoisonError::into_inner).iter().cloned().collect()
    }
}

/// The cap a call runs into, in the order the run would meet them: time, calls, then files and bytes for a call that changes files.
fn verdict(c: &Counters, caps: &Caps, now: u64, tool: &str, args: &Value) -> Result<(), StopReason> {
    let stop = |cap: Cap| Err(StopReason { cap, limit: caps.get(cap) });
    if now.saturating_sub(c.first_ms) / 1000 >= caps.max_seconds {
        return stop(Cap::Seconds);
    }
    if c.calls >= caps.max_tool_calls {
        return stop(Cap::ToolCalls);
    }
    if let Some(plan) = plan_of(tool, args) {
        let new_files = plan.paths.as_ref().map_or(1, |paths| paths.iter().filter(|p| !c.knows(p)).count() as u64);
        if new_files > 0 && (c.files.len() as u64).saturating_add(new_files) > caps.max_files_changed {
            return stop(Cap::FilesChanged);
        }
        if c.bytes.saturating_add(plan.declared) > caps.max_changed_bytes || (plan.declared == 0 && c.bytes >= caps.max_changed_bytes) {
            return stop(Cap::ChangedBytes);
        }
    }
    Ok(())
}

// ---- the process-wide state --------------------------------------------------------------------------------------------------

static GLOBAL: OnceLock<Arc<Autonomy>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// The counters a test gave this thread in place of the process's (see [`with_autonomy`]).
    static THIS_THREADS: std::cell::RefCell<Option<Arc<Autonomy>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with `autonomy` as the setting and the counters of **this thread** (card OA3-01, AQ-04). The caps are a property of the whole process, and a test that lowered them
/// would stop the runs of every test beside it; with this one, a test sets a cap of one call, makes the agent loop's dispatch (`browser_agent::call_named_tool`) or a run record
/// (`agent_runs::Recording::finish`) work against it, and sees the second call stopped, with no other test affected. Only the code that runs on this thread sees it.
#[cfg(test)]
pub(crate) fn with_autonomy<R>(autonomy: Arc<Autonomy>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<Autonomy>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            THIS_THREADS.with(|a| *a.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(THIS_THREADS.with(|a| a.borrow_mut().replace(autonomy)));
    f()
}

/// What a test gave this thread, if it did; always `None` in a build that is not a test.
fn this_threads() -> Option<Arc<Autonomy>> {
    #[cfg(test)]
    {
        THIS_THREADS.with(|a| a.borrow().clone())
    }
    #[cfg(not(test))]
    {
        None
    }
}

/// The setting and counters of this process, read from the data folder the first time they are asked for.
pub fn shared() -> Arc<Autonomy> {
    if let Some(a) = this_threads() {
        return a;
    }
    GLOBAL.get_or_init(|| Arc::new(Autonomy::new(load(&crate::runtime::data_dir())))).clone()
}

/// A run ended: its counters are forgotten. Nothing is started for a process that never counted.
pub fn forget_run(run_id: &str) {
    if let Some(a) = this_threads().or_else(|| GLOBAL.get().cloned()) {
        a.forget(run_id);
    }
}

/// The cap that stopped the run, for its record.
pub fn stopped_by_run(run_id: &str) -> Option<Cap> {
    this_threads().or_else(|| GLOBAL.get().cloned())?.stopped_by(run_id)
}

/// What the run has used so far and the caps it was held to, for the events of its record (`agent_runs`). `None` when nothing was counted for it (and nothing is started for a
/// process that never counted).
pub fn usage_of_run(run_id: &str) -> Option<(RunView, Caps)> {
    let counters = this_threads().or_else(|| GLOBAL.get().cloned())?;
    let view = counters.running().into_iter().find(|v| v.run_id == run_id)?;
    Some((view, counters.settings().caps))
}

/// A helper of the run made `n` calls (see [`Autonomy::add_calls`]), in this process's counters.
pub fn add_calls(run_id: &str, n: u64) {
    shared().add_calls(run_id, n);
}

/// A file tool of the run succeeded (see [`Autonomy::note_changed`]), in this process's counters.
pub fn note_changed(run_id: &str, tool: &str, args: &Value, data: &Value) {
    if run_id.is_empty() || !counts_changes(tool) {
        return;
    }
    shared().note_changed(run_id, tool, args, data);
}

/// At start-up: read the setting, give it to the counters and the level to the tool host's policy. A level that is missing or broken is the default, which asks as Aqua always did.
pub fn restore(host: &ToolHost, data_dir: &Path) -> Settings {
    let settings = load(data_dir);
    shared().set_settings(settings);
    host.policy().set_ask_level(settings.level);
    if settings != Settings::default() {
        info!("autonomy: level {}, caps {:?}", settings.level.id(), settings.caps);
    }
    settings
}

/// Change the setting: checked, kept on disk first (what is enforced is what is saved, and a save that fails changes nothing), then given to the counters and the policy.
/// A change of level forgets every "approve for this session" answer, so that an answer given under the old level does not cover a question the new one asks.
pub fn apply(host: Option<&ToolHost>, autonomy: &Autonomy, data_dir: &Path, new: Settings) -> Result<Settings, String> {
    for cap in Cap::ALL {
        let value = new.caps.get(cap);
        if !(SMALLEST_CAP..=BIGGEST_CAPS.get(cap)).contains(&value) {
            return Err(format!("{} must be a whole number from {SMALLEST_CAP} to {}", cap.id(), BIGGEST_CAPS.get(cap)));
        }
    }
    save(data_dir, &new).map_err(|e| format!("could not save the setting, so nothing was changed: {e}"))?;
    let before = autonomy.settings();
    autonomy.set_settings(new);
    if let Some(host) = host {
        host.policy().set_ask_level(new.level);
        if before.level != new.level {
            host.clear_grants();
        }
    }
    info!("autonomy: level {}, caps {:?}", new.level.id(), new.caps);
    Ok(new)
}

/// What `GET /api/v1/autonomy` says: the level and what each does, the caps and their defaults and bounds, the runs that are going with what they have used, and the last stops.
pub fn describe(autonomy: &Autonomy, host: Option<&ToolHost>, data_dir: &Path) -> Value {
    let settings = autonomy.settings();
    let caps = |c: &Caps| json!({ "max_seconds": c.max_seconds, "max_tool_calls": c.max_tool_calls, "max_files_changed": c.max_files_changed, "max_changed_bytes": c.max_changed_bytes });
    let levels: Vec<Value> = AskLevel::ALL
        .iter()
        .map(|l| {
            let (label, what) = level_words(*l);
            json!({ "id": l.id(), "label": label, "what": what, "current": *l == settings.level, "stricter_than_default": *l > AskLevel::default() })
        })
        .collect();
    let running: Vec<Value> = autonomy
        .running()
        .iter()
        .map(|r| json!({ "run_id": r.run_id, "tool_calls": r.tool_calls, "seconds": r.seconds, "files_changed": r.files_changed, "bytes_changed": r.bytes_changed, "stopped_by": r.stopped_by.map(Cap::id) }))
        .collect();
    let stops: Vec<Value> = autonomy.recent_stops().iter().map(|s| json!({ "run_id": s.run_id, "cap": s.cap.id(), "limit": s.limit, "at_unix_ms": s.at_unix_ms })).collect();
    json!({
        "level": settings.level.id(),
        "levels": levels,
        "caps": caps(&settings.caps),
        "defaults": caps(&DEFAULT_CAPS),
        "bounds": { "least": SMALLEST_CAP, "most": caps(&BIGGEST_CAPS) },
        "saved": path(data_dir).exists(),
        // The level the tool host's policy is applying now; it differs from `level` only if the host could not be reached.
        "applied_level": host.map(|h| h.policy().ask_level().id()),
        "running": running,
        "recent_stops": stops,
        "not_built": ["a cap on money: no tool spends any"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn clocked(settings: Settings) -> (Autonomy, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(0));
        let seen = now.clone();
        (Autonomy::with_clock(settings, Arc::new(move || seen.load(Ordering::SeqCst))), now)
    }

    fn with_caps(change: impl FnOnce(&mut Caps)) -> Settings {
        let mut s = Settings::default();
        change(&mut s.caps);
        s
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_autonomy_unit_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_defaults_are_above_what_a_run_does_today_and_a_plain_run_is_never_stopped() {
        // The longest the loop's own clock lets a run go (`agent_run_limit` clamps `AQUA_AGENT_MAX_SECS` to 7,200, a run in the background has 30 minutes).
        assert!(DEFAULT_CAPS.max_seconds >= 7_200 && DEFAULT_CAPS.max_seconds >= crate::queue::BACKGROUND_RUN_LIMIT.as_secs());
        // The most calls the loop makes: its rounds, and at most three calls chained in a round (`AQUA_PLANNER_BATCH` is clamped to 3).
        assert!(DEFAULT_CAPS.max_tool_calls >= u64::from(crate::modules::browser_agent::AGENT_MAX_ROUNDS) * 3);
        assert!(DEFAULT_CAPS.max_files_changed >= 36 && DEFAULT_CAPS.max_changed_bytes >= 1 << 30);
        for cap in Cap::ALL {
            assert!((SMALLEST_CAP..=BIGGEST_CAPS.get(cap)).contains(&DEFAULT_CAPS.get(cap)), "{cap:?}");
        }
        assert_eq!(Settings::default().level, AskLevel::AskRisky);
        let (a, now) = clocked(Settings::default());
        for i in 0..30u64 {
            now.store(i * 10_000, Ordering::SeqCst);
            let tool = if i % 3 == 0 { "fs.read" } else if i % 3 == 1 { "browser_click" } else { "web.read" };
            assert_eq!(a.check_before_call("run_plain", tool, &json!({"path": "a.txt"})), Ok(()), "call {i}");
        }
        assert_eq!(a.stopped_by("run_plain"), None);
    }

    #[test]
    fn the_call_after_the_last_one_allowed_is_stopped_with_the_words_the_run_reads_and_not_before() {
        let (a, _) = clocked(with_caps(|c| c.max_tool_calls = 3));
        let args = json!({"path": "a.txt"});
        for i in 1..=3 {
            assert_eq!(a.check_before_call("run_x", "fs.read", &args), Ok(()), "call {i} is within the limit");
        }
        let stop = StopReason { cap: Cap::ToolCalls, limit: 3 };
        assert_eq!(a.check_before_call("run_x", "fs.read", &args), Err(stop), "the fourth is the one over");
        assert_eq!(a.check_before_call("run_x", "browser_click", &args), Err(stop), "and so is any other tool after it");
        assert_eq!(
            stop.observation("fs.read"),
            "ERROR [STOPPED_BY_LIMIT] fs.read: this run reached its limit of tool calls (3). Tell the person what was done and what is left."
        );
        // The agent layer gives the run the same words, for the tool as the model wrote it.
        assert_eq!(crate::agent_runs::scope::limit_stop_on(&a, "run_x", "fs_read", &args).as_deref(), Some("ERROR [STOPPED_BY_LIMIT] fs_read: this run reached its limit of tool calls (3). Tell the person what was done and what is left."));
        assert_eq!(a.stopped_by("run_x"), Some(Cap::ToolCalls));
        assert_eq!(a.recent_stops().len(), 1, "a run that is stopped is noted once, not once for every call it makes after");
        assert_eq!((a.recent_stops()[0].cap, a.recent_stops()[0].limit, a.recent_stops()[0].run_id.as_str()), (Cap::ToolCalls, 3, "run_x"));
        // A call the cap refused was not counted: raising the cap lets the run go on from where it was.
        a.set_settings(with_caps(|c| c.max_tool_calls = 5));
        assert_eq!(a.check_before_call("run_x", "fs.read", &args), Ok(()));
        assert_eq!(a.running()[0].tool_calls, 4);
    }

    /// Card OA11-04: the calls a helper made count toward the run that started it, so that a run cannot get round its limit by handing the work to helpers. Nothing is checked when they are
    /// added; the next call of the run is the one that is stopped.
    #[test]
    fn the_calls_of_a_helper_count_toward_the_run_that_asked_and_the_next_call_is_the_one_stopped() {
        let (a, _) = clocked(with_caps(|c| c.max_tool_calls = 5));
        let args = json!({"path": "a.txt"});
        assert_eq!(a.check_before_call("run_h", "agent.task", &args), Ok(()), "the call that starts the helper is one");
        a.add_calls("run_h", 3);
        assert_eq!(a.running()[0].tool_calls, 4);
        assert_eq!(a.check_before_call("run_h", "fs.read", &args), Ok(()), "the fifth is within the limit");
        assert_eq!(a.check_before_call("run_h", "fs.read", &args), Err(StopReason { cap: Cap::ToolCalls, limit: 5 }), "the sixth is over it");
        // Nothing is added for no run, for none, or to a run that was not counted before (it is counted from the helper's calls).
        a.add_calls("", 9);
        a.add_calls("run_other", 0);
        assert_eq!(a.running().len(), 1);
        a.add_calls("run_new", 2);
        assert_eq!(a.running().iter().find(|v| v.run_id == "run_new").map(|v| v.tool_calls), Some(2));
    }

    #[test]
    fn the_seconds_run_from_the_first_call_of_the_run_by_the_clock_given() {
        let (a, now) = clocked(with_caps(|c| c.max_seconds = 10));
        let args = json!({});
        assert_eq!(a.check_before_call("run_t", "fs.read", &args), Ok(()), "the first call starts the clock");
        now.store(9_999, Ordering::SeqCst);
        assert_eq!(a.check_before_call("run_t", "fs.read", &args), Ok(()), "9.999 seconds in");
        now.store(10_000, Ordering::SeqCst);
        assert_eq!(a.check_before_call("run_t", "fs.read", &args), Err(StopReason { cap: Cap::Seconds, limit: 10 }));
        assert_eq!(a.stopped_by("run_t"), Some(Cap::Seconds));
        // A run that begins later has its own clock: its first call is at 5 s, so its limit is at 15 s.
        now.store(5_000, Ordering::SeqCst);
        assert_eq!(a.check_before_call("run_u", "fs.read", &args), Ok(()));
        now.store(14_999, Ordering::SeqCst);
        assert_eq!(a.check_before_call("run_u", "fs.read", &args), Ok(()));
        now.store(15_000, Ordering::SeqCst);
        assert_eq!(a.check_before_call("run_u", "fs.read", &args), Err(StopReason { cap: Cap::Seconds, limit: 10 }));
        assert!(a.check_before_call("run_t", "fs.read", &args).is_err(), "time does not go back");
        assert_eq!(StopReason { cap: Cap::Seconds, limit: 10 }.observation("web.read"), "ERROR [STOPPED_BY_LIMIT] web.read: this run reached its limit of seconds (10). Tell the person what was done and what is left.");
    }

    #[test]
    fn a_run_may_change_as_many_files_as_the_cap_says_a_file_counts_once_and_reads_are_never_stopped_by_it() {
        let (a, _) = clocked(with_caps(|c| c.max_files_changed = 2));
        // The model names a file one way and the tool reports it another; the same name again is the same file.
        let write = |name: &str| json!({"path": name, "content": "hello"});
        assert_eq!(a.check_before_call("run_f", "fs.write", &write("@ws/a.txt")), Ok(()));
        a.note_changed("run_f", "fs.write", &write("@ws/a.txt"), &json!({"path": "E:/ws/a.txt", "bytes": 5, "created": true}));
        assert_eq!(a.check_before_call("run_f", "fs.edit", &json!({"path": "@ws/a.txt", "old": "h", "new": "j"})), Ok(()), "the same file again is not another file");
        assert_eq!(a.check_before_call("run_f", "fs.write", &write("E:\\WS\\A.txt")), Ok(()), "named by the path the tool reported, in any case");
        assert_eq!(a.check_before_call("run_f", "fs_write", &write("@ws/b.txt")), Ok(()), "the second file is within the limit");
        a.note_changed("run_f", "fs.write", &write("@ws/b.txt"), &json!({"path": "E:/ws/b.txt", "bytes": 5, "created": true}));
        let stop = Err(StopReason { cap: Cap::FilesChanged, limit: 2 });
        assert_eq!(a.check_before_call("run_f", "fs.write", &write("@ws/c.txt")), stop, "a third file");
        assert_eq!(a.check_before_call("run_f", "fs.patch", &json!({"edits": [{"path": "@ws/a.txt", "old": "x", "new": "y"}, {"path": "@ws/c.txt", "old": "x", "new": "y"}]})), stop, "a patch that reaches a third file is stopped whole");
        assert_eq!(a.check_before_call("run_f", "fs.patch", &json!({"edits": [{"path": "@ws/a.txt", "old": "x", "new": "y"}, {"path": "@ws/b.txt", "old": "x", "new": "y"}]})), Ok(()), "and one that stays in the two is not");
        assert_eq!(a.check_before_call("run_f", "fs.patch", &json!({"diff": "--- a\n+++ b\n"})), stop, "a diff does not say which files, so it counts as one more");
        assert_eq!(a.check_before_call("run_f", "fs.patch", &json!({"edits": [{"path": "@ws/c.txt", "old": "x", "new": "y"}], "dry_run": true})), Ok(()), "a dry run changes nothing");
        assert_eq!(a.check_before_call("run_f", "fs.read", &json!({"path": "@ws/c.txt"})), Ok(()), "a read is not a change");
        assert_eq!(a.check_before_call("run_f", "web.read", &json!({"url": "https://example.org"})), Ok(()));
        assert_eq!(a.stopped_by("run_f"), Some(Cap::FilesChanged));
        assert_eq!(a.running()[0].files_changed, 2);
    }

    #[test]
    fn a_run_may_change_as_many_bytes_as_the_cap_says_a_call_that_carries_more_than_is_left_is_stopped() {
        let (a, _) = clocked(with_caps(|c| c.max_changed_bytes = 100));
        let write = |n: usize| json!({"path": "@ws/a.txt", "content": "x".repeat(n), "mode": "overwrite"});
        assert_eq!(a.check_before_call("run_b", "fs.write", &write(60)), Ok(()));
        a.note_changed("run_b", "fs.write", &write(60), &json!({"path": "E:/ws/a.txt", "bytes": 60, "created": false}));
        let stop = Err(StopReason { cap: Cap::ChangedBytes, limit: 100 });
        assert_eq!(a.check_before_call("run_b", "fs.write", &write(41)), stop, "60 and 41 are 101");
        assert_eq!(a.check_before_call("run_b", "fs.write", &write(40)), Ok(()), "60 and 40 are exactly the cap");
        a.note_changed("run_b", "fs.write", &write(40), &json!({"path": "E:/ws/a.txt", "bytes": 40, "created": false}));
        assert_eq!(a.check_before_call("run_b", "fs.copy", &json!({"src": "@ws/a.txt", "dst": "@ws/a2.txt"})), stop, "a copy's size is known afterwards, and the bytes are all used");
        assert_eq!(a.check_before_call("run_b", "fs.read", &json!({"path": "@ws/a.txt"})), Ok(()));
        assert_eq!(a.stopped_by("run_b"), Some(Cap::ChangedBytes));
        assert_eq!(a.running()[0].bytes_changed, 100);
    }

    #[test]
    fn the_counters_belong_to_the_run_two_runs_do_not_share_them_and_an_ended_run_is_forgotten() {
        let (a, _) = clocked(with_caps(|c| c.max_tool_calls = 2));
        let args = json!({});
        for run in ["run_one", "run_two"] {
            assert_eq!(a.check_before_call(run, "fs.read", &args), Ok(()), "{run}");
            assert_eq!(a.check_before_call(run, "fs.read", &args), Ok(()), "{run}");
        }
        assert!(a.check_before_call("run_one", "fs.read", &args).is_err() && a.check_before_call("run_two", "fs.read", &args).is_err());
        let view = a.running();
        assert_eq!(view.iter().map(|r| (r.run_id.as_str(), r.tool_calls, r.stopped_by)).collect::<Vec<_>>(), vec![("run_one", 2, Some(Cap::ToolCalls)), ("run_two", 2, Some(Cap::ToolCalls))]);
        a.forget("run_one");
        assert_eq!(a.running().len(), 1);
        assert_eq!(a.stopped_by("run_one"), None, "nothing is left of it");
        assert_eq!(a.check_before_call("run_one", "fs.read", &args), Ok(()), "a run with that id again starts from nothing");
        assert!(a.check_before_call("run_two", "fs.read", &args).is_err(), "the other one is where it was");
        // No run, no limit: the app's own calls and the calls of a test are not a run's.
        for _ in 0..1_000 {
            assert_eq!(a.check_before_call("", "fs.read", &args), Ok(()));
        }
        assert!(a.running().iter().all(|r| !r.run_id.is_empty()));
    }

    #[test]
    fn the_table_of_runs_cannot_grow_without_end() {
        let (a, now) = clocked(Settings::default());
        for i in 0..(MAX_RUNS_KEPT + 20) {
            now.store(i as u64, Ordering::SeqCst);
            assert_eq!(a.check_before_call(&format!("run_{i}"), "fs.read", &json!({})), Ok(()));
        }
        assert_eq!(a.running().len(), MAX_RUNS_KEPT, "the runs that began longest ago are dropped");
        assert!(a.running().iter().all(|r| r.run_id != "run_0") && a.running().iter().any(|r| r.run_id == format!("run_{}", MAX_RUNS_KEPT + 19)));
    }

    #[test]
    fn the_file_is_read_leniently_a_broken_part_gives_its_default_and_nothing_gives_a_looser_level_than_the_default() {
        let (s, problems) = parse(r#"{"level": "ask_all", "max_tool_calls": 50}"#);
        assert_eq!((s.level, s.caps.max_tool_calls, s.caps.max_seconds, problems.len()), (AskLevel::AskAll, 50, DEFAULT_CAPS.max_seconds, 0));
        let (s, problems) = parse("\u{feff}{\"level\": \"ask_outside\"}");
        assert_eq!((s.level, problems.len()), (AskLevel::AskOutside, 0), "a byte-order mark from Notepad is accepted");
        // Text that is not an object is the defaults.
        for text in ["{ not json", "[]", "\"ask_all\"", "", "null"] {
            let (s, problems) = parse(text);
            assert_eq!((s, problems.len()), (Settings::default(), 1), "{text:?}");
        }
        // Each wrong part is the default for that part and does not spoil the others.
        let (s, problems) = parse(r#"{"level": "ask_all", "max_tool_calls": 0, "max_seconds": 99999999, "max_files_changed": "ten", "max_changed_bytes": 1.5, "typo": 1}"#);
        assert_eq!(s.level, AskLevel::AskAll, "a good level survives bad caps");
        assert_eq!(s.caps, DEFAULT_CAPS);
        assert_eq!(problems.len(), 5, "{problems:?}");
        let (s, problems) = parse(r#"{"level": "ask_everything", "max_tool_calls": 7}"#);
        assert_eq!((s.level, s.caps.max_tool_calls, problems.len()), (AskLevel::AskRisky, 7, 1), "an unknown level is the default level");
        let (s, problems) = parse(r#"{"level": null, "max_seconds": null}"#);
        assert_eq!((s, problems.len()), (Settings::default(), 0), "null is the default and is not a mistake");
        // Whatever a file says, it never gives a level looser than the default: there is none.
        for level in AskLevel::ALL {
            assert!(level >= AskLevel::default());
        }
    }

    #[test]
    fn the_setting_is_written_beside_the_file_read_back_and_a_broken_file_is_the_default() {
        let dir = temp("file");
        assert_eq!(load(&dir), Settings::default(), "no file");
        let chosen = Settings { level: AskLevel::AskAll, caps: Caps { max_tool_calls: 40, ..DEFAULT_CAPS } };
        save(&dir, &chosen).unwrap();
        assert_eq!(load(&dir), chosen);
        let text = std::fs::read_to_string(path(&dir)).unwrap();
        assert!(text.contains("\"level\": \"ask_all\"") && text.contains("\"max_tool_calls\": 40"), "{text}");
        assert!(!text.contains("max_seconds") && !text.contains("max_files_changed"), "a cap that is the default is not written: {text}");
        assert!(!path(&dir).with_extension("json.tmp").exists(), "the temporary file was renamed");
        save(&dir, &Settings::default()).unwrap();
        assert_eq!(load(&dir), Settings::default());
        std::fs::write(path(&dir), "{ broken").unwrap();
        assert_eq!(load(&dir), Settings::default(), "a broken file is the default");
        // A save that cannot be made says so: the tools folder is a file.
        let blocked = temp("blocked");
        std::fs::write(blocked.join("tools"), "in the way").unwrap();
        assert!(save(&blocked, &chosen).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&blocked);
    }

    #[test]
    fn the_names_models_write_are_counted_as_the_host_names_them() {
        for (written, host) in [("fs.write", "fs.write"), ("fs_write", "fs.write"), ("FS.WRITE", "fs.write"), ("host.fs.edit", "fs.edit"), ("host_fs_edit", "fs.edit"), ("fs-delete", "fs.delete"), ("browser_click", "browser.click"), ("Click", "click")] {
            assert_eq!(counted_name(written), host, "{written}");
        }
        for tool in ["fs.write", "fs_edit", "FS.PATCH", "fs.copy", "fs.move", "host.fs.delete"] {
            assert!(counts_changes(tool), "{tool}");
        }
        for tool in ["fs.read", "fs.find", "fs.restore", "fs.undo_run", "fs.set_attrs", "term.run", "browser_click", "web.read"] {
            assert!(!counts_changes(tool), "{tool}");
        }
    }

    /// Card OA6-02: an edit by lines counts as an edit in the run's accounting, like `fs.edit`: it is a change, its path and the text it puts in are what the caps are held to.
    #[test]
    fn a_line_edit_is_counted_as_an_edit() {
        for tool in ["fs.edit_lines", "fs_edit_lines", "host.fs.edit_lines", "FS.EDIT_LINES"] {
            assert!(counts_changes(tool), "{tool}");
        }
        assert_eq!(counted_name("host_fs_edit_lines"), "fs.edit_lines");
        let args = json!({"path": "@ws/a.rs", "start": 2, "end": 3, "new": "one\ntwo\n", "expect_first_line": "x"});
        let plan = plan_of("fs.edit_lines", &args).expect("a plan");
        assert_eq!((plan.paths, plan.declared), (Some(vec!["@ws/a.rs".to_string()]), 8));
        assert_eq!(arg_path("fs.edit_lines", &args), Some("@ws/a.rs"));
        let done = changed_by("fs.edit_lines", &args, &json!({"path": "E:/ws/a.rs", "replacements": 1, "replaced": [2, 3], "undo_id": "u", "version": "v"}));
        assert_eq!(done, Some(Changed { paths: vec!["E:/ws/a.rs".to_string()], bytes: 8 }));
        // A refusal changed nothing, and its answer has no path: nothing is counted for it.
        assert_eq!(changed_by("fs.edit_lines", &json!({}), &json!({})), None);
    }

    /// The results are the shapes `adapters/native_tools/src/fs/provider.rs` gives (the integration test `core/tests/autonomy_test.rs` feeds it real ones).
    #[test]
    fn what_a_file_tool_changed_is_read_from_the_result_it_gave() {
        let changed = |tool: &str, args: Value, data: Value| changed_by(tool, &args, &data);
        let one = |path: &str, bytes: u64| Some(Changed { paths: vec![path.to_string()], bytes });
        assert_eq!(changed("fs.write", json!({"path": "@ws/a.txt", "content": "hello"}), json!({"path": "E:/ws/a.txt", "bytes": 5, "created": true, "undo_id": null, "version": "v"})), one("E:/ws/a.txt", 5));
        assert_eq!(changed("fs.write", json!({"path": "@ws/d", "kind": "dir"}), json!({"path": "E:/ws/d", "created": true})), one("E:/ws/d", 0), "a new folder is one change and no bytes");
        assert_eq!(changed("fs.write", json!({"path": "@ws/d", "kind": "dir"}), json!({"path": "E:/ws/d", "created": false})), None, "a folder that was there is no change");
        assert_eq!(changed("fs.edit", json!({"path": "@ws/a.txt", "old": "x", "new": "BETA"}), json!({"path": "E:/ws/a.txt", "replacements": 3, "undo_id": "u", "version": "v"})), one("E:/ws/a.txt", 12));
        let edits = json!([{"path": "@ws/a", "old": "x", "new": "yy"}, {"path": "@ws/b", "old": "x", "new": "z"}, {"path": "@ws/c", "old": "x", "new": "z"}]);
        let files = json!([{"path": "E:/ws/a", "lines_added": 1, "lines_removed": 1, "changed": true}, {"path": "E:/ws/b", "lines_added": 1, "lines_removed": 1, "changed": true}, {"path": "E:/ws/c", "lines_added": 0, "lines_removed": 0, "changed": false}]);
        assert_eq!(changed("fs.patch", json!({"edits": edits}), json!({"files": files, "undo_id": "p_1"})), Some(Changed { paths: vec!["E:/ws/a".into(), "E:/ws/b".into()], bytes: 4 }), "only the files that changed, and the text of the edits");
        assert_eq!(changed("fs.patch", json!({"edits": edits, "dry_run": true}), json!({"dry_run": true, "files": files})), None);
        assert_eq!(changed("fs.patch", json!({"edits": edits}), json!({"files": [{"path": "E:/ws/a", "changed": false}], "undo_id": null})), None, "a patch that was already applied");
        assert_eq!(changed("fs.delete", json!({"path": "@ws/a.txt"}), json!({"path": "E:/ws/a.txt", "undo_id": "u", "kind": "file", "bytes": 12})), one("E:/ws/a.txt", 12));
        assert_eq!(changed("fs.read", json!({"path": "@ws/a.txt"}), json!({"path": "E:/ws/a.txt"})), None);
        assert_eq!(changed("term.run", json!({}), json!({})), None);
        // A copy and a move are what the disk holds at the new path: a file, and a folder with what is in it.
        let dir = temp("sizes");
        std::fs::write(dir.join("one.txt"), "1234567").unwrap();
        std::fs::create_dir_all(dir.join("folder").join("inner")).unwrap();
        std::fs::write(dir.join("folder").join("a.txt"), "123").unwrap();
        std::fs::write(dir.join("folder").join("inner").join("b.txt"), "4567").unwrap();
        let shown = |p: PathBuf| p.to_string_lossy().to_string();
        for tool in ["fs.copy", "fs.move"] {
            assert_eq!(changed(tool, json!({"src": "x", "dst": "y"}), json!({"src": "x", "dst": shown(dir.join("one.txt")), "undo_id": null})), one(&shown(dir.join("one.txt")), 7), "{tool}");
            assert_eq!(changed(tool, json!({"src": "x", "dst": "y", "recursive": true}), json!({"src": "x", "dst": shown(dir.join("folder")), "undo_id": null})), one(&shown(dir.join("folder")), 7), "{tool}: a folder");
        }
        assert_eq!(changed("fs.copy", json!({}), json!({})), None, "a result with no destination says nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_levels_have_words_and_the_default_is_the_one_that_says_so() {
        for level in AskLevel::ALL {
            let (label, what) = level_words(level);
            assert!(!label.is_empty() && what.len() > 60, "{level:?}");
        }
        assert!(level_words(AskLevel::AskRisky).0.contains("default"));
        assert!(level_words(AskLevel::AskAll).1.contains("even a read"));
        assert!(level_words(AskLevel::AskOutside).1.contains("also under full access"));
    }

    #[test]
    fn a_cap_is_read_from_the_settings_at_each_call_so_a_change_reaches_a_run_that_is_going() {
        let (a, _) = clocked(Settings::default());
        for _ in 0..3 {
            assert_eq!(a.check_before_call("run_live", "fs.read", &json!({})), Ok(()));
        }
        a.set_settings(with_caps(|c| c.max_tool_calls = 3));
        assert_eq!(a.check_before_call("run_live", "fs.read", &json!({})), Err(StopReason { cap: Cap::ToolCalls, limit: 3 }), "three calls were made, and the limit is now three");
    }

    /// Card OA3-01 (AQ-04): this was a search of the loop's source for the order in which `call_named_tool` asks the skill's list and the limits. It is run now, with a cap of one call
    /// and a skill's list of its own: what the loop dispatches is what shows the order (a call the list refuses is not counted, a call the cap refuses is not given to the route that
    /// would have taken it, and the list is asked before the cap even when the cap is reached).
    #[test]
    fn the_loop_asks_the_skills_list_and_then_the_limits_before_any_route_takes_a_call_and_counts_only_the_calls_it_gives() {
        use aqua_tool_host::{AuditLedger, Mode, PolicyEngine};
        let dir = temp("dispatch_order");
        let host = Arc::new(ToolHost::new(Arc::new(PolicyEngine::new(Mode::Standard)), Arc::new(AuditLedger::open(dir.join("audit.jsonl")))));
        let counters = Arc::new(Autonomy::new(with_caps(|c| c.max_tool_calls = 1)));
        with_autonomy(counters.clone(), || {
            crate::agent_tools::with_host(host.clone(), || {
                let scope = crate::agent_runs::scope::RunScope::begin(Some(host.clone()));
                let run = scope.id().to_string();
                host.restrict_run(&run, vec!["todo.write".into(), "local_find".into()]);
                let dispatch = |tool: &str| crate::modules::browser_agent::call_named_tool(tool, json!({"items": []}), &[], &None, &None).unwrap_or_else(|e| panic!("{tool}: {e}"));
                let counted = |counters: &Autonomy| counters.running().into_iter().find(|r| r.run_id == run).map_or(0, |r| r.tool_calls);
                // The list is asked first, and a call it refuses is not counted: the cap of one call is still whole.
                let (label, text) = dispatch("Click");
                assert_eq!(label, "guard");
                assert!(text.starts_with("ERROR [PERMISSION_DENIED] Click:"), "{text}");
                assert_eq!(counted(&counters), 0, "a call the skill's list refused is not a call of the run");
                // A call the list names is given to its route (the loop's own checklist) and counted: that is the one call of the cap.
                let (label, text) = dispatch("todo.write");
                assert_eq!((label.as_str(), text.as_str()), ("agent", "Checklist cleared."));
                assert_eq!(counted(&counters), 1);
                // The cap is reached: the same call is not given to its route, and neither is the first route of the function (`local_find`, which would have said what it lacks).
                for tool in ["todo.write", "local_find"] {
                    let (label, text) = dispatch(tool);
                    assert_eq!(label, "guard", "{tool}");
                    assert_eq!(text, StopReason { cap: Cap::ToolCalls, limit: 1 }.observation(tool));
                }
                // The list is still asked before the cap: a call it refuses reads the list's words, not the cap's.
                let (_, text) = dispatch("Click");
                assert!(text.starts_with("ERROR [PERMISSION_DENIED] Click:"), "{text}");
                assert_eq!(counters.stopped_by(&run), Some(Cap::ToolCalls), "and the run record can name the cap");
                assert_eq!(counters.recent_stops().len(), 1);
            });
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Card OA3-01 (AQ-04): a run's counters are forgotten when its scope ends, with the counters given to the thread; the thread's own counters are the ones asked, and the process's
    /// are not touched.
    #[test]
    fn the_counters_a_test_gives_its_thread_are_the_ones_the_scope_forgets_and_the_process_ones_are_left_alone() {
        let (mine, _) = clocked(with_caps(|c| c.max_tool_calls = 1));
        let mine = Arc::new(mine);
        let scope = crate::agent_runs::scope::RunScope::begin(None);
        let id = scope.id().to_string();
        with_autonomy(mine.clone(), || {
            assert_eq!(crate::agent_runs::scope::limit_stop("fs.read", &json!({})), None);
            assert!(crate::agent_runs::scope::limit_stop("fs.read", &json!({})).is_some(), "the thread's cap of one call");
            assert_eq!(stopped_by_run(&id), Some(Cap::ToolCalls));
            drop(scope);
            assert!(mine.running().iter().all(|r| r.run_id != id), "forgotten with the run");
            assert_eq!(stopped_by_run(&id), None);
        });
    }
}

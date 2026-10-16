//! The command tools of the person and of a trusted repository, in the core (card OA11-02; tasks EX-09 and TL-11 of the OpenCode audit).
//!
//! What a declaration is, how a call is checked and why it can never be looser than the `term.run` it stands for is in `aqua_native_tools::term::custom`. This module is where the core
//! finds the declarations and offers the tools (all of it off unless `AQUA_COMMAND_TOOLS=on`):
//!
//! * **`cmd.*`** are declared in `data/tools/custom.json`, the person's own file (the folder no tool can write). They are registered with the tool host when Aqua starts, like any tool.
//! * **`proj.*`** are declared in the `tools.json` of a repository's `.aqua` pack, and exist **only for a run in that project and only when the person has trusted the code kind of the
//!   pack by its hash** (`project_context`): a project that is not trusted has none, a file edited after the trust has none at the next run, and a run in another project has none. The host
//!   learns a name (`proj.test`) when a run first meets it; what the name means is looked up in the pack of the run that makes the call.
//! * The model is shown at most [`MAX_SHOWN`] of them for a run: those the goal names, then, for a coding goal, the rest.
//!
//! Every call is a `ToolHost::call`: validated, classified (as the `term.run` it stands for), decided, asked, masked and written in the audit ledger under its own name.

use aqua_native_tools::term::custom::{self, CommandSource, CommandToolProvider, CustomTool, Domain};
use aqua_native_tools::term::TermProvider;
use aqua_tool_host::ToolHost;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// The most command tools a run is shown.
pub const MAX_SHOWN: usize = 4;
/// The most `proj.*` names the host learns in one process (a name is kept until Aqua stops).
const MAX_PROJECT_NAMES: usize = 64;
/// The biggest `custom.json`, and the biggest `tools.json` that a pack may carry (the pack's own limit).
const MAX_FILE_BYTES: u64 = 64 * 1024;

pub fn enabled() -> bool {
    crate::flags::is_on(&crate::flags::COMMAND_TOOLS)
}

/// `data/tools/custom.json`.
pub fn custom_path(data_dir: &Path) -> PathBuf {
    data_dir.join("tools").join("custom.json")
}

/// What was read of `custom.json`.
#[derive(Debug, Default)]
pub struct Custom {
    pub present: bool,
    pub tools: Vec<CustomTool>,
    pub warnings: Vec<String>,
    /// The file could not be used at all (it is not JSON, or too big).
    pub error: Option<String>,
}

/// Reads the person's file of command tools. A file that is not there is not an error.
pub fn read_custom(data_dir: &Path) -> Custom {
    let path = custom_path(data_dir);
    let Ok(meta) = std::fs::metadata(&path) else { return Custom::default() };
    let mut out = Custom { present: true, ..Default::default() };
    if meta.len() > MAX_FILE_BYTES {
        out.error = Some(format!("custom.json is larger than {} KiB", MAX_FILE_BYTES / 1024));
        return out;
    }
    match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| custom::parse_file(&t, Domain::Cmd)) {
        Ok(loaded) => {
            out.tools = loaded.tools;
            out.warnings = loaded.warnings;
        }
        Err(e) => out.error = Some(format!("custom.json cannot be used: {e}")),
    }
    out
}

/// The declarations of the tools of the pack of one run, read once for the run.
struct Defs {
    tools: Vec<Arc<CustomTool>>,
}

/// Where the declarations of `proj.*` are looked up: the pack of the run that asks (its `tools.json`, when the code kind is trusted).
struct PackSource {
    /// The id of the run on the thread that asks (for the question "is this tool there now", which has no run in it).
    current_run: fn() -> String,
    cache: Mutex<HashMap<String, Arc<Defs>>>,
}

impl PackSource {
    fn defs(&self, run: &str) -> Arc<Defs> {
        if run.is_empty() {
            return Arc::new(Defs { tools: Vec::new() });
        }
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(d) = cache.get(run) {
            return d.clone();
        }
        // What a run's context holds does not change while it runs, so what is read of it is kept; the table is cleared when it grows (it is as long as the runs that are going on).
        if cache.len() >= 64 {
            cache.clear();
        }
        let defs = match crate::project_context::pack_code_file(run, "tools.json") {
            None => Defs { tools: Vec::new() },
            Some(bytes) => match custom::parse_file(&String::from_utf8_lossy(&bytes), Domain::Proj) {
                Ok(loaded) => {
                    for w in &loaded.warnings {
                        tracing::warn!("the tools.json of a project: {w}");
                    }
                    Defs { tools: loaded.tools.into_iter().map(Arc::new).collect() }
                }
                Err(e) => {
                    tracing::warn!("the tools.json of a project cannot be used: {e}");
                    Defs { tools: Vec::new() }
                }
            },
        };
        let defs = Arc::new(defs);
        cache.insert(run.to_string(), defs.clone());
        defs
    }
}

impl CommandSource for PackSource {
    fn tool(&self, run: &str, name: &str) -> Option<Arc<CustomTool>> {
        self.defs(run).tools.iter().find(|t| t.name == name).cloned()
    }

    fn project_dir(&self, run: &str) -> Option<PathBuf> {
        crate::project_context::project_dir_of_run(run)
    }

    fn available(&self, name: &str) -> bool {
        self.tool(&(self.current_run)(), name).is_some()
    }
}

/// What the core keeps for a host: the terminal the command tools run through, the lookup of a pack's tools, the names the host has learned and the person's own tools.
struct Hub {
    term: TermProvider,
    source: Arc<PackSource>,
    learned: Mutex<HashSet<String>>,
    own: Vec<Arc<CustomTool>>,
}

/// A table of hubs by host (the address of the host): a process has one host, and a test makes several.
static HUBS: Mutex<Vec<(usize, Arc<Hub>)>> = Mutex::new(Vec::new());

fn host_key(host: &ToolHost) -> usize {
    host as *const ToolHost as usize
}

fn hub_of(host: &ToolHost) -> Option<Arc<Hub>> {
    HUBS.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(k, _)| *k == host_key(host)).map(|(_, h)| h.clone())
}

/// Offers the command tools to `host`: the person's own (`cmd.*`) are registered now; the `proj.*` of a pack are learned when a run meets them. `term` is the terminal the tools run through (a
/// second handle to the host's own, so that its rules decide). Returns how many tools were registered. Called when the host is built, with the switch on.
pub fn attach(host: &Arc<ToolHost>, data_dir: &Path, term: TermProvider, current_run: fn() -> String) -> usize {
    let custom = read_custom(data_dir);
    if let Some(e) = &custom.error {
        tracing::warn!("command tools: {e}");
    }
    for w in &custom.warnings {
        tracing::warn!("command tools: custom.json: {w}");
    }
    let own: Vec<Arc<CustomTool>> = custom.tools.iter().cloned().map(Arc::new).collect();
    let registered = if custom.tools.is_empty() {
        0
    } else {
        match host.register(Arc::new(CommandToolProvider::fixed(term.clone(), custom.tools))) {
            Ok(n) => {
                tracing::info!("tool host: registered {n} command tool(s) from custom.json");
                n
            }
            Err(e) => {
                tracing::warn!("tool host: could not register the command tools of custom.json: {e}");
                0
            }
        }
    };
    let own = if registered == 0 { Vec::new() } else { own };
    let hub = Arc::new(Hub { term, source: Arc::new(PackSource { current_run, cache: Mutex::new(HashMap::new()) }), learned: Mutex::new(HashSet::new()), own });
    let mut hubs = HUBS.lock().unwrap_or_else(PoisonError::into_inner);
    hubs.retain(|(k, _)| *k != host_key(host));
    hubs.push((host_key(host), hub));
    registered
}

/// The command tools a run may use: the person's own, then the `proj.*` of its project's pack when the code kind is trusted (the host learns their names here). Empty with the switch off or no
/// hub for the host.
pub fn tools_of(host: &ToolHost, run: &str) -> Vec<Arc<CustomTool>> {
    if !enabled() {
        return Vec::new();
    }
    let Some(hub) = hub_of(host) else { return Vec::new() };
    let mut out: Vec<Arc<CustomTool>> = hub.own.clone();
    for t in &hub.source.defs(run).tools {
        let known = hub.learned.lock().unwrap_or_else(PoisonError::into_inner).contains(&t.name);
        if !known {
            let mut learned = hub.learned.lock().unwrap_or_else(PoisonError::into_inner);
            if learned.len() >= MAX_PROJECT_NAMES {
                tracing::warn!("command tools: the host already knows {MAX_PROJECT_NAMES} project command names; {} is not offered", t.name);
                continue;
            }
            match host.register(Arc::new(CommandToolProvider::for_run_lookup(hub.term.clone(), &t.name, hub.source.clone()))) {
                Ok(_) => {
                    learned.insert(t.name.clone());
                }
                Err(e) => {
                    tracing::warn!("command tools: {e}");
                    continue;
                }
            }
        }
        out.push(t.clone());
    }
    out
}

/// One line for the doctor and the status: what is declared and what was left out.
pub fn status_lines(data_dir: &Path) -> Vec<String> {
    let mut lines = Vec::new();
    let custom = read_custom(data_dir);
    if !enabled() {
        lines.push(format!("command tools: off (set AQUA_COMMAND_TOOLS=on to offer them){}", if custom.present { "; data/tools/custom.json is there and is not read" } else { "" }));
        return lines;
    }
    match (&custom.error, custom.present) {
        (Some(e), _) => lines.push(format!("command tools: on; {e}")),
        (None, false) => lines.push("command tools: on; data/tools/custom.json is not there (no cmd.* tool); a trusted project's tools.json gives proj.* tools".into()),
        (None, true) => lines.push(format!("command tools: on; {} cmd.* tool(s) declared ({}){}", custom.tools.len(), custom.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "), if custom.warnings.is_empty() { String::new() } else { format!("; {} warning(s): {}", custom.warnings.len(), custom.warnings.join(" | ")) })),
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_not_an_error_and_a_broken_one_is_said() {
        let d = std::env::temp_dir().join(format!("aqua_cmdtools_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("tools")).unwrap();
        let none = read_custom(&d);
        assert!(!none.present && none.error.is_none() && none.tools.is_empty());
        std::fs::write(custom_path(&d), "{ nope").unwrap();
        let bad = read_custom(&d);
        assert!(bad.present && bad.error.as_deref().is_some_and(|e| e.contains("not valid JSON")), "{bad:?}");
        std::fs::write(custom_path(&d), r#"{"tools":[{"name":"cmd.lint","description":"Lint it.","command":["npm","run","lint"]},{"name":"term.run","description":"x","command":["x"]}]}"#).unwrap();
        let some = read_custom(&d);
        assert_eq!((some.tools.len(), some.warnings.len(), some.error.is_none()), (1, 1, true), "{some:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}

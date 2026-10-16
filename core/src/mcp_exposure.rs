//! Which tools of the MCP servers a run is shown, and which runs a server is for (card OA11-03, task EX-10 of the OpenCode audit).
//!
//! The tool host decides what a run may call. This module decides what a small model is **shown**, which is a different thing and the cheaper to get wrong: a tool that is shown costs
//! prompt tokens every round, and one that is not shown is one the model cannot choose. It can only take away:
//!
//! * a server with a **scope** (`"scope": {"projects": [...], "agents": [...]}` in `mcp.json`, or a server of a repository, which is for its project) is shown to the runs of that scope and to
//!   no other; and the tool host refuses its calls to every other run (`McpProvider::classify_for` reads the run: naming a tool that was not shown gets nowhere);
//! * a server's `"expose"` says how its tools are chosen for a goal: `named` (the goal says the server's name: what it always was, and the default), `ranked` (the tools whose names and
//!   argument names fit the goal, by BM25: **names and argument names only, never what the server wrote about a tool**), or `always`. Whatever the choice, **at most eight** tools of all the
//!   servers are shown to a run, the ones that name the server first;
//! * the tools the tool host leaves out (a tool of a server that changed since it was accepted is not available) are not shown, and the run's own list of allowed tools (a command's, a skill's,
//!   an agent's: `agent_tools::tools_for_goal`) narrows what is left.
//!
//! The rest of the module is the part of the start of a run that concerns the servers of a repository: when a run begins in a project whose `.aqua` pack the person trusted as code, the
//! servers of its `mcp.json` are listed (never started: the person trusts each by itself), and when the pack is not trusted any more or the file changed, they go.

use aqua_mcp_adapters::user_servers::{Exposure, Expose, ProjectSync, RunInfo, RunInfoFn, ScopeRule, ServerManager};
use aqua_tool_host::ToolManifest;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};

/// The most tools of the servers a run is shown.
pub const MAX_SHOWN: usize = 8;

// ---- the manager of the servers, and the keys it signs with ---------------------------------------------------------------------------------------------------------------
//
// The tool host reads `mcp.json` and gives the manager to this module (it is `tool_host`'s job to build things, and this module's to say what is shown of them), together with the two
// functions that find the keys of the credential store. Handing them over, and not reaching back into `tool_host` for them, keeps the two modules from depending on each other.

static SERVERS: OnceLock<Result<Arc<ServerManager>, String>> = OnceLock::new();

/// Given once by the tool host: the servers of `mcp.json`, or why the file is wrong. `false` when it was given already.
pub fn set_servers(opened: Result<Arc<ServerManager>, String>) -> bool {
    SERVERS.set(opened).is_ok()
}

/// The servers of `data/tools/mcp.json`, or why the file is wrong. `None` before the tool host is built.
pub fn servers() -> Option<&'static Result<Arc<ServerManager>, String>> {
    SERVERS.get()
}

/// Where the keys that sign the records of trust are found: `pack` is the key of the project packs (made only when asked to: `true`), `mcp` the key of the servers of the person's
/// (made when needed; says whether it was made now).
#[derive(Clone, Copy)]
pub struct Keys {
    pub pack: fn(bool) -> Option<Vec<u8>>,
    pub mcp: fn() -> Option<(Vec<u8>, bool)>,
}

static KEYS: RwLock<Option<Keys>> = RwLock::new(None);

/// Given by the tool host (and by a test, which has keys of its own). Without them nothing a repository's pack says is trusted: there is no key to check the record by.
pub fn set_keys(keys: Keys) {
    *KEYS.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(keys);
}

fn pack_key(create: bool) -> Option<Vec<u8>> {
    let keys = *KEYS.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    keys.and_then(|k| (k.pack)(create))
}

fn mcp_key() -> Option<(Vec<u8>, bool)> {
    let keys = *KEYS.read().unwrap_or_else(std::sync::PoisonError::into_inner);
    keys.and_then(|k| (k.mcp)())
}

/// What a run is shown of the servers.
#[derive(Debug, Default)]
pub struct Shown {
    /// The tools, in the order they are shown.
    pub tools: Vec<ToolManifest>,
    /// Tools that qualified and did not fit in [`MAX_SHOWN`].
    pub omitted_tools: usize,
    /// Servers that are not shown because the run is not in their scope.
    pub omitted_servers: usize,
}

/// The id of a server in a tool's name (`mcp_<id>.<tool>`).
fn server_id_of(tool: &str) -> Option<&str> {
    tool.strip_prefix("mcp_").and_then(|rest| rest.split('.').next()).filter(|id| !id.is_empty())
}

/// Does `text` (lower case) contain `word` as a word of its own?
fn has_word(text: &str, word: &str) -> bool {
    !word.is_empty()
        && text.match_indices(word).any(|(i, _)| {
            let before = text[..i].chars().next_back().map_or(true, |c| !c.is_alphanumeric());
            let after = text[i + word.len()..].chars().next().map_or(true, |c| !c.is_alphanumeric());
            before && after
        })
}

/// The names of a tool's arguments (the keys of `properties` of its schema), in the schema's order.
fn argument_names(m: &ToolManifest) -> Vec<String> {
    m.input_schema.get("properties").and_then(|p| p.as_object()).map(|o| o.keys().cloned().collect()).unwrap_or_default()
}

/// The tools of `manifests` (the host's, sorted by name) that `run` is shown for `goal`: pure, so that the rules can be tested without a host. `rule_of` says how a server is shown and who
/// it is for, by the id in the tools' names (`None`: a server this module knows nothing about is shown as a server has always been, when the goal names it).
pub fn select(manifests: &[ToolManifest], rule_of: &dyn Fn(&str) -> Option<Exposure>, run: &RunInfo, goal: &str) -> Shown {
    let g = goal.to_ascii_lowercase();
    let mut rules: BTreeMap<String, Exposure> = BTreeMap::new();
    let mut out_of_scope: BTreeSet<String> = BTreeSet::new();
    let (mut named, mut always, mut pool): (Vec<&ToolManifest>, Vec<&ToolManifest>, Vec<&ToolManifest>) = (Vec::new(), Vec::new(), Vec::new());
    for m in manifests {
        let Some(id) = server_id_of(&m.name) else { continue };
        let rule = rules.entry(id.to_string()).or_insert_with(|| rule_of(id).unwrap_or_else(|| Exposure { name: id.to_string(), expose: Expose::Named, scope: ScopeRule::default() })).clone();
        if rule.scope.allows(run).is_err() {
            out_of_scope.insert(id.to_string());
            continue;
        }
        if has_word(&g, &rule.name) || has_word(&g, &rule.name.replace('_', " ")) {
            named.push(m);
            continue;
        }
        match rule.expose {
            Expose::Named => {}
            Expose::Always => always.push(m),
            Expose::Ranked => pool.push(m),
        }
    }
    let mut chosen: Vec<&ToolManifest> = named;
    chosen.extend(always);
    if !pool.is_empty() {
        let texts: Vec<String> = pool.iter().map(|m| format!("{} {}", m.name, argument_names(m).join(" "))).collect();
        let ranker = crate::bm25::Bm25::new(&texts);
        for (i, _) in ranker.rank(goal) {
            if !chosen.iter().any(|c| c.name == pool[i].name) {
                chosen.push(pool[i]);
            }
        }
    }
    let total = chosen.len();
    chosen.truncate(MAX_SHOWN);
    Shown { omitted_tools: total - chosen.len(), omitted_servers: out_of_scope.len(), tools: chosen.into_iter().cloned().collect() }
}

/// What a run is shown of the servers of the person and of its repository, for `goal`.
pub fn tools_for(host: &aqua_tool_host::ToolHost, goal: &str, run: &str) -> Shown {
    tools_for_with(host, servers().and_then(|r| r.as_ref().ok()).map(|m| &**m), goal, run)
}

/// [`tools_for`] for a manager that is given (`None`: every server is shown as a server has always been, when the goal names it).
pub fn tools_for_with(host: &aqua_tool_host::ToolHost, manager: Option<&ServerManager>, goal: &str, run: &str) -> Shown {
    let manifests: Vec<ToolManifest> = host.manifests().into_iter().filter(|m| m.name.starts_with("mcp_")).collect();
    if manifests.is_empty() {
        return Shown::default();
    }
    let rule_of = |id: &str| manager.and_then(|m| m.exposure_of(id));
    let shown = select(&manifests, &rule_of, &run_info(run), goal);
    if shown.omitted_tools > 0 {
        tracing::debug!("{} tool(s) of MCP servers are not shown to this run: no more than {MAX_SHOWN} are", shown.omitted_tools);
    }
    shown
}

/// The project and the agent of a run, for the scope of a server. A run has a project when its goal named a folder inside the folders Aqua works in (`project_context`), an agent when it was asked
/// to be one (`agents`, `AQUA_AGENTS=on`); a call outside a run has neither.
pub fn run_info(run: &str) -> RunInfo {
    if run.is_empty() {
        return RunInfo::default();
    }
    RunInfo { project: crate::project_context::project_dir_of_run(run).map(|p| p.to_string_lossy().to_string()), agent: crate::agents::of_run(run).map(|a| a.name) }
}

/// What the manager of the servers is given to look the run of a call up with.
pub fn run_info_fn() -> RunInfoFn {
    Arc::new(|run| run_info(run))
}

// ---- the servers of a repository -----------------------------------------------------------------------------------------------------------------------------------

/// Makes sure the manager has the key that signs the record of trust (made on first use, like the key of the packs: the person trusting the pack as code made it already). Without a key a
/// record of trust is the plain fingerprint, which whoever can write the file can make.
fn ensure_trust_key(manager: &ServerManager) {
    if !manager.has_trust_key() {
        match mcp_key() {
            Some((key, is_new)) => manager.set_trust_key(key, is_new),
            None => tracing::warn!("MCP servers of a repository: no key for the record of trust could be read or made, so none is started"),
        }
    }
}

/// The servers of the project folder `root` are what `file` (the text of its pack's `mcp.json`, when the pack's code is trusted) says. Nothing is started here.
pub fn sync_project(manager: &ServerManager, root: &Path, file: Option<&[u8]>) -> Result<ProjectSync, String> {
    if file.is_some() {
        ensure_trust_key(manager);
    }
    let sync = manager.sync_project(&root.to_string_lossy(), file);
    if let Err(why) = &sync {
        tracing::warn!("the MCP servers of the project {} are not used: {why}", root.display());
    }
    sync
}

/// Reads the pack of `root` again (what the person trusts of it now) and makes its servers what the trusted `mcp.json` says.
fn resync(manager: &ServerManager, root: &Path) -> Result<ProjectSync, String> {
    let key = pack_key(false);
    let ctx = crate::project_context::ProjectContext::resolve(&crate::runtime::data_dir(), key.as_deref(), root, root);
    sync_project(manager, root, ctx.code_file("mcp.json").as_deref())
}

/// A run begins: the servers of its project are looked at, and so are those of every other project that has some (a pack that was untrusted or edited since is read as it is now, and its
/// servers go). Does nothing unless packs are on (`AQUA_PROJECT_PACK`) and the manager exists.
pub fn run_started(run_id: &str) {
    if !crate::project_context::enabled() {
        return;
    }
    let Some(Ok(manager)) = servers() else { return };
    run_started_with(manager, run_id, &|ids| start_in_background(manager.clone(), ids));
}

/// [`run_started`] for a manager that is given, and `start` to say how the servers that are to be started are (a job, in the program; at once, in a test).
pub fn run_started_with(manager: &Arc<ServerManager>, run_id: &str, start: &dyn Fn(Vec<String>)) {
    // This run's own project: what the run's context holds (what the person trusts of the pack now, read once at the start of the run).
    let own = crate::project_context::for_run(run_id).and_then(|ctx| ctx.root.clone().map(|root| (root, ctx.code_file("mcp.json"))));
    let mut handled: Option<std::path::PathBuf> = None;
    if let Some((root, file)) = own {
        if let Ok(sync) = sync_project(manager, &root, file.as_deref()) {
            if !sync.to_start.is_empty() {
                start(sync.to_start);
            }
        }
        handled = Some(root);
    }
    // The other projects that have servers: their packs are read again.
    for shown in manager.project_roots() {
        let root = std::path::PathBuf::from(&shown);
        if handled.as_ref().is_some_and(|h| aqua_mcp_adapters::user_servers::normalise_path(&h.to_string_lossy()) == aqua_mcp_adapters::user_servers::normalise_path(&shown)) {
            continue;
        }
        if let Ok(sync) = resync(manager, &root) {
            if !sync.to_start.is_empty() {
                start(sync.to_start);
            }
        }
    }
}

/// Reads every project's pack again, for the routes that trust a server and for the reload: a server of a pack that the person does not trust any more is gone before it is trusted.
pub fn refresh_projects() -> usize {
    let Some(Ok(manager)) = servers() else { return 0 };
    let roots = manager.project_roots();
    for root in &roots {
        if let Ok(sync) = resync(manager, Path::new(root)) {
            if !sync.to_start.is_empty() {
                start_in_background(manager.clone(), sync.to_start);
            }
        }
    }
    roots.len()
}

/// Starts the servers the person trusted on a job of the registry (a server takes a while to start).
fn start_in_background(manager: Arc<ServerManager>, ids: Vec<String>) {
    if let Err(e) = crate::jobs::spawn(crate::jobs::JobKind::Maintenance, "mcp-project-start", None, move |_job| {
        manager.start_servers(&ids);
        Ok(())
    }) {
        tracing::warn!("could not start the thread that starts the MCP servers of a project: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_mcp_adapters::user_servers::parse_file;
    use aqua_tool_host::{Annotations, Decision, PermClass};
    use serde_json::json;

    fn tool(server: &str, name: &str, args: &[&str]) -> ToolManifest {
        let properties: serde_json::Map<String, serde_json::Value> = args.iter().map(|a| (a.to_string(), json!({"type": "string"}))).collect();
        ToolManifest {
            name: format!("mcp_{server}.{name}"),
            title: name.into(),
            description: "A tool of a server you added.".into(),
            classes: vec![PermClass::Remote, PermClass::ExternalEffect],
            default_decision: Decision::Ask,
            input_schema: json!({"type": "object", "properties": properties}),
            timeout_default_ms: 1000,
            timeout_max_ms: 1000,
            annotations: Annotations { read_only: false, destructive: false, idempotent: false, open_world: true },
            untrusted_output: true,
        }
    }

    fn rule(entry: &str, owner: Option<&str>, name: &str) -> Exposure {
        let e = parse_file(&format!(r#"{{"mcpServers": {{"x": {entry}}}}}"#)).unwrap().remove("x").unwrap();
        Exposure { name: name.into(), expose: if owner.is_some() { Expose::Named } else { e.expose }, scope: ScopeRule::of(&e, owner) }
    }

    fn run(project: Option<&str>) -> RunInfo {
        RunInfo { project: project.map(str::to_string), agent: None }
    }

    fn names(shown: &Shown) -> Vec<&str> {
        shown.tools.iter().map(|m| m.name.as_str()).collect()
    }

    fn catalogue() -> Vec<ToolManifest> {
        vec![
            tool("gh", "create_pull_request", &["title", "branch"]),
            tool("gh", "list_issues", &["repo"]),
            tool("notes", "add_note", &["text"]),
            tool("notes", "list_notes", &[]),
            tool("wiki", "search_pages", &["query"]),
        ]
    }

    #[test]
    fn by_default_a_server_is_shown_only_when_the_goal_names_it_as_it_always_was() {
        let none = |_: &str| None;
        let shown = select(&catalogue(), &none, &RunInfo::default(), "add a note in notes about lunch");
        assert_eq!(names(&shown), vec!["mcp_notes.add_note", "mcp_notes.list_notes"]);
        assert!(select(&catalogue(), &none, &RunInfo::default(), "open a pull request").tools.is_empty(), "a goal that names no server is shown none: the old rule");
        // The name is a word, not a part of one, and an underscore in a name is a space in a goal.
        assert!(select(&catalogue(), &none, &RunInfo::default(), "write the footnotes").tools.is_empty());
        let two_words = |id: &str| (id == "gh").then(|| Exposure { name: "git_hub".into(), expose: Expose::Named, scope: ScopeRule::default() });
        assert_eq!(select(&catalogue(), &two_words, &RunInfo::default(), "ask git hub for the issues").tools.len(), 2);
    }

    #[test]
    fn a_server_that_says_ranked_is_shown_the_tools_whose_names_fit_the_goal_and_never_by_what_it_wrote_about_them() {
        let rules = |id: &str| (id == "gh").then(|| rule(r#"{"command": "x", "expose": "ranked"}"#, None, "gh"));
        let shown = select(&catalogue(), &rules, &RunInfo::default(), "open a pull request for this branch");
        assert_eq!(names(&shown), vec!["mcp_gh.create_pull_request"], "the tool whose name and argument names share words with the goal; nothing of the others");
        assert!(select(&catalogue(), &rules, &RunInfo::default(), "write a poem").tools.is_empty(), "a goal that fits nothing is shown nothing, however the server is exposed");
        // A description does not count: a server that stuffs its description with words cannot make itself fit.
        let mut stuffed = catalogue();
        stuffed[1].description = "open a pull request pull request pull request".into();
        assert_eq!(names(&select(&stuffed, &rules, &RunInfo::default(), "open a pull request for this branch")), vec!["mcp_gh.create_pull_request"]);
        // The servers that did not say ranked are still shown only when named.
        assert!(!names(&select(&catalogue(), &rules, &RunInfo::default(), "add a note about the branch")).iter().any(|n| n.starts_with("mcp_notes")));
    }

    #[test]
    fn a_server_that_says_always_is_shown_to_the_runs_in_its_scope_and_the_names_come_first_and_eight_is_the_most() {
        let rules = |id: &str| match id {
            "wiki" => Some(rule(r#"{"command": "x", "expose": "always"}"#, None, "wiki")),
            "gh" => Some(rule(r#"{"command": "x", "expose": "always", "scope": {"projects": ["e:/work/app"]}}"#, None, "gh")),
            _ => None,
        };
        let outside = select(&catalogue(), &rules, &run(Some("E:\\play")), "do something");
        assert_eq!(names(&outside), vec!["mcp_wiki.search_pages"], "gh is for e:/work/app only");
        assert_eq!(outside.omitted_servers, 1);
        let inside = select(&catalogue(), &rules, &run(Some("E:\\Work\\App")), "add a note in notes");
        assert_eq!(names(&inside), vec!["mcp_notes.add_note", "mcp_notes.list_notes", "mcp_gh.create_pull_request", "mcp_gh.list_issues", "mcp_wiki.search_pages"], "named first, then the always ones");
        // More than eight: the eight that come first, and the rest are counted.
        let many: Vec<ToolManifest> = (0..12).map(|i| tool("big", &format!("tool_{i:02}"), &[])).collect();
        let big = |id: &str| (id == "big").then(|| rule(r#"{"command": "x", "expose": "always"}"#, None, "big"));
        let shown = select(&many, &big, &RunInfo::default(), "anything");
        assert_eq!((shown.tools.len(), shown.omitted_tools), (MAX_SHOWN, 4));
        assert_eq!(shown.tools[0].name, "mcp_big.tool_00");
    }

    #[test]
    fn a_scope_hides_a_server_from_the_other_projects_and_agents_whatever_the_goal_says() {
        let rules = |id: &str| match id {
            "notes" => Some(rule(r#"{"command": "x", "scope": {"projects": ["e:/work/*"], "agents": ["build"]}}"#, None, "notes")),
            _ => None,
        };
        let goal = "add a note in notes";
        assert_eq!(select(&catalogue(), &rules, &run(Some("E:\\Work\\app")), goal).tools.len(), 2);
        let elsewhere = select(&catalogue(), &rules, &run(Some("D:\\other")), goal);
        assert!(elsewhere.tools.is_empty() && elsewhere.omitted_servers == 1, "named in the goal, and still not shown: {elsewhere:?}");
        assert!(select(&catalogue(), &rules, &RunInfo::default(), goal).tools.is_empty(), "a run with no project");
        let planner = RunInfo { project: Some("E:\\Work\\app".into()), agent: Some("plan".into()) };
        assert!(select(&catalogue(), &rules, &planner, goal).tools.is_empty(), "another agent");
        // A server of a repository is for that repository's runs, and is shown when its (short) name is in the goal.
        let packed = |id: &str| (id == "notes").then(|| rule(r#"{"command": "x", "expose": "always", "scope": {"projects": ["z:/anywhere"]}}"#, Some("E:\\Work\\App"), "notes"));
        assert_eq!(select(&catalogue(), &packed, &run(Some("E:\\Work\\App")), goal).tools.len(), 2);
        assert!(select(&catalogue(), &packed, &run(Some("z:/anywhere")), goal).tools.is_empty());
        assert!(select(&catalogue(), &packed, &run(Some("E:\\Work\\App")), "do something else").tools.is_empty(), "and `always` is not read from a repository's file: named only");
    }
}

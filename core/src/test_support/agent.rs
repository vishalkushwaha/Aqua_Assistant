//! `AgentWorld`: the agent loop, run end to end for a test with no model (card OA2-04; tasks QA-03 and QA-08 of the OpenCode audit).
//!
//! A scenario needs the whole of a run and none of the model: the loop itself (`run_mcp_agent_core_with`, the function the core runs a request through), the tool host Aqua really builds
//! (`build_tool_host`: the real `fs.*` and `term.*` providers, the real policy, masking, cuts and audit ledger), a record of the run in a store of its own, a workspace made of the
//! files the scenario names, and a person who answers the host's questions from a script. What stands in for the model is any [`Planner`], usually a [`super::ScriptedPlanner`].
//!
//! * [`AgentWorld::run`] is a request as the core serves it: the run is recorded, the ledger is written, the answer comes back.
//! * [`AgentWorld::run_killed`] is a request whose process dies: the loop stops where the [`Person`] is asked about a call (or where a [`KillAt`] planner is asked for its
//!   n-th reply), and **the record is left exactly as a killed process leaves it** (the run still `running`, the step still `started`, nothing finished).
//! * [`AgentWorld::restart`] is the next start of the core over the same data folder: a new host (the ledger is opened again, its chain checked), the run store is opened
//!   again and the runs the dead process left going are marked `interrupted`, as `agent_runs::init` does.
//! * [`AgentWorld::plan_resume`] and [`AgentWorld::run_resumed`] are `POST /api/v1/runs/{id}/resume`: the plan from the record (and the question about the call that was
//!   running), and the run that goes on.
//!
//! The world holds the environment lock (`EnvGuard`) for as long as it lives, clears every `AQUA_*` switch the machine has (so that a scenario is the same on every machine) and
//! makes itself the data folder (`AQUA_WORKSPACE_ROOT`), so that two worlds never run at the same time in a process and nothing the loop writes reaches the real `data/`.
//! A host is kept for the life of the process (`agent_tools::with_host` leaks it, as the tests of the loop inside this crate do): a few kilobytes and a watcher thread each.

use super::{EnvGuard, TempWorld};
use crate::agent_runs::{resume, AgentRunStore, Recording};
use crate::memory::AgentLearning;
use crate::modules::agent_experience::ExperienceUse;
use crate::modules::browser_agent::{self, McpAgentResult, Resume};
use crate::modules::planner::{PlanReply, PlanRequest, Planner};
use anyhow::Result;
use aqua_mcp_adapters::browser::BrowserBackend;
use aqua_mcp_adapters::{new_browser_client, new_windows_client};
use aqua_tool_host::{ApprovalOutcome, ApprovalRequest, ApprovalScope, Approver, ProviderFuture, ToolHost};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The person at the screen, as a script: it answers the host's questions from `script`, then with `otherwise`, and keeps what it was asked. It can also be made to die when it is
/// asked about a tool (the process is killed while the question is open).
pub struct Person {
    script: Mutex<VecDeque<ApprovalOutcome>>,
    otherwise: Mutex<ApprovalOutcome>,
    asked: Mutex<Vec<ApprovalRequest>>,
    dies_asked_about: Mutex<Option<String>>,
    /// What the person types with every "no" (card OA10-03, AG-11); `None`: nothing.
    words_with_a_no: Mutex<Option<String>>,
}

impl Person {
    pub fn new(otherwise: ApprovalOutcome) -> Self {
        Self { script: Mutex::new(VecDeque::new()), otherwise: Mutex::new(otherwise), asked: Mutex::new(Vec::new()), dies_asked_about: Mutex::new(None), words_with_a_no: Mutex::new(None) }
    }

    /// What the person types with each "no" they say (`None`: they type nothing), as the app's box for a reason does.
    pub fn say_with_a_no(&self, words: Option<&str>) {
        *self.words_with_a_no.lock().unwrap_or_else(|e| e.into_inner()) = words.map(str::to_string);
    }

    /// The answers to the next questions, in order (after them `otherwise` answers).
    pub fn then(&self, answers: &[ApprovalOutcome]) {
        self.script.lock().unwrap_or_else(|e| e.into_inner()).extend(answers.iter().copied());
    }

    /// What the person says to every question that is not in the script.
    pub fn say_to_the_rest(&self, outcome: ApprovalOutcome) {
        *self.otherwise.lock().unwrap_or_else(|e| e.into_inner()) = outcome;
    }

    /// The process dies when the person is asked about `tool` (or about any tool, with `None`): the question is never answered.
    pub fn die_when_asked_about(&self, tool: Option<&str>) {
        *self.dies_asked_about.lock().unwrap_or_else(|e| e.into_inner()) = Some(tool.unwrap_or("").to_string());
    }

    /// The process does not die any more when it is asked something.
    pub fn live(&self) {
        *self.dies_asked_about.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Every question put to the person so far, in order.
    pub fn asked(&self) -> Vec<ApprovalRequest> {
        self.asked.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The tools of the questions, in order.
    pub fn asked_tools(&self) -> Vec<String> {
        self.asked().into_iter().map(|q| q.tool).collect()
    }
}

impl Approver for Person {
    fn denial_reason(&self, _approval_id: &str) -> Option<String> {
        self.words_with_a_no.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn request(&self, req: ApprovalRequest) -> ProviderFuture<'_, ApprovalOutcome> {
        self.asked.lock().unwrap_or_else(|e| e.into_inner()).push(req.clone());
        let dies = self.dies_asked_about.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(tool) = dies {
            if tool.is_empty() || tool == req.tool {
                // The process is killed here: this thread does not go on, and nothing after the question is ever written.
                panic!("the process was killed while the person was asked about {}", req.tool);
            }
        }
        let answer = self.script.lock().unwrap_or_else(|e| e.into_inner()).pop_front().unwrap_or(*self.otherwise.lock().unwrap_or_else(|e| e.into_inner()));
        Box::pin(async move { answer })
    }
}

/// A planner that dies (panics, as a killed process does not go on) when it is asked for its `n`th reply (counting from 1), and until then gives what `inner` gives.
pub struct KillAt<'a> {
    pub inner: &'a dyn Planner,
    pub n: usize,
    asked: AtomicUsize,
}

impl<'a> KillAt<'a> {
    pub fn new(inner: &'a dyn Planner, n: usize) -> Self {
        Self { inner, n, asked: AtomicUsize::new(0) }
    }
}

impl Planner for KillAt<'_> {
    fn model_name(&self) -> String {
        self.inner.model_name()
    }

    fn max_context_tokens(&self) -> u32 {
        self.inner.max_context_tokens()
    }

    fn warm_prefix(&self, prefix: &str) -> Result<()> {
        self.inner.warm_prefix(prefix)
    }

    fn plan(&self, req: &PlanRequest<'_>, on_tick: &mut dyn FnMut(u64)) -> Result<PlanReply> {
        let n = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.n {
            panic!("the process was killed while the model was asked for reply {n}");
        }
        self.inner.plan(req, on_tick)
    }
}

/// A planner that does something (the person presses Stop: a flag is set, or the running agents are cancelled) when it is asked for its `n`th reply, and answers like `inner`.
pub struct StopAt<'a> {
    pub inner: &'a dyn Planner,
    pub n: usize,
    action: Box<dyn Fn() + 'a>,
    asked: AtomicUsize,
}

impl<'a> StopAt<'a> {
    pub fn new(inner: &'a dyn Planner, n: usize, action: impl Fn() + 'a) -> Self {
        Self { inner, n, action: Box::new(action), asked: AtomicUsize::new(0) }
    }
}

impl Planner for StopAt<'_> {
    fn model_name(&self) -> String {
        self.inner.model_name()
    }

    fn max_context_tokens(&self) -> u32 {
        self.inner.max_context_tokens()
    }

    fn warm_prefix(&self, prefix: &str) -> Result<()> {
        self.inner.warm_prefix(prefix)
    }

    fn plan(&self, req: &PlanRequest<'_>, on_tick: &mut dyn FnMut(u64)) -> Result<PlanReply> {
        let n = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.n {
            (self.action)();
        }
        self.inner.plan(req, on_tick)
    }
}

/// What a run that was killed left: its id in the record, and whether the loop really did stop by dying (a scenario that never reaches the place it kills at is a mistake).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Killed {
    pub run_id: String,
    pub died: bool,
}

/// A throw-away world with a workspace, a tool host, a run store and a person (see the module's page).
pub struct AgentWorld {
    // Dropped in this order: the store and the host first, then the environment is put back, and the folder goes last.
    pub store: Arc<AgentRunStore>,
    pub host: Arc<ToolHost>,
    pub person: Arc<Person>,
    _env: EnvGuard,
    pub world: TempWorld,
    /// The folder the file tools work in: `@ws/...` in a call, `<root>/work space/...` on the disk.
    pub ws: PathBuf,
    data: PathBuf,
}

/// Every `AQUA_*` switch of this process, by name.
fn aqua_switches() -> Vec<String> {
    std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).filter(|k| k.starts_with("AQUA_")).collect()
}

/// A world of an earlier run that could not be removed when its test ended is removed now. One file of each stays (the database of what the person asked Aqua to remember, which
/// a part of the program keeps open for the life of the process, and Windows will not delete an open file), so the folders of the worlds of a run are removed by the next run, once they are
/// old enough (a quarter of an hour) not to belong to a run that is still going.
fn sweep_old_worlds() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else { return };
    for entry in entries.flatten() {
        let old_enough = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > std::time::Duration::from_secs(15 * 60));
        if old_enough && entry.file_name().to_string_lossy().starts_with("aqua_scenario_") {
            let _ = super::world::remove_tree(&entry.path());
        }
    }
}

fn build_host(data: &Path, world: &TempWorld, person: &Arc<Person>) -> Arc<ToolHost> {
    // (The trust record of MCP servers is rewritten by a rename when the host starts; a world that has none keeps the start-up quiet.)
    let browser = Arc::new(BrowserBackend::new(&world.path("browser"), new_browser_client()));
    let host = crate::tool_host::build_tool_host(data, browser, Arc::new(Mutex::new(new_windows_client())));
    host.set_approver(person.clone());
    host
}

impl AgentWorld {
    /// A world named `name` whose workspace holds `files` (`(path below the workspace, text)`), and whose person says `otherwise` to every question. The workspace is a trusted
    /// root of the file tools (what Aqua's own workspace is): a change inside it is made without asking.
    pub fn new(name: &str, files: &[(&str, &str)], otherwise: ApprovalOutcome) -> Self {
        Self::with_trust(name, files, otherwise, true)
    }

    /// [`new`](Self::new) with the workspace a root the person has **not** trusted: the host asks before it changes anything in it.
    pub fn with_trust(name: &str, files: &[(&str, &str)], otherwise: ApprovalOutcome, trusted: bool) -> Self {
        sweep_old_worlds();
        let world = TempWorld::new(&format!("scenario_{name}"));
        let mut env = EnvGuard::hold();
        for switch in aqua_switches() {
            env.remove_var(&switch);
        }
        env.set_var(aqua_mcp_adapters::browser_mcp::health::WORKSPACE_ROOT_ENV, world.root.as_os_str());
        // The workspace folder has a space in its name, as the folders of people's own computers so often do (a path that is split at a space is found at once).
        let ws = world.path("work space");
        std::fs::create_dir_all(&ws).expect("the workspace is made");
        for (relative, text) in files {
            world.write(&format!("work space/{relative}"), text);
        }
        let data = world.data();
        std::fs::create_dir_all(data.join("tools")).expect("the tools folder is made");
        let mut cfg = aqua_native_tools::fs::FsConfig::with_workspace(ws.to_string_lossy().replace('\\', "/"));
        for root in &mut cfg.roots {
            root.trusted = trusted;
        }
        std::fs::write(data.join("tools").join("fs.json"), serde_json::to_string_pretty(&cfg).expect("the config is text")).expect("the file tools are configured");
        // No scenario may reach the network: the web tools are configured to refuse every address (a loop that searches by itself for a goal that sounds like a look-up finds the search
        // service on a port nothing listens on). A call that is refused is still a line of the ledger, and a scenario whose run has a line outside its tools fails.
        let http = serde_json::json!({"allow": [], "deny": [], "allow_private": [], "blocked_ports": [], "unknown_hosts": "deny", "auth": {}, "search": {"engine": "searxng", "url": "http://127.0.0.1:9"}, "search_fallback": [], "scrape": false});
        std::fs::write(data.join("tools").join("http.json"), serde_json::to_string_pretty(&http).expect("the config is text")).expect("the web tools are configured");
        let person = Arc::new(Person::new(otherwise));
        let host = build_host(&data, &world, &person);
        let store = Arc::new(AgentRunStore::open(data.join("agent_runs.db")).expect("the run store opens"));
        Self { store, host, person, _env: env, world, ws, data }
    }

    /// Adds a tool, `mcp_probe.boom`, whose provider panics when it is called (a bug in a provider). The host does not catch a panic: it goes up through the loop, as it would in the core.
    pub fn add_panicking_tool(&self) {
        self.host.register(Arc::new(PanicProvider)).expect("the probe provider is registered");
    }

    /// The text of a workspace file, or `None` when there is no such file.
    pub fn read(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.ws.join(relative)).ok()
    }

    /// The data folder of this world (`<root>/data`).
    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    /// Every file of the workspace and of the data folder whose name says a write was cut short (`.aqua-tmp-*`, `*.partial`).
    pub fn leftovers(&self) -> Vec<String> {
        let mut found = Vec::new();
        let mut stack = vec![self.world.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_symlink() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if kind.is_dir() {
                    stack.push(entry.path());
                } else if name.contains(".aqua-tmp-") || name.ends_with(".partial") {
                    found.push(entry.path().strip_prefix(&self.world.root).unwrap_or(&entry.path()).to_string_lossy().replace('\\', "/"));
                }
            }
        }
        found.sort();
        found
    }

    /// A request as the core serves it (`run_mcp_agent_core_with`): the run is recorded under its own id (`McpAgentResult::run_id`) and the host writes its ledger. `Err` is what the
    /// core answers with when the planner fails before any call was made.
    pub fn run(&self, planner: &dyn Planner, goal: &str, max_steps: u32) -> Result<McpAgentResult> {
        crate::agent_tools::with_host(self.host.clone(), || {
            let learning = AgentLearning::at(&self.world.root);
            let mut exp = ExperienceUse::default();
            crate::runtime::data_tripwire::with_workspace_root(&self.world.root, || {
                browser_agent::run_mcp_agent_core_with(planner, &learning, Some(self.store.clone()), None, None, goal, max_steps, None, None, &mut exp)
            })
        })
    }

    /// A request whose process is killed. The loop is expected to die where it is (the [`Person`] is told to die when asked, or the planner is a [`KillAt`]); what it leaves in the
    /// record is what a killed process leaves: the run is not finished, and neither is the call that was running. (It dies by a panic that is caught here, on this thread: a planner is
    /// not `Send`, so the loop cannot be given a thread of its own.)
    pub fn run_killed(&self, planner: &dyn Planner, goal: &str, max_steps: u32) -> Killed {
        self.run_unfinished(planner, goal, max_steps, None)
    }

    /// [`run`](Self::run) with the parts a kill must be able to cut: the run's record is begun here, and when the loop dies it is forgotten and not finished (a normal end finishes it).
    fn run_unfinished(&self, planner: &dyn Planner, goal: &str, max_steps: u32, from: Option<&resume::Plan>) -> Killed {
        let resume = from.map(|p| Resume { parent_run_id: p.parent_run_id.clone(), history: p.history.clone(), questions_asked: 0, unknown_digests: resume::unknown_digests(p), checklist: p.checklist.clone(), steered: p.steered.clone(), ..Default::default() });
        let block = from.map(resume::context_block);
        browser_agent::with_agent_context(block, || {
            browser_agent::with_agent_resume(resume, || {
                crate::agent_tools::with_host(self.host.clone(), || {
                    let learning = AgentLearning::at(&self.world.root);
                    let mut exp = ExperienceUse::default();
                    let scope = crate::agent_runs::scope::RunScope::begin(crate::agent_tools::host().cloned());
                    let id = scope.id().to_string();
                    browser_agent::take_from_parent_run_in(&id, Some(self.store.as_ref()));
                    let parent = from.map(|p| p.parent_run_id.as_str());
                    let rec = Recording::begin_in(self.store.clone(), &id, goal, "api", "agent", parent).expect("the record opens");
                    crate::agent_runs::note_marks();
                    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        crate::runtime::data_tripwire::with_workspace_root(&self.world.root, || {
                            browser_agent::run_mcp_agent_inner(planner, &learning, None, None, goal, max_steps, None, None, &mut exp)
                        })
                    }));
                    match out {
                        Ok(Ok(r)) => {
                            rec.finish(&r.stopped_reason, r.tool_rounds, &r.final_answer, &r.steps);
                            Killed { run_id: id, died: false }
                        }
                        Ok(Err(e)) => {
                            rec.finish(&format!("error: {e:#}"), 0, "", &[]);
                            Killed { run_id: id, died: false }
                        }
                        Err(_) => {
                            // The process is gone: no `finish`, and no `Drop` that would tell the record how it ended.
                            std::mem::forget(rec);
                            Killed { run_id: id, died: true }
                        }
                    }
                })
            })
        })
    }

    /// The next start of the core over the same data folder: a new host (its ledger opened again and its chain checked), the run store opened again, and the runs that were going
    /// when the process died marked `interrupted` (`agent_runs::init`). Returns how many runs that was.
    pub fn restart(&mut self) -> usize {
        self.person.live();
        self.host = build_host(&self.data, &self.world, &self.person);
        self.store = Arc::new(AgentRunStore::open(self.data.join("agent_runs.db")).expect("the run store opens again"));
        let marked = self.store.mark_interrupted(crate::agent_runs::now_ms()).expect("the sweep runs");
        let _ = self.store.expire_waiting(crate::agent_runs::now_ms(), 0);
        marked
    }

    /// What it takes to go on from `run_id` (`POST /api/v1/runs/{id}/resume`), with what the person said about the calls that were running.
    pub fn plan_resume(&self, run_id: &str, choice: Option<resume::Choice>) -> Result<resume::Plan, resume::Refused> {
        let host = self.host.clone();
        resume::plan(&self.store, run_id, &crate::agent_runs::scope::is_active, &|tool| crate::agent_tools::can_change_something(Some(host.as_ref()), tool), choice)
    }

    /// The run that goes on after the person answered the question a run ended with (`POST /api/v1/runs/{id}/answer`, card OA10-03, AG-03), as `pipeline::run_answered_agent` starts it: the
    /// same task, as the caller that asked, with the question and the answer as data and the calls that came back as history.
    pub fn run_answered(&self, plan: &crate::agent_runs::answer::Plan, planner: &dyn Planner, max_steps: u32) -> Result<McpAgentResult> {
        let (resume, block) = plan.resume_in(&self.store);
        let caller = browser_agent::windows_caller_named(&plan.caller);
        browser_agent::with_agent_context(Some(block), || browser_agent::with_agent_resume(Some(resume), || browser_agent::with_windows_caller(caller, || self.run(planner, &plan.goal, max_steps))))
    }

    /// The run that goes on from `plan`, as `pipeline::run_resumed_agent` starts it.
    pub fn run_resumed(&self, plan: &resume::Plan, planner: &dyn Planner, max_steps: u32) -> Result<McpAgentResult> {
        let resume = Resume { parent_run_id: plan.parent_run_id.clone(), history: plan.history.clone(), questions_asked: 0, unknown_digests: resume::unknown_digests(plan), checklist: plan.checklist.clone(), steered: plan.steered.clone(), ..Default::default() };
        browser_agent::with_agent_context(Some(resume::context_block(plan)), || browser_agent::with_agent_resume(Some(resume), || self.run(planner, &plan.goal, max_steps)))
    }

    /// The lines the host's ledger holds for `run_id`, oldest first, as the ledger wrote them (`tool`, `decision`, `ok`, ... and the hashes that chain them).
    pub fn ledger_lines(&self, run_id: &str) -> Vec<serde_json::Value> {
        aqua_tool_host::proof::run_proof(self.host.audit(), run_id, &crate::audit_key::known_public_keys(&self.data)).lines.into_iter().map(|l| l.line).collect()
    }

    /// What is wrong with the host's ledger or with the lines of `run_id`, in words (empty when the chain, the signed heads and the run's lines are right).
    pub fn ledger_problems(&self, run_id: &str) -> Vec<String> {
        resume::ledger_view(self.host.audit(), &crate::audit_key::known_public_keys(&self.data), run_id).problems
    }

    /// Does the record of `run_id` agree with the ledger? (`resume::check_record`, the check before a run is gone on from.)
    pub fn record_agrees_with_ledger(&self, run_id: &str) -> Result<(), resume::Refused> {
        let served: std::collections::HashSet<String> = self.host.manifests().into_iter().map(|m| m.name).collect();
        let view = resume::ledger_view(self.host.audit(), &crate::audit_key::known_public_keys(&self.data), run_id);
        resume::check_record(&self.store, run_id, &view, &|t| served.contains(t))
    }
}

/// A provider of one tool, `mcp_probe.boom`, that panics when it is called: a provider with a bug (see [`AgentWorld::add_panicking_tool`]). The name is that of a tool of an MCP server the
/// person added, because those are the host tools a goal can call that are not in the packs of Aqua's own: the loop offers one when the goal names the server ("probe").
pub struct PanicProvider;

impl aqua_tool_host::ToolProvider for PanicProvider {
    fn id(&self) -> &str {
        "mcp_probe"
    }

    fn manifests(&self) -> Vec<aqua_tool_host::ToolManifest> {
        vec![aqua_tool_host::ToolManifest {
            name: "mcp_probe.boom".into(),
            title: "mcp_probe.boom".into(),
            description: "A tool that has a bug and panics.".into(),
            classes: vec![aqua_tool_host::PermClass::Read],
            default_decision: aqua_tool_host::Decision::Auto,
            input_schema: serde_json::json!({"type": "object", "properties": {}, "required": []}),
            timeout_default_ms: 5_000,
            timeout_max_ms: 5_000,
            annotations: aqua_tool_host::Annotations { read_only: true, destructive: false, idempotent: true, open_world: false },
            untrusted_output: false,
        }]
    }

    fn classify(&self, _tool: &str, _args: &serde_json::Value) -> Result<aqua_tool_host::Classification, aqua_tool_host::ToolError> {
        Ok(aqua_tool_host::Classification { classes: vec![aqua_tool_host::PermClass::Read], target: "probe".into(), summary: "Call the probe".into(), target_allowed: true })
    }

    fn call<'a>(&'a self, _tool: &'a str, _args: serde_json::Value, _ctx: &'a aqua_tool_host::ExecCtx) -> ProviderFuture<'a, Result<aqua_tool_host::ProviderOutput, aqua_tool_host::ToolError>> {
        Box::pin(async move {
            if true {
                panic!("the probe provider has a bug");
            }
            Ok(aqua_tool_host::ProviderOutput::default())
        })
    }
}

/// The scope a person's `Always`/`Session` answer has: for a script that says "yes" to a check it runs again and again.
pub fn yes_for_the_session() -> ApprovalOutcome {
    ApprovalOutcome::Approved(ApprovalScope::Session)
}

/// A single yes.
pub fn yes_once() -> ApprovalOutcome {
    ApprovalOutcome::Approved(ApprovalScope::Once)
}

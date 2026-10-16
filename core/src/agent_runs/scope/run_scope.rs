//! A run's identity and life on its thread: the id, the scope that ends it, the options a command gave it, the runs that start as having read outside text, and what the process knows of the runs that are going on (move only: this was part of `agent_tools/run_scope.rs`, card OA13-03, task AQ-12). What stays in `agent_tools/run_scope.rs` is the start of a run (`RunScope::begin`), the end of it (`Drop for RunScope`) and the hooks at its start: each of them names `queue` or `project_context`, which `agent_runs` does not name.

use super::*;

/// How many agent runs are going on now (each holds a [`RunScope`]): the reload of the config files waits until there are none (card OA11-03).
pub fn runs_in_flight() -> usize {
    registry().runs_in_flight()
}

/// `tools.search` found these for the run: they are callable from its next round (kept in the registry of the runs until it uses them: the loop adds them to what it may call).
pub fn note_found(run_id: &str, names: &[String]) {
    registry().note_found(run_id, names);
}

/// The tools found for this run since the last time it asked (each only once).
pub fn take_found(run_id: &str) -> Vec<String> {
    registry().take_found(run_id)
}

// The runs that are going on (the active ones, the ones in the background, the unattended ones, who asked for each, the options of each) and the runs that just ended are the fields of
// `registry::RunRegistry` (card OA13-03, task AQ-12); `registry()` is the one of this process.

fn process_started_ms() -> u128 {
    static STARTED: OnceLock<u128> = OnceLock::new();
    *STARTED.get_or_init(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0))
}

thread_local! {
    pub(crate) static RUN_ID: RefCell<String> = const { RefCell::new(String::new()) };
    /// The id the next run that begins on this thread is given, when it was told to the client before it began (a detached run: card OA10-03, UX-03). Taken by the first run.
    pub(crate) static PLANNED_RUN_ID: RefCell<Option<String>> = const { RefCell::new(None) };
    /// How long a question about a call of the run on this thread may wait, when the run asked for longer than the default (see `RequestContext::approval_wait_secs`).
    static APPROVAL_WAIT: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// How long a question about a call of the run on this thread may wait, when the run asked for something other than the default (see [`with_approval_wait`]).
pub fn approval_wait_now() -> Option<u64> {
    APPROVAL_WAIT.with(std::cell::Cell::get)
}

/// A run id that nothing has: the same shape as the ids [`RunScope::begin`] makes, so that a client can be told the id of a run before the run begins (card OA10-03, UX-03).
pub fn new_run_id() -> String {
    format!("run_{}_{:x}_{}", std::process::id(), process_started_ms(), registry().next_seq())
}

/// Runs `f`, and the first run that begins on this thread while it runs is given `id` (made by [`new_run_id`]) instead of one of its own. The previous setting comes back when `f` ends.
pub fn with_planned_run_id<R>(id: &str, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<String>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PLANNED_RUN_ID.with(|p| *p.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(PLANNED_RUN_ID.with(|p| p.borrow_mut().replace(id.to_string())));
    f()
}

/// Runs `f`, and the questions the host puts to the person about the calls of the runs on this thread may wait `secs` seconds for an answer (at most ten minutes), instead of the default.
/// `None`: the default. The previous setting comes back when `f` ends.
pub fn with_approval_wait<R>(secs: Option<u64>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<u64>);
    impl Drop for Restore {
        fn drop(&mut self) {
            APPROVAL_WAIT.with(|a| a.set(self.0));
        }
    }
    let _restore = Restore(APPROVAL_WAIT.with(|a| a.replace(secs)));
    f()
}

/// Is a run the person is waiting for going on (not one in the background)?
pub fn foreground_run_active() -> bool {
    foreground_run_id().is_some()
}

/// The run the person is waiting for: the oldest active one that is neither in the background nor unattended (a step of a workflow that began first is not the
/// person's task, and a line said to the task would otherwise go to it).
pub fn foreground_run_id() -> Option<String> {
    registry().foreground_run_id()
}

/// Who asked for the run that is going on: `"ui"` (the person's own app), `"api"` (a script with the API token) or `""` (nobody marked it, or it is not going on). Kept in memory from the
/// moment the run begins, so that it does not wait for the run's record (which a run may not have, and which comes a little later than the run is active).
pub fn origin_of_run(run_id: &str) -> &'static str {
    registry().origin_of(run_id)
}

/// Is this process working on the run right now?
pub fn is_active(run_id: &str) -> bool {
    registry().is_active(run_id)
}

/// The id of the agent run on this thread ("" outside a run).
pub fn current_run() -> String {
    RUN_ID.with(|r| r.borrow().clone())
}

thread_local! {
    /// While a [`TaintedStart`] lives, every run begun on this thread starts as one that has read text from outside.
    pub(crate) static STARTS_TAINTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs begun on this thread while this lives start as ones that **have already read text from outside** (card OA8-04, SE-12): the agent step of a workflow or a flow that is fed with
/// what an earlier step read, or with what a webhook sent, is a run of its own with an identity of its own, and without this it would start clean and so be let do what a run that had
/// read the same text is not. Nothing is cleaned by dropping it: what the runs read stays read.
#[must_use = "the runs are tainted only while the guard lives"]
pub struct TaintedStart {
    previous: bool,
}

/// See [`TaintedStart`].
pub fn runs_start_tainted() -> TaintedStart {
    TaintedStart { previous: STARTS_TAINTED.with(|c| c.replace(true)) }
}

impl Drop for TaintedStart {
    fn drop(&mut self) {
        STARTS_TAINTED.with(|c| c.set(self.previous));
    }
}

/// What a command (card OA11-02) says about the run it starts. A run that is not started by a command has none of this.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// The run may call only these tools (patterns such as `fs.read` or `git.*`): the host refuses the others, with the refusal a loaded skill's list gives. Empty: no narrowing. It can only narrow:
    /// there is no way to put a tool back.
    pub allowed_tools: Vec<String>,
    /// The packs of tools the run is shown (`files`, `git`, ...), instead of the ones the words of its goal pick. Empty: the words decide (or the tools allowed, when there are some).
    pub forced_packs: Vec<String>,
    /// The goal holds text a program printed (the output of a step): the run starts as one that has read text from outside, so what it changes asks as it does after a web page.
    pub tainted: bool,
}

thread_local! {
    /// Options given to the next run begun on this thread (see [`with_run_options`]).
    pub(crate) static PENDING_OPTIONS: RefCell<Option<RunOptions>> = const { RefCell::new(None) };
}

/// Runs `f` with `options` waiting for the run it starts: the first run begun on this thread while `f` runs (`RunScope::begin`) takes them, whether `f` ends well or not. This is how the entry that
/// starts an agent run (the agent routes, which command a run is started by) says "only these tools" and "these packs" without every function between them carrying the words.
pub fn with_run_options<R>(options: RunOptions, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<RunOptions>);
    impl Drop for Restore {
        fn drop(&mut self) {
            PENDING_OPTIONS.with(|p| *p.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(PENDING_OPTIONS.with(|p| p.borrow_mut().replace(options)));
    f()
}

/// The options of a run a command started, or `None`.
pub fn options_of(run_id: &str) -> Option<RunOptions> {
    if run_id.is_empty() {
        return None;
    }
    registry().options_of(run_id)
}

/// What a run that has just ended held: (it had read text from outside, it held data that is the person's). `None` for a run this process does not remember (it keeps the newest 64).
/// The workflow and flow steps ask it about the agent run they started, to carry what it read back to the run they belong to.
pub fn ended_run_marks(run_id: &str) -> Option<(bool, bool)> {
    registry().ended_marks(run_id)
}

/// A run's identity. Dropping it forgets the run's taint and cancel flag.
pub struct RunScope {
    pub(crate) id: String,
    pub(crate) previous: String,
    pub(crate) host: Option<Arc<ToolHost>>,
    /// The span `run{run_id, route, caller}` (card OA3-04): every line the run's thread logs while the scope lives is written inside it, so the lines of one run can be told from
    /// the next (`aqua-cli logs --run <id>`). Only names and ids are in it.
    pub(crate) span: tracing::Span,
    /// Where the thread is inside that span; left when the scope is dropped, after everything the drop logs.
    pub(crate) _entered: tracing::span::EnteredSpan,
}

impl RunScope {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Says which route and which caller this run is, once the loop knows: the lines of the run carry both in their `run` span.
    pub fn describe(&self, route: &str, caller: &str) {
        self.span.record("route", route).record("caller", caller);
    }
}

/// What a run that ended held is kept by the registry of the runs, the newest 64 (`registry::EndedRun`): a run that goes on after a question is a new run with a new id, so what its
/// parent held has to be handed on, or answering a question would wash the taint off, and the list of a skill that was loaded would be forgotten with the id it was kept under.
pub(crate) fn note_ended(host: &ToolHost, id: &str) {
    registry().note_ended(EndedRun { id: id.to_string(), tainted: host.is_tainted(id), private: host.is_private(id), restrictions: host.restrictions_of(id) });
}

thread_local! {
    /// The run that ended last on this thread (see [`run_that_just_ended`]).
    pub(crate) static LAST_ENDED: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// The id of the run whose scope ended last on this thread: the run that asked a question the person has not answered yet when it was not recorded (so its result has
/// no record id), and what it held is looked up by this id.
pub fn run_that_just_ended() -> Option<String> {
    LAST_ENDED.with(|l| Some(l.borrow().clone()).filter(|id| !id.is_empty()))
}

/// Stop the host calls of every run the person is waiting for (the agent's own cancel flag stops the loop). A run in the background is left going:
/// it is ended from the queue (`queue::cancel`), not by the Stop that is meant for what is being said now.
pub fn cancel_active() {
    let Some(host) = host() else { return };
    registry().for_each_not_in_background(|id| host.cancel_run(id));
}

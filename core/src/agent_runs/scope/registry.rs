//! What this process knows about the agent runs that are going on and the ones that just ended (card OA13-03, task AQ-12 of the OpenCode audit). It was `agent_tools/registry.rs` and is moved here, to `agent_runs::scope`, with the run scope and the run marks that use it.
//!
//! Nine `static`s of `agent_tools.rs` (the active runs, the ones in the background, the unattended ones, who asked for each, the options a command gave each, the tools a search found,
//! the folders a run was asked about, the runs that just ended, and the counter of run ids) are the fields of one [`RunRegistry`]. The one of the process is [`registry`]; the
//! functions of `agent_runs::scope` (`runs_in_flight`, `foreground_run_id`, `is_active`, `origin_of_run`, `options_of`, `ended_run_marks`, `note_found`, `take_found`, `cancel_active`,
//! `new_run_id`, `RunScope`) are thin calls to it (so are `RunScope::begin`, its `Drop` and `around_call`, which stay in `agent_tools`) and keep their names, their arguments and their values. Each table is its own mutex, taken one at a time and never held while another
//! is taken, exactly as the statics were, and a poisoned mutex is used as it is (a panic in a run must not stop the next run from being listed).
//!
//! A registry can be made on its own (`RunRegistry::new`), so that what it keeps can be tested without the process's runs, a lock that serialises the tests, or a host.

use super::RunOptions;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The most tools one run keeps from a search before it uses them.
pub(super) const FOUND_KEEP: usize = 24;
/// How many runs that just ended are remembered.
pub(super) const ENDED_KEEP: usize = 64;
/// How many (run, folder) questions are remembered.
const FOLDERS_KEEP: usize = 64;
/// How many options of runs that are going on are kept before the table is started again.
const OPTIONS_KEEP: usize = 256;

/// What a run that ended held, for the run that goes on from it.
pub(super) struct EndedRun {
    pub(super) id: String,
    /// It had read untrusted text.
    pub(super) tainted: bool,
    /// It held data that is the person's.
    pub(super) private: bool,
    /// The lists of the skills that narrowed it (card R6-16).
    pub(super) restrictions: Vec<Vec<String>>,
}

fn lock<T>(table: &Mutex<T>) -> MutexGuard<'_, T> {
    table.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct RunRegistry {
    /// The runs that are going on now, oldest first (each holds a `RunScope`).
    active: Mutex<Vec<String>>,
    /// The active runs that are in the background (see `queue`).
    background: Mutex<Vec<String>>,
    /// The active runs that nobody is waiting for and that are not in the background queue: a workflow's or flow's agent step, a practice run. A line the person says to the task they wait for
    /// is never theirs.
    unattended: Mutex<Vec<String>>,
    /// Who asked for each of the active runs that somebody marked (`"ui"`: the person's own app, `"api"`: a script with the API token), as `agent_runs::with_origin` said when the run began.
    /// A run nobody marked (the voice orb, a workflow) is not in the table. It is the table of the runs that are going on, and a run leaves it with its scope.
    origins: Mutex<Vec<(String, &'static str)>>,
    /// The options of the runs that are going on, by run id.
    options: Mutex<Vec<(String, RunOptions)>>,
    /// The tools each running agent found by search (run id, names), kept until it uses them: the loop adds them to what it may call.
    found: Mutex<Vec<(String, Vec<String>)>>,
    /// The folders a run was already asked about (run id, folder): one question per folder per run, so that a no is a no and not a question every round.
    folders_asked: Mutex<Vec<(String, String)>>,
    /// What the runs that just ended held, the newest [`ENDED_KEEP`]. A run that goes on after a question is a new run with a new id, so what its parent held has to be handed on, or
    /// answering a question would wash the taint off, and the list of a skill that was loaded would be forgotten with the id it was kept under.
    ended: Mutex<VecDeque<EndedRun>>,
    /// The number of the next run id this process makes.
    seq: AtomicU64,
}

impl RunRegistry {
    pub(super) const fn new() -> Self {
        RunRegistry {
            active: Mutex::new(Vec::new()),
            background: Mutex::new(Vec::new()),
            unattended: Mutex::new(Vec::new()),
            origins: Mutex::new(Vec::new()),
            options: Mutex::new(Vec::new()),
            found: Mutex::new(Vec::new()),
            folders_asked: Mutex::new(Vec::new()),
            ended: Mutex::new(VecDeque::new()),
            seq: AtomicU64::new(1),
        }
    }

    // --- The runs that are going on ---------------------------------------------------------------------------------------------------------------------------------------------

    /// How many agent runs are going on now.
    pub(super) fn runs_in_flight(&self) -> usize {
        lock(&self.active).len()
    }

    /// Is this process working on the run right now?
    pub(super) fn is_active(&self, run_id: &str) -> bool {
        lock(&self.active).iter().any(|x| x == run_id)
    }

    /// A run begins: it is listed as going on, with who asked for it (`origin`, empty when nobody marked it), and as one in the background or one nobody is waiting for.
    pub(crate) fn begin(&self, id: &str, origin: &'static str, in_background: bool, unattended: bool) {
        lock(&self.active).push(id.to_string());
        if !origin.is_empty() {
            lock(&self.origins).push((id.to_string(), origin));
        }
        if in_background {
            lock(&self.background).push(id.to_string());
        }
        if unattended {
            lock(&self.unattended).push(id.to_string());
        }
    }

    /// The options a command gave the run that begins (they stay until it ends).
    pub(crate) fn set_options(&self, id: &str, options: RunOptions) {
        let mut all = lock(&self.options);
        // The table is as long as the runs that are going on; one that never ends cleanly must not make it grow for ever.
        if all.len() >= OPTIONS_KEEP {
            all.clear();
        }
        all.push((id.to_string(), options));
    }

    /// A run ends: it is no longer listed, and what was kept for it (who asked, the tools a search found, the options of a command) goes with it.
    pub(crate) fn end(&self, id: &str) {
        lock(&self.active).retain(|x| x != id);
        lock(&self.background).retain(|x| x != id);
        lock(&self.unattended).retain(|x| x != id);
        lock(&self.origins).retain(|(run, _)| run != id);
        lock(&self.found).retain(|(run, _)| run != id);
        lock(&self.options).retain(|(run, _)| run != id);
    }

    /// The run the person is waiting for: the oldest active one that is neither in the background nor unattended (a step of a workflow that began first is not the
    /// person's task, and a line said to the task would otherwise go to it).
    pub(super) fn foreground_run_id(&self) -> Option<String> {
        let background = lock(&self.background).clone();
        let unattended = lock(&self.unattended).clone();
        lock(&self.active).iter().find(|id| !background.contains(id) && !unattended.contains(id)).cloned()
    }

    /// Calls `cancel` with the id of every active run that is not in the background (the table of the active runs is held while it does, as it was).
    pub(super) fn for_each_not_in_background(&self, mut cancel: impl FnMut(&str)) {
        let background = lock(&self.background).clone();
        for id in lock(&self.active).iter().filter(|id| !background.contains(id)) {
            cancel(id);
        }
    }

    /// Who asked for the run that is going on: `"ui"`, `"api"` or `""` (nobody marked it, or it is not going on).
    pub(super) fn origin_of(&self, run_id: &str) -> &'static str {
        lock(&self.origins).iter().find(|(id, _)| id == run_id).map(|(_, o)| *o).unwrap_or("")
    }

    /// The options of a run a command started, or `None`.
    pub(super) fn options_of(&self, run_id: &str) -> Option<RunOptions> {
        lock(&self.options).iter().find(|(id, _)| id == run_id).map(|(_, o)| o.clone())
    }

    /// The number of the next run id.
    pub(super) fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    // --- The tools a search found -----------------------------------------------------------------------------------------------------------------------------------------------

    /// `tools.search` found these for the run: they are callable from its next round.
    pub(super) fn note_found(&self, run_id: &str, names: &[String]) {
        if run_id.is_empty() || names.is_empty() {
            return;
        }
        let mut found = lock(&self.found);
        let at = match found.iter().position(|(id, _)| id == run_id) {
            Some(i) => i,
            None => {
                found.push((run_id.to_string(), Vec::new()));
                found.len() - 1
            }
        };
        let entry = &mut found[at].1;
        for n in names {
            if !entry.contains(n) && entry.len() < FOUND_KEEP {
                entry.push(n.clone());
            }
        }
    }

    /// The tools found for this run since the last time it asked (each only once).
    pub(super) fn take_found(&self, run_id: &str) -> Vec<String> {
        let mut found = lock(&self.found);
        match found.iter().position(|(id, _)| id == run_id) {
            Some(i) => std::mem::take(&mut found[i].1),
            None => Vec::new(),
        }
    }

    // --- The folders a run was asked about ---------------------------------------------------------------------------------------------------------------------------------------

    /// Notes that the run is asked about `folder`; `false` when it was asked about it already (the folder is compared without regard to case), and nothing is noted. The newest 64 are kept.
    pub(crate) fn ask_folder_once(&self, run_id: &str, folder: &str) -> bool {
        let mut asked = lock(&self.folders_asked);
        if asked.iter().any(|(r, f)| r == run_id && f.eq_ignore_ascii_case(folder)) {
            return false;
        }
        if asked.len() >= FOLDERS_KEEP {
            asked.remove(0);
        }
        asked.push((run_id.to_string(), folder.to_string()));
        true
    }

    // --- The runs that just ended -----------------------------------------------------------------------------------------------------------------------------------------------

    /// A run ended holding this. Only the newest [`ENDED_KEEP`] are kept.
    pub(super) fn note_ended(&self, ended: EndedRun) {
        let mut all = lock(&self.ended);
        all.push_back(ended);
        while all.len() > ENDED_KEEP {
            all.pop_front();
        }
    }

    /// What the run that ended last under this id held: (it had read text from outside, it held data that is the person's), or `None` for a run this registry does not remember.
    pub(super) fn ended_marks(&self, run_id: &str) -> Option<(bool, bool)> {
        lock(&self.ended).iter().rev().find(|e| e.id == run_id).map(|e| (e.tainted, e.private))
    }

    /// What the newest run that ended under this id held, with the lists of skills that narrowed it: (tainted, private, lists).
    pub(crate) fn ended_with_lists(&self, run_id: &str) -> Option<(bool, bool, Vec<Vec<String>>)> {
        lock(&self.ended).iter().rev().find(|e| e.id == run_id).map(|e| (e.tainted, e.private, e.restrictions.clone()))
    }

    /// What a new process would not know: the runs that just ended (tests of a run that goes on from one an earlier process stopped).
    #[cfg(test)]
    pub(super) fn forget_ended(&self) {
        lock(&self.ended).clear();
    }

    /// How many ended runs are remembered.
    #[cfg(test)]
    pub(super) fn ended_len(&self) -> usize {
        lock(&self.ended).len()
    }
}

static REGISTRY: RunRegistry = RunRegistry::new();

/// The registry of this process.
pub(crate) fn registry() -> &'static RunRegistry {
    &REGISTRY
}

#[cfg(test)]
mod tests {
    //! The registry on its own: each test makes one, so none of them sees the runs of another test or of the process, and none needs a lock.

    use super::*;

    fn names(of: &[&str]) -> Vec<String> {
        of.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_run_is_listed_while_it_goes_on_and_forgotten_with_everything_kept_for_it_when_it_ends() {
        let r = RunRegistry::new();
        assert_eq!((r.runs_in_flight(), r.is_active("r1"), r.foreground_run_id(), r.origin_of("r1")), (0, false, None, ""));
        r.begin("r1", "ui", false, false);
        r.set_options("r1", RunOptions { allowed_tools: names(&["fs.read"]), ..Default::default() });
        r.note_found("r1", &names(&["git.status"]));
        assert_eq!((r.runs_in_flight(), r.is_active("r1"), r.foreground_run_id().as_deref(), r.origin_of("r1")), (1, true, Some("r1"), "ui"));
        assert_eq!(r.options_of("r1").unwrap().allowed_tools, names(&["fs.read"]));
        r.end("r1");
        assert_eq!((r.runs_in_flight(), r.is_active("r1"), r.foreground_run_id(), r.origin_of("r1")), (0, false, None, ""));
        assert!(r.options_of("r1").is_none() && r.take_found("r1").is_empty(), "what was kept for the run goes with it");
    }

    #[test]
    fn the_run_the_person_waits_for_is_the_oldest_that_is_neither_in_the_background_nor_unattended() {
        let r = RunRegistry::new();
        r.begin("night", "", true, false);
        r.begin("step", "", false, true);
        assert_eq!(r.foreground_run_id(), None, "a queued job and a step of a flow are nobody's to answer to");
        r.begin("task", "api", false, false);
        r.begin("later", "", false, false);
        assert_eq!(r.foreground_run_id().as_deref(), Some("task"));
        // Stop is for what is being said now: the background run is left going.
        let mut stopped = Vec::new();
        r.for_each_not_in_background(|id| stopped.push(id.to_string()));
        assert_eq!(stopped, names(&["step", "task", "later"]));
        r.end("task");
        assert_eq!(r.foreground_run_id().as_deref(), Some("later"));
        assert_eq!(r.runs_in_flight(), 3);
    }

    #[test]
    fn what_a_search_found_is_given_once_cut_to_the_most_a_run_keeps_and_not_repeated() {
        let r = RunRegistry::new();
        r.note_found("r1", &[]);
        assert!(r.take_found("r1").is_empty());
        r.note_found("r1", &names(&["a", "b", "a"]));
        r.note_found("r1", &names(&["b", "c"]));
        assert_eq!(r.take_found("r1"), names(&["a", "b", "c"]));
        assert!(r.take_found("r1").is_empty(), "each tool is given once");
        let many: Vec<String> = (0..FOUND_KEEP + 10).map(|i| format!("t{i}")).collect();
        r.note_found("r2", &many);
        assert_eq!(r.take_found("r2").len(), FOUND_KEEP);
        assert!(r.take_found("someone else").is_empty());
    }

    #[test]
    fn a_folder_is_asked_about_once_for_each_run_whatever_its_case_and_the_oldest_questions_are_forgotten() {
        let r = RunRegistry::new();
        assert!(r.ask_folder_once("r1", "C:/Work/Docs"));
        assert!(!r.ask_folder_once("r1", "c:/work/docs"), "a no is a no");
        assert!(r.ask_folder_once("r2", "C:/Work/Docs"), "another run is asked again");
        assert!(r.ask_folder_once("r1", "D:/Other"));
        for i in 0..FOLDERS_KEEP {
            assert!(r.ask_folder_once("flood", &format!("F:/{i}")));
        }
        assert!(r.ask_folder_once("r1", "C:/Work/Docs"), "the first question was forgotten when the 64 newest filled the list");
    }

    #[test]
    fn the_options_table_starts_again_when_it_is_full_and_a_run_that_ended_is_remembered_among_the_newest() {
        let r = RunRegistry::new();
        for i in 0..OPTIONS_KEEP {
            r.set_options(&format!("run{i}"), RunOptions { tainted: true, ..Default::default() });
        }
        assert!(r.options_of("run0").is_some());
        r.set_options("one more", RunOptions::default());
        assert!(r.options_of("run0").is_none() && r.options_of("one more").is_some(), "a table that never emptied does not grow for ever");

        r.note_ended(EndedRun { id: "first".into(), tainted: true, private: false, restrictions: vec![names(&["fs.read"])] });
        assert_eq!(r.ended_marks("first"), Some((true, false)));
        assert_eq!(r.ended_with_lists("first").unwrap().2, vec![names(&["fs.read"])]);
        for i in 0..ENDED_KEEP {
            r.note_ended(EndedRun { id: format!("later{i}"), tainted: false, private: true, restrictions: Vec::new() });
        }
        assert_eq!((r.ended_len(), r.ended_marks("first"), r.ended_marks("later0")), (ENDED_KEEP, None, Some((false, true))));
        // Ended twice under one id: the newest says what it held.
        r.note_ended(EndedRun { id: "later1".into(), tainted: true, private: true, restrictions: Vec::new() });
        assert_eq!(r.ended_marks("later1"), Some((true, true)));
        r.forget_ended();
        assert_eq!(r.ended_marks("later1"), None);
    }

    #[test]
    fn run_ids_count_up_from_one_in_each_registry() {
        let (a, b) = (RunRegistry::new(), RunRegistry::new());
        assert_eq!((a.next_seq(), a.next_seq(), b.next_seq(), a.next_seq()), (1, 2, 1, 3));
    }
}

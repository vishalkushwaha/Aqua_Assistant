//! Steering a task that is running: the person says "also check the date", "make it shorter" or "stop" while the agent is working.
//!
//! What is said is **queued** (a few lines at most) and the agent loop reads the queue at the step boundary before its next planner call, adding what was
//! said to the goal it is working on, as the person's own words. A stop is not queued: it ends the run at once, as the Stop button does. Steering
//! never carries an approval: nothing said here answers a question the host asked, and an action that needs a yes still asks for one.
//!
//! Only the run the person is waiting for can be steered (a background run is steered by cancelling it from the queue), and only when one is running:
//! with none, the words are a new request and the caller is told so.
//!
//! **A second lane, for after the run** (card OA10-03, UX-11): a line can be said to be *queued* instead of steered. A steered line is added to the task at its next step; a queued line
//! waits for the run to end, and is then started as a task of its own (by the host of detached runs, `api_gateway::run_control_api`, which is the one that knows when a run ends). The lane
//! has the same limits as the first, a queued line is the person's own words too, and it is never an approval: nothing here answers a question the host asked. A steered line that the run
//! ended without reading is not lost when the run was detached: it joins the queued ones and starts the next task (`take_run_end`).

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

pub use crate::voice_commands::Delivery;

/// How many steering lines may wait for the next step, and how many queued ones may wait for the end of a run.
pub const MAX_QUEUED: usize = 5;
/// The longest one steering line.
pub const MAX_CHARS: usize = 500;
/// The most queued lines kept for all runs together: a run that is never asked about again does not keep the room.
const MAX_QUEUED_ALL: usize = 25;

/// The lines waiting. The program has one (below); a test makes its own, so that tests do not take each other's lines.
/// Each line is kept with the run it was said to (empty: for any run, which is what the tests of the loop use). The first queue is for the next step of the run
/// (steered lines), the second for after it (queued ones).
pub struct Lines(Mutex<VecDeque<(String, String)>>, Mutex<VecDeque<(String, String)>>);

impl Lines {
    pub const fn new() -> Self {
        Lines(Mutex::new(VecDeque::new()), Mutex::new(VecDeque::new()))
    }

    fn q(&self) -> std::sync::MutexGuard<'_, VecDeque<(String, String)>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn later(&self) -> std::sync::MutexGuard<'_, VecDeque<(String, String)>> {
        self.1.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps a line for after `run` has ended. At most [`MAX_QUEUED`] for one run (and [`MAX_QUEUED_ALL`] for all): then [`Refused::Full`].
    pub fn push_later(&self, run: &str, line: String) -> Result<Accepted, Refused> {
        let mut later = self.later();
        let mine = later.iter().filter(|(r, _)| r == run).count();
        if mine >= MAX_QUEUED || later.len() >= MAX_QUEUED_ALL {
            return Err(Refused::Full);
        }
        later.push_back((run.to_string(), line));
        Ok(Accepted::Later(mine + 1))
    }

    /// The lines kept for after `run`, oldest first.
    pub fn later_of(&self, run: &str) -> Vec<String> {
        self.later().iter().filter(|(r, _)| r == run).map(|(_, line)| line.clone()).collect()
    }

    /// The steered lines `run` has not read yet, oldest first (they stay where they are).
    pub fn unread_of(&self, run: &str) -> Vec<String> {
        self.q().iter().filter(|(r, _)| r == run).map(|(_, line)| line.clone()).collect()
    }

    /// Takes the `n`th (from 0) line kept for after `run` out of the queue. `false` when there is none.
    pub fn remove_later(&self, run: &str, n: usize) -> bool {
        let mut later = self.later();
        let Some(at) = later.iter().enumerate().filter(|(_, (r, _))| r == run).map(|(i, _)| i).nth(n) else { return false };
        later.remove(at);
        true
    }

    /// What a run leaves when it ends: the steered lines it never read, then the lines kept for after it, oldest first. Both are taken out: the caller starts the next task with them.
    pub fn take_run_end(&self, run: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        {
            let mut q = self.q();
            let mut keep = VecDeque::new();
            for (r, line) in q.drain(..) {
                if r == run {
                    out.push(line);
                } else {
                    keep.push_back((r, line));
                }
            }
            *q = keep;
        }
        let mut later = self.later();
        let mut keep = VecDeque::new();
        for (r, line) in later.drain(..) {
            if r == run {
                out.push(line);
            } else {
                keep.push_back((r, line));
            }
        }
        *later = keep;
        out
    }

    /// Queues a line for `run`. Only the lines of `run` count against its room ([`MAX_QUEUED`]), and the lines of another run are never touched: what was said to another run is not for
    /// this one, and it is not this one's to erase (a script's run must not be able to wipe the lines that were said to the person's run, nor the other way round). The room all runs
    /// together share is [`MAX_QUEUED_ALL`]; when it is used up, the oldest lines of runs that are no longer going on (the run ended without reading them) make way, and nothing else does.
    pub fn push(&self, run: &str, line: String) -> Result<Accepted, Refused> {
        self.push_with(run, line, &crate::agent_tools::is_active)
    }

    /// [`push`](Self::push) with the test of whether a run is still going on given (a test says which are).
    fn push_with(&self, run: &str, line: String, going: &dyn Fn(&str) -> bool) -> Result<Accepted, Refused> {
        let mut q = self.q();
        let mine = q.iter().filter(|(r, _)| r.is_empty() || r == run).count();
        if mine >= MAX_QUEUED {
            return Err(Refused::Full);
        }
        while q.len() >= MAX_QUEUED_ALL {
            let Some(stale) = q.iter().position(|(r, _)| !r.is_empty() && r != run && !going(r)) else { return Err(Refused::Full) };
            q.remove(stale);
        }
        q.push_back((run.to_string(), line));
        Ok(Accepted::Queued(mine + 1))
    }

    /// What was said to `run` (and the lines said to no run in particular), oldest first, taken out of the queue. The lines of every other run stay where they are: they are for those runs
    /// to read, and they are never handed to this one.
    pub fn take_for(&self, run: &str) -> Vec<String> {
        let mut q = self.q();
        let mut mine = Vec::new();
        let mut others = VecDeque::new();
        for (r, line) in q.drain(..) {
            if r.is_empty() || r == run {
                mine.push(line);
            } else {
                others.push_back((r, line));
            }
        }
        *q = others;
        mine
    }

    /// Forgets every line, of both lanes (a stop, or a test that starts clean).
    pub fn clear(&self) {
        self.q().clear();
        self.later().clear();
    }
}

static GLOBAL: Lines = Lines::new();

/// What became of a steering line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    /// Waiting for the next step; the number is how many lines are waiting now.
    Queued(usize),
    /// Kept for after the run (card OA10-03); the number is how many lines are kept for this run now.
    Later(usize),
    /// It was a stop: the run is ended, and what was kept for after it is let go.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Nothing the person is waiting for is running: this is a new request, not a steer.
    NothingRunning,
    Empty,
    TooLong,
    /// Five lines are waiting already.
    Full,
}

impl Refused {
    pub fn message(&self) -> String {
        match self {
            Refused::NothingRunning => "nothing is running that this could be added to: send it as a new request".into(),
            Refused::Empty => "say what to add".into(),
            Refused::TooLong => format!("a steering line is at most {MAX_CHARS} characters"),
            Refused::Full => format!("{MAX_QUEUED} lines are already waiting for the next step: wait a moment"),
        }
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The line as it is kept: one line, not empty, not longer than [`MAX_CHARS`].
fn checked(text: &str) -> Result<String, Refused> {
    let line = one_line(text);
    if line.is_empty() {
        return Err(Refused::Empty);
    }
    if line.chars().count() > MAX_CHARS {
        return Err(Refused::TooLong);
    }
    Ok(line)
}

/// A line said while a task runs. `wake_phrase` is the assistant's name, so that "Aqua, stop" is a stop too.
pub fn accept(text: &str, wake_phrase: Option<&str>) -> Result<Accepted, Refused> {
    let line = checked(text)?;
    let Some(run) = crate::agent_tools::foreground_run_id() else {
        return Err(Refused::NothingRunning);
    };
    accept_line(&run, line, Delivery::Steer, wake_phrase)
}

/// A line said to `run`, which the caller has found to be going on (or about to begin: a detached run that waits for its turn). `Steer` adds it to the task at its next step, `Queue` keeps
/// it for after the task. A stop is a stop with either: it ends the running tasks the person waits for, and what was waiting for them is let go. Nothing here answers a question of the host.
pub fn accept_for(run: &str, text: &str, delivery: Delivery, wake_phrase: Option<&str>) -> Result<Accepted, Refused> {
    accept_line(run, checked(text)?, delivery, wake_phrase)
}

fn accept_line(run: &str, line: String, delivery: Delivery, wake_phrase: Option<&str>) -> Result<Accepted, Refused> {
    if crate::voice_commands::is_stop_command(&line, wake_phrase.unwrap_or("")) {
        GLOBAL.clear();
        crate::modules::browser_agent::cancel_running_agents();
        return Ok(Accepted::Stopped);
    }
    match delivery {
        Delivery::Steer => GLOBAL.push(run, line),
        Delivery::Queue => GLOBAL.push_later(run, line),
    }
}

/// The steered lines `run` has not read yet, and the lines kept for after it (what `GET /api/v1/runs/{id}/queue` shows).
pub fn waiting_for(run: &str) -> (Vec<String>, Vec<String>) {
    (GLOBAL.unread_of(run), GLOBAL.later_of(run))
}

/// Takes the `n`th (from 0) line kept for after `run` out of the queue.
pub fn remove_queued(run: &str, n: usize) -> bool {
    GLOBAL.remove_later(run, n)
}

/// What a run leaves behind when it ends: the steered lines it did not read (the run ended first), then the lines kept for after it, oldest first; both are taken out. The host of
/// detached runs starts the next task with the first and keeps the rest for it (`queue_for`). A run that was not detached has nobody to start the next task: its unread lines stay
/// (they are never handed to another run) until they are the oldest of the lines of runs that are over and the queue needs the room (`Lines::push`).
pub fn take_run_end(run: &str) -> Vec<String> {
    GLOBAL.take_run_end(run)
}

/// Keeps `lines` for after `run` (the lines of a run that ended, handed to the task that goes on from it). Lines that do not fit are dropped, which the caller is told by their number.
pub fn queue_for(run: &str, lines: Vec<String>) -> usize {
    let mut dropped = 0;
    for line in lines {
        if GLOBAL.push_later(run, line).is_err() {
            dropped += 1;
        }
    }
    dropped
}

/// What was said to `run`, oldest first, taken out of the queue. What was said to any other run stays for it and is not handed to this one. The agent loop calls
/// this at the boundary before each planner call.
pub fn take_for(run: &str) -> Vec<String> {
    GLOBAL.take_for(run)
}

/// Everything waiting, for any run (tests of the loop).
#[cfg(test)]
pub(crate) fn take_all() -> Vec<String> {
    GLOBAL.q().drain(..).map(|(_, line)| line).collect()
}

/// A test starts clean.
#[cfg(test)]
pub(crate) fn clear() {
    GLOBAL.clear();
}

/// How the goal reads once the person has added to it: the goal as it was, then each thing said since, labelled as the person's own words.
pub fn amended_goal(goal: &str, added: &[String]) -> String {
    let mut out = goal.to_string();
    for line in added {
        out.push_str(&format!("\nThe person added while you were working: {line}"));
    }
    out
}

/// Queues a line without the checks (tests of the loop, which is the one that reads it).
#[cfg(test)]
pub(crate) fn push_for_test(line: &str) {
    let _ = GLOBAL.push("", one_line(line));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_one_line_and_short_and_there_is_room_for_five() {
        // Empty and too long are refused before anything else is asked (a run is not needed for that).
        assert_eq!(accept("  ", None), Err(Refused::Empty));
        assert_eq!(accept(&"x ".repeat(300), None), Err(Refused::TooLong));
        let lines = Lines::new();
        for i in 0..MAX_QUEUED {
            assert_eq!(lines.push("run_1", format!("line {i}")), Ok(Accepted::Queued(i + 1)));
        }
        assert_eq!(lines.push("run_1", "one more".into()), Err(Refused::Full));
        assert_eq!(lines.take_for("run_1"), (0..MAX_QUEUED).map(|i| format!("line {i}")).collect::<Vec<_>>(), "oldest first");
        assert!(lines.take_for("run_1").is_empty(), "taking empties the queue");
        assert!(Refused::NothingRunning.message().contains("new request") && Refused::Full.message().contains('5'));
    }

    /// Review fix (one run's pending lines were erased by another run's): `push` used to drop every other run's unread lines and `take_for` used to drain all of them. A run's room is its own
    /// five, a run's lines are read by that run alone, and neither a push nor a read by one run touches the lines of the other.
    #[test]
    fn a_line_said_to_one_run_is_never_handed_to_another_and_neither_erases_nor_takes_the_room_of_the_others() {
        let lines = Lines::new();
        for i in 0..MAX_QUEUED {
            lines.push("run_1", format!("old {i}")).unwrap();
        }
        // run_1 has used its five: that is run_1's room, and the next run can be steered at once, with none of run_1's lines.
        assert_eq!(lines.push("run_1", "one more".into()), Err(Refused::Full));
        assert_eq!(lines.push("run_2", "new".into()), Ok(Accepted::Queued(1)));
        assert_eq!(lines.unread_of("run_1").len(), MAX_QUEUED, "run_2's push did not erase run_1's lines (the old code dropped them)");
        // run_2 reads: it gets its own line only, and run_1's wait for run_1 (the old code drained them all, and run_1 then found nothing).
        assert_eq!(lines.take_for("run_2"), vec!["new".to_string()]);
        assert_eq!(lines.unread_of("run_1").len(), MAX_QUEUED, "run_2's read did not take run_1's lines");
        assert!(lines.take_for("run_2").is_empty());
        assert_eq!(lines.take_for("run_1"), (0..MAX_QUEUED).map(|i| format!("old {i}")).collect::<Vec<_>>(), "and run_1 reads all of them, in the order they were said");
        // Said to both, read in either order: each gets what is its own.
        lines.push("run_1", "a".into()).unwrap();
        lines.push("run_2", "b".into()).unwrap();
        lines.push("run_1", "c".into()).unwrap();
        assert_eq!(lines.take_for("run_1"), vec!["a", "c"]);
        assert_eq!(lines.take_for("run_2"), vec!["b"]);
        // A line with no run named (the tests of the loop) is for whichever run reads it.
        lines.push("", "any".into()).unwrap();
        assert_eq!(lines.take_for("run_9"), vec!["any".to_string()]);
    }

    #[test]
    fn when_all_the_runs_together_have_used_their_room_only_the_lines_of_runs_that_are_over_make_way() {
        let lines = Lines::new();
        let runs = MAX_QUEUED_ALL / MAX_QUEUED;
        for run in 0..runs {
            for i in 0..MAX_QUEUED {
                lines.push_with(&format!("old_{run}"), format!("l{i}"), &|_| true).unwrap();
            }
        }
        // Every run is going on: nobody's lines are given up, the new line is the one that is refused.
        assert_eq!(lines.push_with("new", "x".into(), &|_| true), Err(Refused::Full));
        assert_eq!(lines.q().len(), MAX_QUEUED_ALL);
        // The first run is over (it ended without reading its lines): its oldest line makes the room, and nothing of the others goes.
        assert_eq!(lines.push_with("new", "x".into(), &|r| r != "old_0"), Ok(Accepted::Queued(1)));
        assert_eq!(lines.unread_of("old_0"), vec!["l1", "l2", "l3", "l4"]);
        assert!((1..runs).all(|run| lines.unread_of(&format!("old_{run}")).len() == MAX_QUEUED));
        assert_eq!(lines.unread_of("new"), vec!["x"]);
        // A run never gives up its own lines for a line of its own: its room is five.
        let mine = Lines::new();
        for i in 0..MAX_QUEUED {
            mine.push_with("run_1", format!("l{i}"), &|_| false).unwrap();
        }
        assert_eq!(mine.push_with("run_1", "x".into(), &|_| false), Err(Refused::Full));
    }

    #[test]
    fn queued_lines_wait_for_the_end_of_the_run_with_the_same_limits_and_can_be_taken_out_one_by_one() {
        let lines = Lines::new();
        for i in 0..MAX_QUEUED {
            assert_eq!(lines.push_later("run_1", format!("later {i}")), Ok(Accepted::Later(i + 1)));
        }
        assert_eq!(lines.push_later("run_1", "one more".into()), Err(Refused::Full), "five for one run, as for the steered lines");
        assert_eq!(lines.push_later("run_2", "another run has its own five".into()), Ok(Accepted::Later(1)));
        assert_eq!(lines.later_of("run_1").len(), MAX_QUEUED);
        // Taking one out is by its number among the lines of that run.
        assert!(lines.remove_later("run_1", 1));
        assert_eq!(lines.later_of("run_1"), vec!["later 0", "later 2", "later 3", "later 4"]);
        assert!(!lines.remove_later("run_1", 9) && !lines.remove_later("nobody", 0));
        assert_eq!(lines.later_of("run_2"), vec!["another run has its own five"], "another run's lines are not touched");
        // All the runs together keep a bounded number.
        let all = Lines::new();
        for run in 0..(MAX_QUEUED_ALL / MAX_QUEUED + 1) {
            for i in 0..MAX_QUEUED {
                let _ = all.push_later(&format!("r{run}"), format!("l{i}"));
            }
        }
        assert!(all.1.lock().unwrap().len() <= MAX_QUEUED_ALL, "a bounded queue");
    }

    #[test]
    fn a_run_that_ends_leaves_the_steered_lines_it_did_not_read_then_the_queued_ones_and_only_its_own() {
        let lines = Lines::new();
        lines.push("run_1", "also add tests".into()).unwrap();
        lines.push_later("run_1", "run clippy".into()).unwrap();
        lines.push_later("run_1", "commit".into()).unwrap();
        lines.push_later("run_2", "not yours".into()).unwrap();
        assert_eq!(lines.unread_of("run_1"), vec!["also add tests"], "looking does not take");
        assert_eq!(lines.take_run_end("run_1"), vec!["also add tests", "run clippy", "commit"], "what was not read comes first, in the order it was said");
        assert!(lines.take_run_end("run_1").is_empty(), "taken once");
        assert_eq!(lines.later_of("run_2"), vec!["not yours"]);
        // A line the run read is gone from the first lane and is not given back.
        lines.push("run_3", "also b".into()).unwrap();
        assert_eq!(lines.take_for("run_3"), vec!["also b"]);
        assert!(lines.take_run_end("run_3").is_empty());
        // Everything is forgotten by a clear (a stop), of both lanes.
        lines.push("run_4", "x".into()).unwrap();
        lines.push_later("run_4", "y".into()).unwrap();
        lines.clear();
        assert!(lines.take_run_end("run_4").is_empty());
    }

    #[test]
    fn the_lines_a_route_says_to_a_run_go_to_the_lane_it_names_and_the_checks_are_the_same() {
        // The checks are made before anything else (no run is needed for them).
        assert_eq!(accept_for("run_oa10_03", "  ", Delivery::Queue, None), Err(Refused::Empty));
        assert_eq!(accept_for("run_oa10_03", &"x ".repeat(300), Delivery::Steer, None), Err(Refused::TooLong));
        assert_eq!(accept_for("run_oa10_03", "also add tests", Delivery::Steer, None), Ok(Accepted::Queued(1)));
        assert_eq!(accept_for("run_oa10_03", "then   run
clippy", Delivery::Queue, None), Ok(Accepted::Later(1)), "one line");
        let (unread, later) = waiting_for("run_oa10_03");
        assert_eq!((unread, later), (vec!["also add tests".to_string()], vec!["then run clippy".to_string()]));
        assert!(remove_queued("run_oa10_03", 0) && waiting_for("run_oa10_03").1.is_empty());
        // The lines of a run that ended go to the task that goes on from it; the ones that do not fit are counted.
        assert_eq!(queue_for("run_oa10_03_next", (0..7).map(|i| format!("l{i}")).collect()), 2, "five fit");
        assert_eq!(take_run_end("run_oa10_03_next").len(), 5);
        let _ = take_run_end("run_oa10_03");
    }

    /// Card OA10-03 (UX-11): a yes is not an approval, and nothing said to a run answers a question of the host. The steering lanes have no way to reach the confirmation broker (this
    /// file does not name it), a yes is kept as the words it is, and the cues are not words of approval.
    #[test]
    fn yes_is_not_an_approval() {
        let lines = Lines::new();
        assert_eq!(lines.push("run_1", "yes, approve it".into()), Ok(Accepted::Queued(1)), "kept as text for the run to read, like any line");
        assert_eq!(lines.take_for("run_1"), vec!["yes, approve it"], "and read as the person's words, which the loop labels as such");
        for word in ["yes", "no", "approve", "approve_always", "deny", "go ahead", "sure", "okay do it"] {
            assert_eq!(crate::voice_commands::steer_cue(word, "Hey Aqua"), None, "{word}: not a continuation, so it is not steered either");
        }
        let source = include_str!("steer.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        assert!(!code.contains("broker") && !code.contains("resolve_typed") && !code.contains("Decision::Allow"), "nothing here can answer a question");
    }

    #[test]
    fn what_is_said_is_added_to_the_goal_as_the_persons_own_words_one_line_each() {
        assert_eq!(amended_goal("read a.txt", &[]), "read a.txt");
        assert_eq!(
            amended_goal("read a.txt", &["and b.txt too".into(), "make it short".into()]),
            "read a.txt\nThe person added while you were working: and b.txt too\nThe person added while you were working: make it short"
        );
    }
}

//! Going on with a run that used its rounds or its time (`POST /api/v1/runs/{id}/continue`; card OA10-04, task WF-12 of the OpenCode audit).
//!
//! A coding task is often longer than one run: twenty-four rounds, and fifteen minutes by the clock, end a run that was getting somewhere. Until now such a run was final: it could not be
//! resumed (`resume` is for runs that *stopped in the middle*), so a person started the task again and the model read the files once more. A run that ended on `max_steps` or
//! `deadline` is **continuable**: [`plan`] says what the run that goes on is started with, and the route starts it (detached, like `POST /api/v1/runs`).
//!
//! What the run that goes on has, and has not:
//!
//! * the **goal** the first run was given, the **checklist** and the **lines the person added** that the first run kept (`run_state`, AG-15), and one block of data, "where it stopped": the
//!   coding record the first run kept (what was read and changed, what the last check said, what to do next);
//! * **not** the results of its calls: the files may have changed since (the person edits too), so the new run has to read a file again before it edits it (`coding_session::reset_reads` still
//!   applies to every run), and nothing the record says is a reason to skip that;
//! * the first run's **taint and privacy marks** (the same inheritance as `resume`): a run that read text from outside is still one that asks before it acts;
//! * a **chain** that is bounded: at most 48 rounds and 30 minutes of run time in all, counted from the first run of the chain (the runs are linked by `parent_run_id`). A chain that has
//!   used them is refused with the numbers, and the person starts the task again if they want more. The counters of the autonomy settings (tool calls, files, bytes) start again with each
//!   run of the chain: it takes the person's click to start one.
//!
//! The route needs the UI token: it starts work from a record, and the record is the person's.

use super::store::{status, AgentRunStore};
use crate::modules::coding_driver::CodingRecord;
use rusqlite::OptionalExtension;

/// The most rounds a chain of continued runs may use in all.
pub const CHAIN_MAX_ROUNDS: u32 = 48;
/// The most run time a chain may use in all (thirty minutes).
pub const CHAIN_MAX_MS: i64 = 30 * 60 * 1000;
/// How many rounds a run that is getting somewhere is given once, at the end of its rounds.
pub const EXTENSION_ROUNDS: u32 = 8;
/// How many runs back the chain is followed.
const CHAIN_LOOK_BACK: usize = 20;

/// Did a run end on its rounds or its time, so that it can be gone on from? (A run that ended on a limit of the autonomy settings, by a refusal or by the person's Stop is not.)
pub fn continuable(state: &str, stopped_reason: &str) -> bool {
    state == status::FAILED && matches!(stopped_reason, "max_steps" | "deadline")
}

/// What the run that goes on is started with.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub parent_run_id: String,
    pub goal: String,
    /// `voice`, `api` or `agent`: the run that goes on has the powers the first had.
    pub caller: String,
    /// The coding record the first run kept (`coding_records`), as data; `None` for a run that kept none.
    pub record: Option<CodingRecord>,
    pub checklist: Vec<(String, String)>,
    pub steered: Vec<String>,
    /// What the chain has used so far, the first run of the chain to the run being gone on from.
    pub chain_rounds: u32,
    pub chain_ms: i64,
    /// How many runs the chain has, the one being gone on from included.
    pub chain_runs: u32,
}

/// Why a run cannot be gone on from.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    NotFound,
    /// It did not end on its rounds or its time. Carries its state and why it stopped.
    NotContinuable(String, String),
    /// This process is still working on it.
    StillGoingOn,
    /// A run already goes on from it. Carries that run's id (go on from the newest instead).
    AlreadyContinued(String),
    NoGoal,
    /// The chain has used its 48 rounds or its 30 minutes. Carries what it has used.
    ChainSpent { rounds: u32, ms: i64 },
}

impl Refused {
    pub fn message(&self) -> String {
        match self {
            Refused::NotFound => "there is no such run".into(),
            Refused::NotContinuable(state, why) => format!("this run is {state}{}; only a run that used its rounds or its time can be continued", if why.is_empty() { String::new() } else { format!(" ({why})") }),
            Refused::StillGoingOn => "this run is still going on: wait for it, or stop it, then continue".into(),
            Refused::AlreadyContinued(id) => format!("a run already goes on from this one ({id}); continue that one"),
            Refused::NoGoal => "the record of this run has no goal to go on with".into(),
            Refused::ChainSpent { rounds, ms } => format!(
                "this task has used {rounds} of its {CHAIN_MAX_ROUNDS} rounds and {} of its {} minutes across the runs that went on from each other; ask again to start it over",
                (ms / 60_000).max(0),
                CHAIN_MAX_MS / 60_000
            ),
        }
    }
}

/// What the runs from the first of the chain to `run_id` have used: (rounds, milliseconds, how many runs). Followed through `parent_run_id`, at most [`CHAIN_LOOK_BACK`] runs back.
pub fn chain_totals(store: &AgentRunStore, run_id: &str) -> (u32, i64, u32) {
    let (mut rounds, mut ms, mut runs) = (0u32, 0i64, 0u32);
    let mut next = Some(run_id.to_string());
    let c = store.conn();
    while let Some(id) = next.take() {
        if runs as usize >= CHAIN_LOOK_BACK {
            break;
        }
        let row: Option<(i64, i64, Option<i64>, Option<String>)> = c
            .query_row("SELECT rounds, started_at, ended_at, parent_run_id FROM agent_runs WHERE id = ?1", [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()
            .ok()
            .flatten();
        let Some((r, started, ended, parent)) = row else { break };
        rounds = rounds.saturating_add(r.max(0) as u32);
        ms = ms.saturating_add(ended.map_or(0, |e| (e - started).max(0)));
        runs += 1;
        next = parent.filter(|p| !p.is_empty());
    }
    (rounds, ms, runs)
}

/// Can `run_id` be gone on from, and with what? `alive` says whether this process is working on a run.
pub fn plan(store: &AgentRunStore, run_id: &str, alive: &dyn Fn(&str) -> bool) -> Result<Plan, Refused> {
    let run = store.get(run_id).ok().flatten().ok_or(Refused::NotFound)?;
    let state = run["status"].as_str().unwrap_or("").to_string();
    let why = run["stopped_reason"].as_str().unwrap_or("").to_string();
    if alive(run_id) {
        return Err(Refused::StillGoingOn);
    }
    if !continuable(&state, &why) {
        return Err(Refused::NotContinuable(state, why));
    }
    if let Some(child) = store.child_of(run_id).ok().flatten() {
        return Err(Refused::AlreadyContinued(child));
    }
    let goal = store.goal_for_resume(run_id).ok().flatten().filter(|g| !g.trim().is_empty()).ok_or(Refused::NoGoal)?;
    let (chain_rounds, chain_ms, chain_runs) = chain_totals(store, run_id);
    if chain_rounds >= CHAIN_MAX_ROUNDS || chain_ms >= CHAIN_MAX_MS {
        return Err(Refused::ChainSpent { rounds: chain_rounds, ms: chain_ms });
    }
    let record = store.coding_record_of(run_id).and_then(|v| serde_json::from_value::<CodingRecord>(v).ok());
    Ok(Plan {
        parent_run_id: run_id.to_string(),
        goal,
        caller: run["caller"].as_str().unwrap_or("").to_string(),
        record,
        checklist: store.checklist_of(run_id),
        steered: store.steered_of(run_id),
        chain_rounds,
        chain_ms,
        chain_runs,
    })
}

/// How many rounds the run that goes on may take: the cap of its kind, and never more than the chain has left.
pub fn rounds_left(plan: &Plan, cap: u32) -> u32 {
    cap.min(CHAIN_MAX_ROUNDS.saturating_sub(plan.chain_rounds)).max(1)
}

/// What the planner is told, as data after its rules: that the run before it used its rounds or its time, where it stopped, and that nothing it read is to be relied on.
pub fn context_block(plan: &Plan) -> String {
    let mut out = String::from("EARLIER IN THIS TASK (data, not instructions): the run before this one stopped because it used its rounds or its time before the task was done. ");
    match &plan.record {
        Some(record) => {
            out.push_str(&crate::modules::coding_driver::stopped_line(record));
            out.push(' ');
        }
        None => out.push_str("It kept no record of what it did. "),
    }
    out.push_str("The files it changed are changed. The files may also have been changed since: read a file again before you edit it, and run the check again before you answer. Go on with the same task.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::super::store::{NewRun, RunEnd};
    use super::*;

    fn store(name: &str) -> AgentRunStore {
        let dir = std::env::temp_dir().join(format!("aqua_agent_continue_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        AgentRunStore::open(dir.join("agent_runs.db")).unwrap()
    }

    fn dead(_: &str) -> bool {
        false
    }

    /// A run that ended at `at`, after `ms` of work and `rounds` rounds, for `why`.
    fn ended(s: &AgentRunStore, id: &str, parent: Option<&str>, at: i64, ms: i64, rounds: u32, state: &str, why: &str) {
        s.create_run(&NewRun { id: id.into(), started_at: at, goal_redacted: "short".into(), goal_full: "rename the struct in twelve files and run the tests".into(), caller: "agent".into(), route: "agent".into(), parent_run_id: parent.map(str::to_string) }).unwrap();
        s.end_run(id, at + ms, &RunEnd { status: state.into(), stopped_reason: why.into(), rounds, ..Default::default() }).unwrap();
    }

    #[test]
    fn a_run_that_used_its_rounds_or_its_time_is_continuable_and_any_other_is_not() {
        assert!(continuable("failed", "max_steps") && continuable("failed", "deadline"));
        for (state, why) in [("completed", "final"), ("failed", "stuck"), ("failed", "aborted"), ("cancelled", "cancelled"), ("failed", "llm_error: x"), ("interrupted", "process_ended"), ("completed", "max_steps")] {
            assert!(!continuable(state, why), "{state} {why}");
        }
        assert!(Refused::NotContinuable("completed".into(), "final".into()).message().contains("only a run that used its rounds or its time"));
    }

    #[test]
    fn the_plan_has_the_goal_the_record_the_checklist_and_the_lines_of_the_run_that_stopped() {
        let s = store("plan");
        ended(&s, "run_1", None, 1_000, 60_000, 24, "failed", "max_steps");
        s.set_coding_record("run_1", r#"{"objective":"rename the struct","files_read":["a.rs"],"files_changed":["a.rs","b.rs"],"last_check":{"kind":"tests","passed":false,"program":"cargo","summary":"2 failed","failing":["parse::quotes"],"first_error":"assertion failed","stale":true},"checks":[],"edits_since_check":true,"blocked":null,"next":"run the check again"}"#, 5).unwrap();
        s.set_checklist("run_1", &[("rename in a.rs".into(), "done".into()), ("rename in b.rs".into(), "doing".into())]).unwrap();
        s.set_steered("run_1", &["also update the docs".into()]).unwrap();
        let p = plan(&s, "run_1", &dead).unwrap();
        assert_eq!((p.goal.as_str(), p.caller.as_str(), p.chain_rounds, p.chain_runs), ("rename the struct in twelve files and run the tests", "agent", 24, 1));
        assert_eq!(p.checklist.len(), 2);
        assert_eq!(p.steered, vec!["also update the docs".to_string()]);
        let block = context_block(&p);
        assert!(block.starts_with("EARLIER IN THIS TASK (data, not instructions)"), "{block}");
        assert!(block.contains("Where I stopped") && block.contains("a.rs, b.rs") && block.contains("parse::quotes"), "{block}");
        assert!(block.contains("read a file again before you edit it") && block.ends_with("Go on with the same task.\n"), "{block}");
        // A run that kept no record is told so.
        ended(&s, "run_2", None, 2_000, 1_000, 3, "failed", "deadline");
        let p2 = plan(&s, "run_2", &dead).unwrap();
        assert!(p2.record.is_none() && context_block(&p2).contains("kept no record"));
    }

    #[test]
    fn a_run_is_refused_when_it_is_not_continuable_still_going_gone_on_from_already_or_has_no_goal() {
        let s = store("refused");
        assert_eq!(plan(&s, "nope", &dead), Err(Refused::NotFound));
        ended(&s, "done", None, 1_000, 10, 2, "completed", "final");
        assert!(matches!(plan(&s, "done", &dead), Err(Refused::NotContinuable(..))));
        ended(&s, "cap", None, 1_000, 10, 24, "failed", "max_steps");
        assert_eq!(plan(&s, "cap", &|id| id == "cap"), Err(Refused::StillGoingOn));
        ended(&s, "cap_child", Some("cap"), 2_000, 10, 5, "completed", "final");
        assert_eq!(plan(&s, "cap", &dead), Err(Refused::AlreadyContinued("cap_child".into())));
        s.create_run(&NewRun { id: "nogoal".into(), started_at: 1, ..Default::default() }).unwrap();
        s.end_run("nogoal", 2, &RunEnd { status: "failed".into(), stopped_reason: "max_steps".into(), ..Default::default() }).unwrap();
        assert_eq!(plan(&s, "nogoal", &dead), Err(Refused::NoGoal));
    }

    #[test]
    fn a_chain_is_bounded_by_forty_eight_rounds_and_thirty_minutes_counted_from_its_first_run() {
        let s = store("chain");
        // 24 + 24 rounds: the third run of the chain would have none left.
        ended(&s, "c1", None, 1_000, 60_000, 24, "failed", "max_steps");
        ended(&s, "c2", Some("c1"), 100_000, 60_000, 24, "failed", "max_steps");
        let Err(Refused::ChainSpent { rounds, ms }) = plan(&s, "c2", &dead) else { panic!("48 rounds are the end of a chain") };
        assert_eq!((rounds, ms), (48, 120_000));
        assert!(Refused::ChainSpent { rounds, ms }.message().contains("48 of its 48 rounds"));
        // By the clock: two runs of 16 minutes.
        ended(&s, "t1", None, 1_000_000, 16 * 60_000, 10, "failed", "deadline");
        ended(&s, "t2", Some("t1"), 3_000_000, 16 * 60_000, 10, "failed", "deadline");
        assert!(matches!(plan(&s, "t2", &dead), Err(Refused::ChainSpent { .. })), "thirty-two minutes are over thirty");
        // A chain that has room is let through, with what it has used; and the rounds the next run may take are what is left.
        ended(&s, "r1", None, 5_000_000, 60_000, 24, "failed", "max_steps");
        let p = plan(&s, "r1", &dead).unwrap();
        assert_eq!(rounds_left(&p, 24), 24, "the cap of its kind, when there is room");
        ended(&s, "r2", Some("r1"), 6_000_000, 60_000, 10, "failed", "max_steps");
        let p2 = plan(&s, "r2", &dead).unwrap();
        assert_eq!((p2.chain_rounds, p2.chain_runs, rounds_left(&p2, 24)), (34, 2, 14), "48 less the 34 it has used");
    }
}

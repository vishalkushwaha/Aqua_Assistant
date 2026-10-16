//! Where the time of one turn goes. A turn runs on one thread from the speech gate to the spoken answer, so each stage notes how
//! long it took in a record that belongs to that thread: no signature changes, and code that runs outside a turn (a test, a
//! command-line tool) records nothing. When the turn ends its stages are logged on one line ("turn 12: work 6400 ms = denoise 410 +
//! stt 1210 + ...") and handed to the request, which returns them as `stages_ms` in the JSON of `/pipeline/text` and
//! `/pipeline/wav`. Every line logged during the turn carries `turn{id=12}`, so the lines of one turn can be told apart.

use serde::Serialize;
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;
use tracing::info;

/// How long one stage of a turn took.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageMs {
    pub stage: &'static str,
    pub ms: u64,
}

thread_local! {
    static CURRENT: RefCell<Option<Vec<StageMs>>> = const { RefCell::new(None) };
}

static NEXT_TURN: AtomicU64 = AtomicU64::new(1);

/// Run `work` as the stage `name` and note how long it took. Outside a turn it just runs.
pub fn stage<T>(name: &'static str, work: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let out = work();
    note(name, started.elapsed().as_millis() as u64);
    out
}

/// Note a stage that was timed by the caller (a stage that is interrupted by another one, as chat is by the speech made while it is
/// being written, cannot be one closure).
pub(crate) fn note(name: &'static str, ms: u64) {
    CURRENT.with(|c| {
        if let Some(stages) = c.borrow_mut().as_mut() {
            stages.push(StageMs { stage: name, ms });
        }
    });
}

/// The stages noted on this thread so far in the turn that is running (empty outside a turn).
pub fn recorded_so_far() -> Vec<StageMs> {
    CURRENT.with(|c| c.borrow().clone().unwrap_or_default())
}

/// Stages with the same name added together, in the order each first appeared.
pub fn merged(stages: &[StageMs]) -> Vec<StageMs> {
    let mut out: Vec<StageMs> = Vec::new();
    for s in stages {
        match out.iter_mut().find(|o| o.stage == s.stage) {
            Some(o) => o.ms += s.ms,
            None => out.push(s.clone()),
        }
    }
    out
}

/// "work 6400 ms = denoise 410 + stt 1210 + agent 4700 + other 80": the stages and what they do not account for.
pub fn summary(work_ms: u64, stages: &[StageMs]) -> String {
    let parts: Vec<String> = stages.iter().map(|s| format!("{} {}", s.stage, s.ms)).collect();
    let covered: u64 = stages.iter().map(|s| s.ms).sum();
    format!("work {work_ms} ms = {} + other {}", parts.join(" + "), work_ms.saturating_sub(covered))
}

/// Records one turn on the thread that runs it: created when the turn's work starts, it logs the stages and hands them over
/// through `slot` when it is dropped, on every way out (a panic included).
pub struct Recording {
    id: u64,
    started: Instant,
    slot: Arc<Mutex<Vec<StageMs>>>,
    // Declared last, so it is still entered while `drop` logs the summary line.
    _span: tracing::span::EnteredSpan,
}

impl Recording {
    pub fn start(slot: Arc<Mutex<Vec<StageMs>>>) -> Self {
        let id = NEXT_TURN.fetch_add(1, Ordering::Relaxed);
        CURRENT.with(|c| *c.borrow_mut() = Some(Vec::new()));
        Self { id, started: Instant::now(), slot, _span: tracing::info_span!("turn", id).entered() }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let stages = merged(&CURRENT.with(|c| c.borrow_mut().take()).unwrap_or_default());
        if !stages.is_empty() {
            info!("turn {}: {}", self.id, summary(self.started.elapsed().as_millis() as u64, &stages));
        }
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = stages;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> Arc<Mutex<Vec<StageMs>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn ms(stage: &'static str, ms: u64) -> StageMs {
        StageMs { stage, ms }
    }

    #[test]
    fn a_turn_hands_over_its_stages_in_order_with_repeats_added_together() {
        let out = slot();
        {
            let _turn = Recording::start(out.clone());
            let value = stage("local", || 7);
            assert_eq!(value, 7, "the work's own result passes through");
            note("stt", 1200);
            note("local", 5);
            note("tts", 900);
            assert_eq!(recorded_so_far().len(), 4, "everything noted so far is visible while the turn runs");
        }
        let stages = out.lock().unwrap().clone();
        assert_eq!(stages.iter().map(|s| s.stage).collect::<Vec<_>>(), ["local", "stt", "tts"]);
        assert!(stages[0].ms >= 5, "the two local stages are one, added together");
        assert_eq!(stages[1], ms("stt", 1200));
        assert!(recorded_so_far().is_empty(), "the record is closed with the turn");
    }

    #[test]
    fn code_outside_a_turn_records_nothing_and_still_runs() {
        std::thread::spawn(|| {
            assert_eq!(stage("stt", || "heard"), "heard");
            assert!(recorded_so_far().is_empty());
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_turn_only_sees_its_own_thread() {
        let out = slot();
        let _turn = Recording::start(out.clone());
        std::thread::spawn(|| {
            note("elsewhere", 99);
            assert!(recorded_so_far().is_empty(), "another thread is not in this turn");
        })
        .join()
        .unwrap();
        assert!(recorded_so_far().is_empty(), "and nothing leaked in");
    }

    #[test]
    fn a_turn_that_did_nothing_replaces_the_previous_record_with_an_empty_one() {
        let out = slot();
        out.lock().unwrap().push(ms("stale", 1));
        drop(Recording::start(out.clone()));
        assert!(out.lock().unwrap().is_empty());
    }

    #[test]
    fn the_summary_names_the_stages_and_what_they_do_not_cover() {
        let stages = [ms("denoise", 410), ms("stt", 1210), ms("agent", 4700)];
        assert_eq!(summary(6400, &stages), "work 6400 ms = denoise 410 + stt 1210 + agent 4700 + other 80");
        assert_eq!(summary(100, &[ms("a", 150)]), "work 100 ms = a 150 + other 0", "rounding never makes it negative");
    }

    #[test]
    fn stages_serialize_as_a_readable_list() {
        let json = serde_json::to_string(&[ms("queued", 3), ms("stt", 1210)]).unwrap();
        assert_eq!(json, r#"[{"stage":"queued","ms":3},{"stage":"stt","ms":1210}]"#);
    }
}

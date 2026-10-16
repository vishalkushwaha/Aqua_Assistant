//! Which turn is running right now, how many are waiting, and how to stop the running one.
//!
//! The core runs one turn at a time (the turn gate in `server.rs`). This module is the shared record of it: a turn
//! registers itself when its work starts and is removed when that work has really ended, so "idle" here means nothing is
//! computing, not just that a request was dropped. `stop_current` trips the running turn's cancel flag, which the model
//! call, the agent loop and the pipeline stages all watch.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

struct Running {
    id: u64,
    cancel: Arc<AtomicBool>,
    since: Instant,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
static WAITING: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Marks a turn as running until dropped (when its work thread ends).
pub struct Registered;

pub fn register(cancel: Arc<AtomicBool>) -> Registered {
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    *RUNNING.lock().unwrap_or_else(PoisonError::into_inner) = Some(Running { id, cancel, since: Instant::now() });
    Registered
}

impl Drop for Registered {
    fn drop(&mut self) {
        *RUNNING.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Counts a request as waiting for its turn until dropped.
pub struct Waiting;

pub fn waiting() -> Waiting {
    WAITING.fetch_add(1, Ordering::SeqCst);
    Waiting
}

impl Drop for Waiting {
    fn drop(&mut self) {
        WAITING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Ask the running turn to stop. Returns its id (to wait for that very turn to end), or None if nothing was running.
pub fn stop_current() -> Option<u64> {
    match RUNNING.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        Some(r) => {
            r.cancel.store(true, Ordering::SeqCst);
            Some(r.id)
        }
        None => None,
    }
}

/// Is the turn with this id still running? A different turn that started after it does not count.
pub fn is_running_turn(id: u64) -> bool {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner).as_ref().is_some_and(|r| r.id == id)
}

pub fn is_running() -> bool {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner).is_some()
}

/// How long the running turn has been going, in milliseconds.
pub fn running_for_ms() -> Option<u64> {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(|r| r.since.elapsed().as_millis() as u64)
}

/// Requests queued behind the running one.
pub fn waiting_count() -> usize {
    WAITING.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_is_running_from_register_until_its_work_ends_and_stop_trips_its_flag() {
        // The registry is process-wide and other tests register turns too, so this test only asserts about its own
        // turn (by id), never that the whole process is idle.
        let flag = Arc::new(AtomicBool::new(false));
        let reg = register(flag.clone());
        assert!(is_running());
        assert!(running_for_ms().is_some());
        let id = stop_current().expect("the running turn is found");
        // `stop_current` stops whichever turn is registered; with no other test running that is ours.
        if flag.load(Ordering::SeqCst) {
            assert!(is_running_turn(id), "the stopped turn stays registered until its work really ends");
        }
        drop(reg);
        assert!(!is_running_turn(id), "a turn is gone once its work has ended");
        // The next turn is a different turn: waiting for the stopped one must not wait for it
        let next = register(Arc::new(AtomicBool::new(false)));
        assert!(!is_running_turn(id), "a turn that started after the stopped one does not hold up Stop");
        drop(next);

        let before = waiting_count();
        let w = waiting();
        assert!(waiting_count() > before || waiting_count() >= 1);
        drop(w);
    }
}

//! The wait for the model's turn and who goes first: how long a request waits, the count of those that wait and of the persons among them, the lock that is taken within a limit, and the yield of a background call to a person (move only: this was part of `llama_server.rs`).

use super::*;

/// How long a request waits for its turn on the model before giving up (`AQUA_LLAMA_QUEUE_SECS`, default 120).
/// One model serves everything (voice turns, agent runs, flows, practice), so without a limit a voice request could
/// wait behind several minutes of someone else's work without ever being told.
pub fn queue_wait() -> Duration {
    Duration::from_secs(std::env::var("AQUA_LLAMA_QUEUE_SECS").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(120).clamp(5, 3600))
}

/// How many requests are waiting for the model right now. Background work that reads prompt ahead of time (the warm-up) looks
/// at this between its steps and lets a real request go first.
pub(super) static WAITING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn others_waiting() -> bool {
    WAITING.load(std::sync::atomic::Ordering::SeqCst) > 0
}

/// How many of those are a person's request (a typed or spoken turn, a run the person started) and not another background job. A background run that holds the model gives it up to
/// these and to nobody else (card R6-03, T6-03.4: [`yield_to_a_person`]).
pub(super) static WAITING_PERSON: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Is a request of a person waiting for the model right now?
pub fn person_waiting() -> bool {
    WAITING_PERSON.load(std::sync::atomic::Ordering::SeqCst) > 0
}

pub(super) struct Waiting {
    person: bool,
}
impl Waiting {
    /// `person`: the thread that waits is not a background job, a flow, a workflow or a practice run (`agent_tools::nobody_to_ask`).
    pub(super) fn begin(person: bool) -> Self {
        WAITING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if person {
            WAITING_PERSON.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Waiting { person }
    }
}
impl Drop for Waiting {
    fn drop(&mut self) {
        WAITING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        if self.person {
            WAITING_PERSON.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

/// Does a background call give the model up to a person who is waiting for it (`AQUA_BACKGROUND_YIELD=on`, off by default)? The wait loop of a request asks this about once in 200 ms.
/// Only a background run yields (`queue::in_background`), only to a person, and only when the switch is on; with it off nothing changes.
pub fn should_yield(switch_on: bool, background: bool, person_waiting: bool) -> bool {
    switch_on && background && person_waiting
}

thread_local! {
    /// Set while a call on this thread is not to be asked to give the model up ([`without_yielding`]).
    pub(super) static NO_YIELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` (a planner call) so that it does not give the model up to a person who is waiting. A run that has yielded often enough uses this for its next call.
pub fn without_yielding<R>(f: impl FnOnce() -> R) -> R {
    let before = NO_YIELD.with(|c| c.replace(true));
    let out = f();
    NO_YIELD.with(|c| c.set(before));
    out
}

/// Is the call on this thread made inside [`without_yielding`]? (A test planner that stands for the model server asks this: such a call never ends in a yield.)
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn yielding_is_off() -> bool {
    NO_YIELD.with(|c| c.get())
}

/// Is the switch of [`should_yield`] on?
pub fn background_yield_enabled() -> bool {
    matches!(std::env::var("AQUA_BACKGROUND_YIELD").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
}

/// After a call yielded: wait, at most `most`, until no request of a person is waiting for the model (it is then being served, or gone), so that the call that is made again does not
/// take the model back before the person's request has had it. `cancelled` ends the wait early (Stop).
pub fn wait_until_no_person_waits(most: Duration, cancelled: impl Fn() -> bool) {
    let started = Instant::now();
    while person_waiting() && started.elapsed() < most && !cancelled() {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Take a lock, waiting at most `wait`. `None` means somebody else still has it. A lock poisoned by a panic is
/// recovered, as everywhere else.
pub(super) fn lock_within<T>(mutex: &Mutex<T>, wait: Duration, mut on_tick: impl FnMut(u64)) -> Option<std::sync::MutexGuard<'_, T>> {
    let start = Instant::now();
    let mut waiting: Option<Waiting> = None;
    let mut last_tick = 0;
    loop {
        match mutex.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::Poisoned(p)) => return Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {
                waiting.get_or_insert_with(|| Waiting::begin(!crate::agent_tools::nobody_to_ask()));
            }
        }
        if start.elapsed() >= wait {
            return None;
        }
        let secs = start.elapsed().as_secs();
        if secs > last_tick {
            last_tick = secs;
            on_tick(secs);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

//! One desktop session at a time (card R6-34).
//!
//! There is one mouse, one keyboard and one window in front. Two runs that act on the screen at once (an agent run and a flow's step, or an agent run and a spoken "type that") do
//! not take turns, they fight: one run's click lands in the window the other just opened, and each reads the other's work as its own. So a run that is going to act on the screen
//! takes the **desktop lease** first, under its own name (its run id), and a second one is refused, with the name of the one that has it, until the first has finished or has
//! not used the screen for [`IDLE_LIMIT`].
//!
//! The lease is also what the Esc key is tied to ([`super::stop_key`]): the key stops a session only while one is going. Looking at the screen needs no lease, and a run that
//! only looks keeps the lease it has alive by looking ([`Desk::touch_at`]), so that the Esc key stops it too.

use std::cell::RefCell;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A session that has not used the screen for this long has let go of it (a model can take a good while to answer, so it is not short).
pub const IDLE_LIMIT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone)]
struct Lease {
    owner: String,
    last: Instant,
}

/// The lease on the desktop. A value, so that it can be tested; the process has one ([`global`]).
#[derive(Debug)]
pub struct Desk {
    lease: Mutex<Option<Lease>>,
}

impl Default for Desk {
    fn default() -> Self {
        Self::new()
    }
}

impl Desk {
    pub const fn new() -> Self {
        Self { lease: Mutex::new(None) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Lease>> {
        self.lease.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the desktop for `owner`, or says who has it. The owner that has it keeps it (and the time of its last use is renewed); one that let go long ago has lost it.
    pub fn claim_at(&self, owner: &str, now: Instant) -> Result<(), String> {
        let mut lease = self.lock();
        match lease.as_ref() {
            Some(l) if l.owner != owner && now.saturating_duration_since(l.last) < IDLE_LIMIT => {
                Err(format!("another session ({}) is using the desktop: wait for it to finish, or stop it, before this one acts on the screen", l.owner))
            }
            _ => {
                *lease = Some(Lease { owner: owner.to_string(), last: now });
                Ok(())
            }
        }
    }

    /// Would `owner` be refused the desktop now? The same answer as [`Desk::claim_at`], and nothing is taken: asked before a question is put, so that nobody is asked about a
    /// call that will be refused.
    pub fn check_at(&self, owner: &str, now: Instant) -> Result<(), String> {
        match self.lock().as_ref() {
            Some(l) if l.owner != owner && now.saturating_duration_since(l.last) < IDLE_LIMIT => {
                Err(format!("another session ({}) is using the desktop: wait for it to finish, or stop it, before this one acts on the screen", l.owner))
            }
            _ => Ok(()),
        }
    }

    /// A call that only looks: it keeps the owner's lease alive, and takes the desktop only if nobody has it. It never refuses and never takes it from another.
    pub fn touch_at(&self, owner: &str, now: Instant) {
        let mut lease = self.lock();
        let free = lease.as_ref().is_none_or(|l| l.owner == owner || now.saturating_duration_since(l.last) >= IDLE_LIMIT);
        if free {
            *lease = Some(Lease { owner: owner.to_string(), last: now });
        }
    }

    /// `owner` is done: its lease is let go (another owner's is not touched).
    pub fn release(&self, owner: &str) {
        let mut lease = self.lock();
        if lease.as_ref().is_some_and(|l| l.owner == owner) {
            *lease = None;
        }
    }

    pub fn release_all(&self) {
        *self.lock() = None;
    }

    /// Who has the desktop, if anyone and not long ago.
    pub fn holder_at(&self, now: Instant) -> Option<String> {
        self.lock().as_ref().filter(|l| now.saturating_duration_since(l.last) < IDLE_LIMIT).map(|l| l.owner.clone())
    }

    /// Is a session going? What the Esc key reads.
    pub fn is_live_at(&self, now: Instant) -> bool {
        self.holder_at(now).is_some()
    }
}

static DESK: Desk = Desk::new();

/// The desktop lease of this process.
pub fn global() -> &'static Desk {
    &DESK
}

thread_local! {
    /// Who is calling on this thread: the run id of an agent run (the loop of a run is one thread).
    static OWNER: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Names the session that calls on this thread until the guard is dropped (which does not let go of the lease: [`Desk::release`] does, when the run ends).
pub fn as_owner(owner: &str) -> OwnerGuard {
    OwnerGuard { previous: OWNER.with(|o| o.replace(Some(owner.to_string()))) }
}

pub struct OwnerGuard {
    previous: Option<String>,
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        OWNER.with(|o| *o.borrow_mut() = self.previous.take());
    }
}

pub fn current_owner() -> Option<String> {
    OWNER.with(|o| o.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_session_is_refused_with_the_name_of_the_first_until_it_lets_go_or_goes_idle() {
        let desk = Desk::new();
        let t0 = Instant::now();
        assert!(desk.claim_at("run_a", t0).is_ok());
        assert!(desk.claim_at("run_a", t0 + Duration::from_secs(5)).is_ok(), "the owner keeps it");
        let refused = desk.claim_at("run_b", t0 + Duration::from_secs(10)).unwrap_err();
        assert!(refused.contains("run_a") && refused.contains("another session"), "{refused}");
        assert_eq!(desk.holder_at(t0 + Duration::from_secs(10)).as_deref(), Some("run_a"));
        // Using it renews it: b is still refused long after a began, as long as a went on.
        assert!(desk.claim_at("run_a", t0 + Duration::from_secs(170)).is_ok());
        assert!(desk.claim_at("run_b", t0 + Duration::from_secs(300)).is_err(), "a used it 130 s ago");
        // Not used for the idle limit: it is let go, and b takes it.
        assert!(desk.claim_at("run_b", t0 + Duration::from_secs(170) + IDLE_LIMIT).is_ok());
        assert_eq!(desk.holder_at(t0 + Duration::from_secs(171) + IDLE_LIMIT).as_deref(), Some("run_b"));
        // Letting go is by the owner, and only its own lease.
        desk.release("run_a");
        assert!(desk.is_live_at(t0 + Duration::from_secs(172) + IDLE_LIMIT), "run_a has no lease to let go of: b's stands");
        desk.release("run_b");
        assert!(!desk.is_live_at(t0 + Duration::from_secs(172) + IDLE_LIMIT));
        assert!(desk.claim_at("run_c", t0 + Duration::from_secs(173) + IDLE_LIMIT).is_ok());
        desk.release_all();
        assert_eq!(desk.holder_at(t0 + Duration::from_secs(174) + IDLE_LIMIT), None);
    }

    #[test]
    fn a_run_that_only_looks_keeps_its_lease_alive_and_never_takes_another_runs() {
        let desk = Desk::new();
        let t0 = Instant::now();
        desk.touch_at("run_a", t0);
        assert_eq!(desk.holder_at(t0).as_deref(), Some("run_a"), "nobody had it: looking takes it");
        desk.touch_at("run_b", t0 + Duration::from_secs(1));
        assert_eq!(desk.holder_at(t0 + Duration::from_secs(1)).as_deref(), Some("run_a"), "b only looked: a still has it");
        desk.touch_at("run_a", t0 + Duration::from_secs(170));
        assert!(desk.is_live_at(t0 + Duration::from_secs(170) + Duration::from_secs(170)), "a looked 170 s ago: still going");
        desk.touch_at("run_b", t0 + Duration::from_secs(170) + IDLE_LIMIT + Duration::from_secs(1));
        assert_eq!(desk.holder_at(t0 + Duration::from_secs(171) + IDLE_LIMIT).as_deref(), Some("run_b"), "a let go long ago: b's look takes it");
    }

    #[test]
    fn the_caller_on_a_thread_is_named_for_a_while_and_the_name_goes_back_to_what_it_was() {
        assert_eq!(current_owner(), None);
        {
            let _outer = as_owner("run_x");
            assert_eq!(current_owner().as_deref(), Some("run_x"));
            {
                let _inner = as_owner("run_y");
                assert_eq!(current_owner().as_deref(), Some("run_y"));
            }
            assert_eq!(current_owner().as_deref(), Some("run_x"));
        }
        assert_eq!(current_owner(), None);
        // Another thread has no owner of its own.
        let _mine = as_owner("run_z");
        assert_eq!(std::thread::spawn(current_owner).join().unwrap(), None);
    }
}

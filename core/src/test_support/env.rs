//! `EnvGuard`: environment variables for the length of one test, put back afterwards (also when the test panics).
//!
//! The process environment is shared by every test thread, so a test that sets `AQUA_DOOR` can change what another test sees. The
//! guard takes one process-wide lock for as long as it lives. The lock belongs to the thread, so a thread may hold several guards at
//! once (a helper that sets one variable inside a test that already set another) without waiting for itself. A guard cannot be sent to
//! another thread. The lock cannot be poisoned: a test that panics while holding a guard puts the variables back as it unwinds and
//! frees the lock.
//!
//! The lock only orders the tests that use it. A test that sets a variable by hand (`std::env::set_var`) is not held back by it.

use std::ffi::{OsStr, OsString};
use std::marker::PhantomData;
use std::sync::{Condvar, Mutex};
use std::thread::ThreadId;

struct Holder {
    owner: Option<ThreadId>,
    depth: usize,
}

static HOLDER: Mutex<Holder> = Mutex::new(Holder { owner: None, depth: 0 });
static FREED: Condvar = Condvar::new();

fn acquire() {
    let me = std::thread::current().id();
    let mut h = HOLDER.lock().unwrap_or_else(|e| e.into_inner());
    while h.owner.is_some() && h.owner != Some(me) {
        h = FREED.wait(h).unwrap_or_else(|e| e.into_inner());
    }
    h.owner = Some(me);
    h.depth += 1;
}

fn release() {
    let mut h = HOLDER.lock().unwrap_or_else(|e| e.into_inner());
    h.depth = h.depth.saturating_sub(1);
    if h.depth == 0 {
        h.owner = None;
        FREED.notify_all();
    }
}

/// Holds the environment lock and remembers what every variable it changed held before.
#[must_use = "the variables are put back when the guard is dropped: bind it to a name (`let _env = ...`), not to `_`"]
pub struct EnvGuard {
    /// The first value each changed variable had, in the order they were changed (put back in the reverse order).
    saved: Vec<(OsString, Option<OsString>)>,
    /// A guard belongs to the thread that made it.
    _not_send: PhantomData<*const ()>,
}

impl EnvGuard {
    /// Takes the lock and changes nothing yet (use [`EnvGuard::and_set`] and the like, or the `set_var` methods).
    pub fn hold() -> Self {
        acquire();
        Self { saved: Vec::new(), _not_send: PhantomData }
    }

    /// Takes the lock and sets each variable.
    pub fn set(pairs: &[(&str, &str)]) -> Self {
        let mut g = Self::hold();
        for (k, v) in pairs {
            g.set_var(k, v);
        }
        g
    }

    /// Takes the lock and removes each variable.
    pub fn unset(names: &[&str]) -> Self {
        let mut g = Self::hold();
        for k in names {
            g.remove_var(k);
        }
        g
    }

    /// Sets one more variable (builder form).
    pub fn and_set(mut self, name: &str, value: &str) -> Self {
        self.set_var(name, value);
        self
    }

    /// Removes one more variable (builder form).
    pub fn and_unset(mut self, name: &str) -> Self {
        self.remove_var(name);
        self
    }

    fn remember(&mut self, name: &OsStr) {
        if !self.saved.iter().any(|(k, _)| k == name) {
            self.saved.push((name.to_os_string(), std::env::var_os(name)));
        }
    }

    pub fn set_var(&mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) {
        self.remember(name.as_ref());
        std::env::set_var(name.as_ref(), value.as_ref());
    }

    pub fn remove_var(&mut self, name: impl AsRef<OsStr>) {
        self.remember(name.as_ref());
        std::env::remove_var(name.as_ref());
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, before) in self.saved.drain(..).rev() {
            match before {
                Some(v) => std::env::set_var(&name, v),
                None => std::env::remove_var(&name),
            }
        }
        release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // These names are used by no other test and by no code (and they do not start with AQUA_, so the generated table of environment variables does not list them).
    // Every test has its own pair: a test that sets one by hand (outside the lock, as the tests of the guard must to see what it does) would otherwise change what a test running beside it sees.

    #[test]
    fn a_variable_is_set_inside_and_put_back_after() {
        const A: &str = "TESTKIT_ENV_GUARD_1_A";
        const B: &str = "TESTKIT_ENV_GUARD_1_B";
        std::env::remove_var(A);
        {
            let _env = EnvGuard::set(&[(A, "on")]);
            assert_eq!(std::env::var(A).as_deref(), Ok("on"));
        }
        assert!(std::env::var_os(A).is_none(), "a variable that was not there is removed again");
        std::env::set_var(B, "before");
        {
            let _env = EnvGuard::set(&[(B, "during")]);
            assert_eq!(std::env::var(B).as_deref(), Ok("during"));
        }
        assert_eq!(std::env::var(B).as_deref(), Ok("before"), "a variable that was there gets its own value back");
        {
            let _env = EnvGuard::unset(&[B]);
            assert!(std::env::var_os(B).is_none());
        }
        assert_eq!(std::env::var(B).as_deref(), Ok("before"));
        std::env::remove_var(B);
    }

    #[test]
    fn the_variables_come_back_when_the_test_panics() {
        const A: &str = "TESTKIT_ENV_GUARD_2_A";
        const B: &str = "TESTKIT_ENV_GUARD_2_B";
        std::env::set_var(A, "outside");
        let result = std::panic::catch_unwind(|| {
            let _env = EnvGuard::set(&[(A, "inside")]);
            assert_eq!(std::env::var(A).as_deref(), Ok("inside"));
            panic!("the test failed while the guard was held");
        });
        assert!(result.is_err());
        assert_eq!(std::env::var(A).as_deref(), Ok("outside"), "the unwind put the value back");
        // And the lock is free: this would wait for ever otherwise.
        let _again = EnvGuard::set(&[(B, "x")]);
        std::env::remove_var(A);
    }

    #[test]
    fn guards_nest_in_one_thread_and_put_back_in_the_reverse_order() {
        const A: &str = "TESTKIT_ENV_GUARD_3_A";
        const B: &str = "TESTKIT_ENV_GUARD_3_B";
        std::env::remove_var(A);
        let outer = EnvGuard::set(&[(A, "outer")]);
        {
            let inner = EnvGuard::set(&[(A, "inner")]).and_set(B, "b");
            assert_eq!((std::env::var(A).as_deref(), std::env::var(B).as_deref()), (Ok("inner"), Ok("b")));
            drop(inner);
        }
        assert_eq!(std::env::var(A).as_deref(), Ok("outer"), "the inner guard gave back what the outer one had set");
        assert!(std::env::var_os(B).is_none());
        drop(outer);
        assert!(std::env::var_os(A).is_none());
    }

    #[test]
    fn setting_one_variable_twice_in_one_guard_restores_the_first_value() {
        const A: &str = "TESTKIT_ENV_GUARD_4_A";
        std::env::set_var(A, "first");
        {
            let mut env = EnvGuard::hold();
            env.set_var(A, "second");
            env.set_var(A, "third");
            env.remove_var(A);
        }
        assert_eq!(std::env::var(A).as_deref(), Ok("first"));
        std::env::remove_var(A);
    }

    #[test]
    fn two_threads_with_guards_never_hold_them_at_the_same_time() {
        const A: &str = "TESTKIT_ENV_GUARD_5_A";
        let inside = Arc::new(AtomicUsize::new(0));
        let worst = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let (inside, worst) = (inside.clone(), worst.clone());
                std::thread::spawn(move || {
                    for _ in 0..5 {
                        let _env = EnvGuard::set(&[(A, &format!("thread {i}"))]);
                        let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                        worst.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(2));
                        assert_eq!(std::env::var(A).unwrap(), format!("thread {i}"), "nobody else changed it while this guard was held");
                        inside.fetch_sub(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("a thread of the test panicked");
        }
        assert_eq!(worst.load(Ordering::SeqCst), 1, "never two guards at once");
        assert!(std::env::var_os(A).is_none());
    }
}

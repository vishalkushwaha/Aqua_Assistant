//! Waiting for a signal instead of sleeping.
//!
//! `thread::sleep(500 ms)` followed by an assertion is a bet on how slow the machine is: too short and the test is flaky, too long and
//! every run pays for it. These helpers poll a condition every 10 ms and stop the moment it holds. When it does not hold in time they
//! panic with a sentence that names what was waited for, so a failure reads "timed out after 5000 ms waiting for the run to be
//! marked as stalled" and not "assertion failed".

use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// How often the condition is looked at.
pub const POLL: Duration = Duration::from_millis(10);

/// Waits until `condition` returns true. Panics, naming `what`, when it has not after `timeout`. The condition is looked at once more at
/// the deadline, so a slow poll cannot make a condition that came true in time look false.
pub fn wait_until(timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    wait_for(timeout, what, || condition().then_some(()))
}

/// Waits until `probe` returns a value and gives it back. Panics, naming `what`, after `timeout`.
pub fn wait_for<T>(timeout: Duration, what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let started = Instant::now();
    loop {
        if let Some(found) = probe() {
            return found;
        }
        if started.elapsed() >= timeout {
            // One last look: the probe itself may have taken longer than the time that was left.
            if let Some(found) = probe() {
                return found;
            }
            panic!("timed out after {} ms waiting for {what}", timeout.as_millis());
        }
        std::thread::sleep(POLL);
    }
}

/// Waits until something accepts connections on `127.0.0.1:<port>` (a server that is starting).
pub fn wait_for_port(port: u16, timeout: Duration) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    wait_until(timeout, &format!("port {port} on 127.0.0.1 to accept connections"), || TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok());
}

/// Waits until the file exists (something that writes it in the background) and gives its text back.
pub fn wait_for_file(path: &Path, timeout: Duration) -> String {
    wait_for(timeout, &format!("the file {} to exist", path.display()), || std::fs::read_to_string(path).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn message_of(result: std::thread::Result<()>) -> String {
        let payload = result.err().expect("the wait should have panicked");
        payload.downcast_ref::<String>().cloned().or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string())).expect("a text")
    }

    #[test]
    fn a_condition_that_already_holds_returns_at_once() {
        let started = Instant::now();
        wait_until(Duration::from_secs(5), "nothing", || true);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_condition_that_comes_true_on_another_thread_is_seen() {
        let flag = Arc::new(AtomicBool::new(false));
        let setter = flag.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            setter.store(true, Ordering::SeqCst);
        });
        wait_until(Duration::from_secs(10), "the flag", || flag.load(Ordering::SeqCst));
        t.join().unwrap();
    }

    #[test]
    fn the_timeout_message_names_what_was_waited_for_and_how_long() {
        let m = message_of(std::panic::catch_unwind(|| wait_until(Duration::from_millis(60), "the run to be marked as stalled", || false)));
        assert_eq!(m, "timed out after 60 ms waiting for the run to be marked as stalled");
    }

    #[test]
    fn wait_for_gives_back_what_the_probe_found() {
        let mut calls = 0;
        let found = wait_for(Duration::from_secs(5), "the third call", || {
            calls += 1;
            (calls == 3).then(|| format!("found on call {calls}"))
        });
        assert_eq!(found, "found on call 3");
    }

    #[test]
    fn the_condition_is_looked_at_once_even_with_no_time() {
        wait_until(Duration::ZERO, "something already true", || true);
        let m = message_of(std::panic::catch_unwind(|| wait_until(Duration::ZERO, "something never true", || false)));
        assert!(m.contains("something never true"), "{m}");
    }

    #[test]
    fn a_listening_port_is_ready_and_a_closed_one_times_out() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        wait_for_port(port, Duration::from_secs(5));
        drop(listener);
        let m = message_of(std::panic::catch_unwind(|| wait_for_port(port, Duration::from_millis(100))));
        assert!(m.contains(&format!("port {port}")), "{m}");
    }

    #[test]
    fn a_file_written_later_is_waited_for() {
        let world = super::super::TempWorld::new("wait_file");
        let path = world.path("later.txt");
        let target = path.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            std::fs::write(target, "written").unwrap();
        });
        assert_eq!(wait_for_file(&path, Duration::from_secs(10)), "written");
        t.join().unwrap();
    }
}

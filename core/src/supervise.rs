//! Background threads that must not die quietly, and a record of every panic.
//!
//! The long-running loops of the core (the flow scheduler that also fires reminders, the history prune, the nightly skill tidy, the run watchdog) used to be bare
//! `loop {}` bodies: one panic ended the thread, the rest of the program carried on, and the person simply stopped getting reminders until a restart. [`spawn_supervised`]
//! runs such a loop under `catch_unwind`; a panic is logged once with the thread's name (the payload masked and cut short), counted
//! (`aqua_thread_panics_total{thread=...}`), written to `data/crash/last_panic.json`, and the loop starts again after 1, 2, 4 ... 60 seconds. A thread that panics five times in
//! ten minutes is not a thread that will recover: it is stopped, and its name is in [`background_failed`], which `GET /api/v1/status` reports and `aqua-cli doctor` reads from
//! `data/crash/background_failed.json`.
//!
//! [`install_panic_hook`] adds what the default hook does not give: the thread's name, the place, the masked payload and a backtrace in the log (where the default message
//! reached only the console), and the record in `data/crash`. It runs the hook it replaced afterwards.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, PoisonError};
use std::time::{Duration, Instant};

/// This many panics inside [`WINDOW`] and the thread is given up.
pub const MAX_PANICS: usize = 5;
/// The time in which [`MAX_PANICS`] panics are too many.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);
/// The longest a panic message is kept (in the log and in the record): it can hold a user's text or a path.
const MAX_MESSAGE_CHARS: usize = 300;

/// How long to wait before a thread is started again: `first`, twice that, four times ... and never more than `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Backoff {
    /// 1, 2, 4 ... 60 seconds.
    pub const STANDARD: Backoff = Backoff { first: Duration::from_secs(1), max: Duration::from_secs(60) };

    /// The wait after the panic that has `earlier` panics before it in the window (0 for the first).
    pub fn delay(&self, earlier: u32) -> Duration {
        self.first.saturating_mul(1u32.checked_shl(earlier.min(30)).unwrap_or(u32::MAX)).min(self.max)
    }
}

/// What the supervisor needs from the world: the time (monotonic, so that a change of the clock cannot make panics look rare), sleeping, and where the record goes. The tests
/// put their own in.
#[derive(Clone)]
pub struct Env {
    now: Arc<dyn Fn() -> Duration + Send + Sync>,
    sleep: Arc<dyn Fn(Duration) + Send + Sync>,
    crash_dir: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>,
}

impl Env {
    /// The real clock, real sleeping, and `data/crash` in the data folder.
    pub fn real() -> Env {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let start = *START.get_or_init(Instant::now);
        Env {
            now: Arc::new(move || start.elapsed()),
            sleep: Arc::new(std::thread::sleep),
            crash_dir: Arc::new(|| Some(crate::runtime::data_dir().join("crash"))),
        }
    }

    /// An environment with the given parts (for tests).
    pub fn new(now: impl Fn() -> Duration + Send + Sync + 'static, sleep: impl Fn(Duration) + Send + Sync + 'static, crash_dir: Option<PathBuf>) -> Env {
        Env { now: Arc::new(now), sleep: Arc::new(sleep), crash_dir: Arc::new(move || crash_dir.clone()) }
    }
}

/// The names of the threads that were given up, for this process.
static FAILED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The background threads that panicked too often and were stopped (empty while all is well). `GET /api/v1/status` reports it as `background_failed`.
pub fn background_failed() -> Vec<String> {
    FAILED.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

fn mark_failed(name: &str) {
    let mut failed = FAILED.lock().unwrap_or_else(PoisonError::into_inner);
    if !failed.iter().any(|n| n == name) {
        failed.push(name.to_string());
    }
}

/// What `last_panic.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanicRecord {
    pub thread: String,
    pub at_unix_ms: u64,
    /// `file:line:column`, when the panic hook saw it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// The panic message with secrets masked, on one line, at most 300 characters.
    pub message: String,
    /// Set when a supervisor is starting the thread again: after how many milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_in_ms: Option<u64>,
    /// The thread panicked too often and was stopped.
    #[serde(default)]
    pub gave_up: bool,
}

/// What `background_failed.json` holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedRecord {
    pub threads: Vec<String>,
    pub at_unix_ms: u64,
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// The text of a panic payload (`&str` and `String` are what `panic!` makes; anything else is not text).
pub(crate) fn payload_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(the panic carried something that is not text)".to_string()
    }
}

/// A payload as one line with secrets masked (the tool host's mask) and at most 300 characters.
pub(crate) fn masked(text: &str) -> String {
    let one_line: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let redacted = aqua_tool_host::output::Redactor::new().redact(&one_line);
    let mut out: String = redacted.chars().take(MAX_MESSAGE_CHARS).collect();
    if redacted.chars().count() > MAX_MESSAGE_CHARS {
        out.push_str("...");
    }
    out
}

/// One writer at a time to the files of the crash folder.
static WRITING: Mutex<()> = Mutex::new(());

fn write_json(path: &Path, value: &impl Serialize) {
    let _one = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
    let result = serde_json::to_vec_pretty(value).map_err(std::io::Error::other).and_then(|bytes| aqua_tool_host::fsutil::write_atomic_durable(path, &bytes));
    if let Err(e) = result {
        tracing::warn!("the crash record {} could not be written: {e}", path.display());
    }
}

fn last_panic_path(crash_dir: &Path) -> PathBuf {
    crash_dir.join("last_panic.json")
}

fn failed_path(crash_dir: &Path) -> PathBuf {
    crash_dir.join("background_failed.json")
}

/// The last panic this install recorded (`data/crash/last_panic.json`), if there is one that can be read.
pub fn read_last_panic(data_dir: &Path) -> Option<PanicRecord> {
    serde_json::from_str(&std::fs::read_to_string(last_panic_path(&data_dir.join("crash"))).ok()?).ok()
}

/// The threads the running (or last) core gave up (`data/crash/background_failed.json`), if it recorded any.
pub fn read_failed(data_dir: &Path) -> Option<FailedRecord> {
    serde_json::from_str(&std::fs::read_to_string(failed_path(&data_dir.join("crash"))).ok()?).ok()
}

/// Called once at start-up: what an earlier run recorded as given up is no longer true of this one.
pub fn forget_failed_threads(data_dir: &Path) {
    let _ = std::fs::remove_file(failed_path(&data_dir.join("crash")));
}

/// Runs `body` on a thread named `name`, and starts it again when it panics (see the module's description). `body` is the whole loop and must be able to be entered again:
/// it reads what it needs each time, as the scheduler's tick does. A body that returns ends the thread without a word.
pub fn spawn_supervised(name: &'static str, policy: Backoff, body: impl FnMut() + Send + 'static) -> std::io::Result<std::thread::JoinHandle<()>> {
    spawn_supervised_with(name, policy, Env::real(), body)
}

/// [`spawn_supervised`] with an environment of the caller's.
pub fn spawn_supervised_with(name: &'static str, policy: Backoff, env: Env, body: impl FnMut() + Send + 'static) -> std::io::Result<std::thread::JoinHandle<()>> {
    crate::metrics::describe("aqua_thread_panics_total", "Panics caught in a supervised background thread, by thread name");
    std::thread::Builder::new().name(name.into()).spawn(move || supervise(name, policy, &env, body))
}

/// The supervisor itself, on the calling thread (the tests call it directly with a clock of their own).
pub fn supervise(name: &'static str, policy: Backoff, env: &Env, mut body: impl FnMut()) {
    let mut panics: VecDeque<Duration> = VecDeque::new();
    loop {
        let payload = match catch_unwind(AssertUnwindSafe(&mut body)) {
            Ok(()) => return,
            Err(payload) => payload,
        };
        let now = (env.now)();
        while panics.front().is_some_and(|t| now.saturating_sub(*t) > WINDOW) {
            panics.pop_front();
        }
        panics.push_back(now);
        let message = masked(&payload_text(&*payload));
        drop(payload);
        crate::metrics::inc("aqua_thread_panics_total", &[("thread", name)]);
        let crash_dir = (env.crash_dir)();
        if panics.len() >= MAX_PANICS {
            tracing::error!(thread = name, payload = %message, panics = panics.len(), "background thread panicked {MAX_PANICS} times within ten minutes: it is stopped and stays stopped until Aqua restarts");
            mark_failed(name);
            if let Some(dir) = crash_dir {
                write_json(&last_panic_path(&dir), &PanicRecord { thread: name.into(), at_unix_ms: unix_ms(), location: None, message, restart_in_ms: None, gave_up: true });
                write_json(&failed_path(&dir), &FailedRecord { threads: background_failed(), at_unix_ms: unix_ms() });
            }
            return;
        }
        let wait = policy.delay(panics.len() as u32 - 1);
        tracing::error!(thread = name, payload = %message, restart_in_s = wait.as_secs(), "background thread panicked; starting it again");
        if let Some(dir) = crash_dir {
            write_json(&last_panic_path(&dir), &PanicRecord { thread: name.into(), at_unix_ms: unix_ms(), location: None, message, restart_in_ms: Some(wait.as_millis() as u64), gave_up: false });
        }
        (env.sleep)(wait);
    }
}

/// Puts the panic hook in place (once): every panic, on any thread, is logged with the thread's name, the place, the masked payload and a backtrace, and recorded in
/// `data/crash/last_panic.json`; then the hook that was there before runs. Call it after the log is set up.
pub fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        let env = Env::real();
        std::panic::set_hook(Box::new(move |info| {
            // A panic while handling a panic ends the process, so what the hook does is itself guarded.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let thread = std::thread::current();
                let location = info.location().map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
                let backtrace = std::backtrace::Backtrace::force_capture().to_string();
                report_panic(&env, thread.name().unwrap_or("<unnamed>"), location, info.payload(), &backtrace);
            }));
            previous(info);
        }));
    });
}

/// What the hook does with one panic: the log line (thread, place, masked payload, backtrace) and the record in the crash folder.
fn report_panic(env: &Env, thread: &str, location: Option<String>, payload: &(dyn std::any::Any + Send), backtrace: &str) {
    let message = masked(&payload_text(payload));
    tracing::error!(thread = %thread, location = location.as_deref().unwrap_or("unknown"), payload = %message, "panic\n{backtrace}");
    if let Some(dir) = (env.crash_dir)() {
        write_json(&last_panic_path(&dir), &PanicRecord { thread: thread.into(), at_unix_ms: unix_ms(), location, message, restart_in_ms: None, gave_up: false });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_supervise_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// An environment with a clock that only moves when the supervisor sleeps (or the test moves it), and a sleep that records what it was asked for.
    fn fake(crash_dir: Option<PathBuf>) -> (Env, Arc<Mutex<Duration>>, Arc<Mutex<Vec<Duration>>>) {
        let clock = Arc::new(Mutex::new(Duration::from_secs(1000)));
        let slept = Arc::new(Mutex::new(Vec::new()));
        let (c1, c2, s2) = (clock.clone(), clock.clone(), slept.clone());
        let env = Env::new(
            move || *c1.lock().unwrap(),
            move |d| {
                s2.lock().unwrap().push(d);
                *c2.lock().unwrap() += d;
            },
            crash_dir,
        );
        (env, clock, slept)
    }

    /// A tracing subscriber that keeps what is logged, for the length of the closure.
    fn logged<R>(f: impl FnOnce() -> R) -> (R, String) {
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
            type Writer = Sink;
            fn make_writer(&'a self) -> Sink {
                self.clone()
            }
        }
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt().with_writer(sink.clone()).with_ansi(false).with_max_level(tracing::Level::DEBUG).finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8_lossy(&sink.0.lock().unwrap()).to_string();
        (out, text)
    }

    #[test]
    fn the_wait_doubles_from_one_second_and_stops_at_a_minute() {
        let b = Backoff::STANDARD;
        let waits: Vec<u64> = (0..9).map(|n| b.delay(n).as_secs()).collect();
        assert_eq!(waits, vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(b.delay(u32::MAX), Duration::from_secs(60), "no overflow");
    }

    #[test]
    fn a_panic_is_logged_once_with_the_thread_name_and_the_next_round_runs() {
        let dir = temp("one_panic");
        let (env, _clock, slept) = fake(Some(dir.clone()));
        let rounds = AtomicUsize::new(0);
        let ((), log) = logged(|| {
            supervise("test-scheduler-one", Backoff::STANDARD, &env, || {
                let n = rounds.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    panic!("an injected panic in tick");
                }
                // The second round runs to its end, as a loop that was told to stop would.
            });
        });
        assert_eq!(rounds.load(Ordering::SeqCst), 2, "the body was entered again after the panic");
        assert_eq!(*slept.lock().unwrap(), vec![Duration::from_secs(1)], "one second before the first restart");
        assert_eq!(log.matches("background thread panicked").count(), 1, "logged once: {log}");
        assert!(log.contains("test-scheduler-one") && log.contains("an injected panic in tick"), "{log}");
        assert!(!background_failed().iter().any(|n| n == "test-scheduler-one"), "one panic does not set the flag");
        let written: PanicRecord = serde_json::from_str(&std::fs::read_to_string(dir.join("last_panic.json")).unwrap()).unwrap();
        assert_eq!((written.thread.as_str(), written.restart_in_ms, written.gave_up), ("test-scheduler-one", Some(1000), false));
        assert!(written.message.contains("an injected panic in tick"));
        assert!(!dir.read_dir().unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains(".aqua-tmp-")), "no temporary file is left");
    }

    #[test]
    fn five_panics_in_ten_minutes_stop_the_thread_and_set_the_flag_and_not_before() {
        let dir = temp("five");
        let (env, _clock, slept) = fake(Some(dir.clone()));
        let rounds = AtomicUsize::new(0);
        let flag_early = std::sync::atomic::AtomicBool::new(false);
        let ((), log) = logged(|| supervise("test-always-panics", Backoff::STANDARD, &env, || {
            rounds.fetch_add(1, Ordering::SeqCst);
            if background_failed().iter().any(|n| n == "test-always-panics") {
                flag_early.store(true, Ordering::SeqCst);
            }
            panic!("again");
        }));
        assert_eq!(rounds.load(Ordering::SeqCst), 5, "the fifth panic is the last");
        assert!(!flag_early.load(Ordering::SeqCst), "the flag is not set while it is still being restarted");
        assert_eq!(*slept.lock().unwrap(), vec![1, 2, 4, 8].into_iter().map(Duration::from_secs).collect::<Vec<_>>(), "1, 2, 4, 8 seconds and then it gives up");
        assert!(background_failed().iter().any(|n| n == "test-always-panics"));
        assert!(log.contains("stopped"), "{log}");
        let gave_up: PanicRecord = serde_json::from_str(&std::fs::read_to_string(dir.join("last_panic.json")).unwrap()).unwrap();
        assert!(gave_up.gave_up && gave_up.restart_in_ms.is_none());
        let failed: FailedRecord = serde_json::from_str(&std::fs::read_to_string(dir.join("background_failed.json")).unwrap()).unwrap();
        assert!(failed.threads.iter().any(|n| n == "test-always-panics"));
    }

    #[test]
    fn panics_spread_over_more_than_ten_minutes_never_add_up_and_the_wait_starts_again_each_time() {
        let (env, clock, slept) = fake(None);
        let rounds = AtomicUsize::new(0);
        supervise("test-slow-panics", Backoff::STANDARD, &env, || {
            let n = rounds.fetch_add(1, Ordering::SeqCst);
            // Eleven minutes pass inside each round: the earlier panic is out of the window by the time of the next.
            *clock.lock().unwrap() += Duration::from_secs(11 * 60);
            if n < 8 {
                panic!("once in a long while");
            }
        });
        assert_eq!(rounds.load(Ordering::SeqCst), 9, "eight panics, a ninth round that ends");
        assert!(slept.lock().unwrap().iter().all(|d| *d == Duration::from_secs(1)), "{:?}", slept.lock().unwrap());
        assert!(!background_failed().iter().any(|n| n == "test-slow-panics"));
    }

    #[test]
    fn a_secret_or_a_long_text_in_a_panic_is_masked_and_cut() {
        let (env, _clock, _slept) = fake(None);
        let secret = "password=hunter2secret99";
        let ((), log) = logged(|| {
            let mut first = true;
            supervise("test-secret-panic", Backoff::STANDARD, &env, || {
                if std::mem::take(&mut first) {
                    panic!("the key was {secret} and then {}", "x".repeat(2000));
                }
            });
        });
        assert!(!log.contains("hunter2secret99"), "the secret is not in the log: {log}");
        assert!(!log.contains(&"x".repeat(400)), "the text is cut");
        // A payload that is not text does not break the supervisor.
        let (env2, _c, slept2) = fake(None);
        let mut thrown = false;
        supervise("test-odd-payload", Backoff::STANDARD, &env2, || {
            if !std::mem::replace(&mut thrown, true) {
                std::panic::panic_any(42u32);
            }
        });
        assert_eq!(slept2.lock().unwrap().len(), 1);
        assert_eq!(masked("a\nb\tc"), "a b c");
    }

    #[test]
    fn a_real_thread_is_named_survives_a_panic_and_runs_its_body_again() {
        let (env, _clock, _slept) = fake(None);
        let rounds = Arc::new(AtomicUsize::new(0));
        let seen_name = Arc::new(Mutex::new(String::new()));
        let (r, n) = (rounds.clone(), seen_name.clone());
        let handle = spawn_supervised_with("aqua-test-supervised", Backoff::STANDARD, env, move || {
            *n.lock().unwrap() = std::thread::current().name().unwrap_or("").to_string();
            if r.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("first round");
            }
        })
        .unwrap();
        handle.join().unwrap();
        assert_eq!(rounds.load(Ordering::SeqCst), 2);
        assert_eq!(*seen_name.lock().unwrap(), "aqua-test-supervised");
        assert!(crate::metrics::counter_value("aqua_thread_panics_total", &[("thread", "aqua-test-supervised")]) >= 1.0);
    }

    #[test]
    fn the_crash_files_are_read_back_and_a_new_start_forgets_what_an_earlier_one_gave_up() {
        let data = temp("files");
        let crash = data.join("crash");
        assert!(read_last_panic(&data).is_none() && read_failed(&data).is_none());
        write_json(&last_panic_path(&crash), &PanicRecord { thread: "t".into(), at_unix_ms: 5, location: Some("a.rs:1:2".into()), message: "m".into(), restart_in_ms: None, gave_up: true });
        write_json(&failed_path(&crash), &FailedRecord { threads: vec!["t".into()], at_unix_ms: 5 });
        assert_eq!(read_last_panic(&data).unwrap().location.as_deref(), Some("a.rs:1:2"));
        assert_eq!(read_failed(&data).unwrap().threads, vec!["t".to_string()]);
        forget_failed_threads(&data);
        assert!(read_failed(&data).is_none() && read_last_panic(&data).is_some(), "the last panic stays as a record, the flag does not");
        // A file that is not JSON reads as nothing.
        std::fs::write(last_panic_path(&crash), "{\"thread\":").unwrap();
        assert!(read_last_panic(&data).is_none());
    }

    #[test]
    fn what_the_hook_reports_is_the_thread_the_place_and_a_masked_payload_and_it_is_recorded() {
        let dir = temp("hook");
        let (env, _clock, _slept) = fake(Some(dir.clone()));
        let ((), log) = logged(|| {
            report_panic(&env, "aqua-hook-probe", Some("core/src/x.rs:10:5".into()), &"probe panic with password=hunter2secret99", "0: frame_a\n1: frame_b");
        });
        assert!(log.contains("aqua-hook-probe") && log.contains("core/src/x.rs:10:5") && log.contains("frame_b"), "{log}");
        assert!(!log.contains("hunter2secret99"), "{log}");
        let written: PanicRecord = serde_json::from_str(&std::fs::read_to_string(dir.join("last_panic.json")).unwrap()).unwrap();
        assert_eq!((written.thread.as_str(), written.location.as_deref(), written.gave_up), ("aqua-hook-probe", Some("core/src/x.rs:10:5"), false));
        assert!(!written.message.contains("hunter2secret99") && written.message.contains("probe panic"), "{}", written.message);
        // A payload that is not text is reported as such.
        let ((), log) = logged(|| report_panic(&env, "aqua-hook-probe", None, &42u32, ""));
        assert!(log.contains("not text") && log.contains("unknown"), "{log}");
    }
}

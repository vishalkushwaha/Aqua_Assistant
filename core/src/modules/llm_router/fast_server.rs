//! The second model server of the fast tier (card OA9-06, task PR-11 of the OpenCode audit). **Off unless `AQUA_FAST_TIER` is on and `AQUA_FAST_TIER_MODEL` names the owner's own `.gguf`.**
//!
//! The routing rule (`model::fast_tier`) sends the roles decision, fast and diagnosis to a smaller model and asks the main model when the small one cannot answer. The small model needs a
//! server of its own: the main server holds one model in one static, and a call for another model would unload the first and load the second (10 to 40 s, both prompt caches lost). This
//! file hosts that second process: one `llama-server`, started with the same argument builder as the main one (`launch_args::launch_args`), nothing the speed experiments add (no draft
//! model, no saved slots, no cache options), and these rules:
//!
//! * **It never takes video memory from the main model.** `-ngl 0` and the processor build unless `AQUA_FAST_TIER_NGL` is above 0 (the person's choice and risk: then the graphics-card
//!   build and its device are used, as the main server's plan names them). One slot, a small window (`AQUA_FAST_TIER_CTX`, 4096), half the threads of the main model.
//! * **It is lazy.** Nothing is started when Aqua starts, and nothing is read or started with the switch off. The first call that needs it spawns the process and waits at most
//!   [`START_GRACE`] for its health check; a model that is still loading is not waited for (the call goes to the main model at once, the server goes on loading, and the next call finds
//!   it ready). A start that fails, a server that dies, one that answers with a server error and one that is too slow are remembered for [`BACKOFF`] (five minutes, the rule of
//!   `fast_tier`), and in that time every call goes to the main model without trying: a missing or broken model never delays a call.
//! * **It is stopped** after `AQUA_FAST_TIER_IDLE_SECS` without a call (a reaper looks every five seconds), when the server for the main model is stopped on purpose (`llama_server::shutdown`:
//!   Aqua stopping, the model unloaded), and by its Job Object when Aqua dies, however it dies: the whole tree, as for the main server.
//! * **It listens on `127.0.0.1` only**, on a free port that is not 8767, 8766, 1420 or the main server's, with its own random key (in the environment of the process, not on its command
//!   line), and every request is the main server's request: `llama_server::post_json` with the key.
//! * **One request at a time** (its one slot). A call that finds it busy waits for its turn within the time it was given and then goes to the main model.
//!
//! A typed error of this server is never final for the call: [`Failure`] is mapped by `model::fast_route` to classes the routing rule falls back on (`Unavailable`, `Timeout`,
//! `ContextOverflow`), also for a refusal of the request, which a small model's server may give for a reason that the main one would not have (its window, its template).
//!
//! **Not run on a model.** It was tried against `aqua_fake_llama` only (`core/tests/speed_options_server_test.rs`). Whether a small model on the processor answers a decision faster than the
//! 8B on the card, and whether it is accurate enough, is what `scripts/eval/speed_experiments/run_experiment.py fast_tier` measures.

use super::draft_model::strip_verbatim;
use super::launch_args::{launch_args, Capabilities, Switches, Values};
use super::llama_server;
use crate::flags;
use aqua_native_tools::contain::{Contained, Limits};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// How long a failed start, a dead server, a server error or a slow answer keeps the call away from the second server: what `fast_tier` uses for a fast model that is missing or too slow.
pub(crate) const BACKOFF: Duration = crate::model::fast_tier::COOLDOWN;

/// How long the call that starts the server waits for its health check before the main model answers instead. The server goes on loading, and the next call finds it ready. This is the
/// most a start can cost a call; a small model takes longer than this to load, so the first call after a start (or after the server was stopped when it was idle) is the main model's.
pub(crate) const START_GRACE: Duration = Duration::from_millis(300);

/// How long a server may take to load before it is given up on (and the start counts as failed).
const START_LIMIT: Duration = Duration::from_secs(120);

/// How often the reaper looks at the server.
const REAP_EVERY: Duration = Duration::from_secs(5);

/// The ports of Aqua itself: the core, the tools, the desktop shell.
const AQUA_PORTS: [u16; 3] = [8767, 8766, 1420];

/// What the person set for the second server (the flags of the registry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Settings {
    /// Layers on the graphics card: 0, the processor only, unless the person says otherwise.
    pub layers: u32,
    /// The window of the one slot, in tokens.
    pub window: u32,
    /// How long the server may sit without a call.
    pub idle: Duration,
}

pub(crate) fn settings() -> Settings {
    Settings {
        layers: flags::int(&flags::FAST_TIER_NGL).max(0) as u32,
        window: flags::int(&flags::FAST_TIER_CTX).max(0) as u32,
        idle: Duration::from_secs(flags::int(&flags::FAST_TIER_IDLE_SECS).max(0) as u64),
    }
}

/// Why a call could not be answered by the second server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    /// It cannot answer now: missing, not started, still loading, backing off, died, refused or failed. The main model is asked.
    Unavailable(String),
    /// It took longer than the time the call was given.
    Timeout(String),
    /// The prompt does not fit its window (its own refusal, with the counts).
    TooLong(String),
    /// The person's Stop, or Aqua stopping.
    Cancelled,
}

struct Running {
    child: Child,
    _job: Option<Contained>,
    port: u16,
    key: String,
    model: PathBuf,
    spawned: Instant,
    healthy: bool,
    last_used: Instant,
    log_file: PathBuf,
}

/// The process, held for the whole of a call and of a start (one request at a time).
static SERVER: Mutex<Option<Running>> = Mutex::new(None);

/// Set while [`shutdown`] runs, so that a call that is waiting or being answered lets go within a few tens of milliseconds.
static STOPPING: AtomicBool = AtomicBool::new(false);

static REAPER: OnceLock<()> = OnceLock::new();

/// Where the second server is, for the status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// No process: never started, stopped when it was idle, or stopped with Aqua.
    Waiting,
    /// A process is loading its model.
    Starting,
    /// A process answered its health check.
    Ready,
    /// The last start or the last answer failed (see `why`, and `backoff_until`).
    Failed,
}

impl Phase {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::Starting => "starting",
            Phase::Ready => "ready",
            Phase::Failed => "failed",
        }
    }
}

struct Status {
    phase: Phase,
    why: Option<String>,
    backoff_until: Option<Instant>,
    starts: u64,
    answered: u64,
    stopped_idle: u64,
    failures: u64,
}

impl Status {
    const fn new() -> Status {
        Status { phase: Phase::Waiting, why: None, backoff_until: None, starts: 0, answered: 0, stopped_idle: 0, failures: 0 }
    }
}

/// What may be asked about the second server without waiting for it: only ever held for an instant.
static STATUS: Mutex<Status> = Mutex::new(Status::new());

fn status_lock() -> std::sync::MutexGuard<'static, Status> {
    STATUS.lock().unwrap_or_else(|e| e.into_inner())
}

fn server_lock() -> std::sync::MutexGuard<'static, Option<Running>> {
    SERVER.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long the second server is still left alone at `now`, and why; `None` when it may be tried.
fn backoff_left_in(status: &Mutex<Status>, now: Instant) -> Option<(Duration, String)> {
    let s = status.lock().unwrap_or_else(|e| e.into_inner());
    let until = s.backoff_until?;
    (until > now).then(|| (until - now, s.why.clone().unwrap_or_default()))
}

fn backoff_left(now: Instant) -> Option<(Duration, String)> {
    backoff_left_in(&STATUS, now)
}

/// The start or the server failed at `now`: it is left alone for [`BACKOFF`], and the status says why.
fn fail_in(status: &Mutex<Status>, why: String, now: Instant) -> String {
    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
    s.phase = Phase::Failed;
    s.why = Some(why.clone());
    s.backoff_until = Some(now + BACKOFF);
    s.failures += 1;
    why
}

fn fail(why: String) -> String {
    warn!("the fast tier's model server: {why}; the main model answers for the next {} s", BACKOFF.as_secs());
    fail_in(&STATUS, why, Instant::now())
}

fn set_phase(phase: Phase) {
    let mut s = status_lock();
    s.phase = phase;
    if phase != Phase::Failed {
        s.why = None;
    }
}

/// Is the second server running and answering its health check right now? Never waits for a call.
pub fn is_ready() -> bool {
    match SERVER.try_lock() {
        Ok(g) => g.as_ref().is_some_and(|r| r.healthy),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().as_ref().is_some_and(|r| r.healthy),
        // A call is being answered or the server is being started: the phase says which.
        Err(std::sync::TryLockError::WouldBlock) => status_lock().phase == Phase::Ready,
    }
}

/// The command line of the second server: the main launch's own builder with the small window, one slot and these layers, and **nothing the speed experiments add**.
pub(crate) fn command_line(model: &Path, port: u16, log_file: &Path, s: &Settings, threads: usize, card: Option<(&str, &[String])>) -> Vec<OsString> {
    let (threads, layers) = (threads.to_string(), s.layers.to_string());
    let values = Values { model, port, window: s.window, threads: &threads, threads_batch: &threads, slots: 1, log_file, layers: Some(&layers), card };
    launch_args(&values, &Switches::default(), &Capabilities::default()).args
}

/// A free port of this machine that is none of Aqua's own and not the main server's.
fn pick_port() -> Result<u16, String> {
    for _ in 0..8 {
        let port = llama_server::free_port().map_err(|e| e.to_string())?;
        if !AQUA_PORTS.contains(&port) && Some(port) != llama_server::port_now() {
            return Ok(port);
        }
    }
    Err("no free local port that is not one of Aqua's".to_string())
}

/// Half the threads of the main server (the two share the processor), and at least two.
fn threads() -> usize {
    llama_server::threads().parse::<usize>().unwrap_or_else(|_| llama_server::default_threads()).div_ceil(2).max(2)
}

/// The file of the owner's small model, checked: a plain `.gguf` file name, a file inside the model folder after links are followed, a real GGUF header, and not the main model itself.
/// Nothing is downloaded; the sentence says what is wrong, without a path.
pub(crate) fn check_model(model_folder: &Path, name: &str, main: &Path) -> Result<PathBuf, String> {
    if !crate::model::roles::is_plain_file_name(name) {
        return Err("AQUA_FAST_TIER_MODEL is not the name of a .gguf file of the model folder (no folder, no path)".to_string());
    }
    let real = std::fs::canonicalize(model_folder.join(name)).map_err(|_| "the file named by AQUA_FAST_TIER_MODEL was not found in the model folder".to_string())?;
    if !real.is_file() {
        return Err("the file named by AQUA_FAST_TIER_MODEL is not a file".to_string());
    }
    match std::fs::canonicalize(model_folder) {
        Ok(root) if real.starts_with(&root) => {}
        _ => return Err("the file named by AQUA_FAST_TIER_MODEL is not inside the model folder (a link that leaves it is refused)".to_string()),
    }
    if std::fs::canonicalize(main).ok().as_deref() == Some(real.as_path()) {
        return Err("the fast tier's file is the main model itself".to_string());
    }
    crate::model::gguf::read_cached(&real).map_err(|e| format!("the fast tier's file is not a usable GGUF file: {e}"))?;
    Ok(strip_verbatim(real))
}

/// Starts the process (it is not waited for). The processor build unless layers are asked for.
fn spawn(model: &Path, s: &Settings) -> Result<Running, String> {
    let plan = if s.layers > 0 { super::gpu::plan() } else { None };
    let bin = match &plan {
        Some(p) => p.binary.clone(),
        None => llama_server::server_binary().ok_or_else(|| "llama-server was not found next to llama-cli".to_string())?,
    };
    let port = pick_port()?;
    let key = llama_server::random_key();
    let log_file = crate::runtime::data_dir().join("logs").join("llama-server-fast.log");
    if let Some(dir) = log_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&log_file);
    let card_flags = plan.as_ref().map(|p| super::gpu::server_flags(&p.device)).unwrap_or_default();
    let args = command_line(model, port, &log_file, s, threads(), plan.as_ref().map(|p| (p.device.id.as_str(), card_flags.as_slice())));
    let mut cmd = Command::new(&bin);
    // The key travels in the environment, not on the command line, where any program of this user could read it from the process list.
    cmd.args(&args).env("LLAMA_API_KEY", &key).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", bin.display()))?;
    // The server ends when Aqua does: the job is closed with this process. One that cannot be contained is not kept.
    let job = match Contained::attach(&child, Limits { memory_mb: 8_192, max_processes: 8 }) {
        Ok(j) => Some(j),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("the second llama-server could not be placed under Aqua's process job ({e}), so it was not kept running"));
        }
    };
    info!(
        "starting the fast tier's model server on 127.0.0.1:{port} (model {}, {} layers on the card, window {}, one slot)",
        model.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
        s.layers,
        s.window
    );
    let now = Instant::now();
    Ok(Running { child, _job: job, port, key, model: model.to_path_buf(), spawned: now, healthy: false, last_used: now, log_file })
}

/// Kills the process and waits for it (its job is closed with it).
fn stop_running(running: Option<Running>) {
    if let Some(mut r) = running {
        let _ = r.child.kill();
        let _ = r.child.wait();
        info!("the fast tier's model server stopped");
    }
}

fn health_ok(port: u16) -> bool {
    matches!(ureq::get(&format!("http://127.0.0.1:{port}/health")).timeout(Duration::from_millis(400)).call(), Ok(r) if r.status() == 200)
}

/// The lock on the second server, within the time of the call.
fn lock_until(deadline: Instant, keep_going: &mut dyn FnMut(u64) -> bool, began: Instant) -> Result<std::sync::MutexGuard<'static, Option<Running>>, Failure> {
    loop {
        match SERVER.try_lock() {
            Ok(g) => return Ok(g),
            Err(std::sync::TryLockError::Poisoned(p)) => return Ok(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if STOPPING.load(Ordering::SeqCst) || !keep_going(began.elapsed().as_secs()) {
            return Err(Failure::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Failure::Timeout("the fast tier's server was busy with another call for the whole time this one was given".to_string()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// One request to the running server, given up on when `keep_going` says no (it is also the call's ticker) or when Aqua stops.
fn post_watching(port: u16, key: &str, body: &Value, timeout: Duration, began: Instant, keep_going: &mut dyn FnMut(u64) -> bool) -> Result<Value, String> {
    let abort = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let request = scope.spawn(|| llama_server::post_json(port, key, "/v1/chat/completions", body, timeout, &abort));
        while !request.is_finished() {
            if STOPPING.load(Ordering::SeqCst) || !keep_going(began.elapsed().as_secs()) {
                abort.store(true, Ordering::SeqCst);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        request.join().unwrap_or_else(|_| Err("the request thread ended unexpectedly".to_string()))
    })
}

/// What a failed request means for the call and for the server: the failure, and whether the server is to be stopped and left alone.
fn judge(error: &str) -> (Failure, bool) {
    if error == "cancelled" {
        return (Failure::Cancelled, false);
    }
    if error.starts_with("timed out") {
        return (Failure::Timeout("the fast tier's model took longer than the time it was given".to_string()), true);
    }
    if llama_server::is_client_rejection(error) {
        // The server is well and refused this request. A prompt past its small window is the usual reason.
        return match llama_server::overflow_counts(error) {
            Some(_) => (Failure::TooLong(llama_server::refusal_message(error)), false),
            None => (Failure::Unavailable(format!("the fast tier's server refused the request ({})", error.chars().take(120).collect::<String>())), false),
        };
    }
    (Failure::Unavailable(format!("the fast tier's server failed ({})", error.chars().take(120).collect::<String>())), true)
}

/// One call to the second server: starts it if it is not running, and gives its reply (the JSON body of `/v1/chat/completions`). `budget` is the time the whole call may take, the start
/// included; `keep_going` is told the seconds so far a few times a second and says whether the person is still waiting. Never starts anything for the switch's sake: whoever calls
/// this has checked that the fast tier is on.
pub(crate) fn request(model: &Path, body: &Value, budget: Duration, keep_going: &mut dyn FnMut(u64) -> bool) -> Result<Value, Failure> {
    let began = Instant::now();
    let deadline = began + budget;
    if let Some((left, why)) = backoff_left(began) {
        return Err(Failure::Unavailable(format!("the fast tier's server is left alone for another {} s after: {why}", left.as_secs().max(1))));
    }
    let s = settings();
    let mut guard = lock_until(deadline, keep_going, began)?;
    // A server for another model is replaced; one that died is not started again at once (it is left alone, as for a start that failed).
    let exited = guard.as_mut().is_some_and(|r| r.child.try_wait().ok().flatten().is_some());
    if exited {
        let tail = guard.as_ref().map(|r| llama_server::log_tail(&r.log_file)).unwrap_or_default();
        stop_running(guard.take());
        return Err(Failure::Unavailable(fail(format!("the fast tier's server exited by itself{tail}"))));
    }
    if guard.as_ref().is_some_and(|r| r.model != model) {
        stop_running(guard.take());
    }
    let mut spawned_now = false;
    if guard.is_none() {
        match spawn(model, &s) {
            Ok(r) => {
                status_lock().starts += 1;
                set_phase(Phase::Starting);
                *guard = Some(r);
                spawned_now = true;
                start_reaper();
            }
            Err(why) => return Err(Failure::Unavailable(fail(why))),
        }
    }
    let Some(r) = guard.as_mut() else { return Err(Failure::Unavailable("the fast tier's server vanished".to_string())) };
    if !r.healthy {
        // Only the call that started the process waits for it, and not for long; the others look once.
        let wait_until = if spawned_now { (Instant::now() + START_GRACE).min(deadline) } else { Instant::now() };
        loop {
            if r.child.try_wait().ok().flatten().is_some() {
                let tail = llama_server::log_tail(&r.log_file);
                stop_running(guard.take());
                return Err(Failure::Unavailable(fail(format!("the fast tier's server exited while loading the model{tail}"))));
            }
            if health_ok(r.port) {
                r.healthy = true;
                set_phase(Phase::Ready);
                info!("the fast tier's model server is ready after {} ms", r.spawned.elapsed().as_millis());
                break;
            }
            if STOPPING.load(Ordering::SeqCst) || !keep_going(began.elapsed().as_secs()) {
                return Err(Failure::Cancelled);
            }
            if r.spawned.elapsed() > START_LIMIT {
                stop_running(guard.take());
                return Err(Failure::Unavailable(fail("the fast tier's server did not become ready within two minutes".to_string())));
            }
            if Instant::now() >= wait_until {
                return Err(Failure::Unavailable("the fast tier's model is still loading, so the main model answers this call".to_string()));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    let (port, key) = (r.port, r.key.clone());
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Failure::Timeout("the fast tier's model had no time left to answer".to_string()));
    }
    match post_watching(port, &key, body, remaining, began, keep_going) {
        Ok(reply) => {
            if let Some(r) = guard.as_mut() {
                r.last_used = Instant::now();
            }
            status_lock().answered += 1;
            Ok(reply)
        }
        Err(error) => {
            let (failure, broken) = judge(&error);
            if broken {
                stop_running(guard.take());
                fail(match &failure {
                    Failure::Timeout(why) | Failure::Unavailable(why) => why.clone(),
                    _ => "the fast tier's server failed".to_string(),
                });
            }
            Err(failure)
        }
    }
}

/// One pass of the reaper: a server that exited is dropped (and left alone), one that finished loading is noted as ready, one that has not been called for `idle_after` is stopped.
/// Gives whether a process was stopped. A server that is busy is not looked at.
fn reap_once(idle_after: Duration) -> bool {
    let mut guard = match SERVER.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return false,
    };
    let Some(r) = guard.as_mut() else { return false };
    if r.child.try_wait().ok().flatten().is_some() {
        let tail = llama_server::log_tail(&r.log_file);
        stop_running(guard.take());
        fail(format!("the fast tier's server exited by itself{tail}"));
        return true;
    }
    if !r.healthy {
        if health_ok(r.port) {
            r.healthy = true;
            set_phase(Phase::Ready);
        } else if r.spawned.elapsed() > START_LIMIT {
            stop_running(guard.take());
            fail("the fast tier's server did not become ready within two minutes".to_string());
            return true;
        }
        return false;
    }
    if r.last_used.elapsed() >= idle_after {
        info!("the fast tier's model server was not called for {} s: stopping it to give its memory back", idle_after.as_secs());
        stop_running(guard.take());
        let mut s = status_lock();
        s.phase = Phase::Waiting;
        s.why = Some(format!("stopped after {} s without a call; the next call starts it again", idle_after.as_secs()));
        s.stopped_idle += 1;
        return true;
    }
    false
}

fn start_reaper() {
    REAPER.get_or_init(|| {
        let _ = std::thread::Builder::new().name("fast-tier-reaper".into()).spawn(|| loop {
            std::thread::sleep(REAP_EVERY);
            reap_once(settings().idle);
        });
    });
}

/// Stops the second server if it has been idle for `idle_after` (what the reaper does every five seconds with `AQUA_FAST_TIER_IDLE_SECS`): for a test that cannot wait for the reaper.
#[cfg(any(test, feature = "test-support"))]
pub fn stop_if_idle_for(idle_after: Duration) -> bool {
    reap_once(idle_after)
}

/// Stops the second server now, and forgets what went wrong and for how long it is left alone: called when the main server is stopped on purpose (`llama_server::shutdown`: Aqua stopping,
/// the model unloaded, a model imported or removed, a test). A call that is waiting for the server, or being answered by it, lets go at once.
pub(crate) fn shutdown() {
    STOPPING.store(true, Ordering::SeqCst);
    let taken = server_lock().take();
    stop_running(taken);
    {
        let mut s = status_lock();
        s.phase = Phase::Waiting;
        s.why = None;
        s.backoff_until = None;
    }
    STOPPING.store(false, Ordering::SeqCst);
}

/// What the second server is and has done, for the status: no path, no port, no key.
pub(crate) fn status() -> Value {
    let s = status_lock();
    let left = s.backoff_until.and_then(|u| u.checked_duration_since(Instant::now())).map(|d| d.as_secs().max(1));
    json!({
        "phase": s.phase.word(),
        "why": s.why,
        "left_alone_for_seconds": left,
        "started": s.starts,
        "answered": s.answered,
        "stopped_idle": s.stopped_idle,
        "failed": s.failures,
        "layers_on_the_card": settings().layers,
        "window": settings().window,
        "stopped_after_seconds_idle": settings().idle.as_secs(),
    })
}

/// The one word for the status of the fast tier: `off`, `not_used` (on, but there is no usable small model: the sentence says why), `waiting` (on and usable; the server starts at the first
/// call that needs it, or was stopped when it was idle), `starting`, `ready` or `failed`. A server that failed and whose time of being left alone is over is `waiting` again.
pub(crate) fn state_word(switch_on: bool, unusable: Option<&str>) -> (&'static str, Option<String>) {
    state_in(&STATUS, switch_on, unusable, Instant::now())
}

fn state_in(status: &Mutex<Status>, switch_on: bool, unusable: Option<&str>, now: Instant) -> (&'static str, Option<String>) {
    if !switch_on {
        return ("off", None);
    }
    if let Some(why) = unusable {
        return ("not_used", Some(why.to_string()));
    }
    let s = status.lock().unwrap_or_else(|e| e.into_inner());
    match s.phase {
        Phase::Failed if s.backoff_until.is_some_and(|u| u <= now) => ("waiting", s.why.as_ref().map(|w| format!("{w}; it is tried again at the next call"))),
        phase => (phase.word(), s.why.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempWorld;

    fn settings_of(layers: u32) -> Settings {
        Settings { layers, window: 4096, idle: Duration::from_secs(300) }
    }

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_command_line_is_the_main_launchs_builder_with_the_processor_one_slot_a_small_window_and_nothing_the_experiments_add() {
        let args = strings(&command_line(Path::new("Small.gguf"), 52_000, Path::new("fast.log"), &settings_of(0), 3, None));
        let want: Vec<String> = ["-m", "Small.gguf", "--host", "127.0.0.1", "--port", "52000", "-c", "4096", "-t", "3", "-tb", "3", "--reasoning", "off", "--no-webui", "--parallel", "1", "-lv", "1", "--log-file", "fast.log", "-ngl", "0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(args, want, "no card, no layers, one slot, loopback only");
        for never in ["-dev", "--fit-target", "--spec-draft-model", "--spec-type", "--slot-save-path", "--cache-reuse", "--cache-ram", "--jinja", "-ctk", "-fa"] {
            assert!(!args.iter().any(|a| a == never), "{never} is in the command line of the second server: {args:?}");
        }
        // Layers on the card are the person's: then the card and its options come after, as in the main launch.
        let flags = vec!["--fit-target".to_string(), "1660".to_string()];
        let on_card = strings(&command_line(Path::new("Small.gguf"), 52_000, Path::new("fast.log"), &settings_of(7), 3, Some(("Vulkan1", flags.as_slice()))));
        assert_eq!(&on_card[on_card.len() - 6..], ["-ngl", "7", "-dev", "Vulkan1", "--fit-target", "1660"]);
    }

    #[test]
    fn the_settings_are_the_processor_a_small_window_and_five_minutes_unless_the_flags_say_otherwise() {
        flags::with_overrides(&[("AQUA_FAST_TIER_NGL", None), ("AQUA_FAST_TIER_CTX", None), ("AQUA_FAST_TIER_IDLE_SECS", None)], || {
            assert_eq!(settings(), Settings { layers: 0, window: 4096, idle: Duration::from_secs(300) });
        });
        flags::with_overrides(&[("AQUA_FAST_TIER_NGL", Some("12")), ("AQUA_FAST_TIER_CTX", Some("2048")), ("AQUA_FAST_TIER_IDLE_SECS", Some("60"))], || {
            assert_eq!(settings(), Settings { layers: 12, window: 2048, idle: Duration::from_secs(60) });
        });
        // A number outside the range, or words, is the default: the second server is never given a window of 2 tokens or 500 layers.
        flags::with_overrides(&[("AQUA_FAST_TIER_NGL", Some("500")), ("AQUA_FAST_TIER_CTX", Some("2")), ("AQUA_FAST_TIER_IDLE_SECS", Some("soon"))], || {
            assert_eq!(settings(), Settings { layers: 0, window: 4096, idle: Duration::from_secs(300) });
        });
    }

    #[test]
    fn the_port_is_a_free_one_that_is_none_of_aquas() {
        for _ in 0..20 {
            let port = pick_port().expect("a free port");
            assert!(port >= 1024 && !AQUA_PORTS.contains(&port), "{port}");
            assert!(std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(), "it was free when it was chosen");
        }
    }

    #[test]
    fn the_file_of_the_small_model_is_the_owners_gguf_in_the_model_folder_and_not_the_main_model() {
        let world = TempWorld::new("fast_server_check");
        let folder = world.path("models/llm");
        std::fs::create_dir_all(&folder).unwrap();
        let main = world.write("models/llm/Qwen3-8B-Q4_K_M.gguf", "not a model");
        let header = {
            let mut b = Vec::new();
            b.extend(b"GGUF");
            b.extend(3u32.to_le_bytes());
            b.extend(0u64.to_le_bytes());
            b.extend(0u64.to_le_bytes());
            b
        };
        world.write("models/llm/Small-0.6B-Q8_0.gguf", &header);
        world.write("models/llm/Broken-0.6B-Q8_0.gguf", "this is not a gguf header at all, only words");
        let ok = check_model(&folder, "Small-0.6B-Q8_0.gguf", &main).expect("a usable file");
        assert!(ok.ends_with("Small-0.6B-Q8_0.gguf") && ok.is_absolute());
        for (name, wants) in [
            ("Missing.gguf", "was not found"),
            ("../escape.gguf", "no folder, no path"),
            ("notes.txt", "no folder, no path"),
            ("Broken-0.6B-Q8_0.gguf", "not a usable GGUF file"),
            ("Qwen3-8B-Q4_K_M.gguf", "main model itself"),
            ("", "no folder, no path"),
        ] {
            let why = check_model(&folder, name, &main).unwrap_err();
            assert!(why.contains(wants), "{name:?}: {why}");
            assert!(!why.contains(&*folder.to_string_lossy()), "no path in the sentence: {why}");
        }
    }

    #[test]
    fn a_state_is_one_word_and_a_failed_server_is_waiting_again_when_it_is_no_longer_left_alone() {
        // The switch off, and a small model that cannot be used, do not depend on the process.
        assert_eq!(state_word(false, None), ("off", None));
        assert_eq!(state_word(false, Some("whatever")), ("off", None));
        assert_eq!(state_word(true, Some("the file was not found")), ("not_used", Some("the file was not found".to_string())));
        assert_eq!(Phase::Waiting.word(), "waiting");
        assert_eq!((Phase::Starting.word(), Phase::Ready.word(), Phase::Failed.word()), ("starting", "ready", "failed"));
    }

    #[test]
    fn a_failure_of_a_request_is_judged_by_what_it_says_and_a_refusal_never_stops_the_server() {
        assert_eq!(judge("cancelled"), (Failure::Cancelled, false));
        assert!(matches!(judge("timed out waiting for the reply"), (Failure::Timeout(_), true)), "too slow: left alone");
        // The server is well and refused: a prompt past its window is a counted overflow, any other refusal is a server that cannot take this request.
        let overflow = r#"HTTP 400: {"error":{"code":400,"message":"the request exceeds the available context size","type":"exceed_context_size_error","n_prompt_tokens":5000,"n_ctx":4096}}"#;
        match judge(overflow) {
            (Failure::TooLong(why), false) => assert!(why.contains("5000") && why.contains("4096"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(judge(r#"HTTP 400: {"error":{"message":"grammar not supported"}}"#), (Failure::Unavailable(_), false)));
        // A server error, a refused connection and an unreadable answer: the server is broken.
        for broken in ["HTTP 500: boom", "connection: refused", "read: reset", "the reply had no header"] {
            assert!(matches!(judge(broken), (Failure::Unavailable(_), true)), "{broken}");
        }
        // What the status says of an error is short.
        let long = format!("HTTP 500: {}", "x".repeat(2_000));
        let (Failure::Unavailable(why), _) = judge(&long) else { panic!("a server error is unavailable") };
        assert!(why.len() < 200, "{}", why.len());
    }

    #[test]
    fn a_failed_start_is_remembered_for_five_minutes_and_then_the_server_is_waiting_for_the_next_call() {
        let status = Mutex::new(Status::new());
        let now = Instant::now();
        assert!(backoff_left_in(&status, now).is_none());
        assert_eq!(state_in(&status, true, None, now).0, "waiting", "never started");
        let why = fail_in(&status, "the fast tier's server exited while loading the model".to_string(), now);
        let (left, said) = backoff_left_in(&status, now + Duration::from_secs(1)).expect("left alone");
        assert_eq!(left, BACKOFF - Duration::from_secs(1), "five minutes from the failure");
        assert_eq!(said, why);
        assert_eq!(BACKOFF, Duration::from_secs(300));
        let (word, said) = state_in(&status, true, None, now + Duration::from_secs(10));
        assert_eq!((word, said.as_deref()), ("failed", Some(why.as_str())));
        // Past the time the call goes through again, and the status says that the next call tries it.
        let later = now + BACKOFF + Duration::from_secs(1);
        assert!(backoff_left_in(&status, later).is_none());
        let (word, said) = state_in(&status, true, None, later);
        assert_eq!(word, "waiting");
        assert!(said.is_some_and(|s| s.contains("tried again at the next call")));
        assert_eq!(status.lock().unwrap().failures, 1);
    }

    #[test]
    fn the_status_names_no_path_no_port_and_no_key() {
        let text = status().to_string();
        for needle in ["127.0.0.1", "\\", "C:", "key", "LLAMA_API"] {
            assert!(!text.contains(needle), "{needle} is in the status: {text}");
        }
    }
}

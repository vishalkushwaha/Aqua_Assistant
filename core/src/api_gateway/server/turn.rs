//! One turn at a time (card OA3-06, moved out of `server.rs`): the permit of a turn (`take_turn`), the runner of a turn's blocking work with its deadline and its cancel flag (`run_turn`), and the longest a turn may take (`turn_max`).

use super::*;

/// How long a request waits its turn before being told Aqua is busy. Tasks given while another is running queue up and
/// run one after the other, so this is long: it only guards against a queue that has stopped moving.
const TURN_WAIT_SECS: u64 = 900;

/// One voice or text turn runs at a time: they share the language model, the speech engines and one CPU, so
/// running them side by side only makes each slower. Waiting requests that the caller has given up on are
/// dropped by the framework before they ever start, so abandoned work cannot pile up.
pub(super) fn turn_gate() -> Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))).clone()
}

/// A turn: the permit to use the model for a request, and the note for the background runs that a person is being served (they wait at their next
/// step, see `queue`). Both go when the turn does.
pub(in crate::api_gateway) struct Turn {
    _permit: tokio::sync::OwnedSemaphorePermit,
    _foreground: crate::queue::Foreground,
}

pub(in crate::api_gateway) async fn take_turn() -> Result<Turn, axum::response::Response> {
    take_turn_within(std::time::Duration::from_secs(TURN_WAIT_SECS)).await
}

/// [`take_turn`] that waits as long as `wait` for its turn: a run that is detached from any connection (card OA10-03, UX-03) queues behind the run in front for as long as that takes.
pub(in crate::api_gateway) async fn take_turn_within(wait: std::time::Duration) -> Result<Turn, axum::response::Response> {
    let gate = turn_gate();
    let _waiting = super::super::turn_control::waiting(); // counted as queued until it has its turn (or gives up)
    match tokio::time::timeout(wait, gate.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(Turn { _permit: permit, _foreground: crate::queue::foreground() }),
        _ => Err(api_error_code(axum::http::StatusCode::SERVICE_UNAVAILABLE, super::super::contract::ErrorCode::Busy, "Aqua is busy with another request. Try again in a moment.", super::super::contract::ErrorCode::Busy.hint())),
    }
}

/// How much longer than the agent's own clock a turn may take before it is cut off (card OA4-03, UX-09): the clock stops a run at its next round, the turn's limit is for a turn that is
/// stuck inside one (a model call that does not return).
const TURN_GRACE_SECS: u64 = 30;

/// The longest one turn may take from start to finish, however many model calls and helper programs it makes: the longest the agent's own clock lets a run go (900 s for a typed request,
/// `AQUA_AGENT_MAX_SECS` up to 7,200 s) plus [`TURN_GRACE_SECS`]. Each helper program has its own, shorter deadline; this is the backstop that keeps one stuck turn from holding
/// the single turn permit (and so every later request) indefinitely. It was a fixed 600 s, below the 900 s that a typed run is allowed, so no run could use what it was given.
/// The cap of the person's autonomy settings (`max_seconds`) is not part of it: it is read before every call of the run and ends the run itself, as `stopped_by: max_seconds`; added here
/// it would make this backstop two hours long.
pub(super) fn turn_max() -> std::time::Duration {
    turn_max_for(crate::modules::browser_agent::longest_agent_run())
}

pub(super) fn turn_max_for(agent_clock: std::time::Duration) -> std::time::Duration {
    agent_clock + std::time::Duration::from_secs(TURN_GRACE_SECS)
}

/// Run one turn's blocking work off the async runtime, with the overall deadline. The caller keeps the turn permit
/// for as long as it wants the queue held (until the session has been updated), so it is not passed in.
///
/// The work is a blocking thread that cannot be killed, so it is told to stop instead: if this future is dropped
/// before the work finishes (the caller hung up, or the deadline passed) the turn's cancel flag trips and an agent
/// run ends at its next step rather than driving the browser or desktop for nobody.
pub(in crate::api_gateway) async fn run_turn<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    run_turn_timed(work).await.map(|(result, _)| result)
}

/// [`run_turn`], also returning where the time of the turn went (see `turn_timing`).
pub(super) async fn run_turn_timed<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<(T, Vec<crate::turn_timing::StageMs>), String> {
    let stages = Arc::new(Mutex::new(Vec::new()));
    let stages_for_thread = stages.clone();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut guard = CancelOnDrop { flag: cancel.clone(), armed: true };
    // The turn before this one may still be winding down: it was told to stop but its thread cannot be killed. Waiting here
    // keeps two turns from ever running at the same time, whether the last one finished, was stopped or was hung up on.
    let Ok(work_permit) = work_gate().acquire_owned().await else {
        return Err("the turn could not start".to_string());
    };
    let for_thread = cancel.clone();
    let registered = cancel.clone();
    let job = tokio::task::spawn_blocking(move || {
        let _held = work_permit; // released only when the work has really ended
        let _running = super::super::turn_control::register(registered);
        let _bound = crate::modules::browser_agent::bind_turn_cancel(for_thread);
        // Dropped before the permit above: the stages are in place when the request is told the work has ended.
        let _recording = crate::turn_timing::Recording::start(stages_for_thread);
        work()
    });
    let limit = turn_max();
    let outcome = tokio::time::timeout(limit, job).await;
    if outcome.is_ok() {
        guard.armed = false;
    }
    match outcome {
        Err(_) => Err(format!(
            "the turn timed out after {}s, which is the longest a run may take ({}s, set by AQUA_AGENT_MAX_SECS, at most 7200s) plus {TURN_GRACE_SECS}s. A longer task can run in the background (background: true on the agent request)",
            limit.as_secs(),
            limit.as_secs().saturating_sub(TURN_GRACE_SECS)
        )),
        Ok(Err(e)) => Err(format!("the request stopped unexpectedly: {e}")),
        // Stop was pressed: whatever the work made is dropped, so a half-finished answer is never spoken.
        Ok(Ok(_)) if cancel.load(std::sync::atomic::Ordering::SeqCst) => Err("stopped: the task was cancelled".to_string()),
        Ok(Ok(result)) => result.map(|r| (r, std::mem::take(&mut *stages.lock().unwrap_or_else(PoisonError::into_inner)))),
    }
}

/// One turn's work at a time, counted from the moment its thread starts until it ends (see `run_turn`).
fn work_gate() -> Arc<tokio::sync::Semaphore> {
    static GATE: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))).clone()
}

/// Trips a turn's cancel flag unless the turn finished normally.
struct CancelOnDrop {
    flag: Arc<std::sync::atomic::AtomicBool>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

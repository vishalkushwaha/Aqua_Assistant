//! The one model server of this process: the handle to its child, the lock every request holds, the glance that never waits for it, and how it is looked at, saved, stopped, reaped when idle and shut down (move only: this was part of `llama_server.rs`).

use super::*;

pub(super) struct Running {
    pub(super) child: Child,
    pub(super) _job: Option<Contained>,
    pub(super) port: u16,
    pub(super) key: String,
    pub(super) model: PathBuf,
    pub(super) last_used: Instant,
    /// The graphics card the model is on ("NVIDIA GeForce GTX 1650 (Vulkan1, 3561 MiB free)"), or `None` on the processor.
    pub(super) gpu: Option<String>,
}

pub(super) static SERVER: Mutex<Option<Running>> = Mutex::new(None);
static REAPER: OnceLock<()> = OnceLock::new();

/// What may be asked about the server without waiting for it. `SERVER` is held for the whole of every request (up to its time limit) and
/// for the 15 to 40 seconds of a model load, and the status page asks every 5 seconds: asking the lock for the card blocked a thread of the
/// web server until the answer was finished, and the page showed nothing new for as long. Only ever held for an instant.
pub(super) static GLANCE: Mutex<Option<Option<String>>> = Mutex::new(None);

/// Note what the server is now: `None` when there is none, else the card it is on (`None` inside for the processor).
pub(super) fn show(running: Option<&Running>) {
    *GLANCE.lock().unwrap_or_else(|e| e.into_inner()) = running.map(|r| r.gpu.clone());
}

/// The experiment switches the running server was started with ("kv=q8_0, fa=on"); empty when none is on, which is the usual case. See [`experiments_in_use`].
pub(super) static EXPERIMENTS_IN_USE: Mutex<String> = Mutex::new(String::new());

/// Which of the speed-experiment switches (`AQUA_LLAMA_KV`, `AQUA_LLAMA_FA`, `AQUA_LLAMA_SPEC`, `AQUA_LLAMA_THINK_BUDGET`; card R3-01) the running model server was started with,
/// as one line ("kv=q8_0, fa=on"), or an empty text when none is on or no server runs. Only held for an instant, so it never waits for a request.
pub fn experiments_in_use() -> String {
    EXPERIMENTS_IN_USE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub(super) fn stop(running: Option<Running>) {
    show(None);
    EXPERIMENTS_IN_USE.lock().unwrap_or_else(|e| e.into_inner()).clear();
    if let Some(mut r) = running {
        let _ = r.child.kill();
        let _ = r.child.wait();
        info!("llama-server stopped");
    }
}

/// Save the slot of a server that is about to be stopped on purpose or because it is idle, when `AQUA_LLAMA_SLOT_SAVE` is on and a start of the agent's prompt is in it (card OA9-06; `kv_slots.rs`).
/// With the switch off this reads the switch and does nothing. It never fails: a save that cannot be made is one line of the log.
fn save_slot_before_stopping(running: Option<&Running>) {
    if let Some(r) = running {
        super::kv_slots::save_on_stop(r.port, &r.key, &r.model);
    }
}

/// Stop the server now (tests, and anything that needs the memory back). This is the stop on purpose: Aqua stopping, the model unloaded, a model imported or removed.
pub fn shutdown() {
    // The second server of the fast tier first (card OA9-06): a process that is not started unless AQUA_FAST_TIER is on, and that must never outlive Aqua or the model.
    super::fast_server::shutdown();
    let taken = SERVER.lock().unwrap_or_else(|e| e.into_inner()).take();
    save_slot_before_stopping(taken.as_ref());
    stop(taken);
}

/// The port of the running model server, when there is one and it is not busy with a request (never waits for one): the second server of the fast tier keeps off it.
pub(in crate::modules::llm_router) fn port_now() -> Option<u16> {
    let guard = SERVER.try_lock().ok()?;
    guard.as_ref().map(|r| r.port)
}

/// The graphics card the model server is running on ("NVIDIA GeForce GTX 1650 (Vulkan1, 3561 MiB free)"), or `None` when it runs on
/// the processor or no server is running.
///
/// Never waits for a request that is being answered (see [`GLANCE`]): then it says what was last noted.
pub fn gpu_in_use() -> Option<String> {
    match SERVER.try_lock() {
        Ok(g) => g.as_ref().and_then(|r| r.gpu.clone()),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().as_ref().and_then(|r| r.gpu.clone()),
        Err(std::sync::TryLockError::WouldBlock) => GLANCE.lock().unwrap_or_else(|e| e.into_inner()).clone().flatten(),
    }
}

/// The port, the key and the model file of the running server, when there is one and it is not busy with a request (never waits for one). For the experiment functions of
/// `kv_slots.rs` only: the key is the server's own, made for this run, and goes to nothing but that server.
pub(crate) fn endpoint_now() -> Option<(u16, String, PathBuf)> {
    let guard = SERVER.try_lock().ok()?;
    let running = guard.as_ref()?;
    Some((running.port, running.key.clone(), running.model.clone()))
}

/// The process id of the running model server (tests).
#[cfg(test)]
pub(super) fn server_pid() -> Option<u32> {
    SERVER.lock().unwrap_or_else(|e| e.into_inner()).as_ref().map(|r| r.child.id())
}

/// Is a server running right now? While a request is being answered or a model is loading it is (or is about to be), and this does not
/// wait for the end of that (see [`GLANCE`]).
pub fn is_running() -> bool {
    match SERVER.try_lock() {
        Ok(mut g) => g.as_mut().is_some_and(|r| r.child.try_wait().ok().flatten().is_none()),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().as_mut().is_some_and(|r| r.child.try_wait().ok().flatten().is_none()),
        Err(std::sync::TryLockError::WouldBlock) => true,
    }
}

/// Is the model server free this moment (no request is being served by it)? A caller that must not wait for the model asks first.
pub fn is_free() -> bool {
    !matches!(SERVER.try_lock(), Err(std::sync::TryLockError::WouldBlock))
}

pub(super) fn start_reaper() {
    REAPER.get_or_init(|| {
        let _ = std::thread::Builder::new().name("llama-server-reaper".into()).spawn(|| loop {
            std::thread::sleep(Duration::from_secs(15));
            reap_once(idle_timeout());
        });
    });
}

/// One pass of the reaper: a server that exited by itself is dropped, and one that has been idle for `idle_after` is stopped to give the model's memory back (its slot is saved first when
/// `AQUA_LLAMA_SLOT_SAVE` is on: the lock is held, so no request is in the middle of it). A server that died or was stopped because it failed is never asked to save anything.
fn reap_once(idle_after: Duration) {
    let mut g = SERVER.lock().unwrap_or_else(|e| e.into_inner());
    let dead = g.as_mut().is_some_and(|r| r.child.try_wait().ok().flatten().is_some());
    if dead && g.as_ref().is_some_and(|r| r.gpu.is_some()) {
        super::gpu::note_failure("the model server on the graphics card exited by itself");
    }
    let idle = g.as_ref().is_some_and(|r| r.last_used.elapsed() >= idle_after);
    if dead || idle {
        if idle {
            info!("llama-server idle for {}s: stopping it to free the model's memory", idle_after.as_secs());
        }
        let taken = g.take();
        if idle && !dead {
            save_slot_before_stopping(taken.as_ref());
        }
        stop(taken);
    }
}

/// Stops the server if it has been idle for `idle_after` (what the reaper does every 15 seconds with `AQUA_LLAMA_IDLE_SECS`): for a test that cannot wait for the reaper.
#[cfg(any(test, feature = "test-support"))]
pub fn stop_if_idle_for(idle_after: Duration) {
    reap_once(idle_after);
}

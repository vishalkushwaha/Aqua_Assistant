//! Starting the model server: on the graphics card or on the processor, with the arguments the launch has always passed and the switches of the speed experiments, in a job that ends with Aqua, until it answers its health check; and the check that a server a request was hung up on is idle and well (move only: this was part of `llama_server.rs`).

use super::*;

/// Start the model server: on the graphics card when there is a GPU build and a card worth using (see `gpu`), and on the processor
/// when there is not, or when the card cannot be used (it is then left alone for a while, and the request is not lost).
pub(super) fn spawn_server(model: &Path, on_tick: &mut dyn FnMut(u64)) -> Result<Running, Failure> {
    if let Some(plan) = super::gpu::plan() {
        info!("starting llama-server on the GPU: {}", plan.device.describe());
        match launch(model, &mut *on_tick, Some(&plan)) {
            Ok(running) => return Ok(running),
            Err(Failure::Unavailable(why)) => super::gpu::note_failure(&why),
            Err(other) => return Err(other),
        }
    }
    launch(model, on_tick, None)
}

fn launch(model: &Path, on_tick: &mut dyn FnMut(u64), gpu: Option<&super::gpu::Plan>) -> Result<Running, Failure> {
    let unavailable = |m: String| Failure::Unavailable(m);
    let bin = match gpu {
        Some(plan) => plan.binary.clone(),
        None => server_binary().ok_or_else(|| unavailable("llama-server was not found next to llama-cli".into()))?,
    };
    let port = free_port().map_err(|e| unavailable(e.to_string()))?;
    let key = random_key();
    // How much of the model goes to the card. `AQUA_LLAMA_NGL` wins. Otherwise 24 layers on the card at a window of 2048 (measured on the
    // 4 GB GTX 1650: it boots at about 3.4 GB and writes faster than letting the fit pick ~13 layers), fewer at a larger window (see `default_gpu_layers`: 22 at the
    // default 6144, 3.6 GB), and none on the processor. `--fit-target` is still passed; this llama-server keeps an explicit `-ngl`.
    let ctx = context_tokens();
    let ngl = Some(layers_for_launch(gpu.is_some(), std::env::var("AQUA_LLAMA_NGL").ok(), ctx, profile_gpu_layers(model)));
    // Errors only, to a file that starts empty each time: "exited while loading" can then say why (a bad flag, not
    // enough memory). Prompts are only logged at debug level, which is never asked for. The key travels in the
    // environment, not on the command line, where any program of this user could read it from the process list.
    let log_file = server_log_path();
    if let Some(dir) = log_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::remove_file(&log_file);
    // The speed-experiment switches (card R3-01), all off unless the environment says otherwise. What the build has is read from its `--help` only when one is on, so the usual launch runs
    // nothing it did not run before.
    let (switches, warnings) = super::launch_args::parse_switches(&super::launch_args::raw_switches());
    for w in &warnings {
        warn!("llama-server experiment switch: {w}");
    }
    // The switches that came after them (card OA9-06; `server_options.rs`): the same rule, off unless the environment says otherwise, and the help of the build is read only when one is on.
    let draft_raw = super::server_options::raw_extras();
    let (mut more, more_refused) = super::server_options::parse_extras(&draft_raw);
    // Native tool calls (card OA9-06): the model of this launch is spoken to that way when the switch or its profile says so, and the server then needs its own templates.
    more.native_tools = crate::model::native_tools::mode_for_path(model) == crate::model::ToolMode::Native;
    for w in &more_refused {
        warn!("llama-server experiment switch: {}", w.log_line());
    }
    let caps = if switches.is_default() && more.is_default() { super::launch_args::Capabilities::default() } else { super::launch_args::capabilities_of(&bin) };
    // One card only: the integrated graphics share the processor's memory, and using both made writing slower.
    let card_flags = gpu.map(|plan| super::gpu::server_flags(&plan.device)).unwrap_or_default();
    let launched = super::server_options::full_launch(
        &super::launch_args::Values {
            model,
            port,
            window: ctx,
            threads: &threads(),
            threads_batch: &threads_batch(),
            slots: slots(),
            log_file: &log_file,
            layers: ngl.as_deref(),
            card: gpu.map(|plan| (plan.device.id.as_str(), card_flags.as_slice())),
        },
        &switches,
        &more,
        &caps,
        &|| super::kv_slots::ensure_slot_dir(&crate::runtime::data_dir()),
        &|| super::draft_model::check_file(draft_raw.draft_model.as_deref(), draft_raw.draft_folder.as_deref(), model.parent().unwrap_or(Path::new(".")), Some(model)),
    );
    for note in &launched.notes {
        warn!("llama-server experiment switch: {note}");
    }
    let mut cmd = Command::new(&bin);
    cmd.args(&launched.args).env("LLAMA_API_KEY", &key).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = cmd.spawn().map_err(|e| unavailable(format!("could not start {}: {e}", bin.display())))?;
    // The server ends when Aqua does: the job is closed with this process.
    // A server that is not in the job would survive a crash of this process as an orphan holding about 5 GB, so one that
    // cannot be contained is not kept.
    let job = match Contained::attach(&child, Limits { memory_mb: 20_480, max_processes: 8 }) {
        Ok(j) => Some(j),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(unavailable(format!("llama-server could not be placed under Aqua's process job ({e}), so it was not kept running")));
        }
    };
    // Loading the model takes seconds when it is in the file cache and up to a minute or two when not.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(240);
    let health = format!("http://127.0.0.1:{port}/health");
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(unavailable(format!("llama-server exited while loading the model ({status}){}", log_tail(&log_file))));
        }
        match ureq::get(&health).timeout(Duration::from_secs(3)).call() {
            Ok(r) if r.status() == 200 => break,
            _ => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(unavailable("llama-server did not become ready within 4 minutes".into()));
        }
        on_tick(started.elapsed().as_secs());
        std::thread::sleep(Duration::from_millis(500));
    }
    let experiments = launched.applied.join(", ");
    super::server_options::note_launch(&launched.applied, &launched.notes);
    THINKING_ALLOWED.store(launched.thinking, std::sync::atomic::Ordering::SeqCst);
    *EXPERIMENTS_IN_USE.lock().unwrap_or_else(|e| e.into_inner()) = experiments.clone();
    info!(
        "llama-server ready on 127.0.0.1:{port} after {}s (model {}), {}{}",
        started.elapsed().as_secs(),
        model.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
        gpu.map_or_else(|| "on the processor".to_string(), |p| format!("on the GPU: {}", p.device.describe())),
        if experiments.is_empty() { String::new() } else { format!("; speed-experiment switches on: {experiments}") }
    );
    // The saved state of the agent's prompt start, when `AQUA_LLAMA_SLOT_SAVE` is on and there is a file for this model (card OA9-06; `kv_slots.rs`). Nothing here can fail the start.
    super::kv_slots::restore_on_start(port, &key, model);
    Ok(Running { child, _job: job, port, key, model: model.to_path_buf(), last_used: Instant::now(), gpu: gpu.map(|p| p.device.describe()) })
}

/// After a request was hung up: is the server well and idle, so that it can keep its model and its caches? A server stops writing
/// within a second or two of the connection closing (longer when the machine is busy: a test run with the processor shared by dozens
/// of tests needed more than 4 seconds). One that still reports a busy slot after 10 seconds, or does not answer its health check,
/// is stuck. (A build without the slots endpoint is judged by its health check alone.)
pub(super) fn idle_and_healthy(port: u16, key: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let health = ureq::get(&format!("http://127.0.0.1:{port}/health")).timeout(Duration::from_secs(3)).call();
        if !matches!(health, Ok(ref r) if r.status() == 200) {
            return false;
        }
        match ureq::get(&format!("http://127.0.0.1:{port}/slots")).set("Authorization", &format!("Bearer {key}")).timeout(Duration::from_secs(3)).call() {
            Ok(r) => {
                let busy = r
                    .into_string()
                    .ok()
                    .and_then(|body| serde_json::from_str::<Value>(&body).ok())
                    .and_then(|slots| slots.as_array().map(|all| all.iter().any(|s| s["is_processing"].as_bool().unwrap_or(false))))
                    .unwrap_or(false);
                if !busy {
                    return true;
                }
            }
            Err(_) => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

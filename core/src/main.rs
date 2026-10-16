use anyhow::Result;
use aqua_core::api_gateway::auth::ApiAuth;
use aqua_core::api_gateway::server::run_api_gateway_on;
use aqua_core::api_gateway::{run_api_gateway, AppState};
use aqua_audio_adapter::{KokoroTtsEngine, NoiseSuppressor, VoiceBiometricsEngine, WhisperSttEngine};
use aqua_core::laya_validator::LayaGate;
use aqua_core::runtime::{default_models_dir, inspect_models};
use aqua_core::modules::llm_router::LlamaRouterEngine;
use aqua_core::startup_health::{
    ensure_workspace_dirs, run_startup_health, ComponentHealth, ManagedServices, StartupStep, StartupTrace,
};
use aqua_core::storage::DatabaseManager;
use aqua_mcp_adapters::browser::BrowserBackend;
use aqua_mcp_adapters::{new_browser_client, new_windows_client};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

/// Another core holds the port: say so and stop, before anything is touched. Exit code 3 is what the desktop shell reads as "do not start it again".
fn port_is_taken(port: u16) -> ! {
    eprintln!("Aqua is already running on port {port}; this copy will not start. Close the other one first, or set AQUA_API_PORT.");
    tracing::error!("port {port} is already in use: another Aqua core is running; exiting without touching anything");
    std::process::exit(3);
}

#[tokio::main]
async fn main() -> Result<()> {
    // The log is set up in one place (`logging`): text lines as before (colour codes only on a terminal), `AQUA_LOG_FORMAT=json` for one JSON object a line, `AQUA_LOG_FILE=<path>` to
    // have the core write a rotated file itself, `RUST_LOG` to choose what is shown. A line written inside a run or a tool call names the run and the call.
    let log_note = aqua_core::logging::init(&aqua_core::logging::LogConfig::from_env());
    if let Err(why) = &log_note {
        eprintln!("the log could not be set up: {why}");
    }
    if let Ok(Some(note)) = &log_note {
        tracing::warn!("{note}");
    }
    // Every panic, on any thread, is logged with the thread's name, the place and a backtrace and recorded in data/crash (see `supervise`).
    aqua_core::supervise::install_panic_hook();
    // Two things the lower layers of the core may not name are wired here, where `modules` and `metrics` are in reach (core/tests/layering_test.rs): how many characters of lines a page of
    // a file may have, which is what the model is shown of a result in the window the server runs with (card OA5-03), and the counts the model router keeps of its retries and its
    // shortened prompts (card OA9-02). Both are set before anything is built or asked.
    aqua_core::tool_providers::set_view_chars_source(|| aqua_core::modules::context::budget::Regions::view_chars(aqua_core::modules::llm_router::llama_server::context_tokens()));
    aqua_core::model::set_counter_hook(aqua_core::metrics::inc);
    info!("Starting Aqua Core Engine v{}", aqua_core::version());

    // 8767 unless `AQUA_API_PORT` says otherwise (a second copy, for testing, must not fight the one already running).
    let port = std::env::var("AQUA_API_PORT").ok().and_then(|p| p.parse::<u16>().ok()).filter(|p| *p >= 1024).unwrap_or(8767);
    // Another core already answering on this port is the one that owns the data folder, the managed browser and the
    // practice runs. Starting up beside it would stop its browser and mark its runs interrupted before this copy found
    // out that it cannot bind, so the check comes before anything is touched.
    // `start` says which step of the start has been reached, and a step out of its order is refused (`startup_health::StartupStep` is that order; card OA3-01).
    let mut start = StartupTrace::new();
    start.note(StartupStep::PortChecked);
    if aqua_core::runtime::port_in_use(port) {
        port_is_taken(port);
    }

    // Recordings that an earlier run left in the temp folder when it was stopped without time to clean up (a thread of its own: the folder can be big).
    // (A job of the registry, `jobs`: listed while it runs, and stopped by the shutdown.)
    let _ = aqua_core::jobs::spawn(aqua_core::jobs::JobKind::Maintenance, "aqua-temp-sweep", None, |_job| {
        aqua_core::temp_sweep::remove_old_voice_scratch();
        Ok(())
    });

    let data_dir = aqua_core::runtime::data_dir();
    // A thread an earlier run gave up is not a fact about this one, and neither is what an earlier run's monitor said about its jobs.
    aqua_core::supervise::forget_failed_threads(&data_dir);
    aqua_core::jobs::forget_record(&data_dir);
    // Every 30 seconds: say once in the log when a job shows no sign of life, and keep the record the doctor reads (`aqua-cli doctor`).
    if let Err(e) = aqua_core::jobs::start_monitor(data_dir.clone()) {
        tracing::warn!("the monitor of the background jobs could not start: {e}");
    }
    // A configuration that won the harness (`scripts/eval/quality.py promote`): its flags are set now, before anything reads them, unless the person set
    // them in the environment already. A wrong file is said and ignored.
    match aqua_core::promoted::apply(&data_dir) {
        Ok(Some((label, applied))) => tracing::info!("promoted configuration {label:?}: set {:?}; left alone {:?}", applied.set, applied.left),
        Ok(None) => {}
        Err(e) => tracing::warn!("the promoted configuration is not used: {e}"),
    }
    // The desktop shell reads the UI token from this folder to talk to the core. The startup checks below can take a
    // minute (the speech engine's smoke test), and until the tokens exist the app would only say "did not start", so
    // they are created first. (`run_api_gateway` loads the same files later.)
    //
    // Bind first (card R3-07): the port is taken here, then the tokens are made, and from then on the port answers "starting" (503, with a body that says
    // STARTING, on /api/v1/health and /api/v1/status) until the gateway is ready and takes over the same socket. Before, nothing answered for the whole
    // start-up and a client was refused, and a second Aqua started in that time got past the check above. It is off unless `AQUA_BIND_FIRST=on` (it has not been seen
    // on a second core yet), and any failure of this part restores the old order. Everything after this block is as it was, in the same order.
    let make_auth = |bound: u16| {
        let api = aqua_core::api_gateway::auth::load_or_create_token(&data_dir)?;
        let ui = aqua_core::api_gateway::auth::load_or_create_ui_token(&data_dir)?;
        Ok::<_, anyhow::Error>(ApiAuth::new(api, bound).with_ui_token(ui))
    };
    let early = if aqua_core::startup_health::early::enabled() {
        start.note(StartupStep::PortTaken);
        match aqua_core::startup_health::early::begin(port, make_auth) {
            Ok(gateway) => gateway,
            Err(_) => port_is_taken(port),
        }
    } else {
        if let Err(e) = make_auth(port) {
            tracing::warn!("could not prepare the API tokens early: {e:#}");
        }
        None
    };
    // Started by the desktop app: stop when that app is gone, however it went (Task Manager, a crash, an upgrade), so
    // no core is left holding the port and the installed files.
    #[cfg(windows)]
    if let Some(parent) = std::env::var("AQUA_PARENT_PID").ok().and_then(|p| p.parse::<u32>().ok()) {
        let every = std::time::Duration::from_secs(3);
        let _ = aqua_core::jobs::spawn_supervised(aqua_core::jobs::JobKind::Watcher, "aqua-parent-watch", Some(every), aqua_core::supervise::Backoff::STANDARD, move |job| {
            while job.sleep(every) {
                if !aqua_mcp_adapters::job::process_is_running(parent) {
                    tracing::warn!("the desktop app (pid {parent}) is gone; stopping the core");
                    // The jobs are told to stop and given one second (not the five of a graceful end: the person has closed the app and is waiting for the files to be free), then
                    // the process leaves, as it did before. (This job is not waited for by itself.)
                    let left = aqua_core::jobs::shutdown(std::time::Duration::from_secs(1));
                    if !left.is_empty() {
                        tracing::warn!("still going when the core left: {}", left.join(", "));
                    }
                    std::process::exit(0);
                }
            }
        });
    }
    // The record of agent runs: runs an earlier process left going are marked interrupted, old ones are pruned. A database that
    // cannot be opened leaves runs unrecorded; it never stops Aqua from starting.
    start.note(StartupStep::AgentRuns);
    if let Err(e) = aqua_core::agent_runs::init(&data_dir) {
        tracing::warn!("agent runs will not be recorded: {e}");
    }
    // The vault (opens, reads nothing until a folder is chosen) and the night's tidying of skills Aqua proposed.
    if let Err(e) = aqua_core::vault::init(&data_dir) {
        tracing::warn!("the vault could not be opened: {e}");
    }
    aqua_core::skills_inbox::spawn_nightly(data_dir.clone(), || aqua_core::queue::foreground_active() == 0);
    // Once a minute: a run that is going but has written nothing for five minutes is marked stalled (one nobody is working on, interrupted).
    let every = std::time::Duration::from_secs(60);
    let _ = aqua_core::jobs::spawn_supervised(aqua_core::jobs::JobKind::Watcher, "aqua-run-watchdog", Some(every), aqua_core::supervise::Backoff::STANDARD, move |job| {
        while job.sleep(every) {
            aqua_core::agent_runs::watchdog_tick();
            // The summaries of the coding sessions that ended well, for the learner (card OA10-04, `AQUA_SESSION_SUMMARY`; nothing without the switch).
            aqua_core::modules::coding_session_record::tick();
        }
    });
    // The desktop audit log of the Windows guard lives with the rest of the tool records, in this one data folder.
    aqua_mcp_adapters::WindowsGuard::init_global(data_dir.join("tools").join("audit.jsonl"));
    // What Aqua has learned about starting apps (and a line in the learning activity log when it learns or unlearns one).
    aqua_mcp_adapters::app_launch::init_global(data_dir.join("learning").join("launch_skills.json"));
    aqua_mcp_adapters::app_launch::set_event_sink(|event, detail| aqua_core::memory::experience::service::service().store().log_event(event, &detail, None));
    // Device and battery monitoring: announces connected devices and low batteries (through the interface's speech). A
    // background thread that waits for Windows to report a change; it costs nothing while nothing changes.
    if let Err(e) = aqua_device_monitor::init_global(data_dir.join("devices")) {
        tracing::warn!("device monitoring could not start: {e:#}");
    }
    // The file index (folders, programs and shortcuts, for "open the Personal AI folder"): a background thread that waits
    // whenever a task is thinking or speaking, and costs nothing while nothing changes. Stored in the data folder only.
    if let Err(e) = aqua_mcp_adapters::file_index::init_global(
        data_dir.join("index"),
        Arc::new(|| aqua_core::api_gateway::turn_control::is_running() || aqua_core::api_gateway::turn_control::waiting_count() > 0),
    ) {
        tracing::warn!("the file index could not start: {e:#}");
    }
    start.note(StartupStep::Database);
    let db_manager = Arc::new(DatabaseManager::new(data_dir)?);
    // Only a label for the status page: a full disk or a locked file must not stop Aqua from starting.
    if let Err(e) = db_manager.set_setting("system_status", "STARTING") {
        tracing::warn!("could not record the startup state (the status page says UNKNOWN until it can): {e:#}");
    }

    let models_dir = default_models_dir();
    ensure_workspace_dirs(&models_dir)?;

    // Pin Laya downloads under data/models/laya before any Laya API use.
    let laya = Arc::new(LayaGate::bootstrap(&models_dir));

    info!("==> Startup health: initializing engines…");
    start.note(StartupStep::NoiseSuppressor);
    let noise = NoiseSuppressor::new(models_dir.join("deepfilternet"))?;
    start.note(StartupStep::Whisper);
    let whisper = WhisperSttEngine::new(models_dir.join("whisper"))?;
    // The model the person chose for requests (after wake-up), if any.
    if let Ok(Some(saved)) = db_manager.get_setting("stt_model_active") {
        whisper.set_active_model(Some(saved));
    }
    start.note(StartupStep::Llama);
    let llm = Arc::new(LlamaRouterEngine::new(models_dir.join("llm"))?);
    if let Ok(Some(saved)) = db_manager.get_setting("llm_active_gguf") {
        if let Err(e) = llm.prefer_model_if_present(&saved) {
            tracing::warn!("Could not restore llm_active_gguf={saved}: {e}");
        }
    }
    // Load the language model and have it read the start of its prompts now, while the speech engine and the other checks below
    // start up (about 8 seconds, in parallel), instead of after the API has bound.
    start.note(StartupStep::Warmup);
    aqua_core::api_gateway::server::start_warmup(laya.clone(), llm.clone());
    start.note(StartupStep::Kokoro);
    let tts = KokoroTtsEngine::new(models_dir.join("kokoro"))?;
    let mut biometrics = VoiceBiometricsEngine::new(models_dir.join("biometrics"))?;

    match db_manager.get_master_voice_embedding() {
        Ok(Some(saved_emb)) => {
            if let Err(e) = biometrics.register_master_fingerprint(saved_emb) {
                warn!("The enrolled voice could not be loaded: {e:#}. Enrol your voice again.");
            }
        }
        Ok(None) => {}
        // The key is missing from the credential store, or the stored value is damaged: say so instead of
        // starting as if nothing had ever been enrolled.
        Err(e) => warn!("The enrolled voice could not be read ({e:#}). Voice requests will be refused while voice security is on; enrol your voice again."),
    }
    {
        let enabled = db_manager.voice_lock_enabled();
        let stored = db_manager.get_setting("master_voice_enrolled").ok().flatten().map(|v| v == "true" || v == "1");
        let status = aqua_core::api_gateway::server::voice_security_status_with_stored(enabled, Some(biometrics.is_model_loaded()), Some(biometrics.is_master_registered()), stored);
        if !status.ready {
            warn!("{}", status.note);
        }
    }

    // Managed browser (Playwright MCP, Aqua profile) by default; Browser MCP extension on request.
    let browser = Arc::new(BrowserBackend::new(&aqua_core::runtime::workspace_root(), new_browser_client()));
    let windows_mcp = Arc::new(Mutex::new(new_windows_client()));
    let managed = Arc::new(ManagedServices::default());
    let inventory = inspect_models(&models_dir);

    // Auto-start MCP unless explicitly disabled.
    let start_mcp = std::env::var("AQUA_START_MCP_ON_BOOT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    info!("==> Startup health: verifying dependencies & microservices…");
    start.note(StartupStep::Health);
    let mut report = run_startup_health(
        &inventory,
        &models_dir,
        &noise,
        &whisper,
        &llm,
        &tts,
        &browser,
        &mut windows_mcp.lock().unwrap_or_else(|e| e.into_inner()),
        &managed,
        start_mcp,
    );

    report.components.push(ComponentHealth {
        name: "laya_gate".into(),
        ok: laya.is_ready() || !laya.is_enabled(),
        detail: format!(
            "{} | inventory_laya={}",
            laya.health_detail(),
            inventory.laya_checkpoints
        ),
        auto_fixed: false,
    });

    for c in &report.components {
        if c.ok {
            info!(
                "[health] OK  {:<18} {}{}",
                c.name,
                c.detail,
                if c.auto_fixed { " (auto-fixed)" } else { "" }
            );
        } else {
            warn!("[health] FAIL {:<18} {}", c.name, c.detail);
        }
    }

    if report.ready {
        let _ = db_manager.set_setting("system_status", "ACTIVE");
        info!("==> Startup health PASSED — accepting voice commands.");
    } else {
        let _ = db_manager.set_setting("system_status", "DEGRADED");
        warn!(
            "==> Startup health DEGRADED — API will start but voice pipeline may fail: {:?}",
            report.blockers
        );
    }

    // The quarantined reader (AQUA_READER=on) asks the same model.
    aqua_core::modules::reader::install(llm.clone());
    let state = AppState {
        models_dir: models_dir.clone(),
        db: db_manager,
        noise: Arc::new(Mutex::new(noise)),
        whisper: Arc::new(whisper),
        llm,
        tts: Arc::new(tts),
        biometrics: Arc::new(Mutex::new(biometrics)),
        tools: {
            let host = aqua_core::tool_host::build_tool_host(&aqua_core::runtime::data_dir(), browser.clone(), windows_mcp.clone());
            // The live output of a call that prints as it runs (card OA7-04): the host cleans it like a result and hands it on, and the streams of a run's events carry it to the app. Nothing is
            // shown unless the tool's own setting is on (`live_output` of term.json); the model never reads it. It is wired here and not in `build_tool_host`, which may not name `agent_runs`.
            host.set_progress_source(Some(aqua_core::agent_runs::live::source()));
            // "Full access" for the project stays on across restarts, for the folder it was given for.
            aqua_core::full_access::restore(&host, aqua_core::tool_host::PROJECT_GUARD.get().map(|g| g.as_ref()), &aqua_core::runtime::data_dir(), &aqua_core::runtime::workspace_root());
            // The agent uses this same host, so its calls are decided, asked, masked and audited like any other.
            aqua_core::agent_tools::install(host.clone());
            host
        },
        browser,
        windows_mcp,
        laya,
        active_session_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        managed_services: managed,
        startup_report: Arc::new(Mutex::new(Some(report))),
    };

    info!(
        "Model inventory path: {:?} (weights downloaded in later steps)",
        state.models_dir
    );

    // The Esc key stops a run that is acting on the screen (card R6-34): the same stop as the Stop button, with the key consumed. It passes untouched when no run is acting.
    // `AQUA_DESKTOP_STOP_KEY=off` leaves the keyboard hook out.
    if !matches!(std::env::var("AQUA_DESKTOP_STOP_KEY").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("off" | "0" | "false")) {
        let speech_session = state.active_session_id.clone();
        match aqua_mcp_adapters::windows_mcp::stop_key::install(move || {
            let stopped = aqua_core::api_gateway::turn_control::stop_current();
            aqua_core::modules::browser_agent::cancel_running_agents();
            speech_session.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            info!("The Esc key stopped the running task (a task was running: {})", stopped.is_some());
        }) {
            Ok(()) => info!("The Esc key stops a run that is acting on the screen"),
            Err(e) => warn!("The Esc key stop is not installed: {e}"),
        }
    }

    // The signed heads of the audit ledger are written every minute and at the end (card R6-12), so the newest lines are never unsigned for long.
    let mut audit_sealer = state.tools.audit().spawn_sealer(std::time::Duration::from_secs(60));

    // The port goes to the real gateway: the same socket, so nobody was refused meanwhile and no other Aqua could take it.
    if early.is_some() {
        start.note(StartupStep::HandOver);
    }
    start.note(StartupStep::Serving);
    let served = match early {
        Some(gateway) => match gateway.hand_over() {
            Ok(listener) => run_api_gateway_on(listener, state).await,
            Err(e) => {
                warn!("the port could not be handed over ({e:#}); the gateway binds it again");
                run_api_gateway(port, state).await
            }
        },
        None => run_api_gateway(port, state).await,
    };
    // A graceful end stops the background jobs first, all together and for at most five seconds (a job inside a long call cannot be interrupted: it is named, not waited for), then the
    // seal of the audit log (so that the last lines the jobs wrote are signed), then the model server.
    let left = aqua_core::jobs::shutdown(std::time::Duration::from_secs(5));
    if !left.is_empty() {
        warn!("still going after the grace of five seconds: {}", left.join(", "));
    }
    aqua_core::jobs::forget_record(&aqua_core::runtime::data_dir());
    audit_sealer.stop();
    // A graceful end stops the model server too (a crash is covered by its process job).
    aqua_core::modules::llm_router::llama_server::shutdown();
    served?;
    info!("Aqua Core Backend Engine stopped.");
    Ok(())
}

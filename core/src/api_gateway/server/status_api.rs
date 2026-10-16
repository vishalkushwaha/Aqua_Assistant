//! The status routes (card OA3-06, moved out of `server.rs`): `/status`, `/health`, `/startup/health`, `/pipeline/health`, the status of the gate (`/laya/status`), and what the gate would do with a task (`/task/dispatch`).

use super::*;

#[derive(Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: String,
    pub version: String,
    /// SLA target documented in architecture (not live RSS yet).
    pub ram_sla_mb: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_usage_mb: Option<u32>,
    pub voice_standby_active: bool,
    pub database_ready: bool,
    /// How startup ended: `ACTIVE` (everything required was ready), `DEGRADED` (something required was not), or
    /// `STARTING`. `status` only says the process is running.
    pub startup_state: String,
    /// Whether speaker verification can do its job (with voice security on and no model or enrolment, every voice
    /// request is refused even though the pipeline itself is ready).
    pub voice_security: VoiceSecurityStatus,
    pub biometric_enrolled: bool,
    pub noise_engine_ready: bool,
    pub whisper_engine_ready: bool,
    pub llm_engine_ready: bool,
    /// The graphics card the language model is running on ("NVIDIA GeForce GTX 1650 (Vulkan1, 3561 MiB free)"); left out when it
    /// runs on the processor or is not loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_gpu: Option<String>,
    /// Present while the graphics card is being left alone after a failure, with why and until when (unix seconds): the language model runs on the processor
    /// meanwhile, three to five times slower, and without this nothing but the log said so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_gpu_off: Option<crate::modules::llm_router::gpu::Off>,
    /// The background threads that panicked five times in ten minutes and were stopped (the flow scheduler that fires reminders, the history prune, the skill tidy, the run
    /// watchdog); empty while all is well. A restart starts them again. See `supervise`.
    #[serde(default)]
    pub background_failed: Vec<String>,
    /// The model each role uses and why (card OA9-03): `GET /api/v1/models/roles` says the same with the models in the folder and the warnings.
    #[serde(default)]
    pub llm_roles: Vec<crate::modules::llm_router::RoleView>,
    pub tts_engine_ready: bool,
    pub pipeline_ready: bool,
    pub text_pipeline_ready: bool,
    pub models: ModelsInventory,
}

// The answer of `GET /api/v1/health`, the one route that needs no token (the start-up scripts and the doctor ask it before they have one). To a caller that showed none it says that
// Aqua is up and which version, and nothing about the machine; a caller that holds a token gets the whole status, as `GET /api/v1/status` gives it (card OA1-04, SE-19).
async fn get_health(State(state): State<AppState>, axum::Extension(shown): axum::Extension<super::super::auth::TokenShown>) -> Response {
    if shown.0 {
        get_status(State(state)).await.into_response()
    } else {
        Json(minimal_health()).into_response()
    }
}

pub(super) fn minimal_health() -> serde_json::Value {
    serde_json::json!({ "status": "RUNNING", "version": env!("CARGO_PKG_VERSION") })
}

async fn get_status(State(state): State<AppState>) -> Json<StatusResponse> {
    // Never block the async runtime on long-held pipeline locks.
    let models = inspect_models(&state.models_dir);
    let stored_enrolled = state
        .db
        .get_setting("master_voice_enrolled")
        .ok()
        .flatten()
        .is_some_and(|v| v == "true" || v == "1");

    // While a turn holds the noise engine the answer is the last one seen, not a guess that it is fine.
    static LAST_NOISE_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let noise_engine_ready = match state.noise.try_lock() {
        Ok(n) => {
            let ready = n.is_engine_ready();
            LAST_NOISE_READY.store(ready, std::sync::atomic::Ordering::Relaxed);
            ready
        }
        Err(_) => LAST_NOISE_READY.load(std::sync::atomic::Ordering::Relaxed),
    };
    let database_ready = state.db.get_setting("system_status").is_ok();
    let startup_state = state.db.get_setting("system_status").ok().flatten().unwrap_or_else(|| "UNKNOWN".to_string());
    let security_enabled = state.db.voice_lock_enabled();
    let (model_present, enrolled) = match state.biometrics.try_lock() {
        Ok(b) => (Some(b.is_model_loaded()), Some(b.is_master_registered())),
        Err(_) => (None, None),
    };
    // "Enrolled" means a voice that is actually loaded and can be checked, not only a flag in the database.
    let biometric_enrolled = enrolled.unwrap_or(stored_enrolled);

    let whisper_engine_ready = state.whisper.is_engine_ready();
    let llm_engine_ready = state.llm.is_engine_ready();
    let tts_engine_ready = state.tts.is_engine_ready();

    let health = match state.noise.try_lock() {
        Ok(noise) => collect_stage_health(&noise, &state.whisper, &state.llm, &state.tts),
        Err(_) => PipelineStageHealth {
            stages: vec![],
            pipeline_ready: whisper_engine_ready && llm_engine_ready && tts_engine_ready,
            text_pipeline_ready: llm_engine_ready && tts_engine_ready,
        },
    };

    Json(StatusResponse {
        status: "RUNNING".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        ram_sla_mb: 200,
        ram_usage_mb: None,
        // No wake-word or standby loop is wired into the core, so this is false because it is false.
        voice_standby_active: false,
        database_ready,
        startup_state,
        voice_security: voice_security_status_with_stored(security_enabled, model_present, enrolled, Some(stored_enrolled)),
        biometric_enrolled,
        noise_engine_ready,
        whisper_engine_ready,
        llm_engine_ready,
        llm_gpu: crate::modules::llm_router::llama_server::gpu_in_use(),
        llm_gpu_off: crate::modules::llm_router::gpu::off_status(),
        background_failed: crate::supervise::background_failed(),
        llm_roles: state.llm.role_choices().iter().map(|c| c.view()).collect(),
        tts_engine_ready,
        pipeline_ready: health.pipeline_ready,
        text_pipeline_ready: health.text_pipeline_ready,
        models,
    })
}

pub(super) fn pipeline_health_from_state(state: &AppState) -> PipelineStageHealth {
    match state.noise.try_lock() {
        Ok(noise) => collect_stage_health(&noise, &state.whisper, &state.llm, &state.tts),
        Err(_) => PipelineStageHealth {
            stages: vec![],
            pipeline_ready: state.whisper.is_engine_ready()
                && state.llm.is_engine_ready()
                && state.tts.is_engine_ready(),
            text_pipeline_ready: state.llm.is_engine_ready() && state.tts.is_engine_ready(),
        },
    }
}

/// Whether speaker verification can do its job. With voice security on (the default) and no voiceprint model,
/// every voice request is refused, so this says so instead of letting the pipeline look healthy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VoiceSecurityStatus {
    pub enabled: bool,
    pub model_present: Option<bool>,
    pub enrolled: Option<bool>,
    pub ready: bool,
    pub note: String,
}

pub fn voice_security_status(enabled: bool, model_present: Option<bool>, enrolled: Option<bool>) -> VoiceSecurityStatus {
    voice_security_status_with_stored(enabled, model_present, enrolled, None)
}

/// Like [`voice_security_status`], also given whether the database says a voice was enrolled. A voice that is
/// recorded as enrolled but could not be loaded (its key is missing from the credential store, or the data is
/// damaged) is a different problem from never having enrolled, and the fix is the same, but the person should be told.
pub fn voice_security_status_with_stored(enabled: bool, model_present: Option<bool>, enrolled: Option<bool>, stored_says_enrolled: Option<bool>) -> VoiceSecurityStatus {
    let (ready, note) = match (enabled, model_present, enrolled) {
        (false, _, _) => (true, "Voice security is off: any speaker is accepted.".to_string()),
        (true, Some(false), _) => (false, "Voice security is on but the speaker model is missing (data/models/biometrics/speaker_wespeaker_resnet34_lm.onnx; run scripts/fetch_speaker_model.ps1), so every voice request is refused. Add the model or turn voice security off.".to_string()),
        (true, Some(true), Some(false)) if stored_says_enrolled == Some(true) => (false, "Voice security is on and a voice is recorded as enrolled, but it could not be read (the encryption key is missing from the Windows credential store, or the data is damaged), so every voice request is refused. Enrol your voice again.".to_string()),
        (true, Some(true), Some(false)) => (false, "Voice security is on but no voice is enrolled, so every voice request is refused. Enrol your voice first.".to_string()),
        (true, Some(true), Some(true)) => (true, "Speaker verification is ready.".to_string()),
        (true, _, _) => (true, "Speaker verification is busy; status unknown.".to_string()),
    };
    VoiceSecurityStatus { enabled, model_present, enrolled, ready, note }
}

#[derive(Serialize)]
pub struct PipelineHealthResponse {
    #[serde(flatten)]
    pub health: PipelineStageHealth,
    pub voice_security: VoiceSecurityStatus,
}

async fn pipeline_health_endpoint(State(state): State<AppState>) -> Json<PipelineHealthResponse> {
    let enabled = state.db.voice_lock_enabled();
    let (model_present, enrolled) = match state.biometrics.try_lock() {
        Ok(b) => (Some(b.is_model_loaded()), Some(b.is_master_registered())),
        Err(_) => (None, None),
    };
    let stored = state.db.get_setting("master_voice_enrolled").ok().flatten().map(|v| v == "true" || v == "1");
    Json(PipelineHealthResponse { health: pipeline_health_from_state(&state), voice_security: voice_security_status_with_stored(enabled, model_present, enrolled, stored) })
}

async fn startup_health_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    let report = state
        .startup_report
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match report {
        // The engine checks are repeated on every request, so this never says "ready" about a model that is gone.
        Some(r) => Json(r.rechecked(state.whisper.is_engine_ready(), state.llm.is_engine_ready(), state.tts.is_engine_ready())).into_response(),
        None => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "startup health not available yet" })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct TaskRequest {
    pub task_id: String,
    pub prompt: String,
}

async fn dispatch_task(
    State(state): State<AppState>,
    Json(payload): Json<TaskRequest>,
) -> Json<serde_json::Value> {
    info!("Dispatched task via API: ID={}", payload.task_id);
    // Assume tools may be available for triage (dispatch is pre-agent).
    // Laya can take a minute in debug builds: run it off the runtime with the usual time budget.
    let laya = state.laya.clone();
    let prompt = payload.prompt.clone();
    let decision = match tokio::task::spawn_blocking(move || {
        laya.decide_with_timeout(&prompt, true, LayaGate::gate_timeout(), |_| {})
    })
    .await
    {
        Ok(d) => d,
        Err(e) => {
            return Json(serde_json::json!({ "status": "ERROR", "error": e.to_string() }));
        }
    };
    let estimated = match decision.route {
        GateRoute::Refuse => decision.latency_ms.max(20),
        GateRoute::Chat => 2_000 + decision.latency_ms,
        GateRoute::McpAgent => 60_000 + decision.latency_ms,
    };
    Json(serde_json::json!({
        "status": "ACCEPTED",
        "task_id": payload.task_id,
        "route": decision.route.as_str(),
        "needs_tools": decision.needs_tools,
        "surface": decision.surface.as_str(),
        "gate": decision,
        "estimated_latency_ms": estimated,
    }))
}

async fn laya_status_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.laya.status_json())
}

// `POST /api/v1/laya/feedback` (the routing feedback buttons), its rate and the calibration check are in `routing_feedback_api.rs`.

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/status", get(get_status))
        .route("/api/v1/health", get(get_health))
        .route("/api/v1/startup/health", get(startup_health_endpoint))
        .route("/api/v1/models/status", get(get_status))
        .route("/api/v1/task/dispatch", post(dispatch_task))
        .route("/api/v1/pipeline/health", get(pipeline_health_endpoint))
        .route("/api/v1/laya/status", get(laya_status_endpoint))
}

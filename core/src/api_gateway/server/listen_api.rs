//! Listening while idle (card OA3-06, moved out of `server.rs`): the wake check, the spoken stop and the choice of the speech models that listen (`/voice/wake-check`, `/voice/stop-check`, `/voice/stt-models`).

use super::*;

/// The model used to listen for the wake phrase: smaller and several times faster than the one that transcribes requests
/// (about 1.4 s against 4.9 s for 2.5 s of speech on a typical PC), because it runs on whatever the microphone hears while
/// the orb is idle. `AQUA_WAKE_MODEL` names another file in the Whisper models folder; a missing one falls back to the default.
pub(super) const WAKE_MODEL: &str = "ggml-base.en.bin";

/// The model for the idle checks (waiting for the wake phrase, and "stop" while a task runs): `AQUA_WAKE_MODEL`, else the one
/// chosen in the admin panel, else the default.
fn idle_model_name(state: &AppState) -> String {
    std::env::var("AQUA_WAKE_MODEL")
        .ok()
        .filter(|m| !m.trim().is_empty())
        .or_else(|| state.db.get_setting("stt_model_idle").ok().flatten().filter(|m| !m.trim().is_empty()))
        .unwrap_or_else(|| WAKE_MODEL.to_string())
}

#[derive(Serialize)]
struct SttModelInfo {
    file: String,
    size_mb: u64,
}

#[derive(Serialize)]
struct SttModelsResponse {
    /// The model that listens for "Hey Aqua" while the orb is idle.
    idle: String,
    /// The model that transcribes what is said after wake-up.
    active: String,
    available: Vec<SttModelInfo>,
    default_idle: &'static str,
    /// The voice-activity model (Silero) that keeps wind, typing and fans from being transcribed is present.
    noise_filter: bool,
}

/// Which Whisper models listen while idle and which transcribe requests. A smaller one idle costs less processor time
/// (it runs on whatever the microphone hears); a larger one after wake-up is more accurate.
async fn stt_models_get_endpoint(State(state): State<AppState>) -> Json<SttModelsResponse> {
    let available: Vec<SttModelInfo> = state.whisper.available_models().into_iter().map(|(file, size)| SttModelInfo { file, size_mb: size / (1024 * 1024) }).collect();
    let idle = idle_model_name(&state);
    let active = state.whisper.model_for(None).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
    Json(SttModelsResponse { idle, active, available, default_idle: WAKE_MODEL, noise_filter: state.whisper.vad_active() })
}

#[derive(Deserialize)]
struct SttModelsRequest {
    idle: Option<String>,
    active: Option<String>,
}

async fn stt_models_post_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<SttModelsRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let available: Vec<String> = state.whisper.available_models().into_iter().map(|(f, _)| f).collect();
    for chosen in [&body.idle, &body.active].into_iter().flatten() {
        if !available.contains(chosen) {
            return api_error(axum::http::StatusCode::BAD_REQUEST, &format!("{chosen} is not one of the installed Whisper models"));
        }
    }
    if let Some(idle) = &body.idle {
        if let Err(e) = state.db.set_setting("stt_model_idle", idle) {
            tracing::warn!("could not save stt_model_idle: {e:#}");
            return api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "the setting could not be saved");
        }
    }
    if let Some(active) = &body.active {
        if let Err(e) = state.db.set_setting("stt_model_active", active) {
            tracing::warn!("could not save stt_model_active: {e:#}");
            return api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "the setting could not be saved");
        }
        state.whisper.set_active_model(Some(active.clone()));
    }
    stt_models_get_endpoint(State(state)).await.into_response()
}

#[derive(Deserialize)]
struct SttImportRequest {
    /// Full path of a `ggml-*.bin` already on this machine.
    path: String,
}

#[derive(Deserialize)]
struct SttRemoveRequest {
    model: String,
}

async fn stt_import_model_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<SttImportRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let source = std::path::PathBuf::from(body.path.trim());
    let whisper = state.whisper.clone();
    let imported = tokio::task::spawn_blocking(move || whisper.import_model(&source)).await;
    match imported {
        Ok(Ok(name)) => {
            state.whisper.set_active_model(Some(name.clone()));
            let _ = state.db.set_setting("stt_model_active", &name);
            stt_models_get_endpoint(State(state)).await.into_response()
        }
        Ok(Err(e)) => api_error(axum::http::StatusCode::BAD_REQUEST, &e.to_string()),
        Err(_) => api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "import was interrupted"),
    }
}

async fn stt_remove_model_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<SttRemoveRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match state.whisper.remove_model(&body.model) {
        Ok(()) => {
            if state.db.get_setting("stt_model_active").ok().flatten().as_deref() == Some(body.model.as_str()) {
                let _ = state.db.set_setting("stt_model_active", "");
            }
            if state.db.get_setting("stt_model_idle").ok().flatten().as_deref() == Some(body.model.as_str()) {
                let _ = state.db.set_setting("stt_model_idle", "");
            }
            stt_models_get_endpoint(State(state)).await.into_response()
        }
        Err(e) => api_error(axum::http::StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

/// The wake lane: one check at a time, and never in the way of a real request.
pub(super) fn wake_slot() -> Arc<tokio::sync::Semaphore> {
    static SLOT: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))).clone()
}

#[derive(Deserialize)]
pub struct WakeCheckRequest {
    /// One stretch of speech (WAV, any rate; 16 kHz mono is the cheapest), base64-encoded.
    pub wav_base64: String,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct WakeCheckResponse {
    /// The wake phrase was heard.
    pub wake: bool,
    /// What was heard (empty when the clip was not checked).
    pub heard: String,
    /// Why nothing was checked: `busy` (another check, or a real request, is running), `too_short`, `too_quiet`, `not_speech`, `too_slow`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<&'static str>,
}

impl WakeCheckResponse {
    pub(super) fn skipped(why: &'static str) -> Self {
        Self { wake: false, heard: String::new(), skipped: Some(why) }
    }
}

/// Listen for the wake phrase in one stretch of speech. This is the orb's idle-time check, so it is built to cost little and
/// to get out of the way: it uses a small model, takes its own single slot instead of the turn queue (so room chatter can
/// never make a real request wait), and is skipped, not queued, whenever a real request is running or another check is. Audio
/// that is too short or too quiet to be speech is not transcribed at all (a quiet clip is where Whisper invents text).
async fn wake_check_endpoint(State(state): State<AppState>, Json(payload): Json<WakeCheckRequest>) -> Result<Json<WakeCheckResponse>, Response> {
    let bytes = decode_bytes_base64(&payload.wav_base64).map_err(|e| api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid wav_base64: {e}")))?;
    if bytes.is_empty() {
        return Err(api_error(axum::http::StatusCode::BAD_REQUEST, "empty wav_base64"));
    }
    let pcm = aqua_audio_adapter::extract_pcm16_samples(&bytes).map_err(|e| api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid audio: {e}")))?;
    if pcm.len() < 5_600 {
        return Ok(Json(WakeCheckResponse::skipped("too_short"))); // under 0.35 s: a click or a cough
    }
    if crate::pipeline::loudest_second_rms(&pcm) < 0.008 {
        return Ok(Json(WakeCheckResponse::skipped("too_quiet")));
    }
    // A real request is running: it has the CPU. (Checked first: the voice check below also costs a little.)
    if turn_gate().available_permits() == 0 {
        return Ok(Json(WakeCheckResponse::skipped("busy")));
    }
    // Loud but not a voice (wind, typing, a fan): not worth starting a speech model for.
    if crate::pipeline::is_not_speech(&state.whisper, &pcm) {
        return Ok(Json(WakeCheckResponse::skipped("not_speech")));
    }
    let Ok(permit) = wake_slot().try_acquire_owned() else {
        return Ok(Json(WakeCheckResponse::skipped("busy")));
    };
    let wakeup_word = state.db.get_setting("wakeup_word").ok().flatten().unwrap_or_else(|| "Hey Aqua".to_string());
    let model = idle_model_name(&state);
    let whisper = state.whisper.clone();
    // Whatever the rate or format that arrived, Whisper is given 16 kHz mono.
    let wav16 = aqua_audio_adapter::resample::mono_wav_bytes(&pcm, 16_000).map_err(|e| api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid audio: {e}")))?;
    // The slot is held by the work itself (moved in), so a slow check keeps the lane closed until it really ends.
    let job = tokio::task::spawn_blocking(move || {
        let _slot = permit;
        // The wake check runs all day: few threads, low priority (it must never slow down the model that is answering).
        whisper.transcribe_memory_wav_listening(&wav16, Some(&model)).map_err(|e| format!("{e:#}"))
    });
    match tokio::time::timeout(std::time::Duration::from_secs(12), job).await {
        Err(_) => Ok(Json(WakeCheckResponse::skipped("too_slow"))),
        Ok(Err(e)) => Err(pipeline_failure(&format!("the wake check stopped unexpectedly: {e}"))),
        Ok(Ok(Err(e))) => Err(pipeline_failure(&e)),
        Ok(Ok(Ok(heard))) => {
            let heard: String = heard.trim().chars().take(200).collect();
            let wake = crate::voice_commands::wake_phrase_heard(&heard, &wakeup_word);
            if wake {
                prewarm_for_request(state.llm.clone());
            }
            Ok(Json(WakeCheckResponse { wake, heard, skipped: None }))
        }
    }
}

/// The lane that listens for a spoken "stop". Its own single slot, apart from the turn gate and the wake lane: it has to work
/// exactly when a task is running and the turn gate is taken.
fn stop_slot() -> Arc<tokio::sync::Semaphore> {
    static SLOT: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))).clone()
}

#[derive(Deserialize)]
pub struct StopCheckRequest {
    /// One stretch of speech (WAV, any rate), base64-encoded.
    pub wav_base64: String,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct StopCheckResponse {
    /// The person ordered Aqua to stop.
    pub stop: bool,
    /// What was heard (empty when the clip was not checked).
    pub heard: String,
    /// Why nothing was checked: `busy` (another check is running), `too_short`, `too_quiet`, `too_slow`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<&'static str>,
}

/// Listen for a spoken "stop" in one stretch of speech. Unlike the wake check this runs while a task is running (that is
/// when it matters): it uses the small model, one clip at a time, and a clip that is not an order to stop is simply
/// dropped. It only reports what it heard; the caller stops the task with `/api/v1/tasks/stop`.
async fn stop_check_endpoint(State(state): State<AppState>, Json(payload): Json<StopCheckRequest>) -> Result<Json<StopCheckResponse>, Response> {
    let skipped = |why: &'static str| Json(StopCheckResponse { stop: false, heard: String::new(), skipped: Some(why) });
    let bytes = decode_bytes_base64(&payload.wav_base64).map_err(|e| api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid wav_base64: {e}")))?;
    if bytes.is_empty() {
        return Err(api_error(axum::http::StatusCode::BAD_REQUEST, "empty wav_base64"));
    }
    let pcm = aqua_audio_adapter::extract_pcm16_samples(&bytes).map_err(|e| api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid audio: {e}")))?;
    if pcm.len() < 4_000 {
        return Ok(skipped("too_short")); // under a quarter of a second: "stop" is about half a second long
    }
    if crate::pipeline::loudest_second_rms(&pcm) < 0.006 {
        return Ok(skipped("too_quiet"));
    }
    if crate::pipeline::is_not_speech(&state.whisper, &pcm) {
        return Ok(skipped("not_speech"));
    }
    let Ok(permit) = stop_slot().try_acquire_owned() else {
        return Ok(skipped("busy"));
    };
    let wakeup_word = state.db.get_setting("wakeup_word").ok().flatten().unwrap_or_else(|| "Hey Aqua".to_string());
    let model = idle_model_name(&state);
    let whisper = state.whisper.clone();
    let wav16 = aqua_audio_adapter::resample::mono_wav_bytes(&pcm, 16_000).map_err(|e| api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid audio: {e}")))?;
    let job = tokio::task::spawn_blocking(move || {
        let _slot = permit; // held by the work itself, so a slow check keeps the lane closed until it really ends
        whisper.transcribe_memory_wav_listening(&wav16, Some(&model)).map_err(|e| format!("{e:#}"))
    });
    // The model server is busy with the task being stopped, so this can be slow: allow for it.
    match tokio::time::timeout(std::time::Duration::from_secs(25), job).await {
        Err(_) => Ok(skipped("too_slow")),
        Ok(Err(e)) => Err(pipeline_failure(&format!("the stop check stopped unexpectedly: {e}"))),
        Ok(Ok(Err(e))) => Err(pipeline_failure(&e)),
        Ok(Ok(Ok(heard))) => {
            let heard: String = heard.trim().chars().take(200).collect();
            let stop = crate::voice_commands::is_stop_command(&heard, &wakeup_word);
            if stop {
                info!("Spoken stop heard ({} characters heard)", heard.chars().count());
            }
            Ok(Json(StopCheckResponse { stop, heard, skipped: None }))
        }
    }
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/voice/wake-check", post(wake_check_endpoint).layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)))
        .route("/api/v1/voice/stop-check", post(stop_check_endpoint).layer(axum::extract::DefaultBodyLimit::max(1024 * 1024)))
        .route("/api/v1/voice/stt-models", get(stt_models_get_endpoint).post(stt_models_post_endpoint).delete(stt_remove_model_endpoint))
        .route("/api/v1/voice/stt-models/import", post(stt_import_model_endpoint))
}

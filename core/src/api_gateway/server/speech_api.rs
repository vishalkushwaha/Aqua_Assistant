//! The engines of speech and sound as routes of their own (card OA3-06, moved out of `server.rs`): `/kokoro/synthesize` and `/kokoro/synthesize_wav`, `/whisper/transcribe`, `/deepfilter/denoise`, and the decoding of the audio a body carries.

use super::*;

pub(in crate::api_gateway) fn decode_bytes_base64(b64: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| e.to_string())
}

pub(super) fn decode_pcm_f32_base64(b64: &str) -> Result<Vec<f32>, String> {
    let bytes = decode_bytes_base64(b64)?;
    if !bytes.len().is_multiple_of(4) {
        return Err("pcm length must be a multiple of 4 bytes (f32 LE)".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        out.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct KokoroSynthRequest {
    pub text: String,
    #[serde(default = "default_kokoro_voice")]
    pub voice: String,
    #[serde(default = "default_kokoro_lang")]
    pub lang: String,
}

/// The voice a request that names none is spoken with: the one the person chose (`PUT /api/v1/voice/choice`, card R4-03), `af_sarah` while none is kept.
pub(super) fn default_kokoro_voice() -> String {
    super::super::voice_choice_api::current().voice
}

pub(super) fn default_kokoro_lang() -> String {
    super::super::voice_choice_api::current().lang
}

#[derive(Serialize)]
pub struct KokoroSynthResponse {
    pub wav_base64: String,
    pub sample_rate: u32,
    pub engine_ready: bool,
    pub latency_ms: u64,
}

async fn kokoro_synthesize_endpoint(
    State(state): State<AppState>,
    Json(payload): Json<KokoroSynthRequest>,
) -> Response {
    let _turn = match take_turn().await {
        Ok(p) => p,
        Err(busy) => return busy,
    };
    let start = std::time::Instant::now();
    let engine_ready = state.tts.is_engine_ready();

    let tts = state.tts.clone();
    let wav = match run_blocking(move || {
        tts.synthesize_wav(&payload.text, &payload.voice, &payload.lang)
    })
    .await
    {
        Ok(wav) => wav,
        Err(e) => return upstream_error("kokoro", e),
    };

    let sample_rate = aqua_audio_adapter::kokoro_cli::wav_sample_rate(&wav)
        .unwrap_or_else(|| state.tts.sample_rate());

    Json(KokoroSynthResponse {
        wav_base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wav),
        sample_rate,
        engine_ready,
        latency_ms: start.elapsed().as_millis() as u64,
    })
    .into_response()
}

async fn kokoro_synthesize_wav_endpoint(
    State(state): State<AppState>,
    Json(payload): Json<KokoroSynthRequest>,
) -> Response {
    let _turn = match take_turn().await {
        Ok(p) => p,
        Err(busy) => return busy,
    };
    let tts = state.tts.clone();
    match run_blocking(move || tts.synthesize_wav(&payload.text, &payload.voice, &payload.lang))
        .await
    {
        Ok(wav) => (
            [
                (axum::http::header::CONTENT_TYPE, "audio/wav"),
                (axum::http::header::CACHE_CONTROL, "no-cache"),
            ],
            wav,
        )
            .into_response(),
        Err(e) => upstream_error("kokoro", e),
    }
}

/// Run slow engine work (llama/whisper/kokoro subprocesses, ONNX, Laya) off the async runtime.
async fn run_blocking<T, F>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow::anyhow!("worker task failed: {e}"))?
}

#[derive(Deserialize)]
pub struct WhisperTranscribeRequest {
    pub wav_base64: String,
    pub model: Option<String>,
}

#[derive(Serialize)]
pub struct WhisperTranscribeResponse {
    pub transcription: String,
    pub latency_ms: u64,
    pub engine_ready: bool,
    pub model_loaded: bool,
    pub active_model: String,
}

async fn whisper_transcribe_endpoint(
    State(state): State<AppState>,
    Json(payload): Json<WhisperTranscribeRequest>,
) -> Result<Json<WhisperTranscribeResponse>, axum::response::Response> {
    let start_time = std::time::Instant::now();
    let engine_ready = state.whisper.is_engine_ready();
    let model_loaded = state.whisper.is_model_loaded();
    let requested_model = payload.model.as_deref();

    // A model is chosen by file name from the models folder, never by a path.
    if payload.model.as_deref().is_some_and(|m| m.contains(['/', '\\']) || m.contains("..") || m.len() > 120) {
        return Err(api_error(axum::http::StatusCode::BAD_REQUEST, "model must be a file name from the Whisper models folder"));
    }
    let bytes = decode_bytes_base64(&payload.wav_base64).map_err(|e| api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid wav_base64: {e}")))?;
    if bytes.is_empty() {
        return Err(api_error(axum::http::StatusCode::BAD_REQUEST, "empty wav_base64"));
    }
    // Same queue and deadline as the voice turns: it uses the same speech engine and the same CPU.
    let _turn = take_turn().await?;
    let whisper = state.whisper.clone();
    let model = payload.model.clone();
    let transcription = run_turn(move || whisper.transcribe_memory_wav_with_model(&bytes, model.as_deref()).map_err(|e| format!("{e:#}")))
        .await
        .map_err(|e| pipeline_failure(&e))?;

    let latency_ms = start_time.elapsed().as_millis() as u64;
    // The model that really ran: a requested name that is not on disk falls back to the default, and says so here.
    let active_model = state.whisper.model_for(requested_model).file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();

    Ok(Json(WhisperTranscribeResponse {
        transcription,
        latency_ms,
        engine_ready,
        model_loaded,
        active_model,
    }))
}

#[derive(Deserialize)]
pub struct DeepFilterDenoiseRequest {
    pub wav_base64: String,
}

#[derive(Serialize)]
pub struct DeepFilterDenoiseResponse {
    pub enhanced_wav_base64: String,
    pub latency_ms: u64,
    pub denoise_applied: bool,
    pub engine_ready: bool,
    pub model_loaded: bool,
    /// The attenuation limit DeepFilterNet was allowed (dB); `null` when it did not run. A limit, not a measurement.
    pub noise_reduction_db: Option<f32>,
    pub original_rms: f32,
    pub enhanced_rms: f32,
    /// Change in RMS level in dB (original over enhanced). This is a level ratio, not a measured SNR.
    pub snr_improvement_db: f32,
    /// What actually processed the audio: `deep_filter_net` is neural suppression, `frame_gate` is only a gate.
    pub engine: &'static str,
}

async fn deepfilter_denoise_endpoint(
    State(state): State<AppState>,
    Json(payload): Json<DeepFilterDenoiseRequest>,
) -> Response {
    let start_time = std::time::Instant::now();
    let bytes = match decode_bytes_base64(&payload.wav_base64) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => return api_error(axum::http::StatusCode::BAD_REQUEST, "empty wav_base64"),
        Err(e) => return api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid wav_base64: {e}")),
    };
    // Same queue and deadline as the voice turns: this uses the same noise engine, which a turn can hold for a while.
    let _turn = match take_turn().await {
        Ok(t) => t,
        Err(busy) => return busy,
    };
    let noise_clone = state.noise.clone();
    let done = run_turn(move || {
        let noise = noise_clone.lock().unwrap_or_else(PoisonError::into_inner);
        let info = (noise.is_engine_ready(), noise.is_model_loaded(), noise.attenuation_limit_db());
        // A failure is an error, not "200 with your own audio back": the caller must be able to tell a bad file from a
        // processed one.
        let o = noise.denoise_memory_wav(&bytes).map_err(|e| format!("{e:#}"))?;
        Ok((info, o))
    })
    .await;
    let ((engine_ready, model_loaded, attenuation_limit_db), o) = match done {
        Ok(v) => v,
        Err(e) => return pipeline_failure(&e),
    };
    let neural = o.engine == aqua_audio_adapter::noise_cancellation::DenoiseEngine::DeepFilterNet;
    Json(DeepFilterDenoiseResponse {
        enhanced_wav_base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &o.wav),
        latency_ms: start_time.elapsed().as_millis() as u64,
        denoise_applied: neural,
        engine_ready,
        model_loaded,
        // Only the neural engine has an attenuation limit; a gate has none (it used to report the configured 15 dB either way).
        noise_reduction_db: neural.then_some(attenuation_limit_db),
        original_rms: o.original_rms,
        enhanced_rms: o.enhanced_rms,
        snr_improvement_db: o.level_change_db,
        engine: if neural { "deep_filter_net" } else { "frame_gate" },
    })
    .into_response()
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/whisper/transcribe", post(whisper_transcribe_endpoint).layer(axum::extract::DefaultBodyLimit::max(AUDIO_BODY_LIMIT)))
        .route("/api/v1/deepfilter/denoise", post(deepfilter_denoise_endpoint).layer(axum::extract::DefaultBodyLimit::max(AUDIO_BODY_LIMIT)))
        .route("/api/v1/kokoro/synthesize", post(kokoro_synthesize_endpoint).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)))
        .route("/api/v1/kokoro/synthesize_wav", post(kokoro_synthesize_wav_endpoint).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)))
}

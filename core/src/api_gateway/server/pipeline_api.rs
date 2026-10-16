//! The turn routes that answer in one piece (card OA3-06, moved out of `server.rs`): `/pipeline/text`, `/pipeline/voice-frame` and `/pipeline/wav`, with what they share (the duplicate-audio check, `for_client`, `question_for`, the recording kept for one turn).

use super::*;

/// Identical audio sent twice within a few seconds is almost always one recording posted twice (a double submit in
/// the client). The second copy is answered as ignored rather than run again.
#[cfg(test)]
pub(super) fn is_duplicate_audio(b64: &str) -> bool {
    is_duplicate_key(audio_key(b64))
}

pub(super) fn audio_key(b64: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(b64.as_bytes()).into()
}

fn recent_audio() -> std::sync::MutexGuard<'static, std::collections::HashMap<[u8; 32], std::time::Instant>> {
    static RECENT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<[u8; 32], std::time::Instant>>> = std::sync::OnceLock::new();
    RECENT.get_or_init(Default::default).lock().unwrap_or_else(PoisonError::into_inner)
}

/// True if this audio arrived within the last 5 seconds. The first sight is remembered; a repeat does not extend it.
pub(super) fn is_duplicate_key(key: [u8; 32]) -> bool {
    let mut recent = recent_audio();
    let now = std::time::Instant::now();
    recent.retain(|_, t| now.duration_since(*t) < std::time::Duration::from_secs(5));
    if recent.contains_key(&key) {
        return true;
    }
    recent.insert(key, now);
    false
}

/// The request failed (busy, error): the person's retry must not be taken for a duplicate.
pub(super) fn forget_audio(key: [u8; 32]) {
    recent_audio().remove(&key);
}

#[derive(Deserialize)]
pub struct PipelineTextRequest {
    pub text: String,
    #[serde(default = "default_kokoro_voice")]
    pub voice: String,
    #[serde(default = "default_kokoro_lang")]
    pub lang: String,
    /// `false`: a text-only turn (card OA10-06, UX-08). Nothing is synthesized, the speech engine is not asked, and the answer is the same words: `tts_wav_base64` is empty and
    /// `tts_sample_count` is 0 (a streamed turn still sends each piece of the answer as a `speech` event, with its `text` and an empty `wav_base64`). Not given, or `true`: the answer is
    /// spoken, as it always was. `?tts=0` on the URL says the same.
    #[serde(default)]
    pub tts: Option<bool>,
}

/// Does a request want its answer spoken? `"tts": false` in the body, or `?tts=0` (`false`, `off`, `no`) in the URL, asks for the words only (card OA10-06, UX-08); a request that says neither is
/// spoken, as it always was.
pub(crate) fn wants_sound(body: Option<bool>, query: Option<&str>) -> bool {
    if body == Some(false) {
        return false;
    }
    !matches!(query.map(|q| q.trim().to_ascii_lowercase()).as_deref(), Some("0" | "false" | "off" | "no"))
}

/// The query of the turn routes: `?tts=0` asks for the words only.
#[derive(Deserialize, Default)]
pub(super) struct SoundQuery {
    pub(super) tts: Option<String>,
}

#[derive(Serialize)]
pub struct PipelineTextResponse {
    #[serde(flatten)]
    pub result: crate::pipeline::PipelineTextResult,
    /// Present when the agent asked a question and is waiting for the answer: `{"text": "...", "choices": [...]}`. The answer is the next
    /// request; a client may keep listening for it without the wake phrase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<serde_json::Value>,
    pub latency_ms: u64,
    /// Where the time went: waiting for a turn, then each stage (gate, chat or agent, speech), in order.
    pub stages_ms: Vec<crate::turn_timing::StageMs>,
}

/// Runs the work of a request for the client that made it: whether it takes part in the person's conversation (the UI token, `conversation::with_client`), and for whom the agent runs it
/// starts are recorded (`agent_runs::with_origin`), so that the events of a run asked for with the API token can be read with the API token and no other run's can (card OA10-01).
pub(in crate::api_gateway) fn for_client<R>(ui: bool, f: impl FnOnce() -> R) -> R {
    crate::conversation::with_client(ui, || crate::agent_runs::with_origin(ui, f))
}

/// The question the agent is waiting on, for the client that may see it: the person's own app (the UI token). A script with the API token
/// neither sees nor answers it, and its turns are not part of the person's conversation (see `conversation::with_client`).
pub(super) fn question_for(ui: bool) -> Option<serde_json::Value> {
    if ui {
        crate::conversation::pending_for_client()
    } else {
        None
    }
}

async fn pipeline_text_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    axum::extract::Query(sound): axum::extract::Query<SoundQuery>,
    Json(payload): Json<PipelineTextRequest>,
) -> Result<Json<PipelineTextResponse>, axum::response::Response> {
    let ui = scope == Scope::Ui;
    let speak = wants_sound(payload.tts, sound.tts.as_deref());
    let start = std::time::Instant::now();
    // A sentence that goes on from the task that is running ("also add tests", "then run clippy") is said to that task, and does not wait its turn as a request of its own (card OA10-03,
    // UX-11). Only for the person's own client, only when `AQUA_STEER_CUES` is on, and only when a task the person waits for is going.
    if ui {
        if let Some(said) = steer_by_cue(&state, &payload.text) {
            return Ok(Json(PipelineTextResponse {
                result: crate::pipeline::PipelineTextResult {
                    input_text: payload.text.clone(),
                    assistant_text: said,
                    tts_sample_count: 0,
                    tts_wav_base64: String::new(),
                    tts_sample_rate: state.tts.sample_rate(),
                    llm_engine_ready: state.llm.is_engine_ready(),
                    tts_engine_ready: state.tts.is_engine_ready(),
                },
                question: None,
                latency_ms: start.elapsed().as_millis() as u64,
                stages_ms: Vec::new(),
            }));
        }
    }
    let _turn = take_turn().await?;
    let queued_ms = start.elapsed().as_millis() as u64;
    let (result, mut stages_ms) = run_turn_timed(move || {
        for_client(ui, || {
            crate::pipeline::run_text_turn_with_sound(
                &state.llm,
                &state.tts,
                &payload.text,
                &payload.voice,
                &payload.lang,
                Some(state.browser.clone()),
                Some(state.windows_mcp.clone()),
                Some(&state.laya),
                None,
                speak,
            )
            .map_err(|e| format!("{e:#}"))
        })
    })
    .await
    .map_err(|e| pipeline_failure(&e))?;
    stages_ms.insert(0, crate::turn_timing::StageMs { stage: "queued", ms: queued_ms });

    Ok(Json(PipelineTextResponse {
        result,
        question: question_for(ui),
        latency_ms: start.elapsed().as_millis() as u64,
        stages_ms,
    }))
}

// The dictation routes (`/dictation/transcribe`, `/dictation/rewrite`) are in `dictation_api.rs` (card R6-07): they take the two engines they use and not the whole state.

#[derive(Deserialize)]
pub struct PipelineVoiceFrameRequest {
    /// Little-endian f32 PCM samples, base64-encoded.
    pub pcm_base64: String,
    /// Sample rate of `pcm_base64` (default 16000). Anything else is converted to 16 kHz before use.
    #[serde(default)]
    pub sample_rate: Option<u32>,
    /// Channels of `pcm_base64`, interleaved (default 1).
    #[serde(default)]
    pub channels: Option<u16>,
    /// Ignored: whether a session is open is decided by the server (see `VoiceSession`), so a caller cannot
    /// skip the wake word by claiming one. Kept so existing clients still parse.
    #[serde(default)]
    #[allow(dead_code)]
    pub active_session: bool,
}

#[derive(Serialize)]
pub struct PipelineVoiceFrameResponse {
    pub transcribed_prompt: String,
    pub assistant_text: String,
    pub tts_sample_count: usize,
    /// The spoken answer as a mono 16-bit WAV, base64-encoded. Empty when nothing was spoken (see `tts_spoken`).
    pub tts_wav_base64: String,
    /// Sample rate of `tts_wav_base64`.
    pub tts_sample_rate: u32,
    /// True only when real speech was produced. A text-to-speech engine that is not ready yields half a second of
    /// silence, which used to look like a successful `tts_sample_count`.
    pub tts_spoken: bool,
    pub latency_ms: u64,
    pub stages: PipelineStageHealth,
    /// Whether the server now has a voice session open (clients should follow this, not guess).
    pub session_active: bool,
}

async fn pipeline_voice_frame_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<PipelineVoiceFrameRequest>,
) -> Result<Json<PipelineVoiceFrameResponse>, axum::response::Response> {
    let ui = scope == Scope::Ui;
    let start = std::time::Instant::now();
    let stages = pipeline_health_from_state(&state);

    let frame = decode_pcm_f32_base64(&payload.pcm_base64)
        .map_err(|e| api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid pcm_base64: {e}")))?;
    // Every speech stage assumes 16 kHz mono: say what these samples are and they are converted once, here.
    let frame = aqua_audio_adapter::resample::to_16k_mono(&frame, payload.sample_rate.unwrap_or(16_000), payload.channels.unwrap_or(1))
        .map_err(|e| api_error(axum::http::StatusCode::BAD_REQUEST, &e.to_string()))?;
    let _turn = take_turn().await?;

    let security_enabled = state.db.voice_lock_enabled();
    let wakeup_word = state.db.get_setting("wakeup_word").ok().flatten().unwrap_or_else(|| "Hey Aqua".to_string());

    let session_open = crate::voice_commands::voice_session().is_active(unix_now());
    let wake_for_session = wakeup_word.clone();
    let tts = state.tts.clone();
    let (prompt, response, speech) = run_turn(move || for_client(ui, || {
        let noise = state.noise.lock().unwrap_or_else(PoisonError::into_inner);
        let mut bio = state.biometrics.lock().unwrap_or_else(PoisonError::into_inner);
        run_audio_frame_to_speech_with_mcp(
            &noise,
            &state.whisper,
            &state.llm,
            &state.tts,
            &mut bio,
            security_enabled,
            &wakeup_word,
            session_open,
            &frame,
            Some(state.browser.clone()),
            Some(state.windows_mcp.clone()),
            Some(&state.laya),
        )
        .map_err(|e| format!("{e:#}"))
    }))
    .await
    .map_err(|e| pipeline_failure(&e))?;
    // `_turn` is still held here: the next turn does not start (and read the session) until this one has updated it.

    let session = crate::voice_commands::voice_session();
    session.after_turn(&prompt, &wake_for_session, crate::voice_commands::turn_was_accepted(&response), unix_now());
    let spoken = tts.is_engine_ready() && speech.iter().any(|s| s.abs() > 1e-4);
    let tts_sample_rate = tts.sample_rate();
    let tts_wav_base64 = if spoken {
        use base64::Engine;
        aqua_audio_adapter::resample::mono_wav_bytes(&speech, tts_sample_rate)
            .map(|wav| base64::engine::general_purpose::STANDARD.encode(wav))
            .unwrap_or_default()
    } else {
        String::new()
    };
    Ok(Json(PipelineVoiceFrameResponse {
        transcribed_prompt: prompt,
        assistant_text: response,
        tts_sample_count: speech.len(),
        tts_wav_base64,
        tts_sample_rate,
        tts_spoken: spoken,
        latency_ms: start.elapsed().as_millis() as u64,
        stages,
        session_active: session.is_active(unix_now()),
    }))
}

#[derive(Deserialize)]
pub struct PipelineWavRequest {
    /// Input speech WAV file, base64-encoded.
    pub wav_base64: String,
    #[serde(default = "default_kokoro_voice")]
    pub voice: String,
    #[serde(default = "default_kokoro_lang")]
    pub lang: String,
    /// Ignored: the server decides whether a voice session is open (see `VoiceSession`).
    #[serde(default)]
    pub active_session: bool,
}

#[derive(Serialize)]
pub struct PipelineWavResponse {
    #[serde(flatten)]
    pub result: crate::pipeline::PipelineWavResult,
    /// As in [`PipelineTextResponse`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<serde_json::Value>,
    pub latency_ms: u64,
    /// Where the time went: waiting for a turn, then each stage (denoise, recognition, voiceprint, gate, chat or agent,
    /// speech), in order. Empty for a request that was ignored before any stage ran.
    pub stages_ms: Vec<crate::turn_timing::StageMs>,
    /// Whether the server now has a voice session open (clients should follow this, not guess).
    pub session_active: bool,
}

pub(super) fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

async fn pipeline_wav_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<PipelineWavRequest>,
) -> Result<Json<PipelineWavResponse>, axum::response::Response> {
    let ui = scope == Scope::Ui;
    let start = std::time::Instant::now();
    let session_open = crate::voice_commands::voice_session().is_active(unix_now());
    let audio = audio_key(&payload.wav_base64);
    if is_duplicate_key(audio) {
        info!("pipeline/wav: identical audio was just received; ignoring the second copy");
        return Ok(Json(PipelineWavResponse {
            result: crate::pipeline::PipelineWavResult {
                denoise_applied: false,
                transcribed_text: String::new(),
                assistant_text: "IGNORED: duplicate request.".to_string(),
                session_action: None,
                local_command: false,
                tts_sample_count: 0,
                tts_wav_base64: String::new(),
                tts_sample_rate: state.tts.sample_rate(),
                whisper_engine_ready: state.whisper.is_engine_ready(),
                llm_engine_ready: state.llm.is_engine_ready(),
                tts_engine_ready: state.tts.is_engine_ready(),
            },
            question: None,
            latency_ms: start.elapsed().as_millis() as u64,
            stages_ms: Vec::new(),
            session_active: session_open,
        }));
    }
    // A request that could not even get a turn (busy) is not a duplicate when the person tries again.
    let _turn = match take_turn().await {
        Ok(p) => p,
        Err(r) => {
            forget_audio(audio);
            return Err(r);
        }
    };
    let queued_ms = start.elapsed().as_millis() as u64;
    // Read only now that this turn has its place in the queue: an earlier turn (say "go to sleep") has finished
    // and updated the session, so this one cannot run on a stale answer.
    let session_open = crate::voice_commands::voice_session().is_active(unix_now());
    let wake_outer = state.db.get_setting("wakeup_word").ok().flatten().unwrap_or_else(|| "Hey Aqua".to_string());
    info!(
        "pipeline/wav request session_open={} bytes_b64={}",
        session_open,
        payload.wav_base64.len()
    );

    let (result, mut stages_ms) = run_turn_timed(move || for_client(ui, || wav_turn_work(&state, &payload, session_open, None)))
        .await
        .map_err(|e| {
            forget_audio(audio);
            pipeline_failure(&e)
        })?;

    info!(
        "pipeline/wav done in {}ms heard={} chars action={:?} local={}",
        start.elapsed().as_millis(),
        result.transcribed_text.chars().count(),
        result.session_action,
        result.local_command
    );
    stages_ms.insert(0, crate::turn_timing::StageMs { stage: "queued", ms: queued_ms });

    let session = crate::voice_commands::voice_session();
    crate::conversation::with_client(ui, || session.after_turn(&result.transcribed_text, &wake_outer, crate::voice_commands::turn_was_accepted(&result.assistant_text), unix_now()));
    Ok(Json(PipelineWavResponse {
        result,
        question: question_for(ui),
        latency_ms: start.elapsed().as_millis() as u64,
        stages_ms,
        session_active: session.is_active(unix_now()),
    }))
}

/// The blocking work of one voice turn: the recording goes to a scratch file (removed however this ends, a panic in the audio code
/// included: it used to be removed only on a normal return, so a crash left someone's voice in the temp folder) and through the
/// pipeline. With a `listener` the answer is spoken to it piece by piece, and the result has no sound of its own.
pub(super) fn wav_turn_work(
    state: &AppState,
    payload: &PipelineWavRequest,
    session_open: bool,
    listener: Option<crate::pipeline::Listener<'_>>,
) -> Result<crate::pipeline::PipelineWavResult, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let scratch = std::env::temp_dir().join(format!("aqua_api_wav_{stamp}.wav"));
    let _cleanup = RemoveOnDrop(scratch.clone());
    let bytes = decode_bytes_base64(&payload.wav_base64)?;
    if bytes.is_empty() {
        return Err("empty wav_base64".into());
    }
    std::fs::write(&scratch, &bytes).map_err(|e| format!("write temp wav: {e}"))?;
    let security_enabled = state.db.voice_lock_enabled();
    let wakeup_word = state.db.get_setting("wakeup_word").ok().flatten().unwrap_or_else(|| "Hey Aqua".to_string());
    let noise = state.noise.lock().unwrap_or_else(PoisonError::into_inner);
    let mut bio = state.biometrics.lock().unwrap_or_else(PoisonError::into_inner);
    let deepfilter_dir = state.models_dir.join("deepfilternet");
    let (browser, windows) = (Some(state.browser.clone()), Some(state.windows_mcp.clone()));
    let done = match listener {
        None => run_wav_file_to_speech_with_mcp(
            &noise, &state.whisper, &state.llm, &state.tts, &mut bio, security_enabled, &wakeup_word, session_open, &deepfilter_dir, &scratch, &payload.voice, &payload.lang, browser, windows, Some(&state.laya),
        ),
        Some(listener) => run_wav_file_to_speech_streaming(
            &noise, &state.whisper, &state.llm, &state.tts, &mut bio, security_enabled, &wakeup_word, session_open, &deepfilter_dir, &scratch, &payload.voice, &payload.lang, browser, windows, Some(&state.laya), listener,
        ),
    };
    // The whole chain, not only the outermost context: "whisper transcribe <file>" alone hides that the cause was a timeout or a
    // missing engine, and with it the right status code.
    done.map_err(|e| format!("{e:#}"))
}

/// Deletes a file when dropped.
pub(super) struct RemoveOnDrop(pub(super) std::path::PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/pipeline/text", post(pipeline_text_endpoint).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)))
        .route("/api/v1/pipeline/voice-frame", post(pipeline_voice_frame_endpoint).layer(axum::extract::DefaultBodyLimit::max(AUDIO_BODY_LIMIT)))
        .route("/api/v1/pipeline/wav", post(pipeline_wav_endpoint).layer(axum::extract::DefaultBodyLimit::max(AUDIO_BODY_LIMIT)))
}

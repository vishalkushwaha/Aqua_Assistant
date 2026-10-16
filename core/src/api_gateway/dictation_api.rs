//! The two dictation routes that use the model of this machine: `POST /api/v1/dictation/transcribe` (a clip of speech into clean text) and `POST /api/v1/dictation/rewrite`
//! (command mode: a selection rewritten as the person said, as a preview). Both need the UI token: this is the person's own keyboard and not the agent's.
//!
//! They used to be in `server.rs` and took the whole `AppState`, which a test cannot build (it holds every engine). They take [`DictationState`], the two engines they use, so
//! that a route test can build the router with engines that have no model behind them (card R6-07, task T6-07.10). The paths, the bodies, the limits and the status codes are
//! as they were; the settings and cleanup routes (`/dictation/settings`, `/dictation/clean`) are in `route_api.rs`.

use super::auth::{ui_scope_required, Scope};
use super::server::{api_error, decode_bytes_base64, pipeline_failure, run_turn, take_turn, AppState, AUDIO_BODY_LIMIT};
use crate::modules::llm_router::LlamaRouterEngine;
use aqua_audio_adapter::WhisperSttEngine;
use axum::{
    extract::{FromRef, State},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;

/// What the dictation routes use: the speech model and the language model. The gateway's [`AppState`] gives it (`FromRef`); a test builds it from engines with no model in them.
#[derive(Clone)]
pub struct DictationState {
    pub whisper: Arc<WhisperSttEngine>,
    pub llm: Arc<LlamaRouterEngine>,
}

impl FromRef<AppState> for DictationState {
    fn from_ref(state: &AppState) -> Self {
        Self { whisper: state.whisper.clone(), llm: state.llm.clone() }
    }
}

/// The two routes, with the body limits they had in the gateway (an audio clip may be large; a rewrite is a selection and a sentence).
pub fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    DictationState: FromRef<S>,
{
    Router::new()
        .route("/api/v1/dictation/transcribe", post(dictation_transcribe_endpoint).layer(axum::extract::DefaultBodyLimit::max(AUDIO_BODY_LIMIT)))
        .route("/api/v1/dictation/rewrite", post(dictation_rewrite_endpoint).layer(axum::extract::DefaultBodyLimit::max(32 * 1024)))
}

/// The model of this machine as the rewriter of command mode.
struct LlmRewriter<'a>(&'a LlamaRouterEngine);

impl crate::dictation::command::Rewriter for LlmRewriter<'_> {
    fn rewrite(&self, prompt: &str, max_tokens: u32) -> Result<String, String> {
        self.0.generate_chat_response_limited(prompt, max_tokens).map(|t| crate::modules::llm_router::strip_reasoning_tags(&t)).map_err(|e| format!("{e:#}"))
    }
}

#[derive(Deserialize)]
struct DictationRewriteRequest {
    selection: String,
    instruction: String,
    #[serde(default)]
    style: Option<String>,
}

/// Command mode: rewrites a selection as the person said (a preview with the words that change; nothing is applied). Off unless
/// `AQUA_DICTATION_REWRITE=on`. The model has no tools; the selection goes to it as data. The UI token.
async fn dictation_rewrite_endpoint(State(state): State<DictationState>, axum::Extension(scope): axum::Extension<Scope>, Json(body): Json<DictationRewriteRequest>) -> Response {
    use crate::dictation::command::{self, Refusal};
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    if !command::enabled() {
        return api_error(axum::http::StatusCode::NOT_FOUND, "command mode is off: set AQUA_DICTATION_REWRITE=on (it is experimental)");
    }
    let settings = crate::dictation::load(&crate::runtime::data_dir()).unwrap_or_default();
    let _turn = match take_turn().await {
        Ok(t) => t,
        Err(busy) => return busy,
    };
    let llm = state.llm.clone();
    let req = command::Request { selection: body.selection, instruction: body.instruction, style: body.style };
    let result = run_turn(move || Ok(command::rewrite(&LlmRewriter(&llm), &req, &settings))).await;
    match result {
        Ok(Ok(preview)) => Json(serde_json::json!({ "preview": preview, "applied": false })).into_response(),
        Ok(Err(Refusal::BadRequest(m))) => api_error(axum::http::StatusCode::BAD_REQUEST, &m),
        Ok(Err(Refusal::Guard(m))) => api_error(axum::http::StatusCode::UNPROCESSABLE_ENTITY, &m),
        Ok(Err(Refusal::Model(m))) => api_error(axum::http::StatusCode::BAD_GATEWAY, &m),
        Err(e) => pipeline_failure(&e),
    }
}

#[derive(Deserialize)]
struct DictationTranscribeRequest {
    /// A 16 kHz mono 16-bit WAV, base64.
    wav_base64: String,
}

/// Turns a dictation clip into clean text: the speech model, then the person's cleanup rules (`data/dictation.json`). The clip is not kept. The UI
/// token: this is the person's own keyboard and not the agent's.
async fn dictation_transcribe_endpoint(State(state): State<DictationState>, axum::Extension(scope): axum::Extension<Scope>, Json(payload): Json<DictationTranscribeRequest>) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let started = std::time::Instant::now();
    let bytes = match decode_bytes_base64(&payload.wav_base64) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => return api_error(axum::http::StatusCode::BAD_REQUEST, "empty wav_base64"),
        Err(e) => return api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid wav_base64: {e}")),
    };
    let settings = crate::dictation::load(&crate::runtime::data_dir()).unwrap_or_default();
    // The clip is read first: one longer than the person's limit is refused, and one with nobody talking in it (silence, wind, typing, a fan) is not given to the speech
    // model, which writes words for it.
    let pcm = match aqua_audio_adapter::resample::wav_to_16k_mono(&bytes) {
        Ok(p) => p,
        Err(e) => return api_error(axum::http::StatusCode::BAD_REQUEST, &format!("invalid audio: {e}")),
    };
    let whisper_for_gate = state.whisper.clone();
    match crate::dictation::check_clip(&pcm, &settings, &move |p| crate::pipeline::is_not_speech(&whisper_for_gate, p)) {
        Ok(()) => {}
        Err(crate::dictation::NotText::TooLong { limit_minutes }) => {
            return api_error(axum::http::StatusCode::PAYLOAD_TOO_LARGE, &format!("this clip is longer than the {limit_minutes} minutes set for dictation (max_minutes in the dictation settings)"));
        }
        Err(crate::dictation::NotText::NoSpeech) => {
            return Json(serde_json::json!({ "heard": "", "text": "", "applied": [], "note": "No speech was heard in the clip.", "latency_ms": started.elapsed().as_millis() as u64 })).into_response();
        }
    }
    // The same turn as every other use of the speech engine and the CPU.
    let _turn = match take_turn().await {
        Ok(t) => t,
        Err(busy) => return busy,
    };
    let whisper = state.whisper.clone();
    // The person's own terms prime the speech model, so that their names and jargon are written as they write them. With `AQUA_PROJECT_TERMS` on, the words of the project they work in
    // follow theirs (card OA10-06, UX-16); the walk of the project is a read of files, so it is not done on the async thread.
    let prompt = {
        let for_task = settings.clone();
        tokio::task::spawn_blocking(move || {
            let project = if crate::coding_voice::project_terms_on() { crate::project_terms::current_project().map(|root| crate::project_terms::terms_for(&root)).unwrap_or_default() } else { Vec::new() };
            prompt_with_project_terms(&for_task, &project)
        })
        .await
        .unwrap_or_else(|_| settings.initial_prompt())
    };
    let heard = match run_turn(move || whisper.transcribe_memory_wav_with_prompt(&bytes, None, &prompt).map_err(|e| format!("{e:#}"))).await {
        Ok(text) => text,
        Err(e) => return pipeline_failure(&e),
    };
    let kept = crate::dictation::heard_text(&heard, crate::pipeline::loudest_second_rms(&pcm));
    let invented = kept.is_empty() && !heard.trim().is_empty();
    // The medium level starts the model's program for a moment, so it is not run on the async thread.
    let cleaned = {
        let (for_task, settings_for_task) = (kept.clone(), settings.clone());
        match tokio::task::spawn_blocking(move || crate::dictation::clean_for(&for_task, &settings_for_task, &crate::runtime::data_dir())).await {
            Ok(c) => c,
            Err(_) => crate::dictation::clean_with(&kept, &settings),
        }
    };
    Json(serde_json::json!({
        "heard": heard,
        "text": cleaned.text,
        "applied": cleaned.applied,
        "note": if invented { Some("What the speech model wrote for this clip is not something said, so it was dropped.".to_string()) } else { cleaned.note },
        "latency_ms": started.elapsed().as_millis() as u64,
    }))
    .into_response()
}

/// The prompt that primes the speech model for a clip: the person's own words first, then `project` (the words of the project they work in), each word once, at most 200 words and 800
/// characters. With no project words it is `Settings::initial_prompt` as it always was (card OA10-06, UX-16).
pub(crate) fn prompt_with_project_terms(settings: &crate::dictation::Settings, project: &[String]) -> String {
    if project.is_empty() {
        return settings.initial_prompt();
    }
    crate::project_terms::prompt_of(&crate::project_terms::merge(&settings.terms, project))
}

#[cfg(test)]
mod project_prompt_tests {
    use super::*;

    #[test]
    fn the_persons_words_come_first_and_the_projects_follow_within_the_limits() {
        let settings = crate::dictation::Settings { terms: vec!["Kubernetes".into(), "Aqua".into()], ..Default::default() };
        assert_eq!(prompt_with_project_terms(&settings, &[]), "Kubernetes, Aqua", "no project words: exactly what it was");
        assert_eq!(prompt_with_project_terms(&settings, &["parse_strict".into(), "aqua".into(), "AppState".into()]), "Kubernetes, Aqua, parse_strict, AppState");
        let many: Vec<String> = (0..300).map(|i| format!("project_term_{i:03}")).collect();
        let prompt = prompt_with_project_terms(&settings, &many);
        assert!(prompt.starts_with("Kubernetes, Aqua, project_term_000") && prompt.chars().count() <= 800 && prompt.split(", ").count() <= 200, "{}", prompt.len());
        assert_eq!(prompt_with_project_terms(&crate::dictation::Settings::default(), &["only_project".into()]), "only_project");
    }
}
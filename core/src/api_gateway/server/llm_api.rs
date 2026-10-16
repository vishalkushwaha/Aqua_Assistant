//! The local language model as routes (card OA3-06, moved out of `server.rs`): the models in the folder and the choice of one, a prompt answered in one piece (`/llama/generate`) and as a stream of tokens (`/llama/generate_stream`).

use super::*;

#[derive(Deserialize)]
pub struct LlamaPromptRequest {
    pub prompt: String,
    /// Optional GGUF basename under `data/models/llm/` for this request only.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Serialize)]
pub struct LlamaPromptResponse {
    pub response: String,
    pub latency_ms: u64,
    /// Tokens written per second as the model server measured them (`tokens_per_sec_is_estimate` is `false`). When the answer came from `llama-cli`, which measures nothing, it is
    /// (chars / 4) / wall-clock seconds, and `tokens_per_sec_is_estimate` is `true`.
    pub tokens_per_sec: f32,
    pub tokens_per_sec_is_estimate: bool,
    pub active_model: String,
}

#[derive(Serialize)]
pub struct LlamaModelsResponse {
    pub models: Vec<crate::modules::llm_router::GgufModelInfo>,
    pub active_model: String,
    pub active_filename: String,
    pub engine_ready: bool,
    pub model_dir: String,
    /// The model each role uses and why (card OA9-03), so that the screen that lists the models can say which one the agent really runs.
    pub roles: Vec<crate::modules::llm_router::RoleView>,
}

#[derive(Deserialize)]
pub struct LlamaSetActiveModelRequest {
    /// GGUF basename, e.g. `qwen-0.5b-instruct-q4_k_m.gguf`
    pub model: String,
}

#[derive(Deserialize)]
pub struct LlamaImportModelRequest {
    /// Full path of a `.gguf` already on this machine. It is copied into the model folder.
    pub path: String,
}

async fn llama_list_models_endpoint(State(state): State<AppState>) -> Json<LlamaModelsResponse> {
    Json(llama_models_json(&state))
}

async fn llama_set_active_model_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<LlamaSetActiveModelRequest>,
) -> Response {
    // Which model answers (and decides what tools to use) is part of what Aqua trusts: UI token only.
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    // "Use": the model is the active one and is chosen for the agent and for chat, so the Qwen3-8B preference no longer hides it (card OA9-03, finding D-PR-2).
    match state.llm.use_model(&payload.model) {
        Ok(()) => {
            let _ = state.db.set_setting("llm_active_gguf", &payload.model);
            aqua_tool_host::record_direct("ui", "models.use", &payload.model, true, 0);
            (axum::http::StatusCode::OK, Json(llama_models_json(&state))).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": e.to_string(),
                "active_filename": state.llm.active_model_filename(),
            })),
        )
            .into_response(),
    }
}

fn llama_models_json(state: &AppState) -> LlamaModelsResponse {
    LlamaModelsResponse {
        models: state.llm.list_models(),
        active_model: state.llm.active_model_name(),
        active_filename: state.llm.active_model_filename(),
        engine_ready: state.llm.is_engine_ready(),
        model_dir: state.llm.model_dir().display().to_string(),
        roles: state.llm.role_choices().iter().map(|c| c.view()).collect(),
    }
}

async fn llama_remove_model_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<LlamaSetActiveModelRequest>,
) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    match state.llm.remove_model(&payload.model) {
        Ok(()) => {
            let _ = state.db.set_setting("llm_active_gguf", &state.llm.active_model_filename());
            (axum::http::StatusCode::OK, Json(llama_models_json(&state))).into_response()
        }
        Err(e) => api_error(axum::http::StatusCode::BAD_REQUEST, &e.to_string()),
    }
}

async fn llama_import_model_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<LlamaImportModelRequest>,
) -> Response {
    if let Some(denied) = ui_scope_required(scope) {
        return denied;
    }
    let source = std::path::PathBuf::from(payload.path.trim());
    let model_dir = state.llm.model_dir().to_path_buf();
    // A file that cannot be imported is refused before the model server is stopped for it (stopping it costs the next request a cold start).
    if let Err(e) = crate::modules::llm_router::llama_cli::check_importable(&source) {
        return api_error(axum::http::StatusCode::BAD_REQUEST, &e.to_string());
    }
    crate::modules::llm_router::llama_server::shutdown();
    let imported = tokio::task::spawn_blocking(move || crate::modules::llm_router::llama_cli::import_gguf(&model_dir, &source)).await;
    match imported {
        Ok(Ok(name)) => match state.llm.set_active_model(&name) {
            Ok(()) => {
                let _ = state.db.set_setting("llm_active_gguf", &name);
                (axum::http::StatusCode::OK, Json(llama_models_json(&state))).into_response()
            }
            Err(e) => api_error(axum::http::StatusCode::BAD_REQUEST, &e.to_string()),
        },
        Ok(Err(e)) => api_error(axum::http::StatusCode::BAD_REQUEST, &e.to_string()),
        Err(_) => api_error(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "import was interrupted"),
    }
}

async fn llama_generate_endpoint(
    State(state): State<AppState>,
    Json(payload): Json<LlamaPromptRequest>,
) -> Response {
    let start_time = std::time::Instant::now();
    info!("Dispatched a {}-character prompt to the Llama.cpp gateway", payload.prompt.chars().count());

    // A task like any other: it waits its turn behind the one running, and Stop (or hanging up) ends it.
    let _turn = match take_turn().await {
        Ok(t) => t,
        Err(busy) => return busy,
    };
    let llm = state.llm.clone();
    let prompt = payload.prompt.clone();
    let model_override = payload.model.clone();
    let (generated_text, timings) = match run_turn(move || {
        // Whatever an earlier request left is not this one's; the server's own numbers for this one are taken as soon as it ends, on the thread that made it.
        let _ = crate::modules::llm_router::llama_engine::take_last_timings();
        let text = llm.generate_response_with_model(&prompt, model_override.as_deref()).map_err(|e| format!("{e:#}"))?;
        Ok((text, crate::modules::llm_router::llama_engine::take_last_timings()))
    })
    .await
    {
        Ok(done) => done,
        Err(e) if e.trim_start().to_lowercase().starts_with("stopped") => return pipeline_failure(&e),
        Err(e) => return upstream_error("llama", anyhow::anyhow!(e)),
    };

    let latency_ms = start_time.elapsed().as_millis() as u64;
    let (tokens_per_sec, tokens_per_sec_is_estimate) = generation_speed(timings, &generated_text, latency_ms);

    let active_model = payload
        .model
        .as_deref()
        .map(|m| format!("{} (GGUF)", crate::modules::llm_router::llama_cli::display_name_from_filename(m)))
        .unwrap_or_else(|| format!("{} (GGUF)", state.llm.active_model_name()));

    Json(LlamaPromptResponse {
        response: generated_text,
        latency_ms,
        tokens_per_sec,
        tokens_per_sec_is_estimate,
        active_model,
    })
    .into_response()
}

/// How fast the answer was written, and whether that is a measurement. The model server counts the tokens it wrote and the time it took; with those the figure is exact (to a tenth).
/// Without them (the answer came from `llama-cli`, which measures nothing) it is a guess, (characters / 4) / seconds of the whole request, and says so.
pub(super) fn generation_speed(timings: Option<crate::modules::llm_router::llama_server::Timings>, text: &str, latency_ms: u64) -> (f32, bool) {
    let round = |v: f64| ((v * 10.0).round() / 10.0) as f32;
    match timings.filter(|t| t.predicted_n > 0 && t.predicted_ms > 0) {
        Some(t) => (round(t.decode_tokens_per_second()), false),
        None => {
            let sec = (latency_ms as f64 / 1000.0).max(0.001);
            (round((text.chars().count() as f64 / 4.0) / sec), true)
        }
    }
}

/// Server-sent events for a prompt. It takes the same turn permit and deadline as every other model request and uses the
/// one resident model server (it used to start a second copy of the 8B model through `llama-cli`, with no queue, no
/// time limit and a canned reply when the model was missing). The answer is produced first and then sent in word-sized
/// chunks; a failure arrives as an `error` event, not as a stream that just ends.
async fn llama_generate_stream_endpoint(
    State(state): State<AppState>,
    axum::extract::Query(shape): axum::extract::Query<TokenStreamQuery>,
    Json(payload): Json<LlamaPromptRequest>,
) -> axum::response::Response {
    use tokio_stream::StreamExt;
    let json_tokens = shape.format.as_deref().is_some_and(|f| f.trim().eq_ignore_ascii_case("json"));
    let turn = match take_turn().await {
        Ok(t) => t,
        Err(busy) => return busy,
    };
    let request_session_id = state.active_session_id.load(std::sync::atomic::Ordering::SeqCst);
    info!("Dispatched a {}-character streaming prompt to the Llama.cpp gateway [session={request_session_id}]", payload.prompt.chars().count());
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, String>>(100);

    let llm = state.llm.clone();
    let prompt = payload.prompt.clone();
    let model_override = payload.model.clone();
    let active_session = state.active_session_id.clone();

    tokio::spawn(async move {
        let _turn = turn; // held until the answer has been produced, or the client has gone
        // The playground's Stop closes the connection. Nothing used to look for that: the turn went on to the end of the answer (up to a
        // minute and a half on this machine) and held the turn permit, so "Hey Aqua" waited behind an answer that nobody was reading. Dropping
        // the turn's future hangs up on the model, which keeps its model loaded.
        let work = run_turn(move || llm.generate_response_with_model(&prompt, model_override.as_deref()).map_err(|e| format!("{e:#}")));
        let Some(result) = until_the_client_leaves(work, tx.closed()).await else {
            info!("the playground client left; the model call was stopped");
            return;
        };
        match result {
            Ok(text) => {
                for chunk in text.split_inclusive(' ') {
                    if tx.send(Ok(chunk.to_string())).await.is_err() {
                        break; // the client went away
                    }
                }
            }
            Err(e) => {
                tracing::warn!("streaming prompt failed: {e}");
                let (_, message) = describe_pipeline_error(&e);
                let _ = tx.send(Err(message)).await;
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).filter_map(move |item| {
        let current = active_session.load(std::sync::atomic::Ordering::SeqCst);
        if current != request_session_id {
            info!("Aborting stale stream token output [request_session={request_session_id}, current={current}]");
            return None;
        }
        Some(Ok::<_, std::convert::Infallible>(match item {
            Ok(token) => axum::response::sse::Event::default().data(token_data(&token, json_tokens)),
            Err(message) => axum::response::sse::Event::default().event("error").data(message),
        }))
    });

    axum::response::Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()).into_response()
}

/// The query of `POST /api/v1/llama/generate_stream`: `?format=json` sends each piece as `{"t":"..."}` (card OA10-06, UX-08).
#[derive(Deserialize, Default)]
struct TokenStreamQuery {
    format: Option<String>,
}

/// What one piece of the token stream sends as its `data:`. By default the piece as it always was, with its line breaks made spaces (an event's data is one line, and the playground read
/// it that way). With `?format=json`, `{"t":"<piece>"}`: the JSON carries the line breaks as `\n`, so code, lists and paragraphs come out as they were written (card OA10-06, UX-08).
pub(crate) fn token_data(token: &str, json: bool) -> String {
    if json {
        serde_json::json!({ "t": token.replace("\r\n", "\n").replace('\r', "") }).to_string()
    } else {
        token.replace("\r\n", "\n").replace('\r', "").replace('\n', " ")
    }
}

/// `work`, unless `gone` completes first (the client hung up); then `work` is dropped, which is how a turn is told to stop.
pub(super) async fn until_the_client_leaves<F: std::future::Future>(work: F, gone: impl std::future::Future<Output = ()>) -> Option<F::Output> {
    tokio::pin!(work);
    tokio::select! {
        result = &mut work => Some(result),
        () = gone => None,
    }
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/llama/generate", post(llama_generate_endpoint).layer(axum::extract::DefaultBodyLimit::max(256 * 1024)))
        .route("/api/v1/llama/generate_stream", post(llama_generate_stream_endpoint).layer(axum::extract::DefaultBodyLimit::max(256 * 1024)))
        .route("/api/v1/llama/models", get(llama_list_models_endpoint).post(llama_set_active_model_endpoint).delete(llama_remove_model_endpoint))
        .route("/api/v1/llama/models/import", post(llama_import_model_endpoint))
}

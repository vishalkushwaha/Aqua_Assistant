//! The streamed turn (card OA3-06, moved out of `server.rs`): the server-sent events of `/pipeline/text/stream` and `/pipeline/wav/stream`, and the machinery they and the tests share (`stream_turn_with`, `end_stream`, `step_hook`).

use super::*;

// ---- Streamed turns: the answer is spoken piece by piece while it is still being written ----------------------------------------
//
// `/api/v1/pipeline/text/stream` and `/api/v1/pipeline/wav/stream` take the same requests as `/pipeline/text` and `/pipeline/wav`, and
// answer with server-sent events instead of one JSON body:
//
//   event: heard    {"text": "what is the date"}                              voice only, once, as soon as the words are known
//   event: step     {"title": "Plan with llama.cpp", "status": "running"}     what the agent is doing, as it happens (additive: a client that does not know it ignores it)
//   event: approval a question that needs the person's answer (the data of one entry of `pending` in GET /browser/confirmations)   UI token only; additive; answer it with POST /browser/confirm
//   event: speech   {"index": 0, "text": "Today is Saturday.", "wav_base64": "..."}   one per spoken piece, in order; the sound is empty
//                                                                              when speech could not be made, so the words still arrive
//   event: done     the JSON of the plain endpoint, with an empty `tts_wav_base64`, `"streamed": true` and `first_audio_ms`; for the UI token also `question`
//                   (`{"text", "choices"}`) when the agent asked something and waits for the answer, which is the next request
//   event: error    {"error": "...", "status": 503}                           the turn failed; no `done` follows
//
// A client that hangs up stops the turn. The turn permit, the queue, the time limit and the cancel flag are the plain endpoints'.

/// One thing a streamed turn sends: an event, or the end of the stream.
enum Frame {
    Event(axum::response::sse::Event),
    End,
}

type FrameTx = tokio::sync::mpsc::UnboundedSender<Frame>;

/// "No sound has been sent yet", for the time of the first sound.
const NO_SOUND_YET: u64 = u64::MAX;

/// One event of a streamed turn. The kind is a [`StreamEvent`], not a string: a kind that `stream_events` does not list cannot be sent, and the page of the app asks that every
/// one listed is described (card OA3-01, AQ-04).
fn sse_event(kind: StreamEvent, data: serde_json::Value) -> axum::response::sse::Event {
    axum::response::sse::Event::default().event(kind.name()).data(data.to_string())
}

/// The events a client reads: everything sent up to the first `End`. A thread that is still winding down after the end sends into a
/// closed channel, which tells it that nobody is listening.
fn sse_response(rx: tokio::sync::mpsc::UnboundedReceiver<Frame>) -> Response {
    use tokio_stream::StreamExt;
    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
        .take_while(|frame| matches!(frame, Frame::Event(_)))
        .filter_map(|frame| match frame {
            Frame::Event(event) => Some(Ok::<_, std::convert::Infallible>(event)),
            Frame::End => None,
        });
    axum::response::sse::Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()).into_response()
}

/// A stream of one event (a request that is answered at once).
fn one_event_response(kind: StreamEvent, data: serde_json::Value) -> Response {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let _ = tx.send(Frame::Event(sse_event(kind, data)));
    let _ = tx.send(Frame::End);
    sse_response(rx)
}

/// Turns what a streamed turn tells its listener into server-sent events, and times the first one that carries sound (milliseconds from
/// `started`). The listener it returns says `false` once the client has gone.
fn forward_events(tx: FrameTx, started: std::time::Instant, first_sound_ms: Arc<std::sync::atomic::AtomicU64>) -> impl FnMut(crate::pipeline::TurnEvent) -> bool {
    use crate::pipeline::TurnEvent;
    use std::sync::atomic::Ordering::SeqCst;
    let mut index = 0usize;
    move |event| {
        let event = match event {
            TurnEvent::Heard(text) => sse_event(StreamEvent::Heard, serde_json::json!({ "text": text })),
            TurnEvent::Step { title, status } => sse_event(StreamEvent::Step, serde_json::json!({ "title": title, "status": status })),
            TurnEvent::Speech { text, wav } => {
                if !wav.is_empty() {
                    // Only the first sound counts.
                    let _ = first_sound_ms.compare_exchange(NO_SOUND_YET, started.elapsed().as_millis() as u64, SeqCst, SeqCst);
                }
                let wav_base64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &wav);
                index += 1;
                sse_event(StreamEvent::Speech, serde_json::json!({ "index": index - 1, "text": text, "wav_base64": wav_base64 }))
            }
        };
        tx.send(Frame::Event(event)).is_ok()
    }
}

/// Tells the client of a streamed turn what the agent is doing: one `step` event per phase it reports (the title and whether it is
/// running, done or failed), never the arguments or results of what it calls. The step is built by `pipeline::step_event` (a `TurnEvent::Step`, card R6-22).
///
/// For the person's own stream (`ui`, the UI token) whose turn is an agent run that is being recorded, the first step is preceded by a `run` event (which run, and where to read its
/// events again) and each `step` carries the number of its place in the run's log as `id:` (card OA10-01): a client that was cut off takes the log up again with
/// `GET /api/v1/runs/{id}/stream?after=<the last id it saw>`. The hook runs on the thread that records the run, right after the record has heard the step, so the number is exact.
fn step_hook(tx: FrameTx, ui: bool) -> Arc<dyn Fn(&AgentStep) + Send + Sync> {
    let announced = Arc::new(std::sync::atomic::AtomicBool::new(false));
    Arc::new(move |step: &AgentStep| {
        let Some(crate::pipeline::TurnEvent::Step { title, status }) = crate::pipeline::step_event(step) else { return };
        let place = if ui { crate::agent_runs::cursor().filter(|(_, seq)| *seq > 0) } else { None };
        if let Some((run, _)) = &place {
            if !announced.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let _ = tx.send(Frame::Event(sse_event(StreamEvent::Run, super::super::stream_events::run_event_data(run))));
            }
        }
        let event = sse_event(StreamEvent::Step, serde_json::json!({ "title": title, "status": status }));
        let event = match place {
            Some((_, seq)) => event.id(seq.to_string()),
            None => event,
        };
        let _ = tx.send(Frame::Event(event));
    })
}

fn first_sound(first_sound_ms: &std::sync::atomic::AtomicU64) -> Option<u64> {
    Some(first_sound_ms.load(std::sync::atomic::Ordering::SeqCst)).filter(|ms| *ms != NO_SOUND_YET)
}

/// The data of the final `done` event: the plain endpoint's JSON (without the sound, which has been sent), and when the first sound was
/// ready, in milliseconds from the start of the request.
pub(super) fn streamed_done(response: impl Serialize, first_sound_ms: Option<u64>) -> serde_json::Value {
    let mut data = serde_json::to_value(response).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(map) = data.as_object_mut() {
        map.insert("streamed".into(), true.into());
        map.insert("first_audio_ms".into(), first_sound_ms.map_or(serde_json::Value::Null, Into::into));
    }
    data
}

/// Sends the end of a streamed turn: a `done` event with `on_done(result, stages)` as its data, or an `error` event, and then closes the
/// stream. When the client hangs up first, `work` is dropped, which stops the turn (see `run_turn_timed`); `on_failure` runs in both
/// failing cases.
async fn end_stream<R>(
    tx: &FrameTx,
    work: impl std::future::Future<Output = Result<(R, Vec<crate::turn_timing::StageMs>), String>>,
    on_done: impl FnOnce(R, Vec<crate::turn_timing::StageMs>) -> serde_json::Value,
    on_failure: impl FnOnce(),
) {
    tokio::pin!(work);
    tokio::select! {
        outcome = &mut work => match outcome {
            Ok((result, stages)) => {
                let _ = tx.send(Frame::Event(sse_event(StreamEvent::Done, on_done(result, stages))));
            }
            Err(e) => {
                on_failure();
                tracing::warn!("streamed turn failed: {e}");
                let (status, code, message) = describe_pipeline_fault(&e);
                let _ = tx.send(Frame::Event(sse_event(StreamEvent::Error, serde_json::json!({ "error": message, "status": status.as_u16(), "code": code.as_str() }))));
            }
        },
        _ = tx.closed() => {
            on_failure();
            info!("the client hung up on a streamed turn; the turn is being stopped");
        }
    }
    let _ = tx.send(Frame::End);
}

/// Runs one streamed turn in the background and returns the stream of its events. `work` is the turn (it gets the listener that sends
/// its events to the client); `on_done` makes the data of the last event from the result, the stages and the time of the first sound;
/// `on_failure` runs when the turn fails or the client hangs up. `turn` is the queue's permit, held until the turn is over.
#[cfg(test)]
pub(super) fn stream_turn<R: Send + 'static>(
    turn: impl Send + 'static,
    start: std::time::Instant,
    work: impl FnOnce(&mut dyn FnMut(crate::pipeline::TurnEvent) -> bool) -> Result<R, String> + Send + 'static,
    on_done: impl FnOnce(R, Vec<crate::turn_timing::StageMs>, Option<u64>) -> serde_json::Value + Send + 'static,
    on_failure: impl FnOnce() + Send + 'static,
) -> Response {
    stream_turn_with(None, turn, start, work, on_done, on_failure)
}

/// [`stream_turn`] for a client that is also told, as an `approval` event, of each question that needs its answer while the turn runs (`approvals` is what
/// `stream_events::approvals_for` gave: nothing for a stream asked for with the API token). The forwarding ends with the turn.
pub(super) fn stream_turn_with<R: Send + 'static>(
    approvals: Option<tokio::sync::broadcast::Receiver<aqua_mcp_adapters::browser::ConfirmRequest>>,
    turn: impl Send + 'static,
    start: std::time::Instant,
    work: impl FnOnce(&mut dyn FnMut(crate::pipeline::TurnEvent) -> bool) -> Result<R, String> + Send + 'static,
    on_done: impl FnOnce(R, Vec<crate::turn_timing::StageMs>, Option<u64>) -> serde_json::Value + Send + 'static,
    on_failure: impl FnOnce() + Send + 'static,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let first_sound_ms = Arc::new(std::sync::atomic::AtomicU64::new(NO_SOUND_YET));
    let mut listener = forward_events(tx.clone(), start, first_sound_ms.clone());
    // The agent runs on the turn's own thread: while it does, each of its steps is sent to the client as a `step` event. (`approvals` is there for the person's own stream only.)
    let steps = step_hook(tx.clone(), approvals.is_some());
    let approvals = approvals.map(|questions| {
        let to_client = tx.clone();
        super::super::stream_events::forward_approvals(questions, move |data| to_client.send(Frame::Event(sse_event(StreamEvent::Approval, data))).is_ok())
    });
    tokio::spawn(async move {
        let _turn = turn;
        let turn_work = run_turn_timed(move || crate::modules::browser_agent::with_step_hook(steps, || work(&mut listener)));
        end_stream(&tx, turn_work, |result, stages| on_done(result, stages, first_sound(&first_sound_ms)), on_failure).await;
        if let Some(forwarding) = approvals {
            forwarding.abort();
        }
    });
    sse_response(rx)
}

async fn pipeline_text_stream_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    axum::extract::Query(sound): axum::extract::Query<SoundQuery>,
    Json(payload): Json<PipelineTextRequest>,
) -> Response {
    let ui = scope == Scope::Ui;
    let speak = wants_sound(payload.tts, sound.tts.as_deref());
    let start = std::time::Instant::now();
    let turn = match take_turn().await {
        Ok(turn) => turn,
        Err(busy) => return busy,
    };
    let queued_ms = start.elapsed().as_millis() as u64;
    // The person's own stream is told of each question that needs their answer as it is opened (an `approval` event); another is not.
    let approvals = super::super::stream_events::approvals_for(ui, state.browser.broker());
    stream_turn_with(
        approvals,
        turn,
        start,
        move |listener| {
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
                    Some(listener),
                    speak,
                )
                .map_err(|e| format!("{e:#}"))
            })
        },
        move |result, mut stages_ms, first_sound_ms| {
            stages_ms.insert(0, crate::turn_timing::StageMs { stage: "queued", ms: queued_ms });
            let latency_ms = start.elapsed().as_millis() as u64;
            info!("pipeline/text/stream done in {latency_ms}ms, first sound at {first_sound_ms:?}ms");
            streamed_done(PipelineTextResponse { result, question: question_for(ui), latency_ms, stages_ms }, first_sound_ms)
        },
        || {},
    )
}

async fn pipeline_wav_stream_endpoint(State(state): State<AppState>, axum::Extension(scope): axum::Extension<Scope>, Json(payload): Json<PipelineWavRequest>) -> Response {
    let ui = scope == Scope::Ui;
    let start = std::time::Instant::now();
    let session_open = crate::voice_commands::voice_session().is_active(unix_now());
    let audio = audio_key(&payload.wav_base64);
    if is_duplicate_key(audio) {
        info!("pipeline/wav/stream: identical audio was just received; ignoring the second copy");
        let ignored = PipelineWavResponse {
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
        };
        return one_event_response(StreamEvent::Done, streamed_done(ignored, None));
    }
    // A request that could not even get a turn (busy) is not a duplicate when the person tries again.
    let turn = match take_turn().await {
        Ok(turn) => turn,
        Err(busy) => {
            forget_audio(audio);
            return busy;
        }
    };
    let queued_ms = start.elapsed().as_millis() as u64;
    // Read only now that this turn has its place in the queue: an earlier turn (say "go to sleep") has finished and updated the
    // session, so this one cannot run on a stale answer.
    let session_open = crate::voice_commands::voice_session().is_active(unix_now());
    let wake_outer = state.db.get_setting("wakeup_word").ok().flatten().unwrap_or_else(|| "Hey Aqua".to_string());
    info!("pipeline/wav/stream request session_open={} bytes_b64={}", session_open, payload.wav_base64.len());
    // The permit is held until the session has been updated in `on_done`: the next turn does not read the session before.
    let approvals = super::super::stream_events::approvals_for(ui, state.browser.broker());
    stream_turn_with(
        approvals,
        turn,
        start,
        move |listener| for_client(ui, || wav_turn_work(&state, &payload, session_open, Some(listener))),
        move |result: crate::pipeline::PipelineWavResult, mut stages_ms, first_sound_ms| {
            info!(
                "pipeline/wav done in {}ms heard={} chars action={:?} local={} first_sound={first_sound_ms:?}ms (streamed)",
                start.elapsed().as_millis(),
                result.transcribed_text.chars().count(),
                result.session_action,
                result.local_command
            );
            stages_ms.insert(0, crate::turn_timing::StageMs { stage: "queued", ms: queued_ms });
            let session = crate::voice_commands::voice_session();
            crate::conversation::with_client(ui, || session.after_turn(&result.transcribed_text, &wake_outer, crate::voice_commands::turn_was_accepted(&result.assistant_text), unix_now()));
            let response = PipelineWavResponse { result, question: question_for(ui), latency_ms: start.elapsed().as_millis() as u64, stages_ms, session_active: session.is_active(unix_now()) };
            streamed_done(response, first_sound_ms)
        },
        move || forget_audio(audio),
    )
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/pipeline/text/stream", post(pipeline_text_stream_endpoint).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)))
        .route("/api/v1/pipeline/wav/stream", post(pipeline_wav_stream_endpoint).layer(axum::extract::DefaultBodyLimit::max(AUDIO_BODY_LIMIT)))
}

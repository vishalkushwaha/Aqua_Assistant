use super::*;
use crate::pipeline::TurnEvent;
use axum::{routing::post, Router};
use http_body_util::BodyExt;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use tower::ServiceExt;

async fn post_to(app: Router) -> axum::response::Response {
    app.oneshot(axum::http::Request::builder().method("POST").uri("/turn").body(axum::body::Body::empty()).unwrap()).await.unwrap()
}

/// The events of a response, as (name, data) pairs, read to the end of the stream.
async fn events_of(resp: axum::response::Response) -> Vec<(String, serde_json::Value)> {
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    parse_events(&String::from_utf8_lossy(&bytes))
}

fn parse_events(text: &str) -> Vec<(String, serde_json::Value)> {
    text.split("\n\n")
        .filter(|block| block.contains("data:"))
        .map(|block| {
            let name = block.lines().find_map(|l| l.strip_prefix("event:")).unwrap_or("message").trim().to_string();
            let data: String = block.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim_start).collect::<Vec<_>>().join("\n");
            (name, serde_json::from_str(&data).unwrap_or(serde_json::Value::Null))
        })
        .collect()
}

fn speech(text: &str, sound: &[u8]) -> TurnEvent {
    TurnEvent::Speech { text: text.into(), wav: sound.to_vec() }
}

#[tokio::test]
async fn a_streamed_turn_sends_what_was_heard_then_each_piece_then_the_result() {
    let app = Router::new().route(
        "/turn",
        post(|| async {
            stream_turn(
                (),
                std::time::Instant::now(),
                |listener| {
                    assert!(listener(TurnEvent::Heard("what is the date".into())));
                    assert!(listener(speech("Today is Saturday.", b"RIFFone")));
                    assert!(listener(speech("It is sunny.", b"")));
                    crate::turn_timing::stage("chat", || ());
                    Ok::<_, String>("Today is Saturday. It is sunny.".to_string())
                },
                |answer, stages, first_sound_ms| streamed_done(serde_json::json!({ "assistant_text": answer, "stages": stages.len() }), first_sound_ms),
                || panic!("nothing failed"),
            )
        }),
    );
    let events = events_of(post_to(app).await).await;
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["heard", "speech", "speech", "done"]);
    assert_eq!(events[0].1["text"], "what is the date");
    assert_eq!((events[1].1["index"].as_u64(), events[1].1["text"].as_str()), (Some(0), Some("Today is Saturday.")));
    use base64::Engine;
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(events[1].1["wav_base64"].as_str().unwrap()).unwrap(), b"RIFFone", "the sound arrives as base64");
    assert_eq!((events[2].1["index"].as_u64(), events[2].1["wav_base64"].as_str()), (Some(1), Some("")), "a piece whose speech could not be made still carries its words");
    assert_eq!(events[3].1["assistant_text"], "Today is Saturday. It is sunny.");
    assert_eq!(events[3].1["streamed"], true);
    assert!(events[3].1["first_audio_ms"].is_u64(), "the first piece with sound is timed: {}", events[3].1);
    assert_eq!(events[3].1["stages"], 1);
}

#[tokio::test]
async fn what_the_turn_and_the_agent_are_doing_reaches_the_client_as_step_events_without_their_arguments() {
    use crate::modules::browser_agent::{phase_event, report_step};
    let app = Router::new().route(
        "/turn",
        post(|| async {
            stream_turn(
                (),
                std::time::Instant::now(),
                |listener| {
                    assert!(listener(TurnEvent::Step { title: "Checking your voice".into(), status: "running".into() }));
                    // The agent reports through the thread's hook (it runs on this thread, inside the turn).
                    report_step(&phase_event(1, "plan", "Plan with llama.cpp", "running", "Round 1/6: deciding what to do for \"password is hunter2\"", None, None, None));
                    // A step that is not a phase (a tool call with its arguments) is not sent.
                    report_step(&AgentStep { step: 2, kind: "tool".into(), detail: serde_json::json!({ "tool": "fs.read", "arguments": { "path": "C:/private/notes.txt" } }) });
                    report_step(&phase_event(2, "act", "Calling App", "completed", "done", None, None, None));
                    Ok::<_, String>(())
                },
                |_, _, first| streamed_done(serde_json::json!({ "assistant_text": "ok" }), first),
                || {},
            )
        }),
    );
    let events = events_of(post_to(app).await).await;
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["step", "step", "step", "done"], "{events:?}");
    assert_eq!(events[0].1, serde_json::json!({ "title": "Checking your voice", "status": "running" }));
    assert_eq!(events[1].1, serde_json::json!({ "title": "Plan with llama.cpp", "status": "running" }), "only the title and the status are sent: not the message, which can quote the request");
    assert_eq!(events[2].1, serde_json::json!({ "title": "Calling App", "status": "completed" }));
    assert!(!events.iter().any(|(_, d)| d.to_string().contains("hunter2") || d.to_string().contains("private")));
}

/// Card OA10-01 (UX-02): the voice turn's stream is a shim over the log of the run. For the person's own stream (the UI token) a `run` event comes before the first step and each `step`
/// carries its place in the log as `id:`; the bodies are what they were; a stream asked for with the API token has neither.
#[tokio::test]
async fn the_steps_of_a_recorded_run_reach_the_persons_stream_with_their_place_in_the_log_and_the_api_tokens_without() {
    use crate::modules::browser_agent::{phase_event, report_step};
    for ui in [true, false] {
        let dir = std::env::temp_dir().join(format!("aqua_step_hook_{ui}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Arc::new(crate::agent_runs::AgentRunStore::open(dir.join("agent_runs.db")).unwrap());
        let broker = aqua_mcp_adapters::browser::ConfirmationBroker::default();
        let resp = stream_turn_with(
            ui.then(|| broker.subscribe()),
            (),
            std::time::Instant::now(),
            {
                let store = store.clone();
                move |listener| {
                    assert!(listener(TurnEvent::Heard("read notes.txt".into())));
                    let rec = crate::agent_runs::Recording::begin_in(store, "run-77", "read notes.txt", "voice", "agent", None).unwrap();
                    report_step(&phase_event(1, "plan", "Plan with llama.cpp", "running", "Round 1/6", None, None, None));
                    report_step(&phase_event(2, "act", "Calling App", "completed", "done", None, None, None));
                    rec.finish("final", 1, "ok", &[]);
                    Ok::<_, String>(())
                }
            },
            |_, _, first| streamed_done(serde_json::json!({ "assistant_text": "ok" }), first),
            || {},
        );
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes).to_string();
        let blocks: Vec<(String, Option<String>, serde_json::Value)> = text
            .split("\n\n")
            .filter(|b| b.contains("data:"))
            .map(|b| {
                let name = b.lines().find_map(|l| l.strip_prefix("event:")).unwrap_or("message").trim().to_string();
                let id = b.lines().find_map(|l| l.strip_prefix("id:")).map(|i| i.trim().to_string());
                let data: String = b.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim_start).collect::<Vec<_>>().join("\n");
                (name, id, serde_json::from_str(&data).unwrap_or(serde_json::Value::Null))
            })
            .collect();
        let names: Vec<&str> = blocks.iter().map(|(n, _, _)| n.as_str()).collect();
        if ui {
            assert_eq!(names, ["heard", "run", "step", "step", "done"], "{blocks:?}");
            assert_eq!(blocks[1].2["run_id"], "run-77");
            assert_eq!((blocks[2].1.as_deref(), blocks[3].1.as_deref()), (Some("3"), Some("4")), "run.started 1, status 2, then one row for each phase");
        } else {
            assert_eq!(names, ["heard", "step", "step", "done"], "{blocks:?}");
            assert!(blocks.iter().all(|(_, id, _)| id.is_none()), "the API token's stream is the stream it always was");
        }
        // The bodies are what they were.
        let step = blocks.iter().filter(|(n, _, _)| n == "step").map(|(_, _, d)| d.clone()).collect::<Vec<_>>();
        assert_eq!(step[0], serde_json::json!({ "title": "Plan with llama.cpp", "status": "running" }));
        assert_eq!(step[1], serde_json::json!({ "title": "Calling App", "status": "completed" }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[tokio::test]
async fn a_turn_without_any_sound_says_so_in_the_result() {
    let app = Router::new().route(
        "/turn",
        post(|| async {
            stream_turn(
                (),
                std::time::Instant::now(),
                |listener| {
                    listener(speech("Words only.", b""));
                    Ok::<_, String>(())
                },
                |_, _, first| streamed_done(serde_json::json!({ "assistant_text": "Words only." }), first),
                || {},
            )
        }),
    );
    let events = events_of(post_to(app).await).await;
    let done = &events.last().unwrap().1;
    assert_eq!(done["streamed"], true);
    assert!(done["first_audio_ms"].is_null(), "no sound was made: {done}");
    assert_eq!(done["assistant_text"], "Words only.");
}

#[tokio::test]
async fn a_failed_turn_ends_with_an_error_event_and_no_result() {
    let failed = Arc::new(AtomicBool::new(false));
    let failed_in = failed.clone();
    let app = Router::new().route(
        "/turn",
        post(move || {
            let failed = failed_in.clone();
            async move {
                stream_turn(
                    (),
                    std::time::Instant::now(),
                    |listener| {
                        listener(speech("The first part was said.", b"RIFF"));
                        Err::<(), _>("the local language model is not available (see C:\\Users\\someone\\models)".to_string())
                    },
                    |_, _, _| serde_json::json!({}),
                    move || failed.store(true, SeqCst),
                )
            }
        }),
    );
    let events = events_of(post_to(app).await).await;
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["speech", "error"], "what was said stays said, and the end is an error: no `done`");
    let error = &events[1].1;
    assert_eq!(error["status"], 503);
    assert_eq!(error["code"], "model_unavailable", "the error event of a stream carries the code of the contract (card OA10-02): {error}");
    assert!(error["error"].as_str().unwrap().contains("not available") && !error["error"].as_str().unwrap().contains("someone"), "folders are hidden: {error}");
    assert!(failed.load(SeqCst), "the failure hook ran");
}

/// The client hangs up in the middle of a turn: the turn is told to stop (its cancel flag trips, and the next event it sends is refused),
/// and the failure hook runs, so that a retry of the same recording is not taken for a copy.
#[tokio::test]
async fn a_client_that_hangs_up_stops_the_turn() {
    let saw_cancel = Arc::new(AtomicBool::new(false));
    let was_refused = Arc::new(AtomicBool::new(false));
    let hook_ran = Arc::new(AtomicBool::new(false));
    let (saw, refused, hook) = (saw_cancel.clone(), was_refused.clone(), hook_ran.clone());
    let app = Router::new().route(
        "/turn",
        post(move || {
            let (saw, refused, hook) = (saw.clone(), refused.clone(), hook.clone());
            async move {
                stream_turn(
                    (),
                    std::time::Instant::now(),
                    move |listener| {
                        for i in 0..400 {
                            if crate::modules::browser_agent::turn_cancelled_for_test() {
                                saw.store(true, SeqCst);
                            }
                            if !listener(speech(&format!("piece {i}"), b"RIFF")) {
                                refused.store(true, SeqCst);
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(15));
                        }
                        Ok::<_, String>(())
                    },
                    |_, _, _| serde_json::json!({}),
                    move || hook.store(true, SeqCst),
                )
            }
        }),
    );
    let resp = post_to(app).await;
    let mut body = resp.into_body();
    let first = body.frame().await.expect("a first event").unwrap();
    assert!(first.into_data().is_ok_and(|d| String::from_utf8_lossy(&d).contains("piece 0")));
    drop(body); // the client is gone
    for _ in 0..200 {
        if saw_cancel.load(SeqCst) && was_refused.load(SeqCst) && hook_ran.load(SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(saw_cancel.load(SeqCst), "the turn was never told to stop");
    assert!(was_refused.load(SeqCst), "the turn kept sending to nobody");
    assert!(hook_ran.load(SeqCst), "the failure hook did not run");
}

/// Found in a review: the playground's stream went on with the model call after the client had left (Stop closes the connection), and held the
/// turn permit to the end of the answer.
#[tokio::test]
async fn work_is_dropped_when_the_client_leaves_and_finishes_when_it_stays() {
    struct Flag(Arc<AtomicBool>);
    impl Drop for Flag {
        fn drop(&mut self) {
            self.0.store(true, SeqCst);
        }
    }
    // The client leaves: the receiver is dropped, `closed()` completes, and the work (which would take a minute) is dropped at once.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, String>>(4);
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = Flag(dropped.clone());
    let work = async move {
        let _guard = guard;
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        "the answer"
    };
    let waiting = tokio::spawn(async move { until_the_client_leaves(work, tx.closed()).await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(!waiting.is_finished(), "the client is still there");
    drop(rx);
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), waiting).await.expect("the wait did not end when the client left").unwrap();
    assert_eq!(outcome, None);
    assert!(dropped.load(SeqCst), "the work was not dropped");
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    // The client stays: the work finishes and its result comes back.
    let (tx, _rx) = tokio::sync::mpsc::channel::<Result<String, String>>(4);
    let done = until_the_client_leaves(async { "the answer" }, tx.closed()).await;
    assert_eq!(done, Some("the answer"));
}

/// Card R6-22: a question that needs the person's answer is sent on the stream the moment it is opened, so that the app does not have to ask for the list every second.
#[tokio::test]
async fn a_question_that_needs_an_answer_is_sent_on_the_ui_stream_as_an_approval_event_while_the_turn_waits_for_it() {
    use aqua_mcp_adapters::browser::{ConfirmKind, ConfirmationBroker, Decision, Resolved};
    let broker = Arc::new(ConfirmationBroker::default());
    let for_route = broker.clone();
    let app = Router::new().route(
        "/turn",
        post(move || {
            let broker = for_route.clone();
            async move {
                let asking = broker.clone();
                stream_turn_with(
                    super::super::stream_events::approvals_for(true, &broker),
                    (),
                    std::time::Instant::now(),
                    move |listener| {
                        assert!(listener(TurnEvent::Step { title: "Calling App".into(), status: "running".into() }));
                        // The turn waits here until somebody answers, as an agent run waits for a yes.
                        let (_, decision) = asking.request(ConfirmKind::Tool, "term.run", "run a command on this computer".into(), None, None, std::time::Duration::from_secs(30));
                        Ok::<_, String>(decision)
                    },
                    |decision, _, first| streamed_done(serde_json::json!({ "decision": format!("{decision:?}") }), first),
                    || {},
                )
            }
        }),
    );
    let mut body = post_to(app).await.into_body();
    let mut seen = String::new();
    // Read until the approval has arrived: the turn cannot end before it is answered.
    while !seen.contains("event: approval") {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(60), body.frame()).await.expect("an event came").expect("the stream is open").unwrap();
        seen.push_str(&String::from_utf8_lossy(&frame.into_data().unwrap_or_default()));
    }
    assert!(seen.contains("event: step"), "the step came first: {seen}");
    let open = broker.pending();
    assert_eq!(open.len(), 1);
    assert_eq!(broker.resolve_typed(&open[0].request_id, Decision::Allow), Resolved::Applied);
    let rest = body.collect().await.unwrap().to_bytes();
    seen.push_str(&String::from_utf8_lossy(&rest));
    let events = parse_events(&seen);
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["step", "approval", "done"], "{events:?}");
    assert_eq!((events[1].1["tool"].as_str(), events[1].1["request_id"].as_str()), (Some("term.run"), Some(open[0].request_id.as_str())), "{}", events[1].1);
    assert_eq!(events[1].1["reason"], "run a command on this computer");
    assert_eq!(events[2].1["decision"], "Allow");
}

/// The question shows the exact action and its target: a stream asked for with the API token is not sent it (the list of questions is the UI's too).
#[tokio::test]
async fn a_stream_for_the_api_token_is_sent_no_approval_event_though_a_question_is_opened() {
    use aqua_mcp_adapters::browser::{ConfirmKind, ConfirmationBroker, Decision};
    let broker = Arc::new(ConfirmationBroker::default());
    let for_route = broker.clone();
    let app = Router::new().route(
        "/turn",
        post(move || {
            let broker = for_route.clone();
            async move {
                let asking = broker.clone();
                stream_turn_with(
                    super::super::stream_events::approvals_for(false, &broker),
                    (),
                    std::time::Instant::now(),
                    move |_| {
                        let (_, decision) = asking.request(ConfirmKind::Tool, "term.run", "run a command on this computer".into(), None, None, std::time::Duration::from_secs(30));
                        Ok::<_, String>(decision)
                    },
                    |decision, _, first| streamed_done(serde_json::json!({ "decision": format!("{decision:?}") }), first),
                    || {},
                )
            }
        }),
    );
    let answering = broker.clone();
    let answerer = tokio::spawn(async move {
        // The turn may wait for the one turn gate behind other tests of this module: look for the question for a good while.
        for _ in 0..3000 {
            if let Some(open) = answering.pending().first() {
                answering.resolve_typed(&open.request_id, Decision::Deny);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });
    let events = events_of(post_to(app).await).await;
    answerer.await.unwrap();
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, ["done"], "{events:?}");
    assert_eq!(events[0].1["decision"], "Deny");
}

/// Card R6-22: the question the agent asked is in the final event of the person's stream (and of the plain endpoint), so the orb can show it and keep listening for the answer.
#[test]
fn the_question_the_agent_waits_on_is_in_the_done_event_for_the_ui_token_and_left_out_when_there_is_none() {
    let result = crate::pipeline::PipelineTextResult {
        input_text: "close it".into(),
        assistant_text: "Which window do you mean?".into(),
        tts_sample_count: 0,
        tts_wav_base64: String::new(),
        tts_sample_rate: 24_000,
        llm_engine_ready: true,
        tts_engine_ready: true,
    };
    let asked = PipelineTextResponse { result: result.clone(), question: Some(serde_json::json!({ "text": "Which window do you mean?", "choices": ["Notepad", "Chrome"] })), latency_ms: 5, stages_ms: Vec::new() };
    let done = streamed_done(asked, Some(120));
    assert_eq!(done["question"], serde_json::json!({ "text": "Which window do you mean?", "choices": ["Notepad", "Chrome"] }), "{done}");
    assert_eq!((done["streamed"].clone(), done["first_audio_ms"].clone()), (serde_json::json!(true), serde_json::json!(120)));
    let nothing_asked = streamed_done(PipelineTextResponse { result, question: None, latency_ms: 5, stages_ms: Vec::new() }, None);
    assert!(nothing_asked.get("question").is_none(), "no question, no field: {nothing_asked}");
    // A script with the API token neither sees nor answers the question.
    assert!(question_for(false).is_none());
}

#[test]
fn the_streamed_routes_are_streams_and_are_never_cut_by_the_request_backstop() {
    for path in ["/api/v1/pipeline/text/stream", "/api/v1/pipeline/wav/stream"] {
        assert!(is_stream_path(path), "{path}");
    }
    assert!(!is_stream_path("/api/v1/pipeline/text") && !is_stream_path("/api/v1/pipeline/wav"));
}

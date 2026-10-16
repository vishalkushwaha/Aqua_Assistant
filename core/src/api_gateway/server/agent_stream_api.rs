//! The agent request as a stream of events (card OA3-06, moved out of `server.rs`): `POST /mcp/agent/stream` sends each step as it happens, the questions that need an answer and the result.

use super::*;

/// SSE stream of live agent steps, then a final `done` / `error` event.
///
/// The person's own app (the UI token) gets everything: each step whole, the questions that need its answer (`confirmation`), and the result. A script with the API token gets the phases
/// (number, kind, title, status, progress) and the answer, never a question or a tool's arguments and observations (card OA1-04, UX-01): `stream_events::step_for_scope`.
async fn mcp_unified_agent_stream_endpoint(
    State(state): State<AppState>,
    axum::Extension(scope): axum::Extension<Scope>,
    Json(payload): Json<UnifiedMcpAgentRequest>,
) -> impl IntoResponse {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use std::convert::Infallible;
    use tokio_stream::StreamExt;

    let ui = scope == Scope::Ui;

    // What the caller says the task is (card OA4-01): refused before the stream starts and before a turn is taken.
    let requested = match crate::modules::run_profile::parse_requested(payload.profile.as_deref()) {
        Ok(p) => p,
        Err(why) => return api_error(axum::http::StatusCode::BAD_REQUEST, &why),
    };
    // The session the caller names (card OA10-04): refused before the stream starts and before a turn is taken.
    let session = match super::super::sessions_api::named_session(payload.session_id.as_deref(), ui) {
        Ok(s) => s,
        Err(refused) => return refused,
    };
    // The agent the request asks the run to be (card OA11-04): refused before the stream starts and before a turn is taken.
    let agent = match requested_agent(payload.agent.as_deref()) {
        Ok(a) => a,
        Err(refused) => return refused,
    };
    // An agent task takes its turn in the queue like every other task. The turn is held until the agent has finished.
    let turn = match take_turn().await {
        Ok(t) => t,
        Err(busy) => return busy,
    };
    let run_options = match run_options_of(&payload.allowed_tools, &payload.forced_packs, payload.tainted_start) {
        Ok(o) => o,
        Err(why) => return api_error(axum::http::StatusCode::BAD_REQUEST, &why),
    };
    let llm = state.llm.clone();
    let browser_mcp = state.browser.clone();
    let windows_mcp = state.windows_mcp.clone();
    let laya = state.laya.clone();
    let goal = payload.goal;
    let max_steps = payload.max_steps.clamp(1, crate::modules::browser_agent::max_rounds_for_request(&goal, requested));
    let use_browser = payload.use_browser;
    let use_windows = payload.use_windows;
    let force_agent = payload.force_agent || requested.is_some_and(|p| p.works_on_files());

    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    // Every event of this stream is made by the sink, which applies the caller's scope to each one (card OA1-04, UX-01): this handler holds the stream only through it, so a step, the
    // result or a question cannot be sent whole to a caller that holds only the API token (card OA3-01 made that true by construction; a test in `stream_events` runs it).
    let sink = super::super::stream_events::AgentStreamSink::new(ui, tx);

    // Forward browser confirmation requests (unknown site, Send/Pay/Delete, password fields)
    // to this stream so the UI can ask the user while the agent waits. Only the person's own stream is told (the UI token): a question shows the exact action and its target, and the
    // API token cannot answer it (`stream_events::forward_confirmations`, the rule of the pipeline streams).
    let forwarder = sink.confirmations(state.browser.broker());
    // Run through the same runner as every other turn: Stop and the queue see it, and it ends at its next step when stopped.
    let agent_task = tokio::spawn(run_turn(move || {
      crate::agent_runs::with_origin(ui, move || {
        let send_phase = |sink: &super::super::stream_events::AgentStreamSink, step: AgentStep| {
            let _ = sink.step(&step);
        };

        send_phase(
            &sink,
            phase_event(
                0,
                "receive",
                "Receive goal",
                "completed",
                goal.clone(),
                Some("Laya gate: route refuse / chat / mcp_agent"),
                Some(0),
                Some(payload_max_total(max_steps)),
            ),
        );
        // "open the video folder" and a follow-up "1" are this computer, not a chat reply.
        if let Some(answer) = aqua_mcp_adapters::local_files::handle_request(&goal) {
            send_phase(
                &sink,
                phase_event(
                    0,
                    "finalize",
                    "Search this computer",
                    "completed",
                    answer.clone(),
                    None,
                    Some(1),
                    Some(1),
                ),
            );
            let _ = sink.done(&McpAgentResult {
                final_answer: answer,
                steps: vec![],
                tool_rounds: 0,
                stopped_reason: "local_search".into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            });
            return;
        }
        if let Some(answer) = crate::live_weather::handle_request(&goal) {
            send_phase(
                &sink,
                phase_event(
                    0,
                    "finalize",
                    "Weather",
                    "completed",
                    "Answered from the weather service",
                    None,
                    Some(1),
                    Some(1),
                ),
            );
            let _ = sink.done(&McpAgentResult {
                final_answer: answer,
                steps: vec![],
                tool_rounds: 0,
                stopped_reason: "weather".into(),
                gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
            });
            return;
        }

        send_phase(
            &sink,
            phase_event(
                0,
                "gate",
                "Laya gate",
                "running",
                "Classifying intent & safety…",
                Some("Prepare MCP or answer as chat"),
                Some(0),
                Some(payload_max_total(max_steps)),
            ),
        );

        let mcp_available = use_browser || use_windows;
        let laya_arc = laya.clone();
        let goal_for_gate = goal.clone();
        let sink_tick = sink.clone();
        let decision = laya_arc.decide_with_timeout(
            &goal_for_gate,
            mcp_available,
            LayaGate::gate_timeout(),
            |secs| {
                let _ = sink_tick.step(&phase_event(
                    0,
                    "gate",
                    "Laya gate",
                    "running",
                    format!(
                        "Still classifying… {secs}s (neural gate; release builds are much faster)"
                    ),
                    Some("Prepare MCP or answer as chat"),
                    Some(0),
                    Some(payload_max_total(max_steps)),
                ));
            },
        );
        let decision = crate::laya_validator::refine_route(decision, &goal_for_gate, &llm, mcp_available);
        let decision = if force_agent { decision.forced_to_agent(mcp_available) } else { decision };

        send_phase(
            &sink,
            phase_event(
                0,
                "gate",
                "Laya gate",
                "completed",
                format!(
                    "route={} source={} surface={} ({} ms)",
                    decision.route.as_str(),
                    decision.source,
                    decision.surface.as_str(),
                    decision.latency_ms
                ),
                Some(match decision.route {
                    GateRoute::McpAgent => "Prepare MCP tools / plan with llama.cpp",
                    GateRoute::Chat => "Answer with llama.cpp (no tools)",
                    GateRoute::Refuse => "Refuse unsafe request",
                }),
                Some(1),
                Some(payload_max_total(max_steps)),
            ),
        );

        let send_done = {
            let sink = sink.clone();
            move |result: McpAgentResult| {
                let _ = sink.done(&result);
            }
        };

        match decision.route {
            GateRoute::Refuse => {
                send_phase(
                    &sink,
                    phase_event(
                        0,
                        "finalize",
                        "Refuse",
                        "completed",
                        "Request blocked by safety gate",
                        None,
                        Some(payload_max_total(max_steps)),
                        Some(payload_max_total(max_steps)),
                    ),
                );
                send_done(
                    McpAgentResult {
                        final_answer: crate::laya_validator::GateDecision::refuse_reply().into(),
                        steps: vec![],
                        tool_rounds: 0,
                        stopped_reason: "laya_gate".into(),
                        gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                    }
                    .with_gate(decision),
                );
                return;
            }
            GateRoute::Chat => {
                send_phase(
                    &sink,
                    phase_event(
                        0,
                        "plan",
                        "Chat with llama.cpp",
                        "running",
                        "Tools not needed — generating chat reply…",
                        Some("Return answer"),
                        Some(2),
                        Some(payload_max_total(max_steps)),
                    ),
                );
                let answer = llm
                    .generate_chat_response(&grounded_chat_prompt(&goal))
                    .unwrap_or_else(|e| format!("LLM error: {e}"));
                send_phase(
                    &sink,
                    phase_event(
                        0,
                        "finalize",
                        "Finalize answer",
                        "completed",
                        "Chat reply ready",
                        None,
                        Some(payload_max_total(max_steps)),
                        Some(payload_max_total(max_steps)),
                    ),
                );
                send_done(
                    McpAgentResult {
                        final_answer: crate::modules::llm_router::strip_reasoning_tags(&answer),
                        steps: vec![],
                        tool_rounds: 0,
                        stopped_reason: "laya_gate".into(),
                        gate: None, run_id: None, question: None, choices: Vec::new(), sources: Vec::new(),
                    }
                    .with_gate(decision),
                );
                return;
            }
            GateRoute::McpAgent => {}
        }

        let browser = if use_browser {
            Some(browser_mcp)
        } else {
            None
        };
        let windows = if use_windows {
            Some(windows_mcp)
        } else {
            None
        };
        let hint = decision.surface.agent_prompt_line();

        let (step_tx, step_rx) = std::sync::mpsc::channel::<AgentStep>();
        let sink_steps = sink.clone();
        let forwarder = std::thread::spawn(move || {
            while let Ok(step) = step_rx.recv() {
                if !sink_steps.step(&step) {
                    break;
                }
            }
        });

        let run = crate::agent_runs::scope::with_run_options(run_options, || {
            crate::modules::run_profile::with_requested_profile(requested, || {
                crate::agent_runs::with_session(session, || crate::agents::with_requested(agent, || run_mcp_agent_shared_with_progress(&llm, browser, windows, &goal, max_steps, hint, Some(step_tx))))
            })
        });
        let _ = forwarder.join();

        match run {
            Ok(out) => {
                if out.tool_rounds > 0 {
                    laya.feedback_route(&goal, GateRoute::McpAgent);
                }
                send_done(out.with_gate(decision));
            }
            Err(e) => {
                send_phase(
                    &sink,
                    phase_event(
                        0,
                        "finalize",
                        "Agent error",
                        "failed",
                        e.to_string(),
                        None,
                        None,
                        None,
                    ),
                );
                let _ = sink.error(&e.to_string());
            }
        }
      });
      Ok::<(), String>(())
    }));
    // End the stream once the agent finishes: stop forwarding so the last sender drops, and give the turn back.
    tokio::spawn(async move {
        let _ = agent_task.await;
        if let Some(forwarder) = forwarder {
            forwarder.abort();
        }
        drop(turn);
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<Event, Infallible>);
    Sse::new(stream).keep_alive(KeepAlive::default()).into_response()
}

fn payload_max_total(max_steps: u32) -> u32 {
    max_steps.clamp(1, crate::modules::browser_agent::AGENT_MAX_ROUNDS).saturating_add(3)
}

/// The routes of this file (card OA3-06): `api_routes` merges them into the route table of the gateway.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/mcp/agent/stream", post(mcp_unified_agent_stream_endpoint))
}

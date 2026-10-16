//! What a turn answers: the programs that need no model (memory, reminders, the clock, files, the weather, opening a program), then the safety gate, then chat or the tool agent; how a typed or a spoken request differs; what is remembered of the turn (move only: this was part of `pipeline.rs`).

use super::*;

/// Ends the turn with a "stopped" error if it was told to stop (the caller hung up, or Stop was pressed). Checked between
/// the stages of a turn, so a stop never has to wait for the whole chain.
fn ensure_not_stopped() -> Result<()> {
    if crate::modules::browser_agent::turn_cancelled() {
        anyhow::bail!("stopped: the task was cancelled");
    }
    Ok(())
}

/// How a request reached Aqua. Typing is deliberate; a microphone also picks up the room.
#[derive(Clone, Copy)]
pub(super) enum Heard<'a> {
    Typed,
    /// Through the microphone, with the wake phrase that is set.
    Spoken { wake_phrase: &'a str },
}

impl<'a> Heard<'a> {
    pub(super) fn wake_phrase(self) -> &'a str {
        match self {
            Heard::Typed => "Hey Aqua",
            Heard::Spoken { wake_phrase } => wake_phrase,
        }
    }

    pub(super) fn is_spoken(self) -> bool {
        matches!(self, Heard::Spoken { .. })
    }
}

/// A spoken sentence that is not a request (the television, someone else in the room, the assistant's own voice picked up
/// again) must not start the tool agent: it holds the one language model for minutes (6 of the 25 agent runs in three days of
/// logs were not requests, and five took 205 to 301 seconds). It is answered in chat instead, which takes seconds and cannot do
/// anything. Typed text is deliberate and never checked. Says whether it changed the route.
pub(super) fn keep_non_requests_out_of_the_agent(heard: Heard<'_>, prompt: &str, decision: &mut GateDecision) -> bool {
    if !heard.is_spoken() || decision.route != GateRoute::McpAgent || crate::spoken_request::is_request_like(prompt) {
        return false;
    }
    decision.route = GateRoute::Chat;
    decision.needs_tools = false;
    true
}

/// Prefer MCP tool agent when clients are provided; otherwise plain LLM.
/// Laya gate decides refuse / chat / mcp_agent before any heavy work.
/// Always strips chain-of-thought so TTS does not speak `<think>` blocks.
pub(super) fn generate_assistant_reply(
    llm: &LlamaRouterEngine,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    prompt: &str,
    heard: Heard<'_>,
    laya: Option<&Arc<LayaGate>>,
) -> Result<String> {
    generate_assistant_reply_speaking(llm, browser, windows, prompt, heard, laya, None)
}

/// [`generate_assistant_reply`] for a streamed turn: a plain chat answer is spoken through `speaker` as it is written (and the caller
/// then finds that something has been spoken). Every other kind of answer (a refusal, a local fact, the agent's) is returned whole,
/// for the caller to speak.
pub(super) fn generate_assistant_reply_speaking(
    llm: &LlamaRouterEngine,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    prompt: &str,
    heard: Heard<'_>,
    laya: Option<&Arc<LayaGate>>,
    speaker: Option<&mut Speaker<'_>>,
) -> Result<String> {
    TURN_ROUTE.with(|r| r.set("chat"));
    let started = std::time::Instant::now();
    let result = generate_assistant_reply_inner(llm, browser, windows, prompt, heard, laya, speaker);
    // How the turn was answered and how long it took (numbers and the route's name only: never what was said).
    let route = TURN_ROUTE.with(|r| r.get());
    match &result {
        Ok(_) => {
            crate::metrics::inc("aqua_turns_total", &[("route", route)]);
            crate::metrics::observe_seconds("aqua_turn_seconds", &[("route", route)], started.elapsed().as_secs_f64());
        }
        Err(_) => crate::metrics::inc("aqua_errors_total", &[("kind", "turn")]),
    }
    // What was said and answered is kept for the next turn ("and tomorrow?", "make it shorter"); the wake phrase is not part of it.
    if let Ok(reply) = &result {
        let said = crate::voice_commands::without_wake_phrase(prompt, heard.wake_phrase());
        crate::conversation::record_turn(if said.trim().is_empty() { prompt } else { &said }, reply, TURN_ROUTE.with(|r| r.get()));
    }
    // What the background tasks that have ended since the last turn came to is said at this one (once); it is not part of what was remembered of the turn. A chat answer that was
    // spoken as it was written cannot have it in front of it: there the note is in the text that is returned and is not spoken.
    result.map(with_background_news)
}

/// The reply, with a sentence or two in front of it about the background tasks that have ended since the person's last turn (none when there is nothing to say).
fn with_background_news(reply: String) -> String {
    // The turns of many tests run at once and the queue's announcements are one for the process: a test must not have another's taken from it (the queue's own tests cover the text).
    #[cfg(test)]
    let news = None;
    #[cfg(not(test))]
    let news = crate::queue::announcement_text();
    prefixed_with(news, reply)
}

pub(super) fn prefixed_with(news: Option<String>, reply: String) -> String {
    match news {
        Some(note) if reply.trim().is_empty() => note,
        Some(note) => format!("{note} {reply}"),
        None => reply,
    }
}

thread_local! {
    /// How the turn this thread is handling was answered (`chat`, `agent`, `local`, `weather`, `follow_up`, `refuse`), for the conversation record.
    static TURN_ROUTE: std::cell::Cell<&'static str> = const { std::cell::Cell::new("chat") };
}

pub(super) fn note_route(route: &'static str) {
    TURN_ROUTE.with(|r| r.set(route));
}

fn generate_assistant_reply_inner(
    llm: &LlamaRouterEngine,
    browser: Option<Arc<BrowserBackend>>,
    windows: Option<Arc<Mutex<StdioMcpClient>>>,
    prompt: &str,
    heard: Heard<'_>,
    laya: Option<&Arc<LayaGate>>,
    speaker: Option<&mut Speaker<'_>>,
) -> Result<String> {
    // A stopped task ends at the next stage boundary instead of running on.
    ensure_not_stopped()?;
    let mcp_available = browser.is_some() || windows.is_some();
    // The wake phrase is how the person got Aqua's attention, not part of the request: "Hey Aqua, open Notepad." is "open Notepad."
    // (Speech recognition writes a comma after the name, which every parser below used to take for part of the request.)
    let cleaned = crate::voice_commands::without_wake_phrase(prompt, heard.wake_phrase());
    let prompt = if cleaned.trim().is_empty() { prompt } else { cleaned.as_str() };

    // "New topic" ends the conversation so far: what was said before is not carried into what comes next.
    if crate::conversation::is_new_topic_command(prompt) {
        crate::conversation::clear();
        crate::follow_up::clear();
        // Not itself a turn to remember.
        note_route("none");
        return Ok("Okay, a fresh start.".to_string());
    }
    // "Remember that ...", "forget ...", "what do you remember", "go incognito": answered by the program from the person's own words, with no model.
    // Words that would be refused as a request are not kept either.
    if crate::laya_validator::attack_phrase(prompt).is_none() && crate::laya_validator::harmful_intent(prompt).is_none() {
        // (A script with the API token is not the person: it does not read, change or switch off what they asked Aqua to remember.)
        if let Some(reply) = crate::conversation::takes_part().then(|| crate::memory::user_facts::handle_utterance(prompt)).flatten() {
            ensure_not_stopped()?;
            crate::conversation::take_pending();
            note_route("memory");
            return Ok(reply);
        }
        // "Index my documents folder": the vault is only filled by a person (card R6-15), so the program does not read anything: it puts the question on the screen and says so.
        if let Some(reply) = crate::conversation::takes_part().then(|| crate::vault::voice::handle_utterance(prompt)).flatten() {
            ensure_not_stopped()?;
            note_route("local");
            return Ok(reply);
        }
        // "Read the diff", "next change", "line by line", "repeat", "show me the diff", "undo the last run": read out of what the last run changed, or put on the screen as a card, with no model
        // (card OA10-06, UX-16). Nothing is changed by it, and the undo is a card that the person presses in the app: a voice cannot press it. Off unless `AQUA_CODING_VOICE` is on.
        if let Some(reply) = crate::conversation::takes_part().then(|| crate::coding_voice::handle_utterance(prompt)).flatten() {
            ensure_not_stopped()?;
            note_route("local");
            return Ok(reply);
        }
        // The person is correcting the answer just given: their own words, and only theirs, are offered as a lesson for them to confirm (card R6-36). Nothing is written now, and the
        // request goes on as it would have.
        if crate::conversation::takes_part() && crate::conversation::had_a_recent_answer(std::time::Duration::from_secs(crate::memory::user_facts::dream::CORRECTION_WINDOW_SECS as u64)) {
            crate::memory::user_facts::note_correction(prompt);
        }
    }
    // A question the agent asked is waiting: what is said now is its answer, unless it is a stop or a request of its own.
    if let Some(reply) = answer_pending_question(llm, &browser, &windows, prompt, heard, laya)? {
        return Ok(reply);
    }
    // The text the agent is told came earlier is chosen from what was really said; the programs that answer without a model are given
    // a short follow-up ("and tomorrow?") written out as the whole request.
    let said = prompt;
    let resolved = crate::conversation::resolve_reference(prompt);
    let prompt = resolved.as_deref().unwrap_or(prompt);

    // "Remind me at five to call Anna", "set a timer for ten minutes", "what are my reminders": read by rules and kept by the scheduler, no model. It comes before the
    // follow-up of the step before ("remind me to close it at five" names "it", which is not the window), and it does not wait for the sentence to sound like a request:
    // the words that begin it say what it is.
    if crate::laya_validator::attack_phrase(prompt).is_none() && crate::laya_validator::harmful_intent(prompt).is_none() && crate::reminders::is_about_reminders(prompt) {
        if let Some(reply) = crate::turn_timing::stage("reminder", || crate::reminders::handle_voice(prompt)) {
            ensure_not_stopped()?;
            note_route("reminder");
            return Ok(reply);
        }
    }

    // A follow-up about the window, page, or file from the previous step. A new task falls through.
    if crate::laya_validator::attack_phrase(prompt).is_none() && crate::laya_validator::harmful_intent(prompt).is_none() {
        let broker = browser.as_ref().map(|b| b.broker());
        if let Some(reply) = crate::turn_timing::stage("follow_up", || crate::follow_up::continue_turn(prompt, &windows, broker, heard.is_spoken())) {
            ensure_not_stopped()?;
            note_route("follow_up");
            return Ok(reply);
        }
    }
    // "Type hello": a spoken request may type text into the window in front, with no session and no named app (decision D-14). Never Enter, never a click.
    if heard.is_spoken() && crate::laya_validator::attack_phrase(prompt).is_none() && crate::laya_validator::harmful_intent(prompt).is_none() {
        match crate::follow_up::typing_asked(prompt) {
            crate::follow_up::Typing::Text(text) => {
                let broker = browser.as_ref().map(|b| b.broker());
                let reply = crate::turn_timing::stage("follow_up", || crate::follow_up::type_into_the_window_in_front(&text, &windows, broker));
                ensure_not_stopped()?;
                note_route("follow_up");
                return Ok(reply);
            }
            crate::follow_up::Typing::NeedsEnter => {
                note_route("follow_up");
                return Ok("I can type that, but pressing Enter or sending is not something I do by voice. Say what to type, and press Enter yourself.".to_string());
            }
            crate::follow_up::Typing::NotTyping => {}
        }
    }

    // Commands with a fixed grammar ("open Notepad", "search my system for X", "open the executable", "open the Personal AI
    // folder") are done by the program, not planned by the model, and they do not wait for the 6 to 14 second model-based
    // check either. The phrase rules that refuse known attack wording still run first, and every action goes through the
    // same guards as before (no shells, no scripts, only what a search returned, installers only when asked for).
    if crate::laya_validator::attack_phrase(prompt).is_none() && crate::laya_validator::harmful_intent(prompt).is_none() {
        // The time and the date are on the computer's clock: no model, no waiting.
        if let Some(reply) = crate::turn_timing::stage("local", || crate::local_facts::handle_request(prompt)) {
            ensure_not_stopped()?;
            note_route("local");
            return Ok(reply);
        }
        if let Some(reply) = crate::turn_timing::stage("local", || aqua_mcp_adapters::local_files::handle_request(prompt)) {
            ensure_not_stopped()?;
            note_route("local");
            if reply.starts_with("Opened ") {
                return Ok(crate::follow_up::after_step(&reply, "that file", crate::follow_up::Kind::File));
            }
            if reply.contains("I found") || reply.contains("found ") {
                crate::conversation::note_results(&aqua_mcp_adapters::local_files::last_result_names());
                return Ok(crate::follow_up::after_step(&reply, "those results", crate::follow_up::Kind::Search));
            }
            return Ok(reply);
        }
        // A plain question about the weather is answered from a weather service in about a second (the agent took 156 to 343 s: a
        // search in the browser and a round of the model). Heard by the microphone it must also be a request, not the television.
        if !heard.is_spoken() || crate::spoken_request::is_request_like(prompt) {
            if let Some(reply) = crate::turn_timing::stage("weather", || crate::live_weather::handle_request(prompt)) {
                ensure_not_stopped()?;
                note_route("weather");
                // So that "and tomorrow?" is about this place.
                crate::conversation::note_weather(crate::live_weather::place_asked_about(prompt).flatten().as_deref());
                return Ok(reply);
            }
        }
        if let Some((open_goal, text)) = crate::follow_up::split_open_and_write(prompt) {
            if let Some(reply) = crate::turn_timing::stage("local", || crate::modules::browser_agent::open_app_directly(&open_goal, &windows, WindowsCaller::Voice)) {
                ensure_not_stopped()?;
                let target = crate::modules::browser_agent::plain_open_app_request(&open_goal).unwrap_or_else(|| "it".into());
                let _ = crate::follow_up::after_step(&reply, &target, crate::follow_up::Kind::App);
                let typed = crate::follow_up::continue_turn(&format!("type {text}"), &windows, browser.as_ref().map(|b| b.broker()), heard.is_spoken());
                let typed = typed.unwrap_or_else(|| format!("I opened {target}, but I could not type yet."));
                return Ok(format!("{reply} {typed}"));
            }
        }
        if let Some(reply) = crate::turn_timing::stage("local", || crate::modules::browser_agent::open_app_directly(prompt, &windows, WindowsCaller::Voice)) {
            ensure_not_stopped()?;
            let opened = reply.starts_with("Opened ") || reply.contains(" is already open");
            if !opened {
                return Ok(reply);
            }
            let target = crate::modules::browser_agent::plain_open_app_request(prompt).unwrap_or_else(|| "it".into());
            return Ok(crate::follow_up::after_step(&reply, &target, crate::follow_up::kind_of(prompt)));
        }
    }

    let decision = laya
        // With a time limit: a gate stuck behind other model work must not hold the whole turn for minutes. On
        // timeout it answers with the cautious fallback instead.
        .map(|g| crate::turn_timing::stage("gate", || g.decide_with_timeout(prompt, mcp_available, LayaGate::gate_timeout(), |_| {})))
        .unwrap_or_else(|| GateDecision {
            route: if mcp_available {
                GateRoute::McpAgent
            } else {
                GateRoute::Chat
            },
            needs_tools: mcp_available,
            jailbreak: 0.0,
            prompt_injection: 0.0,
            harm_severity: 0.0,
            confidence: 0.0,
            surface: crate::laya_validator::SurfaceHint::Either,
            source: "ungated".into(),
            latency_ms: 0,
        });

    // Where the switches are on, the model that is loaded is asked once, in one step, and may move the request or refuse it (see `laya_validator::decision_route`); it is not waited for.
    let mut decision = crate::laya_validator::refine_route(decision, prompt, llm, mcp_available);

    info!("Laya gate {}", decision.log_fields());
    ensure_not_stopped()?;

    let not_a_request = keep_non_requests_out_of_the_agent(heard, prompt, &mut decision);
    if not_a_request {
        info!("not a request (heard, not typed): answered in chat, no agent: {:?}", prompt.chars().take(80).collect::<String>());
    }

    // The safety check could not run, and this sounds like something to do rather than something to ask: say so,
    // instead of answering in chat (which would only say "ask Aqua", the very thing being asked). Not for a sentence that
    // was never a request.
    if !not_a_request {
        if let Some(reply) = degraded_gate_reply(&decision.source, prompt, mcp_available) {
            return Ok(reply);
        }
    }

    match decision.route {
        GateRoute::Refuse => {
            note_route("refuse");
            return Ok(GateDecision::refuse_reply().to_string());
        }
        GateRoute::Chat => {
            return chat_reply(llm, prompt, speaker);
        }
        GateRoute::McpAgent => {
            // continue below
        }
    }

    // General path: local LLM plans tools via MCP — no per-task hard-coded scripts.
    if mcp_available {
        // Voice and typed orb requests can look at the screen and open apps by name ("open Notepad"), and find and
        // open programs from a search. The guard still denies clicking, typing and key presses for this caller.
        note_route("agent");
        return run_agent_turn(llm, browser, windows, prompt, said, decision.surface.agent_prompt_line(), laya, None, WindowsCaller::Voice);
    }
    chat_reply(llm, prompt, speaker)
}

/// What the person is told when the safety gate timed out, was busy or could not start, and the request sounds like an
/// action (the gate then only lets plain look-ups through, see `fallback_decision`).
pub(super) fn degraded_gate_reply(gate_source: &str, prompt: &str, tools_available: bool) -> Option<String> {
    let degraded = matches!(gate_source, "timeout" | "busy" | "fallback");
    (degraded
        && tools_available
        && crate::laya_validator::looks_actionable(prompt)
        && !crate::laya_validator::looks_live_info(prompt))
    .then(|| "I can't check this request for safety right now, so I'm only answering questions and looking things up. Please ask again in a moment.".to_string())
}

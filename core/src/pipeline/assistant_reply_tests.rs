//! Tests of `assistant_reply.rs`: what a turn answers, and which spoken sentences never reach the agent (move only: they were part of the inline modules `honest_answer_tests`, `degraded_gate_tests`, `date_and_wake_phrase_tests` and `spoken_request_routing_tests` of `pipeline.rs`; the path of every test is now `pipeline::assistant_reply_tests::<name>`).

use super::*;
use super::degraded_gate_reply as reply;
use crate::laya_validator::{fallback_decision, SurfaceHint};

#[test]
fn what_a_background_task_came_to_goes_in_front_of_the_reply_and_stands_alone_when_there_is_no_reply() {
    assert_eq!(prefixed_with(None, "It is sunny.".into()), "It is sunny.");
    assert_eq!(prefixed_with(Some("A background task has finished: \u{201c}x\u{201d}. Blue.".into()), "It is sunny.".into()), "A background task has finished: \u{201c}x\u{201d}. Blue. It is sunny.");
    assert_eq!(prefixed_with(Some("A background task has finished.".into()), "  ".into()), "A background task has finished.");
}

#[test]
fn a_sentence_that_begins_as_a_reminder_is_answered_by_the_reminders_whether_or_not_it_sounds_like_a_request_and_before_any_follow_up() {
    use crate::reminders::{with_test_api, Due, ReminderApi, ReminderView};
    #[derive(Default)]
    struct Kept(std::sync::Mutex<Vec<String>>);
    impl ReminderApi for Kept {
        fn add(&self, message: &str, due: &Due) -> Result<ReminderView, String> {
            let mut kept = self.0.lock().unwrap();
            kept.push(message.to_string());
            Ok(ReminderView { id: kept.len() as i64, message: message.into(), due_at: Some(1_000), when: format!("{due:?}") })
        }
        fn list(&self) -> Result<Vec<ReminderView>, String> {
            Ok(self.0.lock().unwrap().iter().enumerate().map(|(i, m)| ReminderView { id: i as i64 + 1, message: m.clone(), due_at: Some(1_000), when: String::new() }).collect())
        }
        fn cancel(&self, _: i64) -> Result<bool, String> {
            Ok(false)
        }
    }
    let api = std::sync::Arc::new(Kept::default());
    let llm = LlamaRouterEngine::new(std::env::temp_dir().join("aqua_no_models_for_this_test")).unwrap();
    let _session = crate::follow_up::SESSION_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    crate::conversation::clear();
    // The step before was about a window: "it" is what the follow-up would take this sentence for.
    crate::follow_up::clear();
    let _ = crate::follow_up::after_step("Opened Notepad.", "notepad", crate::follow_up::Kind::App);
    for words in ["remind me to close it in ten minutes", "Hey Aqua, remind me to call Anna in ten minutes", "set a timer for ten minutes"] {
        let reply = with_test_api(api.clone(), || generate_assistant_reply(&llm, None, None, words, Heard::Spoken { wake_phrase: "Hey Aqua" }, None)).unwrap();
        assert!(reply.starts_with("Okay, "), "{words:?} gave {reply:?}");
    }
    assert_eq!(*api.0.lock().unwrap(), vec!["close it".to_string(), "call Anna".to_string(), "Your timer is done".to_string()]);
    // Typed, the same sentence is the same.
    let typed = with_test_api(api.clone(), || generate_assistant_reply(&llm, None, None, "remind me to stretch in 5 minutes", Heard::Typed, None)).unwrap();
    assert_eq!(typed, "Okay, I'll remind you to stretch in 5 minutes.");
    assert!(crate::reminders::is_about_reminders("remind me to stretch at five"));
    assert!(!crate::reminders::is_about_reminders("I remind you of someone") && !crate::reminders::is_about_reminders("tell me a joke about timers"));
    crate::follow_up::clear();
}

#[test]
fn a_script_with_the_api_token_is_not_the_person_and_its_memory_words_change_and_read_nothing() {
    use crate::memory::user_facts::{with_test_store, FactStore};
    let dir = std::env::temp_dir().join(format!("aqua_api_memory_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = std::sync::Arc::new(FactStore::open(dir.join("user_memory.db")).unwrap());
    store.remember("My sister's name is Asha", "user", 1).unwrap();
    let llm = LlamaRouterEngine::new(std::env::temp_dir().join("aqua_no_models_for_this_test")).unwrap();
    // The person's own voice or typing: answered by the program, as before.
    let asked = with_test_store(store.clone(), || generate_assistant_reply(&llm, None, None, "what do you remember", Heard::Typed, None).unwrap());
    assert!(asked.contains("Asha"), "{asked}");
    // A request with the API token takes no part: none of these may be answered from the memory, change it or switch it off. (Where they go instead needs a
    // model, which this test does not have: their result is not what is looked at.)
    for words in ["what do you remember", "forget my sister", "forget everything you remember", "go incognito", "remember that I like tea"] {
        let reply = with_test_store(store.clone(), || crate::conversation::with_client(false, || generate_assistant_reply(&llm, None, None, words, Heard::Typed, None)));
        if let Ok(text) = &reply {
            assert!(!text.contains("Asha"), "{words:?} read the memory: {text}");
        }
    }
    assert_eq!(store.count(), 1, "nothing was forgotten or added");
    assert!(!store.incognito(), "the memory was not switched off");
    assert_eq!(store.list(true, 10)[0].text, "My sister's name is Asha");
}

#[test]
fn when_the_gate_could_not_run_an_action_request_is_told_so_and_a_question_or_lookup_is_not() {
    for source in ["timeout", "busy", "fallback"] {
        let r = reply(source, "send an email to Bob saying I quit", true).expect("an action is told");
        assert!(r.contains("can't check this request for safety") && r.contains("only answering questions"), "{r}");
        assert!(reply(source, "what is the capital of France", true).is_none(), "a plain question is just answered");
        assert!(reply(source, "what is the weather in Pune today", true).is_none(), "a look-up still works");
    }
    // the gate worked, or was switched off on purpose, or there are no tools anyway: nothing to say
    assert!(reply("laya", "send an email to Bob saying I quit", true).is_none());
    assert!(reply("disabled", "send an email to Bob saying I quit", true).is_none());
    assert!(reply("timeout", "send an email to Bob saying I quit", false).is_none());
}

#[test]
fn a_turn_answered_from_the_clock_records_a_local_stage_and_no_model_stage() {
    let llm = LlamaRouterEngine::new(std::env::temp_dir().join("aqua_no_models_for_this_test")).unwrap();
    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let _turn = crate::turn_timing::Recording::start(out.clone());
        generate_assistant_reply(&llm, None, None, "What time is it?", Heard::Typed, None).unwrap();
    }
    let stages: Vec<&str> = out.lock().unwrap().iter().map(|s| s.stage).collect();
    // The follow-up check runs first (it is a few string comparisons); what must not appear is anything that uses a model.
    assert!(stages.contains(&"local"), "the clock answered it: {stages:?}");
    assert!(!stages.iter().any(|s| ["gate", "chat", "agent", "weather", "stt", "tts"].contains(s)), "no gate, no chat, no agent: {stages:?}");
}

#[test]
fn a_turn_is_remembered_without_its_wake_phrase_and_new_topic_forgets_it() {
    let llm = LlamaRouterEngine::new(std::env::temp_dir().join("aqua_no_models_for_this_test")).unwrap();
    crate::conversation::clear();
    let reply = generate_assistant_reply(&llm, None, None, "Hey Aqua, what time is it?", Heard::Spoken { wake_phrase: "Hey Aqua" }, None).unwrap();
    let snap = crate::conversation::snapshot();
    let turn = &snap["turns"][0];
    assert_eq!((turn["user"].as_str(), turn["route"].as_str()), (Some("what time is it?"), Some("local")), "{snap}");
    assert!(turn["assistant"].as_str().unwrap().starts_with("It's "), "{reply}");
    // The next chat prompt carries it, as context.
    assert!(chat_prompt("and in an hour?").contains("User: what time is it?
Aqua: It's "));
    // "New topic" ends it, and is not remembered itself.
    let said = generate_assistant_reply(&llm, None, None, "new topic", Heard::Typed, None).unwrap();
    assert_eq!(said, "Okay, a fresh start.");
    assert_eq!(crate::conversation::snapshot()["turns"], serde_json::json!([]));
    assert!(!chat_prompt("hello").contains("Earlier in this conversation"));
}

#[test]
fn a_time_question_with_the_wake_phrase_is_answered_from_the_clock_with_no_model_at_all() {
    // No models folder, no browser, no windows tools, no gate: the answer can only have come from the computer's clock.
    let llm = LlamaRouterEngine::new(std::env::temp_dir().join("aqua_no_models_for_this_test")).unwrap();
    for said in ["Hey Aqua, what time is it?", "hey aqua what's the time", "Okay Aqua. Could you tell me the time please?", "What time is it?"] {
        let reply = generate_assistant_reply(&llm, None, None, said, Heard::Spoken { wake_phrase: "Hey Aqua" }, None).unwrap();
        assert!(reply.starts_with("It's ") && (reply.ends_with(" AM.") || reply.ends_with(" PM.")), "{said:?} -> {reply:?}");
    }
    let date = generate_assistant_reply(&llm, None, None, "Hey Aqua, what's the date today?", Heard::Typed, None).unwrap();
    assert_eq!(date, format!("Today is {}.", chrono::Local::now().format("%A, %B %-d, %Y")));
    // A phrase chosen in the settings counts the same way.
    let jarvis = generate_assistant_reply(&llm, None, None, "Hey Jarvis, what day is it?", Heard::Spoken { wake_phrase: "Hey Jarvis" }, None).unwrap();
    assert_eq!(jarvis, format!("Today is {}.", chrono::Local::now().format("%A")));
}

/// The goals that reached the tool agent in three days of the log, with whether they were requests (the same table that
/// `spoken_request` is tested with, here through the routing).
const LOGGED: &[(&str, bool)] = &[
    ("open notepad", true),
    ("Open Notepad", true),
    ("Open the calculator and tell me what window is in front", true),
    ("You have to search in my system visual CV and open it for me.", true),
    ("Today is weather. Tell me today is weather.", true),
    ("Tell me today's weather.", true),
    ("Tell me today is weather.", true),
    ("Tell me today is better.", true),
    ("Search my system for Notepad", true),
    ("Search my system for Need for Speed", true),
    ("Open, open.", true),
    ("Open it for a speed.", true),
    ("Can you open a neat voice feed with me for me? Open neat voice feed.", true),
    ("The lady, are you satisfied with your time at the academy so far? Yes, I'm enjoy\u{2026}", false),
    ("Just talking about you. Yeah, naughty, naughty.", false),
    ("I'm going to play the game with the only Thaisur.", false),
    ("I'm going to do a little bit of a video.", false),
    ("I can't understand what are you saying. Please speak properly.", false),
    ("How many persons you are taking right now?", false),
    ("Hi. It's the reason why I'm here today. It's the reason why I'm here today. The \u{2026}", false),
    ("Hello everyone. Hello everyone.", false),
    ("Neat for speed. So I will turn my device.", false),
];

/// What the language-model gate answered for these sentences in the log: the agent, at low confidence.
fn gate_said_agent() -> GateDecision {
    GateDecision {
        route: GateRoute::McpAgent,
        needs_tools: true,
        jailbreak: 0.0,
        prompt_injection: 0.0,
        harm_severity: 0.0,
        confidence: 0.399,
        surface: SurfaceHint::Either,
        source: "laya".into(),
        latency_ms: 0,
    }
}

const SPOKEN: Heard<'static> = Heard::Spoken { wake_phrase: "Hey Aqua" };

#[test]
fn a_spoken_sentence_that_is_not_a_request_never_reaches_the_agent() {
    for (goal, request) in LOGGED {
        let mut decision = gate_said_agent();
        let changed = keep_non_requests_out_of_the_agent(SPOKEN, goal, &mut decision);
        assert_eq!(changed, !*request, "{goal}");
        assert_eq!(decision.route, if *request { GateRoute::McpAgent } else { GateRoute::Chat }, "{goal}");
        assert_eq!(decision.needs_tools, *request, "{goal}");
    }
}

#[test]
fn typed_text_is_deliberate_and_is_never_checked() {
    for (goal, _) in LOGGED {
        let mut decision = gate_said_agent();
        assert!(!keep_non_requests_out_of_the_agent(Heard::Typed, goal, &mut decision), "{goal}");
        assert_eq!(decision.route, GateRoute::McpAgent, "{goal}");
    }
}

#[test]
fn a_refusal_and_a_chat_route_are_left_as_they_are() {
    let mut refused = gate_said_agent();
    refused.route = GateRoute::Refuse;
    assert!(!keep_non_requests_out_of_the_agent(SPOKEN, "Hello everyone.", &mut refused));
    assert_eq!(refused.route, GateRoute::Refuse);
    let mut chat = gate_said_agent();
    chat.route = GateRoute::Chat;
    assert!(!keep_non_requests_out_of_the_agent(SPOKEN, "Hello everyone.", &mut chat));
}

/// The configuration Aqua ships with: the model gate off, so the keyword rules alone choose. Before the two fixes, "play " in
/// "I'm going to play the game" and "right now" in "How many persons you are taking right now?" each started the agent.
#[test]
fn with_the_keyword_router_on_its_own_only_the_requests_end_up_in_the_agent() {
    for (goal, request) in LOGGED {
        let mut decision = fallback_decision(goal, true, "disabled");
        keep_non_requests_out_of_the_agent(SPOKEN, goal, &mut decision);
        assert_eq!(decision.route == GateRoute::McpAgent, *request, "{goal}: {decision:?}");
    }
}
